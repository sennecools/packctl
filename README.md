# packctl

`packctl` is a Rust CLI for safely updating self-hosted Minecraft modpack servers. Upgrading a server pack (for example a CurseForge server pack) usually wipes your admin mods, custom config, and KubeJS changes. `packctl` treats a running server as three distinct layers — the upstream modpack, your local overlay, and persistent runtime data — and updates only what belongs to the modpack, so your customizations and world survive every upgrade.

V1 targets CurseForge server packs (or local server-pack archives) on Linux, with two server controllers: CubeCoders AMP and a generic command-based controller.

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

Requires Linux (x86_64 or arm64). The quickest way is the installer script, which downloads the latest prebuilt binary from [GitHub Releases](https://github.com/sennecools/packctl/releases):

```bash
curl -fsSL https://raw.githubusercontent.com/sennecools/packctl/main/install.sh | sh
```

As a normal user it installs to `~/.local/bin`; run it with `sudo` (or as root) to install to `/usr/local/bin`. The script passes the release and install directory positionally to `sh`:

```bash
# latest stable release (default)
curl -fsSL https://raw.githubusercontent.com/sennecools/packctl/main/install.sh | sh
# a specific release into a custom directory
curl -fsSL https://raw.githubusercontent.com/sennecools/packctl/main/install.sh | sh -s v0.1.0 "$HOME/bin"
```

Every commit to `main` is built automatically and published to a `rolling`
prerelease. To install the very latest commit build:

```bash
curl -fsSL https://raw.githubusercontent.com/sennecools/packctl/main/install.sh | sh -s rolling
```

Stable tagged releases remain the default (`latest`).

Binaries are statically linked (musl), so they run on any glibc-based Linux.

Or build from source with a Rust toolchain:

```bash
cargo build --release
# binary: target/release/packctl
sudo install -m 0755 target/release/packctl /usr/local/bin/packctl
```

The CurseForge API requires a free API key. `packctl` reads it from the
`CF_API_KEY` environment variable and never writes it in plaintext to logs or
state. Get one (one-time, free) at <https://console.curseforge.com/>:

```bash
export CF_API_KEY="..."
```

Without a key, the network commands (`versions`, `plan`, `update`) fail with a
message explaining where to get one. Creating a profile from a numeric project
id still works without a key.

## Configuration

Each server is one TOML profile. The easiest setup lives **inside the server
root**: `cd` into the server directory and run `packctl create`. It writes a
`.packctl.toml` file there, and every later command just works from that
directory without passing a server name:

```bash
cd /amp/instances/AllTheMods10
packctl create atm10
```

`packctl create` asks for the CurseForge modpack (a URL like
`https://www.curseforge.com/minecraft/modpacks/all-the-mods-10`, a numeric
project id, or a slug), an optional API key, and how the server process is
controlled. The server root defaults to the current directory and the overlay
to `<server root>/overlay`, so the instance stays fully visible to your
control panel without extra virtual directories:

```text
Server profile name [AllTheMods10]: atm10
Server root [/amp/instances/AllTheMods10]:
Overlay directory [/amp/instances/AllTheMods10/overlay]:
CurseForge modpack URL or project ID: 925200
CurseForge API key (optional, stored encrypted): <typed or empty>
Found 'project 925200' (project 925200)? [Y/n] y
Server controller: amp
AMP instance name [atm10]: ATM10

Created profile 'atm10'
  file:       /amp/instances/AllTheMods10/.packctl.toml
  pack:       project 925200 (project 925200)
  server:     /amp/instances/AllTheMods10
  overlay:    /amp/instances/AllTheMods10/overlay
  controller: amp (instance ATM10)
  api key:    stored (encrypted)

Next: packctl status
```

The generated file uses relative paths, so it stays valid if the instance
directory moves:

```toml
name = "atm10"

[server]
root = "."

[pack]
provider = "curseforge"
project_id = 925200

[overlay]
path = "overlay"

[controller]
type = "amp"
instance = "ATM10"
```

Every value can be supplied as a flag instead of a prompt for scripting, e.g.:

```bash
packctl create atm10 --source 925200 --apikey "$CF_API_KEY" \
  --controller command \
  --status "pgrep -f server.jar" \
  --stop "screen -S atm10 -X stuff \"stop\n\"" \
  --start "screen -S atm10 -X stuff \"start\n\""
```

### API key storage

The CurseForge API requires a free key (<https://console.curseforge.com/>).
`packctl` reads it from `$CF_API_KEY` if set, otherwise from the profile. When
you store it in the profile it is **encrypted at rest** with AES-256-GCM; the
per-user decryption key lives in `$PACKCTL_HOME/.key`, or otherwise
`$XDG_CONFIG_HOME/packctl/.key` (default `~/.config/packctl/.key`), with mode
0600 on Unix. It is never inside the server root unless you explicitly point
`PACKCTL_HOME` there, so a plain API key is not readable from backups or
control panels. Manage it with:

```bash
packctl apikey            # prompt to store a key
packctl apikey --set "..."
packctl apikey --remove
```

The master key is created the first time you store a secret. Because it
protects only against *file readers* — anyone with shell access as your user
can read the `.key` file — the decrypted key never appears in logs or output.

**One key for every server.** Instead of storing a key per profile, store it
once as a shared, machine-wide key and every profile uses it unless it has its
own:

```bash
packctl apikey --global --set "..."
packctl apikey --global --remove
```

Resolution order per profile: `CF_API_KEY` env var → key stored in the profile
→ shared global key → error. So a single CurseForge API key (the server
administrator's) covers all instances on the machine; nobody else needs one.

### Global profiles

If you prefer one profile directory for many servers, pass `--global` to
`create` (writes `<name>.toml` to the profile directory, resolved in this
order) and use the server name with every command:

1. `$PACKCTL_HOME` if set
2. `$XDG_CONFIG_HOME/packctl`, or `~/.config/packctl` by default
3. `./packctl` (relative to the current directory) as a fallback

The file name (minus `.toml`) is the profile name; the optional `name` field
overrides the display name. A complete commented example lives at
[`examples/atm10.toml`](examples/atm10.toml). The fields:

| Field | Meaning |
| --- | --- |
| `name` | Optional display name; defaults to the file name. |
| `[server] root` | The live server root directory. Relative paths resolve against the profile directory. |
| `[pack] provider` | Pack provider. `"curseforge"` or `"local"` in V1. |
| `[pack] project_id` | The provider's project ID (a number). Required for CurseForge. |
| `[pack] slug` | Optional human-friendly identifier. |
| `[pack] archive` | Optional. For `provider = "local"`: a zip file or a directory of zips. Defaults to `<server root>/packs` when omitted. |
| `[overlay] path` | Directory of the mirrored local overlay. Relative paths resolve against the profile directory. |
| `[controller] type` | `"amp"` or `"command"`. |
| `[controller] instance` | AMP instance name (required when `type = "amp"`). |
| `[controller.command]` | Required when `type = "command"`: `status`, `stop`, and `start` argv arrays plus optional `timeout_ms`. |

### Local archive provider

`provider = "local"` follows a server pack you already have as a zip file, so
updates need no CurseForge API key and no network access. Each archive is one
version (named by its file stem). By default the pack is read from the `packs/`
folder **inside the server root**: download the server-pack zip from the
modpack author, drop it into `packs/`, and run `packctl update`. Create the
profile non-interactively with:

```bash
packctl create sb4 --provider local \
  --controller amp --instance Stoneblock401 --non-interactive
```

`create` makes the `packs/` drop folder automatically. To keep archives
somewhere else instead, pass `--archive <path>` (a single zip or a directory
of zips), in which case the most recently modified zip is "latest".

A local profile looks like:

```toml
name = "sb4"

[server]
root = "."

[pack]
provider = "local"

[overlay]
path = "overlay"

[controller]
type = "amp"
instance = "Stoneblock401"
```

`packctl versions`, `plan`, `update`, and `rollback` behave exactly as with a
CurseForge pack. To update to a new version, drop a newer zip into `packs/`
(or edit `archive` to point at another zip or folder) and run
`packctl update` — the newest zip is treated as "latest".

### Controllers

- **AMP** (`type = "amp"`): drives the CubeCoders `ampinstmgr` CLI for the named instance. `stop` uses `--wait` and confirms the instance actually reports `stopped` before continuing. If present, `controller.command.timeout_ms` also bounds AMP calls; otherwise the default is 120000 ms.
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
| `packctl create <server> [--source <url\|id\|slug>] [--provider <curseforge\|local>] [--archive <path>] [--apikey <key>] [--global] ...` | Interactively create a server profile. Defaults to a local `.packctl.toml` in the current directory; `--global` writes to the profile directory instead. `--provider local` follows a server-pack zip dropped into `<server root>/packs` (no API key needed); `--archive <path>` points elsewhere. `-n`/`--non-interactive` requires every value as a flag; `-f`/`--force` overwrites. |
| `packctl status <server>` | Show installed version, last update, managed-file count, snapshot count, and controller status. Local only, no network. |
| `packctl apikey <server> [--set <key>] [--remove] [--global]` | Store, show, or remove the encrypted CurseForge API key. `--global` stores one shared key used by every profile instead of a per-server key. |
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
