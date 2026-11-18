import importlib.util
import hashlib
import io
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
SPEC = importlib.util.spec_from_file_location('live_disconnect', Path(__file__).resolve().parents[1] / 'validate_live_disconnect.py')
live = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(live)


class RelayTests(unittest.TestCase):
    def setUp(self):
        self.backend = socket.socket()
        self.backend.bind(('127.0.0.1', 0))
        self.backend.listen()
        self.backend.settimeout(.1)
        self.stopping = threading.Event()
        self.clients = []
        self.workers = []
        def echo(stream):
            try:
                while True:
                    value = stream.recv(32768)
                    if not value:
                        break
                    stream.sendall(value)
            except OSError:
                pass
            finally:
                stream.close()
        def accept():
            while not self.stopping.is_set():
                try:
                    stream, _ = self.backend.accept()
                except socket.timeout:
                    continue
                except OSError:
                    break
                self.clients.append(stream)
                thread = threading.Thread(target=echo, args=(stream,), daemon=True)
                self.workers.append(thread)
                thread.start()
        self.backend_thread = threading.Thread(target=accept, daemon=True)
        self.backend_thread.start()
        self.relay = live.CutRelay('127.0.0.1', self.backend.getsockname()[1])
        self.addCleanup(self.close)

    def close(self):
        self.relay.close()
        self.stopping.set()
        self.backend.close()
        for stream in self.clients:
            try:
                stream.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        self.backend_thread.join(timeout=2)
        for worker in self.workers:
            worker.join(timeout=2)

    def connect(self, authority=None):
        stream = socket.create_connection(self.relay.listener.getsockname(), timeout=2)
        self.addCleanup(stream.close)
        host, port = self.backend.getsockname()
        authority = authority or f'{host}:{port}'
        stream.sendall(f'CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n'.encode())
        response = b''
        while b'\r\n\r\n' not in response:
            response += stream.recv(4096)
        return stream, response

    def test_only_allowed_connect_authority_is_forwarded(self):
        stream, response = self.connect('not-allowed.invalid:443')
        self.assertIn(b'403', response)
        self.assertEqual(stream.recv(1), b'')
        self.assertEqual(self.relay.tunnels, 0)
        self.assertEqual(self.relay.rejected, 1)

    def test_tunnel_preserves_opaque_binary_bytes_without_tls_interception(self):
        stream, response = self.connect()
        self.assertIn(b'200', response)
        payload = b'\x16\x03\x03' + bytes(range(256)) * 256
        stream.sendall(payload)
        value = b''
        while len(value) < len(payload):
            value += stream.recv(8192)
        self.assertEqual(value, payload)
        self.assertEqual(self.relay.tunnels, 1)

    def test_cut_closes_only_relay_connections_and_counts_later_reconnect(self):
        stream, _ = self.connect()
        self.assertGreaterEqual(self.relay.cut(), 2)
        self.assertEqual(stream.recv(1), b'')
        second, response = self.connect()
        self.assertIn(b'200', response)
        second.sendall(b'after-cut')
        self.assertEqual(second.recv(100), b'after-cut')
        self.assertEqual(self.relay.after_cut_tunnels, 1)


@unittest.skipUnless(os.name == 'posix', 'Unix process ownership')
class WorkflowTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name).resolve()
        self.config = self.root / 'config'
        self.config.mkdir()
        self.state = self.root / 'state'
        self.binary = self.root / 'synthetic-colab'
        self.binary.write_text('synthetic binary; never executed')
        self.client = Mock()
        self.client.inventory.return_value = {'baseline'}
        def call(args, **kwargs):
            if args[0] == 'new':
                return subprocess.CompletedProcess([], 0, json.dumps({'name': args[2], 'endpoint': 'owned', 'accelerator': 'NONE'}).encode(), b'')
            return subprocess.CompletedProcess([], 0, b'', b'')
        self.client.call.side_effect = call
        self.selected = subprocess.CompletedProcess([], 0, b'{"account":"synthetic","auth":{"provider":"oauth2"}}', b'')
        self.validate = Mock()
        self.validate_death = Mock()

    def run_main(self, *args):
        with patch.object(sys, 'argv', ['harness', '--binary', str(self.binary), '--config-dir', str(self.config), '--state-dir', str(self.state), *args]), patch.object(live, 'Client', return_value=self.client), patch.object(live, 'TerminalClient', return_value=self.client), patch.object(live.subprocess, 'run', return_value=self.selected), patch.object(live, 'validate', self.validate), patch.object(live, 'validate_kernel_death', self.validate_death), patch('sys.stdout', new_callable=io.StringIO):
            code = live.main()
        report = json.loads((self.state / 'report.json').read_text())
        self.assertEqual((self.state / 'report.json').stat().st_mode & 0o777, 0o600)
        return code, report

    def test_terminal_keyring_is_explicit_and_does_not_allocate_during_preflight(self):
        with patch.object(live.os, 'isatty', return_value=True):
            code, report = self.run_main('--terminal-keyring')
        self.assertEqual(code, 0)
        self.assertTrue(report['terminal_keyring'])
        self.assertEqual(report['created_sessions'], 0)
        self.client.call.assert_not_called()

    def test_terminal_keyring_without_tty_rejects_before_creating_state(self):
        with patch.object(live.os, 'isatty', return_value=False), patch('sys.stderr', new_callable=io.StringIO):
            with self.assertRaises(SystemExit): self.run_main('--terminal-keyring')
        self.assertFalse(self.state.exists())
        self.client.inventory.assert_not_called()

    def test_real_death_workflow_fences_every_runtime_call_and_requires_same_pending_id(self):
        directory = self.root / 'probe'; directory.mkdir()
        state_path = self.config / 'accounts/synthetic/sessions.json'; state_path.parent.mkdir(parents=True)
        state_path.write_text(json.dumps({'owned': {'endpoint': 'captured-endpoint', 'running': 'message'}}))
        def call(args, **kwargs):
            self.assertEqual(args[:2], ['--colab-runtime-endpoint', 'captured-endpoint'])
            command = args[2]
            if command == 'download':
                Path(args[-1]).write_text('once\n')
                return subprocess.CompletedProcess([], 0, b'', b'')
            if command == 'restart-kernel':
                state_path.write_text(json.dumps({'owned': {'endpoint': 'captured-endpoint', 'running': None}}))
                return subprocess.CompletedProcess([], 0, b'', b'')
            self.assertEqual(command, 'exec')
            if 'data' in kwargs:
                return subprocess.CompletedProcess([], 0, b'[{"status":"succeeded"}]', b'')
            if 'death' not in self.__dict__:
                self.death = True
                value = {'status': 'unknown', 'msg_id': 'message',
                    'error': 'kernel 已停止或正在重启；尚未确认本次执行结果',
                    'outputs': [{'text': 'COLAB_DEATH_' + '0'*32 + '\n'}]}
                return subprocess.CompletedProcess([], 1, json.dumps([value]).encode(), b'')
            return subprocess.CompletedProcess([], 1, b'', '上次执行尚未确认结束 message'.encode())
        self.client.call.side_effect = call
        report = {'checks': []}
        with patch.object(live.uuid, 'uuid4', return_value=Mock(hex='0'*32)):
            live.validate_kernel_death(self.client, self.config,
                {'account': 'synthetic', 'session': 'owned', 'endpoint': 'captured-endpoint'}, directory, report)
        self.assertEqual(self.client.call.call_count, 6)
        self.assertEqual(len(report['checks']), 4)

    def test_default_preflight_does_not_allocate_or_execute(self):
        code, report = self.run_main()
        self.assertEqual(code, 0)
        self.assertTrue(report['preflight_only'])
        self.assertTrue(report['passed'])
        self.assertEqual(report['created_sessions'], 0)
        self.validate.assert_not_called()
        self.client.call.assert_not_called()

    def test_existing_journal_refuses_all_new_work(self):
        journal = self.config / 'accounts/synthetic/control_operation.json'
        journal.parent.mkdir(parents=True)
        journal.write_text('{}')
        code, report = self.run_main('--execute')
        self.assertEqual(code, 1)
        self.assertEqual(report['error_class'], 'existing_control_operation_requires_recovery')
        self.client.inventory.assert_not_called()
        self.client.call.assert_not_called()

    def test_fault_validation_failure_still_releases_captured_endpoint(self):
        self.validate.side_effect = live.ValidationError('synthetic_probe_failure')
        code, report = self.run_main('--execute')
        self.assertEqual(code, 1)
        self.assertFalse(report['passed'])
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertEqual(report['created_sessions'], 1)
        self.assertEqual(self.client.call.call_args.args[0][-2:], ['--colab-expected-endpoint', 'owned'])
        self.assertEqual(self.client.inventory.call_count, 2)

    def test_kernel_death_mode_uses_same_owned_allocation_and_guarded_cleanup(self):
        code, report = self.run_main('--execute', '--fault', 'kernel-death')
        self.assertEqual(code, 0)
        self.assertEqual(report['fault'], 'kernel-death')
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.validate.assert_not_called()
        self.validate_death.assert_called_once()
        self.assertEqual(self.validate_death.call_args.args[2]['endpoint'], 'owned')

    def test_kernel_death_probe_failure_cannot_skip_vm_cleanup(self):
        self.validate_death.side_effect = live.ValidationError('synthetic_death_probe_failure')
        code, report = self.run_main('--execute', '--fault', 'kernel-death')
        self.assertEqual(code, 1)
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertEqual(report['error_class'], 'synthetic_death_probe_failure')

    def test_unknown_allocation_reconciles_same_request_and_never_executes_code(self):
        requests = []
        journal = self.config / 'accounts/synthetic/control_operation.json'
        def call(args, **kwargs):
            if args[0] == 'new':
                requests.append(args)
                if len(requests) == 1:
                    journal.parent.mkdir(parents=True)
                    journal.write_text(json.dumps({'operation': 'allocate', 'name': args[2]}))
                    return subprocess.CompletedProcess([], 1, b'', b'unknown')
                journal.unlink()
                return subprocess.CompletedProcess([], 0, json.dumps({'name': args[2], 'endpoint': 'owned', 'accelerator': 'NONE'}).encode(), b'')
            return subprocess.CompletedProcess([], 0, b'', b'')
        self.client.call.side_effect = call
        code, report = self.run_main('--execute')
        self.assertEqual(code, 1)
        self.assertEqual(report['cleanup'], 'confirmed_released')
        self.assertEqual(len(requests), 2)
        self.assertEqual(requests[0], requests[1])
        self.validate.assert_not_called()

    def test_cleanup_only_refuses_baseline_endpoint_without_sending_stop(self):
        self.state.mkdir(mode=0o700)
        live.private_json(self.state / 'ledger.json', {'config_dir': str(self.config), 'account': 'synthetic', 'provider': 'oauth2', 'token_storage': 'file', 'session': 'owned', 'baseline_endpoints': ['baseline'], 'endpoint': 'baseline', 'allocation_requested': True})
        code, report = self.run_main('--cleanup-only')
        self.assertEqual(code, 1)
        self.assertEqual(report['cleanup'], 'ownership_refused')
        self.client.call.assert_not_called()
        self.validate.assert_not_called()

    def history(self, error, text='remote-marker\n'):
        path = self.config / 'accounts/synthetic/history/rust-v1' / (hashlib.sha256(b'session').hexdigest() + '.jsonl')
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps({'event_type': 'execution', 'msg_id': 'message', 'status': 'unknown',
                                    'error': error, 'outputs': [{'output_type': 'stream', 'text': text}]}) + '\n')

    def test_durable_history_requires_disconnect_reason_and_retained_marker(self):
        self.history('执行完成前连接丢失')
        self.assertEqual(live.verify_disconnect_history(self.config, 'synthetic', 'session', 'message', 'remote-marker'), 'connection_lost')
        self.history('执行完成前连接关闭', 'unrelated-marker\n')
        with self.assertRaisesRegex(live.ValidationError, 'partial_remote_output_not_retained'):
            live.verify_disconnect_history(self.config, 'synthetic', 'session', 'message', 'remote-marker')

    def test_unknown_timeout_cannot_be_misreported_as_transport_disconnect(self):
        self.history('等待本次执行消息超时；未取消远端执行')
        with self.assertRaisesRegex(live.ValidationError, 'unknown_cause_is_not_transport_disconnect'):
            live.verify_disconnect_history(self.config, 'synthetic', 'session', 'message', 'remote-marker')


if __name__ == '__main__':
    unittest.main()
