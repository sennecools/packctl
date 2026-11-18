[← Docs](README.md)

# Configuration

Each server is described by one TOML profile. `packctl create` writes it for you; this page describes every field.
Complete, commented examples: [`atm10.toml`](../examples/atm10.toml) (CurseForge, command controller on an AMP host),
[`modrinth.toml`](../examples/modrinth.toml) (Modrinth, systemd) and [`local.toml`](../examples/local.toml) (local
archives).

## Local and global profiles

**Local profile (the default).** `cd` into the server root and run `packctl create`. It writes `.packctl.toml` there,
with paths relative to that folder, and every later command finds it when you run it from that folder:

```bash
cd /srv/minecraft/atm10
packctl create atm10
packctl plan --to 8.2
```

**Global profile.** `packctl create <name> --global` writes `<name>.toml` into the profile directory instead, and
every command then takes the name: `packctl plan atm10`. Use this to manage several servers from one place. The
profile directory is the first of:

1. `$PACKCTL_HOME`
2. `$XDG_CONFIG_HOME/packctl`, by default `~/.config/packctl`
3. `./packctl` when no home directory is known

The file name (without `.toml`) is the profile name. `packctl list` shows the global profiles and the local profile in
the current directory.

With a global profile, a version can also be passed positionally (`packctl plan atm10 8.2`). With a local profile
use `--to`: `packctl plan 8.2` reads `8.2` as a profile name, and the error says so.

Relative paths in a profile resolve against the folder that holds the profile file.

## Fields

### Top level

| Field | Required | Meaning |
| --- | --- | --- |
| `name` | no | Display name. Defaults to the file name for global profiles. |

### `[server]`

| Field | Required | Meaning |
| --- | --- | --- |
| `root` | yes | The live server directory (the one with `mods/`, `config/`, `world/`). |

### `[pack]`

| Field | Required | Meaning |
| --- | --- | --- |
| `provider` | yes | `"curseforge"`, `"modrinth"` or `"local"`. |
| `project_id` | CurseForge, Modrinth | The project id. CurseForge ids are numbers (`925200`), Modrinth ids are strings (`"5FFgwNNP"`). A number or a string is accepted for either. |
| `slug` | no | The project slug (`all-the-mods-10`). For Modrinth it is used when `project_id` is absent. |
| `archive` | no | Local provider only: a `.zip` or `.mrpack` file, or a folder of them. Defaults to `<root>/packs`. |
| `api_base` | no | CurseForge only: the API base URL to use instead of the default choice (see [endpoint resolution](#curseforge-endpoint-resolution)). Must be `https://`, or `http://` for localhost. |
| `optional_files` | no | Modrinth packs (and local `.mrpack`s): `"include"` (default) installs files the pack marks optional on servers, `"exclude"` skips them. |

### `[overlay]`

| Field | Required | Meaning |
| --- | --- | --- |
| `path` | yes | The overlay directory, a mirror of the server root whose files win over the pack. `create` uses `<root>/overlay`. See [overlay.md](overlay.md). |

### `[controller]`

| Field | Required | Meaning |
| --- | --- | --- |
| `type` | yes | `"amp"` or `"command"`. |
| `instance` | for `amp` | The AMP instance name, as `ampinstmgr` knows it. |

### `[controller.command]`

Required when `type = "command"`.

| Field | Required | Meaning |
| --- | --- | --- |
| `status` | yes | argv array. Exit 0 means running, 1 means stopped, anything else is unknown. |
| `stop` | yes | argv array that stops the server. packctl then polls `status` until it reports stopped. |
| `start` | yes | argv array that starts the server. packctl then polls `status` until it reports running. |
| `timeout_ms` | no | Upper bound for each command and for the wait, in milliseconds. Default 120000. |

### `[secrets]`

| Field | Meaning |
| --- | --- |
| `api_key` | The encrypted CurseForge API key, written by `packctl apikey` or `create --apikey`. Never edit it by hand. |

## Controller

**AMP** drives the CubeCoders `ampinstmgr` command for the named instance. `stop` waits until AMP reports the instance
stopped before packctl touches any file.

**Command** runs your argv arrays directly, never through a shell, each bounded by `timeout_ms`. `packctl create`
splits `--status`, `--stop` and `--start` on spaces; for anything that needs quotes or shell features, call the shell
yourself in the profile. `systemctl is-active` exits 3 for a stopped unit, so map it to 1:

```toml
[controller]
type = "command"

[controller.command]
status = ["sh", "-c", "systemctl is-active --quiet minecraft || exit 1"]
stop = ["systemctl", "stop", "minecraft"]
start = ["systemctl", "start", "minecraft"]
timeout_ms = 180000
```

The commands run in the directory you run packctl from.

## API key storage

CurseForge works without a key (see [providers.md](providers.md#curseforge)). When you do have one, packctl looks for
it in this order and uses the first it finds:

1. the `CF_API_KEY` environment variable;
2. the key stored in the profile (`packctl apikey`, or `create --apikey`);
3. the shared key for every profile on the machine (`packctl apikey --global`).

Stored keys are encrypted with AES-256-GCM. The master key is created the first time you store a key, in the profile
directory as `.key` (mode 0600 on Unix), never inside a server root unless `PACKCTL_HOME` points there. This protects
the key from people who can read server files (backups, panel file managers), not from someone who can log in as your
user. The key is never printed: `packctl apikey --show` shows where it comes from and its last four characters.

```bash
packctl apikey --global           # hidden prompt; one key for every server
packctl apikey --stdin < key.txt  # per-profile key from a file or pipe
packctl apikey --show
packctl apikey --global --remove
```

## CurseForge endpoint resolution

For each request, packctl picks the CurseForge API endpoint in this order:

1. `PACKCTL_CF_API_BASE`, when set;
2. the profile's `[pack] api_base`;
3. the official API `https://api.curseforge.com`, when a key resolves (see above);
4. otherwise the community proxy `https://api.curse.tools`.

Your key is sent to the official API only. A custom endpoint from 1 or 2 receives it only when
`PACKCTL_CF_PROXY_SEND_KEY=1`, and the community proxy never does. `packctl doctor` and `packctl apikey --show` print
the endpoint in use.

## Environment variables

| Variable | Meaning |
| --- | --- |
| `CF_API_KEY` | CurseForge API key; wins over stored keys. |
| `PACKCTL_HOME` | The profile and key directory (instead of `$XDG_CONFIG_HOME/packctl`). |
| `XDG_CONFIG_HOME` | Base of the default profile directory. |
| `PACKCTL_CF_API_BASE` | CurseForge API base URL for every profile, for example a self-hosted proxy. |
| `PACKCTL_CF_PROXY_SEND_KEY` | `1` sends your CurseForge key to a custom `PACKCTL_CF_API_BASE` / `api_base` too. |
| `PACKCTL_MODRINTH_API_BASE` | Modrinth API base URL, for a mirror or a test server. Defaults to `https://api.modrinth.com/v2`. |
