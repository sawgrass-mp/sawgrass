"""Deterministic rules of the review agent: risk tier, task link, size. Standard library only.

The language model never decides these; it only reviews the content. See docs/roadmap.md, "Review agent".
"""
import fnmatch
import re

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
        ("Size", tier != 1 or changed_lines <= TIER1_MAX_LINES,
         f"{changed_lines} changed lines (tier 1 limit {TIER1_MAX_LINES})"),
    ]
