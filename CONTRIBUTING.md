# Contributing to Remora

Thanks for your interest! This document covers the practicalities; the project
direction lives in [docs/VISION.md](docs/VISION.md).

## Before you start

- For anything beyond a small fix, **open an issue first** (or comment on an
  existing one) so we can agree on the approach before you invest time.
- Check [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) — in particular, UI code
  never talks to ssh/kubectl directly; it goes through `SessionSource`.

## Development setup

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for prerequisites and commands.

## Pull requests

- Keep PRs focused: one logical change per PR.
- PR titles must follow [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat: …`, `fix: …`, `docs: …`, `chore: …`, `refactor: …`, `test: …`,
  `ci: …`). CI enforces this; PRs are squash-merged, so the PR title becomes
  the commit message.
- Add tests for behavior you add or change.
- CI must be green: Biome (lint + format), `tsc`, Vitest, `cargo fmt --check`,
  `cargo clippy -D warnings`, `cargo test`.

Run everything locally before pushing:

```sh
pnpm lint && pnpm typecheck && pnpm test
cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
```

## Code style

- **TypeScript:** formatted and linted by [Biome](https://biomejs.dev)
  (`pnpm lint:fix` to auto-fix). No manual style debates — Biome is the
  arbiter.
- **Rust:** `rustfmt` defaults; `clippy` warnings are errors in CI.
  `unsafe_code` is denied workspace-wide.
- Significant architectural decisions are recorded as ADRs in
  [docs/adr/](docs/adr/). If your change reverses or extends one, add a new
  ADR rather than editing history.

## Licensing of contributions

Remora is licensed under [AGPL-3.0-only](LICENSE). By submitting a
contribution you agree that it is your own work (or that you have the right to
submit it) and that it is licensed under the same terms — the usual
"inbound = outbound" model. There is no CLA.

## Reporting bugs and requesting features

Use the issue templates. For security vulnerabilities, follow
[SECURITY.md](SECURITY.md) instead of opening a public issue.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). Be kind.
