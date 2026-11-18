#!/usr/bin/env python3
"""纯标准库记录 google-auth 使用的 shlex.split 默认参数规则；不执行命令。"""
import argparse
import json
import platform
import shlex
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    commands = [
        '/bin/tool a b', '/bin/tool "a b" \'c d\'', '/bin/tool #literal',
        '/bin/tool a#literal', '/bin/tool "\\$value" "\\`literal"',
        '/bin/tool "a\\\\b" "a\\\"b"', '/bin/tool a\\\nb', '/bin/tool "a\\\nb"',
        '/bin/tool "" \'\'', '/bin/tool a""b', '/bin/tool a\\ b',
        '/bin/tool a\rb', '/bin/tool a\u00a0b', '/bin/tool "$HOME" `literal` ;',
        '/bin/tool "unfinished', '/bin/tool trailing\\', '/bin/tool \'unfinished',
        '/bin/tool "trailing\\', '', '  \t\n', '/bin/tool "你好 参数"',
    ]
    cases = []
    for command in commands:
        try:
            expected = shlex.split(command)
        except ValueError:
            expected = None
        cases.append({'command': command, 'expected': expected})
    args.output.write_text(json.dumps({'python_version': platform.python_version(),
        'operation': 'stdlib shlex.split, posix=True, comments=False; no subprocess',
        'cases': cases}, ensure_ascii=False, indent=2) + '\n')
    print(json.dumps({'cases': len(cases)}))


if __name__ == '__main__':
    main()
