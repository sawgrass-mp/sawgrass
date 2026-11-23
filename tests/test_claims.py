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


class Expiry(unittest.TestCase):
    def test_boundary(self):
        start = claims.parse_time("2026-11-20T10:00:00Z")
        hours = datetime.timedelta(hours=claims.CLAIM_HOURS)
        self.assertFalse(claims.claim_expired(start, start + hours - datetime.timedelta(seconds=1)))
        self.assertTrue(claims.claim_expired(start, start + hours))


if __name__ == "__main__":
    unittest.main()
