"""Read-only, bounded Drive v3 observations for the live validation harness.

No CLI entry point: callers supply a short-lived token in memory. This module
never creates, updates, trashes or deletes Drive objects.
"""
import hashlib
import json
import re
import urllib.error
import urllib.parse
import urllib.request


class ObservationError(RuntimeError):
    pass


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, newurl):
        raise ObservationError('drive_api_redirect_refused')


class DriveObserver:
    def __init__(self, token, *, base='https://www.googleapis.com/drive/v3', timeout=30):
        parsed = urllib.parse.urlsplit(base)
        production = base == 'https://www.googleapis.com/drive/v3'
        loopback = (parsed.scheme == 'http' and parsed.hostname in ('127.0.0.1', '::1')
                    and parsed.port is not None and parsed.path == '/drive/v3'
                    and not parsed.query and not parsed.fragment and not parsed.username and not parsed.password)
        if not production and not loopback:
            raise ObservationError('drive_api_origin_refused')
        if not isinstance(token, str) or not token or any(ord(c) < 33 or ord(c) > 126 for c in token):
            raise ObservationError('invalid_access_token')
        self._token, self.base, self.timeout = token, base, timeout
        self.opener = urllib.request.build_opener(NoRedirect())
        self._verified_objects = None

    def get(self, path, params, limit, allow_missing=False):
        request = urllib.request.Request(self.base + path + '?' + urllib.parse.urlencode(params),
                                         headers={'Authorization': 'Bearer ' + self._token}, method='GET')
        try:
            with self.opener.open(request, timeout=self.timeout) as response:
                if response.status != 200:
                    raise ObservationError('drive_api_non_success')
                data = response.read(limit + 1)
        except urllib.error.HTTPError as error:
            error.close()
            if allow_missing and error.code == 404:
                return None
            raise ObservationError('drive_api_request_failed') from None
        except (urllib.error.URLError, OSError, ValueError):
            # Never expose a server body, URL or token-bearing request object.
            raise ObservationError('drive_api_request_failed') from None
        if len(data) > limit:
            raise ObservationError('drive_api_response_too_large')
        return data

    def json(self, path, params, allow_missing=False):
        try:
            data = self.get(path, params, 128 * 1024, allow_missing)
            if data is None:
                return None
            value = json.loads(data)
        except (ValueError, UnicodeError):
            raise ObservationError('drive_api_invalid_json') from None
        if not isinstance(value, dict):
            raise ObservationError('drive_api_invalid_schema')
        return value

    def children(self, parent, name=None):
        # IDs originate in Google responses, but must never become query syntax.
        if not isinstance(parent, str) or not re.fullmatch('[A-Za-z0-9_-]+', parent):
            raise ObservationError('drive_api_invalid_id')
        query = "'" + parent + "' in parents and trashed = false"
        if name is not None:
            if not re.fullmatch('colabcli-validation-[a-f0-9]{32}', name):
                raise ObservationError('drive_api_invalid_probe_name')
            query += " and name = '" + name + "'"
        value = self.json('/files', {'q': query, 'spaces': 'drive', 'pageSize': 100,
            'fields': 'nextPageToken,incompleteSearch,files(id,name,mimeType,parents,ownedByMe,owners(permissionId),size,md5Checksum)'})
        if (value.get('nextPageToken') or value.get('incompleteSearch') is True
                or ('incompleteSearch' in value and type(value['incompleteSearch']) is not bool)):
            raise ObservationError('drive_api_incomplete_listing')
        rows = value.get('files')
        if not isinstance(rows, list) or any(not isinstance(row, dict) for row in rows):
            raise ObservationError('drive_api_invalid_listing')
        return rows

    def owned(self, row, identity, parent):
        if (row.get('ownedByMe') is not True or row.get('parents') != [parent]
                or not isinstance(row.get('owners'), list)
                or len(row['owners']) != 1 or not isinstance(row['owners'][0], dict)
                or row['owners'][0].get('permissionId') != identity):
            raise ObservationError('drive_api_owner_or_parent_mismatch')
        identifier = row.get('id')
        if not isinstance(identifier, str) or not re.fullmatch('[A-Za-z0-9_-]+', identifier):
            raise ObservationError('drive_api_invalid_id')
        return identifier

    def _identity(self):
        about = self.json('/about', {'fields': 'user(permissionId,me)'})
        user = about.get('user')
        if not isinstance(user, dict) or user.get('me') is not True or not isinstance(user.get('permissionId'), str) or not user['permissionId']:
            raise ObservationError('drive_api_user_unconfirmed')
        identity = user['permissionId']
        root = self.json('/files/root', {'fields': 'id,ownedByMe,owners(permissionId)'})
        # Root has no parent. Keep the same strict ownership checks.
        root_id = root.get('id')
        if (not isinstance(root_id, str) or not re.fullmatch('[A-Za-z0-9_-]+', root_id)
                or root.get('ownedByMe') is not True or root.get('owners') != [{'permissionId': identity}]):
            raise ObservationError('drive_api_root_owner_mismatch')
        return identity, root_id

    def verify_identity(self):
        self._identity()
        return {'same_token_drive_ownership_verified': True, 'oauth_subject_mapping_verified': False}

    def inspect(self, folder, expected_files):
        """Verify exact server bytes under a unique owned MyDrive probe folder.

        Returns only counts/digests, never user identity, file IDs or content.
        Drive permission IDs are not OAuth subject IDs; this does not establish
        their equivalence, cross-account isolation or deletion persistence.
        """
        self._verified_objects = None
        if (not isinstance(folder, str) or not re.fullmatch('colabcli-validation-[a-f0-9]{32}', folder)
                or not isinstance(expected_files, dict) or len(expected_files) != 2
                or any(not isinstance(k, str) or not k or not isinstance(v, bytes) or len(v) > 4096
                       for k, v in expected_files.items())):
            raise ObservationError('drive_api_invalid_expected_probe')
        identity, root_id = self._identity()
        folders = self.children(root_id, folder)
        if len(folders) != 1 or folders[0].get('name') != folder or folders[0].get('mimeType') != 'application/vnd.google-apps.folder':
            raise ObservationError('drive_api_probe_folder_not_unique')
        folder_id = self.owned(folders[0], identity, root_id)
        rows = self.children(folder_id)
        if (len(rows) != 2 or any(not isinstance(r.get('name'), str) for r in rows)
                or {r['name'] for r in rows} != set(expected_files)):
            raise ObservationError('drive_api_probe_entries_mismatch')
        hashes = {}
        identifiers = [folder_id]
        for row in rows:
            identifier = self.owned(row, identity, folder_id)
            identifiers.append(identifier)
            expected = expected_files[row['name']]
            mime = row.get('mimeType')
            if not isinstance(mime, str) or not mime or mime.startswith('application/vnd.google-apps.'):
                raise ObservationError('drive_api_probe_not_binary')
            if row.get('size') != str(len(expected)) or row.get('md5Checksum') != hashlib.md5(expected).hexdigest():
                raise ObservationError('drive_api_probe_metadata_mismatch')
            if self.get('/files/' + identifier, {'alt': 'media'}, len(expected)) != expected:
                raise ObservationError('drive_api_probe_bytes_mismatch')
            hashes[row['name']] = hashlib.sha256(expected).hexdigest()
        if len(set(identifiers)) != 3:
            raise ObservationError('drive_api_probe_ids_not_distinct')
        self._verified_objects = (identity, root_id, tuple(identifiers))
        return {'server_bytes_verified': True, 'same_token_drive_ownership_verified': True,
                'oauth_subject_mapping_verified': False, 'file_count': 2, 'sha256': hashes}

    def verify_removed(self):
        """Observe only IDs whose ownership and exact bytes inspect confirmed.

        Trashed is distinguished from permanently missing. Neither outcome
        proves irreversible destruction; no mutation is sent here.
        """
        snapshot = self._verified_objects
        if snapshot is None:
            raise ObservationError('drive_api_cleanup_requires_verified_snapshot')
        identity, root_id, identifiers = snapshot
        if self._identity() != (identity, root_id):
            raise ObservationError('drive_api_cleanup_identity_changed')
        trashed, missing = 0, 0
        for identifier in identifiers:
            row = self.json('/files/' + identifier,
                {'fields': 'id,trashed,ownedByMe,owners(permissionId)'}, allow_missing=True)
            if row is None:
                missing += 1
            elif (row.get('id') == identifier and row.get('trashed') is True
                    and row.get('ownedByMe') is True and row.get('owners') == [{'permissionId': identity}]):
                trashed += 1
            else:
                raise ObservationError('drive_api_probe_object_still_live_or_changed')
        return {'server_cleanup_verified': True, 'trashed_objects': trashed, 'missing_objects': missing,
                'permanent_deletion_verified': False}
