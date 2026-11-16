#!/usr/bin/env python3
"""离线执行 Rust run 包装模板与合成脚本，检查 Python 退出语义；不请求 Colab。"""
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
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    source = (root / "src/runs.rs").read_text()
    match = re.search(r'let payload = format!\(\s*("(?:[^"\\]|\\.)*")\s*\);', source)
    if not match:
        raise SystemExit("cannot identify the Rust run wrapper literal")
    template = json.loads(match[1])
    mime = re.search(r'pub const RESULT_MIME:\s*&str\s*=\s*"([^"]+)";', source)[1]
    original_modules = dict(sys.modules)
    previous_argv = sys.argv
    checks = []
    try:
        fake_ipython = types.ModuleType("IPython")
        fake_display = types.ModuleType("IPython.display")
        sys.modules["IPython"] = fake_ipython
        sys.modules["IPython.display"] = fake_display
        cases = [
            ("normal __main__/argv/file/future", "from __future__ import annotations\nimport sys\nassert __name__ == '__main__'\nassert __file__ == 'test script.py'\nassert sys.argv == ['test script.py', \"quote'\\\\中文\", '--env', 'VALUE=literal']\nprint('ok')", 0, None, "ok\n", ""),
            ("SystemExit None", "raise SystemExit(None)", 0, None, "", ""),
            ("SystemExit zero", "raise SystemExit(0)", 0, None, "", ""),
            ("SystemExit integer", "raise SystemExit(7)", 7, None, "", ""),
            ("SystemExit negative modulo 256", "raise SystemExit(-1)", 255, None, "", ""),
            ("SystemExit string", "raise SystemExit('message')", 1, None, "", "message\n"),
            ("ordinary exception", "raise ValueError('bad')", 1, ValueError, "", ""),
            ("compile error still publishes result", "invalid syntax @@@", 1, SyntaxError, "", ""),
        ]
        for name, code, expected_code, exception, stdout, stderr in cases:
            recorded = []
            fake_display.display = lambda value, raw: recorded.append(value) if raw else None
            data = {"source": code, "file": "test script.py", "argv": ["test script.py", "quote'\\中文", "--env", "VALUE=literal"], "nonce": "fixture-nonce"}
            payload = template.format(literal=json.dumps(json.dumps(data, ensure_ascii=False), ensure_ascii=False), RESULT_MIME=mime)
            out, err = io.StringIO(), io.StringIO()
            caught = None
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                try:
                    exec(compile(payload, "<run-wrapper>", "exec"), {})
                except BaseException as error:
                    caught = error
            assert (type(caught) if caught else None) == exception, (name, caught)
            assert recorded == [{mime: {"nonce": "fixture-nonce", "exit_code": expected_code}}], (name, recorded)
            assert out.getvalue() == stdout, (name, out.getvalue())
            assert err.getvalue() == stderr, (name, err.getvalue())
            assert sys.argv is previous_argv, name
            checks.append(name)
    finally:
        for name in ["IPython", "IPython.display"]:
            if name in original_modules:
                sys.modules[name] = original_modules[name]
            else:
                sys.modules.pop(name, None)
        sys.argv = previous_argv
    result = {"date": "2026-11-16", "source": "src/runs.rs", "source_sha256": hashlib.sha256(source.encode()).hexdigest(), "passed_checks": checks,
              "limitations": ["synthetic local Python scripts and fake IPython.display", "does not verify Colab/IPython integration or Google compatibility"]}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"passed": len(checks), "output": str(args.output)}, ensure_ascii=False))


if __name__ == "__main__":
    main()
