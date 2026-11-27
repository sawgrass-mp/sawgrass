"""Task claims for the swarm: `/claim` and `/unclaim` comments, and releasing stale claims.

    python3 -I scripts/claims.py command   # handle the issue comment in $GITHUB_EVENT_PATH
    python3 -I scripts/claims.py expire    # release claims older than CLAIM_HOURS with no open pull request
    python3 -I scripts/claims.py link N    # claim the task that the maintainer's pull request N closes
    python3 -I scripts/claims.py link-all  # the same for every open pull request

Needs GITHUB_TOKEN and GITHUB_REPOSITORY (both set by GitHub Actions). Standard library only.

Rules (CONTRIBUTING.md):
- Only an open issue labelled `ready` can be claimed; maintainers add that label, nobody else can.
- A claim adds the `claimed` label and assigns the commenter (GitHub lets anyone who commented be assigned).
- One account holds at most MAX_CLAIMS open claims; maintainers have no limit, since all the maintainer's agents
  share the maintainer's account.
- A pull request the maintainer opens from a branch of this repository claims the task it closes (`Closes #N`)
  without a `/claim` comment, and is labelled `protected` while it changes a tier 3 path, so the maintainer sees
  at merge time which pull requests touch CI, rules or the client.
- A claim with no open pull request by the assignee that links the issue (`Closes #N`) after CLAIM_HOURS is released.
- `/unclaim` works for the assignee and for maintainers.
- A task a maintainer also labels `race` takes a claim from every contributor, up to MAX_ASSIGNEES: each one is
  added as an assignee, and the first pull request that lands closes the task. A racer's `/unclaim` and the
  expiry release only that racer, timed from their own assignment; `claimed` goes when the last one leaves. A
  maintainer who is not racing releases them all with `/unclaim`.

The comment text is untrusted input: it is only compared with the two commands, never run or interpolated
into anything but a reply.
"""
import datetime
import json
import os
import re
import sys
import urllib.error
import urllib.parse
import urllib.request

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tools", "reviewer"))
import policy  # noqa: E402

API = "https://api.github.com"
READY = "ready"
CLAIMED = "claimed"
RACE = "race"
# GitHub keeps at most ten assignees on an issue.
MAX_ASSIGNEES = 10
MAX_CLAIMS = 2
CLAIM_HOURS = 72
PROTECTED = "protected"
MAINTAINER_ASSOCIATIONS = {"OWNER", "MEMBER", "COLLABORATOR"}


# ---- pure logic (tested in tests/test_claims.py) ----

def parse_command(body):
    """'claim', 'unclaim' or None: the first non-empty line of a comment, exactly one of the two commands."""
    for line in (body or "").splitlines():
        line = line.strip()
        if line:
            return {"/claim": "claim", "/unclaim": "unclaim"}.get(line.lower())
    return None


def decide_claim(labels, assignees, user, open_claims, maintainer=False):
    """(ok, reply) for `user` claiming an issue with these label names and assignee logins."""
    race = RACE in labels
    if CLAIMED in labels and not race:
        holders = ", ".join(f"@{a}" for a in assignees) or "someone"
        return False, f"This task is already claimed by {holders}. Pick another one from the `{READY}` list."
    if READY not in labels:
        return False, f"Only issues labelled `{READY}` can be claimed. A maintainer adds that label after triage."
    if race and user in assignees:
        return False, f"@{user} already races on this task."
    if race and len(assignees) >= MAX_ASSIGNEES:
        return False, f"This task already has {MAX_ASSIGNEES} racers, as many as GitHub can assign. Pick another one."
    if open_claims >= MAX_CLAIMS and not maintainer:
        return False, (f"@{user} already holds {open_claims} claims (the limit is {MAX_CLAIMS}). "
                       "Finish or `/unclaim` one first.")
    if race:
        others = ", ".join(f"@{a}" for a in assignees if a != user)
        return True, (f"@{user} joins the race" + (f" with {others}" if others else "") + ". The first pull "
                      "request that works is merged; the others rebase onto it or close. Open a draft pull request "
                      f"with `Closes #ISSUE` in its description within {CLAIM_HOURS} hours, or your claim is "
                      "released.")
    return True, (f"Claimed by @{user}. Open a draft pull request with `Closes #ISSUE` in its description "
                  f"within {CLAIM_HOURS} hours, or the claim is released.")


def decide_unclaim(assignees, user, association):
    """(ok, reply) for `user` releasing a claim."""
    if user in assignees or association in MAINTAINER_ASSOCIATIONS:
        return True, "Claim released; the task is open for others."
    return False, "Only the person who claimed this task or a maintainer can release it."


def unclaim_targets(labels, assignees, user):
    """Whom an allowed `/unclaim` by `user` releases: on a `race` task a racer releases only themself."""
    if RACE in labels and user in assignees:
        return [user]
    return list(assignees)


def expired_racers(number, assigned, now, open_prs):
    """The racers of task `number` to release: `assigned` maps each assignee to when they were assigned (or None),
    `open_prs` is (author, body) for every open pull request."""
    linked = {author for author, body in open_prs if links_issue(body, number)}
    return [login for login, since in assigned.items()
            if since is not None and claim_expired(since, now) and login not in linked]


def links_issue(pr_body, number):
    """Whether a pull request description links issue `number` with a closing keyword."""
    pattern = rf"\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s*:?\s+#{number}\b"
    return re.search(pattern, pr_body or "", re.IGNORECASE) is not None


def claim_expired(claimed_at, now, hours=CLAIM_HOURS):
    return now - claimed_at >= datetime.timedelta(hours=hours)


def parse_time(text):
    return datetime.datetime.fromisoformat(text.replace("Z", "+00:00"))


# ---- GitHub REST ----

class GitHub:
    def __init__(self, token, repo):
        self.token = token
        self.repo = repo

    def call(self, method, path, body=None, query=None):
        url = f"{API}/repos/{self.repo}{path}"
        if query:
            url += "?" + urllib.parse.urlencode(query)
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(url, data=data, method=method, headers={
            "Authorization": f"Bearer {self.token}",
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "sawgrass-claims",
        })
        with urllib.request.urlopen(req, timeout=30) as resp:
            raw = resp.read()
        return json.loads(raw) if raw else None

    def pages(self, path, query):
        page = 1
        while True:
            items = self.call("GET", path, query={**query, "per_page": 100, "page": page})
            yield from items
            if len(items) < 100:
                return
            page += 1

    def comment(self, number, text):
        self.call("POST", f"/issues/{number}/comments", {"body": text})


def handle_command(gh, event):
    issue, comment = event["issue"], event["comment"]
    if "pull_request" in issue or issue.get("state") != "open":
        return
    command = parse_command(comment.get("body"))
    if command is None:
        return
    number, user = issue["number"], comment["user"]["login"]
    labels = {label["name"] for label in issue.get("labels", [])}
    assignees = [a["login"] for a in issue.get("assignees", [])]
    if command == "claim":
        open_claims = sum(1 for i in gh.pages("/issues", {"state": "open", "labels": CLAIMED, "assignee": user})
                          if "pull_request" not in i)
        maintainer = policy.is_maintainer(comment.get("user"))
        ok, reply = decide_claim(labels, assignees, user, open_claims, maintainer)
        if ok:
            gh.call("POST", f"/issues/{number}/labels", {"labels": [CLAIMED]})
            gh.call("POST", f"/issues/{number}/assignees", {"assignees": [user]})
        gh.comment(number, reply.replace("#ISSUE", f"#{number}"))
    else:
        if CLAIMED not in labels:
            return
        ok, reply = decide_unclaim(assignees, user, comment.get("author_association", ""))
        if ok:
            targets = unclaim_targets(labels, assignees, user)
            release(gh, number, targets, last=len(targets) == len(assignees))
            if len(targets) < len(assignees):
                reply = f"@{user} left the race; the others keep their claims."
        gh.comment(number, reply)


def release(gh, number, assignees, last=True):
    """Unassign `assignees`; `last` says nobody is left, so the `claimed` label goes too."""
    if last:
        try:
            gh.call("DELETE", f"/issues/{number}/labels/{CLAIMED}")
        except urllib.error.HTTPError as err:
            if err.code != 404:
                raise
    if assignees:
        gh.call("DELETE", f"/issues/{number}/assignees", {"assignees": assignees})


def decide_link(pr, issue):
    """Whether the maintainer's pull request `pr` claims its open, unclaimed, maintainer-opened task `issue`."""
    if not policy.maintainer_pr(pr) or issue is None or "pull_request" in issue:
        return False
    labels = {label["name"] for label in issue.get("labels", [])}
    return issue.get("state") == "open" and policy.is_maintainer(issue.get("user")) and CLAIMED not in labels


def link(gh, pr):
    """Claim the task of a maintainer's open pull request; label any open one `protected` while it changes tier 3
    paths. The claim comes first, so a failed label cannot block it."""
    if pr.get("state") != "open":
        return
    claim_linked(gh, pr)
    files = list(gh.pages(f"/pulls/{pr['number']}/files", {}))
    paths = [p for f in files for p in (f["filename"], f.get("previous_filename")) if p]
    protected = any(policy.file_tier(p) == 3 for p in paths)
    if protected and PROTECTED not in {label["name"] for label in pr.get("labels", [])}:
        gh.call("POST", f"/issues/{pr['number']}/labels", {"labels": [PROTECTED]})
    elif not protected and PROTECTED in {label["name"] for label in pr.get("labels", [])}:
        try:
            gh.call("DELETE", f"/issues/{pr['number']}/labels/{PROTECTED}")
        except urllib.error.HTTPError as err:
            if err.code != 404:
                raise


def claim_linked(gh, pr):
    if not policy.maintainer_pr(pr):
        return
    number = policy.linked_issue(pr.get("body"))
    issue = gh.call("GET", f"/issues/{number}") if number is not None else None
    if decide_link(pr, issue):
        # A `/claim` may have landed since: it runs in another concurrency group, so read the issue again.
        issue = gh.call("GET", f"/issues/{number}")
    if decide_link(pr, issue):
        user = pr["user"]["login"]
        gh.call("POST", f"/issues/{number}/labels", {"labels": [CLAIMED]})
        gh.call("POST", f"/issues/{number}/assignees", {"assignees": [user]})
        gh.comment(number, f"Claimed by @{user} through pull request #{pr['number']}.")
        print(f"#{number} claimed through #{pr['number']}")


def claimed_at(gh, number):
    """When the `claimed` label was last added, or None."""
    last = None
    for event in gh.pages(f"/issues/{number}/events", {}):
        if event.get("event") == "labeled" and event.get("label", {}).get("name") == CLAIMED:
            last = parse_time(event["created_at"])
    return last


def assigned_at(gh, number, logins):
    """When each of `logins` was last assigned to issue `number`, or None."""
    last = dict.fromkeys(logins)
    for event in gh.pages(f"/issues/{number}/events", {}):
        login = (event.get("assignee") or {}).get("login")
        if event.get("event") == "assigned" and login in last:
            last[login] = parse_time(event["created_at"])
    return last


def expire(gh, now):
    open_prs = [(pr["user"]["login"], pr.get("body") or "") for pr in gh.pages("/pulls", {"state": "open"})]
    for issue in gh.pages("/issues", {"state": "open", "labels": CLAIMED}):
        if "pull_request" in issue:
            continue
        number = issue["number"]
        assignees = [a["login"] for a in issue.get("assignees", [])]
        if RACE in {label["name"] for label in issue.get("labels", [])} and assignees:
            gone = expired_racers(number, assigned_at(gh, number, assignees), now, open_prs)
            if gone:
                release(gh, number, gone, last=len(gone) == len(assignees))
                gh.comment(number, "Claim released for " + ", ".join(f"@{a}" for a in gone) + f": no open pull "
                                   f"request of theirs links this issue {CLAIM_HOURS} hours after they claimed it.")
                print(f"released #{number} for " + ", ".join(gone))
            continue
        since = claimed_at(gh, number)
        if since is None or not claim_expired(since, now):
            continue
        if any(author in assignees and links_issue(body, number) for author, body in open_prs):
            continue
        release(gh, number, assignees)
        gh.comment(number, f"Claim released: no open pull request links this issue {CLAIM_HOURS} hours after "
                           "it was claimed. Anyone can `/claim` it now.")
        print(f"released #{number}")


def link_all(gh):
    """`link` for every open pull request. One failure does not stop the others; any failure fails the run."""
    failed = []
    for pr in list(gh.pages("/pulls", {"state": "open"})):
        try:
            link(gh, pr)
        except urllib.error.HTTPError as err:
            print(f"#{pr['number']}: {err.code} {err.reason}", file=sys.stderr)
            failed.append(pr["number"])
    if failed:
        print("failed: " + ", ".join(f"#{n}" for n in failed), file=sys.stderr)
        return 1
    return 0


def main(argv):
    if argv not in (["command"], ["expire"], ["link-all"]) and not (len(argv) == 2 and argv[0] == "link"
                                                                    and argv[1].isdigit()):
        print(__doc__, file=sys.stderr)
        return 2
    gh = GitHub(os.environ["GITHUB_TOKEN"], os.environ["GITHUB_REPOSITORY"])
    if argv[0] == "link":
        link(gh, gh.call("GET", f"/pulls/{argv[1]}"))
    elif argv == ["link-all"]:
        return link_all(gh)
    elif argv == ["command"]:
        with open(os.environ["GITHUB_EVENT_PATH"], encoding="utf-8") as f:
            handle_command(gh, json.load(f))
    else:
        expire(gh, datetime.datetime.now(datetime.timezone.utc))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
