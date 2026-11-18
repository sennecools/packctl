[← Docs](README.md)

# Adopting an existing server

Most servers were set up by hand: someone unzipped a server pack, added a few mods, changed some configs and started
it. packctl has no record of such a server, so its first update would treat every one of those changes as drift from
the pack. `packctl adopt` records the server as the pack version it runs, keeps your changes in the overlay and in
`.packctlignore`, and after that `packctl plan` shows only what the next version really changes.

```bash
packctl adopt --as "All the Mods 10-8.2"            # dry run: the report only
packctl adopt --as "All the Mods 10-8.2" --apply    # record it (asks first; -y skips the question)
packctl plan                                        # now shows the real update
```

`--as` takes anything `--to` does: an id, a name, a version number or a unique part of a name. Stop the server
before `--apply`, so files do not change while they are copied (or pass `--force`).

## What adopt does

packctl prepares the named version in staging, exactly as an update would, and compares every file in the pack's
top-level folders (`mods/`, `config/`, `kubejs/`, ...) with it. Files are matched by path, size and SHA-256; jars are
also matched by mod id. Nothing outside those folders is read, and the pack folders themselves are never modified.

| Class | Report line | Default action | Flag that changes it |
| --- | --- | --- | --- |
| Same as the pack | `= path` under *Matches the pack* | recorded as installed | |
| Only comments, blank lines or trailing whitespace differ | `≈ path` under *Reformatted only* | recorded; your copy is accepted as-is until the pack changes the file | |
| Edited pack file | `~ path` under *Edited by you* | copied into the overlay | `--modified upstream` records the pack's version (the next update writes it back); `--modified ignore` adds an ignore rule |
| Added mod, KubeJS script, `scripts/` or `defaultconfigs/` file | `+ path` under *Added by you* | copied into the overlay | |
| Added config or other file | `+ path` under *Added by you* | `.packctlignore` rule, so the mod that wrote it keeps updating it | `--extra-configs overlay` freezes `config/` files in the overlay |
| Pack mod swapped for another jar of the same mod | `~ old → new` under *Replaced by you* | new jar into the overlay, ignore rule for the old one | |
| Pack mod renamed to `.disabled` | `- jar (disabled as ...)` under *Disabled by you* | ignore rules for the jar and the disabled file | |
| Pack mod missing from the server | `- path` under *Removed by you* | ignore rule, so it stays removed | |
| Other pack file missing from the server | `- path` under *Missing* | restored by the next update | `--keep-removed` adds an ignore rule instead |
| Runtime data: `.db`, `.sqlite`, `.h2`, `.mv.db`, `.lock`, `.log`, `.bak`, `.old`, `.tmp`, `.dat`, or any non-jar over 16 MiB | `+ path` under *Runtime data* | ignore rule | |
| Persistent data the pack ships (`world/`, `server.properties`, ...) | under *Persistent (untouched)* | nothing | |

A folder that only holds ignored additions becomes one rule (`config/spark` rather than every file in it). A path
already in your overlay is never overwritten: identical files show `already in overlay`, different ones
`CONFLICT: overlay differs, update would overwrite` and are counted under *Overlay conflicts*. For a conflict the
overlay file is kept and the live file is left as it is, so the next update would write the overlay file over your live
edit. packctl records this as live drift and refuses that update without `--force-sync`: copy the live file into the
overlay (or delete the overlay file) to reconcile them first.

adopt refuses, with exit code 2, when:

- the server already has packctl state;
- fewer than half of the pack's files are on the server (probably the wrong version);
- the server is running and you pass `--apply`;
- with `--apply`, a runtime file would stay behind that no `.packctlignore` rule can protect, so the next update would
  delete it (rename or move it first);
- with `--apply`, adopt has files to copy into an overlay outside the server root, where `packctl rollback` could not
  remove them again. A profile like [`examples/atm10.toml`](../examples/atm10.toml), whose overlay is
  `/srv/packctl/overlay`, hits this when anything goes to the overlay; keep the overlay under the server root, or pass
  `--force` and remove those copies by hand if you ever roll back.

`--force` overrides all five.

## A full run

This is the offline demo from [`scripts/demo/setup.sh`](../scripts/demo/setup.sh): `demo-pack-1.0` installed by hand,
plus the spark mod (and the config folder it wrote), an edited backpack config and the grieflogger mod, which is
already in the overlay.

```console
$ packctl adopt --as demo-pack-1.0 --apply -y
demo
Adopting as demo-pack-1.0 (id demo-pack-1.0.zip)
Scanned: config/, kubejs/, mods/

Matches the pack           8 files  → recorded
Edited by you              1 file   → overlay
  ~ config/sophisticatedbackpacks-common.toml
Added by you               3 files
  + config/spark/  → .packctlignore
  + mods/grieflogger.jar  → already in overlay
  + mods/spark-1.10.124-neoforge.jar  → overlay
Overlay conflicts          0

Adopted 'demo' as demo-pack-1.0.
  overlay:        2 files copied to /tmp/packctl-demo/server/overlay
  .packctlignore: 1 rule (block 'packctl adopt')
  state:          8 managed files recorded
Undo with: packctl rollback
Next: packctl plan

$ packctl plan
demo
demo-pack-1.0 → demo-pack-1.1

Upstream
  + 1 file
  ~ 1 file
  - 2 files

Server
  ✓ server checks passed
```

Before the adoption, the same `packctl plan` wanted to delete four files packctl never installed (the spark jar, its
config, and the two jars the pack really drops) and overwrite two configs, and `update` refused to run (see
[the force-sync guard](safety.md#the-force-sync-guard)).

## A report with every kind of change

The same demo server, with more changes made by hand: AppleSkin disabled, JEI swapped for a newer jar, a LuckPerms
database in `config/`, a comment added to a config, and a KubeJS script deleted. `-v` lists every file.

```console
$ packctl adopt --as demo-pack-1.0 -v
demo
Adopting as demo-pack-1.0 (id demo-pack-1.0.zip)
Scanned: config/, kubejs/, mods/

Matches the pack           4 files  → recorded
  = config/create-server.toml
  = mods/create-1.21.1-6.0.3.jar
  = mods/ftbquests-neoforge-2101.1.6.jar
  = mods/sophisticatedbackpacks-1.21.1-3.23.4.1197.jar
Reformatted only           1 file   → accepted as-is
  ≈ config/ftbquests-server.toml
Edited by you              1 file   → overlay
  ~ config/sophisticatedbackpacks-common.toml
Added by you               3 files
  + config/spark/config.json  → .packctlignore
  + mods/grieflogger.jar  → already in overlay
  + mods/spark-1.10.124-neoforge.jar  → overlay
Replaced by you            1 file   → overlay + ignore old
  ~ mods/jei-1.21.1-neoforge-19.21.0.247.jar → mods/jei-1.21.1-neoforge-19.22.0.315.jar
Disabled by you            1 file   → .packctlignore
  - mods/appleskin-neoforge-mc1.21-3.0.5.jar (disabled as mods/appleskin-neoforge-mc1.21-3.0.5.jar.disabled)
Missing                    1 file   → restored on next update
  - kubejs/server_scripts/recipes.js
Runtime data               1 file   → .packctlignore
  + config/luckperms/luckperms-h2-v2.mv.db
Overlay conflicts          0

The next update will restore 1 file from the pack.

Dry run: nothing was changed.
Apply with: packctl adopt --as demo-pack-1.0.zip --apply
  This writes 3 files (0.0 MiB) to /tmp/packctl-demo/server/overlay, 5 rules to .packctlignore and .packctl/state.json.
```

After `--apply`, the next plan restores the deleted script and leaves everything else alone:

```console
$ packctl plan -v
demo
demo-pack-1.0 → demo-pack-1.1

Upstream
  + 2 files
  ~ 1 file
  - 1 file

Files
  + kubejs/server_scripts/recipes.js
  + mods/create-1.21.1-6.0.4.jar
  ~ config/create-server.toml
  - mods/create-1.21.1-6.0.3.jar
  = config/sophisticatedbackpacks-common.toml (overlay, in sync)
  = mods/grieflogger.jar (overlay, in sync)
  = mods/jei-1.21.1-neoforge-19.22.0.315.jar (overlay, in sync)
  = mods/spark-1.10.124-neoforge.jar (overlay, in sync)

Server
  ✓ server checks passed
```

`--json` prints the whole classification (every entry with its class, action, pack and live fingerprints and ignore
rule) for scripts.

## The `.packctlignore` block

adopt writes its rules into a marked block in `.packctlignore`, creating the file if needed. The second line records
the date and the version:

```gitignore
# >>> packctl adopt
# 2026-11-18 demo-pack-1.0 (id demo-pack-1.0.zip)
config/luckperms
config/spark
mods/appleskin-neoforge-mc1.21-3.0.5.jar
mods/appleskin-neoforge-mc1.21-3.0.5.jar.disabled
mods/jei-1.21.1-neoforge-19.21.0.247.jar
# <<< packctl adopt
```

Everything outside the block is yours and is kept byte for byte. Adopting again (with `--force`) replaces only the
block. You may edit or delete rules in the block by hand; the syntax is in [overlay.md](overlay.md#packctlignore).

## Detecting the version

```console
$ packctl adopt --detect
Detected demo-pack-1.0 (100% of the pack's mods match)
demo
Adopting as demo-pack-1.0 (id demo-pack-1.0.zip)
Scanned: config/, kubejs/, mods/

Matches the pack           8 files  → recorded
Edited by you              1 file   → overlay
  ~ config/sophisticatedbackpacks-common.toml
Added by you               3 files
  + config/spark/  → .packctlignore
  + mods/grieflogger.jar  → already in overlay
  + mods/spark-1.10.124-neoforge.jar  → overlay
Overlay conflicts          0

Dry run: nothing was changed.
Apply with: packctl adopt --as demo-pack-1.0.zip --apply
  This writes 2 files (0.0 MiB) to /tmp/packctl-demo/server/overlay, 1 rule to .packctlignore and .packctl/state.json.
```

`--detect` compares the jars in your `mods/` with the jars of the 15 newest versions, by file name, size and hash,
without downloading whole packs where it can avoid it: local archives are read from their zip directory (CRC-32),
Modrinth versions from their index (SHA-1). A version is picked when at least 85% of its mods match and it leads the
runner-up by at least 5 points; otherwise adopt stops and lists the three best matches for you to pass to `--as`.
CurseForge does not support detection yet; pass `--as`.

## Undoing an adoption

`--apply` takes a snapshot first (of `state.json`, `.packctlignore` and the overlay paths it is about to create), so

```bash
packctl rollback
```

restores the state and ignore file and removes the overlay copies adopt made. `packctl status` then shows
`Installed: never` again. If you already updated after adopting, the first rollback undoes the update and the
second undoes the adoption.

## Caveats

- **Reformatting detection is a heuristic.** Only comments, blank lines and trailing whitespace are ignored, for
  common config formats. Anything else counts as an edit and goes to the overlay. A file that may contain a multi-line
  string (TOML `"""`/`'''`, YAML `|`/`>` blocks, `\` line continuations, Forge `<` lists) is always treated as edited,
  because a `#` line or blank line inside such a string is data, not formatting.
- **Overlay files are frozen.** A config adopt copied into the overlay wins over every future pack version, even when
  the pack author fixes something in it. The plan shows a conflict notice when upstream changes such a file; merge by
  hand, or delete the overlay copy to take the pack's version.
- **Ignored paths are yours to maintain.** A mod you removed stays removed; a config folder you ignored is never
  updated by the pack again.
- **Pick the right version.** Adopting the wrong version records the wrong baseline. The report makes that obvious
  (many edited or missing files), and adopt refuses when more than half of the pack's files are missing.
