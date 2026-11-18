"""Offline contract tests for the exact remote artifact embedded by Rust."""
import concurrent.futures
import contextlib
import http.client
import http.server
import importlib.util
import json
import os
from pathlib import Path
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.parse
from unittest import mock

SOURCE = Path(__file__).resolve().parents[2] / "src/drivefs/metadata_shim.py"
spec = importlib.util.spec_from_file_location("drivefs_metadata", SOURCE)
shim = importlib.util.module_from_spec(spec)
spec.loader.exec_module(shim)
SCOPE = shim.DRIVE_SCOPE
SECRETS = ["fixture-access-secret", "fixture-refresh-secret", "fixture-client-secret"]


class OAuth(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        self.server.get_requests.append(self.path)
        self.send_response(200)
        self.end_headers()
        self.wfile.write(json.dumps({"access_token": "redirected-token", "expires_in": 3600}).encode())

    def do_POST(self):
        body = self.rfile.read(int(self.headers["Content-Length"]))
        self.server.forms.append(urllib.parse.parse_qs(body.decode()))
        code, payload = self.server.responses.pop(0)
        self.send_response(code)
        if code == 302:
            self.send_header("Location", self.server.redirect)
        self.end_headers()
        self.wfile.write(payload if isinstance(payload, bytes) else json.dumps(payload).encode())


class ShimTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.directory = Path(self.temporary.name).resolve()
        self.directory.chmod(0o700)
        self.oauth = http.server.ThreadingHTTPServer(("127.0.0.1", 0), OAuth)
        self.oauth.forms = []
        self.oauth.get_requests = []
        self.oauth.responses = [(200, self.reply())]
        self.oauth.redirect = "http://127.0.0.1:1/secret"
        self.thread = threading.Thread(target=self.oauth.serve_forever, kwargs={"poll_interval": .01}, daemon=True)
        self.thread.start()
        self.addCleanup(self.oauth.server_close)
        self.addCleanup(self.oauth.shutdown)
        self.path = self.directory / "credentials.json"
        self.config = dict(version=1, client_id="fixture-client", client_secret=SECRETS[2],
                           refresh_token=SECRETS[1], token_uri=f"http://127.0.0.1:{self.oauth.server_port}/token",
                           user_id="fixture-gaia", email="中文@example.invalid", scopes=["openid", SCOPE])
        self.save()

    def save(self):
        if self.path.exists():
            self.path.chmod(0o600)
        self.path.write_text(json.dumps(self.config))
        self.path.chmod(0o600)

    def reply(self, **fields):
        return dict(access_token=SECRETS[0], expires_in=3600, token_type="Bearer", **fields)

    @contextlib.contextmanager
    def metadata(self, tokens=None):
        with shim.bind(tokens or shim.Tokens(self.path), 0) as server:
            worker = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": .01}, daemon=True)
            worker.start()
            try:
                yield server
            finally:
                server.shutdown()
                worker.join(2)

    def request(self, server, path, method="GET"):
        conn = http.client.HTTPConnection("127.0.0.1", server.server_port, timeout=3)
        try:
            conn.request(method, path)
            reply = conn.getresponse()
            return reply.status, dict(reply.getheaders()), reply.read()
        finally:
            conn.close()

    def test_metadata_routes_headers_unicode_and_exact_suffix(self):
        with self.metadata() as server:
            for path, expected in [("/computeMetadata/v1/guest-attributes/auth/user-id", b"fixture-gaia"),
                                   ("/computeMetadata/v1/email", self.config["email"].encode()),
                                   ("/guest-attributes/auth/scopes", ("openid " + SCOPE).encode())]:
                status, headers, body = self.request(server, path)
                self.assertEqual((status, body), (200, expected))
                self.assertEqual(headers["Content-Length"], str(len(body)))
                self.assertEqual(headers["Metadata-Flavor"], "Google")
                self.assertEqual(headers["Cache-Control"], "no-store")
            for path in ("/", "/email?x=1", "/token", "/guest-attributes/auth/user-id/other"):
                status, headers, body = self.request(server, path)
                self.assertEqual((status, body, headers["Content-Length"]), (404, b"", "0"))
            self.assertEqual(self.request(server, "/email", "POST")[0], 501)
        self.assertEqual(self.oauth.forms, [])

    def test_concurrent_refresh_single_post_and_correct_form(self):
        tokens = shim.Tokens(self.path)
        with self.metadata(tokens) as server:
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as workers:
                responses = list(workers.map(lambda _: self.request(server, "/instance/service-accounts/default/token"), range(8)))
        self.assertTrue(all(s == 200 and json.loads(b)["access_token"] == SECRETS[0] for s, _, b in responses))
        self.assertEqual(self.oauth.forms, [dict(client_id=["fixture-client"], client_secret=[SECRETS[2]],
                                              refresh_token=[SECRETS[1]], grant_type=["refresh_token"])])

    def test_cache_refresh_boundary_and_no_artificial_lifetime_floor(self):
        now = [0.0]
        tokens = shim.Tokens(self.path, lambda: now[0])
        self.assertEqual(tokens.mint()["expires_in"], 3600)
        now[0] = 3479
        self.assertEqual(tokens.mint()["expires_in"], 121)
        self.oauth.responses = [(200, dict(access_token="short-lived", expires_in=9))]
        now[0] = 3480
        self.assertEqual(tokens.mint()["expires_in"], 9)
        self.assertEqual(len(self.oauth.forms), 2)

    def test_expired_token_refresh_and_invalid_grant_status_are_redacted(self):
        now = [0.0]
        tokens = shim.Tokens(self.path, lambda: now[0])
        status_path = self.directory / shim.REFRESH_STATUS_FILE
        token_path = "/instance/service-accounts/default/token"
        with self.metadata(tokens) as server:
            self.assertEqual(self.request(server, token_path)[0], 200)
            self.assertEqual(json.loads(status_path.read_text()), {"version": 1, "status": "ok"})
            now[0] = 3601
            self.oauth.responses = [(400, {"error": "invalid_grant", "error_description": " ".join(SECRETS)})]
            status, _, body = self.request(server, token_path)
            self.assertEqual((status, body), (500, b"metadata unavailable"))
            self.assertIsNone(tokens.cached)
            self.assertEqual(json.loads(status_path.read_text()), {"version": 1, "status": "invalid_grant"})
            self.assertEqual(stat.S_IMODE(status_path.stat().st_mode), 0o600)
            self.oauth.responses = [(200, {"access_token": "fixture-replacement", "expires_in": 3600})]
            status, _, body = self.request(server, token_path)
            self.assertEqual((status, json.loads(body)["access_token"]), (200, "fixture-replacement"))
            self.assertEqual(json.loads(status_path.read_text()), {"version": 1, "status": "ok"})
        self.assertEqual(len(self.oauth.forms), 3)

    def test_diagnostic_write_failure_keeps_successful_token(self):
        tokens = shim.Tokens(self.path)
        with mock.patch.object(shim, "write_private", side_effect=OSError("private diagnostic failure")):
            self.assertEqual(tokens.mint()["access_token"], SECRETS[0])
        self.assertFalse((self.directory / shim.REFRESH_STATUS_FILE).exists())

    def test_rotation_persisted_privately_before_token_return(self):
        self.oauth.responses = [(200, self.reply(refresh_token="rotated", scope=SCOPE))]
        tokens = shim.Tokens(self.path)
        tokens.mint()
        persisted = json.loads(self.path.read_text())
        self.assertEqual(persisted["refresh_token"], "rotated")
        self.assertEqual(persisted["scopes"], [SCOPE])
        self.assertTrue(persisted["scope_confirmed"])
        self.assertEqual(stat.S_IMODE(self.path.stat().st_mode), 0o600)
        self.assertNotIn(SECRETS[0], self.path.read_text())
        self.assertEqual(list(self.directory.glob(".shim-*")), [])

    def test_rotation_write_failure_retains_and_retries_new_refresh_token(self):
        self.oauth.responses = [(200, self.reply(refresh_token="rotated")), (200, self.reply())]
        tokens = shim.Tokens(self.path)
        with mock.patch.object(shim, "write_private", side_effect=OSError("private secret")):
            with self.assertRaisesRegex(shim.ShimError, "^token refresh failed$"):
                tokens.mint()
        self.assertIsNone(tokens.cached)
        tokens.mint()
        self.assertEqual(self.oauth.forms[1]["refresh_token"], ["rotated"])
        self.assertEqual(json.loads(self.path.read_text())["refresh_token"], "rotated")

    def test_upstream_error_and_malformed_responses_are_redacted(self):
        failures = [(400, " ".join(SECRETS).encode()), (200, b"bad json"),
                    (200, b"x" * (shim.LIMIT + 1)), (200, {"access_token": "x", "expires_in": True}),
                    (200, {"access_token": "x", "expires_in": -1}),
                    (200, {"access_token": "x", "expires_in": float("inf")}),
                    (200, {"access_token": ""}), (200, self.reply(scope=""))]
        with self.metadata() as server:
            for reply in failures:
                self.oauth.responses = [reply]
                status, headers, body = self.request(server, "/instance/service-accounts/default/token")
                self.assertEqual((status, body), (500, b"metadata unavailable"))
                self.assertEqual(headers["Content-Length"], str(len(body)))
                self.assertEqual(json.loads((self.directory / shim.REFRESH_STATUS_FILE).read_text()),
                                 {"version": 1, "status": "refresh_failed"})
                for secret in SECRETS:
                    self.assertNotIn(secret.encode(), body)

    def test_no_redirect_or_proxy_forwarding(self):
        self.oauth.responses = [(302, b"redirect secret")]
        self.oauth.redirect = f"http://127.0.0.1:{self.oauth.server_port}/stolen"
        with mock.patch.dict(os.environ, {"http_proxy": "http://127.0.0.1:1", "no_proxy": ""}):
            tokens = shim.Tokens(self.path)
            with self.assertRaises(shim.ShimError):
                tokens.mint()
        self.assertEqual(len(self.oauth.forms), 1)
        self.assertEqual(self.oauth.get_requests, [])

    def test_private_config_rejects_symlinks_permissions_and_hardlinks(self):
        for mode in (0o644, 0o660, 0o400):
            self.path.chmod(mode)
            with self.assertRaises(shim.ShimError):
                shim.Tokens(self.path)
        self.save()
        link = self.directory / "alias"
        link.symlink_to(self.path)
        with self.assertRaises(OSError):
            shim.Tokens(link)
        link.unlink()
        os.link(self.path, link)
        with self.assertRaises(shim.ShimError):
            shim.Tokens(self.path)
        link.unlink()
        self.directory.chmod(0o755)
        with self.assertRaises(shim.ShimError):
            shim.Tokens(self.path)
        self.directory.chmod(0o700)
        outer = self.directory / "symlink-dir"
        outer.symlink_to(self.directory, target_is_directory=True)
        with self.assertRaises(shim.ShimError):
            shim.Tokens(outer / "credentials.json")

    def test_config_schema_scope_and_endpoint_validation(self):
        for update in (dict(version=True), dict(version=2), dict(scopes=["drive.file"]),
                       dict(user_id=""), dict(token_uri="http://example.invalid/token"),
                       dict(token_uri="https://a.invalid/token?secret=value"),
                       dict(token_uri="https://user:secret@a.invalid/token"),
                       dict(token_uri="https://a.invalid/token#secret")):
            self.save()
            candidate = dict(self.config, **update)
            self.path.write_text(json.dumps(candidate))
            with self.assertRaisesRegex(shim.ShimError, "invalid config"):
                shim.Tokens(self.path)

    def test_port_collision_uses_next_port_and_exhaustion_is_bounded(self):
        with socket.socket() as occupied:
            occupied.bind(("127.0.0.1", 0))
            occupied.listen()
            port = occupied.getsockname()[1]
            with shim.bind(shim.Tokens(self.path), port) as server:
                self.assertGreater(server.server_port, port)
                self.assertLess(server.server_port, min(port + 30, 65536))
        import errno
        with mock.patch.object(shim, "MetadataServer", side_effect=OSError(errno.EADDRINUSE, "occupied")) as constructor:
            with self.assertRaisesRegex(shim.ShimError, "ports occupied"):
                shim.bind(shim.Tokens(self.path), 8012)
            self.assertEqual(constructor.call_count, 30)

    def test_real_process_readiness_and_failure_output(self):
        child = subprocess.Popen([sys.executable, str(SOURCE), "--config", str(self.path), "--port", "0"],
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            ready = self.directory / "shim-ready.json"
            deadline = time.monotonic() + 5
            while not ready.exists() and child.poll() is None and time.monotonic() < deadline:
                time.sleep(.01)
            self.assertTrue(ready.exists())
            info = json.loads(ready.read_text())
            self.assertEqual(info["pid"], child.pid)
            self.assertEqual(stat.S_IMODE(ready.stat().st_mode), 0o600)
            conn = http.client.HTTPConnection("127.0.0.1", info["port"], timeout=2)
            conn.request("GET", "/email")
            self.assertEqual(conn.getresponse().read(), self.config["email"].encode())
            conn.close()
        finally:
            child.terminate()
            stdout, stderr = child.communicate(timeout=5)
        self.assertEqual((stdout, stderr), (b"", b""))
        self.path.write_text(" ".join(SECRETS))
        result = subprocess.run([sys.executable, str(SOURCE), "--config", str(self.path)], capture_output=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stderr, b"DriveFS metadata helper failed\n")
        self.assertEqual(result.stdout, b"")


if __name__ == "__main__":
    unittest.main()
