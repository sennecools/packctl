[← packctl](../README.md)

# packctl docs

The [README](../README.md) covers what packctl is and how to get started. These pages go into detail.

| Page | What it covers |
| --- | --- |
| [Installation](installation.md) | The installer and its options, rolling vs stable builds, building from source, verifying downloads, upgrading and uninstalling. |
| [Configuration](configuration.md) | Local and global profiles, every profile field, API key storage and resolution, endpoint resolution, environment variables. |
| [Providers](providers.md) | CurseForge (proxy, personal key, errors), Modrinth (`.mrpack` handling, loaders) and local archives. |
| [Adopting an existing server](adopting.md) | How `packctl adopt` classifies files, the full report, the `.packctlignore` block, `--detect`, undoing it. |
| [Overlay and ignore rules](overlay.md) | Overlay semantics, the conflict notice, `.packctlignore` syntax, persistent paths. |
| [Safety](safety.md) | The guarantees, the force-sync guard, recovering from a failed update, `doctor` and `validate`. |
| [How it works](how-it-works.md) | The update flow, the planner rules, `state.json` and snapshots. |
| [Command reference](commands.md) | Every command and flag, with the exact `--help` output. |

The offline demo used for the examples lives in [`scripts/demo/setup.sh`](../scripts/demo/setup.sh), and the
terminal recording is generated from [`demo.tape`](demo.tape).
