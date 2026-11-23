"""Task dispatcher: labels `ready` the tasks a maintainer approved in advance. Standard library only.

    python3 -I scripts/dispatch.py issue   # check the issue in $GITHUB_EVENT_PATH
    python3 -I scripts/dispatch.py sweep   # check every open issue (after the approved list changes)

Needs GITHUB_TOKEN and GITHUB_REPOSITORY (both set by GitHub Actions).

A maintainer approves task IDs once, by merging them into `.github/approved-tasks.txt`. That file is a tier 3 path,
so adding an ID needs a maintainer's own task and merge. The dispatcher then labels an issue `ready` (and removes
`triage`) only when all of these hold:

- the issue is open, opened by a maintainer, and its title starts with `ID: ` for an ID on the approved list;
- no other issue, open or closed, has a title starting with the same `ID: `;
- its `### Scope` section lists no protected (tier 3) path, no item that could cover one, and no `Allows:`.

Anything else stays in `triage`. When a task title fails a check, the dispatcher says why in one comment on the
issue (edited, never repeated), so the maintainer sees it and decides by hand. Protected tasks (CI, rules, the
reviewer, client, launcher) always need the maintainer's own `ready`.

When a task the dispatcher labelled is edited so that it no longer qualifies (for example its scope widened to a
protected path), the dispatcher removes `ready` again, claimed or not, so the edit cannot grant scope to `pr-guard`.
It never removes a `ready` that someone else added.

Issue titles and bodies are untrusted input: they are only matched against patterns, never run.
"""
import json
import os
import re
import sys
import urllib.error

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tools", "reviewer"))
import claims  # noqa: E402
import policy  # noqa: E402
import pr_guard  # noqa: E402

READY = "ready"
TRIAGE = "triage"
BOT = "github-actions[bot]"
MARKER = "<!-- sawgrass-dispatcher -->"
APPROVED_FILE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".github", "approved-tasks.txt")
TITLE_ID = re.compile(r"^([A-Z]+-[0-9]+): \S")

# One concrete path inside each tier 3 pattern. A scope item that covers any of them is protected, so a broad item
# such as `crates/` or `*` cannot reach the client crate or a workflow without the maintainer.
PROBES = (
    ".github/workflows/x.yml", "AGENTS.md", "CLAUDE.md", "CONTRIBUTING.md", "SECURITY.md", "deny.toml",
    "rust-toolchain.toml", "scripts/x.py", "tools/reviewer/x.py", "docs/adr/0002-x.md",
    "crates/sawgrass-client/src/lib.rs", "crates/sawgrass-launcher/src/main.rs", "build.rs",
    "crates/sawgrass-server/build.rs", ".claude/settings.json",
)


# ---- pure logic (tested in tests/test_dispatch.py) ----

def parse_approved(text):
    """Task IDs from the approved list: one per line, `#` starts a comment."""
    ids = set()
    for line in (text or "").splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            ids.add(line)
    return ids


def task_id(title):
    m = TITLE_ID.match(title or "")
    return m.group(1) if m else None


def protected_scope(body):
    """Reasons the task's scope needs a maintainer: tier 3 items, items that cover one, or any `Allows:`."""
    paths, allows = pr_guard.scope_of(body)
    reasons = []
    if allows:
        reasons.append("it allows " + ", ".join(f"`{a}`" for a in sorted(allows)))
    for item in sorted(paths):
        if policy.file_tier(item) == 3 or any(pr_guard.in_scope(p, {item}) for p in PROBES):
            reasons.append(f"`{item}` is or covers a protected path")
    return reasons


def decide(issue, approved, other_titles):
    """(task ID or None, list of reasons it is not eligible). No reasons means it may be labelled `ready`."""
    tid = task_id(issue.get("title"))
    if tid is None:
        return None, ["the title does not start with a task ID"]
    reasons = []
    if issue.get("state") != "open":
        reasons.append("the issue is not open")
    if issue.get("author_association") not in claims.MAINTAINER_ASSOCIATIONS:
        reasons.append("a maintainer did not open it")
    if tid not in approved:
        reasons.append(f"`{tid}` is not on the approved list (`.github/approved-tasks.txt`)")
    if any(task_id(t) == tid for t in other_titles):
        reasons.append(f"another issue already uses `{tid}`")
    reasons.extend(protected_scope(issue.get("body")))
    return tid, reasons


def comment_text(tid, reasons):
    lines = [MARKER, f"Not labelled `{READY}` automatically: " + "; ".join(reasons) + ".", "",
             f"This task stays in `{TRIAGE}` until a maintainer labels it `{READY}` by hand."]
    return "\n".join(lines)


# ---- GitHub REST ----

def labels_of(issue):
    return {label["name"] for label in issue.get("labels", [])}


def ready_added_by(gh, number):
    """Login of whoever last added `ready`, or None."""
    actor = None
    for event in gh.pages(f"/issues/{number}/events", {}):
        if event.get("event") == "labeled" and event.get("label", {}).get("name") == READY:
            actor = (event.get("actor") or {}).get("login")
    return actor


def upsert_comment(gh, number, text):
    for c in gh.pages(f"/issues/{number}/comments", {}):
        if (c.get("user") or {}).get("login") == BOT and (c.get("body") or "").startswith(MARKER):
            if c.get("body") != text:
                gh.call("PATCH", f"/issues/comments/{c['id']}", {"body": text})
            return
    gh.comment(number, text)


def remove_label(gh, number, name):
    try:
        gh.call("DELETE", f"/issues/{number}/labels/{name}")
    except urllib.error.HTTPError as err:
        if err.code != 404:
            raise


def dispatch(gh, issue, approved, all_issues):
    if "pull_request" in issue:
        return
    number = issue["number"]
    others = [i.get("title") for i in all_issues if i["number"] != number and "pull_request" not in i]
    tid, reasons = decide(issue, approved, others)
    labels = labels_of(issue)
    if not reasons:
        if READY not in labels:
            gh.call("POST", f"/issues/{number}/labels", {"labels": [READY]})
            print(f"#{number} {tid}: ready")
        if TRIAGE in labels:
            remove_label(gh, number, TRIAGE)
        return
    if READY in labels and ready_added_by(gh, number) == BOT:
        remove_label(gh, number, READY)
        gh.call("POST", f"/issues/{number}/labels", {"labels": [TRIAGE]})
        labels.discard(READY)
        print(f"#{number}: ready removed")
    # A task a maintainer already labelled `ready` by hand needs no note.
    if tid is not None and issue.get("state") == "open" and READY not in labels:
        upsert_comment(gh, number, comment_text(tid, reasons))
        print(f"#{number} {tid}: left for a maintainer")


def main(argv):
    if argv not in (["issue"], ["sweep"]):
        print(__doc__, file=sys.stderr)
        return 2
    gh = claims.GitHub(os.environ["GITHUB_TOKEN"], os.environ["GITHUB_REPOSITORY"])
    with open(APPROVED_FILE, encoding="utf-8") as f:
        approved = parse_approved(f.read())
    all_issues = list(gh.pages("/issues", {"state": "all"}))
    if argv == ["issue"]:
        with open(os.environ["GITHUB_EVENT_PATH"], encoding="utf-8") as f:
            number = json.load(f)["issue"]["number"]
        # Re-read the issue: the event payload may be older than the latest edit.
        targets = [gh.call("GET", f"/issues/{number}")]
    else:
        targets = [i for i in all_issues if i.get("state") == "open"]
    for issue in targets:
        dispatch(gh, issue, approved, all_issues)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
