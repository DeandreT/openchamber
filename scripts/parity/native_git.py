"""Qualify the native Git and Diff surfaces against an owned real engine.

Uses the Files and Git fixture repository (`isolated-server.mjs
--git-fixture`) with its bare remote served over loopback smart HTTP, since
the server plans network operations only for http, https and ssh remotes.
Drives the Git surface through AT-SPI and real input on a private display and
checks each outcome in the repository: staged and unstaged rows of one file,
stage, unstage, discard with its confirmation, a stage that fails on a held
`index.lock` and reads the status again, the selected change's diff, a
commit, branch create, rename, checkout and delete, push, fetch and pull as
planned operations, a push cancelled while the remote holds it, and the Diff
surface's scopes.
"""

import atexit
import sys
import time
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

    def diff_surface():
        (project / "README.md").write_text("diff surface change\n")
        session.client.click("rail-diff")
        session.wait_id("diff-panel")
        session.wait_id("diff-content", timeout=30)
        session.client.click("diff-scope-staged")
        session.wait_id("diff-message", timeout=30)
        session.client.click("diff-scope-changed")
        session.wait_id("diff-content", timeout=30)

    return [("status-scopes", status_scopes), ("diff", diff), ("stage", stage), ("unstage", unstage),
            ("discard", discard), ("failed-stage", failed_stage), ("commit", commit), ("branches", branches),
            ("push", push), ("fetch-pull", fetch_pull), ("cancel-push", cancel_push),
            ("diff-surface", diff_surface)]


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
