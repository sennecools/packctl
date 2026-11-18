# Changelog

All notable changes to packctl are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Modrinth provider** (`provider = "modrinth"`). packctl downloads the
  `.mrpack`, verifies it, and builds the server side in staging: verified file
  downloads, then `overrides/`, then `server-overrides/`. Client-only files are
  skipped, and optional files are included unless `[pack] optional_files = "exclude"`.
  The required Minecraft and loader versions are shown in the plan, with a
  warning on a mismatch. Loaders are not installed. Live files that are already
  identical are reused instead of downloaded again.
- Local `.mrpack` files dropped in `packs/` go through the same pipeline.
- **`packctl adopt`** records an existing, hand-installed server as a known pack
  version without wiping it or downloading it again. Added mods and edited files
  move to the overlay, runtime data and removals get `.packctlignore` rules, and
  the result is a dry run until `--apply`. `packctl rollback` undoes it.
  `--detect` picks the version automatically for local and Modrinth packs.
- `packctl create --adopt <VERSION|detect>` adopts the server right after the
  profile is written.
- `--to <VERSION>` on `plan` and `update`, so a local `.packctl.toml` profile can
  target a specific version.
- Version arguments accept an id, an exact name, a version number or a unique
  partial match such as `8.2`. Ambiguous input lists the candidates, and unknown
  input suggests close matches.
- The plan warns about duplicate mods: the same mod id coming from the pack,
  the overlay or a kept live jar.
- `packctl completions <SHELL>` prints a shell completion script.
- `--json` output for `plan` and `status`.
- `packctl apikey --show` (source and a masked key) and `--stdin` (read the key
  from standard input instead of the command line).
- `packctl list` prints a table and names the local profile in the current
  directory.
- `packctl --version` includes the commit for rolling builds and the build
  target.
- `.packctlignore` protects runtime data from the pack-owned folder sweep.
- Example profiles for Modrinth (`examples/modrinth.toml`) and local packs
  (`examples/local.toml`).
- MIT `LICENSE`, `CONTRIBUTING.md`, `SECURITY.md`, issue forms and a pull
  request template.
- A rewritten README and a `docs/` folder (installation, configuration,
  providers, adopting, overlay, safety, internals and a full command
  reference), with examples from real runs of an offline demo
  (`scripts/demo/setup.sh`, recorded by `docs/demo.tape`).
- Release archives include `LICENSE` and `README.md`, carry GitHub build
  provenance attestations, and work with `cargo binstall`.
- The installer can verify the build attestation with
  `PACKCTL_VERIFY_ATTESTATION=1` (requires the GitHub CLI).

### Changed

- **Breaking: `update` refuses risky plans without `--force-sync`.** The first
  update of a server packctl has no record of is refused when it would delete
  or overwrite any file, and any update is refused when it would delete or
  overwrite files packctl never installed or write an overlay file over a live
  file that changed outside packctl. The check runs again after the server
  stops, and an update whose state changed after its plan was made is refused.
  Record an existing server with `packctl adopt` first, keep files with the
  overlay or `.packctlignore`, or pass `--force-sync` to converge onto the
  pack anyway.
- An update whose files are all in place but whose record is out of date
  rewrites only `state.json` (with a snapshot), without stopping the server.
- **CurseForge works without a personal API key.** With no key configured,
  metadata requests go to the community proxy `https://api.curse.tools`. Your own
  key is only ever sent to `api.curseforge.com` (or to a custom endpoint when you
  opt in with `PACKCTL_CF_PROXY_SEND_KEY=1`). Override the endpoint with
  `PACKCTL_CF_API_BASE` or `[pack] api_base`.
- CurseForge downloads are verified with the SHA-1 and size that CurseForge
  reports. A file without a SHA-1 is refused.
- Project ids are stored as strings, so Modrinth ids fit. Existing CurseForge
  profiles with a numeric `project_id` load and save unchanged.
- `--provider` and `--controller` on `create` are validated and listed in
  `--help`. The provider is detected from `--source` when omitted.
- Pack-owned folders are synced on every update, so the server converges on the
  pack.
- The local provider defaults to the server's `packs/` folder when no archive
  is configured.
- Errors print as `error: <message>` with exit code 2 for configuration
  errors, 3 for validation failures and 1 for everything else.
- `status` and `rollback` no longer build a provider, so they never decrypt the
  API key.
- A relative `--overlay` on `create` resolves against the server root.
- `plan` output: pluralized counts, overlay files already in sync are shown
  only with `--verbose`, and the required Minecraft and loader versions are
  listed.
- Minimum supported Rust version is 1.88.
- Upgraded `zip` to 9.x.

### Fixed

- The first `update` of a server packctl has never managed no longer deletes
  the admin's mods or overwrites edited configs. Identical files are left
  alone, differing files are planned as replacements, and the update refuses
  to run until the server is adopted or `--force-sync` is passed.
- An overlay no longer makes every update stop and restart the server. Overlay
  files already in place count as no change.
- An overlay file that shadows a pack file now reports every upstream change to
  that file, not just the first one.
- Ignored paths are no longer recorded as pack-managed files, and `validate`
  no longer reports them.
- CurseForge requests parse the real API envelope and pagination.
- Server packs wrapped in a single top-level folder extract correctly.
- Files packctl installed in a folder a newer pack version no longer ships (or
  under a wrapper folder an older packctl did not strip) are removed by the
  update instead of being left on disk and forgotten.
- A `.mrpack` is checked as a whole before any of its files are downloaded.
- Zip directory entries with trailing slashes are accepted.
- `rollback` explains when the restored state has no recorded version.
- Piping output into a reader that closes early (`packctl list | head -1`) no
  longer panics.
- A failing controller command reports a plain exit code (`exited with code 1`)
  or that it was terminated by a signal.

### Security

- All downloads go through one verified downloader: https only, size capped,
  hash checked before the file is renamed into place, and the partial file is
  removed on any error.
- Modrinth downloads must start on an allowlisted host (`cdn.modrinth.com`,
  `github.com`, `raw.githubusercontent.com`, `gitlab.com`, plus
  `staging-cdn.modrinth.com` only for packs from the staging API) and carry
  SHA-1, SHA-512 and an exact size. Paths in `modrinth.index.json` are checked
  like archive paths.
- Pack content can never write packctl's own files (`.packctl/`,
  `.packctl.toml`, `.packctlignore`), in any letter case.
- The CurseForge key is sent to the CurseForge CDN only over https, and never
  to a third-party redirect target.
- `apikey --stdin` and the hidden prompt keep the key out of shell history.
- The installer resolves `latest` without the rate-limited GitHub API, falls
  back to `shasum`, extracts only the binary and installs it with
  `install -m 0755`, so a root-owned directory never gets a user-owned binary.
- Release workflows run with read-only permissions by default; only the
  publish jobs can write releases and attestations.
- `cargo-deny` checks advisories, licenses and sources in CI.

## [0.1.0] - 2026-08-15

### Added

- First release: plan, update, rollback, status, versions, doctor and validate
  for self-hosted modpack servers.
- CurseForge server packs (with a personal API key) and local server-pack
  archives.
- AMP and command-based server controllers.
- Overlay directory, persistent data rules, snapshots and rollback.
- Encrypted per-profile and shared API key storage.
- Static Linux musl binaries for x86_64 and aarch64, an installer script with
  checksum verification, and a rolling build from `main`.

[Unreleased]: https://github.com/sennecools/packctl/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/sennecools/packctl/releases/tag/v0.1.0
