"""Pull request guard: fails loudly on changes that should never slip past a reviewer. Standard library only.

    python3 -I scripts/pr_guard.py PR_NUMBER [OWNER/REPO]

reads the pull request, its changed files and its linked task through the `gh` command line tool, prints the
findings and exits 1 if any rule fails. It never checks out or runs pull request code: the diff is data.

The rules catch three patterns seen in public pull requests to similar projects:

1. Smuggling. A pull request titled as one thing that adds workflows (scheduled, using secrets, force-pushing,
   deleting repositories) or changes other protected files. Every tier 3 path must be in the linked task's scope,
   and dangerous workflow constructs must be allowed by name.
2. Vandalism. Deleting or rewriting most of a prominent file such as README.md.
3. Shell injection. Code that hands a string to a shell, or a new shell or batch script.

A task grants scope in the `### Scope` section of its issue, with backticked items: paths (`scripts/x.py`, a
directory as `docs/assets/`, or a glob) and, after `Allows:`, constructs by name (`schedule`, `secrets`, `shell`).
Scope counts only on an open task labelled `ready`, opened by a maintainer and claimed by (assigned to) the pull
request's author, so an outside contributor can neither widen a task nor borrow another one's scope.
"""
import fnmatch
import json
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "tools", "reviewer"))
import policy  # noqa: E402

# Authors whose issues can grant scope. Only people with write access to the repository have these associations.
MAINTAINER_ASSOCIATIONS = {"OWNER", "MEMBER", "COLLABORATOR"}

WORKFLOW_PATHS = (".github/workflows/*", ".github/actions/*")

# Constructs in added workflow lines that need the task's explicit permission (`Allows: name`).
WORKFLOW_RULES = (
    ("schedule", re.compile(r"\bschedule\b|\bcron\s*:")),
    ("pull_request_target", re.compile(r"\bpull_request_target\b")),
    ("workflow_run", re.compile(r"\bworkflow_run\b")),
    ("secrets", re.compile(r"\bsecrets\b|\bgithub\.token\b|\bGITHUB_TOKEN\b")),
    ("write-permissions", re.compile(r"\bwrite-all\b|:\s*['\"]?write\b")),
    ("force-push", re.compile(r"--force|\bpush\b.*\s(-f\b|\+\S)|\+refs/|\bforce\s*[:=]\s*true")),
    ("api-write", re.compile(r"(-X|--method|--request)\s*['\"]?(DELETE|PATCH|PUT|POST)\b", re.IGNORECASE)),
    ("pipe-to-shell", re.compile(r"(curl|wget|iwr|Invoke-WebRequest)\b[^|]*\|\s*(sudo\s+)?(ba|z|da)?sh\b|\|\s*iex\b",
                                 re.IGNORECASE)),
)

# Shell execution in added code lines, in any file but Markdown. Allowed by `Allows: shell`.
SHELL_RULE = re.compile(
    r"Command::new\(\s*\"(cmd|sh|bash|zsh|dash|powershell|pwsh|wscript|cscript)(\.exe)?\""
    r"|\.args?\(\s*(\[\s*)?\"/[cCkK]\""
    r"|\bshell\s*=\s*True\b"
    r"|\bos\.(system|popen)\s*\("
    r"|\bchild_process\b"
    r"|\b(os\.execute|io\.popen)\s*\("
    r"|\bShellExecute|\bWinExec\s*\(|\bCreateProcess[AW]?\s*\("
)
# Files that change how Cargo builds or which code it pulls in. Allowed by `Allows: build`.
BUILD_PATHS = (".cargo/*", "*/.cargo/*")
BUILD_RULE = re.compile(r"^\s*(build|links)\s*=|proc-macro\s*=\s*true|\bgit\s*="
                        r"|^\s*\[(patch|replace)\b|\brustc-wrapper\b")
# Binary files that carry no code; any other file without an inspectable diff fails.
INERT_BINARY = ("docs/*.png", "docs/*.jpg", "docs/*.gif", "docs/*.webp")
SCRIPT_EXTENSIONS = (".bat", ".cmd", ".ps1", ".psm1", ".sh", ".bash", ".vbs", ".hta")
NO_CODE_EXTENSIONS = (".md", ".txt", ".svg")

# Prominent files that must not be deleted or mostly rewritten unless the task lists them.
PROMINENT = ("README.md", "AGENTS.md", "CLAUDE.md", "CONTRIBUTING.md", "SECURITY.md", "LICENSE",
             "docs/architecture.md")

SCOPE_SECTION = re.compile(r"^###\s*Scope\s*$(.*?)(?=^###\s|\Z)", re.MULTILINE | re.DOTALL)
BACKTICKED = re.compile(r"`([^`\n]+)`")


def scope_of(body):
    """(paths, allows) granted by the `### Scope` section of a task body."""
    m = SCOPE_SECTION.search(body or "")
    if not m:
        return set(), set()
    text = m.group(1)
    head, _, tail = text.partition("Allows:")
    paths = {t.strip() for t in BACKTICKED.findall(head) if t.strip()}
    allows = {t.strip() for t in BACKTICKED.findall(tail) if t.strip()}
    return paths, allows


def in_scope(path, paths):
    for item in paths:
        if path == item or fnmatch.fnmatchcase(path, item):
            return True
        if path.startswith(item.rstrip("/") + "/"):
            return True
    return False


def added_lines(patch):
    """(line number in the new file, text) for each added line of a unified diff hunk list."""
    out, line = [], 0
    for raw in (patch or "").splitlines():
        if raw.startswith("@@"):
            m = re.search(r"\+(\d+)", raw)
            line = int(m.group(1)) if m else 0
        elif raw.startswith("+") and not raw.startswith("+++"):
            out.append((line, raw[1:]))
            line += 1
        elif not raw.startswith("-"):
            line += 1
    return out


COMMENT_PREFIXES = (
    ((".rs", ".js", ".ts", ".c", ".cpp", ".h"), ("//",)),
    ((".py", ".sh", ".bash", ".ps1", ".psm1", ".toml", ".yml", ".yaml"), ("#",)),
    ((".lua", ".luau", ".sql"), ("--",)),
    ((".bat", ".cmd"), ("REM ", "rem ", "::")),
)


def _code(name, text):
    """False for blank lines and for comment lines in the file's own language, which cannot run anything."""
    stripped = text.strip()
    if not stripped:
        return False
    for extensions, prefixes in COMMENT_PREFIXES:
        if name.lower().endswith(extensions):
            return not stripped.startswith(prefixes)
    return True


def _clean(text):
    """One line of plain text: file names and diff lines are untrusted and may hold control characters."""
    return "".join(c if c.isprintable() else "?" for c in text)


def evaluate(pr, files, issue, base_lines):
    """Failures and warnings as lists of (path or None, message).

    `pr`, `files` and `issue` are REST API JSON (`issue` is None without a linked task). `base_lines` maps each
    prominent path the pull request changes to its line count on the base branch.
    """
    failures, warnings = [], []
    author = (pr.get("user") or {}).get("login")
    task_ok = (issue is not None
               and issue.get("state") == "open"
               and "ready" in {label["name"] for label in issue.get("labels", [])}
               and issue.get("author_association") in MAINTAINER_ASSOCIATIONS
               and author in {a["login"] for a in issue.get("assignees") or []})
    paths, allows = scope_of(issue.get("body")) if task_ok else (set(), set())
    if issue is None:
        warnings.append((None, "No linked task (`Closes #N`): nothing grants scope, so protected paths fail."))
    elif not task_ok:
        warnings.append((None, f"Task #{issue.get('number')} is not open, `ready`, opened by a maintainer and "
                               "claimed by this pull request's author, so its scope grants nothing."))

    for f in files:
        name, status, patch = f["filename"], f.get("status"), f.get("patch")
        names = [p for p in (name, f.get("previous_filename")) if p]
        lines = added_lines(patch)
        added = [(n, t) for n, t in lines if _code(name, t)]

        # 1. Protected paths and workflow constructs.
        for p in names:
            if policy.file_tier(p) == 3 and not in_scope(p, paths):
                failures.append((p, f"protected (tier 3) path changed, but the linked task's scope does not list "
                                    f"it. The title says \"{pr.get('title', '')}\"."))
            elif paths and not in_scope(p, paths):
                warnings.append((p, "outside the linked task's scope"))
        if policy._matches(name, WORKFLOW_PATHS) and status != "removed":
            if patch is None:
                failures.append((name, "workflow diff is too large or binary to inspect"))
            for construct, rule in WORKFLOW_RULES:
                if construct in allows:
                    continue
                # Comments too: a workflow line split with `\` can start a continuation with anything.
                for n, text in lines:
                    if rule.search(text):
                        failures.append((f"{name}:{n}", f"adds `{construct}` to a workflow, which the task does not "
                                                        f"allow: {text.strip()[:120]}"))

        # Build configuration that can run code at build time or pull it from elsewhere.
        if "build" not in allows and status != "removed":
            if policy._matches(name, BUILD_PATHS) and not in_scope(name, paths):
                failures.append((name, "Cargo configuration changed; the task must list it and allow `build`"))
            elif name == "Cargo.toml" or name.endswith("/Cargo.toml"):
                for n, text in added:
                    if BUILD_RULE.search(text):
                        failures.append((f"{name}:{n}", f"adds a build script, proc macro, git source or patch, "
                                                        f"which the task does not allow: {text.strip()[:120]}"))

        # 2. Vandalism of prominent files.
        for p in names:
            if p not in PROMINENT or in_scope(p, paths):
                continue
            base = base_lines.get(p, 0)
            if status == "removed" or (status == "renamed" and p != name):
                failures.append((p, "prominent file deleted or renamed away"))
            elif p == name and f.get("deletions", 0) >= 3 and 2 * f.get("deletions", 0) > base:
                failures.append((p, f"prominent file mostly rewritten: {f['deletions']} of {base} lines deleted"))

        # 3. Shell execution.
        if "shell" in allows or name.endswith(NO_CODE_EXTENSIONS) or status == "removed":
            continue
        if name.lower().endswith(SCRIPT_EXTENSIONS) and status in ("added", "renamed", "copied"):
            failures.append((name, "new shell, batch or script file; the task must allow `shell`"))
        if patch is None and not policy._matches(name, INERT_BINARY):
            failures.append((name, "diff is too large or binary to inspect"))
        for n, text in added:
            if SHELL_RULE.search(text):
                failures.append((f"{name}:{n}", f"hands a command to a shell, which the task does not allow: "
                                                f"{text.strip()[:120]}"))
    return failures, warnings


def report(failures, warnings):
    lines = []
    for path, msg in failures:
        lines.append(_clean(f"FAIL {path + ': ' if path else ''}{msg}"))
    for path, msg in warnings:
        lines.append(_clean(f"warn {path + ': ' if path else ''}{msg}"))
    if not failures:
        lines.append("OK: no guarded pattern found.")
    else:
        lines.append("A maintainer widens the scope by editing the task (see AGENTS.md, \"Protected changes\"); "
                     "never by editing this guard in the same pull request.")
    return lines


def _gh(path, raw=False):
    cmd = ["gh", "api", path] + (["-H", "Accept: application/vnd.github.raw"] if raw else [])
    out = subprocess.run(cmd, check=True, capture_output=True, text=True).stdout
    return out if raw else json.loads(out)


def main(argv):
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
    number = policy.linked_issue(pr.get("body"))
    issue = _gh(f"{base}/issues/{number}") if number is not None else None
    if issue is not None and "pull_request" in issue:
        issue = None
    base_lines = {}
    for f in files:
        for p in (f["filename"], f.get("previous_filename")):
            if p in PROMINENT:
                try:
                    text = _gh(f"{base}/contents/{p}?ref={pr['base']['sha']}", raw=True)
                    base_lines[p] = len(text.splitlines())
                except subprocess.CalledProcessError:
                    base_lines[p] = 0
    failures, warnings = evaluate(pr, files, issue, base_lines)
    print("\n".join(report(failures, warnings)))
    if os.environ.get("GITHUB_ACTIONS"):
        for path, msg in failures:
            file_part = f" file={_clean(path.split(':')[0]).replace(',', '%2C')}" if path else ""
            print(f"::error{file_part}::{_clean(msg).replace('%', '%25')}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
