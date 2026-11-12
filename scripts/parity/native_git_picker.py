"""Check branch-search key dispatch with default or configured New session.

Runs only on the private display. The configured profile sets the existing
new_chat preference to Ctrl+P before launch; it changes no application default.
"""

import json
from pathlib import Path
import sys
import time

from native_adapter import _atspi
from fault_proxy import RuleFaultProxy
from native_files import git, parse_args, run_checks
from native_platform import command, editable_text, wait_for
import virtual_display
import steps

SENTINEL = "Branch picker retained draft"


def mutation_guards():
    paths = ("/api/git/merge", "/api/git/rebase", "/api/git/network-operations*",
             "/api/git/checkout", "/api/git/commit", "/api/session/*/abort", "/api/session")
    return [steps.fault_rule({"id": f"premature-mutation-{index}", "method": "POST", "path": path,
                              "when": "before", "action": "fail", "times": 100}, "native")
            for index, path in enumerate(paths)]


def entries(root):
    stack, result = [root], []
    while stack:
        node = stack.pop()
        if node.get_role_name() == "entry":
            result.append(node)
        stack.extend(node.get_child_at_index(index) for index in range(node.get_child_count()))
    return result


def picker_steps(session, configured, escape_only=False):
    def check():
        # Keep the entire owned window inside the private display, including
        # the composer below the branch dialog. No desktop window is moved.
        virtual_display.verify_owned_display()
        command(session.env, "xdotool", "windowmove", session.client._window(), "0", "0")
        session.choose_project()
        session.focus_composer()
        session.type(SENTINEL)
        wait_for("the retained draft was not typed", lambda: session.client.focused_text() == SENTINEL)
        git(session.project, "branch", "keyboard-side")
        head = git(session.project, "rev-parse", "HEAD")
        branch = git(session.project, "rev-parse", "--abbrev-ref", "HEAD")

        def session_ids():
            reply = session.request("/api/session?limit=100")["data"]
            assert isinstance(reply, list), "the session read was incomplete"
            return sorted(row["id"] for row in reply)

        def draft_keys():
            return sorted(row["key"] for row in session.client.rows() if ":draft:" in row["key"])

        def composer():
            inputs = entries(session.client._find("composer"))
            assert len(inputs) == 1, "the composer needs exactly one editable"
            return inputs[0]

        sessions_before, drafts_before = session_ids(), draft_keys()
        assert drafts_before, "the retained draft has no sidebar owner"
        session.client.click("rail-git")
        session.wait_id("git-panel")
        session.client.click("git-repository-views")
        session.wait_id("git-update-branch")
        session.client.click("git-update-branch")
        session.wait_id("git-update-dialog")
        session.client.click("git-update-target")
        session.wait_id("git-update-search")
        session.type("i")
        wait_for("the branch search did not take the query", lambda: session.client.focused_text() == "i")
        session.wait_id("git-update-target:keyboard-side")
        session.wait_id("git-update-target:origin/main")
        proof = []

        def unchanged(stage):
            state = _atspi().StateType
            search = entries(session.client._find("git-update-search"))
            assert len(search) == 1 and search[0].get_state_set().contains(state.FOCUSED), "branch search lost focus"
            assert editable_text(composer()) == SENTINEL, "navigation changed the visible composer"
            assert session_ids() == sessions_before, "navigation created or removed a session"
            assert draft_keys() == drafts_before, "navigation changed the draft owner"
            assert git(session.project, "rev-parse", "HEAD") == head, "navigation changed HEAD"
            assert git(session.project, "rev-parse", "--abbrev-ref", "HEAD") == branch, "navigation switched branch"
            assert not (session.project / ".git/MERGE_HEAD").exists(), "navigation started a merge"
            assert not (session.project / ".git/rebase-merge").exists(), "navigation started a rebase"
            assert not (session.project / ".git/rebase-apply").exists(), "navigation started a rebase"
            assert not session.proxy.hit_list(), "navigation dispatched a Git mutation request"
            proof.append({"stage": stage, "focusedSearch": True, "composerUnchanged": True,
                          "sessions": sessions_before, "drafts": drafts_before, "mutationStarted": False,
                          "mutationRequests": session.proxy.hit_list()})
            (session.args.output / "key-dispatch.json").write_text(json.dumps(proof, indent=2) + "\n")

        first, second = "git-update-target:keyboard-side", "git-update-target:origin/main"
        wait_for("initial branch highlight was missing", lambda: session.has_state(first, "SELECTED"))
        unchanged("opened")
        pairs = (("Down", "Up"),) if escape_only else (("Down", "Up"), ("ctrl+n", "ctrl+p"))
        for down, up in pairs:
            session.keys(down)
            wait_for(f"{down} did not move to the next branch", lambda: session.has_state(second, "SELECTED"))
            unchanged(down)
            session.keys(up)
            wait_for(f"{up} did not return to the first branch", lambda: session.has_state(first, "SELECTED"))
            unchanged(up)
        session.keys("Escape")
        time.sleep(0.1)
        focused = [{"id": node.get_accessible_id(), "name": node.get_name(), "role": node.get_role_name()}
                   for node in session.client._walk()
                   if node.get_state_set().contains(_atspi().StateType.FOCUSED)]
        proof.append({"stage": "Escape-dispatched", "pickerPresent": session.has("git-update-targets"),
                      "focused": focused, "sessions": session_ids(), "drafts": draft_keys(),
                      "mutationRequests": session.proxy.hit_list()})
        (session.args.output / "key-dispatch.json").write_text(json.dumps(proof, indent=2) + "\n")
        session.wait_gone("git-update-targets")
        wait_for("Escape did not restore focus to the branch trigger", lambda:
                 session.has_state("git-update-target", "FOCUSED"))
        assert editable_text(composer()) == SENTINEL, "Escape changed the composer"
        assert session_ids() == sessions_before and draft_keys() == drafts_before, "Escape changed the navigation owner"
        assert not session.proxy.hit_list(), "Escape dispatched a mutation request"
        if escape_only:
            return
        session.client.click("git-update-close")
        session.wait_gone("git-update-dialog")
        # The commit field exists only while the repository has changes.
        (session.project / "picker-dirty.txt").write_text("dirty\n")
        session.client.click("git-refresh")
        session.wait_id("git-commit-message")
        commit = entries(session.client._find("git-commit-message"))
        assert len(commit) == 1, "the commit field needs exactly one editable"
        session.click_node(commit[0])
        wait_for("the commit field did not gain focus", lambda:
                 commit[0].get_state_set().contains(_atspi().StateType.FOCUSED))
        global_key = "ctrl+p" if configured else "ctrl+n"
        session.keys(global_key)
        wait_for("the closed picker still blocked New session", lambda:
                 composer().get_state_set().contains(_atspi().StateType.FOCUSED)
                 and editable_text(composer()) == SENTINEL)
        assert session_ids() == sessions_before, "the global shortcut submitted a server session"
        assert any(SENTINEL in row["label"] for row in session.client.rows()), "New session lost the retained draft"
        assert git(session.project, "rev-parse", "HEAD") == head, "the closed shortcut mutated Git"
        assert not session.proxy.hit_list(), "the closed shortcut dispatched a Git mutation request"
        proof.append({"stage": "closed", "globalShortcut": global_key, "composerRefocused": True,
                      "retainedDraftPresent": True, "mutationStarted": False,
                      "dismissal": "Escape", "mutationRequests": session.proxy.hit_list()})
        (session.args.output / "key-dispatch.json").write_text(json.dumps(proof, indent=2) + "\n")

    return [("branch-picker-escape" if escape_only else "branch-picker-key-dispatch", check)]


def main():
    mode = sys.argv.pop(1) if len(sys.argv) > 1 else ""
    if mode not in ("default", "configured", "escape"):
        raise SystemExit("Expected default, configured or escape before the standard native driver options")
    if not virtual_display.inside():
        problem = virtual_display.available()
        if problem:
            raise RuntimeError(problem)
        return virtual_display.run_inside([sys.executable, str(Path(__file__).resolve()), mode, *sys.argv[1:]], 1440, 900)
    virtual_display.verify_owned_display()
    _, args = parse_args(__doc__, "native-git-picker")
    configured = mode == "configured"

    def configure(session):
        session.proxy = RuleFaultProxy(session.server.url)
        for rule in mutation_guards():
            session.proxy.add(rule)
        if configured:
            saved = session.request("/api/config/settings?surface=desktop",
                                    {"shortcutOverrides": {"new_chat": "mod+p"}}, method="PUT")
            assert saved.get("shortcutOverrides", {}).get("new_chat") == "mod+p", "the fixture override was not saved"

    return run_checks(args, lambda session: picker_steps(session, configured, mode == "escape"), configure)


if __name__ == "__main__":
    sys.exit(main())
