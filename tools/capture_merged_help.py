#!/usr/bin/env python3
"""采集固定 CLI+扩展的运行时 help 契约；禁止网络、钥匙串、浏览器及外部程序。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def fingerprint_sources(paths):
    manifest = json.loads((ROOT / 'research/source-inventory.json').read_text())
    result = {}
    for name, path in paths.items():
        baseline = manifest['repositories'][name]
        head = subprocess.check_output(['git', '-C', str(path), 'rev-parse', 'HEAD'], text=True).strip()
        if head != baseline['commit']:
            raise ValueError(f'{name} HEAD differs from pinned source')
        for item in baseline['files']:
            if hashlib.sha256((path / item['path']).read_bytes()).hexdigest() != item['sha256']:
                raise ValueError(f'{name} source hash mismatch: {item["path"]}')
        result[name] = {'repository': baseline['repository'], 'commit': head, 'verified_files': len(baseline['files'])}
    return result


def rust_options(help_text):
    """Only declarations at the beginning of a help row; never prose mentions."""
    found = set()
    for line in help_text.splitlines():
        match = re.match(r'^\s+((?:-[a-zA-Z0-9],\s+)?--[a-zA-Z0-9][a-zA-Z0-9-]*|-[a-zA-Z0-9])(?=[\s=\[<]|$)', line)
        if match:
            found.update(re.findall(r'--[a-zA-Z0-9][a-zA-Z0-9-]*|-[a-zA-Z0-9]', match[1]))
        aliases = re.search(r'\[alias(?:es)?: ([^]]+)\]', line)
        if aliases:
            found.update(re.findall(r'--[a-zA-Z0-9][a-zA-Z0-9-]*|-[a-zA-Z0-9]', aliases[1]))
    return sorted(found)


def normalize(text, home):
    text = re.sub(r'\x1b\[[0-9;]*m', '', text)
    return text.replace(str(home), '<HOME>').replace(str(home.resolve()), '<HOME>')


def probe(args):
    # Executed with -S and a fresh HOME. No installed .pth/loader may run.
    sys.path[:0] = [str(args.cli / 'src'), str(args.kernel), str(args.ext), str(args.deps)]
    import socket
    import webbrowser
    violations = []

    def forbidden(*_args, **_kwargs):
        violations.append('forbidden external operation')
        raise RuntimeError('merged-help probe forbids network, credentials, browser and subprocess')

    socket.getaddrinfo = socket.socket.connect = socket.socket.connect_ex = forbidden
    socket.create_connection = forbidden
    webbrowser.open = webbrowser.open_new = webbrowser.open_new_tab = forbidden
    subprocess.Popen = subprocess.run = subprocess.call = subprocess.check_call = subprocess.check_output = forbidden
    os.system = os.popen = forbidden
    import keyring
    keyring.get_password = keyring.set_password = keyring.delete_password = forbidden
    from colab_cli import auto_update
    auto_update.run_background_check = lambda: None
    from colab_cli.cli import app
    import colab_ext
    import jupyter_kernel_client
    assert Path(colab_ext.__file__).resolve().is_relative_to(args.ext.resolve())
    assert Path(jupyter_kernel_client.__file__).resolve().is_relative_to(args.kernel.resolve())
    import colab_cli.cli
    assert Path(colab_cli.cli.__file__).resolve().is_relative_to(args.cli.resolve())
    assert colab_ext.register(app), 'extension registration failed'
    try:
        import typer._click as click
    except ImportError:
        import click
    import typer.main
    from typer.testing import CliRunner
    command = typer.main.get_command(app)
    runner = CliRunner()
    records = []

    def simple(value):
        if isinstance(value, str):
            return normalize(value, args.home)
        if value is None or isinstance(value, (bool, int, float)):
            return value
        if isinstance(value, (list, tuple)):
            return [simple(item) for item in value]
        return normalize(str(value), args.home)

    def walk(node, path):
        parameters = []
        for param in node.get_params(click.Context(node)):
            param_type = getattr(param, 'type', None)
            annotation = getattr(getattr(param, 'type_descriptor', None), 'annotation', None)
            type_name = getattr(param_type, 'name', None) or getattr(annotation, '__name__', str(annotation))
            choices = getattr(param_type, 'choices', None)
            if choices is None and hasattr(annotation, '__members__'):
                choices = [item.value for item in annotation]
            parameters.append({'name': param.name, 'kind': param.param_type_name,
                'options': list(param.opts), 'secondary_options': list(param.secondary_opts),
                'required': param.required, 'multiple': param.multiple, 'nargs': param.nargs,
                'is_flag': getattr(param, 'is_flag', False), 'type': type_name,
                'choices': simple(choices), 'default': simple(param.default)})
        outputs = {}
        for flag in ('--help', '-h'):
            result = runner.invoke(app, path + [flag], prog_name='colab', color=False)
            outputs[flag] = {'exit_code': result.exit_code, 'stdout': normalize(result.stdout, args.home),
                'stderr': normalize(result.stderr, args.home)}
        records.append({'path': path, 'hidden': node.hidden, 'parameters': parameters,
            'allow_extra_args': node.allow_extra_args, 'ignore_unknown_options': node.ignore_unknown_options,
            'help': outputs})
        if hasattr(node, 'commands'):
            for name, child in sorted(node.commands.items()):
                walk(child, path + [name])
    walk(command, [])
    import importlib.metadata
    distributions = {d.metadata['Name']: d.version for d in importlib.metadata.distributions(path=[str(args.deps)])}
    print(json.dumps({'schema_version': 1, 'python_version': sys.version.split()[0], 'commands': records, 'forbidden_operations': violations,
        'dependency_versions': distributions}, ensure_ascii=False))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('cli', 'ext', 'kernel'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--python', type=Path)
    parser.add_argument('--deps', type=Path)
    parser.add_argument('--rust', type=Path, default=ROOT / 'target/debug/colab')
    parser.add_argument('--output', type=Path, default=ROOT / 'research/merged-help.json')
    parser.add_argument('--child', action='store_true', help=argparse.SUPPRESS)
    parser.add_argument('--home', type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.child:
        return probe(args)
    if not args.python:
        parser.error('--python is required')
    sources = fingerprint_sources({name: getattr(args, name) for name in ('cli', 'ext', 'kernel')})
    if args.deps is None:
        args.deps = Path(subprocess.check_output([str(args.python), '-S', '-c', 'import sysconfig; print(sysconfig.get_path("purelib"))'], text=True).strip())
    with tempfile.TemporaryDirectory(prefix='ch-', dir='/tmp' if Path('/tmp').is_dir() else None) as temporary:
        home = Path(temporary)
        env = {'HOME': str(home), 'PATH': os.environ.get('PATH', ''), 'LANG': 'C.UTF-8',
            'COLAB_CLI_CONFIG_DIR': str(home / 'config'), 'XDG_CONFIG_HOME': str(home / '.config'),
            'COLUMNS': '120', 'LINES': '40', 'NO_COLOR': '1', 'TERM': 'dumb'}
        command = [str(args.python), '-S', str(Path(__file__).resolve()), '--child', '--home', str(home), '--deps', str(args.deps)]
        for name in ('cli', 'ext', 'kernel'):
            command += ['--' + name, str(getattr(args, name).resolve())]
        run = subprocess.run(command, capture_output=True, text=True, env=env, cwd=home, timeout=90)
        if run.returncode:
            sys.stderr.write(run.stderr)
            raise SystemExit('merged help probe failed')
        report = json.loads(run.stdout)
        report.update(sources=sources, rust_binary_sha256=hashlib.sha256(args.rust.read_bytes()).hexdigest(),
            isolation={'site_hooks': False, 'network': 'forbidden', 'keyring': 'forbidden',
                'browser': 'forbidden', 'subprocess_in_official_probe': 'forbidden', 'home': 'temporary',
                'background_update_check': 'disabled',
                'scope': 'runtime command registration, parameter metadata and actual help invocation only'})
        failures = []
        for item in report['commands']:
            path = item['path']
            if item['help']['--help']['exit_code'] != 0:
                failures.append('official help: ' + ' '.join(path))
            run = subprocess.run([str(args.rust.resolve()), *path, '--help'], capture_output=True, text=True, env=env, cwd=home, timeout=10)
            item['rust'] = {'exit_code': run.returncode, 'stdout': normalize(run.stdout, home), 'stderr': normalize(run.stderr, home)}
            flags = set(rust_options(run.stdout))
            item['rust']['declared_options'] = sorted(flags)
            official_flags = {flag for param in item['parameters'] if param['kind'] == 'option' for flag in param['options'] + param['secondary_options']}
            item['options_not_shown_in_rust_help'] = sorted(official_flags - flags)
            if run.returncode:
                failures.append('Rust help: ' + ' '.join(path))
        report['failures'] = failures
        report['summary'] = {'commands_including_root_and_hidden_aliases': len(report['commands']),
            'missing_rust_commands': sum(item['rust']['exit_code'] != 0 for item in report['commands']),
            'commands_with_options_not_shown_in_rust_help': sum(bool(item['options_not_shown_in_rust_help']) for item in report['commands']),
            'official_short_help_failures': sum(item['help']['-h']['exit_code'] != 0 for item in report['commands'])}
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
        print(json.dumps(report['summary']))
        if failures or report['forbidden_operations']:
            raise SystemExit(1)


if __name__ == '__main__':
    main()
