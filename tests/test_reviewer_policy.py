"""Unit tests for tools/reviewer/policy.py. Standard library only: python3 -I tests/test_reviewer_policy.py"""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tools", "reviewer"))
import policy  # noqa: E402


class Tiers(unittest.TestCase):
    def test_file_tiers(self):
        cases = {
            "docs/architecture.md": 1,
            "resources/police/server.luau": 1,
            "tests/test_claims.py": 1,
            "README.md": 1,
            "crates/sawgrass-server/src/main.rs": 2,
            "crates/sawgrass-protocol/src/lib.rs": 2,
            "Cargo.toml": 2,
            "Cargo.lock": 2,
            ".github/workflows/ci.yml": 3,
            "AGENTS.md": 3,
            "scripts/claims.py": 3,
            "tools/reviewer/policy.py": 3,
            "docs/adr/0002-scripting.md": 3,
            "crates/sawgrass-client/src/lib.rs": 3,
            "crates/sawgrass-launcher/src/main.rs": 3,
            "crates/sawgrass-server/build.rs": 3,
            "deny.toml": 3,
        }
        for path, tier in cases.items():
            self.assertEqual(policy.file_tier(path), tier, path)

    def test_pr_tier_is_highest(self):
        self.assertEqual(policy.pr_tier(["docs/a.md", "tests/t.py"]), 1)
        self.assertEqual(policy.pr_tier(["docs/a.md", "crates/sawgrass-server/src/x.rs"]), 2)
        self.assertEqual(policy.pr_tier(["docs/a.md", ".github/CODEOWNERS"]), 3)

    def test_empty_is_tier3(self):
        self.assertEqual(policy.pr_tier([]), 3)


class LinkedIssue(unittest.TestCase):
    def test_found(self):
        self.assertEqual(policy.linked_issue("Closes #12"), 12)
        self.assertEqual(policy.linked_issue("text\nresolves: #7 and closes #9"), 7)

    def test_missing(self):
        for body in (None, "", "See #12", "Depends on: #3"):
            self.assertIsNone(policy.linked_issue(body), body)


class Checks(unittest.TestCase):
    def rows(self, **kw):
        args = dict(title="DOC-1: Fix typo", body="Closes #1\n\nModel: Claude Opus 5.5", paths=["docs/a.md"], changed_lines=10,
                    issue={"labels": [{"name": "ready"}]})
        args.update(kw)
        return {name: ok for name, ok, _ in policy.checks(**args)}

    def test_all_pass(self):
        self.assertTrue(all(self.rows().values()))

    def test_title(self):
        self.assertFalse(self.rows(title="Fix typo")["Task ID in title"])
        self.assertFalse(self.rows(title="FOO-1: x")["Task ID in title"])

    def test_issue(self):
        rows = self.rows(issue=None)
        self.assertFalse(rows["Linked task"])
        self.assertFalse(rows["Task is `ready`"])
        self.assertFalse(self.rows(issue={"labels": [{"name": "triage"}]})["Task is `ready`"])

    def test_model(self):
        for model in ("Claude Opus 5.5", "claude fable", "Claude Fable 5.2"):
            self.assertTrue(self.rows(body=f"Closes #1\nModel: {model}")["Model declared"], model)
        for body in ("Closes #1", "Closes #1\nModel: <!-- Claude Opus 5.5 or Claude Fable -->",
                     "Closes #1\nModel: Claude Opus 5", "Closes #1\nModel: GPT", "Closes #1\nModel: Claude Sonnet 5.5",
                     "Closes #1\nWe did not use Model: Claude Fable"):
            self.assertFalse(self.rows(body=body)["Model declared"], body)

    def test_maintainer_pull_request(self):
        rows = self.rows(body="Closes #1", issue={"labels": [{"name": "triage"}]}, maintainer=True)
        self.assertTrue(rows["Model declared"])
        self.assertTrue(rows["Task is `ready`"])
        self.assertFalse(self.rows(issue=None, maintainer=True)["Linked task"])

    def test_maintainer_pr(self):
        repo = {"repo": {"full_name": "sawgrass-mp/sawgrass"}}
        pr = {"author_association": "MEMBER", "head": repo, "base": repo}
        self.assertTrue(policy.maintainer_pr(pr))
        self.assertTrue(policy.maintainer_pr({**pr, "author_association": "OWNER"}))
        self.assertFalse(policy.maintainer_pr({**pr, "author_association": "CONTRIBUTOR"}))
        self.assertFalse(policy.maintainer_pr({**pr, "head": {"repo": {"full_name": "fork/sawgrass"}}}))
        self.assertFalse(policy.maintainer_pr({**pr, "head": {"repo": None}}))

    def test_size_limit_only_for_tier1(self):
        self.assertFalse(self.rows(changed_lines=policy.TIER1_MAX_LINES + 1)["Size"])
        self.assertTrue(self.rows(changed_lines=policy.TIER1_MAX_LINES)["Size"])
        self.assertTrue(self.rows(paths=["crates/sawgrass-server/src/x.rs"], changed_lines=5000)["Size"])


class Report(unittest.TestCase):
    def test_report(self):
        pr = {"title": "DOC-1: Fix typo", "body": "Closes #1\nModel: Claude Fable"}
        files = [{"filename": "docs/b.md", "previous_filename": "docs/a.md", "additions": 3, "deletions": 2}]
        lines = policy.report(pr, files, {"labels": [{"name": "ready"}]})
        self.assertEqual(lines[0], "Tier 1")
        self.assertTrue(all(line.startswith("- ✅") for line in lines[1:]), lines)
        self.assertIn("5 changed lines", lines[-1])

    def test_report_without_task(self):
        lines = policy.report({"title": "x", "body": None}, [], None)
        self.assertEqual(lines[0], "Tier 3")
        self.assertEqual(sum(line.startswith("- ❌") for line in lines), 4)


if __name__ == "__main__":
    unittest.main()
