"""Qualify the native Git and Diff surfaces against an owned real engine.

Uses the Files and Git fixture repository (`isolated-server.mjs
--git-fixture`) with its bare remote served over loopback smart HTTP, since
the server plans network operations only for http, https and ssh remotes.
Drives the Git surface through AT-SPI and real input on a private display and
checks each outcome in the repository: staged and unstaged rows of one file,
stage, unstage, discard with its confirmation, a stage that fails on a held
`index.lock` and reads the status again, the selected change's diff, a
commit, branch create, rename, checkout and delete, push, fetch and pull as
planned operations, a push cancelled while the remote holds it, the Update
branch dialog's merge that stops on a conflict and is aborted from the banner,
its rebase onto a remote branch it fetches first, switching to the nested
repository and back, applying a stored identity, and the Diff surface's
scopes.
"""

import atexit
import json
import subprocess
import sys
import time
import urllib.request
import urllib.parse
from pathlib import Path

from git_http_remote import GitHttpRemote
from native_files import Session, git, parse_args, porcelain, run_checks
from native_platform import wait_for
import virtual_display

COMMIT_MESSAGE = "Native commit from the Git panel"


def change_id(scope: str, path: str) -> str:
    return f"git-change:{scope}:{path}"


def git_steps(session: Session):
    project = session.project
    remote = GitHttpRemote(project.parent)
    atexit.register(remote.stop)
    git(project, "remote", "set-url", "origin", f"{remote.url}/remote.git")
    other = project.parent / "other-clone"

    def refresh():
        session.client.click("git-refresh")
        time.sleep(0.5)

    def status_scopes():
        session.choose_project()
        lines = (project / "src/app.txt").read_text().splitlines()
        lines[0] = "staged edit"
        (project / "src/app.txt").write_text("\n".join(lines) + "\n")
        git(project, "add", "src/app.txt")
        lines[-1] = "unstaged edit"
        (project / "src/app.txt").write_text("\n".join(lines) + "\n")
        session.client.click("rail-git")
        session.wait_id("git-panel")
        session.wait_id(change_id("staged", "src/app.txt"))
        session.wait_id(change_id("unstaged", "src/app.txt"))
        session.wait_id(change_id("unstaged", "notes/untracked.txt"))
        if porcelain(project) != ["?? nested/", "?? notes/", "MM src/app.txt"]:
            raise RuntimeError(f"unexpected status {porcelain(project)}")

    def diff():
        session.client.click(change_id("unstaged", "src/app.txt"))
        session.wait_id("diff-content")

    def stage():
        session.client.click("git-stage:notes/untracked.txt")
        session.wait_id(change_id("staged", "notes/untracked.txt"))
        wait_for("the server did not stage", lambda: "A  notes/untracked.txt" in porcelain(project))

    def unstage():
        session.client.click("git-unstage:notes/untracked.txt")
        session.wait_id(change_id("unstaged", "notes/untracked.txt"))
        wait_for("the server did not unstage", lambda: "?? notes/" in porcelain(project))

    def discard():
        (project / "README.md").write_text("staged README\n")
        git(project, "add", "README.md")
        (project / "README.md").write_text("discard me\n")
        assert "MM README.md" in porcelain(project)
        refresh()
        session.wait_id("git-discard:README.md")
        session.client.click("git-discard:README.md")
        session.wait_id("git-discard-cancel")
        session.client.click("git-discard-cancel")
        session.wait_id("git-discard:README.md")
        if (project / "README.md").read_text() != "discard me\n":
            raise RuntimeError("cancel discarded the change")
        session.client.click("git-discard:README.md")
        session.wait_id("git-discard-confirm")
        session.client.click("git-discard-confirm")
        session.wait_gone(change_id("unstaged", "README.md"))
        wait_for("the staged change did not survive", lambda: "M  README.md" in porcelain(project))
        assert git(project, "show", ":README.md") == "staged README\n"
        assert (project / "README.md").read_text() == "staged README\n"

    def failed_stage():
        lock = project / ".git" / "index.lock"
        lock.write_text("")
        try:
            session.client.click("git-stage:src/app.txt")
            session.wait_label("git-notice", lambda text: text.startswith("Failed to stage changes"),
                               "the failed stage was not reported")
            session.wait_id(change_id("unstaged", "src/app.txt"))
            if "MM src/app.txt" not in porcelain(project):
                raise RuntimeError("the failed stage changed the index")
        finally:
            lock.unlink(missing_ok=True)

    def commit():
        session.client.click("git-stage:src/app.txt")
        wait_for("src/app.txt was not staged", lambda: "M  src/app.txt" in porcelain(project))
        session.wait_gone(change_id("unstaged", "src/app.txt"))
        session.client.click("git-commit-message")
        session.type(COMMIT_MESSAGE)
        session.client.click("git-commit")
        wait_for("the commit was not made", lambda: git(project, "log", "-1", "--format=%s").strip() == COMMIT_MESSAGE,
                 timeout=20)
        if git(project, "log", "-1", "--format=%an <%ae>").strip() != "Fixture Author <fixture@example.invalid>":
            raise RuntimeError("the commit has another author")
        session.wait_label("git-notice", lambda text: text == "Commit created", "the commit was not reported")

    def branches():
        session.client.click("git-branch-button")
        session.wait_id("git-branches")
        session.client.click("git-branch-create")
        session.type("feature/native")
        session.keys("Return")
        wait_for("the branch was not created and checked out",
                 lambda: git(project, "rev-parse", "--abbrev-ref", "HEAD").strip() == "feature/native", timeout=20)
        session.wait_id("git-branch-rename")
        session.client.click("git-branch-rename")
        time.sleep(0.3)
        session.keys("ctrl+a")
        session.type("feature/renamed")
        session.keys("Return")
        wait_for("the branch was not renamed",
                 lambda: git(project, "rev-parse", "--abbrev-ref", "HEAD").strip() == "feature/renamed", timeout=20)
        session.wait_id("git-branch:main")
        session.client.click("git-branch:main")
        wait_for("main was not checked out", lambda: git(project, "rev-parse", "--abbrev-ref", "HEAD").strip() == "main",
                 timeout=20)
        session.wait_id("git-branch-delete:feature/renamed")
        session.client.click("git-branch-delete:feature/renamed")
        wait_for("the branch was not deleted", lambda: "feature/renamed" not in git(project, "branch", "--list"),
                 timeout=20)
        session.client.click("git-branch-button")
        session.wait_gone("git-branches")

    def push():
        refresh()
        session.client.click("git-push")
        session.wait_label("git-notice", lambda text: text == "Push · Succeeded", "the push did not succeed",
                           timeout=60)
        remote_head = git(project.parent / "remote.git", "rev-parse", "main").strip()
        if remote_head != git(project, "rev-parse", "HEAD").strip():
            raise RuntimeError("the remote does not have the pushed commit")

    def fetch_pull():
        git(project.parent, "clone", "-q", str(project.parent / "remote.git"), str(other))
        git(other, "config", "user.name", "Other Author")
        git(other, "config", "user.email", "other@example.invalid")
        (other / "OTHER.md").write_text("from another clone\n")
        git(other, "add", "OTHER.md")
        git(other, "commit", "-q", "-m", "Other clone")
        git(other, "push", "-q", "origin", "main")
        upstream = git(other, "rev-parse", "HEAD").strip()
        session.client.click("git-fetch")
        session.wait_label("git-notice", lambda text: text == "Fetched from origin", "the fetch did not succeed",
                           timeout=60)
        if git(project, "rev-parse", "origin/main").strip() != upstream:
            raise RuntimeError("the fetch did not update origin/main")
        session.client.click("git-pull")
        session.wait_label("git-notice", lambda text: text == "Pulled from origin", "the pull did not succeed",
                           timeout=60)
        if git(project, "rev-parse", "HEAD").strip() != upstream:
            raise RuntimeError("the pull did not bring the upstream commit")

    def cancel_push():
        before = git(project.parent / "remote.git", "rev-parse", "main")
        (project / "SLOW.md").write_text("held by the remote\n")
        git(project, "add", "SLOW.md")
        git(project, "commit", "-q", "-m", "Slow push")
        refresh()
        remote.hold_pushes()
        pushes_before = remote.pushes()
        try:
            session.client.click("git-push")
            wait_for("the push never reached the remote", lambda: remote.pushes() > pushes_before, timeout=60)
            session.wait_id("git-operation-cancel", timeout=20)
            session.client.click("git-operation-cancel")
            session.wait_label("git-notice", lambda text: text.startswith("Push · Outcome unknown")
                               or text.startswith("Push · Cancelled"), "the cancel did not settle", timeout=60)
            session.wait_label("git-operation", lambda text: text in ("Push · Outcome unknown", "Push · Cancelled"),
                               "the operation row does not show the outcome")
        finally:
            remote.release()
        assert git(project.parent / "remote.git", "rev-parse", "main") == before, "cancelled held push changed main"

    def open_update_branch():
        refresh()
        session.client.click("git-repository-views")
        session.wait_id("git-update-branch")
        session.client.click("git-update-branch")
        session.wait_id("git-update-dialog")

    def choose_target(label: str):
        session.client.click("git-update-target")
        session.wait_id("git-update-targets")
        session.wait_id(f"git-update-target:{label}", timeout=20)
        if label == "side":
            options = [node.get_accessible_id() for node in session.client._walk()
                       if (node.get_accessible_id() or "").startswith("git-update-target:")]
            assert len(options) >= 2, "the keyboard proof needs more than one branch"
            wait_for("the picker has no initial highlight", lambda: session.has_state(options[0], "SELECTED"))
            for down, up in (("Down", "Up"), ("ctrl+n", "ctrl+p")):
                session.keys(down)
                wait_for("branch navigation did not move forward", lambda: session.has_state(options[1], "SELECTED"))
                session.keys(up)
                wait_for("branch navigation did not move back", lambda: session.has_state(options[0], "SELECTED"))
            session.type("side")
            session.wait_gone("git-update-target:origin/main")
            session.keys("Return")
        else:
            session.client.click(f"git-update-target:{label}")
        session.wait_label("git-update-target", lambda text: text == label, f"{label} was not chosen")

    def finish_update(expected: str):
        session.wait_id("git-update-done", timeout=60)
        session.wait_label("git-update-done", lambda text: text == expected, f"the run did not end with {expected}")
        session.client.click("git-update-done")
        session.wait_id("git-update-confirm")
        session.client.click("git-update-close")
        session.wait_gone("git-update-dialog")

    def merge_conflict():
        git(project, "checkout", "-q", "-b", "side")
        lines = (project / "src/app.txt").read_text().splitlines()
        lines[1] = "side edit"
        (project / "src/app.txt").write_text("\n".join(lines) + "\n")
        git(project, "commit", "-q", "-am", "Side edit")
        git(project, "checkout", "-q", "main")
        lines[1] = "main edit"
        (project / "src/app.txt").write_text("\n".join(lines) + "\n")
        git(project, "commit", "-q", "-am", "Main edit")
        open_update_branch()
        if not session.has_state("git-update-merge", "CHECKED") and not session.has_state("git-update-merge",
                                                                                            "PRESSED"):
            raise RuntimeError("Merge is not the starting operation")
        choose_target("side")
        session.wait_label("git-update-summary", lambda text: "side" in text and "main" in text,
                           "the summary does not name both branches")
        session.client.click("git-update-confirm")
        session.wait_id("git-conflict", timeout=60)
        session.wait_id("git-conflict-file:src/app.txt", timeout=20)
        if "UU src/app.txt" not in porcelain(project):
            raise RuntimeError(f"the merge did not stop on the conflict: {porcelain(project)}")
        before = {record["id"] for record in session.request("/api/session?limit=100")["data"]}
        session.client.click("git-conflict-new-session")
        session.wait_gone("git-conflict")
        finish_update("Close")
        wait_for("Resolve in new session did not fill the composer",
                 lambda: (session.client.focused_text() or "").startswith("Investigate the merge conflicts"))
        assert not any(node.get_role_name() == "button" and "conflict context (JSON)" in (node.get_name() or "")
                       for node in session.client._walk()), "hidden parts appeared as attachment chips"
        session.client.click("composer-submit")
        created = []

        def created_once():
            created[:] = [record for record in session.request("/api/session?limit=100")["data"]
                          if record["id"] not in before]
            return len(created) == 1

        wait_for("Resolve did not create its session", created_once, timeout=45)
        session_id = created[0]["id"]

        def handoff_sent(count):
            records = session.request(f"/api/session/{session_id}/message?limit=100")["data"]
            user = [record for record in records if record["info"]["role"] == "user"]
            synthetic = [record for record in records if record["info"]["role"] == "synthetic"]
            encoded = json.dumps(synthetic)
            return (len(user) == count and "merge conflict context (JSON)" in encoded
                    and str(project) in encoded and "src/app.txt" in encoded
                    and all("statusPorcelain" not in json.dumps(record) for record in user))

        wait_for("the hidden conflict parts did not reach the engine", lambda: handoff_sent(1), timeout=45)
        session.wait_id("git-operation-resolve")
        session.client.click("git-operation-resolve")
        session.wait_id("git-conflict-current-session")
        session.client.click("git-conflict-current-session")
        session.wait_gone("git-conflict")
        wait_for("Resolve in current session did not fill the composer",
                 lambda: (session.client.focused_text() or "").startswith("Investigate the merge conflicts"))
        session.client.click("composer-submit")
        wait_for("the current-session handoff was not sent", lambda: handoff_sent(2), timeout=45)
        assert len([record for record in session.request("/api/session?limit=100")["data"]
                    if record["id"] not in before]) == 1, "current-session Resolve created another session"
        session.wait_id("git-in-progress", timeout=20)
        session.client.click("git-operation-abort")
        session.wait_gone("git-in-progress", timeout=30)
        wait_for("the merge was not aborted", lambda: not (project / ".git" / "MERGE_HEAD").exists())
        if any(line.startswith("UU") for line in porcelain(project)):
            raise RuntimeError("the abort left a conflict")

    def continue_merge():
        open_update_branch()
        choose_target("side")
        session.client.click("git-update-confirm")
        session.wait_id("git-conflict")
        session.client.click("git-conflict-later")
        finish_update("Close")
        lines = (project / "src/app.txt").read_text().splitlines()
        (project / "src/app.txt").write_text("merge resolved\n" + "\n".join(line for line in lines
                            if not line.startswith(("<<<<<<<", "=======", ">>>>>>>"))) + "\n")
        git(project, "add", "src/app.txt")
        refresh()
        session.wait_id("git-operation-continue")
        session.client.click("git-operation-continue")
        session.wait_gone("git-in-progress", timeout=30)
        assert not (project / ".git/MERGE_HEAD").exists(), "Continue left the merge open"
        assert len(git(project, "rev-list", "--parents", "-n", "1", "HEAD").split()) == 3

    def rebase_conflict():
        git(project, "checkout", "-q", "-b", "rebase-base")
        (project / "rebase.txt").write_text("base version\n")
        git(project, "add", "rebase.txt")
        git(project, "commit", "-q", "-m", "Rebase base")
        git(project, "checkout", "-q", "main")
        (project / "rebase.txt").write_text("main version\n")
        git(project, "add", "rebase.txt")
        git(project, "commit", "-q", "-m", "Rebase main")
        before = git(project, "rev-parse", "HEAD")
        for abort in (True, False):
            open_update_branch()
            session.client.click("git-update-rebase")
            choose_target("rebase-base")
            session.client.click("git-update-confirm")
            session.wait_id("git-conflict")
            session.wait_id("git-conflict-file:rebase.txt")
            session.client.click("git-conflict-later")
            finish_update("Close")
            if abort:
                session.client.click("git-operation-abort")
                session.wait_gone("git-in-progress", timeout=30)
                assert git(project, "rev-parse", "HEAD") == before, "Abort changed the starting commit"
            else:
                (project / "rebase.txt").write_text("both versions resolved\n")
                git(project, "add", "rebase.txt")
                refresh()
                session.wait_id("git-operation-continue")
                session.client.click("git-operation-continue")
                session.wait_gone("git-in-progress", timeout=30)
                assert (project / "rebase.txt").read_text() == "both versions resolved\n"
                assert git(project, "rev-parse", "--abbrev-ref", "HEAD").strip() == "main"

    def rebase_remote():
        (other / "REMOTE.md").write_text("from the remote\n")
        git(other, "add", "REMOTE.md")
        git(other, "commit", "-q", "-m", "Remote only")
        git(other, "push", "-q", "origin", "main")
        upstream = git(other, "rev-parse", "HEAD").strip()
        git(project, "fetch", "-q", "origin")
        git(project, "update-ref", "refs/remotes/origin/main", git(project, "rev-parse", "HEAD~3").strip())
        open_update_branch()
        session.client.click("git-update-rebase")
        choose_target("origin/main")
        fetched_before = remote.fetches()
        remote.hold_fetches()
        try:
            session.client.click("git-update-confirm")
            wait_for("the integration fetch did not reach the remote", lambda: remote.fetches() > fetched_before)
            session.wait_id("git-update-cancel-fetch")
            time.sleep(32)
            assert session.has("git-update-cancel-fetch"), "a lost execute answer discarded the original operation ID"
            assert not session.has("git-update-done"), "a running fetch was presented as a finished failure"
        finally:
            remote.release_fetches()
        finish_update("Done")
        if git(project, "rev-parse", "origin/main").strip() != upstream:
            raise RuntimeError("the rebase did not fetch the remote branch first")
        subprocess.run(["git", "merge-base", "--is-ancestor", upstream, "HEAD"], cwd=project, check=True)
        if git(project, "rev-parse", "--abbrev-ref", "HEAD").strip() != "main":
            raise RuntimeError("the rebase left main")

    def nested():
        inner = project / "nested" / "inner"
        session.wait_id("git-repository")
        session.client.click("git-repository")
        session.wait_id(f"git-repository:{inner}")
        session.client.click(f"git-repository:{inner}")
        session.wait_label("git-repository", lambda text: text.endswith("nested/inner"),
                           "the surface did not switch to the nested repository", timeout=20)
        (inner / "inner-new.txt").write_text("nested change\n")
        refresh()
        session.wait_id(change_id("unstaged", "inner-new.txt"), timeout=20)
        session.client.click("git-stage:inner-new.txt")
        wait_for("the nested repository did not stage", lambda: "A  inner-new.txt" in porcelain(inner))
        if "inner-new.txt" in git(project, "status", "--porcelain"):
            raise RuntimeError("the outer repository took the nested change")
        session.client.click("git-repository")
        session.wait_id(f"git-repository:{project}")
        session.client.click(f"git-repository:{project}")
        session.wait_gone(change_id("staged", "inner-new.txt"), timeout=20)

    def identity():
        body = json.dumps({"id": "work", "name": "Work", "userName": "Work Author",
                           "userEmail": "work@example.invalid"}).encode()
        request = urllib.request.Request(f"{session.server.url}/api/git/identities", data=body, method="POST",
                                         headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(request, timeout=10) as response:
            response.read()
        session.client.click("git-identity")
        session.wait_id("git-identity:work", timeout=20)
        session.client.click("git-identity:work")
        wait_for("the identity was not written",
                 lambda: git(project, "config", "--local", "user.email").strip() == "work@example.invalid",
                 timeout=20)
        session.wait_label("git-identity", lambda text: text.endswith("Work"), "the header does not show Work",
                           timeout=20)
        session.wait_label("git-notice", lambda text: "Work" in text, "applying was not reported")

    def diff_surface():
        (project / "README.md").write_text("diff surface change\n")
        with (project / "src/app.txt").open("a") as source:
            source.write("walkthrough second stop\n")
        session.client.click("rail-diff")
        session.wait_id("diff-panel")
        session.wait_id("diff-content", timeout=30)
        session.client.click("diff-scope")
        session.wait_id("diff-scopes")
        session.client.click("diff-scope-staged")
        session.wait_id("diff-message", timeout=30)
        session.client.click("diff-scope")
        session.wait_id("diff-scopes")
        session.client.click("diff-scope-changed")
        session.wait_id("diff-content", timeout=30)

    def walkthrough():
        source = {"kind": "working-tree", "scope": "working"}
        query = urllib.parse.urlencode({"directory": str(project), "source": json.dumps(source)})
        session.client.click("diff-walkthrough")
        session.wait_id("walkthrough-panel")
        session.wait_id("walkthrough-generate", timeout=30)
        before = session.request("/api/walkthrough?" + query)
        assert before["walkthrough"] is None and not before["generating"], "opening spent model tokens"
        session.client.click("walkthrough-generate")
        session.wait_id("walkthrough-cancel")
        session.wait_id("walkthrough-progress")
        session.client.click("walkthrough-cancel")
        session.wait_gone("walkthrough-cancel", timeout=30)
        session.wait_id("walkthrough-generate", timeout=30)
        session.client.click("walkthrough-generate")
        session.wait_gone("walkthrough-cancel", timeout=60)
        result = session.request("/api/walkthrough?" + query)
        chapters = result["walkthrough"]["chapters"]
        stops = [stop for chapter in chapters for stop in chapter["stops"]]
        assert len(chapters) >= 2 and len(stops) >= 2, "the runtime did not generate navigable chapters"
        session.wait_id(f"walkthrough-stop:{stops[0]['id']}")
        session.wait_id("diff-content")
        session.client.click("walkthrough-next")
        assert (session.has_state(f"walkthrough-stop:{stops[1]['id']}", "CHECKED")
                or session.has_state(f"walkthrough-stop:{stops[1]['id']}", "PRESSED")), "Next did not advance"
        session.client.click("walkthrough-previous")
        assert (session.has_state(f"walkthrough-stop:{stops[0]['id']}", "CHECKED")
                or session.has_state(f"walkthrough-stop:{stops[0]['id']}", "PRESSED")), "Previous did not return"
        session.client.click("diff-walkthrough")
        session.wait_gone("walkthrough-panel")

    return [("status-scopes", status_scopes), ("diff", diff), ("stage", stage), ("unstage", unstage),
            ("discard", discard), ("failed-stage", failed_stage), ("commit", commit), ("branches", branches),
            ("push", push), ("fetch-pull", fetch_pull), ("cancel-push", cancel_push),
            ("merge-conflict", merge_conflict), ("continue-merge", continue_merge), ("rebase-remote", rebase_remote),
            ("rebase-conflict", rebase_conflict), ("nested", nested),
            ("identity", identity), ("diff-surface", diff_surface), ("walkthrough", walkthrough)]


def main():
    parser, args = parse_args(__doc__, "native-git")
    if not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            parser.error(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), *sys.argv[1:]],
                                          round(1440 * args.scale), round(900 * args.scale))
    return run_checks(args, git_steps)


if __name__ == "__main__":
    sys.exit(main())
