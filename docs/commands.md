[← Docs](README.md)

# Command reference

Every command except `list`, `create` and `completions` takes an optional
`[SERVER]`: the name of a global profile. Without it, packctl uses the
`.packctl.toml` in the current directory. With a local profile, pass a
version with `--to` (a bare `packctl plan 8.2` reads `8.2` as a profile name).

Errors are printed as `error: <message>` on stderr. Exit codes: `0` success,
`2` a configuration or input error (a broken profile, an ambiguous version, a
refused adoption or update), `3` validation errors (`doctor`, `validate`),
`1` anything else: something not found (an unknown profile or version, also
when a bare `packctl plan 8.2` reads `8.2` as a profile name), network,
provider, controller, filesystem.

The help blocks below are the exact `--help` output of this version.

| Command | What it does |
| --- | --- |
| [`list`](#list) | List configured server profiles |
| [`create`](#create) | Create a new server profile interactively |
| [`adopt`](#adopt) | Record a hand-installed server as a pack version without changing it |
| [`status`](#status) | Show the current state of a server |
| [`versions`](#versions) | List available upstream versions for a server |
| [`plan`](#plan) | Show what an update would do without changing anything |
| [`update`](#update) | Apply an update |
| [`rollback`](#rollback) | Roll back to the previous successful version |
| [`doctor`](#doctor) | Check a server for common problems |
| [`validate`](#validate) | Validate the installed server against its recorded state |
| [`apikey`](#apikey) | Store, show, or remove the encrypted CurseForge API key for a server |
| [`completions`](#completions) | Print a shell completion script (bash, zsh, fish, powershell, elvish) |

## packctl

```text
$ packctl --help
Safely update self-hosted Minecraft modpack servers

Usage: packctl <COMMAND>

Commands:
  list         List configured server profiles
  create       Create a new server profile interactively
  adopt        Record a hand-installed server as a pack version without changing it
  status       Show the current state of a server
  apikey       Store, show, or remove the encrypted CurseForge API key for a server
  versions     List available upstream versions for a server
  plan         Show what an update would do without changing anything
  update       Apply an update
  rollback     Roll back to the previous successful version
  doctor       Check a server for common problems
  validate     Validate the installed server against its recorded state
  completions  Print a shell completion script (bash, zsh, fish, powershell, elvish)
  help         Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version

Docs: https://github.com/sennecools/packctl#readme
```

## list

Lists the global profiles as a table, and names the local `.packctl.toml` in the
current directory when there is one. Reads nothing but the profile files.

```console
$ packctl list
NAME       PROVIDER    PROJECT   SERVER ROOT
atm10      curseforge  925200    /srv/minecraft/atm10
cobblemon  modrinth    5FFgwNNP  /srv/minecraft/cobblemon
sb4        local       -         /srv/minecraft/sb4
```

```text
$ packctl list --help
List configured server profiles

Usage: packctl list

Options:
  -h, --help  Print help
```

## create

Writes a profile. Without `--global` it writes `.packctl.toml` into the current
directory (the server root); with `--global` it writes `<name>.toml` into the
profile directory (see [Configuration](configuration.md#local-and-global-profiles)).
Every prompt has a flag, so `-n` runs it unattended. When the server already has
mods installed, the wizard asks which version it runs and offers to
[adopt](adopting.md) it; `--adopt <VERSION|detect>` shows that dry run directly.

```bash
# CurseForge, AMP, fully scripted
packctl create atm10 --source 925200 --controller amp --instance ATM10 -n

# Modrinth (the provider is detected from the URL)
packctl create cobblemon --source https://modrinth.com/modpack/cobblemon-fabric \
  --controller command --status 'pgrep -f fabric-server-launch.jar' \
  --stop 'systemctl stop minecraft' --start 'systemctl start minecraft' -n

# Local server-pack zips dropped into ./packs
packctl create sb4 --provider local --controller amp --instance Stoneblock4 -n
```

The `--status`, `--stop` and `--start` values are split on spaces and run without
a shell. For anything that needs quoting or a shell, edit the argv arrays in the
profile afterwards (see [Controllers](configuration.md#controller)).

```text
$ packctl create --help
Create a new server profile interactively

Usage: packctl create [OPTIONS] [NAME]

Arguments:
  [NAME]
          Server profile name (defaults to the current directory name)

Options:
  -n, --non-interactive
          Do not prompt; provide every value as an option

  -f, --force
          Overwrite the profile if it already exists

      --global
          Write the profile into the global profile directory instead of the current directory

      --source <SOURCE>
          Modpack URL, project ID, or slug (CurseForge or Modrinth)

      --provider <PROVIDER>
          Pack provider (when omitted: detected from a CurseForge or Modrinth URL or a .zip/.mrpack path in --source; a bare project ID or slug means curseforge)

          Possible values:
          - curseforge: A CurseForge modpack
          - modrinth:   A Modrinth modpack
          - local:      Server-pack archives on this machine

      --archive <ARCHIVE>
          Local archive path (.zip or .mrpack file, or a directory of them) for --provider local; defaults to <server root>/packs

      --apikey <APIKEY>
          CurseForge API key to store (encrypted) with the new profile

      --root <ROOT>
          Server root directory (defaults to the current directory)

      --overlay <OVERLAY>
          Overlay directory (relative paths are resolved against the server root; default <root>/overlay)

      --controller <CONTROLLER>
          Server controller

          Possible values:
          - amp:     An AMP instance, driven through ampinstmgr
          - command: Custom status, stop and start commands

      --instance <INSTANCE>
          AMP instance name (for --controller amp)

      --status <STATUS>
          Status command (space-separated argv) for a command controller

      --stop <STOP>
          Stop command (space-separated argv) for a command controller

      --start <START>
          Start command (space-separated argv) for a command controller

      --timeout-ms <TIMEOUT_MS>
          Timeout in milliseconds for command-controller invocations

      --adopt <VERSION|detect>
          The server already runs this pack version (or "detect"): show what adopting it would do after the profile is written

  -h, --help
          Print help (see a summary with '-h')
```

## adopt

Records a server that already runs a pack version, without downloading it again
and without changing the pack folders. A dry run unless `--apply` is given. See
[Adopting an existing server](adopting.md) for what each class of file becomes.

```bash
packctl adopt --as "All the Mods 10-8.2"      # dry run: the report only
packctl adopt --detect                          # local and Modrinth packs: guess the version
packctl adopt --as 8.2 --apply                  # write overlay, .packctlignore and state
packctl adopt --as 8.2 --modified upstream --apply   # let the next update undo your edits
```

Exit codes: 2 when it refuses (state already exists, the server is running with
`--apply`, or most pack files are missing), 1 for other errors.

```text
$ packctl adopt --help
Record a hand-installed server as a pack version without changing it

Compares every file in the pack's folders with the given version: files that match are recorded, edited files and added mods go to the overlay, and added configs, runtime data and removed or disabled mods get .packctlignore rules. Nothing is written without --apply, and 'packctl rollback' undoes an applied adoption.

Usage: packctl adopt [OPTIONS] <--as <VERSION>|--detect> [SERVER]

Arguments:
  [SERVER]
          Server profile name (default: .packctl.toml in the current directory)

Options:
      --as <VERSION>
          Version the server currently runs (name, number or id)

      --detect
          Guess the version from the jars in mods/ (local and Modrinth packs only)

      --apply
          Write the overlay, .packctlignore and state (default: dry run)

  -y, --yes
          Do not ask for confirmation with --apply

  -j, --json
          Print the report as JSON

  -v, --verbose
          List every file

      --modified <MODIFIED>
          What to do with pack files you edited

          Possible values:
          - overlay:  Copy the edited file into the overlay (it wins over every update)
          - upstream: Record the pack's version; the next update writes it back
          - ignore:   Ignore the file; updates never touch it again

          [default: overlay]

      --extra-configs <EXTRA_CONFIGS>
          What to do with config files you added

          Possible values:
          - ignore:  Ignore them, so the mods that own them keep updating them
          - overlay: Freeze them in the overlay

          [default: ignore]

      --keep-removed
          Keep files the pack ships but the server lacks out of future updates

      --force
          Adopt even when state exists, most pack files are missing, or the server is running

  -h, --help
          Print help (see a summary with '-h')
```

## status

Local only: reads `.packctl/state.json` and asks the controller whether the
server runs. It never contacts a provider and never decrypts the API key.

```console
$ packctl status
demo
  Installed: demo-pack-1.1 (id demo-pack-1.1.zip)
  Provider: local
  Adopted: demo-pack-1.0 on 2026-11-18T18:05:24Z
  Last update: 2026-11-18T18:05:24Z
  Managed files: 7
  Snapshots: 2
  Server: Running
```

```text
$ packctl status --help
Show the current state of a server

Usage: packctl status [OPTIONS] [SERVER]

Arguments:
  [SERVER]  Server profile name (default: .packctl.toml in the current directory)

Options:
  -j, --json  Print the status as JSON
  -h, --help  Print help
```

## versions

Lists the versions the provider offers, newest first, with the id to pass to
`--to` or `--as`. CurseForge lists only versions that have a server pack;
Modrinth lists versions that ship an `.mrpack`; local lists the archives in the
drop folder, newest file first.

```console
$ packctl versions
Available versions for demo
  1. demo-pack-1.1 (id demo-pack-1.1.zip)
  2. demo-pack-1.0 (id demo-pack-1.0.zip)
```

CurseForge and Modrinth lines also show the Minecraft versions (and, for Modrinth, the loaders) in brackets, for
example `1. All the Mods 10-8.6 (id 9620701) [1.21.1]`.

`--json` prints an array of `{id, name, number, released, game_versions, loaders}`.

```text
$ packctl versions --help
List available upstream versions for a server

Usage: packctl versions [OPTIONS] [SERVER]

Arguments:
  [SERVER]  Server profile name (default: .packctl.toml in the current directory)

Options:
  -j, --json  Print versions as JSON
  -h, --help  Print help
```

## plan

Prepares the target version in staging and prints what an update would do. It
never stops the server and never writes to the server root. `plan` and `update`
share the same planner, so the preview is exactly what `update` applies.

```bash
packctl plan                       # latest version
packctl plan --to 8.2              # a version by number, name or id
packctl plan -v --to 8.2           # every file
packctl plan --json                # machine-readable (no server checks)
packctl plan atm10 "All the Mods 10-8.2"   # global profile + positional version
```

Version matching, in order: exact id, exact name (case-insensitive), exact
version number, then a unique partial match (a leading `v` is ignored). An
ambiguous query lists the candidates; an unknown one suggests close matches.

```text
$ packctl plan --help
Show what an update would do without changing anything

Usage: packctl plan [OPTIONS] [SERVER] [VERSION]

Arguments:
  [SERVER]   Server profile name (default: .packctl.toml in the current directory)
  [VERSION]  Target version name or id (prefer --to when using a local .packctl.toml)

Options:
      --to <VERSION>  Target version: an id, a name, or a unique part of a name such as 1.2.0 (defaults to the latest)
  -v, --verbose       Print the full file-level plan
  -j, --json          Print the plan as JSON (skips the server checks)
  -h, --help          Print help
```

## update

Prepares and prints the plan, asks for confirmation, then stops the server,
takes a snapshot, applies upstream files and the overlay, validates, starts the
server and commits the new state. An empty plan leaves the server alone; at most
it rewrites `state.json` when only the record is out of date.

```bash
packctl update                      # pick a version from a menu, confirm
packctl update --to 8.2             # a specific version, confirm
packctl update -n                   # latest, no prompts (cron, CI)
packctl update -n --force-sync      # first update of a server you chose not to adopt
```

Without a terminal, `update` refuses to run unless `-n` is given. It refuses a
plan that deletes or overwrites files packctl never installed, or writes an
overlay file over a live file that changed outside packctl, until you keep them
or pass `--force-sync`; on a server packctl has never recorded,
[adopt](adopting.md) it first (see
[the force-sync guard](safety.md#the-force-sync-guard)). Live edits to files
the pack installed are not protected: the update puts the pack's version back,
so keep such edits in the overlay.

```text
$ packctl update --help
Apply an update

Usage: packctl update [OPTIONS] [SERVER] [VERSION]

Arguments:
  [SERVER]   Server profile name (default: .packctl.toml in the current directory)
  [VERSION]  Target version name or id (prefer --to when using a local .packctl.toml)

Options:
      --to <VERSION>     Target version: an id, a name, or a unique part of a name such as 1.2.0 (defaults to interactive selection)
  -n, --non-interactive  Do not prompt; fail if confirmation would be required
  -v, --verbose          Print the full file-level plan before applying
      --force-sync       Apply the plan even when it deletes or overwrites files packctl never installed, or writes overlay files over live files that changed outside packctl
  -h, --help             Print help
```

## rollback

Restores the newest snapshot: stops the server, puts back the files the last
change touched (and removes the ones it added), restores `state.json`, the
overlay files and `.packctlignore` that adopt wrote, and starts the server.
Running it again steps back one more change.

```console
$ packctl rollback
Rolled back to demo-pack-1.0 (id demo-pack-1.0.zip)
Snapshot: /tmp/packctl-demo/server/.packctl/snapshots/2026-11-18T18-05-24Z-1
```

```text
$ packctl rollback --help
Roll back to the previous successful version

Usage: packctl rollback [SERVER]

Arguments:
  [SERVER]  Server profile name (default: .packctl.toml in the current directory)

Options:
  -h, --help  Print help
```

## doctor

Environment checks: the server root exists and is writable, a server entry file
(`run.sh`, `server.jar`, ...) is present, the overlay exists, the controller
answers, the provider can be set up, and, for CurseForge, which endpoint is used
and whether it answers. Exits with 3 when there are errors.

```console
$ packctl doctor
[warning] packctl has no record of this server yet; run 'packctl adopt --as <version>' before the first update
1 warning(s), 0 error(s)
All checks passed.
```

```text
$ packctl doctor --help
Check a server for common problems

Usage: packctl doctor [SERVER]

Arguments:
  [SERVER]  Server profile name (default: .packctl.toml in the current directory)

Options:
  -h, --help  Print help
```

## validate

Hashes every managed file and compares it with `state.json`, then runs the
local `doctor` checks (server root, entry files, overlay, controller, provider
setup). Use it to find files that changed outside packctl. Exits with 3 when
there are errors.

```console
$ packctl validate
0 warning(s), 0 error(s)
```

```text
$ packctl validate --help
Validate the installed server against its recorded state

Usage: packctl validate [SERVER]

Arguments:
  [SERVER]  Server profile name (default: .packctl.toml in the current directory)

Options:
  -h, --help  Print help
```

## apikey

Manages the encrypted CurseForge API key, per profile or (`--global`) shared by
every profile on the machine. Prefer the hidden prompt or `--stdin` over `--set`,
which leaves the key in your shell history.

```bash
packctl apikey --global                       # hidden prompt
printf '%s\n' "$KEY" | packctl apikey --global --stdin
packctl apikey --show                         # where the key comes from, last 4 characters
packctl apikey --remove
```

```console
$ packctl apikey --show
CurseForge API key: none
Endpoint: community proxy https://api.curse.tools (no API key configured)
```

```text
$ packctl apikey --help
Store, show, or remove the encrypted CurseForge API key for a server

Without --set, --stdin, --show or --remove, the key is read from a hidden prompt.

Usage: packctl apikey [OPTIONS] [SERVER]

Arguments:
  [SERVER]
          Server profile name (default: .packctl.toml in the current directory)

Options:
      --set <KEY>
          Store this API key (it ends up in shell history and process lists; prefer --stdin or the prompt)

      --stdin
          Read the API key to store from one line of standard input

      --show
          Show where the API key comes from and its last four characters

      --remove
          Remove the stored API key

      --global
          Manage the shared, machine-wide key used by every profile instead of a per-server key

  -h, --help
          Print help (see a summary with '-h')
```

## completions

Prints a completion script for your shell. Subcommands, flags and the values
of `--provider`, `--controller`, `--modified` and `--extra-configs` complete.

```bash
packctl completions bash > ~/.local/share/bash-completion/completions/packctl
packctl completions zsh > "${fpath[1]}/_packctl"
packctl completions fish > ~/.config/fish/completions/packctl.fish
```

```text
$ packctl completions --help
Print a shell completion script (bash, zsh, fish, powershell, elvish)

Write the script where your shell loads completions, for example:

  packctl completions bash > ~/.local/share/bash-completion/completions/packctl
  packctl completions zsh > "${fpath[1]}/_packctl"

Usage: packctl completions <SHELL>

Arguments:
  <SHELL>
          The shell to generate completions for

          [possible values: bash, elvish, fish, powershell, zsh]

Options:
  -h, --help
          Print help (see a summary with '-h')
```
