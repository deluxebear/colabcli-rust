#!/usr/bin/env python3
"""验证 Rust run 包装模板，可选启动本机 IPython kernel。"""
import argparse
import contextlib
from datetime import date
import hashlib
import importlib.metadata
import io
import json
import os
from pathlib import Path
import re
import sys
import tempfile
import traceback
import types


def make_payload(template, mime, data):
    literal = json.dumps(json.dumps(data, ensure_ascii=False), ensure_ascii=False)
    return template.format(literal=literal, RESULT_MIME=mime)


def check_kernel(template, mime):
    from jupyter_client import KernelManager

    def execute(client, code):
        msg_id = client.execute(code, store_history=False, allow_stdin=False)
        outputs = []
        while True:
            message = client.get_iopub_msg(timeout=10)
            if message["parent_header"].get("msg_id") != msg_id:
                continue
            kind, content = message["msg_type"], message["content"]
            if kind == "status" and content["execution_state"] == "idle":
                break
            outputs.append((kind, content))
        while True:
            reply = client.get_shell_msg(timeout=10)
            if reply["parent_header"].get("msg_id") == msg_id:
                return reply["content"]["status"], outputs

    restore_check = """
assert sys.modules['__main__'] is original_main
assert sys.argv is original_argv
assert 'Record' not in globals()
assert '__colab_cli_run__' not in globals()
"""
    cases = [
        ("main identity and pickle",
         "import __main__, pickle, sys\nfrom dataclasses import dataclass\n"
         "@dataclass\nclass Record:\n    field: int\n"
         "assert __main__.__dict__ is globals()\n"
         "assert pickle.loads(pickle.dumps(Record(7))) == Record(7)\n"
         "assert sys.argv == ['fixture.py', '中文']\n", "ok", 0),
        ("system exit", "raise SystemExit(23)\n", "ok", 23),
        ("exception preserves line", "\nrecord = 1\nraise ValueError('fixture-error')\n", "error", 1),
    ]
    checks = []
    with tempfile.TemporaryDirectory(prefix="colab-run-kernel-") as root:
        kernel = KernelManager(ip="127.0.0.1")
        kernel.start_kernel(cwd=root, env={
            "PATH": os.defpath,
            "IPYTHONDIR": str(Path(root) / "ipython"),
            "JUPYTER_CONFIG_DIR": str(Path(root) / "jupyter"),
        })
        try:
            client = kernel.client()
            client.start_channels()
            try:
                client.wait_for_ready(timeout=20)
                status, _ = execute(client, "import sys\noriginal_main = sys.modules['__main__']\noriginal_argv = sys.argv")
                assert status == "ok"
                for name, code, expected_status, exit_code in cases:
                    data = {"source": code, "file": "fixture.py", "argv": ["fixture.py", "中文"], "nonce": "kernel-probe"}
                    status, outputs = execute(client, make_payload(template, mime, data))
                    assert status == expected_status, (name, status, outputs)
                    markers = [item["data"][mime] for kind, item in outputs if kind == "display_data" and mime in item["data"]]
                    assert markers == [{"nonce": data["nonce"], "exit_code": exit_code}], (name, markers)
                    if status == "error":
                        error, = [item for kind, item in outputs if kind == "error"]
                        assert (error["ename"], error["evalue"]) == ("ValueError", "fixture-error"), error
                        trace = re.sub(r"\x1b\[[0-9;]*m", "", "\n".join(error["traceback"]))
                        assert "fixture.py:3" in trace, trace
                    assert execute(client, restore_check)[0] == "ok", name
                    checks.append({"case": name, "shell_status": status, "script_exit_code": exit_code, "kernel_state_restored": True})
            finally:
                client.stop_channels()
        finally:
            kernel.shutdown_kernel(now=True)

    return {
        "python": sys.version.split()[0],
        "dependencies": {name: importlib.metadata.version(name) for name in ["ipykernel", "ipython", "jupyter-client"]},
        "checks": checks,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--kernel", action="store_true", help="另启动本机 ipykernel 执行消息级验证")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    source = (root / "src/runs.rs").read_text()
    match = re.search(r'let payload = format!\(\s*(?:r#"(.*?)"#|("(?:[^"\\]|\\.)*"))\s*\);', source, re.S)
    if not match:
        raise SystemExit("cannot identify the Rust run wrapper literal")
    template = match[1] if match[1] is not None else json.loads(match[2])
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
            ("script main module supports imports and pickle",
             "import __main__, pickle\nclass Record: pass\n"
             "assert __main__.__dict__ is globals()\n"
             "assert type(pickle.loads(pickle.dumps(Record()))) is Record", 0, None, "", ""),
            ("runtime error keeps physical source line", "\nimport sys\nraise ValueError('line three')",
             1, ValueError, "", ""),
        ]
        for name, code, expected_code, exception, stdout, stderr in cases:
            recorded = []
            fake_display.display = lambda value, raw: recorded.append(value) if raw else None
            data = {"source": code, "file": "test script.py", "argv": ["test script.py", "quote'\\中文", "--env", "VALUE=literal"], "nonce": "fixture-nonce"}
            payload = make_payload(template, mime, data)
            out, err = io.StringIO(), io.StringIO()
            caught = None
            namespace = {}
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                try:
                    exec(compile(payload, "<run-wrapper>", "exec"), namespace)
                except BaseException as error:
                    caught = error
            assert (type(caught) if caught else None) == exception, (name, caught)
            assert recorded == [{mime: {"nonce": "fixture-nonce", "exit_code": expected_code}}], (name, recorded)
            assert out.getvalue() == stdout, (name, out.getvalue())
            assert err.getvalue() == stderr, (name, err.getvalue())
            assert sys.argv is previous_argv, name
            assert sys.modules["__main__"] is original_modules["__main__"], name
            if name == "runtime error keeps physical source line":
                frame = traceback.extract_tb(caught.__traceback__)[-1]
                assert (frame.filename, frame.lineno) == ("test script.py", 3), frame
            assert "__colab_cli_run__" not in namespace, name
            checks.append(name)
    finally:
        for name in ["IPython", "IPython.display"]:
            if name in original_modules:
                sys.modules[name] = original_modules[name]
            else:
                sys.modules.pop(name, None)
        sys.argv = previous_argv
    result = {"date": date.today().isoformat(), "source": "src/runs.rs", "source_sha256": hashlib.sha256(source.encode()).hexdigest(), "passed_checks": checks,
              "limitations": ["synthetic local Python scripts and fake IPython.display", "does not verify Colab/IPython integration or Google compatibility"]}
    if args.kernel:
        result["local_kernel"] = check_kernel(template, mime)
        result["limitations"] = ["synthetic scripts in a local Python process and a local IPython kernel",
                                 "does not verify the Colab gateway or Google's runtime version"]
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"passed": len(checks), "output": str(args.output)}, ensure_ascii=False))


if __name__ == "__main__":
    main()
