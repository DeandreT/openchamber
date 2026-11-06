"""A loopback stand-in for github.com and api.github.com.

The isolated server reaches GitHub through its standard outbound proxy
(`HTTPS_PROXY`, `lib/outbound-proxy.js`) and trusts this fixture's
throwaway CA through `NODE_EXTRA_CA_CERTS`. The fixture answers `CONNECT`
only for github.com and api.github.com, terminates TLS with a leaf the CA
signed, and serves an in-memory repository, `acme/widgets`, through the REST
and GraphQL calls the server's GitHub provider makes for the source board:
the device flow, the signed-in user, repository metadata, reference search,
an item's detail and stack, pull request summaries, the pending review, and
the comment, state and review writes with their preflight reads. Every other
host is refused, so nothing leaves the machine.

The plain HTTP side of the same port is the test's control: `GET
/__fixture/state` returns the repository and every write, `GET
/__fixture/fail?operation=<name>` makes that operation (`search` or
`comment`) answer 502 until `GET /__fixture/recover`, and `GET
/__fixture/hold?operation=state` holds state changes until `GET
/__fixture/release`. The token it issues is fixed synthetic data; the
CA and key live in the run's own directory and are never installed anywhere.
"""

import json
import re
import ssl
import subprocess
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

OWNER = "acme"
REPO = "widgets"
TOKEN = "gho_fixture_synthetic_token"
VIEWER = {"login": "octo-reviewer", "id": 4242}
AUTHOR = {"login": "octo-author", "id": 1001}
HOSTS = ("github.com", "api.github.com")
CHECK_RUN = {
    "__typename": "CheckRun", "databaseId": 1, "name": "build", "status": "COMPLETED",
    "conclusion": "SUCCESS", "startedAt": "2026-11-01T09:00:00Z", "checkSuite": {"app": {"databaseId": 15368}},
}


def certificates(directory: Path) -> tuple[Path, Path, Path]:
    """A CA and a github.com/api.github.com leaf signed by it, made with the
    system's openssl in `directory`."""
    directory.mkdir(parents=True, exist_ok=True)
    ca_key, ca, key, csr, leaf = (directory / name for name in ("ca.key", "ca.pem", "leaf.key", "leaf.csr", "leaf.pem"))
    extensions = directory / "leaf.ext"
    extensions.write_text("subjectAltName=DNS:github.com,DNS:api.github.com\nbasicConstraints=CA:FALSE\n"
                          "keyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
    run = lambda *args: subprocess.run(["openssl", *args], check=True, capture_output=True, timeout=60)  # noqa: E731
    run("req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", str(ca_key), "-out", str(ca), "-days", "2",
        "-subj", "/CN=OpenChamber parity GitHub fixture CA",
        "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
    run("req", "-newkey", "rsa:2048", "-nodes", "-keyout", str(key), "-out", str(csr), "-subj", "/CN=github.com")
    run("x509", "-req", "-in", str(csr), "-CA", str(ca), "-CAkey", str(ca_key), "-CAcreateserial",
        "-out", str(leaf), "-days", "2", "-extfile", str(extensions))
    return ca, leaf, key


def _user(user: dict) -> dict:
    return {"login": user["login"], "avatarUrl": None}


class Repository:
    """`acme/widgets`: two open issues, an open and a draft pull request, and
    no closed issue, so the Closed filter is empty."""

    def __init__(self):
        self.lock = threading.RLock()
        self.items = {
            1: {"kind": "issue", "title": "Crash on startup when the config is empty", "state": "OPEN",
                "body": "Steps:\n\n1. Remove the config file\n2. Start the app", "labels": [("bug", "d73a4a")],
                "author": AUTHOR, "updatedAt": "2026-11-01T10:00:00Z",
                "comments": [{"author": AUTHOR, "body": "Still happens on 2.6.", "createdAt": "2026-11-01T10:00:00Z"}]},
            2: {"kind": "issue", "title": "Add a dark theme", "state": "OPEN", "body": "", "labels": [("enhancement", "a2eeef")],
                "author": AUTHOR, "updatedAt": "2026-10-20T08:00:00Z", "comments": []},
            3: {"kind": "pull", "title": "Fix the crash on an empty config", "state": "OPEN", "draft": False,
                "body": "Fixes #1.", "labels": [], "author": AUTHOR, "updatedAt": "2026-11-02T12:00:00Z",
                "head": "fix-empty-config", "base": "main", "sha": "3f2a9c1e5b7d4a60c8e1f2b3a4c5d6e7f8091a2b",
                "additions": 12, "deletions": 3, "files": 2, "comments": [], "reviews": []},
            4: {"kind": "pull", "title": "Refactor the loader", "state": "OPEN", "draft": True, "body": "",
                "labels": [], "author": AUTHOR, "updatedAt": "2026-10-15T08:00:00Z", "head": "loader", "base": "main",
                "sha": "9d8c7b6a5f4e3d2c1b0a99887766554433221100", "additions": 40, "deletions": 38, "files": 5,
                "comments": [], "reviews": []},
        }
        self.writes: list[dict] = []
        self.failing: set[str] = set()
        self.held = threading.Event()
        self.held.set()
        self.counter = 100

    def url(self, number: int) -> str:
        kind = "pull" if self.items[number]["kind"] == "pull" else "issues"
        return f"https://github.com/{OWNER}/{REPO}/{kind}/{number}"

    def snapshot(self) -> dict:
        with self.lock:
            return {"items": {str(number): {key: value for key, value in item.items() if key != "author"}
                              for number, item in self.items.items()},
                    "writes": list(self.writes), "failing": sorted(self.failing)}

    # GraphQL nodes ------------------------------------------------------

    def node(self, number: int) -> dict:
        item = self.items[number]
        node = {
            "__typename": "PullRequest" if item["kind"] == "pull" else "Issue",
            "number": number, "title": item["title"], "url": self.url(number),
            "createdAt": "2026-10-01T08:00:00Z", "updatedAt": item["updatedAt"], "body": item["body"],
            "author": _user(item["author"]),
            "labels": {"nodes": [{"name": name, "color": color} for name, color in item["labels"]]},
            "comments": {"totalCount": len(item["comments"])},
            "repository": {"name": REPO, "owner": {"login": OWNER}},
            "state": item["state"],
        }
        if item["kind"] == "issue":
            node["stateReason"] = None if item["state"] == "OPEN" else "COMPLETED"
        else:
            node.update({"isDraft": item["draft"], "headRefName": item["head"], "baseRefName": item["base"],
                         "headRefOid": item["sha"], "headRepository": {
                             "name": REPO, "url": f"https://github.com/{OWNER}/{REPO}",
                             "sshUrl": f"git@github.com:{OWNER}/{REPO}.git", "owner": {"login": OWNER}}})
        return node

    def search(self, query: str, first: int) -> dict:
        tokens = query.split()
        pulls = "is:pr" in tokens
        text = " ".join(token for token in tokens if ":" not in token).lower()
        matches = []
        for number, item in self.items.items():
            if (item["kind"] == "pull") != pulls:
                continue
            if "is:open" in tokens and item["state"] != "OPEN":
                continue
            if "is:closed" in tokens and item["state"] == "OPEN":
                continue
            if "is:merged" in tokens and item["state"] != "MERGED":
                continue
            if "author:@me" in tokens and item["author"] is not VIEWER:
                continue
            if ("assignee:@me" in tokens or "review-requested:@me" in tokens):
                continue
            if text and text not in item["title"].lower() and text not in item["body"].lower():
                continue
            matches.append(number)
        matches.sort(key=lambda number: self.items[number]["updatedAt"], reverse=True)
        nodes = [self.node(number) for number in matches[:first]]
        return {"search": {"issueCount": len(matches), "pageInfo": {"hasNextPage": False, "endCursor": None},
                           "nodes": nodes}}

    def detail(self, number: int) -> dict:
        item = self.items.get(number)
        if item is None:
            return {"repository": {"issueOrPullRequest": None}}
        comments = [{"author": _user(comment["author"]), "body": comment["body"], "createdAt": comment["createdAt"],
                     "url": f"{self.url(number)}#issuecomment-{index + 1}"}
                    for index, comment in enumerate(item["comments"])]
        if item["kind"] == "issue":
            node = {"__typename": "Issue", "number": number,
                    "comments": {"totalCount": len(comments), "nodes": comments}}
        else:
            verdicts = [review["state"] for review in item["reviews"] if review["state"] != "COMMENTED"]
            node = {
                "__typename": "PullRequest", "number": number, "state": item["state"],
                "reviewDecision": verdicts[-1] if verdicts else "REVIEW_REQUIRED",
                "headCommit": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": [CHECK_RUN]}}}}]},
                "reviewRequests": {"nodes": []},
                "additions": item["additions"], "deletions": item["deletions"], "changedFiles": item["files"],
                "comments": {"totalCount": len(comments), "nodes": comments},
                "commits": {"totalCount": 1, "nodes": [{"commit": {
                    "oid": item["sha"], "messageHeadline": item["title"], "committedDate": item["updatedAt"],
                    "url": f"https://github.com/{OWNER}/{REPO}/commit/{item['sha']}",
                    "author": {"name": "Octo Author", "user": _user(item["author"])}}}]},
                "reviews": {"nodes": [{"author": _user(VIEWER), "body": review["body"], "state": review["state"],
                                       "createdAt": review["createdAt"],
                                       "url": f"{self.url(number)}#pullrequestreview-{review['id']}"}
                                      for review in item["reviews"]]},
                "reviewThreads": {"nodes": []},
            }
        return {"repository": {"issueOrPullRequest": node}}

    def summary(self, number: int) -> dict | None:
        item = self.items.get(number)
        if item is None or item["kind"] != "pull":
            return None
        return {
            "number": number, "title": item["title"], "state": item["state"], "isDraft": item["draft"],
            "mergeable": "MERGEABLE", "mergeStateStatus": "CLEAN", "isInMergeQueue": False,
            "mergeQueueEntry": None, "autoMergeRequest": None, "headRefOid": item["sha"], "baseRefName": item["base"],
            "comments": {"totalCount": len(item["comments"])}, "reviews": {"totalCount": len(item["reviews"])},
            "recentComments": {"nodes": []}, "recentReviews": {"nodes": []},
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": [CHECK_RUN]}}}}]},
        }

    # REST shapes ----------------------------------------------------------

    def rest_user(self, user: dict) -> dict:
        return {"login": user["login"], "id": user["id"], "avatar_url": None,
                "html_url": f"https://github.com/{user['login']}", "type": "User"}

    def rest_repo(self) -> dict:
        return {"id": 77, "node_id": "R_77", "name": REPO, "full_name": f"{OWNER}/{REPO}",
                "owner": {"login": OWNER, "id": 9}, "html_url": f"https://github.com/{OWNER}/{REPO}",
                "clone_url": f"https://github.com/{OWNER}/{REPO}.git", "ssh_url": f"git@github.com:{OWNER}/{REPO}.git",
                "fork": False, "private": False, "default_branch": "main"}

    def rest_issue(self, number: int) -> dict:
        item = self.items[number]
        issue = {"id": 5000 + number, "node_id": f"I_{number}", "number": number, "title": item["title"],
                 "body": item["body"], "state": "open" if item["state"] == "OPEN" else "closed",
                 "html_url": self.url(number), "user": self.rest_user(item["author"]),
                 "labels": [{"name": name, "color": color} for name, color in item["labels"]],
                 "comments": len(item["comments"])}
        if item["kind"] == "pull":
            issue["pull_request"] = {"url": f"https://api.github.com/repos/{OWNER}/{REPO}/pulls/{number}"}
        return issue

    def rest_pull(self, number: int) -> dict:
        item = self.items[number]
        repo = self.rest_repo()
        return {"id": 6000 + number, "node_id": f"PR_{number}", "number": number, "title": item["title"],
                "body": item["body"], "html_url": self.url(number), "user": self.rest_user(item["author"]),
                "state": "open" if item["state"] == "OPEN" else "closed", "draft": item["draft"],
                "merged": item["state"] == "MERGED",
                "head": {"ref": item["head"], "sha": item["sha"], "repo": repo},
                "base": {"ref": item["base"], "sha": "0" * 40, "repo": repo}}


class Fixture:
    """The proxy, its repository and the files the server needs to trust it."""

    def __init__(self, directory: Path):
        self.repository = Repository()
        self.ca, leaf, key = certificates(directory)
        self.context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.context.load_cert_chain(str(leaf), str(key))
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), self._proxy_handler())
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def proxy_url(self) -> str:
        return f"http://127.0.0.1:{self.server.server_address[1]}"

    def stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()

    def _proxy_handler(self):
        fixture = self

        class Proxy(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def do_CONNECT(self):
                host, _, port = self.path.partition(":")
                if host.lower() not in HOSTS or port != "443":
                    self.send_error(403, "the fixture serves GitHub only")
                    return
                self.send_response(200, "Connection Established")
                self.end_headers()
                try:
                    tls = fixture.context.wrap_socket(self.connection, server_side=True)
                except (ssl.SSLError, OSError):
                    self.close_connection = True
                    return
                fixture._github_handler(host.lower())(tls, self.client_address, self.server)
                self.close_connection = True

            def do_GET(self):
                url = urlparse(self.path)
                query = parse_qs(url.query)
                if url.path == "/__fixture/state":
                    body = fixture.repository.snapshot()
                elif url.path == "/__fixture/fail":
                    with fixture.repository.lock:
                        fixture.repository.failing.add(query.get("operation", [""])[0])
                    body = {"ok": True}
                elif url.path == "/__fixture/recover":
                    with fixture.repository.lock:
                        fixture.repository.failing.clear()
                    body = {"ok": True}
                elif url.path == "/__fixture/hold":
                    fixture.repository.held.clear()
                    body = {"ok": True}
                elif url.path == "/__fixture/release":
                    fixture.repository.held.set()
                    body = {"ok": True}
                else:
                    self.send_error(404)
                    return
                payload = json.dumps(body).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        return Proxy

    def _github_handler(self, host: str):
        repository = self.repository

        class GitHub(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_):
                pass

            def reply(self, status: int, body) -> None:
                payload = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json; charset=utf-8")
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("X-OAuth-Scopes", "repo, read:org, read:user")
                self.end_headers()
                self.wfile.write(payload)

            def body(self) -> dict:
                length = int(self.headers.get("Content-Length") or 0)
                raw = self.rfile.read(length) if length else b""
                if not raw:
                    return {}
                if "json" in (self.headers.get("Content-Type") or ""):
                    return json.loads(raw)
                return {key: values[0] for key, values in parse_qs(raw.decode()).items()}

            def authorized(self) -> bool:
                header = self.headers.get("Authorization") or ""
                return header in (f"token {TOKEN}", f"bearer {TOKEN}", f"Bearer {TOKEN}")

            def failing(self, operation: str) -> bool:
                with repository.lock:
                    return operation in repository.failing

            def handle_request(self, method: str):
                url = urlparse(self.path)
                body = self.body()
                if host == "github.com":
                    if method == "POST" and url.path == "/login/device/code":
                        return self.reply(200, {"device_code": "fixture-device", "user_code": "FIXT-0001",
                                                "verification_uri": "https://github.com/login/device",
                                                "expires_in": 900, "interval": 1})
                    if method == "POST" and url.path == "/login/oauth/access_token":
                        return self.reply(200, {"access_token": TOKEN, "token_type": "bearer", "scope": "repo"})
                    return self.reply(404, {"message": "Not Found"})
                if not self.authorized():
                    return self.reply(401, {"message": "Bad credentials"})
                path = url.path
                if method == "POST" and path == "/graphql":
                    return self.graphql(body)
                if method == "GET" and path == "/user":
                    return self.reply(200, {**repository.rest_user(VIEWER), "name": "Octo Reviewer",
                                            "email": "reviewer@example.invalid"})
                if method == "GET" and path == "/user/emails":
                    return self.reply(200, [])
                prefix = f"/repos/{OWNER}/{REPO}"
                if method == "GET" and path == prefix:
                    return self.reply(200, repository.rest_repo())
                match = re.fullmatch(rf"{prefix}/(issues|pulls)/(\d+)(/comments|/reviews)?", path)
                if not match:
                    return self.reply(404, {"message": "Not Found"})
                collection, number, tail = match.group(1), int(match.group(2)), match.group(3)
                if method == "PATCH":
                    repository.held.wait(timeout=60)
                with repository.lock:
                    item = repository.items.get(number)
                    if item is None or (collection == "pulls" and item["kind"] != "pull"):
                        return self.reply(404, {"message": "Not Found"})
                    if method == "GET" and not tail:
                        rest = repository.rest_pull(number) if collection == "pulls" else repository.rest_issue(number)
                        return self.reply(200, rest)
                    if method == "PATCH" and not tail and body.get("state") in ("open", "closed"):
                        if item["state"] == "MERGED":
                            return self.reply(422, {"message": "Cannot change a merged pull request"})
                        item["state"] = "OPEN" if body["state"] == "open" else "CLOSED"
                        item["updatedAt"] = "2026-11-06T12:00:00Z"
                        repository.writes.append({"write": "state", "number": number, "state": body["state"]})
                        rest = repository.rest_pull(number) if collection == "pulls" else repository.rest_issue(number)
                        return self.reply(200, rest)
                    if method == "POST" and tail == "/comments" and collection == "issues":
                        if self.failing("comment"):
                            return self.reply(502, {"message": "Server Error"})
                        repository.counter += 1
                        item["comments"].append({"author": VIEWER, "body": body.get("body", ""),
                                                 "createdAt": "2026-11-06T12:00:00Z"})
                        repository.writes.append({"write": "comment", "number": number, "body": body.get("body")})
                        return self.reply(201, {"id": repository.counter, "body": body.get("body"),
                                                "user": repository.rest_user(VIEWER),
                                                "html_url": f"{repository.url(number)}#issuecomment-{repository.counter}",
                                                "created_at": "2026-11-06T12:00:00Z"})
                    if method == "POST" and tail == "/reviews" and collection == "pulls":
                        event = body.get("event")
                        states = {"APPROVE": "APPROVED", "REQUEST_CHANGES": "CHANGES_REQUESTED", "COMMENT": "COMMENTED"}
                        if body.get("commit_id") != item["sha"] or event not in states:
                            return self.reply(422, {"message": "Unprocessable Entity"})
                        repository.counter += 1
                        review = {"id": repository.counter, "state": states[event], "body": body.get("body") or "",
                                  "createdAt": "2026-11-06T12:00:00Z"}
                        item["reviews"].append(review)
                        repository.writes.append({"write": "review", "number": number, "state": review["state"],
                                                  "body": body.get("body")})
                        return self.reply(200, {"id": review["id"], "state": review["state"], "body": review["body"],
                                                "user": repository.rest_user(VIEWER)})
                return self.reply(404, {"message": "Not Found"})

            def graphql(self, body: dict):
                query = body.get("query") or ""
                variables = body.get("variables") or {}
                with repository.lock:
                    if "query ReferenceSearch" in query:
                        if "search" in repository.failing:
                            return self.reply(502, {"message": "Server Error"})
                        return self.reply(200, {"data": repository.search(variables.get("q", ""),
                                                                          int(variables.get("first") or 30))})
                    if "query ReferenceDetail" in query:
                        return self.reply(200, {"data": repository.detail(int(variables["number"]))})
                    if "query PullStack" in query:
                        item = repository.items.get(int(variables["number"]))
                        typename = "PullRequest" if item and item["kind"] == "pull" else "Issue"
                        return self.reply(200, {"data": {"repository": {"issueOrPullRequest": {
                            "__typename": typename, "stack": None}}}})
                    if "reviews(states: [PENDING]" in query:
                        return self.reply(200, {"data": {"repository": {"pullRequest": {"reviews": {"nodes": []}}}}})
                    if "fragment PrSummary" in query:
                        data = {}
                        for key, value in variables.items():
                            if key.startswith("p"):
                                summary = repository.summary(int(value))
                                data[f"a{key[1:]}"] = {"pullRequest": summary}
                        return self.reply(200, {"data": data})
                return self.reply(200, {"errors": [{"message": "The GitHub fixture does not answer this query"}]})

            def do_GET(self):
                self.handle_request("GET")

            def do_POST(self):
                self.handle_request("POST")

            def do_PATCH(self):
                self.handle_request("PATCH")

        return GitHub
