## What and why

<!-- What does this change, and what problem does it solve? Link the issue: "Fixes #123". -->

## Checklist

- [ ] `cargo fmt --all --check` passes
- [ ] `cargo clippy --locked --all-targets --all-features -- -D warnings` passes
- [ ] `cargo test --locked --workspace --all-features` passes
- [ ] New behavior has tests, and the tests do not use the network
- [ ] Untrusted paths go through `normalize_relative` / `safe_join`; nothing touches the live server before the snapshot
- [ ] `CHANGELOG.md` has an entry under `## [Unreleased]` (user-visible changes only)
- [ ] Docs and `--help` text are updated where behavior changed
