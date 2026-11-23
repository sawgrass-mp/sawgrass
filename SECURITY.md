# Security policy

Sawgrass ships a launcher and a game module that run on players' computers, so a malicious change can reach many
machines. Please report vulnerabilities privately.

## Reporting

Use GitHub's private vulnerability reporting: **Security → Report a vulnerability** on
[sawgrass-mp/sawgrass](https://github.com/sawgrass-mp/sawgrass/security/advisories/new). Do not open a public
issue. Include what is affected, how to reproduce it, and the impact you expect.

## What is in scope

- Code execution or file access on a player's machine through the launcher, the client module or downloaded
  server resources.
- Bypassing the server's authority (money, inventory, ownership) or crashing a server remotely.
- Anything that lets a pull request or issue run code with repository secrets, or tamper with releases.

## How releases are protected

- `main` changes only through reviewed pull requests; only maintainers merge.
- Release binaries are built by CI from tagged commits on `main` and signed with a key that is not in the
  repository. The launcher refuses unsigned updates.
- CI on pull requests runs without secrets and with read-only permissions.
