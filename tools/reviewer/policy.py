"""Deterministic rules of the review agent: risk tier, task link, size. Standard library only.

The language model never decides these; it only reviews the content. See docs/roadmap.md, "Review agent".

    python3 -I tools/reviewer/policy.py [--check] PR_NUMBER [OWNER/REPO]

prints the tier and the policy checks of a pull request, read through the `gh` command line tool. With --check it
exits 1 when any check fails (the `policy` CI job).
"""
import fnmatch
import json
import os
import re
import subprocess
import sys

# Tier 3: maintainers only. Changes here can reach players' machines, weaken CI, or change the agents' rules.
TIER3 = (
    ".github/*",
    "AGENTS.md",
    "CLAUDE.md",
    "CONTRIBUTING.md",
    "SECURITY.md",
    "deny.toml",
    "rust-toolchain.toml",
    "scripts/*",
    "tools/reviewer/*",
    "docs/adr/*",
    "crates/sawgrass-client*",
    "crates/sawgrass-launcher*",
    "*build.rs",
    ".claude/*",
)

# Tier 1: low risk, eligible for automatic merging once it is enabled.
TIER1 = (
    "docs/*",
    "resources/*",
    "tests/*",
    "README.md",
)

# Everything else (server core, protocol, other crates, Cargo.toml and Cargo.lock, migrations) is tier 2.

TASK_ID = re.compile(r"^(NET|SRV|IM|DB|SCRIPT|RP|BOT|CLIENT|LAUNCH|OPS|DOC)-[0-9]+: \S")
CLOSES = re.compile(r"\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s*:?\s+#([0-9]+)\b", re.IGNORECASE)
TIER1_MAX_LINES = 400
# Declared, not verified: GitHub cannot tell which model wrote a pull request. See AGENTS.md, "Hard rules".
MODEL = re.compile(r"^Model:[ \t]*Claude (?:Opus 5\.5|Fable)\b", re.IGNORECASE | re.MULTILINE)


def _matches(path, patterns):
    return any(fnmatch.fnmatchcase(path, p) for p in patterns)


def file_tier(path):
    if _matches(path, TIER3):
        return 3
    if _matches(path, TIER1):
        return 1
    return 2


def pr_tier(paths):
    """The highest tier among the changed paths (a rename counts both names). No files: tier 3."""
    return max((file_tier(p) for p in paths), default=3)


def linked_issue(body):
    """The first issue number a closing keyword links, or None."""
    m = CLOSES.search(body or "")
    return int(m.group(1)) if m else None


def checks(title, body, paths, changed_lines, issue):
    """Policy checks as a list of (name, passed, detail). `issue` is the linked issue's JSON or None."""
    labels = {label["name"] for label in (issue or {}).get("labels", [])}
    tier = pr_tier(paths)
    return [
        ("Task ID in title", bool(TASK_ID.match(title or "")), "titles start with `AREA-N: `"),
        ("Linked task", issue is not None, "`Closes #N` in the description, pointing at an issue"),
        ("Task is `ready`", "ready" in labels, "a maintainer labelled the task `ready`"),
        ("Model declared", bool(MODEL.search(body or "")),
         "`Model: Claude Opus 5.5` or `Model: Claude Fable` in the description (declared, not verified)"),
        ("Size", tier != 1 or changed_lines <= TIER1_MAX_LINES,
         f"{changed_lines} changed lines, added plus deleted (tier 1 limit {TIER1_MAX_LINES})"),
    ]


def report(pr, files, issue):
    """Markdown lines with the tier and checks, from the pull request, file and linked issue JSON of the REST API."""
    paths = [p for f in files for p in (f["filename"], f.get("previous_filename")) if p]
    changed = sum(f["additions"] + f["deletions"] for f in files)
    lines = [f"Tier {pr_tier(paths)}"]
    for name, ok, detail in checks(pr["title"], pr.get("body"), paths, changed, issue):
        lines.append(f"- {'✅' if ok else '❌'} {name}: {detail}")
    return lines


def _gh(path):
    out = subprocess.run(["gh", "api", path], check=True, capture_output=True, text=True).stdout
    return json.loads(out)


def main(argv):
    strict = argv[:1] == ["--check"]
    argv = argv[1:] if strict else argv
    if len(argv) not in (1, 2) or not argv[0].isdigit():
        print(__doc__, file=sys.stderr)
        return 2
    repo = argv[1] if len(argv) == 2 else os.environ.get("GITHUB_REPOSITORY", "sawgrass-mp/sawgrass")
    base = f"repos/{repo}"
    pr = _gh(f"{base}/pulls/{argv[0]}")
    files, page = [], 1
    while True:
        batch = _gh(f"{base}/pulls/{argv[0]}/files?per_page=100&page={page}")
        files += batch
        if len(batch) < 100:
            break
        page += 1
    number = linked_issue(pr.get("body"))
    issue = _gh(f"{base}/issues/{number}") if number is not None else None
    if issue is not None and "pull_request" in issue:
        issue = None
    lines = report(pr, files, issue)
    print("\n".join(lines))
    if strict and any(line.startswith("- ❌") for line in lines):
        for line in lines[1:]:
            if line.startswith("- ❌"):
                print(f"::error::{line[4:]}")
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
