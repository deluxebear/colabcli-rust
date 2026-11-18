#!/usr/bin/env python3
"""核对显式构造的 google-auth 配额项目语义；禁止网络/子进程，不发现真实凭据。"""
import argparse
import importlib.metadata
import hashlib
import json
import os
from pathlib import Path
import sys
from unittest.mock import patch


def probe():
    def denied(*_args, **_kwargs):
        raise AssertionError('network and subprocesses are forbidden')

    cases = []
    with patch.dict(os.environ, {}, clear=True), patch('socket.socket.connect', denied), patch('socket.create_connection', denied), patch('socket.getaddrinfo', denied), patch('subprocess.Popen', denied):
        from google.auth import _default, identity_pool, impersonated_credentials, credentials as auth_credentials, external_account
        from google.oauth2.credentials import Credentials

        def user(project):
            return Credentials(token='fixture-access', refresh_token='fixture-refresh',
                token_uri='https://oauth2.googleapis.com/token', client_id='fixture-client',
                client_secret='fixture-secret', quota_project_id=project)

        def applied(credentials, token='fixture-access'):
            headers = {}
            credentials.apply(headers, token=token)
            assert headers.pop('authorization') == 'Bearer ' + token
            return headers.get('x-goog-user-project')

        def environment(value):
            if value is None:
                os.environ.pop('GOOGLE_CLOUD_QUOTA_PROJECT', None)
            else:
                os.environ['GOOGLE_CLOUD_QUOTA_PROJECT'] = value

        for file, env, expected in [(None, None, None), ('file-project', None, 'file-project'),
                ('file-project', '', 'file-project'), ('file-project', 'env-project', 'env-project'),
                ('', None, None), (None, 'env-project', 'env-project')]:
            environment(env)
            effective = _default._apply_quota_project_id(user(file), None)
            assert applied(effective) == expected
            cases.append({'kind': 'user', 'file': file, 'environment': env, 'effective': expected})

        for env in [None, 'env-project']:
            environment(env)
            target = impersonated_credentials.Credentials(source_credentials=user('source-project'),
                target_principal='target@example.invalid', target_scopes=['fixture-scope'],
                quota_project_id='target-project')
            target = _default._apply_quota_project_id(target, None)
            assert applied(target._source_credentials, 'source-access') == 'source-project'
            assert applied(target, 'target-access') == (env or 'target-project')
            cases.append({'kind': 'impersonation', 'environment': env,
                          'iam_source': 'source-project', 'target': env or 'target-project'})

        for env in [None, 'env-project']:
            environment(env)
            source = identity_pool.Credentials(
                audience='//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/p/providers/p',
                subject_token_type='urn:ietf:params:oauth:token-type:jwt',
                token_url='https://sts.googleapis.com/v1/token',
                credential_source={'file': '/__fixture_subject_must_not_be_read__'},
                quota_project_id='file-project',
                service_account_impersonation_url='https://iamcredentials.googleapis.com/v1/projects/-/serviceAccounts/target@example.invalid:generateAccessToken')
            source = _default._apply_quota_project_id(source, None)
            target = source._initialize_impersonated_credentials()
            expected = env or 'file-project'
            assert applied(target._source_credentials, 'sts-access') == expected
            assert applied(source, 'target-access') == expected
            cases.append({'kind': 'external_iam', 'environment': env, 'iam_source': expected, 'target': expected})
    return {'google_auth_version': importlib.metadata.version('google-auth'),
            'isolation': 'synthetic in-memory credentials; empty environment; network/subprocess denied; no ADC discovery or refresh',
            'dependency_sources': {module.__name__: hashlib.sha256(Path(module.__file__).read_bytes()).hexdigest() for module in (_default, auth_credentials, impersonated_credentials, external_account)},
            'cases': cases, 'limitations': ['installed library observation only', 'no live billing, Google permissions or Colab acceptance']}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--deps', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    sys.path.insert(0, str(args.deps))
    result = probe()
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({'cases': len(result['cases']), 'google_auth_version': result['google_auth_version']}))


if __name__ == '__main__':
    main()
