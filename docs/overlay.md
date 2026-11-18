[← Docs](README.md)

# Overlay and ignore rules

packctl replaces the pack's folders with the new version on every update. Two things keep your own changes alive:
the **overlay**, for files you want to win over the pack, and **`.packctlignore`**, for files the pack must leave
alone.

## The overlay

The overlay is a plain folder (by default `overlay/` in the server root) that mirrors the server root:

```text
overlay/
├── mods/
│   ├── grieflogger.jar
│   └── spark-1.10.124-neoforge.jar
├── config/
│   └── sophisticatedbackpacks-common.toml
├── kubejs/
│   └── server_scripts/
│       └── our-recipes.js
└── server.properties
```

- **The overlay always wins.** After the pack's files are written, every overlay file is copied to the same path in
  the server root. If the pack ships that path too, your version is the one that ends up on the server.
- **Overlay files are never swept.** A file the pack does not ship is kept when the overlay provides it.
- **Only changes are written.** An overlay file that already matches the live file counts as in sync; a plan where
  nothing else changed is empty, and the server is not restarted.
- **`server.properties`** is persistent data by default and never touched. Put it in the overlay and packctl manages
  it from there.
- **Removing an overlay file** does not delete the copy on the server. If the pack ships that path, the next update
  writes the pack's version; otherwise, inside the pack's folders, the copy is removed like any other file the pack
  does not ship.

`packctl plan -v` lists the overlay files the update would write, and marks those already in place:

```text
  = mods/grieflogger.jar (overlay, in sync)
```

The overlay folder itself, the local provider's drop folder (`packs/`, or the configured `archive` inside the server
root), `.packctl/`, `.packctl.toml` and `.packctlignore` are reserved: a pack can never write to them, and the sweep
never removes them.

## The conflict notice

When the pack changes a file that your overlay replaces, the plan says so, on every update where it happens:

```console
$ packctl plan --to demo-pack-1.1
demo
demo-pack-1.0 → demo-pack-1.1

Upstream
  + 1 file
  - 2 files

Overlay
  ⚠ 1 overlay file replaces a file that changed upstream

Overlay conflict notice
~ config/create-server.toml
  Upstream changed this file.
  Local overlay replaces it.
  Overlay version will be used.

Server
  ✓ server checks passed
```

This is information, not an error: the update still uses your file. packctl does not merge configs. If the pack's
change matters, merge it into your overlay copy by hand, or delete the overlay copy to take the pack's version.

## .packctlignore

Mods write runtime data inside the pack's folders: a permission plugin keeps its database under `config/`, some mods
write configs on first start, a panel may manage `jre/`. The sweep would remove those files on every update.
`.packctlignore` in the server root lists paths packctl must never touch: they are neither removed nor replaced from
the pack. The overlay still applies to an ignored path when it provides one.

```gitignore
# Comments and blank lines are ignored.
config/luckperms/              # a directory and everything below it
defaultconfigs/ftbranks
config/*.bak                   # * and ? match within one path segment
**/tmp                         # ** matches any number of segments
```

- Patterns are paths relative to the server root, with `/` separators. A trailing `/` is optional.
- A pattern matches the path it names and, when that is a directory, everything below it.
- `*` and `?` match within one segment, `**` matches zero or more segments.
- A `#` at the start of a line, or after whitespace, starts a comment. A `#` inside a name (`mods/a#b.jar`) is part of
  the name.
- Patterns are always relative to the server root: a leading `/` is dropped, so `/mods` means `mods`. `.` and `..`
  segments and empty segments (`config//a`) are rejected. Negation (`!`) is not supported.

`packctl adopt` writes its rules into a marked block in this file and never touches the rest (see
[adopting.md](adopting.md#the-packctlignore-block)).

## Persistent data

These paths never enter a plan, even when a pack ships them:

| Path | |
| --- | --- |
| `world/`, `logs/`, `backups/`, `crash-reports/` | the whole directory |
| `server.properties` | unless the overlay provides it |
| `ops.json`, `whitelist.json`, `banned-players.json`, `banned-ips.json`, `usercache.json` | |

Folders the pack does not ship at all (`libraries/`, panel files, your own folders) are outside the sweep, so they
are never touched either.
