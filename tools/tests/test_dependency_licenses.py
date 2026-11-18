#!/usr/bin/env python3
import hashlib
import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest

TOOLS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS))
SPEC = importlib.util.spec_from_file_location('dependency_licenses', TOOLS / 'collect_dependency_licenses.py')
licenses = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(licenses)


class DependencyLicenseTests(unittest.TestCase):
    def test_graph_keeps_build_and_shared_edges_but_excludes_dev_only_cycles(self):
        def edge(key, *kinds):
            return {'pkg': key, 'dep_kinds': [{'kind': kind} for kind in kinds]}
        metadata = {'resolve': {'root': 'root', 'nodes': [
            {'id': 'root', 'deps': [edge('normal', None), edge('build', 'build'), edge('dev', 'dev'), edge('shared', 'dev', None)]},
            {'id': 'normal', 'deps': [edge('root', None)]},
            {'id': 'build', 'deps': []}, {'id': 'dev', 'deps': []}, {'id': 'shared', 'deps': []}]}}
        self.assertEqual(licenses.dependency_ids(metadata), {'root', 'normal', 'build', 'shared'})

    def fixture(self, root, name):
        path = root / name
        path.mkdir()
        return {'name': name, 'version': '1.0.0', 'source': 'registry+https://example.invalid',
                'manifest_path': str(path / 'Cargo.toml'), 'license': 'MIT', 'license_file': None}

    def test_documents_deduplicate_without_losing_per_package_attribution(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            packages = [self.fixture(root, name) for name in ('one', 'two')]
            for package in packages:
                (Path(package['manifest_path']).parent / 'LICENSE').write_text('fixture copyright and license\n')
            result = licenses.collect(packages, b'lock fixture', {'target': 2})
            self.assertEqual(len(result['packages']), 2)
            self.assertEqual(len(result['documents']), 1)
            self.assertEqual(result['cargo_lock_sha256'], hashlib.sha256(b'lock fixture').hexdigest())
            self.assertNotIn(str(root), str(result))
            for package in result['packages']:
                self.assertEqual(package['files'][0]['path'], 'LICENSE')
                self.assertIn(package['files'][0]['sha256'], result['documents'])

    def test_missing_or_external_license_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package = self.fixture(root, 'one')
            with self.assertRaisesRegex(ValueError, '缺少'):
                licenses.collect([package], b'lock', {})
            outside = root / 'external'
            outside.write_text('not inside package')
            package['license_file'] = '../external'
            with self.assertRaisesRegex(ValueError, '包内'):
                licenses.collect([package], b'lock', {})

    def test_symlink_license_is_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package = self.fixture(root, 'one')
            (root / 'external').write_text('external fixture')
            (root / 'one/LICENSE').symlink_to(root / 'external')
            with self.assertRaisesRegex(ValueError, '包内'):
                licenses.collect([package], b'lock', {})


if __name__ == '__main__':
    unittest.main()
