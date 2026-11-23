# Maintainer setup

One-time repository settings that cannot live in files. Run them with an admin token after this foundation is
merged.

## Labels

```sh
R=sawgrass-mp/sawgrass
gh label create ready            -R $R -c 0e8a16 -d "Triaged; can be claimed with /claim"
gh label create claimed          -R $R -c fbca04 -d "Someone is working on it"
gh label create triage           -R $R -c d4c5f9 -d "New; waits for a maintainer"
gh label create "good first task" -R $R -c 7057ff -d "Small and self-contained"
gh label create "size: S"        -R $R -c c2e0c6 -d "An evening"
gh label create "size: M"        -R $R -c c2e0c6 -d "A few days"
gh label create "size: L"        -R $R -c c2e0c6 -d "Several pull requests"
```

Only people with triage access can add labels, so only maintainers can mark a task `ready`.

## Branch protection on `main`

- Require a pull request before merging, with 1 approval and review from Code Owners.
- Dismiss stale approvals when new commits are pushed.
- Require these status checks: `fmt`, `clippy`, `test`, `deny`, `scripts`, `no-game-data`, `task-id`.
- Require linear history (squash merge only); block force pushes and deletion.
- Do not allow bypassing the above, including for administrators.

## Actions settings

- Settings → Actions → General: "Require approval for all external contributors" for fork pull requests.
- Workflow permissions: read repository contents by default; workflows ask for more themselves.
- Never add a `pull_request_target` workflow that checks out pull request code.

## Security

- Enable private vulnerability reporting, Dependabot alerts and secret scanning with push protection.
- The release signing key lives outside the repository (an Actions environment secret limited to tags on `main`,
  with a required reviewer).
