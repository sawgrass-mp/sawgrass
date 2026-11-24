"""Unit tests for scripts/claims.py. Standard library only: python3 -I tests/test_claims.py"""
import datetime
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "scripts"))
import claims  # noqa: E402


class ParseCommand(unittest.TestCase):
    def test_commands(self):
        self.assertEqual(claims.parse_command("/claim"), "claim")
        self.assertEqual(claims.parse_command("\n  /CLAIM  \nthanks"), "claim")
        self.assertEqual(claims.parse_command("/unclaim"), "unclaim")

    def test_not_commands(self):
        for body in ("", None, "I'd like to /claim this", "/claim please", "/claimed", "hello\n/claim"):
            self.assertIsNone(claims.parse_command(body), body)


class DecideClaim(unittest.TestCase):
    def test_ready_and_free(self):
        ok, reply = claims.decide_claim({"ready"}, [], "alice", 0)
        self.assertTrue(ok)
        self.assertIn("@alice", reply)

    def test_already_claimed(self):
        ok, reply = claims.decide_claim({"ready", "claimed"}, ["bob"], "alice", 0)
        self.assertFalse(ok)
        self.assertIn("@bob", reply)

    def test_not_ready(self):
        self.assertFalse(claims.decide_claim({"triage"}, [], "alice", 0)[0])

    def test_limit(self):
        self.assertFalse(claims.decide_claim({"ready"}, [], "alice", claims.MAX_CLAIMS)[0])


class DecideUnclaim(unittest.TestCase):
    def test_assignee(self):
        self.assertTrue(claims.decide_unclaim(["alice"], "alice", "NONE")[0])

    def test_maintainer(self):
        self.assertTrue(claims.decide_unclaim(["alice"], "stiv", "OWNER")[0])

    def test_stranger(self):
        self.assertFalse(claims.decide_unclaim(["alice"], "mallory", "CONTRIBUTOR")[0])


class LinksIssue(unittest.TestCase):
    def test_keywords(self):
        for body in ("Closes #12", "fixes #12.", "Resolves: #12", "closed #12\n"):
            self.assertTrue(claims.links_issue(body, 12), body)

    def test_not_linked(self):
        for body in ("", None, "See #12", "Closes #123", "Closes #1"):
            self.assertFalse(claims.links_issue(body, 12), body)


class MaintainerClaims(unittest.TestCase):
    REPO = {"repo": {"full_name": "sawgrass-mp/sawgrass"}}

    def pr(self, **kw):
        return {"number": 50, "state": "open", "user": {"login": "stiv"}, "author_association": "MEMBER",
                "head": self.REPO, "base": self.REPO, "body": "Closes #49", "labels": [], **kw}

    def issue(self, labels=("ready",), association="MEMBER", state="open"):
        return {"number": 49, "state": state, "author_association": association,
                "labels": [{"name": n} for n in labels], "assignees": []}

    def test_no_limit_for_maintainers(self):
        self.assertTrue(claims.decide_claim({"ready"}, [], "stiv", claims.MAX_CLAIMS + 5, maintainer=True)[0])

    def test_decide_link(self):
        self.assertTrue(claims.decide_link(self.pr(), self.issue()))
        self.assertTrue(claims.decide_link(self.pr(), self.issue(labels=("triage",))))
        self.assertFalse(claims.decide_link(self.pr(), self.issue(labels=("ready", "claimed"))))
        self.assertFalse(claims.decide_link(self.pr(), self.issue(association="NONE")))
        self.assertFalse(claims.decide_link(self.pr(), self.issue(state="closed")))
        self.assertFalse(claims.decide_link(self.pr(), None))
        self.assertFalse(claims.decide_link(self.pr(author_association="CONTRIBUTOR"), self.issue()))
        self.assertFalse(claims.decide_link(self.pr(head={"repo": {"full_name": "x/fork"}}), self.issue()))

    def test_link_claims_and_marks_protected(self):
        pr, issue = self.pr(), self.issue()

        class Fake:
            calls = []

            def call(self, method, path, body=None, query=None):
                self.calls.append((method, path, body))
                return issue if method == "GET" else None

            def pages(self, path, query):
                return [{"filename": ".github/workflows/ci.yml"}]

            def comment(self, number, text):
                self.calls.append(("COMMENT", number, text))

        gh = Fake()
        claims.link(gh, pr)
        self.assertIn(("POST", "/issues/50/labels", {"labels": ["protected"]}), gh.calls)
        self.assertIn(("POST", "/issues/49/labels", {"labels": ["claimed"]}), gh.calls)
        self.assertIn(("POST", "/issues/49/assignees", {"assignees": ["stiv"]}), gh.calls)


class Expiry(unittest.TestCase):
    def test_boundary(self):
        start = claims.parse_time("2026-11-20T10:00:00Z")
        hours = datetime.timedelta(hours=claims.CLAIM_HOURS)
        self.assertFalse(claims.claim_expired(start, start + hours - datetime.timedelta(seconds=1)))
        self.assertTrue(claims.claim_expired(start, start + hours))


if __name__ == "__main__":
    unittest.main()
