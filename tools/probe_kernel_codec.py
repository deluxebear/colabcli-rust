#!/usr/bin/env python3
"""仅执行固定依赖中的纯编解码函数，生成无账户信息的离线协议样本。"""
import argparse
import ast
import base64
import copy
import hashlib
import json
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    manifest = json.loads((root / "research/source-inventory.json").read_text())
    repo = manifest["repositories"]["kernel"]
    name = "jupyter_kernel_client/utils.py"
    source = (args.kernel / name).read_bytes()
    expected = next(f["sha256"] for f in repo["files"] if f["path"] == name)
    assert hashlib.sha256(source).hexdigest() == expected, "source identity mismatch"
    names = {"serialize_msg_to_ws_default", "deserialize_msg_from_ws_default", "serialize_msg_to_ws_json",
             "serialize_msg_to_ws_v1", "deserialize_msg_from_ws_v1"}
    tree = ast.parse(source.decode())
    functions = [node for node in tree.body if isinstance(node, ast.FunctionDef) and node.name in names]
    assert {f.name for f in functions} == names
    # 不 import 目标包；只从已审阅 utils.py 提取这五个无 I/O 函数。
    module = ast.Module(body=functions, type_ignores=[])
    namespace = {"json": json}
    exec(compile(module, name, "exec"), namespace)
    msg = {"header": {"msg_id": "fixture-1", "session": "fixture-session", "date": "2026-11-16T00:00:00Z",
                      "msg_type": "execute_request", "version": "5.3"},
           "parent_header": {}, "metadata": {}, "content": {"code": "print('你好')", "silent": False},
           "channel": "shell"}
    checks = []
    text = namespace["serialize_msg_to_ws_json"](copy.deepcopy(msg))
    assert namespace["deserialize_msg_from_ws_default"](text) == msg
    checks.append("DEFAULT TEXT roundtrip")
    buffered = {**copy.deepcopy(msg), "buffers": [b"\x00\xff\x80hello", b""]}
    binary = namespace["serialize_msg_to_ws_default"](copy.deepcopy(buffered))
    assert namespace["deserialize_msg_from_ws_default"](binary) == buffered
    checks.append("DEFAULT BINARY non-UTF8 and empty buffer roundtrip")
    count = int.from_bytes(binary[:4], "big")
    offsets = [int.from_bytes(binary[4+i*4:8+i*4], "big") for i in range(count)]
    assert count == 3 and offsets[0] == 4 * (count + 1)
    checks.append("DEFAULT big-endian count and first offset")
    one = namespace["serialize_msg_to_ws_default"](copy.deepcopy(msg))
    try:
        namespace["deserialize_msg_from_ws_default"](one)
    except ValueError as exc:
        assert "unsupported number" in str(exc)
    else:
        raise AssertionError("expected N=1 BINARY rejection")
    checks.append("N=1 BINARY rejected by decoder; sender must choose TEXT")
    for label, index in [("invalid JSON offset", 4), ("invalid binary buffer offset", 12)]:
        bad = bytearray(binary)
        bad[index:index+4] = (len(binary) + 1).to_bytes(4, "big")
        try:
            namespace["deserialize_msg_from_ws_default"](bytes(bad))
        except ValueError:
            pass
        else:
            raise AssertionError(f"expected {label} rejection")
        checks.append(f"DEFAULT {label} rejection")
    segments = [b"{}", b"{}", b"{}", b'{"code":"1+1"}', b"\xff\x00"]
    v1 = namespace["serialize_msg_to_ws_v1"](segments, "shell")
    assert namespace["deserialize_msg_from_ws_v1"](v1) == ("shell", segments)
    assert int.from_bytes(v1[:8], "little") == len(segments) + 2
    checks.append("V1 little-endian offsets and binary roundtrip")
    output = {"research_date": "2026-11-16", "source": {"repository": repo["repository"],
              "commit": repo["commit"], "path": name, "sha256": expected},
              "passed_checks": checks, "fixtures": {"default_text": text,
              "default_binary_base64": base64.b64encode(binary).decode(), "default_count": count,
              "default_offsets": offsets, "v1_binary_base64": base64.b64encode(v1).decode()},
              "limitations": ["仅执行选定纯函数，没有导入目标包", "不是实际 Colab 抓包或互操作验证", "畸形帧覆盖有限，不代表完整安全审计"]}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(output, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"passed_checks": len(checks), "source_sha256": expected}, ensure_ascii=False))


if __name__ == "__main__":
    main()
