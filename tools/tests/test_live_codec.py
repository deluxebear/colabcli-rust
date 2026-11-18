import base64
import copy
import json
import contextlib
import io
import tempfile
from pathlib import Path
import sys
import unittest
from unittest import mock
sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from validate_live_codec import TEXT, BINARY, BUFFERS, sanitize_capture, inventory_before_allocation, local_owned_state
from validate_live_ssh import ValidationError
import validate_live_codec as live


def serialize(message):
    value = copy.deepcopy(message)
    buffers = value.pop('buffers', [])
    data = json.dumps(value).encode()
    parts = [data] + buffers
    position = 4 * (len(parts) + 1)
    offsets = []
    for part in parts:
        offsets.append(position); position += len(part)
    return len(parts).to_bytes(4, 'big') + b''.join(n.to_bytes(4, 'big') for n in offsets) + b''.join(parts)


def deserialize(raw):
    if isinstance(raw, str): return json.loads(raw)
    count = int.from_bytes(raw[:4], 'big')
    offsets = [int.from_bytes(raw[4 + 4*i:8 + 4*i], 'big') for i in range(count)] + [len(raw)]
    value = json.loads(raw[offsets[0]:offsets[1]])
    value['buffers'] = [raw[offsets[i]:offsets[i+1]] for i in range(1, count)]
    return value


FUNCTIONS = {'deserialize_msg_from_ws_default': deserialize, 'serialize_msg_to_ws_default': serialize, 'serialize_msg_to_ws_json': json.dumps}


def capture():
    samples = []
    for wire_type, msg_type, content, buffers in [('text', 'stream', TEXT, []), ('binary', 'display_data', BINARY, BUFFERS)]:
        message = {'header': {'msg_id': 'PRIVATE-ID', 'session': 'PRIVATE-SESSION', 'username': 'PRIVATE-USER', 'msg_type': msg_type, 'version': '5.3'},
            'parent_header': {'msg_id': 'PRIVATE-PARENT'}, 'metadata': {'private': 'PRIVATE-METADATA'}, 'content': content, 'channel': 'iopub'}
        raw = json.dumps(message).encode() if wire_type == 'text' else serialize({**message, 'buffers': buffers})
        samples.append({'wire_type': wire_type, 'wire_base64': base64.b64encode(raw).decode(), 'expected_envelope': message,
            'buffers_base64': [base64.b64encode(b).decode() for b in buffers]})
    return {'passed': True, 'samples': samples}


class LiveCodecTests(unittest.TestCase):
    def test_sanitizes_private_fields_and_preserves_non_utf8_and_empty_buffers(self):
        result = sanitize_capture(capture(), FUNCTIONS)
        for sample in result['samples']:
            raw = base64.b64decode(sample['wire_base64'])
            self.assertNotIn(b'PRIVATE-', raw)
            value = deserialize(raw.decode() if sample['wire_type'] == 'text' else raw)
            buffers = value.pop('buffers', [])
            self.assertEqual(value, sample['expected_envelope'])
            self.assertEqual(buffers, [] if sample['wire_type'] == 'text' else BUFFERS)
        self.assertEqual(result['samples'][1]['buffers_base64'], ['AP+AQQ==', ''])

    def test_accepts_only_empty_text_inline_buffer_convenience_field(self):
        value = capture(); row = value['samples'][0]
        row['expected_envelope']['buffers'] = []
        row['wire_base64'] = base64.b64encode(json.dumps(row['expected_envelope']).encode()).decode()
        self.assertTrue(sanitize_capture(value, FUNCTIONS)['raw_rust_and_official_decoders_agreed'])
        row['expected_envelope']['buffers'] = ['unexpected']
        with self.assertRaises(ValidationError): sanitize_capture(value, FUNCTIONS)

    def test_refuses_unexpected_content_or_decoder_disagreement(self):
        for change in ('content', 'envelope', 'buffers', 'duplicate', 'incomplete'):
            with self.subTest(change=change):
                value = capture()
                if change == 'content':
                    row = value['samples'][0]; row['expected_envelope']['content'] = {'name': 'stdout', 'text': 'unexpected private output'}
                    row['wire_base64'] = base64.b64encode(json.dumps(row['expected_envelope']).encode()).decode()
                if change == 'envelope': value['samples'][0]['expected_envelope']['metadata'] = {}
                if change == 'buffers': value['samples'][1]['buffers_base64'] = []
                if change == 'duplicate': value['samples'][1] = copy.deepcopy(value['samples'][0])
                if change == 'incomplete': value['passed'] = False
                with self.assertRaises(ValidationError): sanitize_capture(value, FUNCTIONS)

    def test_failed_inventory_does_not_publish_baseline_or_allocate(self):
        ledger = {'allocation_requested': False, 'baseline_endpoints': []}
        client = mock.Mock(); client.inventory.side_effect = ValidationError('missing credentials')
        save = mock.Mock()
        with self.assertRaises(ValidationError): inventory_before_allocation(client, ledger, save)
        self.assertFalse(ledger['allocation_requested']); save.assert_not_called(); client.call.assert_not_called()

    def test_failed_capture_is_not_replayed_and_owned_vm_is_released(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / 'config'; account = root / 'accounts/profile'; account.mkdir(parents=True)
            (account / 'sessions.json').write_text('{}')
            state = Path(temporary) / 'private'
            binary = Path(temporary) / 'colab'; binary.write_bytes(b'fake binary')
            probe = Path(temporary) / 'probe'; probe.write_bytes(b'fake probe')
            client = mock.Mock(); client.inventory.return_value = set()
            def call(args, **kwargs):
                if args[0] == 'new':
                    name = args[2]
                    (account / 'sessions.json').write_text(json.dumps({name: {'endpoint': 'owned-endpoint', 'kernel_id': 'owned-kernel'}}))
                    return mock.Mock(returncode=0, stdout=json.dumps({'name': name, 'endpoint': 'owned-endpoint', 'accelerator': 'NONE'}).encode())
                if 'exec' in args: return mock.Mock(returncode=0, stdout=b'[{"status":"succeeded"}]')
                self.assertEqual(args[0], 'stop')
                self.assertEqual(args[-1], 'owned-endpoint')
                return mock.Mock(returncode=0)
            client.call.side_effect = call
            args = ['probe', '--execute', '--config-dir', str(root), '--account', 'profile', '--binary', str(binary),
                '--probe', str(probe), '--kernel', temporary, '--state-dir', str(state)]
            with mock.patch.object(sys, 'argv', args), mock.patch.object(live, 'TerminalClient', return_value=client), \
                    mock.patch.object(live, 'official_functions', return_value=(FUNCTIONS, {})), \
                    mock.patch.object(live.subprocess, 'run', return_value=mock.Mock(returncode=1)) as run, \
                    contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaises(SystemExit) as exited:
                    live.main()
                self.assertEqual(exited.exception.code, 1)
            run.assert_called_once()
            self.assertEqual(client.call.call_count, 3)
            report = json.loads((state / 'report.json').read_text())
            self.assertFalse(report['passed']); self.assertEqual(report['error_class'], 'capture_failed')
            self.assertEqual(report['cleanup'], 'confirmed_released')
            self.assertFalse((state / 'sanitized-samples.json').exists())

    def test_cleanup_does_not_require_probe_or_source_and_never_allocates(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / 'config'; root.mkdir()
            state = Path(temporary) / 'private'; state.mkdir(mode=0o700)
            binary = Path(temporary) / 'colab'; binary.write_bytes(b'fake binary')
            ledger = {'config_dir': str(root.resolve()), 'account': 'profile', 'auth': 'oauth2', 'token_storage': 'keyring',
                'session': 'owned', 'endpoint': 'owned-endpoint', 'baseline_endpoints': [], 'allocation_requested': True}
            (state / 'ledger.json').write_text(json.dumps(ledger))
            client = mock.Mock(); client.call.return_value = mock.Mock(returncode=0); client.inventory.return_value = set()
            args = ['probe', '--cleanup-only', '--config-dir', str(root), '--account', 'profile', '--binary', str(binary),
                '--probe', str(Path(temporary) / 'missing-probe'), '--state-dir', str(state)]
            with mock.patch.object(sys, 'argv', args), mock.patch.object(live, 'TerminalClient', return_value=client), \
                    mock.patch.object(live, 'official_functions', side_effect=AssertionError('must not need source')), \
                    contextlib.redirect_stdout(io.StringIO()):
                live.main()
            client.call.assert_called_once_with(['stop', '-s', 'owned', '--colab-expected-endpoint', 'owned-endpoint'])
            self.assertEqual(json.loads((state / 'report.json').read_text())['cleanup'], 'confirmed_released')

    def test_endpoint_replacement_refused_before_reading_runtime_credentials(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); account = root / 'accounts/profile'; account.mkdir(parents=True)
            (account / 'sessions.json').write_text(json.dumps({'owned': {'endpoint': 'different-endpoint'}}))
            with self.assertRaisesRegex(ValidationError, 'runtime_endpoint_replaced'):
                local_owned_state(root, {'account': 'profile', 'session': 'owned', 'endpoint': 'captured-endpoint'})



if __name__ == '__main__': unittest.main()
