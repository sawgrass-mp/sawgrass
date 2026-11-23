Closes #<!-- the task's issue number -->

Depends on: <!-- #N for every pull request this one builds on or conflicts with; delete the line if none -->

### What and why

<!-- What this changes and the problem it solves. Cite the ADR or section of docs/architecture.md it follows. -->

### Tested

<!-- The commands you ran and their results. For performance work: tick p50/p99 and bytes per player, before
     and after. -->

AI-assisted: <!-- yes or no -->

### Checklist

- [ ] The title starts with the task ID (`NET-12: …`).
- [ ] English only; no game data, keys or decryption, including in logs quoted here.
- [ ] `cargo fmt`, `cargo clippy -D warnings` and `cargo test` pass locally.
- [ ] New dependencies, if any, are justified above and pass `cargo deny check`.
