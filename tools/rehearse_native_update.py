#!/usr/bin/env python3
"""在临时目录安装并回滚原生 CLI，可验证 loopback 候选或已发布的 GitHub Release。"""

import argparse
from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import threading
from urllib.error import HTTPError
from urllib.parse import quote, unquote, urlsplit
from urllib.request import HTTPRedirectHandler, ProxyHandler, build_opener

from package_release import MAX_BINARY, SEMVER
from validate_native_release import native_target


def sha(data):
    return hashlib.sha256(data).hexdigest()


def require(condition, message):
    if not condition:
        raise ValueError(message)


def github_manifest_url(requested, artifact_url, version, name):
    path = urlsplit(artifact_url).path
    match = re.fullmatch(r'/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)/releases/download/[^/]+/[^/]+', path)
    require(match, '候选未使用固定版本的 GitHub Releases 下载路径')
    repository = match.group(1)
    require(all(part not in ('.', '..') for part in repository.split('/')), '无效的 GitHub 仓库')
    root = f'https://github.com/{repository}/releases'
    fixed = f'{root}/download/{quote("v" + version, safe="-._~")}'
    require(artifact_url == f'{fixed}/{quote(name, safe="-._~")}', '候选发行物 URL 与版本或文件名不匹配')
    require(requested in (f'{fixed}/colab-update.json', f'{root}/latest/download/colab-update.json'),
            '清单必须来自对应 GitHub 仓库的固定版本或 latest')
    return f'{fixed}/colab-update.json'


class HttpsRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        uri = urlsplit(newurl)
        if uri.scheme != 'https' or uri.username is not None or uri.password is not None:
            raise ValueError('发行清单重定向需要无认证信息的 HTTPS')
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def verify_published_manifest(url, expected):
    opener = build_opener(ProxyHandler({}), HttpsRedirect())
    try:
        with opener.open(url, timeout=30) as response:
            manifest = response.read(65537)
    except HTTPError as error:
        raise ValueError(f'发行清单返回 HTTP {error.code}') from None
    except OSError:
        raise ValueError('无法下载发行清单') from None
    require(manifest == expected, '公开清单的字节与已核对的本地候选不一致')


@contextmanager
def local_source(manifest, target, name, image):
    requests = []
    state = {'corrupt': True}
    path = '/' + name

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            requests.append(self.path)
            if self.path == '/colab-update.json':
                body = rewritten
            elif self.path == path:
                body = image[:-1] + bytes([image[-1] ^ 1]) if state['corrupt'] else image
            else:
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    with ThreadingHTTPServer(('127.0.0.1', 0), Handler) as server:
        uri = f'http://127.0.0.1:{server.server_port}'
        manifest['artifacts'][target]['url'] = uri + path
        rewritten = json.dumps(manifest).encode()
        worker = threading.Thread(target=server.serve_forever, daemon=True)
        worker.start()
        try:
            yield uri + '/colab-update.json', path, requests, state
        finally:
            server.shutdown()
            worker.join()


def rehearse(package, older, published_manifest=None):
    with (package / 'colab-update.json').open('rb') as reader:
        manifest_bytes = reader.read(65537)
    require(len(manifest_bytes) <= 65536, '候选清单超过大小上限')
    manifest = json.loads(manifest_bytes)
    target = native_target()
    require(isinstance(manifest, dict), '候选清单必须是 JSON 对象')
    version = manifest.get('version')
    require(manifest.get('schema_version') == 1 and manifest.get('package') == 'colabcli-rust'
            and isinstance(version, str) and SEMVER.fullmatch(version), '候选清单的包、版本或 schema 无效')
    artifacts = manifest.get('artifacts')
    require(isinstance(artifacts, dict) and isinstance(artifacts.get(target), dict), '候选缺少本平台发行物')
    item = artifacts[target]
    name = f'colab-{version}-{target}'
    require(isinstance(item.get('url'), str) and type(item.get('size')) is int
            and 0 < item['size'] <= MAX_BINARY, '候选发行物 URL 或大小无效')
    require(unquote(urlsplit(item['url']).path.rsplit('/', 1)[-1]) == name, '候选发行物文件名不匹配')
    with (package / name).open('rb') as reader:
        image = reader.read(item['size'] + 1)
    require(len(image) == item['size'] and sha(image) == item.get('sha256'), '候选发行物与清单不符')
    if published_manifest is not None:
        fixed_manifest = github_manifest_url(published_manifest, item['url'], version, name)
        verify_published_manifest(published_manifest, manifest_bytes)
        if fixed_manifest != published_manifest:
            verify_published_manifest(fixed_manifest, manifest_bytes)
    predecessor = older.read_bytes()

    with tempfile.TemporaryDirectory(prefix='colab-update-rehearsal-') as directory:
        root = Path(directory)
        installed = root / 'colab'
        shutil.copyfile(older, installed)
        installed.chmod(0o700)
        home = root / 'home'
        home.mkdir(mode=0o700)
        env = {'PATH': os.defpath, 'HOME': str(home)}

        def run(*arguments, success=True, offline=False):
            child_env = dict(env)
            if offline:
                child_env['COLAB_CLI_UPDATE_MANIFEST'] = 'invalid-offline-source'
            result = subprocess.run([str(installed), *arguments], env=child_env, cwd=root,
                                    capture_output=True, text=True, timeout=95)
            if (result.returncode == 0) != success:
                raise ValueError(f'{arguments[0]} 返回非预期状态码 {result.returncode}')
            return (result.stdout if success else result.stderr).strip()

        def install(uri):
            result = json.loads(run('update', '--install', '--manifest', uri, '--json'))
            require(result['status'] == 'installed', '未安装新版本')
            require(installed.read_bytes() == image, '安装结果与已核对的候选二进制不一致')
            require(run('version') == f'Version: {version}', '已安装二进制的版本不符')
            check = json.loads(run('update', '--manifest', uri, '--json'))
            require(check['latest_version'] == version and check['update_available'] is False,
                    '安装后同版本检查失败')

        before = run('version')
        if published_manifest is not None:
            install(fixed_manifest)
            source_checks = {'published_manifest': published_manifest,
                             'installation_manifest': fixed_manifest, 'remote_release_verified': True}
        else:
            with local_source(manifest, target, name, image) as (uri, path, requests, state):
                error = run('update', '--install', '--manifest', uri, '--json', success=False)
                require('SHA-256 不匹配' in error, '未报告校验和错误')
                manifest_path = '/colab-update.json'
                require(requests == [manifest_path, path], '校验失败的请求序列不符')
                require(installed.read_bytes() == predecessor, '校验失败后旧二进制被修改')
                require(not json.loads(run('update', '--status'))['rollback_available'],
                        '校验失败后留下回滚记录')
                state['corrupt'] = False
                install(uri)
                require(requests == [manifest_path, path] * 2 + [manifest_path], '安装请求序列不符')
            source_checks = {'loopback_requests': requests, 'corruption_preserved_old_binary': True,
                             'same_version_check_skipped_artifact': True, 'remote_release_verified': False}

        require(json.loads(run('update', '--status', offline=True))['rollback_available'], '未保留回滚点')
        rollback = json.loads(run('update', '--rollback', '--json', offline=True))
        require(rollback['status'] == 'rolled_back' and installed.read_bytes() == predecessor
                and run('version') == before, '回滚后旧版本或二进制不符')
        require(not json.loads(run('update', '--status', offline=True))['rollback_available'], '回滚点未消耗')
        require(installed.stat().st_mode & 0o777 == 0o700, '安装文件的权限不符')
        return {'target': target, 'before': before, 'installed_version': version,
                'candidate_manifest_sha256': sha(manifest_bytes), 'artifact_sha256': sha(image),
                'predecessor_sha256': sha(predecessor), 'installed_candidate_bytes_match': True,
                'offline_rollback_restored_old_bytes': True, 'mode_preserved': True, **source_checks}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--package', type=Path, required=True)
    parser.add_argument('--older', type=Path, required=True)
    parser.add_argument('--published-manifest', help='可选的同仓库 GitHub Releases HTTPS 清单')
    args = parser.parse_args()
    try:
        print(json.dumps(rehearse(args.package, args.older, args.published_manifest), indent=2))
    except (ValueError, OSError, KeyError, subprocess.TimeoutExpired) as error:
        parser.exit(1, f'更新验收失败: {error}\n')


if __name__ == '__main__':
    main()
