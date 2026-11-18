import errno
import importlib.util
import json
import os
from pathlib import Path
import sys
import socket
import tempfile
import unittest
from unittest.mock import patch

TOOLS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS))
import drive_validation_payload as payload
import prepare_live_drive_validation as preparation


@unittest.skipUnless(os.name == 'posix', 'Unix directory descriptors')
class PayloadTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.nonce = 'a' * 32
        self.mount = self.base / ('colabcli-drive-' + self.nonce)
        (self.mount / 'MyDrive').mkdir(parents=True)
        self.plan = {'nonce': self.nonce, 'mountpoint': str(self.mount), 'folder': 'colabcli-validation-' + self.nonce}
        self.folder = self.mount / 'MyDrive' / self.plan['folder']
        self.mount_verifier = payload.verify_mount
        self.guard = patch.object(payload, 'verify_mount')
        self.guard.start()
        self.addCleanup(self.guard.stop)

    def run_phase(self, phase):
        return payload.perform(self.plan, phase)

    def test_create_binary_readback_then_exact_nonrecursive_cleanup(self):
        result = self.run_phase('create')
        self.assertEqual(result['status'], 'created_and_read_back')
        self.assertFalse(result['live_identity_verified'])
        self.assertEqual((self.folder / payload.DATA).read_bytes(), payload.PAYLOAD)
        self.assertEqual(self.run_phase('cleanup')['status'], 'removed')
        self.assertFalse(self.folder.exists())
        self.assertEqual(self.run_phase('cleanup')['status'], 'already_absent')

    def test_verification_rewinds_retained_descriptor_and_still_rejects_extra_files(self):
        self.run_phase('create')
        descriptor = payload.open_directory(self.folder)
        self.addCleanup(os.close, descriptor)
        os.listdir(descriptor)
        expected = payload.owner_value(self.nonce, self.plan['folder'])
        seek = os.lseek
        with patch.object(payload.os, 'lseek', wraps=seek) as rewind:
            self.assertEqual(payload.check_files(descriptor, expected), {payload.OWNER, payload.DATA})
            rewind.assert_called_once_with(descriptor, 0, os.SEEK_SET)
        (self.folder / 'user-content').write_bytes(b'preserve')
        with self.assertRaisesRegex(payload.ProbeError, 'unexpected_directory_entries'):
            payload.check_files(descriptor, expected)
        self.assertEqual((self.folder / 'user-content').read_bytes(), b'preserve')

    def test_create_waits_for_missing_known_entries_without_recreating_files(self):
        self.run_phase('create')
        descriptor = payload.open_directory(self.folder)
        self.addCleanup(os.close, descriptor)
        expected = payload.owner_value(self.nonce, self.plan['folder'])
        with patch.object(payload.os, 'listdir', side_effect=[[], [payload.OWNER], [payload.OWNER, payload.DATA]]) as listing, \
                patch.object(payload.time, 'sleep') as sleep:
            self.assertEqual(payload.check_files(descriptor, expected, wait_for_created=True), {payload.OWNER, payload.DATA})
            self.assertEqual(listing.call_count, 3)
            self.assertEqual(sleep.call_count, 2)

    def test_create_missing_listing_expires_and_extra_entries_are_never_waited_away(self):
        self.run_phase('create')
        descriptor = payload.open_directory(self.folder)
        self.addCleanup(os.close, descriptor)
        expected = payload.owner_value(self.nonce, self.plan['folder'])
        with patch.object(payload.os, 'listdir', return_value=[]), \
                patch.object(payload.time, 'monotonic', side_effect=[0, 6]), \
                patch.object(payload.time, 'sleep') as sleep:
            with self.assertRaisesRegex(payload.ProbeError, 'unexpected_directory_entries'):
                payload.check_files(descriptor, expected, wait_for_created=True)
            sleep.assert_not_called()
        with patch.object(payload.os, 'listdir', return_value=[payload.OWNER, payload.DATA, 'foreign']), \
                patch.object(payload.time, 'sleep') as sleep:
            with self.assertRaisesRegex(payload.ProbeError, 'unexpected_directory_entries'):
                payload.check_files(descriptor, expected, wait_for_created=True)
            sleep.assert_not_called()

    def test_existing_directory_cannot_be_adopted_by_replaying_create(self):
        self.run_phase('create')
        before = {p.name: p.read_bytes() for p in self.folder.iterdir()}
        with self.assertRaises(FileExistsError):
            self.run_phase('create')
        self.assertEqual({p.name: p.read_bytes() for p in self.folder.iterdir()}, before)

    def test_missing_fuse_mount_fails_before_creating_any_cloud_path(self):
        payload.verify_mount.side_effect = payload.ProbeError('not-mounted')
        with self.assertRaises(payload.ProbeError):
            self.run_phase('create')
        self.assertFalse(self.folder.exists())

    def test_mount_table_must_match_opened_device_path_and_fuse_type(self):
        descriptor = os.open(self.mount, os.O_RDONLY | os.O_DIRECTORY)
        self.addCleanup(os.close, descriptor)
        device = os.fstat(descriptor).st_dev
        identity = f'{os.major(device)}:{os.minor(device)}'
        original = self.mount_verifier
        line = f'40 20 {identity} / {self.mount} rw - fuse.drivefs drive rw\n'
        with patch.object(payload.Path, 'read_text', return_value=line):
            original(str(self.mount), descriptor)
        for wrong in [line.replace('fuse.drivefs', 'ext4'), line.replace(identity, '999:999'),
                      line.replace(str(self.mount), '/another-mount')]:
            with patch.object(payload.Path, 'read_text', return_value=wrong), self.assertRaises(payload.ProbeError):
                original(str(self.mount), descriptor)

    def test_symlinked_mydrive_root_cannot_escape_the_validated_mount(self):
        (self.mount / 'MyDrive').rmdir()
        (self.mount / 'MyDrive').symlink_to(self.base, target_is_directory=True)
        with self.assertRaises(OSError):
            self.run_phase('create')
        self.assertFalse((self.base / self.plan['folder']).exists())

    def test_readonly_write_denial_preserves_exact_original_bytes(self):
        self.run_phase('create')
        with patch.object(payload.os, 'pwrite', side_effect=OSError(errno.EROFS, 'synthetic readonly')):
            self.assertEqual(self.run_phase('readonly')['status'], 'write_denied')
        self.assertEqual((self.folder / payload.DATA).read_bytes(), payload.PAYLOAD)

    def test_writable_mount_is_not_misreported_as_readonly_and_content_stays_same(self):
        self.run_phase('create')
        with self.assertRaisesRegex(payload.ProbeError, 'readonly_write_unexpectedly_allowed'):
            self.run_phase('readonly')
        self.assertEqual((self.folder / payload.DATA).read_bytes(), payload.PAYLOAD)

    def test_unrelated_io_error_cannot_prove_readonly(self):
        self.run_phase('create')
        with patch.object(payload.os, 'pwrite', side_effect=OSError(errno.EIO, 'synthetic failure')):
            with self.assertRaises(OSError):
                self.run_phase('readonly')

    def test_owner_mismatch_and_changed_content_block_cleanup(self):
        self.run_phase('create')
        owner = self.folder / payload.OWNER
        original = owner.read_bytes()
        wrong_type = json.loads(original)
        wrong_type['version'] = True
        for changed in ['{}', json.dumps(wrong_type, sort_keys=True)]:
            owner.write_text(changed)
            with self.assertRaisesRegex(payload.ProbeError, 'owner_marker_mismatch'):
                self.run_phase('cleanup')
        owner.write_bytes(original)
        (self.folder / payload.DATA).write_bytes(b'user-changed')
        with self.assertRaisesRegex(payload.ProbeError, 'payload_changed'):
            self.run_phase('cleanup')
        self.assertEqual((self.folder / payload.DATA).read_bytes(), b'user-changed')
        self.assertTrue(owner.exists())

    def test_extra_user_file_prevents_every_delete(self):
        self.run_phase('create')
        (self.folder / 'unrelated.txt').write_text('keep')
        with self.assertRaisesRegex(payload.ProbeError, 'unexpected_directory_entries'):
            self.run_phase('cleanup')
        self.assertEqual(len(list(self.folder.iterdir())), 3)

    def test_symlinked_sample_and_folder_are_never_followed(self):
        self.run_phase('create')
        outside = self.base / 'outside'
        outside.write_bytes(payload.PAYLOAD)
        (self.folder / payload.DATA).unlink()
        (self.folder / payload.DATA).symlink_to(outside)
        with self.assertRaises(OSError):
            self.run_phase('cleanup')
        self.assertEqual(outside.read_bytes(), payload.PAYLOAD)
        (self.folder / payload.DATA).unlink()
        (self.folder / payload.OWNER).unlink()
        self.folder.rmdir()
        self.folder.symlink_to(self.base, target_is_directory=True)
        with self.assertRaises(OSError):
            self.run_phase('cleanup')
        self.assertTrue(outside.exists())

    def test_partial_cleanup_can_resume_only_with_matching_owner_marker(self):
        self.run_phase('create')
        (self.folder / payload.DATA).unlink()
        self.assertEqual(self.run_phase('cleanup')['status'], 'removed')

    def test_fifo_cannot_block_or_pass_regular_file_validation(self):
        self.run_phase('create')
        (self.folder / payload.DATA).unlink()
        os.mkfifo(self.folder / payload.DATA)
        with self.assertRaisesRegex(payload.ProbeError, 'unexpected_file_type'):
            self.run_phase('cleanup')

    def test_invalid_namespace_rejected_before_touching_existing_paths(self):
        for field, value in [('folder', '../outside'), ('nonce', 'z' * 32), ('mountpoint', '/content/drive')]:
            with self.subTest(field=field), self.assertRaisesRegex(payload.ProbeError, 'probe_plan_invalid'):
                payload.perform(dict(self.plan, **{field: value}), 'cleanup')


class PreparationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.root = self.base / 'config'
        (self.root / 'accounts/storage').mkdir(parents=True)
        (self.root / 'accounts.json').write_text(json.dumps({'accounts': {'compute': {}, 'storage': {}}}))
        self.creds = self.root / 'accounts/storage/dfs_credentials.json'
        self.creds.write_text(json.dumps({'client_id': 'synthetic-client', 'client_secret': 'synthetic-client-secret',
                                         'refresh_token': 'synthetic-refresh', 'user_id': 'synthetic-user',
                                         'scopes': ['https://www.googleapis.com/auth/drive']}))
        self.binary = self.base / 'synthetic-binary'
        self.binary.write_bytes(b'not executable; preparation must never spawn it')
        self.state = self.base / 'state'

    def test_preparation_never_executes_or_exports_credentials_and_preserves_input_bytes(self):
        original = self.creds.read_bytes()
        registry = (self.root / 'accounts.json').read_bytes()
        environment = dict(os.environ)
        with patch.object(socket, 'create_connection', side_effect=AssertionError('network forbidden')), \
                patch.object(socket, 'getaddrinfo', side_effect=AssertionError('DNS forbidden')):
            report = preparation.prepare(self.root, self.state, 'compute', 'storage', self.binary)
        self.assertTrue(report['prepared'])
        self.assertFalse(report['live_validation_passed'])
        self.assertEqual(report['cloud_writes'], 0)
        self.assertEqual(self.creds.read_bytes(), original)
        self.assertEqual((self.root / 'accounts.json').read_bytes(), registry)
        self.assertEqual(dict(os.environ), environment)
        for path in self.state.iterdir():
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            text = path.read_text()
            for secret in ['synthetic-refresh', 'synthetic-client-secret', 'synthetic-user', 'synthetic-client']:
                self.assertNotIn(secret, text)
            if path.suffix == '.py':
                compile(text, str(path), 'exec')
        self.assertEqual(self.state.stat().st_mode & 0o777, 0o700)
        self.assertEqual(len(report['payload_sha256']), 3)
        self.assertFalse(json.loads((self.state / 'plan.json').read_text())['execution_authorized'])
        commands = json.loads((self.state / 'commands.json').read_text())
        self.assertFalse(commands['executable_workflow'])
        self.assertEqual(len(commands['steps']), 14)
        sensitive = [row['name'] for row in commands['steps'] if row['sensitive_stdout_memory_only']]
        self.assertEqual(sensitive, ['identity_before_lease'])
        stop = next(row for row in commands['steps'] if row['name'] == 'release_only_recorded_endpoint')
        self.assertEqual(stop['argv'][-2:], ['--colab-expected-endpoint', '<captured-endpoint>'])

    def test_malformed_scopes_profile_and_config_state_paths_fail_closed(self):
        for scopes in [None, 'https://www.googleapis.com/auth/drive', ['https://www.googleapis.com/auth/drive.file']]:
            value = json.loads(self.creds.read_text()); value['scopes'] = scopes
            self.creds.write_text(json.dumps(value))
            with self.subTest(scopes=scopes), self.assertRaises(ValueError):
                preparation.prepare(self.root, self.state, 'compute', 'storage', self.binary)
            self.assertFalse(self.state.exists())
        with self.assertRaises(ValueError):
            preparation.prepare(self.root, self.state, '../compute', 'storage', self.binary)
        with self.assertRaises(ValueError):
            preparation.prepare(self.root, self.root / 'state', 'compute', 'storage', self.binary)

    def test_symlinked_credentials_and_preexisting_state_are_not_adopted(self):
        outside = self.base / 'credentials'
        outside.write_bytes(self.creds.read_bytes())
        self.creds.unlink()
        self.creds.symlink_to(outside)
        with self.assertRaisesRegex(ValueError, 'symlinked_credentials_refused'):
            preparation.prepare(self.root, self.state, 'compute', 'storage', self.binary)
        self.creds.unlink(); self.creds.write_bytes(outside.read_bytes())
        self.state.mkdir()
        (self.state / 'keep').write_text('unrelated')
        with self.assertRaises(FileExistsError):
            preparation.prepare(self.root, self.state, 'compute', 'storage', self.binary)
        self.assertEqual((self.state / 'keep').read_text(), 'unrelated')


if __name__ == '__main__':
    unittest.main()
