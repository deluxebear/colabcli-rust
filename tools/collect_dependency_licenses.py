#!/usr/bin/env python3
"""从锁定 Cargo 依赖生成四个原生发行目标的许可材料；不作法律授权判断。"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

from package_release import TARGETS

ROOT = Path(__file__).resolve().parents[1]
LICENSE_NAME = re.compile(r'^(licen[cs]e|copying|copyright|notice|unlicense)([._-]|$)', re.I)


def dependency_ids(metadata):
    """Cargo 已按目标过滤节点边；保留 normal/build，排除纯 dev 边。"""
    nodes = {node['id']: node for node in metadata['resolve']['nodes']}
    seen, pending = set(), [metadata['resolve']['root']]
    while pending:
        identifier = pending.pop()
        if identifier in seen:
            continue
        seen.add(identifier)
        for edge in nodes[identifier]['deps']:
            if any(kind['kind'] != 'dev' for kind in edge['dep_kinds']):
                pending.append(edge['pkg'])
    return seen


def collect(packages, lock_bytes, target_counts):
    documents, entries = {}, []
    for package in sorted(packages, key=lambda p: (p['name'], p['version'], p['source'])):
        root = Path(package['manifest_path']).parent.resolve()
        paths = {p for p in root.rglob('*') if LICENSE_NAME.match(p.name) and p.is_file()}
        if package.get('license_file'):
            paths.add(root / package['license_file'])
        files = []
        for path in sorted(paths):
            if path.is_symlink() or not path.is_file() or not path.resolve().is_relative_to(root):
                raise ValueError('依赖许可材料必须为包内普通文件')
            if not 0 < path.stat().st_size <= 1024 * 1024:
                raise ValueError('依赖许可材料大小无效')
            data = path.read_bytes()
            digest = hashlib.sha256(data).hexdigest()
            documents[digest] = data.decode('utf-8')
            files.append({'path': path.relative_to(root).as_posix(), 'sha256': digest})
        if not files:
            raise ValueError(f'依赖缺少许可文本: {package["name"]} {package["version"]}')
        entries.append({'name': package['name'], 'version': package['version'],
                        'source': package['source'], 'license': package['license'],
                        'repository': package.get('repository'), 'files': files})
    return {'schema_version': 1, 'cargo_lock_sha256': hashlib.sha256(lock_bytes).hexdigest(),
            'scope': 'default features; normal and build dependencies; four native release targets; excludes dev-only edges',
            'target_package_counts': target_counts, 'packages': entries, 'documents': documents}


def generate(root):
    packages, selected, counts = {}, set(), {}
    for target in TARGETS:
        metadata = json.loads(subprocess.check_output(
            ['cargo', 'metadata', '--locked', '--offline', '--format-version', '1', '--filter-platform', target], cwd=root))
        identifiers = dependency_ids(metadata)
        packages.update({p['id']: p for p in metadata['packages']})
        selected.update(identifiers)
        counts[target] = len(identifiers - {metadata['resolve']['root']})
    return collect([packages[key] for key in selected if packages[key]['source'] is not None],
                   (root / 'Cargo.lock').read_bytes(), counts)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    data = (json.dumps(generate(ROOT), ensure_ascii=False, indent=2, sort_keys=True) + '\n').encode()
    output = ROOT / 'licenses/rust-dependencies.json'
    if args.check:
        if output.read_bytes() != data:
            parser.exit(1, '依赖许可材料已过期，请重新生成\n')
    else:
        output.write_bytes(data)
    print(json.dumps({'bytes': len(data), 'check': args.check}))


if __name__ == '__main__':
    main()
