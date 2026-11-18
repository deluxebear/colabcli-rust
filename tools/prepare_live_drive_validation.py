#!/usr/bin/env python3
"""Prepare a private, bounded DriveFS validation plan; never calls the CLI or network."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import uuid

from validate_live_ssh import private_json


def profile(value):
    if not isinstance(value, str) or not re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9._@-]{0,63}', value) or '..' in value:
        raise ValueError('invalid_profile')
    return value


def prepare(root, state, compute, drive, binary, token_storage='file', *, nonce=None):
    if nonce is not None and (not isinstance(nonce, str) or not re.fullmatch('[a-f0-9]{32}', nonce)):
        raise ValueError('invalid_retry_nonce')
    root, state, binary = root.resolve(), state.resolve(), binary.resolve()
    if state.is_relative_to(root):
        raise ValueError('state_directory_must_be_outside_configuration')
    registry_path = root / 'accounts.json'
    if registry_path.resolve() != registry_path:
        raise ValueError('symlinked_registry_refused')
    registry = json.loads(registry_path.read_text())
    compute, drive = profile(compute), profile(drive)
    if compute not in registry['accounts'] or drive not in registry['accounts']:
        raise ValueError('selected_profile_missing')
    provider = registry['accounts'][compute].get('auth_provider', 'oauth2')
    if provider not in ('oauth2', 'adc') or token_storage not in ('file', 'auto', 'keyring'):
        raise ValueError('unsupported_authentication_source')
    credential_path = root / 'accounts' / drive / 'dfs_credentials.json'
    if credential_path.resolve() != credential_path:
        raise ValueError('symlinked_credentials_refused')
    credential = json.loads(credential_path.read_text())
    if (not isinstance(credential, dict) or not all(isinstance(credential.get(k), str) and credential[k]
            for k in ('client_id', 'refresh_token', 'user_id'))
            or not isinstance(credential.get('scopes'), list)
            or 'https://www.googleapis.com/auth/drive' not in credential['scopes']):
        raise ValueError('drive_metadata_incomplete')
    identity = hashlib.sha256(json.dumps([credential['client_id'], credential['user_id']],
                                         ensure_ascii=True).encode()).hexdigest()
    # Only the non-secret identity digest is retained in the private plan.
    del credential
    nonce = uuid.uuid4().hex if nonce is None else nonce
    plan = {'schema_version': 1, 'config_dir': str(root), 'compute_account': compute, 'drive_account': drive,
            'session': 'drive-validation-' + nonce[:16], 'endpoint': None,
            'nonce': nonce, 'mountpoint': '/content/colabcli-drive-' + nonce,
            'folder': 'colabcli-validation-' + nonce, 'mode': 'shim', 'metadata_port': 0,
            'binary': str(binary), 'binary_sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
            'auth_provider': provider, 'token_storage': token_storage,
            'drive_identity_sha256': identity,
            'execution_authorized': False, 'executed': False}
    template = Path(__file__).with_name('drive_validation_payload.py').read_text()
    state.mkdir(mode=0o700)
    private_json(state / 'plan.json', plan)
    prefix = [str(binary), '--config-dir', str(root), '--account', compute, '--auth', provider,
              '--token-storage', token_storage, '--assign-retries', '0']
    session, mount = plan['session'], plan['mountpoint']
    mount_args = ['drive', 'mount', '-s', session, '--drive-account', drive, '--path', mount,
                  '--mode', 'shim', '--port', '0', '--json']
    umount_args = ['drive', 'umount', '-s', session, '--path', mount, '--purge-state', '--json']
    steps = [
        ('inventory', ['sessions', '--json']),
        ('identity_before_lease', ['drive', 'token', '--drive-account', drive, '--whoami', '--json']),
        ('allocate_owned_cpu', ['new', '-s', session, '--json']),
        ('mount_rw', mount_args),
        ('create_and_readback', ['exec', '-s', session, '--json']),
        ('unmount_and_recover_credentials', umount_args),
        ('mount_readonly', mount_args + ['--readonly']),
        ('assert_write_denied', ['exec', '-s', session, '--json']),
        ('unmount_readonly', umount_args),
        ('remount_rw_for_owned_cleanup', mount_args),
        ('remove_verified_probe_files', ['exec', '-s', session, '--json']),
        ('final_unmount_and_recover_credentials', umount_args),
        ('release_only_recorded_endpoint', ['stop', '-s', session, '--colab-expected-endpoint', '<captured-endpoint>']),
        ('confirm_backend_absence', ['sessions', '--json']),
    ]
    private_json(state / 'commands.json', {
        'executable_workflow': False,
        'environment_policy': 'Use child-only environment with COLAB_* removed; never switch process-global identity',
        'preconditions': ['No pending account/control/Drive mutation or active credential lease',
                          'Verify live user_id against pre-refresh selected Drive identity; token stdout stays in memory',
                          'Capture a new NONE CPU endpoint not present in baseline before any runtime operation',
                          'Verify account/session/endpoint before each runtime command; never use an existing user VM'],
        'steps': [{'name': name, 'argv': prefix + (
                       ['--colab-runtime-endpoint', '<captured-endpoint>']
                       if args[0] in ('exec',) or args[:2] in (['drive', 'mount'], ['drive', 'umount']) else []) + args,
                   'stdin_payload_file': ({'create_and_readback': 'create.py', 'assert_write_denied': 'readonly.py',
                                           'remove_verified_probe_files': 'cleanup.py'}.get(name)),
                   'sensitive_stdout_memory_only': name == 'identity_before_lease'} for name, args in steps],
        'recovery': ['Never blindly repeat unknown create/mount/unmount/exec mutations',
                     'Refuse cloud deletion if marker/content/type checks fail or extra entries appear',
                     'Do not release VM before confirmed unmount and credential recovery; retain ledger on failure',
                     'Resolve endpoint placeholder only from owned allocation ledger; verify backend absence'],
    })
    payloads = {}
    for phase in ('create', 'readonly', 'cleanup'):
        path = state / (phase + '.py')
        constants = {'nonce': nonce, 'mountpoint': plan['mountpoint'], 'folder': plan['folder']}
        code = 'PLAN = ' + repr(constants) + '\nPHASE = ' + repr(phase) + '\n' + template
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(descriptor, 'w') as stream:
            stream.write(code)
        payloads[path.name] = hashlib.sha256(code.encode()).hexdigest()
    report = {'schema_version': 1, 'prepared': True, 'live_validation_passed': False,
              'network_calls': 0, 'created_sessions': 0, 'cloud_writes': 0,
              'compute_and_drive_profiles_differ': compute != drive,
              'binary_sha256': plan['binary_sha256'], 'payload_sha256': payloads,
              'review_steps': len(steps), 'workflow_executable': False,
              'scope': 'One new randomly named MyDrive directory and two known files; no recursive deletion',
              'phases': ['create_and_binary_readback_on_rw_mount', 'write_denied_and_bytes_unchanged_on_ro_mount',
                         'marker_and_content_verified_nonrecursive_cleanup_on_rw_mount'],
              'limitations': ['Preparation only; no CLI, keyring, token refresh or Google requests',
                              'No Drive identity, server durability, cross-account or refresh acceptance yet',
                              'Remote execution still requires an owned CPU, mount lifecycle and explicit cloud test scope',
                              'Unexpected entries, missing owner, changed bytes or unknown result require inspection; do not blindly replay']}
    private_json(state / 'preparation.json', report)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config-dir', type=Path, required=True)
    parser.add_argument('--compute-account', required=True)
    parser.add_argument('--drive-account', required=True)
    parser.add_argument('--binary', type=Path, default=Path('target/debug/colab'))
    parser.add_argument('--state-dir', type=Path, required=True)
    parser.add_argument('--token-storage', choices=('file', 'auto', 'keyring'), default='file')
    args = parser.parse_args()
    try:
        result = prepare(args.config_dir, args.state_dir, args.compute_account, args.drive_account, args.binary,
                         args.token_storage)
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(json.dumps({'prepared': False, 'error_class': type(error).__name__, 'network_calls': 0}))
        return 1
    print(json.dumps(result))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
