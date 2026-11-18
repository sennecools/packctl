[← Docs](README.md)

# Providers

A provider lists a pack's versions and builds the server side of one version in a staging folder. Everything after
that (planning, snapshots, the overlay, rollback) is the same for every provider.

| | CurseForge | Modrinth | Local |
| --- | --- | --- | --- |
| `provider =` | `"curseforge"` | `"modrinth"` | `"local"` |
| A version is | a client file with a paired server pack | a version with an `.mrpack` | a `.zip` or `.mrpack` in the drop folder |
| API key | optional | never | never |
| Network | metadata + CDN download | API + `.mrpack` + mod downloads | none for zips |
| `adopt --detect` | no (use `--as`) | yes | yes |

When you ask for the latest version, packctl stays on the Minecraft version the server runs: a newer version for
another Minecraft version is skipped, with a notice that names it, and older versions are never picked. Use `--to`
to switch on purpose.

## CurseForge

The first lines of `packctl versions` for All the Mods 10 (146 versions at the time of writing):

```text
Available versions for atm10
  1. All the Mods 10-8.6 (id 9620701) [1.21.1]
  2. All the Mods 10-8.5 (id 9431424) [1.21.1]
  3. All the Mods 10-8.4 (id 9366926) [1.21.1]
  4. All the Mods 10-8.3 (id 9190157) [1.21.1]
  5. All the Mods 10-8.2 (id 8945086) [1.21.1]
```

A CurseForge modpack version is a client file. packctl lists only the files whose metadata names a paired server pack
(`serverPackFileId`) and installs that server pack. Version names are the file's display name, ids are CurseForge file
ids. `create` accepts a project URL (`https://www.curseforge.com/minecraft/modpacks/all-the-mods-10`), a slug or a
numeric project id.

### Access modes

| Mode | When | Key sent | Notes |
| --- | --- | --- | --- |
| Community proxy `https://api.curse.tools` | no key configured (the default) | never | Third-party and anonymous, not affiliated with CurseForge, no SLA, responses cached for up to an hour. Using it is a grey area of the [CurseForge 3rd-party API terms](https://support.curseforge.com/en/support/solutions/articles/9000207405-curse-forge-3rd-party-api-terms-and-conditions). |
| Official API `https://api.curseforge.com` | a key is configured | yes | Recommended for production. Free key at [console.curseforge.com](https://console.curseforge.com/). |
| Custom endpoint | `PACKCTL_CF_API_BASE` or `[pack] api_base` | only with `PACKCTL_CF_PROXY_SEND_KEY=1` | A self-hosted proxy such as [bmpm-mc/cfproxy](https://github.com/bmpm-mc/cfproxy) or [CurseForge.ApiProxy](https://github.com/CurseForgeCommunity/CurseForge.ApiProxy). |

The full resolution order is in [configuration.md](configuration.md#curseforge-endpoint-resolution). packctl never
ships, embeds or borrows another application's API key, and there is no second fallback proxy.

### Downloads

The server pack is downloaded from CurseForge's CDN (`*.forgecdn.net`) over https, whichever endpoint served the
metadata. packctl requires the SHA-1 and size CurseForge reports, checks both before the file is used, and refuses a
file that has no SHA-1. A download URL outside `forgecdn.net` is refused.

With a key and the official API, the key is also sent to the CDN, and redirects may only go to `*.forgecdn.net` over
https, so the key never reaches a third party. CurseForge has announced that the CDN will require a key; once it does,
keyless downloads fail with a message that tells you to store one.

### Errors

| Message (shortened) | What to do |
| --- | --- |
| `the CurseForge endpoint https://api.curse.tools refused or rate-limited the request` | The proxy is down or throttling. Store your own key: `packctl apikey --global`. |
| `CurseForge requires an API key for file downloads` | The CDN now requires a key. Store one with `packctl apikey --global`. |
| `CurseForge rejected the API key; check CF_API_KEY or run 'packctl apikey'` | The key is wrong or revoked. `packctl apikey --show` tells you which key is used. |
| `the official CurseForge API requires a key` | `PACKCTL_CF_API_BASE` points at the official API but no key is set. Store a key or unset the variable. |
| `... cannot be downloaded by third-party tools because its author disabled it` | The author turned off third-party downloads. Download the server pack from the page in the message, drop it in `packs/` and use the [local provider](#local). packctl never works around this. |
| `... has no paired server pack` | That version has no server pack on CurseForge. Pick another version, or use a server pack from the author with the local provider. |
| `... reports no SHA-1 hash; refusing to install an unverified download` | CurseForge returned no hash for the file. Try again later or use the local provider. |

`packctl doctor` prints the endpoint in use and checks that it answers:

```console
$ packctl doctor
CurseForge endpoint: community proxy https://api.curse.tools (no API key configured)
0 warning(s), 0 error(s)
All checks passed.
```

## Modrinth

```console
$ packctl create cobblemon --source https://modrinth.com/modpack/cobblemon-fabric --controller amp --instance Cobblemon -n
Created profile 'cobblemon'
  file:       /srv/minecraft/cobblemon/.packctl.toml
  pack:       Cobblemon Official Modpack [Fabric] (Modrinth project 5FFgwNNP)
  server:     /srv/minecraft/cobblemon
  overlay:    /srv/minecraft/cobblemon/overlay
  controller: amp (instance Cobblemon)
  api key:    not needed (Modrinth)

Next: packctl status

$ packctl versions
Available versions for cobblemon
  1. Cobblemon Official Modpack [Fabric] 1.8.4 · 1.8.4 (id YQpPe5Iu) [1.21.1 fabric]
  2. Cobblemon Official Modpack [Fabric] 1.8.3 · 1.8.3 (id y1Dq3GgQ) [1.21.1 fabric]
  3. Cobblemon Official Modpack [Fabric] 1.8.2 · 1.8.2 (id BpTjgzL8) [1.21.1 fabric]
  4. Cobblemon Official Modpack [Fabric] 1.8.1 · 1.8.1 (id Cqimd3JM) [1.21.1 fabric]
  5. Cobblemon Official Modpack [Fabric] 1.8 · 1.8 (id zsTAkftH) [1.21.1 fabric]
  6. Cobblemon Official Modpack [Fabric] 1.7.3 · 1.7.3 (id Lydu1ZNo) [1.21.1 fabric]
  7. Cobblemon Official Modpack [Fabric] 1.7.2 · 1.7.2 (id pVGZ9l8W) [1.21.1 fabric]
  8. Cobblemon Official Modpack [Fabric] 1.7.1 · 1.7.1 (id DbQNxSJ0) [1.21.1 fabric]
  9. Cobblemon Official Modpack [Fabric] 1.7 · 1.7 (id jMz7A3RO) [1.21.1 fabric]
  10. Cobblemon Official Modpack [Fabric] 1.6.1.4 · 1.6.1.4 (id 98odLiu9) [1.21.1 fabric]
  11. Cobblemon Official Modpack [Fabric] 1.6.1.2 · 1.6.1.2 (id zqbIAux1) [1.21.1 fabric]
  12. Cobblemon Official Modpack [Fabric] 1.6.1.1 · 1.6.1.1 (id ZGcN3At3) [1.21.1 fabric]
  13. Cobblemon Official Modpack [Fabric] 1.6.1 · 1.6.1 (id cqaC80tF) [1.21.1 fabric]
  14. Cobblemon Official Modpack [Fabric] 1.6 · 1.6 (id Jln0oj6n) [1.21.1 fabric]
  15. Cobblemon Official Modpack [Fabric] 1.5.2 · 1.5.2 (id bpaivauC) [1.20.1 fabric]
```

Modrinth needs no key. `create` accepts a project URL (a trailing tab such as `/versions` or `/changelog` is fine), a
slug or a project id; a URL that names a version (`.../version/1.8.2`) is noted, and you install that version with
`--to`. Only `modrinth.com` URLs are recognized as Modrinth: a bare slug or id needs `--provider modrinth` (also in the
wizard), otherwise it is looked up on CurseForge. Client-only modpacks are refused.

### How a `.mrpack` becomes a server

1. The version's `.mrpack` is downloaded and checked against the hashes Modrinth lists for it.
2. Every file in its `modrinth.index.json` is downloaded into staging and verified. Each entry must carry a SHA-1, a
   SHA-512 and its exact size, its path goes through the same checks as an archive path, and its first download URL
   must be https on `cdn.modrinth.com`, `github.com`, `raw.githubusercontent.com` or `gitlab.com` (redirects: at most
   5, https only). Packs from the staging API (`PACKCTL_MODRINTH_API_BASE=https://staging-api.modrinth.com/v2`) may
   also use `staging-cdn.modrinth.com`.
3. `overrides/` is copied on top, then `server-overrides/`. A later layer overwrites an earlier one.
   `client-overrides/` is never used.

Files are chosen by their `env.server` value:

| `env.server` | Installed |
| --- | --- |
| `required` | yes |
| `optional` | yes, unless the profile has `optional_files = "exclude"` |
| `unsupported` | no |

**Reuse.** A file that is already on the server with the same size and SHA-512 is copied from there instead of
downloaded again, so an update downloads only what changed.

**Loader.** The plan shows the Minecraft and mod loader versions the pack needs. packctl does not install loaders:
it looks for the installed loader and warns when it is missing or does not match. `packctl doctor` prints the
detected loader for Modrinth profiles.

```console
$ packctl plan
adrenaserver
none → 1.7.0+1.21.1.fabric
Requires Minecraft 1.21.1 · Fabric 0.16.2

Upstream
  + 15 files

Reused 0 unchanged files from the server, downloaded 11

Loader not detected
  Pack requires Fabric 0.16.2; packctl could not find it in the server root.
  packctl does not install mod loaders.

Server
  ✓ server checks passed
```

When the server has a different loader version, the notice is `Loader mismatch` and asks you to update the loader
(for example in your panel) before starting the server.

`PACKCTL_MODRINTH_API_BASE` points packctl at another Modrinth API (a mirror or the staging API).

## Local

The local provider reads server packs you downloaded yourself. It works offline for `.zip` packs, and it is the
fallback when a CurseForge author disabled third-party downloads.

```bash
packctl create sb4 --provider local --controller amp --instance Stoneblock4 -n
cp ~/Downloads/ServerFiles-4.0.1.zip packs/
packctl plan
```

- **Where.** By default the `packs/` folder in the server root; `create` makes it. `archive = "..."` (or
  `create --archive`) points at a single file or another folder.
- **Versions.** Every `.zip` and `.mrpack` in the folder is one version. The name is the file name without its
  extension, the id is the file name. The most recently modified file is the latest version.
- **Zips.** A server-pack zip is extracted as it is. A zip whose content sits in a single top-level folder is
  unwrapped, and the plan says so.
- **`.mrpack`.** A local `.mrpack` goes through the same pipeline as a Modrinth version, so its index files are
  downloaded from the hosts listed above (only `overrides/` content is available offline).
- **`adopt --detect`** compares your `mods/` with the jars in each archive's zip directory (name, size, CRC-32),
  without extracting anything.

```console
$ packctl versions
Available versions for demo
  1. demo-pack-1.1 (id demo-pack-1.1.zip)
  2. demo-pack-1.0 (id demo-pack-1.0.zip)
```

`packs/` (or the configured archive folder inside the server root) is never part of a plan and never swept.
