# Contributing to packctl

Thanks for helping out. Bug reports, fixes and docs improvements are all welcome.
For anything larger than a small fix, please open an issue first so we can agree
on the approach.

## Toolchain

- Rust stable, edition 2024. The minimum supported Rust version (MSRV) is
  **1.88** (`rust-version` in `Cargo.toml`); CI checks it with
  `cargo +1.88 check --locked --all-targets --all-features`.
- packctl targets Linux servers, but the test suite also runs on Windows and
  macOS in CI, so you can develop on any of them.
- Optional: [`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) and
  [`shellcheck`](https://www.shellcheck.net/) to run the same checks CI runs.

## Source layout

```text
src/
├── cli/            # one module per command, argument parsing, output
├── config/         # profiles (.packctl.toml) and encrypted secrets
├── controllers/    # AMP and command controllers
├── core/           # planner, executor, updater, adopt, snapshots, state, ignore rules
├── fs/             # path safety, extraction, hashing, copying
└── providers/      # CurseForge, Modrinth, local archives, verified downloads
```

## Before you open a pull request

Run the same gate CI runs:

```sh
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
```

And, if you have them installed:

```sh
cargo +1.88 check --locked --all-targets --all-features
cargo deny check
shellcheck -s sh install.sh scripts/demo/*.sh
```

### Tests

- **No network in tests.** Use the JSON fixtures under
  `src/providers/*/fixtures/`, zips built in a temporary directory, and the
  injectable fake `Fetcher` from `src/providers/http.rs`.
- **Sandbox your home directory.** packctl keeps profiles and keys under
  `PACKCTL_HOME`. Point it at a temporary directory when you run the binary by
  hand, so you never touch your real profiles:

  ```sh
  PACKCTL_HOME="$(mktemp -d)" cargo run -- list
  ```

  That does not sandbox a server: state, snapshots and the lock live in each
  server root's `.packctl/`, and a `.packctl.toml` in the current directory is
  used whatever `PACKCTL_HOME` says. Try changes in the demo fixture
  (`scripts/demo/setup.sh`) or a scratch directory without `.packctl.toml`,
  never in a real server directory.

- **Unix-only tests.** Tests for symlink handling, file permissions and the
  master-key file mode are gated with `#[cfg(unix)]`. They do not run on
  Windows, so if you change that code, check it on Linux or macOS (or let CI do
  it) and keep the gate on any new test that needs Unix semantics.
- Prefer `Path::join` over `/` literals in assertions about paths, so tests pass
  on every platform.

### Docs

`tests/docs.rs` keeps [README.md](README.md) and [`docs/`](docs/README.md) in
step with the code: every flag after a `packctl <command>` must exist, every
relative link and `#anchor` must resolve, every `gitignore` code block must
parse as `.packctlignore` rules, and the help blocks in
[`docs/commands.md`](docs/commands.md) must equal the real `--help` output.
After changing a help text, regenerate those blocks with:

```sh
PACKCTL_BLESS_DOCS=1 cargo test --test docs
```

Example output in the docs comes from real runs. The offline demo builds a
reproducible server for that:

```sh
cargo build && PATH="$PWD/target/debug:$PATH"
. scripts/demo/setup.sh        # leaves you in /tmp/packctl-demo/server
packctl status
```

The demo GIF is rendered from [`docs/demo.tape`](docs/demo.tape) by the manual
`demo` workflow; download its artifact and commit `docs/media/demo.gif`.

### Safety rules

packctl edits live game servers, so changes must keep these guarantees:

- Build everything in staging first; never touch the live server while
  preparing an update.
- Take a snapshot before any mutation, so `packctl rollback` can undo it.
- Route every untrusted path (archive entries, `.mrpack` indexes, profile
  values) through `fs::paths::normalize_relative` and `safe_join`.
- Size-check and hash-verify every download before it is renamed into place.

## Changelog

Every user-visible change needs an entry under `## [Unreleased]` in
[`CHANGELOG.md`](CHANGELOG.md), in the
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) style
(`Added`, `Changed`, `Fixed`, `Security`, ...). Internal refactors and test-only
changes do not need one.

## Commit messages

Short imperative subject lines ("Add Modrinth provider", "Fix overlay no-op"),
with a body when the reason is not obvious.

## Releasing (maintainers)

1. Bump `version` in `Cargo.toml` and run `cargo check` so `Cargo.lock` follows.
2. In `CHANGELOG.md`, rename `## [Unreleased]` content into a
   `## [X.Y.Z] - YYYY-MM-DD` section with today's date (a section prepared
   ahead of time is dated `Unreleased` until the tag is cut; replace that word
   with the date), add a fresh empty `## [Unreleased]`, and update the
   comparison links at the bottom.
3. Commit, then tag and push:

   ```sh
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

The `release` workflow then runs the full CI gate, checks that the tag matches
`Cargo.toml` and that `CHANGELOG.md` has a dated `## [X.Y.Z]` section, builds
the static musl binaries, attests them, and publishes the GitHub release with
the changelog section as its notes. A tag with a `-` (for example
`v0.3.0-rc.1`) is published as a pre-release.

Every push to `main` that passes CI also updates the `rolling` pre-release.
