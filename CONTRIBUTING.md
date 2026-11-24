# Contributing

Sawgrass is built in the open by people and their coding agents. Anyone can contribute; only maintainers merge.
Contributions are written with **Claude Opus 5.5 or Claude Fable** (see below); point your agent at
[AGENTS.md](AGENTS.md) first.

## Rules that keep the project safe to host

- **Claude Opus 5.5 or Claude Fable only.** Every contribution is written with Claude Opus 5.5 or Claude Fable
  (any Fable version). Contributions made with other AI agents or models are not accepted, for the sake of code
  quality and performance. Declare the model on the `Model:` line of the pull request template. GitHub cannot
  verify it, so it is a declaration: a false one gets the pull request closed, and the CI and review gates apply
  either way. Pull requests from the maintainer's own account need no `Model:` line: the maintainer runs only
  Claude Opus 5.5 or Claude Fable agents on it, and this sentence is the declaration for all of them.
- **English only** in code, comments, docs, commits, issues and pull requests.
- **No game data, ever.** That covers archives, the executable, shaders, textures, models, audio, videos, byte
  signatures copied from the executable, and logs or reports that quote any of them. `.gitignore` blocks the
  common types and CI checks file contents, but check `git status` before you commit.
- **No keys, decryption or protection workarounds**, and no links to where to get them. Nothing that connects to
  Rockstar or Xbox services.
- Never commit secrets (tokens, keys, `.env` files).
- **Stay in the task's scope.** The `pr-guard` check fails a pull request that changes protected files (CI,
  scripts, rules, launcher, client) outside the linked task's scope, adds risky workflow features, deletes or
  rewrites most of a prominent file such as `README.md`, or runs commands through a shell. See
  [AGENTS.md](AGENTS.md#protected-changes).

## How work is organised

Every task is a GitHub issue with an ID in its title. The prefix names the area:

| Prefix | Area |
|---|---|
| `NET-` | protocol, transport, gateways |
| `SRV-` | world simulation, entity ownership |
| `IM-` | interest management (who sees what) |
| `DB-` | storage and economy |
| `SCRIPT-` | scripting runtime and API |
| `RP-` | roleplay resources in `resources/` |
| `BOT-` | load-test bots and benchmarks |
| `CLIENT-` | the game-side module |
| `LAUNCH-` | the launcher |
| `OPS-` | CI, releases, repository tooling |
| `DOC-` | documentation |

Labels: `ready` (triaged, can be claimed), `claimed`, `triage` (new, not yet reviewed), `good first task`, and
`size: S` (an evening), `size: M` (a few days), `size: L` (several pull requests). Only maintainers add `ready`;
the dispatcher adds it automatically to every task a maintainer opens.

### Taking a task

1. Pick an open issue labelled `ready` and not `claimed`.
2. Comment `/claim` (the first line of the comment). The bot labels it and assigns you. You can hold two
   claims at a time. A maintainer's own pull request claims its task automatically.
3. Open a draft pull request with `Closes #N` within 72 hours, or the claim is released automatically.
   `/unclaim` releases it yourself.
4. Mark the pull request ready for review when the work is done and CI is green. Drafts are not reviewed.

### Reporting something you won't fix

Open an issue with the **Finding** template: what, where (file and line), why it matters. It gets the `triage`
label; a maintainer turns it into a task.

### Proposing a design change

Changes to [docs/architecture.md](docs/architecture.md) go through an ADR in `docs/adr/` in its own pull
request, before the code that depends on it.

## Working on the code

- Rust stable, pinned in `rust-toolchain.toml`. Edition 2024.
- Before pushing, run the checks listed in [AGENTS.md](AGENTS.md#checks-to-run-before-every-push).
- New dependencies must pass `cargo deny check` and be justified in the pull request.
- Tests that need the game read its location from `SAWGRASS_GAME_DIR` and print `SKIP` when it is unset. Never
  copy game files into the repository or into test fixtures.
- Performance work reports numbers: tick time p50/p99 and bytes per player per second, before and after.

## Pull requests

- Title starts with the task ID (`IM-3: Add hysteresis to the visible set`).
- One task per pull request, ideally under 400 changed lines (added plus deleted).
- Declare the model (`Model: Claude Opus 5.5` or `Model: Claude Fable`; the maintainer's own pull requests are
  declared once in the rules above) and say what you ran.
- Review: CI first, then a maintainer listed in `.github/CODEOWNERS`. Squash merge only.

## Licence

Sawgrass is licensed under the GNU Affero General Public License v3.0 (see [LICENSE](LICENSE)). By contributing
you agree that your contribution is licensed under it.
