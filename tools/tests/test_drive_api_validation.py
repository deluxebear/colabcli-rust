import copy
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import sys
import threading
import unittest
from urllib.parse import parse_qs, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from drive_api_validation import DriveObserver, ObservationError


class ObserverTests(unittest.TestCase):
    def setUp(self):
        self.folder = 'colabcli-validation-' + 'a' * 32
        self.expected = {'owner.json': b'owner', 'probe-雪.bin': bytes(range(256))}
        def row(identifier, name, parent, **extra):
            return dict(id=identifier, name=name, parents=[parent], ownedByMe=True,
                        owners=[{'permissionId': 'synthetic-permission'}], **extra)
        self.about = {'user': {'permissionId': 'synthetic-permission', 'me': True}}
        self.root = {'id': 'root-id', 'ownedByMe': True, 'owners': [{'permissionId': 'synthetic-permission'}]}
        self.folders = {'files': [row('folder-id', self.folder, 'root-id', mimeType='application/vnd.google-apps.folder')]}
        self.files = {'files': [row('file'+str(i), n, 'folder-id', mimeType='application/octet-stream',
                                  size=str(len(v)), md5Checksum=hashlib.md5(v).hexdigest())
                                for i, (n, v) in enumerate(self.expected.items())]}
        self.media = {'file'+str(i): v for i, v in enumerate(self.expected.values())}
        self.objects = {row['id']: dict(row, trashed=False) for row in self.folders['files'] + self.files['files']}
        self.calls = []
        self.error = None
        outer = self
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass
            def do_GET(self):
                outer.calls.append((self.command, self.path, self.headers.get('Authorization')))
                path = urlsplit(self.path).path
                params = parse_qs(urlsplit(self.path).query)
                if outer.error:
                    code, body, headers = outer.error
                else:
                    if path.endswith('/about'):
                        value = outer.about
                    elif path.endswith('/files/root'):
                        value = outer.root
                    elif path.endswith('/files'):
                        value = outer.folders if "'root-id'" in params['q'][0] else outer.files
                    else:
                        identifier = path.rsplit('/', 1)[1]
                        value = outer.media[identifier] if params.get('alt') == ['media'] else outer.objects.get(identifier)
                    code, body, headers = (404 if value is None else 200), value if isinstance(value, bytes) else json.dumps(value).encode(), {}
                self.send_response(code)
                for k, v in headers.items():
                    self.send_header(k, v)
                self.end_headers()
                self.wfile.write(body)
        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.client = DriveObserver('synthetic-token', base=f'http://127.0.0.1:{self.server.server_port}/drive/v3')

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def check(self):
        return self.client.inspect(self.folder, self.expected)

    def test_exact_server_bytes_and_owned_my_drive(self):
        report = self.check()
        self.assertTrue(report['server_bytes_verified'])
        self.assertFalse(report['oauth_subject_mapping_verified'])
        self.assertEqual(len(self.calls), 6)
        self.assertTrue(all(c[0] == 'GET' and c[2] == 'Bearer synthetic-token' for c in self.calls))
        self.assertNotIn('synthetic-permission', json.dumps(report))
        self.assertNotIn('synthetic-token', json.dumps(report))

    def test_identity_and_root_owner_must_agree(self):
        self.root['owners'][0]['permissionId'] = 'another-user'
        with self.assertRaisesRegex(ObservationError, 'root_owner'):
            self.check()
        self.assertEqual(len(self.calls), 2)

    def test_user_must_be_current_authenticated_principal(self):
        self.about['user']['me'] = False
        with self.assertRaisesRegex(ObservationError, 'user_unconfirmed'):
            self.check()

    def test_duplicate_folder_is_not_adopted(self):
        self.folders['files'] *= 2
        with self.assertRaisesRegex(ObservationError, 'not_unique'):
            self.check()

    def test_extra_entries_and_wrong_parent_owner_are_rejected(self):
        original = copy.deepcopy(self.files)
        for change in ['extra', 'owner', 'parent', 'shortcut']:
            with self.subTest(change=change):
                self.files = copy.deepcopy(original)
                if change == 'extra':
                    self.files['files'].append(copy.deepcopy(self.files['files'][0]))
                elif change == 'owner':
                    self.files['files'][0]['ownedByMe'] = False
                elif change == 'parent':
                    self.files['files'][0]['parents'] = ['elsewhere']
                else:
                    self.files['files'][0]['mimeType'] = 'application/vnd.google-apps.shortcut'
                with self.assertRaises(ObservationError):
                    self.check()

    def test_pagination_and_incomplete_search_fail_closed(self):
        for extra in [{'nextPageToken': 'next'}, {'incompleteSearch': True}]:
            with self.subTest(extra=extra):
                self.folders.update(extra)
                with self.assertRaisesRegex(ObservationError, 'incomplete_listing'):
                    self.check()
                for key in extra:
                    del self.folders[key]

    def test_checksum_and_download_bytes_are_both_checked(self):
        self.files['files'][0]['md5Checksum'] = 'incorrect'
        with self.assertRaisesRegex(ObservationError, 'metadata_mismatch'):
            self.check()
        self.files['files'][0]['md5Checksum'] = hashlib.md5(b'owner').hexdigest()
        self.media['file0'] = b'other'
        with self.assertRaisesRegex(ObservationError, 'bytes_mismatch'):
            self.check()
        self.media['file0'] = b'owner-over-limit'
        with self.assertRaisesRegex(ObservationError, 'too_large'):
            self.check()

    def test_redirect_is_refused_before_second_request(self):
        self.error = (302, b'private error', {'Location': self.client.base+'/files/root'})
        with self.assertRaisesRegex(ObservationError, 'redirect_refused'):
            self.check()
        self.assertEqual(len(self.calls), 1)

    def test_server_error_and_invalid_json_never_escape_body(self):
        for code, body in [(401, b'synthetic-token private'), (200, b'private-invalid-json')]:
            self.error = (code, body, {})
            with self.assertRaises(ObservationError) as error:
                self.check()
            self.assertNotIn('private', str(error.exception))
            self.assertNotIn('synthetic-token', str(error.exception))

    def test_untrusted_origin_and_query_injection_are_local_failures(self):
        for base in ['https://evil.example/drive/v3', 'http://localhost/drive/v3',
                     'https://www.googleapis.com/drive/v3/other', 'http://127.0.0.1:80/drive/v3?x=1']:
            with self.assertRaises(ObservationError):
                DriveObserver('synthetic-token', base=base)
        with self.assertRaises(ObservationError):
            self.client.children("root' or trashed=true")
        with self.assertRaises(ObservationError):
            self.client.inspect('existing-user-folder', self.expected)
        self.assertEqual(self.calls, [])

    def test_cleanup_requires_successful_exact_bytes_snapshot(self):
        with self.assertRaisesRegex(ObservationError, 'requires_verified_snapshot'):
            self.client.verify_removed()
        self.assertEqual(self.calls, [])
        self.check()
        self.media['file0'] = b'wrong'
        with self.assertRaises(ObservationError):
            self.check()
        count = len(self.calls)
        with self.assertRaisesRegex(ObservationError, 'requires_verified_snapshot'):
            self.client.verify_removed()
        self.assertEqual(len(self.calls), count)

    def test_cleanup_distinguishes_trashed_and_missing_without_deleting(self):
        self.check()
        self.objects['folder-id']['trashed'] = True
        self.objects['file0']['trashed'] = True
        del self.objects['file1']
        result = self.client.verify_removed()
        self.assertEqual(result, {'server_cleanup_verified': True, 'trashed_objects': 2,
                                 'missing_objects': 1, 'permanent_deletion_verified': False})
        self.assertTrue(all(method == 'GET' for method, _, _ in self.calls))
        self.assertNotIn('folder-id', json.dumps(result))

    def test_cleanup_does_not_confuse_empty_listing_with_deleted_ids(self):
        self.check()
        self.folders = {'files': []}
        with self.assertRaisesRegex(ObservationError, 'still_live_or_changed'):
            self.client.verify_removed()

    def test_cleanup_rejects_changed_identity_before_querying_objects(self):
        self.check()
        self.about['user']['permissionId'] = 'other'
        self.root['owners'] = [{'permissionId': 'other'}]
        count = len(self.calls)
        with self.assertRaisesRegex(ObservationError, 'identity_changed'):
            self.client.verify_removed()
        self.assertEqual(len(self.calls) - count, 2)

    def test_cleanup_403_is_not_missing_and_trash_requires_same_owner(self):
        self.check()
        self.error = (403, b'private failure', {})
        with self.assertRaisesRegex(ObservationError, 'request_failed'):
            self.client.verify_removed()
        self.error = None
        self.objects['folder-id'].update(trashed=True, ownedByMe=False)
        with self.assertRaisesRegex(ObservationError, 'still_live_or_changed'):
            self.client.verify_removed()

    def test_duplicate_object_ids_cannot_form_cleanup_snapshot(self):
        self.files['files'][1]['id'] = 'file0'
        self.media['file0'] = b'owner'
        # Duplicate IDs cannot verify both differing byte payloads.
        with self.assertRaises(ObservationError):
            self.check()
        with self.assertRaisesRegex(ObservationError, 'requires_verified_snapshot'):
            self.client.verify_removed()

    def test_malformed_api_fields_fail_with_bounded_observation_errors(self):
        original = copy.deepcopy(self.files)
        for field, value in [('mimeType', 1), ('name', {}), ('size', 5)]:
            with self.subTest(field=field):
                self.files = copy.deepcopy(original)
                self.files['files'][0][field] = value
                with self.assertRaises(ObservationError):
                    self.check()
