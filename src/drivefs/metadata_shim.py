"""Remote DriveFS metadata subset. Standard library only; never logs secrets.

The containing operation directory must already be private (0700), owned by
this uid and contain no symlink components. The controller owns its cleanup.
This process refreshes credentials on demand; it does not supervise the VM.
"""
import argparse
import http.client
import http.server
import json
import math
import os
import stat
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request

LIMIT = 65536
DRIVE_SCOPE = "https://www.googleapis.com/auth/drive"
REFRESH_STATUS_FILE = "shim-refresh-status.json"
REFRESH_STATUSES = ("ok", "invalid_grant", "refresh_failed")


class ShimError(Exception):
    """Static diagnostics only. Never chain or display upstream exceptions."""


def private_directory(path):
    path = os.path.abspath(os.fspath(path))
    current = os.path.sep
    for part in path.split(os.path.sep)[1:]:
        current = os.path.join(current, part)
        info = os.lstat(current)
        if not stat.S_ISDIR(info.st_mode):
            raise ShimError("unsafe directory")
    info = os.stat(path)
    if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o700:
        raise ShimError("directory must be private")
    return path


def read_config(path):
    parent = private_directory(os.path.dirname(os.path.abspath(path)))
    path = os.path.join(parent, os.path.basename(path))
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, "rb") as source:
        info = os.fstat(source.fileno())
        if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid()
                or stat.S_IMODE(info.st_mode) != 0o600 or info.st_nlink != 1):
            raise ShimError("config must be private")
        raw = source.read(LIMIT + 1)
    if len(raw) > LIMIT:
        raise ShimError("config too large")
    try:
        config = json.loads(raw)
        validate_config(config)
    except Exception:
        raise ShimError("invalid config") from None
    return path, config


def text(value, limit, empty=False):
    return (isinstance(value, str) and (empty or bool(value.strip()))
            and len(value) <= limit and not any(ord(c) < 32 or ord(c) == 127 for c in value))


def validate_config(config):
    if not isinstance(config, dict) or type(config.get("version")) is not int or config["version"] != 1:
        raise ShimError("invalid version")
    for key in ("client_id", "refresh_token", "user_id", "token_uri"):
        if not text(config.get(key), LIMIT):
            raise ShimError("invalid field")
    if not text(config.get("email", ""), 1024, empty=True):
        raise ShimError("invalid email")
    if type(config.get("scope_confirmed", False)) is not bool:
        raise ShimError("invalid scope provenance")
    if config.get("client_secret") is not None and not text(config["client_secret"], LIMIT, empty=True):
        raise ShimError("invalid client")
    scopes = config.get("scopes")
    if (not isinstance(scopes, list) or not 1 <= len(scopes) <= 64
            or any(not text(s, 2048) or any(c.isspace() for c in s) for s in scopes)
            or DRIVE_SCOPE not in scopes):
        raise ShimError("invalid scopes")
    uri = urllib.parse.urlsplit(config["token_uri"])
    # HTTP is exclusively for loopback fixture servers, matching local OAuth.
    if (uri.scheme not in ("http", "https") or not uri.hostname
            or (uri.scheme == "http" and uri.hostname not in ("localhost", "127.0.0.1", "::1"))
            or uri.username is not None or uri.password is not None or uri.query or uri.fragment):
        raise ShimError("invalid token endpoint")
    _ = uri.port


def write_private(path, value):
    parent = private_directory(os.path.dirname(path))
    encoded = json.dumps(value, ensure_ascii=True, allow_nan=False)
    if len(encoded) > LIMIT:
        raise ShimError("state too large")
    fd, temporary = tempfile.mkstemp(prefix=".shim-", dir=parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as target:
            target.write(encoded)
            target.flush()
            os.fsync(target.fileno())
        os.replace(temporary, path)
        directory = os.open(parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, message, headers, newurl):
        return None


class Tokens:
    def __init__(self, config_path, clock=time.monotonic):
        self.path, self.config = read_config(config_path)
        self.clock = clock
        self.lock = threading.Lock()
        self.cached = None
        self.deadline = 0
        self.dirty = False
        self.reported_refresh = None
        self.opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())

    def record_refresh(self, status):
        if status == self.reported_refresh:
            return
        try:
            write_private(os.path.join(os.path.dirname(self.path), REFRESH_STATUS_FILE),
                          {"version": 1, "status": status})
        except (OSError, ShimError):
            # Diagnostic persistence cannot invalidate a usable access token.
            return
        self.reported_refresh = status

    def mint(self):
        with self.lock:
            now = self.clock()
            if self.cached is not None and now < self.deadline - 120:
                return dict(self.cached, expires_in=max(0, int(self.deadline - now)))
            started = now
            form = {k: self.config[k] for k in ("refresh_token", "client_id")}
            form["grant_type"] = "refresh_token"
            if self.config.get("client_secret"):
                form["client_secret"] = self.config["client_secret"]
            request = urllib.request.Request(self.config["token_uri"],
                data=urllib.parse.urlencode(form).encode("ascii"), method="POST",
                headers={"Content-Type": "application/x-www-form-urlencoded"})
            try:
                with self.opener.open(request, timeout=20) as response:
                    raw = response.read(LIMIT + 1)
                if len(raw) > LIMIT:
                    raise ShimError("token response too large")
                result = json.loads(raw)
                token = result.get("access_token")
                lifetime = result.get("expires_in", 3599)
                if (not text(token, LIMIT) or type(lifetime) not in (int, float)
                        or not math.isfinite(lifetime) or not 0 < lifetime <= 2147483647
                        or not text(result.get("token_type", "Bearer"), 128)):
                    raise ShimError("invalid token response")
                scope = result.get("scope", " ".join(self.config["scopes"]))
                if not text(scope, 8192, empty=True):
                    raise ShimError("invalid token scope")
                rotated = result.get("refresh_token")
                if rotated is not None:
                    if not text(rotated, LIMIT):
                        raise ShimError("invalid refresh token")
                    self.config["refresh_token"] = rotated
                    self.dirty = True
                if "scope" in result:
                    self.config["scopes"] = scope.split()
                    self.config["scope_confirmed"] = True
                    self.dirty = True
                # Persist rotation before exposing the access token. On a write
                # failure keep the new refresh token in memory for the next try.
                if self.dirty:
                    write_private(self.path, self.config)
                    self.dirty = False
                if DRIVE_SCOPE not in scope.split():
                    raise ShimError("drive scope unavailable")
                self.deadline = started + lifetime
                remaining = max(0, int(self.deadline - self.clock()))
                if remaining == 0:
                    raise ShimError("token expired")
                self.cached = {"access_token": token, "token_type": result.get("token_type", "Bearer"), "scope": scope}
                self.record_refresh("ok")
                return dict(self.cached, expires_in=remaining)
            except urllib.error.HTTPError as error:
                status = "refresh_failed"
                with error:
                    if error.code == 400:
                        try:
                            raw = error.read(LIMIT + 1)
                            response = json.loads(raw) if len(raw) <= LIMIT else None
                            if isinstance(response, dict) and response.get("error") == "invalid_grant":
                                status = "invalid_grant"
                        except (OSError, ValueError, http.client.HTTPException):
                            pass
                self.cached = None
                self.record_refresh(status)
                raise ShimError("token refresh failed") from None
            except Exception:
                self.cached = None
                self.record_refresh("refresh_failed")
                raise ShimError("token refresh failed") from None

    def identity(self, key):
        with self.lock:
            value = self.config.get(key, "")
            return " ".join(value) if key == "scopes" else value


class MetadataServer(http.server.ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = False
    request_queue_size = 16

    def handle_error(self, request, client_address):
        pass  # The stdlib default prints a traceback containing handler state.


class Handler(http.server.BaseHTTPRequestHandler):
    server_version = "ColabDriveMetadata"
    sys_version = ""

    def setup(self):
        super().setup()
        self.connection.settimeout(2)

    def log_message(self, *args):
        pass

    def send_body(self, code, body=b"", content_type="text/plain; charset=utf-8"):
        self.send_response(code)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        if code == 200:
            self.send_header("Metadata-Flavor", "Google")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        # Match the researched suffix protocol exactly; query is not stripped.
        try:
            if self.path.endswith("/instance/service-accounts/default/token"):
                body = json.dumps(self.server.tokens.mint(), ensure_ascii=True).encode("utf-8")
                self.send_body(200, body, "application/json")
                return
            for suffix, key in (("/guest-attributes/auth/user-id", "user_id"),
                                ("/guest-attributes/auth/scopes", "scopes"), ("/email", "email")):
                if self.path.endswith(suffix):
                    self.send_body(200, self.server.tokens.identity(key).encode("utf-8"))
                    return
            self.send_body(404)
        except Exception:
            self.send_body(500, b"metadata unavailable")

    def send_error(self, code, message=None, explain=None):
        self.send_body(code)  # No reflected request text or HTML error template.


def bind(tokens, port=8012):
    if type(port) is not int or not 0 <= port <= 65535:
        raise ShimError("invalid port")
    for candidate in ([0] if port == 0 else range(port, min(port + 30, 65536))):
        try:
            server = MetadataServer(("127.0.0.1", candidate), Handler)
        except OSError as error:
            import errno
            if error.errno == errno.EADDRINUSE:
                continue
            raise ShimError("metadata bind failed") from None
        server.tokens = tokens
        return server
    raise ShimError("metadata ports occupied")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--config", required=True)
    parser.add_argument("--port", type=int, default=8012)
    args = parser.parse_args()
    try:
        tokens = Tokens(args.config)
        with bind(tokens, args.port) as server:
            # Readiness is token-free and private. The mount controller must
            # still probe /token and verify PID ownership before trusting it.
            ready = os.path.join(os.path.dirname(tokens.path), "shim-ready.json")
            write_private(ready, {"version": 1, "pid": os.getpid(), "port": server.server_port})
            server.serve_forever(poll_interval=0.2)
    except Exception:
        raise SystemExit("DriveFS metadata helper failed") from None


if __name__ == "__main__":
    main()
