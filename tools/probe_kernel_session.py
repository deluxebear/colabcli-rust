#!/usr/bin/env python3
"""Observe fixed upstream constructor/session propagation without network or credentials."""
import argparse
import hashlib
import importlib.metadata
import json
from pathlib import Path
import socket
import sys
import sysconfig
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kernel', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if not sys.flags.no_site:
        parser.error('run Python with -S to disable installed CLI .pth hooks')
    for name in ('purelib', 'platlib'):
        directory = sysconfig.get_path(name)
        if directory not in sys.path:
            sys.path.append(directory)
    root = args.kernel.resolve()
    inventory = json.loads((Path(__file__).resolve().parents[1] / 'research/source-inventory.json').read_text())
    baseline = inventory['repositories']['kernel']
    hashes = {}
    for row in baseline['files']:
        if row['path'].startswith('jupyter_kernel_client/') and row['path'].endswith('.py'):
            actual = hashlib.sha256((root / row['path']).read_bytes()).hexdigest()
            if actual != row['sha256']:
                raise RuntimeError('fixed kernel source hash mismatch: ' + row['path'])
            hashes[row['path']] = actual
    sys.path.insert(0, str(root))
    import jupyter_kernel_client as package
    from jupyter_kernel_client import manager, wsclient
    for module in (package, manager, wsclient):
        if not Path(module.__file__).resolve().is_relative_to(root):
            raise RuntimeError('unexpected installed module used instead of fixed baseline')

    def forbidden(*args, **kwargs):
        raise AssertionError('constructor probe forbids real network')

    class Response:
        def json(self):
            return {'id': 'synthetic-kernel', 'execution_state': 'idle'}

    class NoThread:
        def __init__(self, **kwargs):
            pass

        def start(self):
            pass

        def is_alive(self):
            return False

    cases = []
    with patch.object(socket, 'socket', forbidden), patch.object(socket, 'create_connection', forbidden), \
            patch.object(socket, 'getaddrinfo', forbidden), \
            patch.object(manager, 'fetch', return_value=Response()) as fetch, \
            patch.object(wsclient, 'Thread', NoThread):
        for supplied in (None, 'persisted-session-a', 'persisted-session-b'):
            kwargs = {'subprotocol': package.JupyterSubprotocol.DEFAULT,
                      'extra_params': {'colab-runtime-proxy-token': 'synthetic-extra-value'}}
            if supplied is not None:
                kwargs['session'] = supplied
            client = package.KernelClient(
                server_url='https://constructor.invalid/prefix', token='synthetic-runtime-token',
                kernel_id='synthetic-kernel', client_kwargs=kwargs,
                headers={'X-Colab-Client-Agent': 'colab-cli',
                         'X-Colab-Runtime-Proxy-Token': 'synthetic-runtime-token'})
            client._own_kernel = False
            connection = client._manager.client
            connection.connection_ready.set()
            try:
                with patch.object(wsclient.websocket, 'WebSocketApp') as app:
                    connection.start_channels(shell=False, iopub=False, stdin=False, hb=False, control=False)
                    url = app.call_args.args[0]
                    query = parse_qs(urlsplit(url).query)
                    header_session = connection.session.msg('kernel_info_request')['header']['session']
                    assert query['session_id'] == [header_session]
                    assert query['token'] == ['synthetic-runtime-token']
                    assert 'colab-runtime-proxy-token' not in query
                    assert header_session and header_session != supplied
                    assert app.call_args.kwargs['header']['X-Colab-Runtime-Proxy-Token'] == 'synthetic-runtime-token'
                    assert app.call_args.kwargs['subprotocols'] == []
                    cases.append({'supplied_session': supplied,
                                  'message_session_matches_supplied': header_session == supplied,
                                  'query_session_matches_message': True,
                                  'extra_params_forwarded_to_query': False,
                                  'token_query_and_runtime_header_preserved': True,
                                  'default_subprotocol': True})
            finally:
                client.stop()
        assert fetch.call_count == 3
        assert all(call.kwargs['method'] == 'GET' for call in fetch.call_args_list)
    args.output.write_text(json.dumps({
        'source_commit': baseline['commit'], 'source_sha256': hashes,
        'dependencies': {name: importlib.metadata.version(name) for name in
                         ('jupyter-client', 'traitlets', 'requests', 'websocket-client')},
        'scope': 'Actual fixed KernelClient/manager/WS constructors and URL builder; synthetic REST response and inert WebSocket/thread boundaries',
        'network': 'forbidden after dependency imports', 'real_colab': False,
        'cases': cases,
        'limitations': ['No WebSocket connection or remote kernel execution',
                        'Does not prove whether Google requires a particular session identity']
    }, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({'cases': len(cases), 'supplied_session_consumed': False, 'network': 'forbidden'}))


if __name__ == '__main__':
    main()
