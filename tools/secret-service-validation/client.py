#!/usr/bin/env python3
"""Synthetic interop client; never select the user's configured keyring backend."""
import json
import os
import sys
import keyring
from keyring.backends.SecretService import Keyring


def guard():
    if os.geteuid() != 1000 or os.environ.get('HOME') != '/validation/home':
        raise RuntimeError('isolated container required')
    if not os.environ.get('DBUS_SESSION_BUS_ADDRESS', '').startswith('unix:path=/tmp/'):
        raise RuntimeError('private temporary session bus required')
    if os.environ.get('COLAB_NATIVE_STORE_TEST_ISOLATED') != '1':
        raise RuntimeError('explicit isolated test opt-in required')


def main():
    guard()
    keyring.set_keyring(Keyring())
    action, slot = sys.argv[1:]
    if not (slot == 'oauth_token' or slot.startswith('oauth_token:')):
        raise RuntimeError('unexpected synthetic slot')
    if action == 'set':
        raw = sys.stdin.read(65537)
        if len(raw) > 65536 or not isinstance(json.loads(raw), dict):
            raise RuntimeError('invalid synthetic input')
        keyring.set_password('google-colab-cli', slot, raw)
    elif action == 'check':
        expected = json.loads(sys.stdin.read(65537))
        raw = keyring.get_password('google-colab-cli', slot)
        if raw is None or json.loads(raw) != expected:
            raise RuntimeError('synthetic interop value differs')
    elif action == 'missing':
        if keyring.get_password('google-colab-cli', slot) is not None:
            raise RuntimeError('synthetic entry was not deleted')
    elif action == 'delete':
        keyring.delete_password('google-colab-cli', slot)
    else:
        raise RuntimeError('unknown test action')


if __name__ == '__main__':
    main()
