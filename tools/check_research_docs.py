#!/usr/bin/env python3
"""核对研究文档相对链接、固定源码行号及清单哈希；不访问网络。"""
import argparse
import hashlib
import json
from pathlib import Path
import re


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--ext", type=Path, required=True)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    manifest = json.loads((root / "research/source-inventory.json").read_text())
    sources = {}
    errors = []
    hashes = 0
    for key, repo in manifest["repositories"].items():
        directory = getattr(args, key)
        sources[(repo["repository"], repo["commit"])] = directory
        for record in repo["files"]:
            path = directory / record["path"]
            if not path.is_file() or hashlib.sha256(path.read_bytes()).hexdigest() != record["sha256"]:
                errors.append(f"source hash mismatch: {key}/{record['path']}")
            hashes += 1
    docs = [root / "README.md", *sorted((root / "docs").glob("*.md")), *sorted((root / "research").glob("*.md"))]
    relative_links = 0
    citations = set()
    pattern = re.compile(r"https://github\.com/([^/]+/[^/]+)/blob/([0-9a-f]{40})/([^\s)\]]+)")
    for doc in docs:
        content = doc.read_text()
        if sum(line.startswith("```") for line in content.splitlines()) % 2:
            errors.append(f"unclosed fenced block: {doc.name}")
        for target in re.findall(r"\]\(([^)]+)\)", content):
            if "://" in target or target.startswith("#"):
                continue
            relative_links += 1
            if not (doc.parent / target.split("#", 1)[0]).is_file():
                errors.append(f"missing local link: {doc.name} -> {target}")
        for repo, rev, suffix in pattern.findall(content):
            citations.add((repo, rev, suffix))
            directory = sources.get((f"https://github.com/{repo}", rev))
            if directory is None:
                errors.append(f"unrecognized baseline: {repo}@{rev}")
                continue
            path, _, anchor = suffix.partition("#")
            source = directory / path
            if not source.is_file():
                errors.append(f"missing cited source: {repo}/{path}")
                continue
            if anchor:
                m = re.fullmatch(r"L(\d+)(?:-L(\d+))?", anchor)
                length = len(source.read_text().splitlines())
                if not m or not (1 <= int(m[1]) <= int(m[2] or m[1]) <= length):
                    errors.append(f"invalid source anchor: {doc.name} {suffix}")
    result = {"research_date": "2026-11-16", "checks": {"markdown_files": len(docs),
              "relative_links": relative_links, "unique_fixed_source_citations": len(citations),
              "source_file_hashes": hashes}, "errors": errors,
              "not_checked": ["Markdown rendering and Mermaid layout", "claim entailment by automation", "remote URL availability", "target code execution", "OAuth and live Colab behavior"]}
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps(result, ensure_ascii=False))
    raise SystemExit(bool(errors))


if __name__ == "__main__":
    main()
