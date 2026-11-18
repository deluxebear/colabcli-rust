#!/usr/bin/env python3
"""Offline X.509 federation probe using the reference CLI's google-auth environment."""
import datetime
import hashlib
import importlib.metadata
import json
from pathlib import Path
import tempfile
import urllib.parse

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import ec
from cryptography.x509.oid import NameOID
from google.auth import external_account, identity_pool
from google.auth.transport import _mtls_helper


def main():
    now = datetime.datetime.now(datetime.timezone.utc)
    ca_key = ec.generate_private_key(ec.SECP256R1())
    key = ec.generate_private_key(ec.SECP256R1())
    ca_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, 'synthetic-ca')])
    leaf_name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, 'synthetic-leaf')])

    def certificate(subject, public_key, serial):
        return (x509.CertificateBuilder().subject_name(subject).issuer_name(ca_name)
                .public_key(public_key).serial_number(serial)
                .not_valid_before(now - datetime.timedelta(days=1))
                .not_valid_after(now + datetime.timedelta(days=1))
                .sign(ca_key, hashes.SHA256()))

    ca = certificate(ca_name, ca_key.public_key(), 1)
    leaf = certificate(leaf_name, key.public_key(), 2)
    pem = serialization.Encoding.PEM
    import base64
    encode = lambda cert: base64.b64encode(cert.public_bytes(serialization.Encoding.DER)).decode()
    cases = []
    with tempfile.TemporaryDirectory(prefix='colab-x509-official-') as directory:
        root = Path(directory)
        cert_path, key_path, config_path, trust_path = [root / name for name in ['leaf.pem', 'key.pem', 'config.json', 'trust.pem']]
        cert_path.write_bytes(leaf.public_bytes(pem))
        key_path.write_bytes(key.private_bytes(pem, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
        key_path.chmod(0o600)
        config_path.write_text(json.dumps({'cert_configs': {'workload': {'cert_path': str(cert_path), 'key_path': str(key_path)}}}))
        for label, chain, expected in [('leaf_only', None, [encode(leaf)]),
                                       ('chain_with_leaf_first', [leaf, ca], [encode(leaf), encode(ca)]),
                                       ('chain_without_leaf', [ca], [encode(leaf), encode(ca)])]:
            source = {'certificate_config_location': str(config_path)}
            if chain is not None:
                trust_path.write_bytes(b''.join(cert.public_bytes(pem) for cert in chain))
                source['trust_chain_path'] = str(trust_path)
            info = {'type': 'external_account',
                    'audience': '//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/pool/providers/provider',
                    'subject_token_type': 'urn:ietf:params:oauth:token-type:mtls',
                    'token_url': 'https://sts.mtls.googleapis.com/v1/token',
                    'credential_source': {'certificate': source}}
            seen = []

            def request(url, method, headers, body, **kwargs):
                assert url == info['token_url'] and method == 'POST'
                assert kwargs['cert'] == (str(cert_path), str(key_path))
                fields = dict(urllib.parse.parse_qsl(body.decode()))
                assert json.loads(fields['subject_token']) == expected
                assert fields['subject_token_type'] == info['subject_token_type']
                seen.append(True)
                data = {'access_token': 'synthetic-access', 'token_type': 'Bearer', 'expires_in': 3600,
                        'issued_token_type': 'urn:ietf:params:oauth:token-type:access_token'}
                return type('Response', (), {'status': 200, 'data': json.dumps(data).encode()})()

            credentials = identity_pool.Credentials.from_info(info, scopes=['https://www.googleapis.com/auth/cloud-platform'])
            credentials.refresh(request)
            assert len(seen) == 1
            cases.append({'name': label, 'passed': True, 'subject_certificate_count': len(expected),
                          'sts_received_certificate_paths': True})
        trust_path.write_bytes(ca.public_bytes(pem) + leaf.public_bytes(pem))
        try:
            credentials.retrieve_subject_token(None)
        except Exception as error:
            from google.auth.exceptions import RefreshError
            assert isinstance(error, RefreshError)
            cases.append({'name': 'leaf_later_in_chain_rejected', 'passed': True})
        else:
            raise AssertionError('leaf order was accepted')
    report = {'schema_version': 1, 'network_calls': 0, 'real_credentials_used': False,
              'google_auth_version': importlib.metadata.version('google-auth'), 'cases': cases,
              'probe_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              'source_sha256': {module.__name__: hashlib.sha256(Path(module.__file__).read_bytes()).hexdigest()
                                for module in [identity_pool, external_account, _mtls_helper]}}
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
