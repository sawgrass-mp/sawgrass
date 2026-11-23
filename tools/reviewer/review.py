"""Review agent, comment-only mode (task OPS-4).

    python tools/reviewer/review.py PR_NUMBER

Reads the pull request through the GitHub API (it never checks out or runs the pull request's code), reviews the
diff with Claude against the repository's rules, and posts or updates one comment with the tier, the policy checks,
the verdict and the findings. It never approves or merges.

Environment: GITHUB_REPOSITORY, GITHUB_TOKEN (read), REVIEWER_TOKEN (optional, posts the comment; defaults to
GITHUB_TOKEN), ANTHROPIC_API_KEY (without it the script logs a warning and exits 0).
Dependencies: tools/reviewer/requirements.txt (pinned with hashes).
"""
import json
import os
import pathlib
import sys
import urllib.parse
import urllib.request

import anthropic

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))  # python -I leaves the script's folder out
import policy  # noqa: E402

ROOT = pathlib.Path(__file__).resolve().parents[2]
HERE = pathlib.Path(__file__).resolve().parent
MODEL = "claude-opus-5-5"
MARKER = "<!-- sawgrass-reviewer -->"
# The whole diff goes to the model or none of it: a review of half a diff would be misleading.
MAX_DIFF_CHARS = 600_000
TIER_TEXT = {
    1: "Tier 1: low risk. Eligible for automatic merging once that is enabled; for now a maintainer merges.",
    2: "Tier 2: core code. A maintainer merges after reading this review.",
    3: "Tier 3: maintainers only. This review is advisory.",
}

REVIEW_SCHEMA = {
    "type": "object",
    "properties": {
        "verdict": {"type": "string", "enum": ["approve", "request_changes"]},
        "summary": {"type": "string"},
        "scope_ok": {"type": "boolean"},
        "findings": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "severity": {"type": "string", "enum": ["blocking", "minor"]},
                    "category": {"type": "string",
                                 "enum": ["safety", "scope", "correctness", "design", "tests"]},
                    "file": {"type": "string"},
                    "line": {"type": "integer", "description": "Line in the new file; 0 if not applicable."},
                    "message": {"type": "string"},
                },
                "required": ["severity", "category", "file", "line", "message"],
                "additionalProperties": False,
            },
        },
    },
    "required": ["verdict", "summary", "scope_ok", "findings"],
    "additionalProperties": False,
}


class GitHub:
    def __init__(self, token, repo):
        self.token, self.repo = token, repo

    def call(self, method, path, body=None, query=None):
        url = f"https://api.github.com/repos/{self.repo}{path}"
        if query:
            url += "?" + urllib.parse.urlencode(query)
        req = urllib.request.Request(url, method=method,
                                     data=json.dumps(body).encode() if body is not None else None, headers={
                                         "Authorization": f"Bearer {self.token}",
                                         "Accept": "application/vnd.github+json",
                                         "X-GitHub-Api-Version": "2022-11-28",
                                         "User-Agent": "sawgrass-reviewer",
                                     })
        with urllib.request.urlopen(req, timeout=60) as resp:
            raw = resp.read()
        return json.loads(raw) if raw else None

    def pages(self, path):
        page = 1
        while True:
            items = self.call("GET", path, query={"per_page": 100, "page": page})
            yield from items
            if len(items) < 100:
                return
            page += 1


def untrusted(name, text):
    # Neutralise a closing tag inside the content so it cannot end the block early.
    safe = (text or "").replace("</untrusted", "<\\/untrusted")
    return f'<untrusted name="{name}">\n{safe}\n</untrusted>'


def repo_context():
    parts = []
    for rel in ["AGENTS.md", "docs/architecture.md", "docs/roadmap.md",
                *sorted(str(p.relative_to(ROOT)) for p in (ROOT / "docs/adr").glob("*.md"))]:
        path = ROOT / rel
        if path.is_file():
            parts.append(f"<file path=\"{rel}\">\n{path.read_text(encoding='utf-8')}\n</file>")
    return "\n\n".join(parts)


def diff_text(files):
    out = []
    for f in files:
        header = f"--- {f['status']}: {f['filename']}"
        if f.get("previous_filename"):
            header += f" (from {f['previous_filename']})"
        out.append(header + "\n" + (f.get("patch") or "(no text diff: binary or too large for GitHub to show)"))
    return "\n\n".join(out)


def ask_claude(pr, issue, diff):
    client = anthropic.Anthropic()
    task = untrusted("linked-task", f"#{issue['number']} {issue['title']}\n\n{issue.get('body') or ''}") \
        if issue else "No linked task."
    user = "\n\n".join([
        "Repository rules and design (trusted, from the main branch):\n\n" + repo_context(),
        "Linked task:\n\n" + task,
        "Pull request title and description:\n\n" + untrusted("pull-request", f"{pr['title']}\n\n{pr.get('body') or ''}"),
        "Diff:\n\n" + untrusted("diff", diff),
    ])
    response = client.beta.messages.create(
        model=MODEL,
        max_tokens=16000,
        betas=["server-side-fallback-2026-07-01"],
        fallbacks="default",
        system=(HERE / "instructions.md").read_text(encoding="utf-8"),
        output_config={"effort": "high", "format": {"type": "json_schema", "schema": REVIEW_SCHEMA}},
        messages=[{"role": "user", "content": user}],
    )
    if response.stop_reason in ("refusal", "max_tokens"):
        return None, f"The model stopped with `{response.stop_reason}`; a maintainer needs to review this one."
    text = next((b.text for b in response.content if b.type == "text"), None)
    if text is None:
        return None, "The model returned no review; a maintainer needs to review this one."
    return json.loads(text), response.model


def render(pr, tier, check_rows, review, note, model):
    lines = [MARKER, "## Automated review (comment-only mode)", "", f"**{TIER_TEXT[tier]}**", "",
             "### Policy checks", ""]
    lines += [f"- {'✅' if ok else '❌'} {name}: {detail}" for name, ok, detail in check_rows]
    lines.append("")
    if review is None:
        lines += ["### Verdict: none", "", note]
    else:
        verdict = "looks good to merge" if review["verdict"] == "approve" else "request changes"
        lines += [f"### Verdict: {verdict}", "", review["summary"]]
        if not review["scope_ok"]:
            lines += ["", "⚠️ The change goes beyond the linked task."]
        if review["findings"]:
            lines += ["", "### Findings", ""]
            for f in sorted(review["findings"], key=lambda f: f["severity"] != "blocking"):
                where = f"`{f['file']}:{f['line']}`" if f["line"] else f"`{f['file']}`"
                lines.append(f"- **{f['severity']}** {where} ({f['category']}): {f['message']}")
    data = {"head": pr["head"]["sha"], "tier": tier, "model": model,
            "checks": {name: ok for name, ok, _ in check_rows}, "review": review}
    lines += ["", f"<sub>Head {pr['head']['sha'][:7]} · {model or 'no model run'} · This review never approves or "
                  "merges; a maintainer decides.</sub>",
              "<!-- sawgrass-reviewer-data: " + json.dumps(data).replace("--", "-\\u002d") + " -->"]
    return "\n".join(lines)


def main(argv):
    if len(argv) != 1 or not argv[0].isdigit():
        print(__doc__, file=sys.stderr)
        return 2
    if not os.environ.get("ANTHROPIC_API_KEY"):
        print("::warning::ANTHROPIC_API_KEY is not set; the review agent is skipped.")
        return 0
    number, repo = int(argv[0]), os.environ["GITHUB_REPOSITORY"]
    reader = GitHub(os.environ["GITHUB_TOKEN"], repo)
    writer = GitHub(os.environ.get("REVIEWER_TOKEN") or os.environ["GITHUB_TOKEN"], repo)

    pr = reader.call("GET", f"/pulls/{number}")
    if pr["draft"] or pr["state"] != "open":
        print("Draft or closed; nothing to review.")
        return 0
    files = list(reader.pages(f"/pulls/{number}/files"))
    paths = [p for f in files for p in (f["filename"], f.get("previous_filename")) if p]
    issue_number = policy.linked_issue(pr.get("body"))
    issue = None
    if issue_number is not None:
        candidate = reader.call("GET", f"/issues/{issue_number}")
        issue = None if "pull_request" in candidate else candidate
    tier = policy.pr_tier(paths)
    check_rows = policy.checks(pr["title"], pr.get("body"), paths,
                               sum(f["additions"] + f["deletions"] for f in files), issue)

    diff = diff_text(files)
    if len(diff) > MAX_DIFF_CHARS:
        review, model, note = None, None, (f"The diff is {len(diff):,} characters, over the {MAX_DIFF_CHARS:,} "
                                           "this agent reviews in one pass. Split the pull request.")
    else:
        review, model = ask_claude(pr, issue, diff)
        note = model if review is None else ""
        if review is None:
            model = None

    body = render(pr, tier, check_rows, review, note, model)
    mine = next((c for c in writer.pages(f"/issues/{number}/comments") if MARKER in (c.get("body") or "")), None)
    if mine:
        writer.call("PATCH", f"/issues/comments/{mine['id']}", {"body": body})
    else:
        writer.call("POST", f"/issues/{number}/comments", {"body": body})
    print(body)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
