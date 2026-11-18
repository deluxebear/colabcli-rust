#!/usr/bin/env python3
"""Probe reference metadata policy/adapter offline, without reading host certificate files."""
import ast
import hashlib
import importlib.metadata
import json
from pathlib import Path
import tempfile
import types
from unittest import mock

import requests
from google.auth import environment_vars, exceptions
from google.auth.compute_engine import _metadata, _mtls


def main():
    source = Path(_mtls.__file__).read_bytes()
    functions = [node for node in ast.parse(source).body if isinstance(node, ast.FunctionDef)
                 and node.name in {'_parse_mds_mode', 'should_use_mds_mtls'}]
    assert len(functions) == 2
    namespace = {'os': types.SimpleNamespace(environ={}), 'environment_vars': environment_vars,
                 'MdsMtlsMode': _mtls.MdsMtlsMode, 'MdsMtlsConfig': _mtls.MdsMtlsConfig,
                 '_certs_exist': _mtls._certs_exist, 'exceptions': exceptions}
    exec(compile(ast.Module(body=functions, type_ignores=[]), 'reference-metadata-policy', 'exec'), namespace)
    selections = []
    with tempfile.TemporaryDirectory(prefix='colab-mds-policy-') as directory:
        root, bundle = Path(directory) / 'root', Path(directory) / 'bundle'
        config = _mtls.MdsMtlsConfig(ca_cert_path=root, client_combined_cert_path=bundle)
        for mode in ['none', 'strict', 'default']:
            for root_exists, bundle_exists in [(False, False), (True, False), (False, True), (True, True)]:
                for path, exists in [(root, root_exists), (bundle, bundle_exists)]:
                    path.unlink(missing_ok=True)
                    if exists:
                        path.write_bytes(b'synthetic-presence-only')
                namespace['os'].environ = {'GCE_METADATA_MTLS_MODE': mode}
                try:
                    enabled = namespace['should_use_mds_mtls'](config)
                    error = False
                except exceptions.MutualTLSChannelError:
                    enabled, error = None, True
                assert error == (mode == 'strict' and not (root_exists and bundle_exists))
                if not error:
                    assert enabled == (mode != 'none' and root_exists and bundle_exists)
                selections.append({'mode': mode, 'root_exists': root_exists, 'bundle_exists': bundle_exists,
                                   'enabled': enabled, 'error': error})
    adapters = []
    for mode in [_mtls.MdsMtlsMode.STRICT, _mtls.MdsMtlsMode.DEFAULT]:
        for outcome in ['ok', 'status_503', 'tls_error']:
            # Avoid __init__: SSL material loading is tested by Rust's real TLS fixture.
            adapter = _mtls.MdsMtlsAdapter.__new__(_mtls.MdsMtlsAdapter)
            fallback = mock.Mock()
            fallback.send.return_value = 'fallback-response'
            adapter._fallback_adapter = fallback
            request = requests.Request('GET', 'https://metadata.google.internal/fixture',
                                       headers={'Metadata-Flavor': 'Google'}).prepare()
            response = requests.Response()
            response.status_code = 503 if outcome == 'status_503' else 200
            response.url = request.url
            with mock.patch.object(_mtls, '_parse_mds_mode', return_value=mode), mock.patch.object(
                    requests.adapters.HTTPAdapter, 'send',
                    side_effect=requests.exceptions.SSLError('synthetic') if outcome == 'tls_error' else None,
                    return_value=response):
                try:
                    adapter.send(request)
                    raised = False
                except requests.exceptions.SSLError:
                    raised = True
            used = fallback.send.call_count
            assert used == int(mode == _mtls.MdsMtlsMode.DEFAULT and outcome != 'ok')
            assert raised == (mode == _mtls.MdsMtlsMode.STRICT and outcome == 'tls_error')
            assert request.url.startswith('http:' if used else 'https:')
            adapters.append({'mode': mode.value, 'outcome': outcome, 'http_fallbacks': used, 'raised': raised})
    from urllib.parse import urlsplit
    metadata_source = Path(_metadata.__file__).read_bytes()
    root_function = next(node for node in ast.parse(metadata_source).body
                         if isinstance(node, ast.FunctionDef) and node.name == '_get_metadata_root')
    root_namespace = {}
    exec(compile(ast.Module(body=[root_function], type_ignores=[]), 'reference-metadata-root', 'exec'), root_namespace)
    ports = []
    for authority, expected in [('metadata.google.internal:80', 80), ('[::1]:80', 80),
                                ('metadata.google.internal', 443)]:
        root_namespace['_GCE_METADATA_HOST'] = authority
        uri = urlsplit(root_namespace['_get_metadata_root'](True))
        assert (uri.port or 443) == expected
        ports.append({'authority': authority, 'https_port': expected})
    print(json.dumps({'schema_version': 1, 'google_auth_version': importlib.metadata.version('google-auth'),
                      'network_calls': 0, 'host_certificates_read': False, 'selection_cases': selections,
                      'adapter_cases': adapters, 'port_cases': ports,
                      'metadata_source_sha256': hashlib.sha256(metadata_source).hexdigest(),
                      'source_sha256': hashlib.sha256(source).hexdigest(),
                      'probe_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}, indent=2))


if __name__ == '__main__':
    main()
