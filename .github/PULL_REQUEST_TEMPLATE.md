## What

<!-- What does this PR change? One or two sentences. -->

## Why

<!-- Link the issue this addresses (e.g. "Closes #123"), or explain the
     motivation if there isn't one. For non-trivial changes, please open an
     issue first — see CONTRIBUTING.md. -->

## Checklist

- [ ] PR title follows [Conventional Commits](https://www.conventionalcommits.org/) (it becomes the squash commit message)
- [ ] Tests added/updated for changed behavior
- [ ] `pnpm lint && pnpm typecheck && pnpm test` passes
- [ ] `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` passes
- [ ] New architectural decisions are recorded as an ADR in `docs/adr/` (if applicable)
