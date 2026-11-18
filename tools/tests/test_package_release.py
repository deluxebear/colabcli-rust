#!/usr/bin/env python3
"""发行清单格式/完整性测试；只用临时合成原生文件头，不执行二进制。"""
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from zipfile import ZipFile

SPEC = importlib.util.spec_from_file_location('package_release', Path(__file__).resolve().parents[1] / 'package_release.py')
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.cargo = self.root / 'Cargo.toml'
        self.cargo.write_text('[package]\nname = "colabcli-rust"\nversion = "0.2.0"\n')
        self.binaries = []
        for target, (kind, machine) in release.TARGETS.items():
            data = bytearray(64)
            if kind == 'elf':
                data[:6] = b'\x7fELF\x02\x01'
                data[18:20] = machine.to_bytes(2, 'little')
            else:
                data[:4] = b'\xcf\xfa\xed\xfe'
                data[4:8] = machine.to_bytes(4, 'little')
            path = self.root / target
            path.write_bytes(data + b'synthetic-format-fixture')
            self.binaries.append(f'{target}={path}')
        self.output = self.root / 'dist'

    def package(self, **overrides):
        args = dict(cargo_toml=self.cargo, binaries=self.binaries, base_url='https://example.invalid/v0.2.0', output=self.output)
        args.update(overrides)
        return release.package(**args)

    def test_complete_manifest_and_sums_match_exact_published_bytes(self):
        result = self.package()
        self.assertEqual(set(result['artifacts']), set(release.TARGETS))
        self.assertEqual(json.loads((self.output / 'colab-update.json').read_text()), result)
        for target, artifact in result['artifacts'].items():
            path = self.output / f'colab-0.2.0-{target}'
            self.assertEqual(artifact['sha256'], hashlib.sha256(path.read_bytes()).hexdigest())
            self.assertEqual(artifact['size'], path.stat().st_size)
            self.assertTrue(artifact['url'].endswith(path.name))
        for line in (self.output / 'SHA256SUMS').read_text().splitlines():
            digest, name = line.split('  ', 1)
            self.assertEqual(digest, hashlib.sha256((self.output / name).read_bytes()).hexdigest())

    def test_partial_artifacts_require_explicit_choice(self):
        with self.assertRaises(ValueError):
            self.package(binaries=self.binaries[:1])
        self.assertFalse(self.output.exists())
        self.package(binaries=self.binaries[:1], allow_partial=True)

    def test_release_channel_derives_fixed_version_and_repository(self):
        for version in ('0.2.0', '0.3.1-rc.1+build.2'):
            with self.subTest(version=version):
                self.cargo.write_text(f'[package]\nname = "colabcli-rust"\nversion = "{version}"\n')
                settings = release.release_channel(self.cargo, 'sample/colab-cli')
                root = 'https://github.com/sample/colab-cli/releases'
                tag = 'v' + version.replace('+', '%2B')
                self.assertEqual(settings, {'download_base_url': f'{root}/download/{tag}',
                                            'manifest_url': f'{root}/latest/download/colab-update.json'})

    def test_release_channel_keeps_explicit_https_overrides(self):
        download = 'https://releases.example.invalid/cli/0.2.0'
        manifest = 'https://updates.example.invalid/manifest.json'
        self.assertEqual(release.release_channel(self.cargo, 'sample/colab-cli', download + '/', manifest),
                         {'download_base_url': download, 'manifest_url': manifest})

    def test_release_channel_can_read_cargo_repository_for_local_validation(self):
        with self.cargo.open('a') as manifest:
            manifest.write('repository = "https://github.com/sample/colab-cli"\n')
        self.assertEqual(release.release_channel(self.cargo), release.release_channel(self.cargo, 'sample/colab-cli'))

    def test_release_channel_rejects_stale_github_drafts_or_unsafe_inputs(self):
        github = 'https://github.com/sample/colab-cli/releases'
        for download in (f'{github}/download/v0.1.0', f'{github}/latest/download',
                         'http://127.0.0.1/downloads', 'https://example.invalid/?private-marker=x'):
            with self.subTest(download=download), self.assertRaises(ValueError) as caught:
                release.release_channel(self.cargo, 'sample/colab-cli', download)
            self.assertNotIn('private-marker', str(caught.exception))
        for manifest in ('http://localhost/manifest', 'https://example.invalid/manifest\nother=fixture'):
            with self.subTest(manifest=manifest), self.assertRaises(ValueError):
                release.release_channel(self.cargo, 'sample/colab-cli', manifest_url=manifest)
        with self.assertRaises(ValueError):
            release.release_channel(self.cargo, 'sample/../colab-cli')

    def write_native_reports(self, directory):
        for binary in self.binaries:
            target, _, path = binary.partition('=')
            data = Path(path).read_bytes()
            report_path = directory / f'validation-{target}' / 'native-validation.json'
            report_path.parent.mkdir(parents=True)
            report_path.write_text(json.dumps({
                'target': target,
                'version': '0.2.0',
                'packaged_version_output': 'Version: 0.2.0',
                'release_binary_size': len(data),
                'release_binary_sha256': hashlib.sha256(data).hexdigest(),
                'download_base_url': 'https://example.invalid/v0.2.0/',
                'checks': ['host native architecture', 'byte-identical packaged executable',
                           'packaged executable version'],
            }))

    def test_native_reports_are_checked_against_exact_packaged_contents(self):
        reports = self.root / 'validations'
        self.write_native_reports(reports)
        result = self.package(validation_dir=reports)
        self.assertEqual(set(result['artifacts']), set(release.TARGETS))

    def test_invalid_native_report_leaves_no_partial_release(self):
        reports = self.root / 'validations'
        self.write_native_reports(reports)
        target = 'x86_64-unknown-linux-gnu'
        report_path = reports / f'validation-{target}' / 'native-validation.json'
        original = json.loads(report_path.read_text())
        for change in (
            {'target': 'aarch64-unknown-linux-gnu'},
            {'version': '0.2.1'},
            {'packaged_version_output': 'Version: 0.2.1'},
            {'release_binary_size': original['release_binary_size'] + 1},
            {'release_binary_sha256': '0' * 64},
            {'download_base_url': 'https://example.invalid/v0.2.1'},
            {'download_base_url': []},
            {'checks': []},
        ):
            with self.subTest(change=change):
                report_path.write_text(json.dumps(original | change))
                with self.assertRaisesRegex(release.NativeValidationError, target):
                    self.package(validation_dir=reports)
                self.assertFalse(self.output.exists())
        for invalid in (None, b'not-json', b' ' * (release.MAX_VALIDATION_REPORT + 1)):
            with self.subTest(size=len(invalid) if invalid else 0):
                report_path.unlink(missing_ok=True)
                if invalid is not None:
                    report_path.write_bytes(invalid)
                with self.assertRaisesRegex(release.NativeValidationError, target):
                    self.package(validation_dir=reports)
                self.assertFalse(self.output.exists())

    def test_notices_are_included_and_covered_by_checksums(self):
        self.package()
        sums = dict(line.split('  ', 1)[::-1] for line in (self.output / 'SHA256SUMS').read_text().splitlines())
        self.assertTrue(all(path.is_file() for path in self.output.iterdir()))
        self.assertEqual(set(sums), {path.name for path in self.output.iterdir()} - {'SHA256SUMS'})
        archive_path = self.output / release.NOTICE_ARCHIVE
        self.assertEqual(sums[archive_path.name], hashlib.sha256(archive_path.read_bytes()).hexdigest())
        with ZipFile(archive_path) as archive:
            self.assertEqual(archive.namelist(), list(release.NOTICE_FILES))
            for name in release.NOTICE_FILES:
                self.assertEqual(archive.read(name), (release.ROOT / name).read_bytes())
        second = self.root / 'second'
        self.package(output=second)
        self.assertEqual((second / archive_path.name).read_bytes(), archive_path.read_bytes())

    def test_missing_or_invalid_notices_leave_no_partial_release(self):
        notices = self.root / 'notices'
        notices.mkdir()
        for name in release.NOTICE_FILES:
            path = notices / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes((release.ROOT / name).read_bytes())
        victim = notices / release.NOTICE_FILES[0]
        for problem in ('missing', 'empty', 'symlink', 'directory', 'encoding', 'size'):
            with self.subTest(problem=problem):
                victim.unlink(missing_ok=True)
                if problem == 'empty':
                    victim.touch()
                elif problem == 'symlink':
                    victim.symlink_to(release.ROOT / release.NOTICE_FILES[0])
                elif problem == 'directory':
                    victim.mkdir()
                elif problem == 'encoding':
                    victim.write_bytes(b'\xff')
                elif problem == 'size':
                    with victim.open('wb') as file:
                        file.truncate(1024 * 1024 + 1)
                with self.assertRaises((ValueError, OSError)):
                    self.package(notice_root=notices)
                self.assertFalse(self.output.exists())
                if victim.is_dir():
                    victim.rmdir()

    def test_stale_dependency_lock_digest_rejects_candidate(self):
        notices = self.root / 'stale-notices'
        for name in release.NOTICE_FILES:
            path = notices / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes((release.ROOT / name).read_bytes())
        path = notices / 'licenses/rust-dependencies.json'
        value = json.loads(path.read_bytes())
        value['cargo_lock_sha256'] = '0' * 64
        path.write_text(json.dumps(value))
        with self.assertRaisesRegex(ValueError, 'Cargo.lock'):
            self.package(notice_root=notices)
        self.assertFalse(self.output.exists())

    def test_existing_output_is_never_overwritten(self):
        self.output.mkdir()
        marker = self.output / 'user-file'
        marker.write_text('keep')
        with self.assertRaises(FileExistsError):
            self.package()
        self.assertEqual(marker.read_text(), 'keep')

    def test_invalid_architecture_or_input_type_never_publishes_partial_directory(self):
        for problem in ('header', 'wrong-arch', 'symlink', 'directory', 'size'):
            with self.subTest(problem=problem):
                target = self.binaries[0].partition('=')[0]
                source = self.root / f'bad-{problem}'
                if problem == 'symlink':
                    source.symlink_to(self.binaries[0].partition('=')[2])
                elif problem == 'directory':
                    source.mkdir()
                elif problem == 'size':
                    with source.open('wb') as file:
                        file.truncate(release.MAX_BINARY + 1)
                else:
                    source.write_bytes(b'x' * 64 if problem == 'header' else (self.root / 'x86_64-unknown-linux-gnu').read_bytes())
                with self.assertRaises(ValueError):
                    self.package(binaries=[f'{target}={source}'], allow_partial=True)
                self.assertFalse(self.output.exists())

    def test_unsupported_and_duplicate_targets_are_rejected(self):
        for binaries in ([self.binaries[0], self.binaries[0]], ['windows=missing'], ['no-separator']):
            with self.assertRaises(ValueError):
                self.package(binaries=binaries)
        self.assertFalse(self.output.exists())

    def test_bad_source_urls_never_appear_in_error_text(self):
        for uri in ('http://private.invalid/base', 'https://user:private-password@example.invalid', 'https://example.invalid/?private-token=x', 'https://example.invalid/#', 'https://example.invalid/?', 'https://exa\nmple.invalid'):
            with self.assertRaises(ValueError) as caught:
                self.package(base_url=uri)
            self.assertNotIn('private-', str(caught.exception))
        self.assertFalse(self.output.exists())

    def test_invalid_package_versions_are_not_published(self):
        for version in ('v0.2.0', '0.2', '0.2.0-01', '18446744073709551616.0.0', 'private-invalid'):
            self.cargo.write_text(f'[package]\nname = "colabcli-rust"\nversion = "{version}"\n')
            with self.assertRaises(ValueError):
                self.package()
        self.assertFalse(self.output.exists())

    def test_build_metadata_is_preserved_and_url_encoded(self):
        self.cargo.write_text('[package]\nname = "colabcli-rust"\nversion = "0.2.0+build.1"\n')
        result = self.package()
        self.assertEqual(result['version'], '0.2.0+build.1')
        self.assertTrue(all('%2Bbuild.1-' in item['url'] for item in result['artifacts'].values()))


if __name__ == '__main__':
    unittest.main()
