#!/usr/bin/env python3
"""执行 Rust install 模板的离线替身检查；不运行 pip/uv、不访问网络或账户。"""
import argparse
import contextlib
import hashlib
import io
import json
from pathlib import Path
import re
import sys
import types


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    source = (Path(__file__).resolve().parents[1] / 'src/installation.rs').read_text()
    match = re.search(r'const INSTALL_TEMPLATE: &str = r#"(.*?)"#;', source, re.S)
    if not match:
        raise SystemExit('cannot identify install template')
    template = match[1]
    marker = '__COLAB_INSTALL_ARGUMENTS_JSON__'
    if template.count(marker) != 1:
        raise SystemExit('install template must contain one data placeholder')
    cases = [
        ('uv-success', ['demo==1.2'], None, None),
        ('uv-missing', ['demo>=1'], OSError, None),
        ('uv-error', ['demo'], RuntimeError, None),
        ('pip-error', ['demo'], OSError, RuntimeError),
        ('interrupt-no-fallback', ['demo'], KeyboardInterrupt, None),
        ('quoted-data', ["quote'\"\n中文; raise RuntimeError('injected')", marker, '--index-url', 'https://example.invalid/simple'], None, None),
        ('requirements-and-packages', ['-r', '/content/requirements #中.txt', 'extra>=1'], OSError, None),
    ]
    results = []
    previous_subprocess = sys.modules.get('subprocess')
    try:
        for name, arguments, uv_error, pip_error in cases:
            calls = []
            module = types.ModuleType('subprocess')

            def check_call(command):
                calls.append(command)
                error = uv_error if len(calls) == 1 else pip_error
                if error:
                    raise error('synthetic installer failure')

            module.check_call = check_call
            sys.modules['subprocess'] = module
            code = template.replace(marker, json.dumps(json.dumps(arguments, ensure_ascii=False), ensure_ascii=False))
            output = io.StringIO()
            caught = None
            try:
                with contextlib.redirect_stdout(output):
                    exec(compile(code, '<Rust install template>', 'exec'), {'__name__': '__main__'})
            except BaseException as error:
                caught = type(error).__name__
            expected = [['uv', 'pip', 'install', '--system', *arguments]]
            if uv_error and uv_error is not KeyboardInterrupt:
                expected.append([sys.executable, '-m', 'pip', 'install', *arguments])
            assert calls == expected, name
            if uv_error is KeyboardInterrupt:
                assert caught == 'KeyboardInterrupt' and not output.getvalue(), name
            elif pip_error:
                assert caught == pip_error.__name__ and not output.getvalue(), name
            else:
                assert caught is None, name
                assert output.getvalue() == 'Installation Complete (via ' + ('pip' if uv_error else 'uv') + ')!\n', name
            results.append({'name': name, 'passed': True, 'calls': calls, 'exception': caught})
    finally:
        if previous_subprocess is None:
            sys.modules.pop('subprocess', None)
        else:
            sys.modules['subprocess'] = previous_subprocess
    report = {'source': 'src/installation.rs', 'source_sha256': hashlib.sha256(source.encode()).hexdigest(),
              'template_sha256': hashlib.sha256(template.encode()).hexdigest(),
              'mode': 'stdlib exec with fake subprocess; no processes, network, Google or real package installs', 'checks': results}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({'passed': len(results), 'failed': 0}))


if __name__ == '__main__':
    main()
