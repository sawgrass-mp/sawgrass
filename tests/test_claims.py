"""Unit tests for scripts/claims.py. Standard library only: python3 -I tests/test_claims.py"""
import datetime
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "scripts"))
import claims  # noqa: E402
import policy  # noqa: E402

policy.MAINTAINERS = {"stiv"}


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


class Race(unittest.TestCase):
    LABELS = {"ready", "race", "claimed"}

    def test_more_racers_join(self):
        ok, reply = claims.decide_claim({"ready", "race"}, [], "alice", 0)
        self.assertTrue(ok)
        ok, reply = claims.decide_claim(self.LABELS, ["alice"], "bob", 0)
        self.assertTrue(ok)
        self.assertIn("@alice", reply)
        self.assertTrue(claims.decide_claim(self.LABELS, ["alice", "bob"], "carol", 1)[0])

    def test_same_racer_twice(self):
        self.assertFalse(claims.decide_claim(self.LABELS, ["alice"], "alice", 1)[0])

    def test_assignee_limit(self):
        full = [f"user{i}" for i in range(claims.MAX_ASSIGNEES)]
        self.assertFalse(claims.decide_claim(self.LABELS, full, "alice", 0)[0])
        self.assertTrue(claims.decide_claim(self.LABELS, full[:-1], "alice", 0)[0])

    def test_claim_limit_still_holds(self):
        self.assertFalse(claims.decide_claim(self.LABELS, ["bob"], "alice", claims.MAX_CLAIMS)[0])

    def test_race_needs_ready(self):
        self.assertFalse(claims.decide_claim({"race", "triage"}, [], "alice", 0)[0])

    def test_unclaim_targets(self):
        self.assertEqual(claims.unclaim_targets(self.LABELS, ["alice", "bob"], "bob"), ["bob"])
        self.assertEqual(claims.unclaim_targets(self.LABELS, ["alice", "bob"], "stiv"), ["alice", "bob"])
        self.assertEqual(claims.unclaim_targets({"ready", "claimed"}, ["alice"], "alice"), ["alice"])

    def test_unclaim_releases_only_the_racer(self):
        calls = []

        class Fake:
            def call(self, method, path, body=None, query=None):
                calls.append((method, path, body))

            def comment(self, number, text):
                calls.append(("COMMENT", number, text))

        issue = {"number": 7, "state": "open", "labels": [{"name": n} for n in sorted(self.LABELS)],
                 "assignees": [{"login": "alice"}, {"login": "bob"}]}
        event = {"issue": issue, "comment": {"body": "/unclaim", "user": {"login": "bob"},
                                             "author_association": "NONE"}}
        claims.handle_command(Fake(), event)
        self.assertIn(("DELETE", "/issues/7/assignees", {"assignees": ["bob"]}), calls)
        self.assertNotIn(("DELETE", "/issues/7/labels/claimed", None), calls)

    def test_expiry_per_racer(self):
        now = claims.parse_time("2026-11-30T10:00:00Z")
        old = now - datetime.timedelta(hours=claims.CLAIM_HOURS + 1)
        new = now - datetime.timedelta(hours=1)
        assigned = {"alice": old, "bob": old, "carol": new, "dave": None}
        prs = [("bob", "Closes #7"), ("alice", "Closes #8")]
        self.assertEqual(claims.expired_racers(7, assigned, now, prs), ["alice"])


    def test_assigned_at_takes_each_racers_last_assignment(self):
        class Fake:
            def pages(self, path, query):
                return [{"event": "assigned", "assignee": {"login": "alice"}, "created_at": "2026-11-20T10:00:00Z"},
                        {"event": "labeled", "label": {"name": "claimed"}, "created_at": "2026-11-20T10:00:01Z"},
                        {"event": "assigned", "assignee": {"login": "bob"}, "created_at": "2026-11-21T10:00:00Z"},
                        {"event": "assigned", "assignee": {"login": "alice"}, "created_at": "2026-11-22T10:00:00Z"},
                        {"event": "assigned", "assignee": {"login": "eve"}, "created_at": "2026-11-23T10:00:00Z"}]

        got = claims.assigned_at(Fake(), 7, ["alice", "bob", "carol"])
        self.assertEqual(got, {"alice": claims.parse_time("2026-11-22T10:00:00Z"),
                               "bob": claims.parse_time("2026-11-21T10:00:00Z"), "carol": None})

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
                "user": {"login": "stiv" if association == "MEMBER" else "outsider"},
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
        self.assertFalse(claims.decide_link(self.pr(user={"login": "member"}), self.issue()))
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

        # A fork's pull request is labelled too, but claims nothing.
        gh.calls.clear()
        claims.link(gh, self.pr(user={"login": "outsider"}, head={"repo": {"full_name": "x/fork"}}))
        self.assertEqual(gh.calls, [("POST", "/issues/50/labels", {"labels": ["protected"]})])


class LinkAll(unittest.TestCase):
    def test_one_failure_does_not_stop_the_rest(self):
        import urllib.error
        seen = []

        class Fake:
            def pages(self, path, query):
                return [{"number": 1}, {"number": 2}, {"number": 3}]

        def fake_link(gh, pr):
            seen.append(pr["number"])
            if pr["number"] == 2:
                raise urllib.error.HTTPError("u", 403, "Forbidden", {}, None)

        original, claims.link = claims.link, fake_link
        try:
            self.assertEqual(claims.link_all(Fake()), 1)
        finally:
            claims.link = original
        self.assertEqual(seen, [1, 2, 3])


class Expiry(unittest.TestCase):
    def test_boundary(self):
        start = claims.parse_time("2026-11-20T10:00:00Z")
        hours = datetime.timedelta(hours=claims.CLAIM_HOURS)
        self.assertFalse(claims.claim_expired(start, start + hours - datetime.timedelta(seconds=1)))
        self.assertTrue(claims.claim_expired(start, start + hours))


if __name__ == "__main__":
    unittest.main()
