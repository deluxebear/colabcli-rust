#!/usr/bin/env python3
"""纯内存核对已安装 google-auth 的 STS 请求编码；不加载凭据、不发网络请求。"""
import argparse
import base64
import importlib.metadata
import json
from pathlib import Path
from urllib.parse import parse_qs, unquote


def probe():
    from google.oauth2 import sts, utils

    requests = []

    class Reply:
        status = 200
        data = b'{"access_token":"synthetic-access","expires_in":3600}'

    def request(**kwargs):
        requests.append(kwargs)
        return Reply()

    client = sts.Client('https://sts.googleapis.com/v1/token')
    client.exchange_token(request, 'urn:ietf:params:oauth:grant-type:token-exchange',
                          'synthetic-subject', 'urn:ietf:params:oauth:token-type:jwt',
                          additional_options={'userProject': '12345'})
    exchange = requests.pop()
    assert exchange['method'] == 'POST'
    assert exchange['url'] == 'https://sts.googleapis.com/v1/token'
    assert 'Authorization' not in exchange['headers']
    options = parse_qs(exchange['body'].decode())['options'][0]
    assert options.startswith('%7B%22userProject%22%3A')
    assert json.loads(unquote(options)) == {'userProject': '12345'}
    client = sts.Client('https://sts.googleapis.com/v1/oauth/token',
                        utils.ClientAuthentication(utils.ClientAuthType.basic, 'synthetic-client', 'synthetic:secret&你好'))
    client.refresh_token(request, 'synthetic-refresh')
    refresh = requests.pop()
    assert refresh['url'] == 'https://sts.googleapis.com/v1/oauth/token'
    assert refresh['method'] == 'POST'
    expected = 'Basic ' + base64.b64encode('synthetic-client:synthetic:secret&你好'.encode()).decode()
    assert refresh['headers']['Authorization'] == expected
    assert parse_qs(refresh['body'].decode()) == {'grant_type': ['refresh_token'], 'refresh_token': ['synthetic-refresh']}
    return {'google_auth_version': importlib.metadata.version('google-auth'),
            'transport': 'in-memory callback; no credentials or network',
            'checks': ['exchange has no Authorization', 'options JSON quoted before outer form encoding',
                       'refresh uses UTF-8 Basic client authentication', 'refresh body has only grant_type and refresh_token'],
            'limitations': ['installed library protocol observation only', 'no live STS or Colab acceptance']}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    result = probe()
    args.output.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({'checks': len(result['checks']), 'google_auth_version': result['google_auth_version']}))


if __name__ == '__main__':
    main()
