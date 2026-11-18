#!/usr/bin/env python3
"""使用合成密钥对照 Python 标准库与 google-auth 的 SigV4；不读取环境凭据或发网络请求。"""
import argparse
import datetime
import hashlib
import hmac
import importlib.metadata
import json
from pathlib import Path
from unittest.mock import patch


def main():
    from google.auth import aws
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    cases = []
    for region, suffix, session in [('us-east-1', '', None), ('eu-west-2', '', 'synthetic-session+/='), ('cn-north-1', '.cn', 'synthetic-china-session')]:
        instant = datetime.datetime(2026, 11, 17, 1, 2, 3)
        audience = '//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/p/providers/p'
        access, secret = 'AKIDEXAMPLE', 'synthetic-secret-key-not-a-real-credential'
        host = f'sts.{region}.amazonaws.com{suffix}'
        url = f'https://{host}?Action=GetCallerIdentity&Version=2011-06-15'
        headers = {'host': host, 'x-amz-date': instant.strftime('%Y%m%dT%H%M%SZ'), 'x-goog-cloud-target-resource': audience}
        if session:
            headers['x-amz-security-token'] = session
        signed = ';'.join(sorted(headers))
        canonical = '\n'.join(['POST', '/', 'Action=GetCallerIdentity&Version=2011-06-15', ''.join(f'{k}:{headers[k]}\n' for k in sorted(headers)), signed, hashlib.sha256(b'').hexdigest()])
        date = instant.strftime('%Y%m%d')
        scope = f'{date}/{region}/sts/aws4_request'
        message = '\n'.join(['AWS4-HMAC-SHA256', headers['x-amz-date'], scope, hashlib.sha256(canonical.encode()).hexdigest()])
        key = ('AWS4' + secret).encode()
        for component in (date, region, 'sts', 'aws4_request'):
            key = hmac.new(key, component.encode(), hashlib.sha256).digest()
        signature = hmac.new(key, message.encode(), hashlib.sha256).hexdigest()
        authorization = f'AWS4-HMAC-SHA256 Credential={access}/{scope}, SignedHeaders={signed}, Signature={signature}'
        with patch('google.auth.aws._helpers.utcnow', return_value=instant):
            observed = aws.RequestSigner(region).get_request_options(aws.AwsSecurityCredentials(access, secret, session), url, 'POST', additional_headers={'x-goog-cloud-target-resource': audience})
        assert observed['headers']['Authorization'] == authorization
        cases.append({'region': region, 'template': 'https://sts.{region}.amazonaws.com' + suffix + '?Action=GetCallerIdentity&Version=2011-06-15',
                      'audience': audience, 'instant': instant.isoformat() + 'Z', 'access_key': access, 'secret_key': secret, 'session_token': session,
                      'url': url, 'authorization': authorization, 'headers': headers})
    args.output.write_text(json.dumps({'google_auth_version': importlib.metadata.version('google-auth'), 'transport': 'stdlib HMAC + in-memory SDK signer; no credentials or network', 'cases': cases}, indent=2) + '\n')
    print(json.dumps({'cases': len(cases), 'sdk_and_stdlib_agree': True}))


if __name__ == '__main__':
    main()
