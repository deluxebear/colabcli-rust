#!/usr/bin/env python3
"""静态提取研究基线；不导入目标模块，不读取用户凭据，不执行目标代码。"""
import argparse
import ast
import hashlib
import json
from pathlib import Path
import subprocess


REPOS = {
    "cli": ("https://github.com/googlecolab/google-colab-cli", "02d2321c38dd215b7804eebf78b61d98b856bcd3"),
    "ext": ("https://github.com/deluxebear/colab-ext", "980a39748badc3168a5af8734b2467351bdafcbc"),
    "kernel": ("https://github.com/googlecolab/jupyter-kernel-client", "f18e982c3265df5e923aa9def101ab3fd737e139"),
}


def command_calls(tree):
    registrations = {}
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call) or not isinstance(node.func, ast.Call):
            continue
        inner = node.func
        if not isinstance(inner.func, ast.Attribute) or inner.func.attr != "command":
            continue
        if node.args and isinstance(node.args[0], ast.Name):
            registrations.setdefault(node.args[0].id, []).append(ast.unparse(inner))
    return registrations


def inventory(key, root):
    url, expected = REPOS[key]
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
    if revision != expected:
        raise ValueError(f"{key}: expected {expected}, got {revision}")
    if subprocess.run(["git", "diff", "--quiet", "HEAD", "--"], cwd=root).returncode:
        raise ValueError(f"{key}: tracked files differ from fixed HEAD")
    tracked = subprocess.check_output(["git", "ls-files", "-z"], cwd=root).decode().split("\0")
    records, commands, tests = [], [], []
    for name in sorted(filter(None, tracked)):
        path = root / name
        if not path.is_file():
            continue
        data = path.read_bytes()
        records.append({"path": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()})
        if path.suffix != ".py":
            continue
        source = data.decode("utf-8-sig")
        tree = ast.parse(source)
        regs = command_calls(tree)
        for node in ast.walk(tree):
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name.startswith("test_"):
                tests.append({"path": name, "function": node.name, "line": node.lineno})
        # 顶层入口与回调；嵌套包装器的运行时改写留给人工核对。
        for node in tree.body:
            if not isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                continue
            decorators = [ast.unparse(d) for d in node.decorator_list]
            exposed = node.name in regs or any(".command(" in d or ".callback(" in d for d in decorators)
            if not exposed:
                continue
            args = node.args
            defaults = [None] * (len(args.args) - len(args.defaults)) + list(args.defaults)
            params = [{"name": p.arg, "annotation": ast.unparse(p.annotation) if p.annotation else None,
                       "default": ast.unparse(d) if d is not None else None}
                      for p, d in zip(args.args, defaults)]
            commands.append({"path": name, "function": node.name, "line": node.lineno,
                             "end_line": node.end_lineno, "registrations": regs.get(node.name, []),
                             "decorators": decorators, "parameters": params,
                             "source": f"{url}/blob/{revision}/{name}#L{node.lineno}"})
    return {"repository": url, "commit": revision, "files": records, "command_entrypoints": commands,
            "test_functions": tests, "limitations": ["静态 AST 入口不等于运行时合并后的 help", "不展开 monkey-patch、Annotated 别名或 Typer 自动生成的选项", "测试函数数不等于参数化测试用例数", "未运行认证、目标代码或线上请求"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in REPOS:
        parser.add_argument(f"--{key}", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = {"schema_version": 1, "research_date": "2026-11-16", "repositories": {
        key: inventory(key, getattr(args, key).resolve()) for key in REPOS}}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({key: {"files": len(repo["files"]), "entrypoints": len(repo["command_entrypoints"]),
                            "test_functions": len(repo["test_functions"])}
                      for key, repo in result["repositories"].items()}, ensure_ascii=False))


if __name__ == "__main__":
    main()
