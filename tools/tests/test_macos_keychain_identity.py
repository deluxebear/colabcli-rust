import hashlib
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import inspect_macos_keychain_identity as identity


class IdentityTests(unittest.TestCase):
    def inspect(self, output, returncode=0):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'not-executable'
            path.write_bytes(b'fixture')
            with mock.patch.object(identity.subprocess, 'run', return_value=
                    subprocess.CompletedProcess([], returncode, b'', output)) as run:
                row = identity.inspect(path)
            self.assertEqual(run.call_args.args[0],
                ['/usr/bin/codesign', '--display', '--requirements', '-', str(path.resolve())])
            self.assertEqual(row['sha256'], hashlib.sha256(b'fixture').hexdigest())
            self.assertNotIn(directory, str(row))
            return row

    def test_hash_only_identity_changes_with_requirement(self):
        first = self.inspect(b'Executable=/private/path\n# designated => cdhash H"abc123"\n')
        second = self.inspect(b'# designated => cdhash H"abc124"\n')
        self.assertTrue(first['identity_bound_only_to_code_hash'])
        self.assertNotEqual(first['requirement_sha256'], second['requirement_sha256'])

    def test_certificate_identity_is_not_classified_as_hash_only(self):
        row = self.inspect(b'# designated => identifier "example" and anchor apple generic\n')
        self.assertTrue(row['inspection_ok'])
        self.assertFalse(row['identity_bound_only_to_code_hash'])

    def test_errors_and_missing_requirement_do_not_expose_diagnostics(self):
        for output, code in [(b'/private/path: sensitive diagnostic', 1),
                             (b'Executable=/private/path\n', 0)]:
            row = self.inspect(output, code)
            self.assertFalse(row['inspection_ok'])
            self.assertNotIn('private', str(row))


if __name__ == '__main__':
    unittest.main()
