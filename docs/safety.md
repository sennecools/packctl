[← Docs](README.md)

# Safety

packctl changes files on a live game server, so every step is built to fail safely and to be undoable.

## Guarantees

- **Staging before mutation.** The target version is resolved, downloaded, verified and extracted into a staging
  folder outside the server root, and the plan is built from it. `packctl plan` stops here, so it never changes
  anything.
- **Verified downloads.** Every download is https only, size checked and hash checked (SHA-1 for CurseForge, SHA-1
  and SHA-512 for Modrinth) before it is renamed into place. A partial file is deleted on any error.
- **Checks before the stop.** The server root must exist and be writable, the controller must answer and the disk
  must have room for the new files (plus 15%) before the server is stopped.
- **Snapshot before mutation.** The files the plan touches, and `state.json`, are copied to
  `.packctl/snapshots/<time>/` before the first change. World data is not copied, because it is never changed.
- **Commit last.** The new version is written to `state.json` only after the files, the overlay, the post-update
  checks and the start all succeeded. A failed update never claims the new version is installed.
- **Scoped removals.** A file can only be removed inside a folder the new version ships (or, elsewhere, when
  packctl installed it from an earlier pack version), and only when it is not in the new version, not provided by the
  overlay, not ignored by `.packctlignore` and not persistent data.
- **Persistent data is off limits.** `world/`, `logs/`, `backups/`, `crash-reports/`, `server.properties`,
  `ops.json`, the whitelist, the ban lists and `usercache.json` never enter a plan (`server.properties` only does when
  your overlay provides it).
- **Untrusted paths are contained.** Paths from archives, `modrinth.index.json`, providers, profiles and the overlay
  are normalized and rejected when they hold `..`, absolute or empty parts, or NUL bytes. Symlinks are not followed
  when deleting or overwriting. A pack can never write to packctl's own files (`.packctl/`, `.packctl.toml`,
  `.packctlignore`), the overlay or the local provider's drop folder (`packs/`, or the configured `archive` inside
  the server root).
- **One change at a time.** Updates and rollbacks take an exclusive lock (`.packctl/update.lock`), so two cannot run
  on the same server at once.
- **No-op plans leave the server alone.** When no file needs to change, the server is not stopped and no server file
  is written. If only the record is out of date (another version id with identical files, or a `state.json` written
  by an older packctl), packctl rewrites `state.json` alone, after snapshotting it, so `rollback` can undo that too.
- **Nothing packctl did not make is lost silently.** An update that would delete or overwrite files packctl never
  installed, or write an overlay file over a live file that changed outside packctl, is refused unless you pass
  `--force-sync` (see [the force-sync guard](#the-force-sync-guard)). The check runs again once the server has
  stopped, so a file a mod saves on shutdown is covered too, and an update whose `state.json` or `.packctlignore`
  changed after its plan was made (another packctl command ran meanwhile) is refused.
- **Secrets stay secret.** The CurseForge key is encrypted at rest and never printed. It is sent only to the official
  API (`api.curseforge.com`) and, for downloads, to the CurseForge CDN (`*.forgecdn.net`), both over https; a custom
  endpoint gets it only with `PACKCTL_CF_PROXY_SEND_KEY=1`, and the community proxy never does.

## The force-sync guard

`update` refuses to run, unless you pass `--force-sync`, when the plan would:

- delete files packctl never installed, such as a mod you dropped into `mods/` by hand;
- overwrite a live file packctl never installed with a file the pack now ships, such as the config a hand-added mod
  generated, once that mod joins the pack;
- write an overlay file over a live file that changed since packctl last wrote it (*live drift*), such as a config
  you edited on the server while the overlay holds an older copy;
- on a server packctl has no record of yet (no `.packctl/state.json`), delete or overwrite any file at all.

The last case is the first update of a hand-installed server. Without the guard, its plan would delete every file in
the pack's folders that the new version does not ship, including mods you added by hand, and overwrite every config
you edited. packctl shows that and refuses:

```console
$ packctl update -n --to demo-pack-1.1
demo
none → demo-pack-1.1

Upstream
  + 1 file
  ~ 2 files
  - 4 files

⚠ 4 files packctl never installed would be deleted
  config/spark/config.json
  mods/appleskin-neoforge-mc1.21-3.0.5.jar
  mods/create-1.21.1-6.0.3.jar
  mods/spark-1.10.124-neoforge.jar
  To keep them, copy them into the overlay or add .packctlignore rules.
  To delete them, run 'packctl update' with --force-sync.

⚠ First update of an unmanaged server
  4 files not in the pack would be deleted, 2 files that differ from the pack would be overwritten.
  Adopt it first: packctl adopt --as <version>

error: refusing the first update of 'demo': packctl has no record of this server, and the plan would delete 4 file(s) and overwrite 2 file(s) that differ from the pack.
Would delete:
  config/spark/config.json
  mods/appleskin-neoforge-mc1.21-3.0.5.jar
  mods/create-1.21.1-6.0.3.jar
  mods/spark-1.10.124-neoforge.jar
Would overwrite:
  config/create-server.toml
  config/sophisticatedbackpacks-common.toml
If the server already runs this pack, record it first:
  packctl adopt --as <version>
To converge onto the pack anyway (deleting and overwriting those files), re-run with --force-sync.
```

Files that are already identical to the new version are left alone, so this list is only what really differs. Two
ways forward:

- **[Adopt](adopting.md) the server** as the version it runs. Your additions and edits move into the overlay and
  `.packctlignore`, and the next plan shows only the real update.
- **`packctl update --force-sync`** when you do want the server to become exactly the pack. Interactively, `update`
  asks `Delete or overwrite the files listed above anyway?` instead.

On a server packctl already manages, the refusal starts with `refusing to update '<server>':` and lists the files
packctl never installed that would be deleted or overwritten and the live files the overlay would overwrite. Keep what you need
first (copy files into the overlay, add `.packctlignore` rules, or copy a live edit over the overlay file), or pass
`--force-sync` to converge onto the pack anyway.

The guard is in packctl's core, not just the CLI, so it protects every caller.

## When an update fails

If anything fails after the server was stopped (a write, the overlay, the post-update checks or the start), packctl
stops, does not commit the new version, and names the snapshot:

```console
$ packctl update -n --to 1.1
demo
demo-pack-1.0 → demo-pack-1.1

Upstream
  + 1 file
  ~ 1 file
  - 2 files

error: controller error: start command exited with code 1: false touch .running
The new version was not committed.
Rollback snapshot:
  /tmp/packctl-demo/server/.packctl/snapshots/2026-11-18T18-15-33Z-1
```

To go back to the previous version, fix the cause if it is in the way (here, a broken start command) and roll back:

```console
$ packctl rollback
Rolled back to demo-pack-1.0 (id demo-pack-1.0.zip)
Snapshot: /tmp/packctl-demo/server/.packctl/snapshots/2026-11-18T18-15-33Z-1
```

`rollback` stops the server, restores every file the snapshot holds, removes the files the failed change added,
restores `state.json` and starts the server. Each rollback steps back one change, so running it again undoes the
change before that (an update, or an [adoption](adopting.md#undoing-an-adoption)). A snapshot that was rolled back is
never restored twice.

Snapshots are kept until you delete them from `.packctl/snapshots/`. They hold only the files a change touched, so
they stay small.

## doctor and validate

`packctl doctor` checks the environment without changing anything: the server root exists and is writable, a server
start file is present (`run.sh`, `server.jar`, `forge-*.jar`, ...), the overlay exists, the controller answers, the
provider can be set up (for example that a stored API key still decrypts), and, for CurseForge, which endpoint is used
and that it answers. It also warns when packctl has no record of a server that already has mods installed.

`packctl validate` hashes every file packctl manages and compares it with `state.json`, so it finds files that were
changed or deleted outside packctl, and runs the local doctor checks.

Before the demo server is adopted, and after an update:

```console
$ packctl doctor
[warning] packctl has no record of this server yet; run 'packctl adopt --as <version>' before the first update
1 warning(s), 0 error(s)
All checks passed.
```

```console
$ packctl validate
0 warning(s), 0 error(s)
```

Both exit with code 3 when they find errors, so they fit in monitoring scripts.
