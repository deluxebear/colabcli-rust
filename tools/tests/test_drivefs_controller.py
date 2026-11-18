"""Remote controller contract tests. No real FUSE, DriveFS, or Google access."""
import copy
import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import select
import stat
import subprocess
import sys
import tempfile
import unittest
import types
from unittest import mock

REMOTE = Path(__file__).resolve().parents[2] / "src/drivefs"
sys.path.insert(0, str(REMOTE))
import metadata_shim as shim
import controller as control

SOURCE = (REMOTE / "metadata_shim.py").read_text()
CLIENT = "947318989803-k88lapdik9bledfml8rr69ic6d3rdv57.apps.googleusercontent.com"


class FakeSystem:
    def __init__(self):
        self.entries = [dict(id=1, path="/", filesystem="ext4", source="root")]
        self.processes = {}
        self.events = []
        self.counter = 100
        self.mount_on_launch = True
        self.fail_unmount = False
        self.fail_flush = False
        self.fail_probe = False
        self.fail_drive_launch = False
        self.drive_exits = False
        self.drive_not_started = False
        self.fail_stop = False
        self.skip_readiness = False
        self.phase_at_launch = []

    def mounts(self):
        return copy.deepcopy(self.entries)

    def preflight(self):
        self.events.append(("preflight",))

    def launch(self, argv, environment, log_path, operation):
        location = Path(log_path).parent
        state = json.loads((location / "state.json").read_text())
        self.phase_at_launch.append(state["stage"])
        if argv[0] == control.DRIVE and self.fail_drive_launch:
            raise control.ControlError("launch_outcome_unknown")
        if argv[0] == control.DRIVE and self.drive_not_started:
            raise control.NotStarted("process_not_started")
        self.counter += 1
        record = dict(pid=self.counter, start_ticks=str(self.counter * 3), executable=os.path.realpath(argv[0]),
                      command_hash=hashlib.sha256(("\0".join(argv) + "\0").encode()).hexdigest())
        self.processes[record["pid"]] = (operation, record)
        self.events.append(("launch", operation, list(argv), dict(environment)))
        Path(log_path).write_text("potentially sensitive private log")
        Path(log_path).chmod(0o600)
        if argv[0] == control.DRIVE:
            if self.drive_exits:
                self.processes.pop(record["pid"])
            elif self.mount_on_launch:
                self.entries.append(dict(id=self.counter, path=argv[-1], filesystem="fuse.drivefs", source="drive"))
        elif not self.skip_readiness:
            shim.write_private(str(location / "shim-ready.json"), dict(version=1, pid=record["pid"], port=8012))
        return record

    def alive(self, record, operation):
        current = self.processes.get(record["pid"])
        if current is None:
            return False
        if current != (operation, record):
            raise control.ControlError("process_ownership_mismatch")
        return True

    def stop(self, record, operation):
        self.alive(record, operation)
        if self.fail_stop:
            raise control.ControlError("process_stop_timeout")
        self.events.append(("stop", operation, record["pid"]))
        self.processes.pop(record["pid"], None)

    def discover(self, operation, expected):
        matches = [record for owner, record in self.processes.values()
                   if owner == operation and all(record.get(key) == value for key, value in expected.items())]
        if len(matches) > 1:
            raise control.ControlError("ambiguous_owned_process")
        return matches[0] if matches else None

    def probe(self, port):
        self.events.append(("probe", port))
        if self.fail_probe:
            raise control.ControlError("metadata_probe_failed")

    def flush(self, record, operation):
        self.events.append(("flush", operation))
        if self.fail_flush:
            raise control.ControlError("drive_flush_failed")

    def unmount(self, path, expected):
        self.events.append(("unmount", path))
        if self.fail_unmount:
            raise control.ControlError("unmount_failed")
        self.entries = [entry for entry in self.entries if entry["path"] != path]


class ControllerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name).resolve()
        self.root = self.directory / "private-state"
        self.path = self.directory / "drive 空格'$(printf injected)"
        self.system = FakeSystem()
        now = [0.0]

        def sleep(interval):
            now[0] += interval

        self.controller = control.Controller(str(self.root), self.system, lambda: now[0], sleep)
        self.credentials = dict(version=1, client_id="fixture-client", client_secret="fixture-client-secret",
                                refresh_token="fixture-refresh-secret", token_uri="https://oauth2.googleapis.com/token",
                                user_id="fixture-gaia", email="fixture@example.invalid", scopes=[shim.DRIVE_SCOPE])

    def request(self, operation="a" * 32, path=None, **options):
        return dict(operation_id=operation, mountpoint=str(path or self.path), **options)

    def mount(self, operation="a" * 32, path=None, **options):
        return self.controller.run("mount", self.request(operation, path, **options), self.credentials, SOURCE)

    def read_state(self, operation="a" * 32):
        return json.loads((self.root / "operations" / operation / "state.json").read_text())

    def unmount(self, request):
        result = self.controller.run("umount", request)
        if result["ok"] and result["stage"] == "credentials_pending":
            owner = result["owner_operation_id"]
            raw = (self.root / "operations" / owner / "credentials.json").read_bytes()
            # Simulate the client's explicit acknowledgement after adoption.
            request = dict(request, owner_operation_id=owner, credentials_receipt=hashlib.sha256(raw).hexdigest())
            result = self.controller.run("finalize", request)
        return result

    def test_status_reads_only_private_refresh_enum_and_cleanup_removes_file(self):
        self.assertTrue(self.mount()["ok"])
        path = self.root / "operations" / ("a" * 32) / shim.REFRESH_STATUS_FILE
        request = self.request("b" * 32)
        self.assertNotIn("refresh_status", self.controller.run("status", request))
        events = list(self.system.events)
        for value in ("fixture-private-upstream-description", "invalid_grant", "refresh_failed", "ok"):
            shim.write_private(str(path), {"version": 1, "status": value, "untrusted": "fixture-private"})
            output = self.controller.run("status", request)
            if value in shim.REFRESH_STATUSES:
                self.assertEqual(output["refresh_status"], value)
            else:
                self.assertNotIn("refresh_status", output)
            self.assertNotIn("fixture-private", json.dumps(output))
        path.chmod(0o644)
        self.assertNotIn("refresh_status", self.controller.run("status", request))
        self.assertEqual(self.system.events, events)
        self.assertTrue(self.unmount(dict(request, owner_operation_id="a" * 32))["ok"])
        self.assertFalse(path.exists())

    def test_explicit_unmount_recovers_only_exact_owned_drive_launch_snapshot_gap(self):
        self.assertTrue(self.mount()["ok"])
        state = self.read_state()
        state.update(stage="unknown", failure_stage="drive_starting", mount=None, drive=None)
        self.system.entries[-1]["filesystem"] = "fuse.drive"
        self.controller.save(state, "unknown")
        state_path = self.root / "operations" / ("a" * 32) / "state.json"
        before = state_path.read_bytes()
        events = list(self.system.events)
        result = self.controller.run("status", self.request("b" * 32))
        self.assertTrue(result["ok"] and result["mounted"] and result["drive_alive"], result)
        self.assertEqual(result["stage"], "unknown")
        self.assertEqual(state_path.read_bytes(), before)
        self.assertEqual(self.system.events, events)
        result = self.unmount(self.request("b" * 32, owner_operation_id="a" * 32))
        self.assertTrue(result["ok"], result)
        self.assertEqual(result["stage"], "finished")
        self.assertFalse(result["mounted"])

    def test_snapshot_gap_does_not_adopt_wrong_filesystem_launch_identity_or_stage(self):
        self.assertTrue(self.mount()["ok"])
        original = self.read_state()
        for variant in ("filesystem", "command", "stage", "missing_launch", "dead"):
            with self.subTest(variant=variant):
                state = copy.deepcopy(original)
                state.update(stage="unknown", failure_stage="drive_starting", mount=None)
                self.system.entries[-1]["filesystem"] = "fuse.drive"
                if variant == "filesystem": self.system.entries[-1]["filesystem"] = "fuse.other"
                if variant == "command": state["drive_launch"]["command_hash"] = "0" * 64
                if variant == "stage": state["failure_stage"] = "mounting"
                if variant == "missing_launch": state.pop("drive_launch")
                if variant == "dead":
                    saved = self.system.processes.pop(state["drive"]["pid"])
                self.controller.save(state, "unknown")
                before = list(self.system.events)
                result = self.controller.run("umount", self.request("b" * 32, owner_operation_id="a" * 32))
                self.assertFalse(result["ok"], result)
                self.assertEqual(self.system.events, before)
                if variant == "dead": self.system.processes[state["drive"]["pid"]] = saved

    def test_system_flush_requires_unique_owned_ipc_and_reports_timeout(self):
        system = control.System()
        record = dict(pid=123)
        header = "Num RefCount Protocol Flags Type St Inode Path\n"
        row = "0: 2 0 00010000 0001 01 456 /tmp/drivefs_ipc." + str(os.getuid()) + "\n"
        for variant in ("missing", "duplicate", "foreign", "timeout", "success"):
            with self.subTest(variant=variant), \
                    mock.patch.object(system, "alive", return_value=True), \
                    mock.patch.object(Path, "read_text", return_value=header + ("" if variant == "missing" else row * (2 if variant == "duplicate" else 1))), \
                    mock.patch.object(Path, "iterdir", return_value=[Path("/proc/123/fd/4")]), \
                    mock.patch.object(os, "readlink", return_value="socket:[999]" if variant == "foreign" else "socket:[456]"), \
                    mock.patch.object(subprocess, "run", return_value=types.SimpleNamespace(returncode=0)) as run:
                if variant == "timeout":
                    run.side_effect = subprocess.TimeoutExpired("drive", 35)
                if variant == "success":
                    system.flush(record, "a" * 32)
                    self.assertEqual(run.call_args.args[0], [control.DRIVE, "--push_changes_and_quit", "--single_process", "--timeout_sec=30"])
                else:
                    with self.assertRaises(control.ControlError):
                        system.flush(record, "a" * 32)
                    if variant != "timeout":
                        run.assert_not_called()

    def test_flush_failure_preserves_mount_processes_and_credentials_until_retry(self):
        self.assertTrue(self.mount()["ok"])
        self.system.fail_flush = True
        result = self.controller.run("umount", self.request("b" * 32, owner_operation_id="a" * 32))
        self.assertFalse(result["ok"])
        self.assertEqual(result["error"], "drive_flush_failed")
        self.assertTrue(self.controller.table(str(self.path)))
        self.assertEqual(len(self.system.processes), 2)
        self.assertTrue((self.root / "operations" / ("a" * 32) / "credentials.json").exists())
        self.assertFalse(any(event[0] in ("unmount", "stop") for event in self.system.events))
        self.system.fail_flush = False
        result = self.controller.run("umount", self.request("c" * 32, owner_operation_id="a" * 32))
        self.assertTrue(result["ok"], result)
        events = [event[0] for event in self.system.events]
        self.assertLess(events.index("flush"), events.index("unmount"))
        self.assertLess(events.index("unmount"), events.index("stop"))

    def test_mount_argv_private_state_and_readonly_status_reuse(self):
        result = self.mount(readonly=True)
        self.assertTrue(result["ok"], result)
        self.assertTrue(result["mounted"])
        self.assertTrue(result["helper_alive"])
        self.assertTrue(result["drive_alive"])
        self.assertEqual(self.system.phase_at_launch, ["shim_starting", "drive_starting"])
        launched = [event for event in self.system.events if event[0] == "launch"]
        command = launched[-1][2]
        self.assertEqual(command[-1], str(self.path))
        self.assertIn("--single_process", command)
        self.assertIn("read_only_mode:true", command[1])
        self.assertIn("mount_point_path:" + str(self.path), command[-2])
        self.assertTrue(any(argument.startswith("--metadata_server_auth_uri=127.0.0.1:") for argument in command))
        self.assertNotIn("fixture-refresh-secret", json.dumps(command))
        self.assertEqual(launched[0][3]["HOME"], launched[1][3]["HOME"])
        self.assertNotIn(".", launched[-1][3]["PATH"].split(":"))
        for path in (self.root, self.root / "operations", self.root / "operations" / ("a" * 32)):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700)
        location = self.root / "operations" / ("a" * 32)
        self.assertEqual(stat.S_IMODE((location / "credentials.json").stat().st_mode), 0o600)
        state_before = (location / "state.json").read_bytes()
        result = self.controller.run("status", self.request("b" * 32))
        self.assertTrue(result["mounted"])
        self.assertEqual((location / "state.json").read_bytes(), state_before)
        result = self.mount("b" * 32, readonly=True)
        self.assertTrue(result["ok"])
        self.assertEqual(result["operation_id"], "b" * 32)
        self.assertEqual(result["owner_operation_id"], "a" * 32)
        self.assertEqual(len([event for event in self.system.events if event[0] == "launch"]), 2)
        self.assertFalse((self.root / "operations" / ("b" * 32)).exists())
        for secret in ("fixture-refresh-secret", "fixture-client-secret"):
            self.assertNotIn(secret, json.dumps(result))
            self.assertNotIn(secret.encode(), state_before)

    def test_requested_metadata_port_identity_and_unmount_owner_guard(self):
        result = self.mount(port=8137, readonly=True)
        self.assertTrue(result["ok"], result)
        self.assertTrue(result["readonly"])
        expected = hashlib.sha256(json.dumps([self.credentials["client_id"], self.credentials["user_id"]]).encode()).hexdigest()
        self.assertEqual(result["identity"], expected)
        launch = next(event for event in self.system.events if event[0] == "launch")
        command = launch[2]
        self.assertEqual(command[command.index("--port") + 1], "8137")
        before = copy.deepcopy(self.system.events)
        result = self.controller.run("umount", self.request("b" * 32, owner_operation_id="c" * 32))
        self.assertFalse(result["ok"])
        self.assertEqual(result["error"], "mount_ownership_mismatch")
        self.assertEqual(self.system.events, before)
        self.assertTrue(self.controller.run("status", self.request("d" * 32))["mounted"])

    def test_extra_flags_are_exact_argv_and_cannot_override_controller_fields(self):
        for flags in (["--features=read_only_mode:false"], ["--preferences=mount_point_path:/other"],
                      ["--single_process=false"], ["--push_changes_and_quit"], ["--timeout_sec=0"],
                      ["--refresh_token=private"], ["--metadata_server_auth_uri=other"], ["--user_id=other"],
                      ["--help"], ["/other"], ["--curl_logging\n--bad"], ["--x=" + "a" * 4097], [True]):
            result = self.mount(extra_flags=flags)
            self.assertEqual(result["error"], "invalid_options", flags)
            self.assertFalse(any(event[0] == "launch" for event in self.system.events))
        flags = ["--curl_logging", "--example_value=quote' 中文;$(ignored)"]
        result = self.mount(extra_flags=flags)
        self.assertTrue(result["ok"], result)
        command = [event for event in self.system.events if event[0] == "launch"][-1][2]
        self.assertEqual(command[-3:-1], flags)
        self.assertEqual(command[-1], str(self.path))
        self.assertEqual(self.read_state()["extra_flags"], flags)
        self.assertTrue(self.mount("b" * 32, extra_flags=flags)["ok"])
        self.assertEqual(self.mount("c" * 32, extra_flags=["--different"])["error"], "operation_pending")

    def test_unmount_failure_keeps_mountpoint_credentials_and_processes(self):
        self.assertTrue(self.mount()["ok"])
        self.system.fail_unmount = True
        before = copy.deepcopy(self.system.processes)
        location = self.root / "operations" / ("a" * 32)
        credentials = (location / "credentials.json").read_bytes()
        result = self.unmount(self.request("b" * 32, purge_state=True))
        self.assertEqual(result["error"], "unmount_failed")
        self.assertTrue(self.path.is_dir())
        self.assertEqual(self.system.processes, before)
        self.assertEqual((location / "credentials.json").read_bytes(), credentials)
        self.assertFalse(any(event[0] == "stop" for event in self.system.events))
        self.assertEqual(self.read_state()["stage"], "unmounting")
        self.system.fail_unmount = False
        result = self.unmount(self.request("c" * 32, purge_state=True))
        self.assertTrue(result["ok"], result)
        self.assertFalse(result["mounted"])
        self.assertEqual(result["stage"], "finished")
        self.assertFalse(self.path.exists())
        self.assertFalse((location / "credentials.json").exists())
        self.assertFalse((location / "home").exists())
        self.assertEqual(self.system.processes, {})
        self.assertTrue(self.unmount(self.request("d" * 32, purge_state=True))["ok"])

    def test_mountpoint_contents_are_never_recursively_removed(self):
        self.assertTrue(self.mount()["ok"])
        contents = self.path / "local-or-remote-data"
        contents.write_text("must survive")
        with mock.patch.object(control.shutil, "rmtree", side_effect=AssertionError("must not recursively delete")):
            result = self.unmount(self.request("b" * 32))
        self.assertTrue(result["ok"], result)
        self.assertEqual(contents.read_text(), "must survive")

    def test_two_mounts_have_isolated_home_processes_and_cleanup(self):
        first = self.mount()
        other = self.directory / "another-drive"
        second = self.mount("b" * 32, other)
        self.assertTrue(first["ok"] and second["ok"])
        second_state = self.read_state("b" * 32)
        self.assertTrue(self.unmount(self.request("c" * 32))["ok"])
        status = self.controller.run("status", self.request("d" * 32, other))
        self.assertTrue(status["mounted"] and status["helper_alive"] and status["drive_alive"])
        self.assertEqual(set(self.system.processes), {second_state["helper"]["pid"], second_state["drive"]["pid"]})
        self.assertTrue((self.root / "operations" / ("b" * 32) / "credentials.json").exists())

    def test_foreign_mount_replacement_and_reused_pid_are_never_stopped(self):
        self.assertTrue(self.mount()["ok"])
        before = copy.deepcopy(self.system.processes)
        self.system.entries[-1]["id"] += 1
        for action in ("status", "umount", "mount"):
            result = self.controller.run(action, self.request("b" * 32, force=True), self.credentials, SOURCE)
            self.assertEqual(result["error"], "mount_ownership_mismatch")
        self.assertEqual(self.system.processes, before)
        self.assertFalse(any(event[0] == "unmount" for event in self.system.events))
        self.system.entries[-1]["id"] -= 1
        record = self.read_state()["drive"]
        replacement = dict(record, start_ticks="replacement-process")
        self.system.processes[record["pid"]] = ("a" * 32, replacement)
        result = self.unmount(self.request("b" * 32))
        self.assertEqual(result["error"], "process_ownership_mismatch")
        self.assertFalse(any(event[0] == "unmount" for event in self.system.events))

    def test_unknown_launch_result_cannot_be_replayed_or_claimed_clean(self):
        self.system.fail_drive_launch = True
        result = self.mount()
        self.assertEqual(result["error"], "launch_outcome_unknown")
        self.assertEqual(self.read_state()["stage"], "unknown")
        events = list(self.system.events)
        self.assertEqual(self.mount()["error"], "operation_pending")
        self.assertEqual(self.mount("b" * 32)["error"], "operation_pending")
        self.assertEqual(self.unmount(self.request("b" * 32))["error"], "process_result_unknown")
        status = self.controller.run("status", self.request("b" * 32))
        self.assertTrue(status["ok"])
        self.assertEqual(status["stage"], "unknown")
        self.assertEqual(self.system.events, events)

    def test_timeout_and_probe_failure_preserve_recovery_state_and_hide_logs(self):
        self.system.mount_on_launch = False
        result = self.mount(timeout=1)
        self.assertEqual(result["error"], "mount_timeout")
        self.assertEqual(self.read_state()["stage"], "unknown")
        self.assertEqual(self.mount("b" * 32)["error"], "operation_pending")
        self.assertNotIn("potentially sensitive", json.dumps(result))
        self.assertTrue(self.unmount(self.request("b" * 32))["ok"])
        self.system.fail_probe = True
        result = self.mount("c" * 32)
        self.assertEqual(result["error"], "metadata_probe_failed")
        self.assertEqual(self.read_state("c" * 32)["stage"], "unknown")
        self.assertTrue((self.root / "operations" / ("c" * 32) / "credentials.json").exists())
        self.assertTrue(self.unmount(self.request("d" * 32))["ok"])

    def test_helper_timeout_and_drive_early_exit_are_reported_and_cleanable(self):
        self.system.skip_readiness = True
        self.assertEqual(self.mount(timeout=1)["error"], "helper_timeout")
        self.assertTrue(self.unmount(self.request("b" * 32))["ok"])
        self.system.skip_readiness = False
        self.system.drive_exits = True
        self.assertEqual(self.mount("c" * 32)["error"], "drive_exited")
        self.assertFalse(self.controller.run("status", self.request("d" * 32))["mounted"])
        self.assertTrue(self.unmount(self.request("d" * 32))["ok"])

    def test_definitely_unstarted_child_does_not_leave_a_false_unknown_pid(self):
        self.system.drive_not_started = True
        self.assertEqual(self.mount()["error"], "process_not_started")
        self.assertNotIn("drive_launch", self.read_state())
        self.assertTrue(self.unmount(self.request("b" * 32))["ok"])

    def test_force_replaces_only_owned_mount_after_successful_unmount(self):
        self.assertTrue(self.mount()["ok"])
        self.credentials["user_id"] = "other-fixture-gaia"
        self.assertEqual(self.mount("b" * 32)["error"], "operation_pending")
        self.assertEqual(self.mount("b" * 32, force=True)["error"], "credentials_recovery_required")
        self.assertFalse((self.root / "operations" / ("b" * 32)).exists())
        self.assertTrue(self.unmount(self.request("c" * 32))["ok"])
        self.assertTrue(self.mount("b" * 32, force=True)["ok"])
        self.assertEqual(self.read_state()["stage"], "finished")
        events = [event[0] for event in self.system.events]
        self.assertLess(events.index("unmount"), events.index("stop"))
        self.assertFalse((self.root / "operations" / ("a" * 32) / "credentials.json").exists())

    def test_rt_argv_is_not_journalled_and_no_shim_is_allocated(self):
        self.assertEqual(self.mount(mode="rt")["error"], "rt_client_mismatch")
        self.credentials["client_id"] = CLIENT
        result = self.mount(mode="rt")
        self.assertTrue(result["ok"], result)
        launched = [event for event in self.system.events if event[0] == "launch"]
        self.assertEqual(len(launched), 1)
        self.assertIn("--refresh_token=fixture-refresh-secret", launched[0][2])
        self.assertFalse((self.root / "operations" / ("a" * 32) / "credentials.json").exists())
        self.assertNotIn("fixture-refresh-secret", json.dumps(self.read_state()))
        self.assertFalse(result["helper_alive"])

    def test_readonly_status_without_state_and_foreign_mount_never_create_files(self):
        result = self.controller.run("status", self.request())
        self.assertTrue(result["ok"])
        self.assertFalse(result["managed"] or result["mounted"])
        self.assertFalse(self.root.exists())
        self.system.entries.append(dict(id=2, path=str(self.path), filesystem="fuse.foreign", source="other"))
        self.assertEqual(self.mount()["error"], "foreign_mount")
        self.assertEqual(self.unmount(self.request())["error"], "foreign_mount")
        self.assertEqual(self.system.events, [])

    def test_control_directory_inside_fuse_is_rejected_before_writes(self):
        self.system.entries.append(dict(id=10, path=str(self.directory), filesystem="fuse.other", source="other"))
        self.assertEqual(self.mount()["error"], "state_on_fuse")
        self.assertFalse(self.root.exists())

    def test_path_options_nonempty_symlink_and_lock_guards_precede_launch(self):
        for path in ("/", "/content/../drive", "/content/a,b", "/content/a:b", str(self.root / "inside"), str(self.directory)):
            result = self.mount(path=path)
            self.assertFalse(result["ok"], path)
        self.path.mkdir()
        (self.path / "user-data").write_text("keep")
        self.assertEqual(self.mount()["error"], "mountpoint_not_empty")
        (self.path / "user-data").unlink()
        self.path.rmdir()
        self.path.symlink_to(self.directory, target_is_directory=True)
        self.assertEqual(self.mount()["error"], "unsafe_mountpoint")
        self.path.unlink()
        for options in (dict(timeout=True), dict(timeout=0), dict(readonly="true"), dict(mode="unknown"), dict(extra_flags=["--refresh_token=bad"])):
            self.assertEqual(self.mount(**options)["error"], "invalid_options")
        with self.controller.lock(str(self.path)):
            self.assertEqual(self.mount()["error"], "mount_busy")
        self.assertFalse(any(event[0] == "launch" for event in self.system.events))

    def test_process_stop_failure_keeps_credentials_for_retry(self):
        self.assertTrue(self.mount()["ok"])
        self.system.fail_stop = True
        result = self.unmount(self.request("b" * 32))
        self.assertEqual(result["error"], "process_stop_timeout")
        self.assertTrue((self.root / "operations" / ("a" * 32) / "credentials.json").exists())
        self.assertTrue(self.path.exists())
        self.system.fail_stop = False
        self.assertTrue(self.unmount(self.request("c" * 32))["ok"])

    def test_mount_parser_uses_kernel_identity_and_unescapes_paths(self):
        source = "22 1 0:2 / /content/drive\\040space rw - fuse.drivefs drive rw\n"
        self.assertEqual(control.parse_mounts(source), [dict(id=22, path="/content/drive space", filesystem="fuse.drivefs", source="drive")])
        for source in ("", "malformed", "1 2 - nofields"):
            with self.assertRaisesRegex(control.ControlError, "mount_table_unavailable"):
                control.parse_mounts(source)

    def test_missing_pid_checkpoint_is_discovered_without_status_writes(self):
        self.assertTrue(self.mount()["ok"])
        state = self.read_state()
        state["helper"] = None
        state["drive"] = None
        location = self.root / "operations" / ("a" * 32) / "state.json"
        shim.write_private(str(location), state)
        before = location.read_bytes()
        status = self.controller.run("status", self.request("b" * 32))
        self.assertTrue(status["ok"], status)
        self.assertTrue(status["helper_alive"] and status["drive_alive"])
        self.assertFalse(status["helper_unknown"] or status["drive_unknown"])
        self.assertEqual(before, location.read_bytes())
        self.assertTrue(self.unmount(self.request("b" * 32))["ok"])

    def test_finished_state_does_not_signal_a_reused_pid(self):
        self.assertTrue(self.mount()["ok"])
        state = self.read_state()
        self.assertTrue(self.unmount(self.request("b" * 32))["ok"])
        record = state["drive"]
        self.system.processes[record["pid"]] = ("another-owner", dict(record, start_ticks="reused"))
        events = list(self.system.events)
        self.assertTrue(self.unmount(self.request("c" * 32))["ok"])
        self.assertTrue(self.controller.run("status", self.request("c" * 32))["ok"])
        self.assertEqual(events, self.system.events)
        self.assertIn(record["pid"], self.system.processes)

    def test_replaced_mountpoint_inode_is_not_removed(self):
        self.assertTrue(self.mount()["ok"])
        moved = self.directory / "original-directory"
        self.path.rename(moved)
        self.path.mkdir()
        result = self.unmount(self.request("b" * 32))
        self.assertEqual(result["error"], "mountpoint_replaced")
        self.assertTrue(self.path.is_dir())
        self.assertTrue(moved.is_dir())

    def test_purge_refuses_nested_mounts_and_preserves_other_operation_state(self):
        self.assertTrue(self.mount()["ok"])
        home = self.root / "operations" / ("a" * 32) / "home"
        nested = home / "foreign-fuse"
        nested.mkdir()
        data = nested / "user-data"
        data.write_text("keep")
        self.system.entries.append(dict(id=999, path=str(nested), filesystem="fuse.other", source="other"))
        result = self.unmount(self.request("b" * 32, purge_state=True))
        self.assertEqual(result["error"], "state_contains_mount")
        self.assertEqual(data.read_text(), "keep")
        self.assertEqual(self.read_state()["stage"], "finalizing")

    def test_unmount_freezes_rotation_until_exact_owner_and_receipt_are_acknowledged(self):
        self.assertTrue(self.mount()["ok"])
        location = self.root / "operations" / ("a" * 32)
        credentials = json.loads((location / "credentials.json").read_text())
        credentials["refresh_token"] = "rotated-remote-fixture-token"
        shim.write_private(str(location / "credentials.json"), credentials)
        result = self.controller.run("umount", self.request("b" * 32))
        self.assertTrue(result["ok"])
        self.assertEqual(result["stage"], "credentials_pending")
        self.assertFalse(result["mounted"] or result["drive_alive"] or result["helper_alive"])
        self.assertTrue(self.path.exists())
        raw = (location / "credentials.json").read_bytes()
        self.system.entries.append(self.read_state()["mount"])
        self.assertEqual(self.controller.run("status", self.request("b" * 32))["error"], "mount_ownership_mismatch")
        self.system.entries.pop()
        receipt = hashlib.sha256(raw).hexdigest()
        self.assertNotIn("rotated-remote-fixture-token", json.dumps(result))
        for owner, digest, error in [("c" * 32, receipt, "credentials_owner_mismatch"),
                                     ("a" * 32, "0" * 64, "credentials_receipt_mismatch")]:
            request = self.request("b" * 32, owner_operation_id=owner, credentials_receipt=digest)
            self.assertEqual(self.controller.run("finalize", request)["error"], error)
            self.assertEqual((location / "credentials.json").read_bytes(), raw)
        request = self.request("b" * 32, owner_operation_id="a" * 32, credentials_receipt=receipt)
        self.assertEqual(self.controller.run("finalize", dict(request, purge_state="yes"))["error"], "invalid_options")
        self.assertEqual((location / "credentials.json").read_bytes(), raw)
        self.assertEqual(self.controller.run("finalize", request)["stage"], "finished")
        self.assertFalse((location / "credentials.json").exists())
        self.assertFalse(self.path.exists())
        self.assertTrue(self.controller.run("finalize", request)["ok"])

    def test_finalize_crash_after_secret_removal_can_resume_without_replaying_unmount(self):
        self.assertTrue(self.mount()["ok"])
        location = self.root / "operations" / ("a" * 32)
        self.controller.run("umount", self.request("b" * 32))
        receipt = hashlib.sha256((location / "credentials.json").read_bytes()).hexdigest()
        request = self.request("b" * 32, owner_operation_id="a" * 32, credentials_receipt=receipt)
        real_unlink = os.unlink

        def failure(path, *args, **kwargs):
            if str(path).endswith("helper.log"):
                raise OSError("fixture interruption")
            return real_unlink(path, *args, **kwargs)

        with mock.patch.object(control.os, "unlink", side_effect=failure):
            self.assertFalse(self.controller.run("finalize", request)["ok"])
        self.assertFalse((location / "credentials.json").exists())
        self.assertEqual(self.read_state()["stage"], "finalizing")
        unmounts = [event for event in self.system.events if event[0] == "unmount"]
        self.assertTrue(self.controller.run("finalize", request)["ok"])
        self.assertEqual([event for event in self.system.events if event[0] == "unmount"], unmounts)

    def test_unmount_fallback_checks_replacement_before_next_command(self):
        system = control.System()
        expected = dict(id=10, path=str(self.path), filesystem="fuse.drivefs", source="drive")
        replacement = dict(expected, id=11)
        with mock.patch.object(system, "mounts", side_effect=[[expected], [replacement], [replacement]]), \
                mock.patch.object(control.subprocess, "run") as run:
            with self.assertRaisesRegex(control.ControlError, "mount_ownership_mismatch"):
                system.unmount(str(self.path), expected)
            self.assertEqual(run.call_count, 1)
            self.assertEqual(run.call_args.args[0][-1], str(self.path))
            self.assertNotIn("shell", run.call_args.kwargs)

    def test_pidfd_signalling_rechecks_identity_and_uses_stable_handle(self):
        system = control.System()
        record = dict(pid=123, start_ticks="456", executable="fixture", command_hash="hash")
        with mock.patch.object(control.os, "pidfd_open", create=True, return_value=77), \
                mock.patch.object(control.os, "close") as close, \
                mock.patch.object(control.signal, "pidfd_send_signal", create=True) as send, \
                mock.patch.object(control.select, "select", return_value=([77], [], [])), \
                mock.patch.object(system, "alive", return_value=True) as alive:
            system.stop(record, "a" * 32)
            self.assertEqual(alive.call_count, 2)
            send.assert_called_once_with(77, control.signal.SIGTERM)
            close.assert_called_once_with(77)
        with mock.patch.object(control.os, "pidfd_open", create=True, return_value=77), \
                mock.patch.object(control.os, "close"), \
                mock.patch.object(control.signal, "pidfd_send_signal", create=True) as send, \
                mock.patch.object(system, "alive", side_effect=[True, control.ControlError("process_ownership_mismatch")]):
            with self.assertRaises(control.ControlError):
                system.stop(record, "a" * 32)
            send.assert_not_called()

    def test_launch_observes_same_child_through_transient_proc_identity(self):
        system = control.System()
        argv = [sys.executable, "fixture-script"]
        expected = dict(pid=123, start_ticks="1", executable=os.path.realpath(argv[0]),
            command_hash=hashlib.sha256(b"\0".join(os.fsencode(arg) for arg in argv) + b"\0").hexdigest())
        child = mock.Mock(pid=123)
        child.poll.return_value = None
        with mock.patch.object(control.subprocess, "Popen", return_value=child) as spawn, \
                mock.patch.object(system, "process", side_effect=[control.ControlError("process_identity_unavailable"), expected]) as observe, \
                mock.patch.object(control.time, "sleep"):
            self.assertEqual(system.launch(argv, {}, str(self.directory / "retry.log"), "a" * 32), expected)
            self.assertEqual(observe.call_count, 2)
            spawn.assert_called_once()

    def test_launch_identity_deadline_and_exit_never_respawn_or_accept_wrong_process(self):
        for exited in (False, True):
            with self.subTest(exited=exited):
                system = control.System()
                child = mock.Mock(pid=123)
                child.poll.return_value = 1 if exited else None
                with mock.patch.object(control.subprocess, "Popen", return_value=child) as spawn, \
                        mock.patch.object(system, "process", return_value=dict(executable="other", command_hash="wrong")), \
                        mock.patch.object(system, "discover", return_value=None), \
                        mock.patch.object(control.time, "monotonic", side_effect=[0, 3]):
                    with self.assertRaisesRegex(control.ControlError, "process_exited" if exited else "process_identity_unavailable"):
                        system.launch(["fixture"], {}, str(self.directory / ("exit" if exited else "deadline")), "a" * 32)
                    spawn.assert_called_once()

    def test_zombie_proc_is_dead_without_reading_empty_environment(self):
        system = control.System()
        fields = ["Z"] + ["0"] * 21
        with mock.patch.object(control.Path, "read_text", return_value="123 (drive) " + " ".join(fields)), \
                mock.patch.object(control.Path, "read_bytes") as read:
            self.assertIsNone(system.process(123, "a" * 32))
            read.assert_not_called()

    def test_exited_launcher_can_resolve_only_a_unique_exact_owned_descendant(self):
        system = control.System()
        child = mock.Mock(pid=123)
        child.poll.return_value = 0
        record = dict(pid=456, start_ticks="1", executable="fixture", command_hash="hash")
        with mock.patch.object(control.subprocess, "Popen", return_value=child) as spawn, \
                mock.patch.object(system, "discover", return_value=record) as discover:
            self.assertEqual(system.launch(["fixture"], {}, str(self.directory / "daemon.log"), "a" * 32), record)
            expected = discover.call_args.args[1]
            self.assertEqual(expected["executable"], os.path.realpath("fixture"))
            self.assertEqual(expected["command_hash"], hashlib.sha256(b"fixture\0").hexdigest())
            spawn.assert_called_once()

    def test_io_failure_after_popen_is_never_classified_as_not_started(self):
        actual_fdopen = os.fdopen

        @contextlib.contextmanager
        def failing_close(descriptor, mode):
            with actual_fdopen(descriptor, mode) as output:
                yield output
            raise OSError("failure after child launch")

        with mock.patch.object(control.os, "fdopen", side_effect=failing_close), \
                mock.patch.object(control.subprocess, "Popen") as popen:
            with self.assertRaises(OSError):
                control.System().launch(["fixture"], {}, str(self.directory / "late-io.log"), "a" * 32)
            popen.assert_called_once()

    @unittest.skipUnless(sys.platform.startswith("linux") and hasattr(os, "pidfd_open"), "Linux pidfd only")
    def test_linux_proc_identity_and_pidfd_terminate_only_the_owned_child(self):
        operation = "e" * 32
        environment = dict(os.environ, COLAB_CLI_DRIVEFS_OPERATION=operation)
        child_code = "import time; print('ready', flush=True); time.sleep(30)"
        with subprocess.Popen([sys.executable, "-c", child_code], env=environment,
                              stdout=subprocess.PIPE, stderr=subprocess.DEVNULL) as child:
            try:
                # Read /proc only after the child has executed with its new environment.
                self.assertTrue(select.select([child.stdout], [], [], 5)[0], "child did not become ready")
                self.assertEqual(child.stdout.readline(), b"ready\n")
                system = control.System()
                record = system.process(child.pid, operation)
                self.assertIsNotNone(record)
                self.assertTrue(system.alive(record, operation))
                with self.assertRaisesRegex(control.ControlError, "process_ownership_mismatch"):
                    system.process(child.pid, "f" * 32)
                self.assertEqual(system.discover(operation, {key: record[key] for key in ("executable", "command_hash")}), record)
                system.stop(record, operation)
                child.wait(timeout=3)
            finally:
                if child.poll() is None:
                    child.kill()
                    child.wait(timeout=3)


class WrapperTests(unittest.TestCase):
    def execute_wrapper(self, action, secret, controller_source):
        request = dict(operation_id="a" * 32, mountpoint="/content/drive")
        payload = dict(action=action, request=request, prompt="COLAB_DRIVE_CONFIG:" + "a" * 32,
                       shim_source=SOURCE, controller_source=controller_source)
        template = (REMOTE / "wrapper.py").read_text()
        source = template.replace("__COLAB_DRIVEFS_PAYLOAD__", json.dumps(json.dumps(payload)), 1)
        self.assertNotIn(secret, source)
        displays = []
        module = types.ModuleType("IPython.display")
        module.display = lambda result, **kwargs: displays.append((result, kwargs))
        previous = sys.modules.get("metadata_shim")
        output = io.StringIO()
        with mock.patch.dict(sys.modules, {"IPython.display": module}), \
                mock.patch("getpass.getpass", return_value=secret) as getpass, \
                contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            namespace = {}
            exec(compile(source, "<actual-drivefs-wrapper>", "exec"), namespace)
        self.assertIs(sys.modules.get("metadata_shim"), previous)
        self.assertEqual(output.getvalue(), "")
        self.assertNotIn("_colab_cli_drivefs_operation", namespace)
        self.assertEqual(len(displays), 1)
        self.assertEqual(displays[0][1], dict(raw=True))
        result = displays[0][0]["application/vnd.colab-cli.drivefs-result+json"]
        return result, getpass

    def test_wrapper_transfers_password_input_as_data_and_restores_modules(self):
        code = '''import metadata_shim as shim
class Controller:
    def run(self, action, request, credentials, source):
        assert credentials["refresh_token"] == "fixture-secret"
        assert source and action == "mount"
        return dict(version=1, operation_id=request["operation_id"], ok=True)
'''
        result, getpass = self.execute_wrapper("mount", json.dumps(dict(refresh_token="fixture-secret")), code)
        self.assertTrue(result["ok"])
        getpass.assert_called_once_with("COLAB_DRIVE_CONFIG:" + "a" * 32)

    def test_wrapper_never_prompts_for_status_or_unmount(self):
        code = '''import metadata_shim as shim
class Controller:
    def run(self, action, request, credentials, source):
        assert credentials is None and action in ("status", "umount")
        return dict(version=1, operation_id=request["operation_id"], ok=True)
'''
        for action in ("status", "umount"):
            result, getpass = self.execute_wrapper(action, "unused-fixture-secret", code)
            self.assertTrue(result["ok"])
            getpass.assert_not_called()

    def test_wrapper_redacts_malformed_credential_input_and_remote_exceptions(self):
        code = '''class Controller:
    def run(self, *args):
        raise RuntimeError("fixture-private-diagnostic")
'''
        for secret in ("invalid-json-fixture-secret", '{"refresh_token":"fixture-secret"}'):
            result, _ = self.execute_wrapper("mount", secret, code)
            self.assertFalse(result["ok"])
            self.assertEqual(result["error"], "remote_operation_failed")
            self.assertNotIn("fixture", json.dumps(result))


if __name__ == "__main__":
    unittest.main()
