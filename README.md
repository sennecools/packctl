# packctl

`packctl` is a Rust CLI for safely updating self-hosted Minecraft modpack servers. Upgrading a server pack (for example a CurseForge server pack) usually wipes your admin mods, custom config, and KubeJS changes. `packctl` treats a running server as three distinct layers — the upstream modpack, your local overlay, and persistent runtime data — and updates only what belongs to the modpack, so your customizations and world survive every upgrade.

V1 targets CurseForge server packs on Linux, with two server controllers: CubeCoders AMP and a generic command-based controller.

## How it works

### The three-layer model

```text
UPSTREAM MODPACK
       +
LOCAL OVERLAY
       +
PERSISTENT RUNTIME DATA
       =
RUNNING SERVER
```

- **Upstream modpack** — what the pack author ships. `packctl` manages whole modpack versions, not individual mods.
- **Local overlay** — a mirrored directory of files that always win over upstream.
- **Persistent runtime data** — world, logs, whitelist, and friends that must never be replaced by an update.

### Update flow

An update runs through these steps, in this order:

```text
stop server
    ↓
snapshot
    ↓
apply upstream changes
    ↓
apply overlay
    ↓
validate
    ↓
start server
    ↓
commit new state
```

Preparation happens entirely before this: the target version is resolved, downloaded, and extracted into a **staging** directory, the overlay is scanned, and an exact plan is built. The live server is never touched during preparation, and `packctl plan` and `packctl update` use the same planner. If nothing changed, the plan is empty and the server is never stopped.

### What `packctl` deliberately does not do

- **No config merging.** There is no "smart" merge of TOML, JSON, YAML, SNBT, KubeJS, or properties files. The model is upstream first, overlay second.
- **Unknown files are never deleted.** A file can only be removed if the updater can prove the previous upstream version managed it, the new version dropped it, and it is neither persistent nor overlay-provided. Anything else stays.
- **Persistent runtime data is never touched.** `world/` and the other persistent paths below are excluded from the update plan entirely.
- It is not a launcher, a modpack authoring tool, a generic mod updater, a hosting panel, a world backup system, or a replacement for AMP. It manages safe upstream modpack upgrades for self-hosted servers.

## Installation

Requires Linux and a Rust toolchain. Build and install:

```bash
cargo build --release
# binary: target/release/packctl
```

Optionally copy it into your `PATH`:

```bash
sudo install -m 0755 target/release/packctl /usr/local/bin/packctl
```

The CurseForge API key is optional — public API reads work without one. If you run into rate limits, export it:

```bash
export CF_API_KEY="..."
```

The key is read from the environment only and never written to logs or state.

## Configuration

Each server is one TOML profile. Create `<name>.toml` (for example `atm10.toml`) in the profile directory, which is resolved in this order:

1. `$PACKCTL_HOME` if set
2. `$XDG_CONFIG_HOME/packctl`, or `~/.config/packctl` by default
3. `./packctl` (relative to the current directory) as a fallback

The file name (minus `.toml`) is the profile name you pass to commands; the optional `name` field overrides the display name.

A complete commented example lives at [`examples/atm10.toml`](examples/atm10.toml). The fields:

| Field | Meaning |
| --- | --- |
| `name` | Optional display name; defaults to the file name. |
| `[server] root` | The live server root directory. Relative paths resolve against the profile directory. |
| `[pack] provider` | Pack provider. Only `"curseforge"` is supported in V1. |
| `[pack] project_id` | The provider's project ID (a number). |
| `[pack] slug` | Optional human-friendly identifier. |
| `[overlay] path` | Directory of the mirrored local overlay. Relative paths resolve against the profile directory. |
| `[controller] type` | `"amp"` or `"command"`. |
| `[controller] instance` | AMP instance name (required when `type = "amp"`). |
| `[controller.command]` | Required when `type = "command"`: `status`, `stop`, and `start` argv arrays plus optional `timeout_ms`. |

### Controllers

- **AMP** (`type = "amp"`): drives the CubeCoders `ampinstmgr` CLI for the named instance. `stop` uses `--wait` and confirms the instance actually stopped before continuing.
- **Command** (`type = "command"`): runs the `status`/`stop`/`start` argv arrays directly — never through a shell. Each invocation is bounded by `timeout_ms` (default 120000 ms). A `status` command is expected to exit `0` when running and `1` when stopped.

## The overlay

The overlay is a plain directory whose structure mirrors the server root:

```text
overlay/
├── mods/
│   └── grieflogger.jar
├── config/
│   └── MiniMOTD/
│       └── main.conf
└── server.properties
```

Files are copied over the upstream installation after every update. **The overlay always wins**: if the upstream pack also contains `config/MiniMOTD/main.conf`, the server ends up with your overlay version.

When the upstream content of a path that your overlay replaces changed since the last update, the plan surfaces an informational notice:

```text
Overlay conflict notice

~ config/example.toml
  Upstream changed this file.
  Local overlay replaces it.
  Overlay version will be used.
```

This is informational, not an error. `server.properties` is persistent by default, but it is treated as a managed file when it is present in the overlay — put it there and the updater applies your version and records it.

## Persistent runtime data

The updater never plans changes for these paths, even when a new modpack version ships them:

```text
world/            (whole directory tree)
logs/
backups/
crash-reports/
server.properties   (persistent by default; managed if present in the overlay)
ops.json
whitelist.json
banned-players.json
banned-ips.json
usercache.json
```

Everything else under the server root is treated as updater-managed content.

## Commands

| Command | Description |
| --- | --- |
| `packctl list` | List configured server profiles. |
| `packctl status <server>` | Show installed version, last update, managed-file count, snapshot count, and controller status. Local only, no network. |
| `packctl versions <server> [--json]` | List available upstream versions. `-j`/`--json` prints an array of `{id, name, released}`. |
| `packctl plan <server> [version] [--verbose]` | Preview an update without changing anything. `-v`/`--verbose` lists every file. |
| `packctl update <server> [version] [--non-interactive] [--verbose]` | Apply an update. Interactive prompts select the version (when omitted) and confirm before applying. |
| `packctl rollback <server>` | Restore the latest snapshot (the previous successful version). |
| `packctl doctor <server>` | Run environment checks: root exists and is writable, entry files present, overlay present, controller usable. Exits non-zero on errors. |
| `packctl validate <server>` | Verify installed managed files match the recorded state (hash check) plus the `doctor` checks. Exits non-zero on errors. |

When you run `update` interactively, the target version defaults to a numbered menu with a `(latest)` option; without a version in non-interactive mode, the latest version is used. `update` requires confirmation before mutating anything, so when stdin is not a terminal you must pass `--non-interactive` (otherwise the command refuses to run). `plan` is always read-only.

## Safety model

Safety is the top priority. The guarantees:

- **Staging before mutation** — downloads, extraction, and preparation happen in a staging directory, never in the live server.
- **Snapshot before mutation** — before any live file changes, the exact files the plan touches are copied to `server_root/.packctl/snapshots/<timestamp>/` along with a manifest and the previous `state.json`.
- **Commit state last** — the new version is recorded only after files, overlay, and validation all succeeded. A failed update never leaves state claiming the new version is installed.
- **Only previously-managed files can be removed** — removals require proof of prior upstream ownership, plus the file being absent from the new pack, non-persistent, and not overlay-provided.
- **Unknown and persistent files are never touched** — untracked files have no plan entry and survive; persistent paths never enter the plan.
- **Path safety** — archive, provider, config, and overlay paths are treated as untrusted. Traversal (`..`), absolute paths, empty components, and NUL bytes are rejected, and symlinks are not followed during destructive operations.
- **No-op plans do nothing** — if the plan is empty, the server is not stopped, nothing is snapshotted, and nothing is committed.

## Recovering from a failed update

A failure during the mutation phase aborts the update and prints the rollback snapshot location. The new version is not committed. To return to the previous successful version:

```bash
packctl rollback <server>
```

Rollback restores the latest snapshot: it stops the server, restores the captured managed files (removing anything the failed update added), and starts the server again.

If something looks wrong outside of an update, `packctl doctor <server>` and `packctl validate <server>` report what is broken — for example a missing or tampered managed file, a missing overlay, or an unusable controller.

## Testing

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```
