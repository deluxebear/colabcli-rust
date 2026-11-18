#!/usr/bin/env python3
"""原生更新线上演练的来源与清单校验测试。"""

from io import BytesIO
import json
from pathlib import Path
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import Mock, patch
from urllib.error import HTTPError

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import rehearse_native_update as rehearsal


ROOT = 'https://github.com/example/colabcli-rust/releases'
VERSION = '1.2.3+build'
NAME = f'colab-{VERSION}-aarch64-apple-darwin'
FIXED = ROOT + '/download/v1.2.3%2Bbuild/colab-update.json'
LATEST = ROOT + '/latest/download/colab-update.json'
ARTIFACT = ROOT + '/download/v1.2.3%2Bbuild/colab-1.2.3%2Bbuild-aarch64-apple-darwin'


class PublishedSources(unittest.TestCase):
    def test_matches_same_repository_and_fixed_version(self):
        for requested in (FIXED, LATEST):
            with self.subTest(requested=requested):
                self.assertEqual(rehearsal.github_manifest_url(requested, ARTIFACT, VERSION, NAME), FIXED)

    def test_refuses_changed_repository_tag_or_transport(self):
        for requested in (FIXED.replace('https://', 'http://'), FIXED + '?',
                          LATEST.replace('example/', 'other/'), FIXED.replace('1.2.3', '2.0.0')):
            with self.subTest(requested=requested), self.assertRaises(ValueError):
                rehearsal.github_manifest_url(requested, ARTIFACT, VERSION, NAME)
        for artifact in (ARTIFACT.replace('github.com', 'example.invalid'), ARTIFACT + '#',
                         ARTIFACT.replace('v1.2.3', 'v1.0.0'), ARTIFACT.replace('darwin', 'linux')):
            with self.subTest(artifact=artifact), self.assertRaises(ValueError):
                rehearsal.github_manifest_url(FIXED, artifact, VERSION, NAME)

    @patch.object(rehearsal, 'build_opener')
    def test_requires_exact_manifest_bytes_and_limits_response(self, opener):
        remote = opener.return_value
        remote.open.return_value = BytesIO(b'{"version":"1.2.3"}\n')
        rehearsal.verify_published_manifest(FIXED, b'{"version":"1.2.3"}\n')
        remote.open.assert_called_once_with(FIXED, timeout=30)
        for response in (b'{"version":"1.2.3"}', b'a' * 65538):
            with self.subTest(size=len(response)), self.assertRaisesRegex(ValueError, '字节'):
                remote.open.return_value = BytesIO(response)
                rehearsal.verify_published_manifest(FIXED, b'{"version":"1.2.3"}\n')

    @patch.object(rehearsal, 'build_opener')
    def test_reports_http_status_without_redirect_url(self, opener):
        opener.return_value.open.side_effect = HTTPError('https://example.invalid/?private=token', 404, '', {}, None)
        with self.assertRaises(ValueError) as raised:
            rehearsal.verify_published_manifest(FIXED, b'{}')
        self.assertEqual(str(raised.exception), '发行清单返回 HTTP 404')

    def test_redirect_rejects_plaintext_and_credentials(self):
        handler = rehearsal.HttpsRedirect()
        for url in ('http://example.invalid/', 'https://user:token@example.invalid/'):
            with self.subTest(url=url), self.assertRaises(ValueError):
                handler.redirect_request(Mock(), None, 302, '', {}, url)

    @patch.object(rehearsal, 'native_target', return_value='aarch64-apple-darwin')
    @patch.object(rehearsal, 'verify_published_manifest', side_effect=[None, ValueError('固定清单不可用')])
    @patch.object(rehearsal.subprocess, 'run')
    def test_latest_and_fixed_manifest_must_match_before_running_cli(self, run, verify, _target):
        image = b'synthetic local candidate'
        manifest = json.dumps({'schema_version': 1, 'package': 'colabcli-rust', 'version': VERSION,
                               'artifacts': {'aarch64-apple-darwin': {
                                   'url': ARTIFACT, 'size': len(image), 'sha256': rehearsal.sha(image),
                               }}}).encode()
        with TemporaryDirectory() as directory:
            package = Path(directory)
            (package / NAME).write_bytes(image)
            (package / 'colab-update.json').write_bytes(manifest)
            with self.assertRaisesRegex(ValueError, '固定清单不可用'):
                rehearsal.rehearse(package, package / 'unused-older', LATEST)
        self.assertEqual([call.args for call in verify.call_args_list], [(LATEST, manifest), (FIXED, manifest)])
        run.assert_not_called()


if __name__ == '__main__':
    unittest.main()
