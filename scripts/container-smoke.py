#!/usr/bin/env python3
"""Exercise the packaged facade against CI's disposable S3-compatible store."""

import hashlib
import hmac
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import URLError
from urllib.request import Request, urlopen

BASE = "http://127.0.0.1:8097"
REPO = "image-e2e/docs"
HOOKS = {"/first": (101, "first-test-secret"), "/second": (202, "second-test-secret")}
deliveries = []
errors = []
changed = threading.Condition()


class Receiver(BaseHTTPRequestHandler):
    def do_POST(self):
        try:
            installation, secret = HOOKS[self.path]
            body = self.rfile.read(int(self.headers["Content-Length"]))
            expected = "sha256=" + hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
            assert hmac.compare_digest(self.headers.get("X-Hub-Signature-256", ""), expected), "signature"
            payload = json.loads(body)
            assert payload["installation"]["id"] == installation, "installation"
            assert payload["repository"]["full_name"] == REPO, "repository"
            assert self.headers.get("X-GitHub-Delivery"), "delivery ID"
            with changed:
                deliveries.append((installation, self.headers["X-GitHub-Event"], payload))
                changed.notify_all()
            self.send_response(200)
        except Exception as exc:
            with changed:
                errors.append(str(exc))
                changed.notify_all()
            self.send_response(400)
        self.end_headers()

    def log_message(self, *_args):
        pass


def api(path, body=None, method="GET"):
    req = Request(BASE + path, data=None if body is None else json.dumps(body).encode(),
                  headers={"Content-Type": "application/json"}, method=method)
    with urlopen(req, timeout=15) as response:
        raw = response.read()
        return json.loads(raw) if raw else None


def ready():
    until = time.monotonic() + 60
    while time.monotonic() < until:
        try:
            with urlopen(BASE + "/readyz", timeout=2) as response:
                if response.status == 200:
                    return
        except (URLError, TimeoutError):
            pass
        time.sleep(0.25)
    raise RuntimeError("packaged facade did not become ready")


def wait_for(event, predicate):
    until = time.monotonic() + 30
    with changed:
        while True:
            assert not errors, errors
            found = {installation for installation, name, payload in deliveries
                     if name == event and predicate(payload)}
            if found == {101, 202}:
                return
            remaining = until - time.monotonic()
            assert remaining > 0, f"missing {event} deliveries: got installations {found}"
            changed.wait(timeout=remaining)


def main():
    receiver = ThreadingHTTPServer(("127.0.0.1", 8098), Receiver)
    threading.Thread(target=receiver.serve_forever, daemon=True).start()
    try:
        ready()
        for app_id, (path, (installation, secret)) in enumerate(HOOKS.items(), start=1):
            registration = api("/api/v3/_dev/integrations" + path, {
                "app_id": app_id,
                "webhook_url": "http://127.0.0.1:8098" + path,
                "webhook_secret": secret,
                "installations": [{"id": installation, "owner": "image-e2e",
                                   "repository_selection": "all", "repositories": []}],
            }, "PUT")
            assert "webhook_secret" not in registration

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null",
                       GIT_TERMINAL_PROMPT="0")

            def git(*args):
                return subprocess.run(["git", *args], cwd=root, env=env, check=True,
                                      capture_output=True, text=True, timeout=60).stdout.strip()

            git("init", "-q", "-b", "main")
            git("config", "user.name", "Container smoke test")
            git("config", "user.email", "smoke@example.invalid")
            git("remote", "add", "origin", BASE + "/" + REPO + ".git")
            (root / "index.mdx").write_text("# Container smoke test\n")
            git("add", ".")
            git("commit", "-qm", "Seed fixture")
            git("push", "-q", "origin", "main")
            first = git("rev-parse", "HEAD")
            wait_for("push", lambda p: p["after"] == first)
            git("checkout", "-qb", "feature")
            (root / "index.mdx").write_text("# Updated fixture\n")
            git("commit", "-qam", "Update fixture")
            git("push", "-q", "origin", "feature")
            pr = api("/api/v3/repos/" + REPO + "/pulls",
                     {"title": "Container smoke", "head": "feature", "base": "main"}, "POST")
            wait_for("pull_request", lambda p: p["action"] == "opened" and p["number"] == pr["number"])

            subprocess.run(["docker", "restart", "walgithub-smoke"], check=True, timeout=150)
            ready()
            assert len(api("/api/v3/_dev/integrations")) == 2
            assert api("/api/v3/repos/" + REPO + "/pulls/" + str(pr["number"]))["state"] == "open"
            (root / "index.mdx").write_text("# Updated after restart\n")
            git("commit", "-qam", "Verify persisted integration")
            git("push", "-q", "origin", "feature")
            after = git("rev-parse", "HEAD")
            wait_for("push", lambda p: p["after"] == after)
            wait_for("pull_request", lambda p: p["action"] == "synchronize" and p["after"] == after)
        assert not errors, errors
        print("Container smoke passed: S3-backed Git, two signed webhook integrations, PRs, and restart persistence")
    finally:
        receiver.shutdown()
        receiver.server_close()


if __name__ == "__main__":
    main()
