#!/usr/bin/env python3
"""在隔离目录执行固定官方 CLI；仅替换认证、更新检查和服务地址。由 compare_cli.py 调用。"""
import argparse
import ipaddress
import os
from pathlib import Path
import socket
import sys


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--kernel', type=Path, required=True)
    parser.add_argument('--deps', type=Path, required=True)
    parser.add_argument('--root', type=Path, required=True)
    parser.add_argument('--base', required=True)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    # Launch with -S: do not execute installed .pth hooks (including colab-ext).
    sys.path[:0] = [str(args.source / 'src'), str(args.kernel), str(args.deps)]
    real_getaddrinfo = socket.getaddrinfo
    real_connect = socket.socket.connect
    real_connect_ex = socket.socket.connect_ex

    def check(address):
        if not isinstance(address, tuple) or not ipaddress.ip_address(address[0]).is_loopback:
            raise RuntimeError('official probe forbids non-loopback connections')

    def connect(sock, address):
        check(address)
        return real_connect(sock, address)

    def connect_ex(sock, address):
        check(address)
        return real_connect_ex(sock, address)

    def getaddrinfo(host, *values, **kwargs):
        if not ipaddress.ip_address(host).is_loopback:
            raise RuntimeError('official probe forbids non-loopback DNS')
        return real_getaddrinfo(host, *values, **kwargs)

    socket.getaddrinfo = getaddrinfo
    socket.socket.connect = connect
    socket.socket.connect_ex = connect_ex
    import requests
    from colab_cli import common, auto_update
    from colab_cli.client import Client, Prod
    from colab_cli.history import HistoryLogger
    from colab_cli.state import StateStore, SettingsStore
    from colab_cli.cli import app
    import jupyter_kernel_client
    assert Path(common.__file__).resolve().is_relative_to(args.source.resolve())
    assert Path(jupyter_kernel_client.__file__).resolve().is_relative_to(args.kernel.resolve())
    assert not any(name == 'colab_ext' or name.startswith('colab_ext.') for name in sys.modules)
    import keyring
    import webbrowser

    def forbidden(*_args, **_kwargs):
        raise RuntimeError('real credential/browser access forbidden in official probe')

    keyring.get_password = keyring.set_password = keyring.delete_password = forbidden
    def browser(url, *_args, **_kwargs):
        capture = os.environ.get('FIXTURE_BROWSER_URLS')
        if not capture:
            return forbidden()
        with open(capture, 'a', encoding='utf-8') as stream:
            stream.write(url + '\n')
        return True
    webbrowser.open = browser
    auto_update.run_background_check = lambda: None
    session = requests.Session()
    session.trust_env = False
    session.headers['Authorization'] = 'Bearer compare-oauth-fixture'
    common.state._client = Client(Prod(domain=args.base, api=args.base), session)
    common.state._store = StateStore(str(args.root / 'sessions.json'))
    common.state._settings_store = SettingsStore(str(args.root / 'settings.json'))
    common.state._history = HistoryLogger(str(args.root / 'history'))
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if command and command[0] == 'whoami':
        # A real Google Credentials refresh against loopback; no command patch.
        from google.oauth2.credentials import Credentials
        from google.auth.transport.requests import AuthorizedSession
        from colab_cli import auth
        def credentials(*_args, **_kwargs):
            return AuthorizedSession(Credentials(token='compare-oauth-fixture', refresh_token='compare-refresh-fixture',
                client_id='compare-client', client_secret='compare-secret-fixture', token_uri=args.base + '/oauth/token', scopes=['openid']))
        auth.get_credentials = credentials
        import urllib.request
        original_urlopen = urllib.request.urlopen
        def tokeninfo_urlopen(url, *values, **kwargs):
            if isinstance(url, str) and url.startswith('https://oauth2.googleapis.com/tokeninfo?'):
                url = args.base + '/tokeninfo?' + url.split('?', 1)[1]
            return original_urlopen(url, *values, **kwargs)
        urllib.request.urlopen = tokeninfo_urlopen
    app(args=command, prog_name='colab')


if __name__ == '__main__':
    main()
