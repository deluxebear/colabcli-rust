#!/usr/bin/env python3
"""Compare history routing with the pinned merged CLI using offline synthetic profiles."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile

from capture_merged_help import fingerprint_sources, normalize

ROOT = Path(__file__).resolve().parents[1]

CASES = [
    ('active-live-wins', ['default','work'], ['default','work'], [], {}, 'default'),
    ('live-owner-before-active-history', ['work'], ['default','work'], [], {}, 'work'),
    ('live-owner-without-history', ['work'], ['default'], [], {}, 'no-history'),
    ('explicit-over-env-and-session', ['work'], ['default','work'], ['-a','default'], {'COLAB_ACCOUNT':'work'}, 'default'),
    ('env-over-session', ['work'], ['default','work'], [], {'COLAB_ACCOUNT':'default'}, 'default'),
    ('empty-explicit-falls-through-to-env', ['work'], ['default','work'], ['-a',''], {'COLAB_ACCOUNT':'default'}, 'default'),
    ('multiple-live-owners-require-selection', ['work','third'], ['default','third'], [], {}, 'ambiguous'),
    ('active-history-only', [], ['default','work'], [], {}, 'default'),
    ('stopped-history-on-nonactive-account', [], ['work'], [], {}, 'stopped-extension'),
]


def create_fixture(config, owners, histories):
    config.mkdir()
    profiles = ['default', 'work', 'third']
    registry = {'version': 1, 'active': 'default', 'accounts': {profile: {} for profile in profiles}}
    (config / 'accounts.json').write_text(json.dumps(registry))
    for profile in profiles:
        account = config / 'accounts' / profile
        history = account / 'history'
        history.mkdir(parents=True)
        state = {
            'name': 'shared', 'endpoint': 'fixture-endpoint-' + profile,
            'token': 'fixture-proxy-never-output', 'url': 'https://runtime.invalid',
        }
        (account / 'sessions.json').write_text(json.dumps({'shared': state} if profile in owners else {}))
        if profile in histories:
            event = {
                'timestamp': '2026-11-18T00:00:00.000000+00:00',
                'event_type': 'execution', 'code': 'marker-' + profile,
            }
            (history / 'shared.jsonl').write_text(json.dumps(event) + '\n')
    for path in config.rglob('*'):
        if path.is_file():
            path.chmod(0o600)


def invoke(command, arguments, owners, histories, extra, official=False):
    with tempfile.TemporaryDirectory(prefix='colab-history-fixture-') as directory:
        home = Path(directory)
        config = home / 'config'
        create_fixture(config, owners, histories)
        before = {p: p.read_bytes() for p in config.rglob('*') if p.is_file()}
        env = {
            'HOME': str(home), 'PATH': os.environ.get('PATH', ''), 'LANG': 'C.UTF-8',
            'COLAB_TOKEN_STORAGE': 'file', 'COLAB_CLI_CONFIG_DIR': str(config),
            'NO_COLOR': '1', 'TERM': 'dumb', **extra,
        }
        response = subprocess.run(command + arguments, env=env, cwd=home, capture_output=True, text=True, timeout=15)
        if official:
            response.check_returncode()
            observed = json.loads(response.stdout)
            assert not observed.pop('forbidden_operations')
        else:
            observed = {'exit_code': response.returncode, 'stdout': response.stdout, 'stderr': response.stderr}
        assert all(path.read_bytes() == value for path, value in before.items())
        assert 'fixture-proxy-never-output' not in observed['stdout'] + observed['stderr']
        return {key: normalize(value, home) if isinstance(value, str) else value for key, value in observed.items()}


def classify(official, rust, expected):
    if expected == 'ambiguous':
        assert official == rust and official['exit_code'] == 1 and not official['stdout']
        assert 'exists in multiple accounts (work, third)' in official['stderr']
        return 'matched'
    if expected == 'stopped-extension':
        assert official['exit_code'] == rust['exit_code'] == 0
        assert 'No history found' in official['stdout'] and 'marker-work' in rust['stdout']
        return 'retained_history_lookup'
    expected_text = 'No history found' if expected == 'no-history' else 'marker-' + expected
    assert official == rust and official['exit_code'] == 0 and expected_text in official['stdout'], (official, rust)
    return 'matched'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('cli', 'ext', 'kernel', 'python'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--rust', type=Path, default=ROOT / 'target/debug/colab')
    parser.add_argument('--output', type=Path, default=ROOT / 'research/merged-history-routing.json')
    args = parser.parse_args()
    sources = {key: getattr(args, key).resolve() for key in ('cli', 'ext', 'kernel')}
    fingerprints = fingerprint_sources(sources)
    python = str(args.python.absolute())
    deps = subprocess.check_output(
        [python, '-S', '-c', 'import sysconfig; print(sysconfig.get_paths()["purelib"])'], text=True,
    ).strip()
    official_command = [python, '-S', str(ROOT / 'tools/compare_merged_accounts.py'), '--child', '--deps', deps]
    for key, path in sources.items():
        official_command += ['--' + key, str(path)]
    rust_command = [str(args.rust.resolve())]

    results = []
    summary = {}
    for name, owners, histories, flags, env, expected in CASES:
        arguments = flags + ['log', '-s', 'shared']
        official = invoke(official_command, ['--', *arguments], owners, histories, env, official=True)
        rust = invoke(rust_command, arguments, owners, histories, env)
        classification = classify(official, rust, expected)
        summary[classification] = summary.get(classification, 0) + 1
        results.append({
            'name': name, 'arguments': arguments, 'official': official,
            'rust': rust, 'classification': classification,
        })
        print(name, classification)
    report = {
        'source_fingerprints': fingerprints,
        'rust_binary_sha256': hashlib.sha256(args.rust.read_bytes()).hexdigest(),
        'scope': 'offline paired CLI subprocesses with synthetic session records and history; original network, keyring, browser and subprocess use forbidden; existing fixture payloads unchanged',
        'cases': results, 'summary': summary,
    }
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')


if __name__ == '__main__':
    main()
