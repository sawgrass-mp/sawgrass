# Contributing

Sawgrass is built in the open by people and their coding agents. Anyone can contribute; only maintainers merge.
If you use an agent, point it at [AGENTS.md](AGENTS.md) first.

## Rules that keep the project safe to host

- **English only** in code, comments, docs, commits, issues and pull requests.
- **No game data, ever.** That covers archives, the executable, shaders, textures, models, audio, videos, byte
  signatures copied from the executable, and logs or reports that quote any of them. `.gitignore` blocks the
  common types and CI checks file contents, but check `git status` before you commit.
- **No keys, decryption or protection workarounds**, and no links to where to get them. Nothing that connects to
  Rockstar or Xbox services.
- Never commit secrets (tokens, keys, `.env` files).

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
`size: S` (an evening), `size: M` (a few days), `size: L` (several pull requests). Only maintainers add `ready`.

### Taking a task

1. Pick an open issue labelled `ready` and not `claimed`.
2. Comment `/claim`. The bot labels it and assigns you. You can hold two claims at a time.
3. Open a draft pull request with `Closes #N` within 72 hours, or the claim is released automatically.
   `/unclaim` releases it yourself.

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
- One task per pull request, ideally under 400 changed lines.
- Say whether it was AI-assisted, and what you ran.
- Review: CI first, then a maintainer listed in `.github/CODEOWNERS`. Squash merge only.

## Licence

Sawgrass is licensed under the GNU Affero General Public License v3.0 (see [LICENSE](LICENSE)). By contributing
you agree that your contribution is licensed under it.
