#!/usr/bin/env python3
"""合并 CLI 研究工具的基线篡改检测与参数提取验证；无凭据或网络。"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location('capture_merged_help', Path(__file__).resolve().parents[1] / 'capture_merged_help.py')
probe = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(probe)


class MergedHelpTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.previous_root = probe.ROOT
        self.addCleanup(setattr, probe, 'ROOT', self.previous_root)
        probe.ROOT = self.root
        (self.root / 'research').mkdir()
        self.git_env = {'HOME': str(self.root), 'PATH': os.environ.get('PATH', ''),
            'GIT_CONFIG_NOSYSTEM': '1', 'GIT_CONFIG_GLOBAL': os.devnull}
        self.source = self.root / 'source'
        self.source.mkdir()
        subprocess.run(['git', 'init', '-q', str(self.source)], check=True, env=self.git_env)
        (self.source / 'source.py').write_text('fixture = 1\n')
        subprocess.run(['git', '-C', str(self.source), 'add', 'source.py'], check=True, env=self.git_env)
        subprocess.run(['git', '-C', str(self.source), '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', '-c', 'commit.gpgsign=false', 'commit', '-qm', 'fixture'], check=True, env=self.git_env)
        head = subprocess.check_output(['git', '-C', str(self.source), 'rev-parse', 'HEAD'], text=True, env=self.git_env).strip()
        self.manifest = {'repositories': {'cli': {'repository': 'https://example.invalid/fixture', 'commit': head,
            'files': [{'path': 'source.py', 'sha256': hashlib.sha256((self.source / 'source.py').read_bytes()).hexdigest()}]}}}
        self.write_manifest()

    def write_manifest(self):
        (self.root / 'research/source-inventory.json').write_text(json.dumps(self.manifest))

    def test_verifies_head_and_every_pinned_file(self):
        result = probe.fingerprint_sources({'cli': self.source})
        self.assertEqual(result['cli']['verified_files'], 1)
        self.assertEqual(result['cli']['commit'], self.manifest['repositories']['cli']['commit'])

    def test_changed_source_is_rejected_even_when_git_head_matches(self):
        (self.source / 'source.py').write_text('fixture = 2\n')
        with self.assertRaisesRegex(ValueError, 'source hash mismatch'):
            probe.fingerprint_sources({'cli': self.source})

    def test_an_unpinned_commit_is_rejected_before_accepting_file_bytes(self):
        self.manifest['repositories']['cli']['commit'] = '0' * 40
        self.write_manifest()
        with self.assertRaisesRegex(ValueError, 'HEAD differs'):
            probe.fingerprint_sources({'cli': self.source})

    def test_help_options_include_aliases_but_exclude_prose_and_environment_values(self):
        text = '''Options:
  -A, --all
          [alias: --all-accounts]
      --config-dir <PATH>
          See --unregistered; default from --not-a-declaration.
          [env: EXAMPLE=--secret]
      --alpha <VALUE> [aliases: --first, --second]
  -h, --help  Print help
'''
        self.assertEqual(probe.rust_options(text), ['--all', '--all-accounts', '--alpha', '--config-dir', '--first', '--help', '--second', '-A', '-h'])

    def test_normalization_removes_ansi_and_temp_home_only(self):
        home = self.root / 'home'
        self.assertEqual(probe.normalize(f'\x1b[31m{home}/config\x1b[0m --flag', home), '<HOME>/config --flag')


if __name__ == '__main__':
    unittest.main()
