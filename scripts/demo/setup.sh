#!/bin/sh
# Builds an offline packctl demo: a fake modpack server, two versions of a
# local server pack, an overlay and a command-controlled "server" that is
# just a marker file. No network access and no API key are needed.
#
# Usage (from the repository root, with packctl on PATH):
#
#   . scripts/demo/setup.sh        # build the fixture, cd into the server
#   sh scripts/demo/setup.sh       # only build the fixture
#
# The fixture lives in ${PACKCTL_DEMO_DIR:-/tmp/packctl-demo} and is rebuilt
# from scratch on every run. The script only deletes a directory it built
# itself (marked by a .packctl-demo file) and refuses any other non-empty one:
#
#   server/                  the "live" server (pack 1.0 plus admin changes)
#   server/packs/            demo-pack-1.0.zip and demo-pack-1.1.zip
#   server/overlay/          mods/grieflogger.jar
#   home/                    PACKCTL_HOME (keeps your real config untouched)
#
# When sourced, the script exports PACKCTL_HOME and leaves you in server/.
# Then try:
#
#   packctl status
#   packctl versions
#   packctl adopt --as demo-pack-1.0 --apply -y
#   packctl plan --to demo-pack-1.1
#   packctl update -n --to demo-pack-1.1
#   packctl rollback

PACKCTL_DEMO_DIR="${PACKCTL_DEMO_DIR:-/tmp/packctl-demo}"

# Runs in a subshell so `set -eu` and `cd` never leak into a sourcing shell.
packctl_demo_build() (
  set -eu

  say() { printf 'demo: %s\n' "$*"; }
  die() { printf 'demo: error: %s\n' "$*" >&2; exit 1; }

  command -v packctl >/dev/null 2>&1 || die "packctl is not on PATH"

  # --- a zip writer: python3 (or python / py) first, then Info-ZIP ---
  python=""
  for candidate in python3 python py; do
    if command -v "$candidate" >/dev/null 2>&1 \
      && "$candidate" -c 'import zipfile' >/dev/null 2>&1; then
      python="$candidate"
      break
    fi
  done
  if [ -z "$python" ] && ! command -v zip >/dev/null 2>&1; then
    die "python3 or zip is required to build the demo archives"
  fi

  # Stores every file under the current directory in <out.zip>, with
  # forward-slash names relative to it (and a fixed timestamp with python).
  zip_here() {
    if [ -n "$python" ]; then
      "$python" -c '
import os, sys, zipfile
with zipfile.ZipFile(sys.argv[1], "w", zipfile.ZIP_DEFLATED) as z:
    for top, dirs, files in os.walk("."):
        dirs.sort()
        for name in sorted(files):
            path = os.path.join(top, name)
            info = zipfile.ZipInfo(os.path.relpath(path, ".").replace(os.sep, "/"), (2026, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            with open(path, "rb") as f:
                z.writestr(info, f.read())
' "$1"
    else
      zip -qrX "$1" .
    fi
  }

  # stub_jar <dir> <file name> <mod id> <version>: a stub NeoForge mod jar.
  # Each jar is built once, so every copy of it is byte-identical.
  stub_jar() {
    cached="$work/jars/$2"
    if [ ! -f "$cached" ]; then
      stub="$work/jar"
      rm -rf "$stub"
      mkdir -p "$stub/META-INF" "$work/jars"
      cat >"$stub/META-INF/neoforge.mods.toml" <<EOF
modLoader = "javafml"
loaderVersion = "[4,)"
license = "demo"

[[mods]]
modId = "$3"
version = "$4"
displayName = "$3"
EOF
      (cd "$stub" && zip_here "$cached")
    fi
    mkdir -p "$1"
    cp "$cached" "$1/$2"
  }

  # pack <dir> <version>: the files of one demo pack version.
  pack() {
    dir="$1"
    mkdir -p "$dir/mods" "$dir/config" "$dir/kubejs/server_scripts"
    stub_jar "$dir/mods" "jei-1.21.1-neoforge-19.21.0.247.jar" jei 19.21.0.247
    stub_jar "$dir/mods" "sophisticatedbackpacks-1.21.1-3.23.4.1197.jar" sophisticatedbackpacks 3.23.4
    stub_jar "$dir/mods" "ftbquests-neoforge-2101.1.6.jar" ftbquests 2101.1.6
    printf '[general]\nautoSaveInterval = 6000\n' >"$dir/config/ftbquests-server.toml"
    printf '[common]\nmaxBackpackSize = 54\n' >"$dir/config/sophisticatedbackpacks-common.toml"
    printf '// Demo pack recipes\nServerEvents.recipes(event => {\n  event.remove({ output: "minecraft:ender_chest" })\n})\n' \
      >"$dir/kubejs/server_scripts/recipes.js"
    if [ "$2" = "1.0" ]; then
      stub_jar "$dir/mods" "create-1.21.1-6.0.3.jar" create 6.0.3
      stub_jar "$dir/mods" "appleskin-neoforge-mc1.21-3.0.5.jar" appleskin 3.0.5
      printf '[kinetics]\nmaxRotationSpeed = 256\n' >"$dir/config/create-server.toml"
    else
      # 1.1: Create 6.0.4 with a new config value, and AppleSkin is gone.
      stub_jar "$dir/mods" "create-1.21.1-6.0.4.jar" create 6.0.4
      printf '[kinetics]\nmaxRotationSpeed = 512\n' >"$dir/config/create-server.toml"
    fi
  }

  root="$PACKCTL_DEMO_DIR"
  case "$root" in
    "" | /) die "refusing to use '$root' as the demo directory" ;;
  esac
  # Only ever delete a directory this script built: it must be missing,
  # empty, or carry the marker file written below.
  marker=".packctl-demo"
  if [ -e "$root" ] || [ -L "$root" ]; then
    if [ -L "$root" ] || [ ! -d "$root" ]; then
      die "'$root' exists and is not a directory"
    fi
    if [ ! -f "$root/$marker" ] && [ -n "$(ls -A "$root")" ]; then
      die "'$root' is not empty and was not built by this script; remove it or set PACKCTL_DEMO_DIR to a new directory"
    fi
  fi
  rm -rf "$root"
  mkdir -p "$root/home" "$root/work"
  root="$(cd "$root" && pwd)"
  printf 'Built by scripts/demo/setup.sh; deleted and rebuilt on every run.\n' >"$root/$marker"
  work="$root/work"
  server="$root/server"

  say "building the demo packs in $root"
  for version in 1.0 1.1; do
    pack "$work/pack-$version" "$version"
    mkdir -p "$server/packs"
    (cd "$work/pack-$version" && zip_here "$server/packs/demo-pack-$version.zip")
  done
  # The local provider treats the newest file as the latest version.
  TZ=UTC touch -t 202610021200 "$server/packs/demo-pack-1.0.zip"
  TZ=UTC touch -t 202611141200 "$server/packs/demo-pack-1.1.zip"

  say "installing pack 1.0 by hand, the way most servers start"
  cp -R "$work/pack-1.0/." "$server/"
  mkdir -p "$server/world"
  printf 'demo level\n' >"$server/world/level.dat"
  printf 'motd=packctl demo\nmax-players=20\n' >"$server/server.properties"
  printf 'eula=true\n' >"$server/eula.txt"
  printf '#!/bin/sh\necho "the demo server has no Minecraft in it"\n' >"$server/run.sh"

  # The admin's own changes: a mod added by hand (which wrote its own
  # config), an edited pack config, and a mod kept in the overlay.
  stub_jar "$server/mods" "spark-1.10.124-neoforge.jar" spark 1.10.124
  mkdir -p "$server/config/spark"
  printf '{ "backgroundProfiler": false }\n' >"$server/config/spark/config.json"
  printf '[common]\nmaxBackpackSize = 81\n' >"$server/config/sophisticatedbackpacks-common.toml"
  stub_jar "$server/overlay/mods" "grieflogger.jar" grieflogger 1.2.0
  cp "$server/overlay/mods/grieflogger.jar" "$server/mods/grieflogger.jar"
  # The "server" runs while server/.running exists. It starts stopped,
  # because `packctl adopt --apply` wants a stopped server.

  rm -rf "$work"

  say "creating the profile"
  cd "$server" || exit 1
  PACKCTL_HOME="$root/home" packctl create demo --provider local \
    --controller command \
    --status 'test -f .running' --stop 'rm -f .running' --start 'touch .running' \
    -n >/dev/null
  say "ready: $server"
)

# Called as a plain command, never as an `if` or `&&` condition: the shell
# ignores `set -e` for the whole body of a function run as a condition, so
# a failing step inside it would be skipped instead of stopping the build.
packctl_demo_build
packctl_demo_status=$?
if [ "$packctl_demo_status" -eq 0 ]; then
  # Resolved once, so a relative PACKCTL_DEMO_DIR still works after the cd.
  if packctl_demo_root="$(cd "$PACKCTL_DEMO_DIR" && pwd)"; then
    PACKCTL_HOME="$packctl_demo_root/home"
    export PACKCTL_HOME
    cd "$packctl_demo_root/server" || :
  fi
  unset packctl_demo_root
fi
# Fail the run (or the `.`) too, so CI notices a broken fixture. The status
# is expanded before the unset runs.
eval "unset packctl_demo_status; [ $packctl_demo_status -eq 0 ]"
