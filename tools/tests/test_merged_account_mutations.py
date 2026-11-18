"""Safety gate: an accidentally selected native binary must never run mutation fixtures."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import unittest
from unittest.mock import patch

TOOLS = Path(__file__).resolve().parents[1]
# Probe modules import their sibling helpers, just as when executed as scripts.
if str(TOOLS) not in sys.path:
    sys.path.insert(0,str(TOOLS))
SPEC = importlib.util.spec_from_file_location('merged_account_mutations', TOOLS / 'compare_merged_account_mutations.py')
probe = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(probe)


class MutationProbeSafetyTests(unittest.TestCase):
    def test_native_or_unknown_capability_fails_closed_before_mutations(self):
        for capability in ({'native_keyring_compiled': True}, {}, {'native_keyring_compiled': None}):
            with self.subTest(capability=capability), patch.object(probe.subprocess,'run', return_value=subprocess.CompletedProcess([],0,json.dumps(capability),'')) as run:
                with self.assertRaises(SystemExit):
                    probe.require_no_native_keyring(Path('/synthetic/colab'))
                self.assertEqual(run.call_count,1)
                self.assertEqual(run.call_args.args[0][1:],['ext','version','--json'])
                self.assertEqual(set(run.call_args.kwargs['env']),{'PATH'})

    def test_failed_capability_command_is_not_treated_as_no_native_backend(self):
        with patch.object(probe.subprocess,'run',side_effect=subprocess.CalledProcessError(1,['colab'])):
            with self.assertRaises(subprocess.CalledProcessError):
                probe.require_no_native_keyring(Path('/synthetic/colab'))


class LoopbackProbePolicyTests(unittest.TestCase):
    def test_only_exact_numeric_host_and_port_are_allowed(self):
        from compare_merged_accounts import loopback_endpoint, loopback_address
        endpoint=loopback_endpoint('http://127.0.0.1:12345')
        self.assertTrue(loopback_address(endpoint,('127.0.0.1',12345)))
        for address in [('127.0.0.1',12346),('localhost',12345),('169.254.169.254',80),('example.invalid',12345),('::1',12345,0,0),'/tmp/socket',None]:
            with self.subTest(address=address):
                self.assertFalse(loopback_address(endpoint,address))

    def test_unsafe_origin_cannot_enable_network_access(self):
        from compare_merged_accounts import loopback_endpoint
        for origin in ['https://127.0.0.1:12345','http://localhost:12345','http://127.0.0.1','http://127.0.0.1:12345/path','http://user@127.0.0.1:12345','http://127.0.0.1:12345?x=1','http://127.0.0.1:12345#fragment']:
            with self.subTest(origin=origin),self.assertRaises(ValueError):
                loopback_endpoint(origin)
