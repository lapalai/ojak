#!/bin/sh
# Ojak installer for macOS Apple silicon (arm64).
# POSIX sh. Repo token: change only OJAK_REPO below.
# shellcheck shell=sh
set -eu

# OJAK_REPO is the only GitHub owner/repo in this file.
OJAK_REPO=lapalai/ojak

SYSTEM=0
UNINSTALL=0
SKIP_SERVICE=0
SKIP_INTEGRATION=0
SKIP_SHELL=0
CLEAR_QUARANTINE=0
workdir=""
mnt=""

usage() {
  cat <<'EOF'
Usage: install.sh [--system] [--skip-service] [--skip-integration] [--skip-shell] [--clear-quarantine]
       install.sh --uninstall [--system]

Installs the latest Ojak.app from GitHub Releases into ~/Applications
(or /Applications with --system) and runs the bundled setup commands.

  --system            install or remove /Applications/Ojak.app (uses sudo)
  --skip-service      do not run `aam service install`
  --skip-integration  do not run `aam integration install`
  --skip-shell        do not run `aam shell install`
  --uninstall         run `aam deactivate`, then remove the app
  --clear-quarantine  opt in to `xattr -dr com.apple.quarantine` on the copied app

--clear-quarantine is off unless you pass it. It does not notarize the app and
does not guarantee macOS will allow it to open. The official path when macOS
blocks the first launch is System Settings → Privacy & Security → Open Anyway:
https://support.apple.com/102445
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --system) SYSTEM=1 ;;
    --uninstall) UNINSTALL=1 ;;
    --skip-service) SKIP_SERVICE=1 ;;
    --skip-integration) SKIP_INTEGRATION=1 ;;
    --skip-shell) SKIP_SHELL=1 ;;
    --clear-quarantine) CLEAR_QUARANTINE=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

if [ "$(uname -s)" != "Darwin" ]; then
  echo "Ojak installs on macOS only." >&2
  exit 1
fi
if [ "$(uname -m)" != "arm64" ]; then
  echo "This release is for Apple silicon (arm64). Intel Macs are not supported yet." >&2
  exit 1
fi

cleanup() {
  if [ -n "$mnt" ]; then
    /usr/bin/hdiutil detach "$mnt" >/dev/null 2>&1 || true
  fi
  if [ -n "$workdir" ] && [ -d "$workdir" ]; then
    /bin/rm -rf "$workdir"
  fi
}
trap cleanup EXIT

remove_app() {
  target="$1"
  if [ ! -d "$target" ]; then
    return 0
  fi
  parent=$(dirname "$target")
  if [ "$parent" = "/Applications" ]; then
    if [ "$SYSTEM" -ne 1 ]; then
      echo "Left $target in place. Re-run with --uninstall --system to remove it." >&2
      return 0
    fi
    /usr/bin/sudo /bin/rm -rf "$target"
  else
    /bin/rm -rf "$target"
  fi
  echo "Removed $target"
}

if [ "$UNINSTALL" -eq 1 ]; then
  aam=""
  if [ -x "$HOME/Applications/Ojak.app/Contents/MacOS/aam" ]; then
    aam="$HOME/Applications/Ojak.app/Contents/MacOS/aam"
  elif [ -x "/Applications/Ojak.app/Contents/MacOS/aam" ]; then
    aam="/Applications/Ojak.app/Contents/MacOS/aam"
  fi
  if [ -z "$aam" ]; then
    echo "Ojak.app was not found in ~/Applications or /Applications." >&2
    exit 1
  fi
  "$aam" deactivate
  remove_app "$HOME/Applications/Ojak.app"
  remove_app "/Applications/Ojak.app"
  echo "Ojak is deactivated. Accounts, profiles, and logs under ~/Library/Application Support/AI Account Manager were kept."
  exit 0
fi

workdir=$(/usr/bin/mktemp -d "${TMPDIR:-/tmp}/ojak-install.XXXXXX")
api="$workdir/release.json"
if ! /usr/bin/curl -fsSL --proto '=https' --tlsv1.2 -A "ojak-install" -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/${OJAK_REPO}/releases/latest" -o "$api"; then
  echo "Could not read the latest GitHub release for ${OJAK_REPO}." >&2
  echo "If no release is published yet, build from source. Otherwise open https://github.com/${OJAK_REPO}/releases/latest and retry. Nothing was installed." >&2
  exit 1
fi

# name과 URL이 한 줄에 있어도, GitHub가 줄을 나눠도 둘 다 찾는다.
pick=$(awk '
  function field(line, key,    rest) {
    rest = line
    key = "\"" key "\""
    if (index(rest, key) == 0) return ""
    rest = substr(rest, index(rest, key))
    if (sub(/^"[^"]+"[[:space:]]*:[[:space:]]*"/, "", rest) != 1) return ""
    sub(/".*/, "", rest)
    return rest
  }
  function consider(line,    name_field, url_field, row) {
    name_field = field(line, "name")
    if (name_field != "") name = name_field
    url_field = field(line, "browser_download_url")
    if (url_field == "" || name == "") return
    if (length(name) >= 4 && substr(name, length(name) - 3) == ".dmg") {
      if (index(name, "aarch64") > 0 || index(name, "arm64") > 0) preferred = url_field "\t" name
    }
    if (name == "SHA256SUMS") sums = url_field
    name = ""
  }
  {
    line = $0
    gsub(/"name"/, "\n\"name\"", line)
    gsub(/"browser_download_url"/, "\n\"browser_download_url\"", line)
    count = split(line, parts, "\n")
    for (i = 1; i <= count; i++) consider(parts[i])
  }
  END {
    if (preferred != "") print preferred
    if (sums != "") print sums "\tSHA256SUMS"
  }
' "$api")

dmg_url=$(printf '%s\n' "$pick" | awk -F '\t' '$2 != "SHA256SUMS" { print $1; exit }')
dmg_name=$(printf '%s\n' "$pick" | awk -F '\t' '$2 != "SHA256SUMS" { print $2; exit }')
sums_url=$(printf '%s\n' "$pick" | awk -F '\t' '$2 == "SHA256SUMS" { print $1; exit }')

if [ -z "$dmg_url" ] || [ -z "$dmg_name" ] || [ -z "$sums_url" ]; then
  echo "The latest release is missing an arm64 DMG or SHA256SUMS." >&2
  echo "Open https://github.com/${OJAK_REPO}/releases/latest and confirm both files are attached. Nothing was installed." >&2
  exit 1
fi

case "$dmg_name" in
  *[!A-Za-z0-9._-]*) echo "Refusing an unexpected DMG name: $dmg_name" >&2; exit 1 ;;
esac

release_prefix="https://github.com/${OJAK_REPO}/releases/download/"
case "$dmg_url" in
  "$release_prefix"* ) ;;
  *)
    echo "Refusing a DMG URL that is not an ${OJAK_REPO} release asset." >&2
    echo "Expected ${release_prefix}<tag>/<file>. Nothing was installed." >&2
    exit 1
    ;;
esac
case "$sums_url" in
  "$release_prefix"*/SHA256SUMS) ;;
  *)
    echo "Refusing a SHA256SUMS URL that is not an ${OJAK_REPO} release asset." >&2
    echo "Expected ${release_prefix}<tag>/SHA256SUMS. Nothing was installed." >&2
    exit 1
    ;;
esac
case "$dmg_url$sums_url" in
  *[[:space:]]*|*'..'*)
    echo "Refusing a release URL with whitespace or '..'. Nothing was installed." >&2
    exit 1
    ;;
esac

if ! /usr/bin/curl -fsSL --proto '=https' --tlsv1.2 -A "ojak-install" -L "$sums_url" -o "$workdir/SHA256SUMS"; then
  echo "Could not download SHA256SUMS." >&2
  echo "Check https://github.com/${OJAK_REPO}/releases/latest and retry. Nothing was installed." >&2
  exit 1
fi
if ! /usr/bin/curl -fsSL --proto '=https' --tlsv1.2 -A "ojak-install" -L "$dmg_url" -o "$workdir/$dmg_name"; then
  echo "Could not download ${dmg_name}." >&2
  echo "Check https://github.com/${OJAK_REPO}/releases/latest and retry. Nothing was installed." >&2
  exit 1
fi

# SHA256SUMS is the release workflow's integrity check for the DMG. It is not a
# minisign check: stock macOS /usr/bin/openssl cannot verify the Ed25519 updater
# signature. The in-app updater verifies Ojak.app.tar.gz.sig with the pubkey in
# tauri.conf.json. Do not skip this hash check.
expected=$(awk -v name="$dmg_name" '$2 == name { print $1; exit }' "$workdir/SHA256SUMS")
actual=$(/usr/bin/shasum -a 256 "$workdir/$dmg_name" | awk '{ print $1 }')
if ! printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{64}$'; then
  echo "SHA256SUMS has no valid SHA-256 line for ${dmg_name}." >&2
  echo "The release is incomplete. Check https://github.com/${OJAK_REPO}/releases/latest. Nothing was installed." >&2
  exit 1
fi
if ! printf '%s\n' "$actual" | grep -Eq '^[0-9a-f]{64}$' || [ "$expected" != "$actual" ]; then
  echo "SHA256 mismatch for ${dmg_name}. The download was not installed." >&2
  echo "Expected: ${expected}" >&2
  echo "Actual:   ${actual}" >&2
  echo "Retry, or download the DMG and SHA256SUMS from https://github.com/${OJAK_REPO}/releases/latest and run: shasum -a 256 ${dmg_name}" >&2
  exit 1
fi

mnt="$workdir/mnt"
/bin/mkdir -p "$mnt"
/usr/bin/hdiutil attach -nobrowse -readonly -mountpoint "$mnt" "$workdir/$dmg_name" >/dev/null
app=""
if [ -d "$mnt/Ojak.app" ]; then
  app="$mnt/Ojak.app"
else
  for candidate in "$mnt"/*.app; do
    if [ -d "$candidate" ]; then
      app="$candidate"
      break
    fi
  done
fi
if [ -z "$app" ]; then
  echo "The DMG does not contain Ojak.app." >&2
  exit 1
fi

if [ "$SYSTEM" -eq 1 ]; then
  dest="/Applications"
  /usr/bin/sudo /bin/rm -rf "$dest/Ojak.app"
  /usr/bin/sudo /usr/bin/ditto "$app" "$dest/Ojak.app"
else
  dest="$HOME/Applications"
  /bin/mkdir -p "$dest"
  /bin/rm -rf "$dest/Ojak.app"
  /usr/bin/ditto "$app" "$dest/Ojak.app"
fi

/usr/bin/hdiutil detach "$mnt" >/dev/null
mnt=""

installed="$dest/Ojak.app"
if [ "$CLEAR_QUARANTINE" -eq 1 ]; then
  echo "--clear-quarantine was set. Removing the quarantine attribute."
  echo "This does not notarize Ojak and does not guarantee macOS will allow it to open."
  echo "Official path if macOS blocks the app: System Settings → Privacy & Security → Open Anyway"
  echo "https://support.apple.com/102445"
  /usr/bin/xattr -dr com.apple.quarantine "$installed"
fi

aam="$installed/Contents/MacOS/aam"
if [ ! -x "$aam" ]; then
  echo "Copied the app, but the bundled aam command is missing: $aam" >&2
  exit 1
fi

run_aam() {
  if ! "$aam" "$@"; then
    echo "aam $* failed." >&2
    echo "macOS may be blocking the app. Official path: System Settings → Privacy & Security → Open Anyway" >&2
    echo "https://support.apple.com/102445" >&2
    echo "After the app opens once, re-run: \"$aam\" $*" >&2
    exit 1
  fi
}

if [ "$SKIP_SERVICE" -eq 0 ]; then
  run_aam service install
fi
if [ "$SKIP_INTEGRATION" -eq 0 ]; then
  run_aam integration install
fi
if [ "$SKIP_SHELL" -eq 0 ]; then
  run_aam shell install
fi

cat <<EOF

Ojak is in $installed.
Open it from that folder. If macOS says the developer cannot be verified, use
System Settings → Privacy & Security → Open Anyway
https://support.apple.com/102445
This build is not notarized. install.sh does not bypass Gatekeeper.

Next, add your own accounts in the app. The omp bridge is optional:
  "$aam" omp-broker connect && "$aam" omp-bridge connect

Ojak is an unofficial multi-account usage view and per-account CLI launcher.
It is not affiliated with Anthropic, OpenAI, Google, or xAI.
Use only your own accounts. Do not share accounts.
The omp bridge routes subscription credentials through a local proxy and may
conflict with provider terms:
  https://code.claude.com/docs/en/legal-and-compliance
  https://openai.com/policies/row-terms-of-use/
You are responsible for how you use it. Account suspension is possible.
This installer does not claim that use is legal or safe.

New terminals pick up the shell PATH. Then: aam --help
EOF
