[← Docs](README.md)

# Installation

packctl is a single static binary for Linux on x86_64 and arm64 (aarch64). It has no runtime dependencies. Install
it on the machine that holds the server files.

## The installer

```bash
curl -fsSL https://raw.githubusercontent.com/sennecools/packctl/main/install.sh | sh -s rolling
```

[`install.sh`](../install.sh) detects the architecture, downloads `packctl-<target>.tar.gz` from the GitHub release,
checks it against the release's `SHA256SUMS` (it refuses to install when the checksum is missing or wrong), extracts
only the binary and installs it with mode 0755.

It takes two optional positional arguments, passed with `sh -s`, or the matching environment variables:

| Argument | Variable | Default | Meaning |
| --- | --- | --- | --- |
| 1 | `VERSION` | `latest` | A release tag such as `v0.2.0`, `latest` for the newest stable release, or `rolling`. |
| 2 | `INSTALL_DIR` | `/usr/local/bin` as root, else `~/.local/bin` | Where `packctl` is installed. The installer uses `sudo` when the directory is not writable. |

Other environment variables:

| Variable | Meaning |
| --- | --- |
| `PACKCTL_VERIFY_ATTESTATION=1` | Also verify the GitHub build provenance with `gh attestation verify` (needs the GitHub CLI). |
| `PACKCTL_DOWNLOAD_BASE` | Download from this `https://` or `file://` base instead of the GitHub release (used by CI). |
| `PACKCTL_REPO` | Install from a fork (`owner/name`). |

```bash
# A specific release into ~/bin
curl -fsSL https://raw.githubusercontent.com/sennecools/packctl/main/install.sh | sh -s v0.2.0 "$HOME/bin"

# The rolling build, system-wide, with the attestation checked
curl -fsSL https://raw.githubusercontent.com/sennecools/packctl/main/install.sh \
  | PACKCTL_VERIFY_ATTESTATION=1 sh -s rolling /usr/local/bin
```

The attestation check runs `gh attestation verify`, which needs a logged-in GitHub CLI, so run the installer as
yourself rather than under `sudo` (root's `gh` is usually not logged in). It asks for `sudo` only to copy the binary
into a directory you cannot write to.

The installer prints `packctl --version` at the end and warns when the install directory is not on your `PATH`.

## Rolling or stable

- **Stable** releases are tagged `vX.Y.Z` and listed in the [CHANGELOG](../CHANGELOG.md). `latest` always means the
  newest stable release.
- **Rolling** is a pre-release rebuilt from every commit on `main` that passed CI. `packctl --version` shows the commit
  it was built from, for example `0.2.0+1a2b3c4 (x86_64-unknown-linux-musl)`.

Until v0.2.0 is released, `latest` still installs v0.1.0, which lacks Modrinth, adopt, keyless CurseForge and most of
what these docs describe. Install `rolling` until then.

## From source

With Rust 1.88 or newer:

```bash
cargo install --git https://github.com/sennecools/packctl --locked
```

[cargo-binstall](https://github.com/cargo-bins/cargo-binstall) fetches the release binary instead of compiling, once a
release matching the version in `Cargo.toml` exists:

```bash
cargo binstall --git https://github.com/sennecools/packctl packctl
```

Or from a checkout:

```bash
git clone https://github.com/sennecools/packctl
cd packctl
cargo build --release --locked
sudo install -m 0755 target/release/packctl /usr/local/bin/packctl
```

## Verifying a download

Every release, rolling included, has a `SHA256SUMS` file and a GitHub build provenance attestation for each archive:

```bash
gh release download rolling -R sennecools/packctl -p 'packctl-x86_64-unknown-linux-musl.tar.gz' -p SHA256SUMS
sha256sum -c --ignore-missing SHA256SUMS
gh attestation verify packctl-x86_64-unknown-linux-musl.tar.gz -R sennecools/packctl
tar -xzf packctl-x86_64-unknown-linux-musl.tar.gz
install -m 0755 packctl-x86_64-unknown-linux-musl ~/.local/bin/packctl
```

The archive holds `packctl-<target>` (the binary), `LICENSE` and `README.md`.

## Shell completions

```bash
packctl completions bash > ~/.local/share/bash-completion/completions/packctl
packctl completions zsh > "${fpath[1]}/_packctl"
packctl completions fish > ~/.config/fish/completions/packctl.fish
```

`powershell` and `elvish` are supported too. Regenerate the script after upgrading.

## Upgrading

Run the installer again with the version you want. Profiles, state and snapshots are not touched by an upgrade:
profiles and keys live in the config directory, and each server's state lives in its own `.packctl/` folder.

## Uninstalling

Remove the binary:

```bash
rm "$(command -v packctl)"
```

Then, if you want to remove everything packctl stored, delete the config directory: `$PACKCTL_HOME` when it is set,
otherwise `$XDG_CONFIG_HOME/packctl`, which defaults to `~/.config/packctl`. It holds global profiles (`<name>.toml`),
the master key (`.key`) and the shared API key (`apikey`).

```bash
rm -r "${PACKCTL_HOME:-${XDG_CONFIG_HOME:-$HOME/.config}/packctl}"
```

Each server root also has packctl files you may delete: `.packctl/` (state and snapshots), `.packctl.toml`,
`.packctlignore` and, if you no longer want it, the `overlay/` folder. Deleting the master key makes every encrypted
API key in your profiles unreadable.
