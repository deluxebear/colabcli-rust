#!/usr/bin/env python3
"""离线生成原生发行物、更新清单与校验和；不发布、不执行输入二进制。"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import tempfile
import tomllib
from urllib.parse import quote, unquote, urlsplit
from zipfile import ZipFile, ZipInfo

ROOT = Path(__file__).resolve().parents[1]
TARGETS = {
    'aarch64-apple-darwin': ('macho', 0x0100000C),
    'x86_64-apple-darwin': ('macho', 0x01000007),
    'aarch64-unknown-linux-gnu': ('elf', 183),
    'x86_64-unknown-linux-gnu': ('elf', 62),
}
MAX_BINARY = 256 * 1024 * 1024
NOTICE_ARCHIVE = 'colab-licenses.zip'
MAX_VALIDATION_REPORT = 64 * 1024
NOTICE_FILES = (
    'THIRD_PARTY_NOTICES.md',
    'licenses/google-colab-cli-Apache-2.0.txt',
    'licenses/jupyter-kernel-client-BSD-3-Clause.txt',
    'licenses/jupyter-server-BSD-3-Clause.txt',
    'licenses/rust-dependencies.json',
)
SEMVER = re.compile(r'(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-((?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9][0-9]*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?')


def validate_base_url(raw):
    try:
        uri = urlsplit(raw)
        safe = (uri.scheme == 'https' or (uri.scheme == 'http' and uri.hostname in ('localhost', '127.0.0.1', '::1')))
        safe = safe and uri.hostname and not uri.username and not uri.password and not uri.query and not uri.fragment and '?' not in raw and '#' not in raw
        if not safe or any(c.isspace() or ord(c) < 32 or ord(c) == 127 for c in raw) or chr(92) in raw:
            raise ValueError()
        _ = uri.port
    except ValueError:
        raise ValueError('发行 base URL 无效；需要无认证/query/fragment 的 HTTPS，或 loopback HTTP') from None
    return raw.rstrip('/') + '/'


def release_metadata(cargo_toml):
    metadata = tomllib.loads(Path(cargo_toml).read_text())['package']
    version = metadata['version']
    match = SEMVER.fullmatch(version) if isinstance(version, str) else None
    if metadata['name'] != 'colabcli-rust' or not match or any(int(match.group(index)) > 2**64-1 for index in (1, 2, 3)):
        raise ValueError('Cargo package 或版本无效')
    return metadata


def release_channel(cargo_toml, repository=None, download_base_url='', manifest_url=''):
    metadata = release_metadata(cargo_toml)
    version = metadata['version']
    if repository is None:
        prefix = 'https://github.com/'
        repository_url = metadata.get('repository', '')
        if not isinstance(repository_url, str) or not repository_url.startswith(prefix):
            raise ValueError('Cargo repository 必须是 GitHub HTTPS 仓库')
        repository = repository_url.removeprefix(prefix).rstrip('/')
    if not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository) or any(
        part in ('.', '..') for part in repository.split('/')
    ):
        raise ValueError('GitHub 仓库必须为 owner/repo')
    prefix = f'https://github.com/{repository}/releases'
    tag = quote(f'v{version}', safe='-._~')
    download = validate_base_url(download_base_url or f'{prefix}/download/{tag}').rstrip('/')
    manifest = manifest_url or f'{prefix}/latest/download/colab-update.json'
    validate_base_url(manifest)
    parsed = urlsplit(download)
    if parsed.scheme != 'https' or urlsplit(manifest).scheme != 'https':
        raise ValueError('发行工作流要求公开 HTTPS 地址')
    if parsed.hostname == 'github.com' and not re.fullmatch(
        rf'/[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+/releases/download/v{re.escape(version)}', unquote(parsed.path)
    ):
        raise ValueError('GitHub 发行下载目录必须固定到当前 Cargo 版本的 tag')
    return {'download_base_url': download, 'manifest_url': manifest}


def image_matches(header, target):
    if len(header) < 64:
        return False
    kind, machine = TARGETS[target]
    if kind == 'elf':
        return header[:6] == b'\x7fELF\x02\x01' and int.from_bytes(header[18:20], 'little') == machine
    return header[:4] == b'\xcf\xfa\xed\xfe' and int.from_bytes(header[4:8], 'little') == machine


class NativeValidationError(ValueError):
    pass


def verify_native_report(directory, target, version, base_url, artifact):
    path = Path(directory) / f'validation-{target}' / 'native-validation.json'
    try:
        metadata = path.lstat()
        if not stat.S_ISREG(metadata.st_mode) or not 1 <= metadata.st_size <= MAX_VALIDATION_REPORT:
            raise ValueError()
        with path.open('rb') as reader:
            data = reader.read(MAX_VALIDATION_REPORT + 1)
        if len(data) > MAX_VALIDATION_REPORT:
            raise ValueError()
        report = json.loads(data)
    except (OSError, ValueError):
        raise NativeValidationError(f'{target} 原生验收报告缺失或无效') from None
    expected = {
        'target': target,
        'version': version,
        'packaged_version_output': f'Version: {version}',
        'release_binary_size': artifact['size'],
        'release_binary_sha256': artifact['sha256'],
    }
    if not isinstance(report, dict) or any(report.get(key) != value for key, value in expected.items()):
        raise NativeValidationError(f'{target} 原生验收报告与发行物不一致')
    checks = report.get('checks')
    if not isinstance(checks, list) or any(check not in checks for check in (
        'host native architecture', 'byte-identical packaged executable', 'packaged executable version',
    )):
        raise NativeValidationError(f'{target} 原生验收报告缺少必要的运行检查')
    reported_url = report.get('download_base_url')
    try:
        if not isinstance(reported_url, str) or validate_base_url(reported_url) != base_url:
            raise ValueError()
    except ValueError:
        raise NativeValidationError(f'{target} 原生验收报告与发行下载目录不一致') from None


def package(cargo_toml, binaries, base_url, output, allow_partial=False, notice_root=ROOT,
            validation_dir=None):
    version = release_metadata(cargo_toml)['version']
    base_url = validate_base_url(base_url)
    mapping = {}
    for item in binaries:
        target, separator, source = item.partition('=')
        if not separator or target not in TARGETS or target in mapping:
            raise ValueError('binary 必须为唯一受支持 TARGET=FILE')
        path = Path(source)
        meta = path.lstat()
        if not stat.S_ISREG(meta.st_mode) or not 64 <= meta.st_size <= MAX_BINARY:
            raise ValueError('输入必须为大小受限的普通原生可执行文件，不能为符号链接')
        mapping[target] = path
    if not mapping or (not allow_partial and mapping.keys() != TARGETS.keys()):
        raise ValueError('默认要求四个平台；单平台打包必须显式 --allow-partial')
    output = Path(output)
    if output.exists() or output.is_symlink():
        raise FileExistsError('输出目录已存在；不会覆盖已有发行物')
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.colab-release-', dir=output.parent) as temporary:
        stage = Path(temporary) / 'release'
        stage.mkdir()
        artifacts = {}
        sums = {}
        notice_archive = stage / NOTICE_ARCHIVE
        with ZipFile(notice_archive, 'x') as archive:
            for name in NOTICE_FILES:
                source = Path(notice_root) / name
                meta = source.lstat()
                if not stat.S_ISREG(meta.st_mode) or not 1 <= meta.st_size <= 1024 * 1024:
                    raise ValueError('发行声明必须为非空、大小受限的普通文件')
                data = source.read_bytes()
                if not 1 <= len(data) <= 1024 * 1024:
                    raise ValueError('发行声明读取大小无效')
                data.decode('utf-8')
                if name == 'licenses/rust-dependencies.json':
                    dependency_notices = json.loads(data)
                    if dependency_notices.get('cargo_lock_sha256') != hashlib.sha256((ROOT / 'Cargo.lock').read_bytes()).hexdigest():
                        raise ValueError('依赖许可材料与当前 Cargo.lock 不匹配')
                entry = ZipInfo(name)
                entry.create_system = 3
                entry.external_attr = (stat.S_IFREG | 0o644) << 16
                archive.writestr(entry, data)
        sums[NOTICE_ARCHIVE] = hashlib.sha256(notice_archive.read_bytes()).hexdigest()
        for target, source in sorted(mapping.items()):
            before = source.lstat()
            if not stat.S_ISREG(before.st_mode):
                raise ValueError('读取前输入文件已变化')
            name = f'colab-{version}-{target}'
            destination = stage / name
            digest = hashlib.sha256()
            size = 0
            with source.open('rb') as reader, destination.open('xb') as writer:
                header = reader.read(64)
                if not image_matches(header, target):
                    raise ValueError('原生文件格式或架构与 target 不匹配')
                reader.seek(0)
                for chunk in iter(lambda: reader.read(65536), b''):
                    size += len(chunk)
                    if size > MAX_BINARY:
                        raise ValueError('发行物超过上限')
                    digest.update(chunk)
                    writer.write(chunk)
                writer.flush()
                os.fsync(writer.fileno())
            after = source.lstat()
            if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) != (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns) or size != after.st_size:
                raise ValueError('打包期间输入文件已变化')
            destination.chmod(0o755)
            checksum = digest.hexdigest()
            artifacts[target] = {'url': base_url + quote(name, safe='-._~'), 'sha256': checksum, 'size': size}
            if validation_dir is not None:
                verify_native_report(validation_dir, target, version, base_url, artifacts[target])
            sums[name] = checksum
        manifest = {'schema_version': 1, 'package': 'colabcli-rust', 'version': version, 'artifacts': artifacts}
        manifest_bytes = (json.dumps(manifest, indent=2, sort_keys=True) + '\n').encode()
        (stage / 'colab-update.json').write_bytes(manifest_bytes)
        sums['colab-update.json'] = hashlib.sha256(manifest_bytes).hexdigest()
        (stage / 'SHA256SUMS').write_text(''.join(f'{digest}  {name}\n' for name, digest in sorted(sums.items())))
        os.rename(stage, output)
    return manifest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--cargo-toml', type=Path, default=ROOT / 'Cargo.toml')
    parser.add_argument('--binary', action='append', required=True, help='TARGET=FILE，可重复')
    parser.add_argument('--base-url', required=True)
    parser.add_argument('--output-dir', type=Path, required=True)
    parser.add_argument('--allow-partial', action='store_true')
    parser.add_argument('--validation-dir', type=Path, help='要求每个目标有对应的原生验收报告')
    args = parser.parse_args()
    try:
        result = package(args.cargo_toml, args.binary, args.base_url, args.output_dir,
                         args.allow_partial, validation_dir=args.validation_dir)
    except NativeValidationError as error:
        parser.exit(1, f'发行物生成失败: {error}\n')
    except (ValueError, OSError, KeyError, tomllib.TOMLDecodeError) as error:
        parser.exit(1, f'发行物生成失败: {error.__class__.__name__}\n')
    print(json.dumps({'version': result['version'], 'targets': sorted(result['artifacts']), 'output': str(args.output_dir)}))


if __name__ == '__main__':
    main()
