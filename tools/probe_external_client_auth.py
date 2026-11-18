#!/usr/bin/env python3
"""Offline google-auth comparison. Run with the reference CLI's Python interpreter."""
import base64
import hashlib
import importlib.metadata
import json
from pathlib import Path
import tempfile
import urllib.parse

from google.auth import external_account, identity_pool
from google.oauth2 import sts, utils


def main():
    cases = []
    with tempfile.TemporaryDirectory(prefix='colab-external-auth-probe-') as directory:
        source = Path(directory) / 'subject'
        source.write_text('synthetic-subject')
        for identifier, secret in [(None, 'unused-secret'), ('', 'unused-secret'),
                                   ('client', None), ('client', ''),
                                   ('client-é', 'secret:+& 字')]:
            info = {
                'type': 'external_account',
                'audience': '//iam.googleapis.com/locations/global/workforcePools/pool/providers/provider',
                'subject_token_type': 'urn:ietf:params:oauth:token-type:jwt',
                'token_url': 'https://sts.googleapis.com/v1/token',
                'credential_source': {'file': str(source)},
                'workforce_pool_user_project': '12345',
            }
            if identifier is not None:
                info['client_id'] = identifier
            if secret is not None:
                info['client_secret'] = secret
            seen = []

            def request(url, method='GET', headers=None, body=None, **kwargs):
                assert url == 'https://sts.googleapis.com/v1/token' and method == 'POST'
                fields = dict(urllib.parse.parse_qsl(body.decode()))
                seen.append({'authorization': headers.get('Authorization'),
                             'options_present': 'options' in fields,
                             'body_client_id_present': 'client_id' in fields,
                             'body_client_secret_present': 'client_secret' in fields})
                data = {'access_token': 'synthetic-access', 'expires_in': 3600,
                        'token_type': 'Bearer',
                        'issued_token_type': 'urn:ietf:params:oauth:token-type:access_token'}
                return type('Response', (), {'status': 200, 'data': json.dumps(data).encode()})()

            credentials = identity_pool.Credentials.from_info(
                info, scopes=['https://www.googleapis.com/auth/cloud-platform'])
            credentials.refresh(request)
            assert len(seen) == 1
            row = seen[0]
            expected = ('Basic ' + base64.b64encode(
                (identifier + ':' + (secret or '')).encode()).decode()) if identifier else None
            assert row['authorization'] == expected
            assert row['options_present'] == (not bool(identifier))
            assert not row['body_client_id_present'] and not row['body_client_secret_present']
            cases.append({'fixture_client_id': identifier, 'fixture_client_secret': secret, **row})
    report = {
        'schema_version': 1,
        'google_auth_version': importlib.metadata.version('google-auth'),
        'network_calls': 0, 'real_credentials_used': False, 'cases': cases,
        'probe_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'source_sha256': {module.__name__: hashlib.sha256(Path(module.__file__).read_bytes()).hexdigest()
                          for module in [identity_pool, external_account, sts, utils]},
    }
    print(json.dumps(report, ensure_ascii=False, indent=2))


if __name__ == '__main__':
    main()
