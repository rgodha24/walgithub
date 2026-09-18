#!/usr/bin/env python3
"""Exercise editor Git/API operations against the release image and disposable S3."""

import base64
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
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
from urllib.parse import quote
from urllib.request import Request, urlopen

BASE = "http://127.0.0.1:8097"
REPO = "image-e2e/docs"
REST = "/api/v3/repos/" + REPO
REMOTE = BASE + "/" + REPO + ".git"
HOOKS = {"/first": (101, "first-test-secret"), "/second": (202, "second-test-secret")}
SEED = '---\ntitle: "Quickstart"\ndescription: "Keep this frontmatter"\n---\n\n# Start here\n'
deliveries = []
rejected = []
errors = []
changed = threading.Condition()
fail_second = threading.Event()


@contextmanager
def timed(label):
    started = time.monotonic()
    try:
        yield
    finally:
        print(f"{label}: {time.monotonic() - started:.3f}s", flush=True)


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
            failed = installation == 202 and fail_second.is_set()
            with changed:
                (rejected if failed else deliveries).append(
                    (installation, self.headers["X-GitHub-Event"], payload))
                changed.notify_all()
            self.send_response(503 if failed else 200)
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
    with timed(f"{method} {path}"), urlopen(req, timeout=30) as response:
        raw = response.read()
        return json.loads(raw) if raw else None


def contents(ref):
    file = api(REST + "/contents/index.mdx?ref=" + quote(ref, safe=""))
    return base64.b64decode(file["content"]).decode()


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


def wait_for(event, predicate, installations=frozenset({101, 202}), records=deliveries):
    until = time.monotonic() + 45
    with timed(f"webhook {event} to {sorted(installations)}"), changed:
        while True:
            assert not errors, errors
            found = {installation for installation, name, payload in records
                     if name == event and predicate(payload)}
            if found >= installations:
                return
            remaining = until - time.monotonic()
            assert remaining > 0, f"missing {event} deliveries: got installations {found}"
            changed.wait(timeout=remaining)


def run(args, **kwargs):
    result = subprocess.run(args, capture_output=True, text=True, timeout=60, **kwargs)
    assert result.returncode == 0, f"{args[0]} failed ({result.returncode}): {result.stderr}"
    return result.stdout.strip()


def replace_container():
    # A restart retains Docker's anonymous cache volume. Replace both the container
    # and its volume to prove the bucket alone preserves Git, PRs and webhook cursors.
    info = json.loads(run(["docker", "inspect", "walgithub-smoke"]))[0]
    logs = Path("smoke-logs")
    logs.mkdir(exist_ok=True)
    with (logs / "before-replacement.log").open("w") as output:
        subprocess.run(["docker", "logs", "walgithub-smoke"], stdout=output,
                       stderr=subprocess.STDOUT, check=True, timeout=30)
    run(["docker", "rm", "-fv", "walgithub-smoke"])
    fail_second.clear()
    env = [arg for value in info["Config"]["Env"] for arg in ("-e", value)]
    run(["docker", "run", "-d", "--name", "walgithub-smoke", "--network", "host",
         *env, info["Config"]["Image"]])
    ready()


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

            def git(*args, cwd=root):
                with timed("git " + args[0]):
                    return run(["git", "-c", "transfer.bundleURI=false", *args], cwd=cwd, env=env)

            def identity(path):
                git("config", "user.name", "Container smoke test", cwd=path)
                git("config", "user.email", "smoke@example.invalid", cwd=path)

            git("init", "-q", "-b", "main")
            identity(root)
            git("remote", "add", "origin", REMOTE)
            (root / "index.mdx").write_text(SEED)
            git("add", ".")
            git("commit", "-qm", "Seed fixture")
            git("push", "-q", "origin", "main")
            first = git("rev-parse", "HEAD")
            wait_for("push", lambda p: p["after"] == first)

            barrier = threading.Barrier(5, timeout=60)

            def push_branch(i):
                branch = f"editor/parallel-{i}"
                clone = root / f"clone-{i}"
                git("clone", "-q", "--depth=1", "--single-branch", "--branch", "main", REMOTE, str(clone))
                identity(clone)
                git("checkout", "-qb", branch, cwd=clone)
                content = SEED + f"\nExternal push {i}\n"
                (clone / "index.mdx").write_text(content)
                git("commit", "-qam", branch, cwd=clone)
                head = git("rev-parse", "HEAD", cwd=clone)
                barrier.wait()
                git("push", "-q", "origin", branch, cwd=clone)
                assert contents(branch) == content
                assert api(REST + "/branches/" + branch)["commit"]["sha"] == head
                pr = api(REST + "/pulls", {"title": branch, "head": branch, "base": "main"}, "POST")
                wait_for("push", lambda p: p["after"] == head and p["ref"] == "refs/heads/" + branch)
                wait_for("pull_request", lambda p: p["action"] == "opened" and p["number"] == pr["number"])
                return branch, head, content, pr["number"]

            with ThreadPoolExecutor(max_workers=4) as pool, timed("concurrent pushes and warm reads"):
                pending = [pool.submit(push_branch, i) for i in range(4)]
                barrier.wait()
                for _ in range(4):
                    assert contents("main") == SEED
                    assert api(REST + "/branches/main")["commit"]["sha"] == first
                branches = [future.result(timeout=120) for future in pending]
            assert len({number for _, _, _, number in branches}) == 4, "concurrent PR allocation collided"

            branch, head, _, number = branches[0]
            edited = SEED + "\nPublished through the editor API\n"
            mutation = {
                "query": "mutation($input: CreateCommitOnBranchInput!) { createCommitOnBranch(input: $input) { commit { oid } } }",
                "variables": {"input": {
                    "branch": {"repositoryNameWithOwner": REPO, "branchName": branch},
                    "expectedHeadOid": head,
                    "message": {"headline": "Editor publish"},
                    "fileChanges": {"additions": [{"path": "index.mdx", "contents": base64.b64encode(edited.encode()).decode()}]},
                }},
            }
            result = api("/api/graphql", mutation, "POST")
            assert not result.get("errors"), result
            published = result["data"]["createCommitOnBranch"]["commit"]["oid"]
            assert contents(branch) == edited
            wait_for("pull_request", lambda p: p["action"] == "synchronize" and p["after"] == published)
            stale = api("/api/graphql", mutation, "POST")
            assert stale.get("errors"), "stale editor save overwrote the branch"
            assert api(REST + "/branches/" + branch)["commit"]["sha"] == published

            fail_second.set()
            git("checkout", "-qb", "external/recovery")
            (root / "index.mdx").write_text(SEED + "\nRecover this webhook\n")
            git("commit", "-qam", "Offline receiver")
            git("push", "-q", "origin", "external/recovery")
            recovery = git("rev-parse", "HEAD")
            wait_for("push", lambda p: p["after"] == recovery, {101})
            wait_for("push", lambda p: p["after"] == recovery, {202}, rejected)
            with timed("cold replacement and replay"):
                replace_container()
                assert contents(branch) == edited
                assert len(api("/api/v3/_dev/integrations")) == 2
                assert api(REST + f"/pulls/{number}")["state"] == "open"
                wait_for("push", lambda p: p["after"] == recovery)
            for other, expected, content, _ in branches[1:]:
                assert contents(other) == content
                assert api(REST + "/branches/" + other)["commit"]["sha"] == expected

            merged = api(REST + f"/pulls/{number}/merge", {"merge_method": "merge"}, "PUT")
            assert merged["merged"], merged
            wait_for("push", lambda p: p["ref"] == "refs/heads/main" and p["after"] == merged["sha"])
            wait_for("pull_request", lambda p: p["number"] == number and p["action"] == "closed" and p["pull_request"]["merged"])
            assert contents("main") == edited
            git("clone", "-q", "--depth=1", "--single-branch", "--branch", "main", REMOTE, "published")
            assert git("-C", "published", "rev-parse", "HEAD") == merged["sha"]
            assert (root / "published" / "index.mdx").read_text() == edited
            git("push", "-q", "origin", "--delete", "external/recovery")
            wait_for("delete", lambda p: p["ref"] == "external/recovery")
        assert not errors, errors
        print("Container smoke passed: concurrent Git, editor publish, PR merge, signed webhook recovery, cold S3 persistence")
    finally:
        receiver.shutdown()
        receiver.server_close()


if __name__ == "__main__":
    main()
