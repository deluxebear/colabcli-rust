"""Remote mount lifecycle. No shell, global process kills, or recursive mount deletion.

The caller supplies credentials separately from executable source. All returned
errors are fixed codes; DriveFS logs and credential contents are never returned.
"""
import contextlib
import errno
import fcntl
import hashlib
import http.client
import hmac
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import stat
import subprocess
import sys
import time
import unicodedata

import metadata_shim as shim

ROOT = "/content/.colab-cli-drivefs"
DRIVE = "/opt/google/drive/drive"
ROOTS = "/opt/google/drive/roots.pem"


def validate_extra_flags(flags):
    reserved = {"features", "preferences", "user_id", "user_email", "client_id", "client_secret",
                "mountpoint", "mount_point_path", "single_process", "push_changes_and_quit", "timeout_sec",
                "home", "log_dir", "log_file", "log_path", "config", "help", "version"}
    if not isinstance(flags, list) or len(flags) > 32:
        raise ControlError("invalid_options")
    for flag in flags:
        if not isinstance(flag, str):
            raise ControlError("invalid_options")
        name = flag[2:].split("=", 1)[0] if flag.startswith("--") else ""
        if (not re.fullmatch(r"[a-z][a-z0-9_]*", name) or len(flag.encode()) > 4096
                or any(unicodedata.category(c) == "Cc" for c in flag) or name in reserved
                or any(word in name for word in ("auth", "token", "secret", "password"))):
            raise ControlError("invalid_options")
    return flags


class ControlError(Exception):
    pass


class NotStarted(ControlError):
    """The child definitely did not exec; unlike an unknown launch outcome."""


def identifier(value):
    if not isinstance(value, str) or re.fullmatch(r"[0-9a-f]{32}", value) is None:
        raise ControlError("invalid_operation")
    return value


def mount_path(value, root):
    if (not isinstance(value, str) or not value.startswith("/") or value == "/"
            or value.endswith("/") or "//" in value
            or len(value.encode("utf-8")) > 4096
            or any(c in ",:" or unicodedata.category(c) == "Cc" for c in value)
            or any(part in (".", "..") for part in value.split("/"))):
        raise ControlError("invalid_mountpoint")
    path = os.path.normpath(value)
    if path == root or path.startswith(root + "/") or root.startswith(path + "/"):
        raise ControlError("mountpoint_overlaps_state")
    return path


def no_symlinks(path, missing_leaf=False):
    current = "/"
    parts = path.split("/")[1:]
    for index, part in enumerate(parts):
        current = os.path.join(current, part)
        try:
            info = os.lstat(current)
        except FileNotFoundError:
            if missing_leaf and index == len(parts) - 1:
                return
            raise ControlError("parent_missing") from None
        if not stat.S_ISDIR(info.st_mode):
            raise ControlError("unsafe_mountpoint")


def directory(path):
    try:
        os.mkdir(path, 0o700)
    except FileExistsError:
        pass
    shim.private_directory(path)


def private_bytes(path, contents):
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "wb") as output:
        output.write(contents)
        output.flush()
        os.fsync(output.fileno())


def private_payload(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as source:
        info = os.fstat(source.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or stat.S_IMODE(info.st_mode) != 0o600 or info.st_nlink != 1):
            raise ControlError("unsafe_state")
        raw = source.read(shim.LIMIT + 1)
        if len(raw) > shim.LIMIT:
            raise ControlError("invalid_state")
        return raw


def private_json(path):
    try:
        return json.loads(private_payload(path))
    except (ValueError, UnicodeError):
        raise ControlError("invalid_state") from None


def unescape_mount(value):
    return re.sub(r"\\([0-7]{3})", lambda match: chr(int(match[1], 8)), value)


def parse_mounts(contents):
    result = []
    try:
        for line in contents.splitlines():
            before, after = line.split(" - ", 1)
            fields, filesystem = before.split(), after.split()
            if len(fields) < 6 or len(filesystem) < 3:
                raise ValueError()
            result.append(dict(id=int(fields[0]), path=unescape_mount(fields[4]),
                               filesystem=filesystem[0], source=unescape_mount(filesystem[1])))
    except Exception:
        raise ControlError("mount_table_unavailable") from None
    if not result:
        raise ControlError("mount_table_unavailable")
    return result


class System:
    def mounts(self):
        try:
            return parse_mounts(Path("/proc/self/mountinfo").read_text())
        except OSError:
            raise ControlError("mount_table_unavailable") from None

    def process(self, pid, operation):
        if type(pid) is not int or pid <= 0:
            raise ControlError("invalid_process")
        path = Path("/proc") / str(pid)
        try:
            # comm can itself contain spaces and ')'. Fields after the final
            # closing parenthesis start at field 3; starttime is field 22.
            fields = (path / "stat").read_text().rsplit(")", 1)[1].split()
            if fields[0] in ("Z", "X"):
                return None  # Exited processes have no reliable environ/exe.
            environment = (path / "environ").read_bytes().split(b"\0")
            marker = ("COLAB_CLI_DRIVEFS_OPERATION=" + operation).encode()
            if marker not in environment:
                raise ControlError("process_ownership_mismatch")
            executable = os.readlink(path / "exe")
            command_hash = hashlib.sha256((path / "cmdline").read_bytes()).hexdigest()
            return dict(pid=pid, start_ticks=fields[19], executable=executable, command_hash=command_hash)
        except FileNotFoundError:
            return None
        except (OSError, IndexError):
            raise ControlError("process_identity_unavailable") from None

    def alive(self, record, operation):
        current = self.process(record["pid"], operation)
        if current is None:
            return False
        if current != record:
            raise ControlError("process_ownership_mismatch")
        return True

    def discover(self, operation, expected):
        matches = []
        for name in os.listdir("/proc"):
            if not name.isdecimal():
                continue
            try:
                record = self.process(int(name), operation)
            except ControlError:
                continue
            if record is not None and all(record.get(key) == value for key, value in expected.items()):
                matches.append(record)
        if len(matches) > 1:
            raise ControlError("ambiguous_owned_process")
        return matches[0] if matches else None

    def launch(self, argv, environment, log_path, operation):
        try:
            fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        except OSError:
            raise NotStarted("process_not_started") from None
        with os.fdopen(fd, "wb") as output:
            try:
                child = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=output,
                                         stderr=subprocess.STDOUT, env=environment,
                                         start_new_session=True, close_fds=True)
            except OSError:
                raise NotStarted("process_not_started") from None
        # exec may still be in progress. Popen has waited for its exec error
        # pipe, so a successful return is ready for /proc identity capture.
        expected = dict(executable=os.path.realpath(argv[0]),
            command_hash=hashlib.sha256(b"\0".join(os.fsencode(arg) for arg in argv) + b"\0").hexdigest())
        deadline = time.monotonic() + 2
        while True:
            if child.poll() is not None:
                descendant = self.discover(operation, expected)
                if descendant is not None:
                    return descendant
                raise ControlError("process_exited")
            try:
                record = self.process(child.pid, operation)
                if record is not None and all(record.get(key) == value for key, value in expected.items()):
                    return record
            except ControlError:
                pass  # Successful exec can transiently hide /proc identity.
            if time.monotonic() >= deadline:
                raise ControlError("process_identity_unavailable")
            time.sleep(.02)  # Observe the same child; never spawn another one.

    def stop(self, record, operation):
        if not self.alive(record, operation):
            return
        if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
            raise ControlError("safe_process_signal_unavailable")
        try:
            descriptor = os.pidfd_open(record["pid"])
        except ProcessLookupError:
            return
        try:
            # Verify after obtaining the stable handle, not before signalling a
            # potentially reused numeric PID. Signals never target a group.
            if not self.alive(record, operation):
                return
            try:
                signal.pidfd_send_signal(descriptor, signal.SIGTERM)
                if select.select([descriptor], [], [], 2)[0]:
                    return
                signal.pidfd_send_signal(descriptor, signal.SIGKILL)
                if not select.select([descriptor], [], [], 2)[0]:
                    raise ControlError("process_stop_timeout")
            except ProcessLookupError:
                return
        finally:
            os.close(descriptor)

    def flush(self, record, operation):
        # Drive's shutdown command addresses the per-uid IPC listener. Refuse
        # to send it unless this exact owned process holds the only listener.
        if not record or not self.alive(record, operation):
            raise ControlError("drive_flush_unavailable")
        try:
            socket_path = "/tmp/drivefs_ipc." + str(os.getuid())
            listeners = [row.split() for row in Path("/proc/net/unix").read_text().splitlines()[1:]
                         if len(row.split()) == 8 and row.split()[3] == "00010000"
                         and row.split()[7] == socket_path]
            if len(listeners) != 1:
                raise ControlError("drive_ipc_ownership_unconfirmed")
            target = "socket:[" + listeners[0][6] + "]"
            if not any(os.readlink(fd) == target for fd in (Path("/proc") / str(record["pid"]) / "fd").iterdir()):
                raise ControlError("drive_ipc_ownership_unconfirmed")
            if not self.alive(record, operation):
                raise ControlError("drive_flush_unavailable")
            result = subprocess.run([DRIVE, "--push_changes_and_quit", "--single_process", "--timeout_sec=30"],
                                    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                    stderr=subprocess.DEVNULL, timeout=35, check=False)
        except (OSError, subprocess.TimeoutExpired):
            raise ControlError("drive_flush_failed") from None
        if result.returncode:
            raise ControlError("drive_flush_failed")

    def unmount(self, path, expected):
        for command in (["fusermount", "-u", "--", path], ["fusermount3", "-u", "--", path],
                        ["umount", "--", path]):
            current = [item for item in self.mounts() if item["path"] == path]
            if not current:
                return
            if current != [expected]:
                raise ControlError("mount_ownership_mismatch")
            try:
                subprocess.run(command, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL, timeout=10, check=False)
            except (OSError, subprocess.TimeoutExpired):
                pass
            if not any(item["path"] == path for item in self.mounts()):
                return
        raise ControlError("unmount_failed")

    def preflight(self):
        if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
            raise ControlError("safe_process_signal_unavailable")
        try:
            os.close(os.pidfd_open(os.getpid()))
        except OSError:
            raise ControlError("safe_process_signal_unavailable") from None
        if not os.path.isfile(DRIVE) or not os.access(DRIVE, os.X_OK) or not os.path.isfile(ROOTS):
            raise ControlError("drivefs_unavailable")
        if not os.path.exists("/dev/fuse"):
            raise ControlError("fuse_unavailable")

    def probe(self, port):
        connection = http.client.HTTPConnection("127.0.0.1", port, timeout=20)
        try:
            connection.request("GET", "/computeMetadata/v1/instance/service-accounts/default/token")
            response = connection.getresponse()
            raw = response.read(shim.LIMIT + 1)
            if (response.status != 200 or response.getheader("Metadata-Flavor") != "Google"
                    or len(raw) > shim.LIMIT):
                raise ControlError("metadata_probe_failed")
            value = json.loads(raw)
            if (not shim.text(value.get("access_token"), shim.LIMIT)
                    or type(value.get("expires_in")) is not int or value["expires_in"] <= 0
                    or not isinstance(value.get("scope"), str) or shim.DRIVE_SCOPE not in value["scope"].split()):
                raise ControlError("metadata_probe_failed")
        except Exception:
            raise ControlError("metadata_probe_failed") from None
        finally:
            connection.close()


class Controller:
    def __init__(self, root=ROOT, system=None, clock=time.monotonic, sleep=time.sleep):
        self.root = os.path.abspath(root)
        self.system = system or System()
        self.clock, self.sleep = clock, sleep

    def table(self, path):
        matches = [entry for entry in self.system.mounts() if entry["path"] == path]
        if len(matches) > 1:
            raise ControlError("stacked_mounts")
        return matches[0] if matches else None

    def registry(self, path):
        return os.path.join(self.root, "mounts", hashlib.sha256(path.encode()).hexdigest() + ".json")

    def operation_dir(self, operation):
        return os.path.join(self.root, "operations", identifier(operation))

    def save(self, state, stage):
        state["stage"] = stage
        shim.write_private(os.path.join(self.operation_dir(state["operation_id"]), "state.json"), state)

    def load(self, path):
        try:
            shim.private_directory(self.root)
        except FileNotFoundError:
            return None
        shim.private_directory(os.path.join(self.root, "mounts"))
        try:
            pointer = private_json(self.registry(path))
        except FileNotFoundError:
            return None
        if (not isinstance(pointer, dict) or type(pointer.get("version")) is not int
                or pointer.get("version") != 1):
            raise ControlError("invalid_state")
        operation = identifier(pointer.get("operation_id"))
        location = self.operation_dir(operation)
        shim.private_directory(os.path.join(self.root, "operations"))
        shim.private_directory(location)
        state = private_json(os.path.join(location, "state.json"))
        if (not isinstance(state, dict) or type(state.get("version")) is not int or state.get("version") != 1
                or state.get("operation_id") != operation or state.get("mountpoint") != path
                or state.get("stage") not in ("prepared", "shim_starting", "shim_ready", "drive_starting",
                                               "mounting", "mounted", "unmounting", "credentials_pending", "finalizing", "finished", "unknown")):
            raise ControlError("invalid_state")
        return state

    @contextlib.contextmanager
    def lock(self, path):
        if any(entry["filesystem"].startswith("fuse")
               and (self.root == entry["path"] or self.root.startswith(entry["path"].rstrip("/") + "/"))
               for entry in self.system.mounts()):
            raise ControlError("state_on_fuse")
        directory(self.root)
        for leaf in ("mounts", "operations"):
            directory(os.path.join(self.root, leaf))
        lock_path = self.registry(path) + ".lock"
        descriptor = os.open(lock_path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
        try:
            info = os.fstat(descriptor)
            if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                    or stat.S_IMODE(info.st_mode) != 0o600 or info.st_nlink != 1):
                raise ControlError("unsafe_lock")
            try:
                fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise ControlError("mount_busy") from None
            yield
        finally:
            os.close(descriptor)

    def recover_mount_snapshot(self, state, current):
        # A process may mount after launch's /proc capture failed. Only an
        # explicitly recorded launch, rediscovered with exact executable and
        # command hash plus operation environment, can bridge that narrow gap.
        if (current is None or state.get("mount") is not None or state.get("stage") != "unknown"
                or state.get("failure_stage") != "drive_starting"
                or current.get("filesystem") != "fuse.drive"
                or not state.get("drive_launch") or not state.get("drive")):
            return state
        expected, record = state["drive_launch"], state["drive"]
        if (expected.get("executable") != os.path.realpath(DRIVE)
                or any(record.get(key) != expected.get(key) for key in ("executable", "command_hash"))
                or not self.system.alive(record, state["operation_id"])):
            return state
        state = dict(state)
        state["mount"] = current
        return state

    def inspect(self, operation, path, *, include_refresh=False):
        state = self.load(path)
        mounted = self.table(path)
        result = dict(version=1, operation_id=operation, mountpoint=path,
                      mounted=mounted is not None, managed=state is not None,
                      ok=True, stage="unmanaged", error=None)
        if state is None:
            return result
        state = self.recover_mount_snapshot(self.reconcile(state), mounted)
        result.update(owner_operation_id=state["operation_id"], stage=state["stage"], mode=state["mode"],
                      readonly=state["readonly"], identity=state["identity"])
        for role in ("helper", "drive"):
            record = state.get(role)
            result[role + "_alive"] = bool(state["stage"] not in ("credentials_pending", "finalizing", "finished") and record and self.system.alive(record, state["operation_id"]))
            result[role + "_pid"] = record["pid"] if record else None
            result[role + "_unknown"] = bool(state.get(role + "_launch") and record is None and state["stage"] not in ("credentials_pending", "finalizing", "finished"))
        if include_refresh and state["mode"] == "shim" and result["helper_alive"]:
            status_path = os.path.join(self.operation_dir(state["operation_id"]), shim.REFRESH_STATUS_FILE)
            try:
                refresh = private_json(status_path)
            except (OSError, ControlError):
                refresh = None
            if (isinstance(refresh, dict) and type(refresh.get("version")) is int and refresh["version"] == 1
                    and refresh.get("status") in shim.REFRESH_STATUSES):
                result["refresh_status"] = refresh["status"]
        if mounted is not None and (state["stage"] in ("credentials_pending", "finalizing", "finished") or mounted != state.get("mount")):
            raise ControlError("mount_ownership_mismatch")
        return result

    def reconcile(self, state):
        state = dict(state)
        if state["stage"] not in ("credentials_pending", "finalizing", "finished"):
            for role in ("helper", "drive"):
                expected = state.get(role + "_launch")
                record = state.get(role)
                if expected and (not record or not self.system.alive(record, state["operation_id"])):
                    descendant = self.system.discover(state["operation_id"], expected)
                    if descendant is not None or record is None:
                        state[role] = descendant
        return state

    def launch(self, state, role, argv, environment, stage):
        state[role + "_launch"] = dict(executable=os.path.realpath(argv[0]),
            command_hash=hashlib.sha256(b"\0".join(os.fsencode(arg) for arg in argv) + b"\0").hexdigest())
        self.save(state, stage)
        try:
            state[role] = self.system.launch(argv, environment,
                os.path.join(self.operation_dir(state["operation_id"]), role + ".log"), state["operation_id"])
        except NotStarted:
            state.pop(role + "_launch")
            self.save(state, stage)
            raise
        self.save(state, stage)

    def environment(self, state):
        return {"HOME": os.path.join(self.operation_dir(state["operation_id"]), "home"),
                "PATH": "/usr/local/bin:/usr/local/sbin:/usr/bin:/usr/sbin:/bin:/sbin",
                "FUSE_DEV_NAME": "/dev/fuse", "COLAB_CLI_DRIVEFS_OPERATION": state["operation_id"]}

    def mount(self, operation, path, request, credentials, source):
        if not isinstance(credentials, dict):
            raise ControlError("credentials_required")
        shim.validate_config(credentials)
        mode = request.get("mode", "shim")
        readonly, force = request.get("readonly", False), request.get("force", False)
        timeout = request.get("timeout", 30)
        port = request.get("port", 8012)
        extra_flags = validate_extra_flags(request.get("extra_flags", []))
        if (mode not in ("shim", "rt") or type(readonly) is not bool or type(force) is not bool
                or type(timeout) is not int or not 1 <= timeout <= 3600 or type(port) is not int
                or not 0 <= port <= 65535):
            raise ControlError("invalid_options")
        if mode == "rt" and credentials["client_id"] != "947318989803-k88lapdik9bledfml8rr69ic6d3rdv57.apps.googleusercontent.com":
            raise ControlError("rt_client_mismatch")
        identity = hashlib.sha256(json.dumps([credentials["client_id"], credentials["user_id"]]).encode()).hexdigest()
        existing = self.load(path)
        current = self.table(path)
        if existing is not None and existing["stage"] != "finished":
            if (not force and current is not None and existing.get("mount") == current
                    and existing["stage"] == "mounted" and existing.get("identity") == identity
                    and existing.get("readonly") == readonly and existing.get("mode") == mode
                    and existing.get("extra_flags", []) == extra_flags
                    and existing.get("drive") and self.system.alive(existing["drive"], existing["operation_id"])
                    and (mode != "shim" or (existing.get("helper") and self.system.alive(existing["helper"], existing["operation_id"])))):
                return self.inspect(operation, path)
            if not force:
                raise ControlError("operation_pending")
            self.unmount(existing, path, purge=False)
            if self.load(path)["stage"] != "finished":
                raise ControlError("credentials_recovery_required")
        elif current is not None:
            raise ControlError("foreign_mount")
        self.system.preflight()
        no_symlinks(path, missing_leaf=True)
        if any(entry["filesystem"].startswith("fuse") and path.startswith(entry["path"].rstrip("/") + "/")
               for entry in self.system.mounts()):
            raise ControlError("nested_fuse_mount")
        if os.path.isdir(path) and os.listdir(path):
            raise ControlError("mountpoint_not_empty")
        location = self.operation_dir(operation)
        # Never reuse an operation ID, including after a previous attempt failed.
        try:
            os.mkdir(location, 0o700)
        except FileExistsError:
            raise ControlError("operation_already_exists") from None
        state = dict(version=1, operation_id=operation, mountpoint=path, identity=identity,
                     mode=mode, readonly=readonly, extra_flags=extra_flags, helper=None, drive=None, mount=None,
                     created_mountpoint=False, mountpoint_inode=None)
        self.save(state, "prepared")
        shim.write_private(self.registry(path), dict(version=1, operation_id=operation))
        try:
            if not os.path.exists(path):
                os.mkdir(path, 0o700)
                state["created_mountpoint"] = True
            info = os.lstat(path)
            state["mountpoint_inode"] = [info.st_dev, info.st_ino]
            directory(os.path.join(location, "home"))
            self.save(state, "prepared")
            environment = self.environment(state)
            if mode == "shim":
                if not isinstance(source, str) or not source:
                    raise ControlError("shim_source_required")
                shim.write_private(os.path.join(location, "credentials.json"), credentials)
                script = os.path.join(location, "metadata_shim.py")
                private_bytes(script, source.encode())
                self.launch(state, "helper", [sys.executable, script, "--config", os.path.join(location, "credentials.json"), "--port", str(port)],
                    environment, "shim_starting")
                ready = os.path.join(location, "shim-ready.json")
                deadline = self.clock() + timeout
                while not os.path.exists(ready):
                    if not self.system.alive(state["helper"], operation):
                        raise ControlError("helper_exited")
                    if self.clock() >= deadline:
                        raise ControlError("helper_timeout")
                    self.sleep(.1)
                info = private_json(ready)
                if (type(info.get("version")) is not int or info.get("version") != 1 or info.get("pid") != state["helper"]["pid"]
                        or type(info.get("port")) is not int or not 1 <= info["port"] <= 65535):
                    raise ControlError("invalid_helper_readiness")
                self.system.probe(info["port"])
                auth = ["--metadata_server_auth_uri=127.0.0.1:%d/computeMetadata/v1" % info["port"]]
                self.save(state, "shim_ready")
            else:
                auth = ["--refresh_token=" + credentials["refresh_token"], "--token_uri=" + credentials["token_uri"],
                        "--user_id=" + credentials["user_id"], "--user_email=" + credentials.get("email", "")]
            features = ("crash_throttle_percentage:100,fuse_max_background:1000,max_read_qps:1000,"
                        "max_write_qps:1000,max_operation_batch_size:15,max_parallel_push_task_instances:10,"
                        "opendir_timeout_ms:%d,virtual_folders_omit_spaces:true,read_only_mode:%s"
                        % (timeout * 1000, "true" if readonly else "false"))
            preferences = "trusted_root_certs_file_path:%s,feature_flag_restart_seconds:129600,mount_point_path:%s" % (ROOTS, path)
            self.launch(state, "drive", [DRIVE, "--features=" + features, "--single_process"] + auth + ["--preferences=" + preferences] + extra_flags + [path],
                environment, "drive_starting")
            self.save(state, "mounting")
            deadline = self.clock() + timeout
            while True:
                mounted = self.table(path)
                if mounted is not None:
                    if not mounted["filesystem"].startswith("fuse"):
                        raise ControlError("unexpected_filesystem")
                    state["mount"] = mounted
                    self.save(state, "mounted")
                    return self.inspect(operation, path)
                before = state.get("drive")
                state = self.reconcile(state)
                if state.get("drive") != before:
                    self.save(state, "mounting")
                if not state.get("drive") or not self.system.alive(state["drive"], operation):
                    raise ControlError("drive_exited")
                if self.clock() >= deadline:
                    raise ControlError("mount_timeout")
                self.sleep(.1)
        except Exception:
            # Any error after reserving the operation is recorded. Do not infer
            # rollback or replay: a process or FUSE mount may already exist.
            state["failure_stage"] = state["stage"]
            self.save(state, "unknown")
            raise

    def unmount(self, state, path, purge=False):
        if type(purge) is not bool:
            raise ControlError("invalid_options")
        current = self.table(path)
        if state["stage"] == "finished":
            if current is not None:
                raise ControlError("mount_ownership_mismatch")
            if purge:
                self.purge(state)
            return
        if state["stage"] in ("credentials_pending", "finalizing"):
            if current is not None:
                raise ControlError("mount_ownership_mismatch")
            if purge:
                state["purge_state"] = True
                self.save(state, state["stage"])
            return
        state = self.recover_mount_snapshot(self.reconcile(state), current)
        if current is not None and current != state.get("mount"):
            raise ControlError("mount_ownership_mismatch")
        operation = state["operation_id"]
        # Unknown launch results cannot be interpreted as 'no process'.
        for role in ("helper", "drive"):
            if state.get(role + "_launch") and not state.get(role):
                raise ControlError("process_result_unknown")
        for role in ("drive", "helper"):
            if state.get(role):
                self.system.alive(state[role], operation)
        self.save(state, "unmounting")
        if current is not None:
            # FUSE unlink/write success can precede the server upload. Do not
            # kill Drive or remove its credentials until its flush succeeds.
            self.system.flush(state.get("drive"), operation)
            after_flush = self.table(path)
            if after_flush is not None:
                if after_flush != current:
                    raise ControlError("mount_ownership_mismatch")
                self.system.unmount(path, current)
        if self.table(path) is not None:
            raise ControlError("unmount_failed")
        # Only after actual unmount may owned processes or credentials go away.
        for role in ("drive", "helper"):
            if state.get(role):
                self.system.stop(state[role], operation)
        if self.table(path) is not None:
            raise ControlError("mount_reappeared")
        location = self.operation_dir(operation)
        state["purge_state"] = purge
        credentials = os.path.join(location, "credentials.json")
        if os.path.exists(credentials):
            # A stopped helper cannot rotate again. Keep its final refresh token
            # until the client confirms durable local adoption of these bytes.
            self.save(state, "credentials_pending")
            return
        if state["mode"] == "shim" and state.get("helper_launch"):
            raise ControlError("credentials_missing")
        self.finish_cleanup(state, path)

    def finalize(self, state, path, request):
        if type(request.get("purge_state", False)) is not bool:
            raise ControlError("invalid_options")
        if request.get("owner_operation_id") != state["operation_id"]:
            raise ControlError("credentials_owner_mismatch")
        receipt = request.get("credentials_receipt")
        if not isinstance(receipt, str) or re.fullmatch(r"[0-9a-f]{64}", receipt) is None:
            raise ControlError("invalid_credentials_receipt")
        if state["stage"] == "finished":
            if state.get("credentials_receipt") != receipt:
                raise ControlError("credentials_receipt_mismatch")
            if self.table(path) is not None:
                raise ControlError("mount_ownership_mismatch")
            return
        if state["stage"] not in ("credentials_pending", "finalizing"):
            raise ControlError("credentials_not_frozen")
        if self.table(path) is not None:
            raise ControlError("mount_reappeared")
        credentials = os.path.join(self.operation_dir(state["operation_id"]), "credentials.json")
        if os.path.exists(credentials):
            expected = hashlib.sha256(private_payload(credentials)).hexdigest()
        elif state["stage"] == "finalizing":
            expected = state.get("credentials_receipt", "")
        else:
            raise ControlError("credentials_missing")
        if not hmac.compare_digest(receipt, expected):
            raise ControlError("credentials_receipt_mismatch")
        state["credentials_receipt"] = receipt
        state["purge_state"] = state.get("purge_state", False) or request.get("purge_state", False)
        self.save(state, "finalizing")
        self.finish_cleanup(state, path)

    def finish_cleanup(self, state, path):
        location = self.operation_dir(state["operation_id"])
        for leaf in ("credentials.json", "helper.log", "drive.log", "metadata_shim.py", "shim-ready.json",
                     shim.REFRESH_STATUS_FILE):
            try:
                os.unlink(os.path.join(location, leaf))
            except FileNotFoundError:
                pass
        if state.get("purge_state", False):
            self.purge(state)
        if state["created_mountpoint"]:
            no_symlinks(path, missing_leaf=True)
            try:
                info = os.lstat(path)
                if [info.st_dev, info.st_ino] != state["mountpoint_inode"]:
                    raise ControlError("mountpoint_replaced")
                os.rmdir(path)  # Never recursive, even when unmount succeeded.
            except FileNotFoundError:
                pass
            except OSError as error:
                if error.errno not in (errno.ENOTEMPTY, errno.EEXIST):
                    raise
        self.save(state, "finished")

    def purge(self, state):
        location = self.operation_dir(state["operation_id"])
        home = os.path.join(location, "home")
        if any(item["path"] == location or item["path"].startswith(location + "/") for item in self.system.mounts()):
            raise ControlError("state_contains_mount")
        no_symlinks(home, missing_leaf=True)
        try:
            shutil.rmtree(home)
        except FileNotFoundError:
            pass

    def run(self, action, request, credentials=None, shim_source=None):
        operation, path = None, None
        try:
            operation = identifier(request.get("operation_id"))
            path = mount_path(request.get("mountpoint"), self.root)
            if action == "status":
                return self.inspect(operation, path, include_refresh=True)  # No writes, locks or process changes.
            if action not in ("mount", "umount", "finalize"):
                raise ControlError("invalid_action")
            with self.lock(path):
                if action == "mount":
                    return self.mount(operation, path, request, credentials, shim_source)
                state = self.load(path)
                if action == "umount" and state is not None and request.get("owner_operation_id") is not None and request["owner_operation_id"] != state["operation_id"]:
                    raise ControlError("mount_ownership_mismatch")
                if state is None:
                    if action == "finalize":
                        raise ControlError("credentials_not_frozen")
                    if self.table(path) is not None:
                        raise ControlError("foreign_mount")
                elif action == "finalize":
                    self.finalize(state, path, request)
                else:
                    self.unmount(state, path, request.get("purge_state", False))
                return self.inspect(operation, path)
        except ControlError as error:
            code = str(error)
        except Exception:
            code = "remote_operation_failed"
        return dict(version=1, operation_id=operation, mountpoint=path, ok=False,
                    stage="error", error=code, mounted=None, managed=None)
