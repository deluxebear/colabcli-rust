import importlib.util
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('linux_validation', Path(__file__).resolve().parents[1] / 'linux-validation/validate.py')
validation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validation)


class ArchitectureTests(unittest.TestCase):
    def test_release_notices_and_license_materials_are_validation_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ('Cargo.toml', 'Cargo.lock', 'build.rs', 'README.md', 'THIRD_PARTY_NOTICES.md',
                         'licenses/rust-dependencies.json', 'licenses/upstream.txt',
                         'tools/linux-validation/Dockerfile'):
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('synthetic material')
            with patch.object(validation, 'ROOT', root):
                before = validation.source_hashes()
                changed = root / 'licenses/upstream.txt'
                changed.write_text('modified material')
                after = validation.source_hashes()
            self.assertIn('THIRD_PARTY_NOTICES.md', before)
            self.assertIn('licenses/rust-dependencies.json', before)
            self.assertNotEqual(before['licenses/upstream.txt'], after['licenses/upstream.txt'])

    def test_runtime_toolchain_and_explicit_target_must_all_match(self):
        for machine, host in [('x86_64', 'x86_64-unknown-linux-gnu'), ('aarch64', 'aarch64-unknown-linux-gnu')]:
            text = 'rustc synthetic\nhost: ' + host + '\nrelease: synthetic\n'
            self.assertEqual(validation.execution_architecture(text, machine, machine), host)
            with self.assertRaises(ValueError):
                validation.execution_architecture(text, machine, 'wrong-target')
            with self.assertRaises(ValueError):
                validation.execution_architecture(text, 'other')
            with self.assertRaises(ValueError):
                validation.execution_architecture(text.replace(host, 'wrong-host'), machine)
            with self.assertRaises(ValueError):
                validation.execution_architecture(text + 'host: '+host+'\n', machine)

    def test_elf_output_architecture_is_read_from_header(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'binary'
            for value, machine in [(62, 'x86_64'), (183, 'aarch64')]:
                data = bytearray(20)
                data[:6] = b'\x7fELF\x02\x01'
                data[18:20] = value.to_bytes(2, 'little')
                path.write_bytes(data)
                self.assertEqual(validation.elf_machine(path), machine)

    def test_malformed_non_elf_big_endian_or_other_machine_are_rejected(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'binary'
            for data in [b'', b'MZ'+b'\0'*18, b'\x7fELF\x01\x01'+b'\0'*14,
                         b'\x7fELF\x02\x02'+b'\0'*14, b'\x7fELF\x02\x01'+b'\0'*14]:
                path.write_bytes(data)
                with self.assertRaises(ValueError):
                    validation.elf_machine(path)

    def test_failed_suite_counts_are_preserved_and_ignored_are_separate(self):
        totals = validation.rust_test_totals(
            'test result: ok. 3 passed; 0 failed; 5 ignored; 0 measured; 0 filtered out\n'
            'test result: FAILED. 48 passed; 28 failed; 0 ignored; 0 measured; 0 filtered out\n')
        self.assertEqual(totals, {'tests_passed': 51, 'tests_failed': 28, 'tests_ignored': 5, 'test_summaries': 2})
        self.assertEqual(validation.rust_test_totals('not test output')['test_summaries'], 0)
