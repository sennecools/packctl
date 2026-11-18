# Security policy

## Supported versions

| Version | Supported |
| --- | --- |
| Latest release | Yes |
| `rolling` (latest `main`) | Yes |
| Older releases | No; please upgrade |

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/sennecools/packctl/security/advisories/new).
Do not open a public issue for a security problem.

Include the packctl version (`packctl --version`), what you did, what happened,
and, if you can, a minimal reproduction (for example a crafted archive). You
should get a first response within a week. Fixes are released as soon as they
are ready and credited in the changelog unless you prefer otherwise.

## Scope

In scope:

- **API key storage**: the encrypted per-profile and shared CurseForge keys,
  the master key file, and any path where a key could be printed, logged or
  sent to a host other than intended.
- **Path traversal**: server-pack zips, `.mrpack` archives and their
  `modrinth.index.json`, overlay and profile paths escaping the server root.
- **Data loss**: snapshot, update and rollback behavior that deletes or
  overwrites admin data (worlds, configs, overlay files) without a way back.
- **Installer integrity**: `install.sh` and the release pipeline, including
  checksum and attestation verification.
- **The community CurseForge proxy**: without a personal key, packctl reads
  CurseForge metadata through the third-party proxy `api.curse.tools`. Files
  are always downloaded over https from CurseForge's CDN (`*.forgecdn.net`)
  and checked against the SHA-1 and size in that metadata, and your API key is
  never sent to the proxy. The proxy does decide which CDN file and hash
  packctl expects, so it is trusted to report them honestly; use a personal key
  if that trust is not acceptable for your server. Ways to abuse this trust
  beyond choosing a CDN file (for example escaping the server root or reaching
  a non-CDN host) are in scope.

Out of scope: vulnerabilities in the modpacks or mods themselves, in your server
panel (AMP) or in Minecraft, and issues that require an attacker who can
already write to the server directory or to `PACKCTL_HOME`.
