"""Task dispatcher: labels `ready` every task a maintainer opens. Standard library only.

    python3 -I scripts/dispatch.py issue   # check the issue in $GITHUB_EVENT_PATH
    python3 -I scripts/dispatch.py sweep   # check every open issue

Needs GITHUB_TOKEN and GITHUB_REPOSITORY (both set by GitHub Actions).

The maintainer's own agents work through the maintainer's account, so their tasks need no hand labelling. The
dispatcher labels an issue `ready` (and removes `triage`) when all of these hold:

- the issue is open and was opened by a maintainer (an account with write access);
- its title starts with a task ID (`AREA-N: `) that no other issue uses.

That includes tasks whose scope lists protected (tier 3) paths: `pr-guard` still limits a pull request to the
paths its task names, and the maintainer's merge is the gate for those. Issues from anyone else stay in `triage`
for the maintainer; when such an issue has a task ID, one comment says so (edited, never repeated).

When a task the dispatcher labelled is edited so that it no longer qualifies, the dispatcher removes `ready`
again. It never removes a `ready` that someone else added.

Issue titles and bodies are untrusted input: they are only matched against patterns, never run.
"""
import json
import os
import re
import sys
import urllib.error

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import claims  # noqa: E402

READY = "ready"
TRIAGE = "triage"
BOT = "github-actions[bot]"
MARKER = "<!-- sawgrass-dispatcher -->"
TITLE_ID = re.compile(r"^([A-Z]+-[0-9]+): \S")

# ---- pure logic (tested in tests/test_dispatch.py) ----

def task_id(title):
    m = TITLE_ID.match(title or "")
    return m.group(1) if m else None


def decide(issue, other_titles):
    """(task ID or None, list of reasons it is not eligible). No reasons means it may be labelled `ready`."""
    tid = task_id(issue.get("title"))
    if tid is None:
        return None, ["the title does not start with a task ID"]
    reasons = []
    if issue.get("state") != "open":
        reasons.append("the issue is not open")
    if issue.get("author_association") not in claims.MAINTAINER_ASSOCIATIONS:
        reasons.append("a maintainer did not open it")
    if any(task_id(t) == tid for t in other_titles):
        reasons.append(f"another issue already uses `{tid}`")
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


def dispatch(gh, issue, all_issues):
    if "pull_request" in issue:
        return
    number = issue["number"]
    others = [i.get("title") for i in all_issues if i["number"] != number and "pull_request" not in i]
    tid, reasons = decide(issue, others)
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
    all_issues = list(gh.pages("/issues", {"state": "all"}))
    if argv == ["issue"]:
        with open(os.environ["GITHUB_EVENT_PATH"], encoding="utf-8") as f:
            number = json.load(f)["issue"]["number"]
        # Re-read the issue: the event payload may be older than the latest edit.
        targets = [gh.call("GET", f"/issues/{number}")]
    else:
        targets = [i for i in all_issues if i.get("state") == "open"]
    for issue in targets:
        dispatch(gh, issue, all_issues)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
