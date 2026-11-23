"""Unit tests for scripts/dispatch.py. Standard library only: python3 -I tests/test_dispatch.py"""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "scripts"))
import dispatch  # noqa: E402


def issue(title="NET-2: Client-to-gateway connection", scope="New crate `crates/sawgrass-gateway`, `Cargo.toml`.",
          association="MEMBER", state="open"):
    body = f"### What\n\nSomething.\n\n### Scope\n\n{scope}\n\n### Depends on\n\n#2\n"
    return {"number": 3, "title": title, "body": body, "author_association": association, "state": state}


APPROVED = {"NET-2", "BOT-1"}


class ParseApproved(unittest.TestCase):
    def test_ids_and_comments(self):
        text = "# Approved\nNET-2   # gateway\n\n  BOT-1\n#DOC-1\n"
        self.assertEqual(dispatch.parse_approved(text), {"NET-2", "BOT-1"})


class TaskId(unittest.TestCase):
    def test_ids(self):
        self.assertEqual(dispatch.task_id("NET-2: QUIC"), "NET-2")
        for title in ("OPS-5 follow-ups: guard", "Finding: x", "NET-2:QUIC", " NET-2: x", "", None):
            self.assertIsNone(dispatch.task_id(title), title)


class Decide(unittest.TestCase):
    def test_eligible(self):
        self.assertEqual(dispatch.decide(issue(), APPROVED, ["BOT-1: bots"]), ("NET-2", []))

    def test_not_approved(self):
        tid, reasons = dispatch.decide(issue(title="NET-9: x"), APPROVED, [])
        self.assertEqual(tid, "NET-9")
        self.assertIn("approved list", reasons[0])

    def test_no_task_id(self):
        self.assertEqual(dispatch.decide(issue(title="Finding: x"), APPROVED, [])[0], None)

    def test_outsider_or_closed(self):
        self.assertTrue(dispatch.decide(issue(association="NONE"), APPROVED, [])[1])
        self.assertTrue(dispatch.decide(issue(association="CONTRIBUTOR"), APPROVED, [])[1])
        self.assertTrue(dispatch.decide(issue(state="closed"), APPROVED, [])[1])

    def test_duplicate_id(self):
        reasons = dispatch.decide(issue(), APPROVED, ["NET-2: an older copy"])[1]
        self.assertTrue(any("another issue" in r for r in reasons))

    def test_protected_scope(self):
        for scope in ("`.github/workflows/ci.yml`", "`scripts/`", "`crates/sawgrass-client`", "`AGENTS.md`",
                      "`crates/`", "`*`", "`crates/*`", "`crates/sawgrass-server/build.rs`", "`docs/adr/`",
                      "`crates/sawgrass-server`. Allows: `shell`", "`tools/`", "`.github`"):
            self.assertTrue(dispatch.decide(issue(scope=scope), APPROVED, [])[1], scope)

    def test_ordinary_scope(self):
        for scope in ("`crates/sawgrass-bots`", "`docs/architecture.md`", "`Cargo.toml`, `Cargo.lock`",
                      "`tests/test_x.py`", "`resources/jobs/`", "`crates/sawgrass-server/src/`"):
            self.assertEqual(dispatch.decide(issue(scope=scope), APPROVED, [])[1], [], scope)

    def test_comment(self):
        text = dispatch.comment_text("NET-9", ["`NET-9` is not on the approved list"])
        self.assertTrue(text.startswith(dispatch.MARKER))
        self.assertIn("by hand", text)


class FakeGitHub:
    def __init__(self, ready_actor=None, comments=()):
        self.calls, self.ready_actor, self.comments = [], ready_actor, list(comments)

    def call(self, method, path, body=None, query=None):
        self.calls.append((method, path, body))

    def pages(self, path, query):
        if path.endswith("/events"):
            return [{"event": "labeled", "label": {"name": "ready"}, "actor": {"login": self.ready_actor}}]
        return self.comments

    def comment(self, number, text):
        self.calls.append(("COMMENT", number, text))


def with_labels(i, *names):
    return {**i, "labels": [{"name": n} for n in names]}


class Dispatch(unittest.TestCase):
    def test_labels_ready(self):
        gh = FakeGitHub()
        dispatch.dispatch(gh, with_labels(issue(), "triage"), APPROVED, [])
        self.assertIn(("POST", "/issues/3/labels", {"labels": ["ready"]}), gh.calls)
        self.assertIn(("DELETE", "/issues/3/labels/triage", None), gh.calls)

    def test_comments_once_when_not_eligible(self):
        gh = FakeGitHub()
        dispatch.dispatch(gh, with_labels(issue(title="NET-9: x"), "triage"), APPROVED, [])
        self.assertEqual([c[0] for c in gh.calls], ["COMMENT"])
        old = {"id": 7, "user": {"login": dispatch.BOT}, "body": dispatch.MARKER + " old"}
        gh = FakeGitHub(comments=[old])
        dispatch.dispatch(gh, with_labels(issue(title="NET-9: x"), "triage"), APPROVED, [])
        self.assertEqual([c[:2] for c in gh.calls], [("PATCH", "/issues/comments/7")])

    def test_removes_own_ready_after_scope_widens(self):
        gh = FakeGitHub(ready_actor=dispatch.BOT)
        dispatch.dispatch(gh, with_labels(issue(scope="`scripts/`"), "ready", "claimed"), APPROVED, [])
        self.assertIn(("DELETE", "/issues/3/labels/ready", None), gh.calls)

    def test_keeps_maintainers_ready(self):
        gh = FakeGitHub(ready_actor="spaceosint")
        dispatch.dispatch(gh, with_labels(issue(scope="`scripts/`"), "ready"), APPROVED, [])
        self.assertEqual(gh.calls, [])

    def test_ignores_pull_requests(self):
        gh = FakeGitHub()
        dispatch.dispatch(gh, {**with_labels(issue()), "pull_request": {}}, APPROVED, [])
        self.assertEqual(gh.calls, [])


class ApprovedFile(unittest.TestCase):
    def test_parses(self):
        with open(dispatch.APPROVED_FILE, encoding="utf-8") as f:
            ids = dispatch.parse_approved(f.read())
        for tid in ids:
            self.assertIsNotNone(dispatch.task_id(tid + ": x"), tid)


if __name__ == "__main__":
    unittest.main()
