#!/bin/sh
# Ojak installer for macOS Apple silicon (arm64).
# POSIX sh. Repo token: change only OJAK_REPO below.
# shellcheck shell=sh
set -eu

# OJAK_REPO is the only GitHub owner/repo in this file.
OJAK_REPO=lapalai/ojak

# --- external tools: absolute paths, never PATH lookups. Tests substitute this block. ---
CURL=/usr/bin/curl
SHASUM=/usr/bin/shasum
HDIUTIL=/usr/bin/hdiutil
DITTO=/usr/bin/ditto
CODESIGN=/usr/bin/codesign
LIPO=/usr/bin/lipo
XATTR=/usr/bin/xattr
PLUTIL=/usr/bin/plutil
UNAME=/usr/bin/uname
SYSCTL=/usr/sbin/sysctl
SW_VERS=/usr/bin/sw_vers
SUDO=/usr/bin/sudo
SYSTEM_APPS=/Applications
# --- end external tools ---

SYSTEM=0
UNINSTALL=0
DRY_RUN=0
WITH_OMP=0
REQ_VERSION=""
SKIP_SERVICE=0
SKIP_INTEGRATION=0
SKIP_SHELL=0
CLEAR_QUARANTINE=0
workdir=""
mnt=""
staging=""
cleanup_staging=0
install_lock=""

# Exit codes: 0 done (ready, dry run, or setup skipped on request), 1 failed with the
# previous state kept or restored, 2 usage, 3 refused because a managed session is active
# or its state is uncertain (nothing changed), 4 installed but not ready, 5 installed and
# ready but the requested omp connection failed, 6 rollback could not restart the service.

usage() {
  cat <<'EOF'
Usage: install.sh [--version X.Y.Z] [--with-omp] [--dry-run] [--system]
                  [--skip-service] [--skip-integration] [--skip-shell] [--clear-quarantine]
       install.sh --uninstall [--system] [--dry-run]
       install.sh --help

Installs Ojak.app from a public GitHub Release into ~/Applications (or /Applications
with --system) and runs the bundled setup. The default is the latest release, resolved
once and pinned. The installer never builds from source and never installs Claude Code,
Codex, or omp. If no official CLI is installed and signed in, setup reports that as
"installed, setup incomplete".

  --version X.Y.Z     install that public release instead of the latest
  --with-omp          also run `aam setup --with-omp` (explicit opt-in). It connects an
                      EXISTING omp installation through a local proxy and may conflict
                      with provider terms. Without this flag omp is never connected.
  --dry-run           print the plan (release, files, service handling) and change nothing
                      (with --uninstall: `aam deactivate --dry-run`)
  --system            install or remove /Applications/Ojak.app (uses sudo)
  --skip-service      skip service setup; cannot replace an existing app safely
  --skip-integration  do not run `aam integration install`
  --skip-shell        do not run `aam shell install`
  --uninstall         run `aam deactivate`, then remove the app
  --clear-quarantine  opt in to `xattr -dr com.apple.quarantine` on the copied app
  -h, --help          show this help

Updating an existing install:
  * The download is verified (SHA256SUMS), copied to a staging folder next to the
    destination, and checked (arm64, version, code signature, `aam --version`) before
    anything is replaced.
  * If the Ojak service is running, it is stopped with `aam service stop`, which refuses
    while a managed session is active or its state is uncertain (exit 3, nothing
    changed). Nothing is force-quit; end the session and re-run.
  * The previous app is kept until the new one and its service are verified, and is
    restored if replacement fails. The Ojak desktop app, if open, is not closed: quit and
    reopen it after the update.
  * Re-running with the same version only re-checks and re-runs setup.

After installing, the installer runs `aam setup` (or the explicit --skip-* steps) and
then `aam setup --check`. It reports success only when `ready` is true and
`serviceVersionMismatch` is false. With --with-omp, the omp broker, bridge and observer
are also checked and `omp.error` must be empty. The last line is
OJAK_INSTALL_RESULT=<ready|installed-not-ready|installed-omp-failed|installed-setup-skipped|dry-run>.
--with-omp cannot be combined with --skip-service, --skip-integration, or --skip-shell.

--clear-quarantine is off unless you pass it. It does not notarize the app and
does not guarantee macOS will allow it to open. The official path when macOS
blocks the first launch is System Settings → Privacy & Security → Open Anyway:
https://support.apple.com/102445
EOF
}

die() {
  die_code=$1
  shift
  for die_line in "$@"; do
    printf '%s\n' "$die_line" >&2
  done
  exit "$die_code"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --system) SYSTEM=1 ;;
    --uninstall) UNINSTALL=1 ;;
    --dry-run) DRY_RUN=1 ;;
    --with-omp) WITH_OMP=1 ;;
    --version)
      [ $# -ge 2 ] || die 2 "--version needs a value such as 0.5.8."
      REQ_VERSION=$2
      shift
      ;;
    --version=*) REQ_VERSION=${1#--version=} ;;
    --skip-service) SKIP_SERVICE=1 ;;
    --skip-integration) SKIP_INTEGRATION=1 ;;
    --skip-shell) SKIP_SHELL=1 ;;
    --clear-quarantine) CLEAR_QUARANTINE=1 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

REQ_VERSION=${REQ_VERSION#v}
if [ -n "$REQ_VERSION" ] && ! printf '%s\n' "$REQ_VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  die 2 "--version must look like 0.5.8 (a public release, no pre-release suffix)."
fi
if [ "$UNINSTALL" -eq 1 ] && { [ "$WITH_OMP" -eq 1 ] || [ -n "$REQ_VERSION" ]; }; then
  die 2 "--uninstall cannot be combined with --with-omp or --version."
fi
if [ "$WITH_OMP" -eq 1 ] && { [ "$SKIP_SERVICE" -eq 1 ] || [ "$SKIP_INTEGRATION" -eq 1 ] || [ "$SKIP_SHELL" -eq 1 ]; }; then
  die 2 "--with-omp runs the full 'aam setup --with-omp' and cannot be combined with --skip-service, --skip-integration, or --skip-shell."
fi
SKIPPING=0
if [ "$SKIP_SERVICE" -eq 1 ] || [ "$SKIP_INTEGRATION" -eq 1 ] || [ "$SKIP_SHELL" -eq 1 ]; then
  SKIPPING=1
fi

if [ "$("$UNAME" -s)" != "Darwin" ]; then
  die 1 "Ojak installs on macOS only."
fi
arch=$("$UNAME" -m)
if [ "$arch" != "arm64" ]; then
  # A shell running under Rosetta reports x86_64 on an Apple silicon Mac.
  if [ "$("$SYSCTL" -in hw.optional.arm64 2>/dev/null || true)" != "1" ]; then
    die 1 "This release is for Apple silicon (arm64). Intel Macs are not supported yet. Nothing was installed."
  fi
fi
macos_major=$("$SW_VERS" -productVersion 2>/dev/null | awk -F. '{ print $1 }')
case "$macos_major" in
  ''|*[!0-9]*) die 1 "Could not read the macOS version. Nothing was installed." ;;
esac
if [ "$macos_major" -lt 13 ]; then
  die 1 "Ojak needs macOS 13 or newer (this Mac reports $macos_major). Nothing was installed."
fi

cleanup() {
  if [ -n "$mnt" ]; then
    "$HDIUTIL" detach "$mnt" >/dev/null 2>&1 || true
  fi
  if [ "$cleanup_staging" -eq 1 ] && [ -n "$staging" ]; then
    fs /bin/rm -rf "$staging" || true
  fi
  if [ -n "$workdir" ] && [ -d "$workdir" ]; then
    /bin/rm -rf "$workdir"
  fi
  if [ -n "$install_lock" ]; then
    fs /bin/rmdir "$install_lock" 2>/dev/null || true
  fi
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM HUP

# Run a filesystem command with sudo for --system.
fs() {
  if [ "$SYSTEM" -eq 1 ]; then
    "$SUDO" "$@"
  else
    "$@"
  fi
}

remove_app() {
  target="$1"
  if [ ! -d "$target" ]; then
    return 0
  fi
  parent=$(dirname "$target")
  if [ "$parent" = "$SYSTEM_APPS" ]; then
    if [ "$SYSTEM" -ne 1 ]; then
      echo "Left $target in place. Re-run with --uninstall --system to remove it." >&2
      return 0
    fi
    "$SUDO" /bin/rm -rf "$target"
  else
    /bin/rm -rf "$target"
  fi
  echo "Removed $target"
}

if [ "$UNINSTALL" -eq 1 ]; then
  aam=""
  if [ -x "$HOME/Applications/Ojak.app/Contents/MacOS/aam" ]; then
    aam="$HOME/Applications/Ojak.app/Contents/MacOS/aam"
  elif [ -x "$SYSTEM_APPS/Ojak.app/Contents/MacOS/aam" ]; then
    aam="$SYSTEM_APPS/Ojak.app/Contents/MacOS/aam"
  fi
  if [ -z "$aam" ]; then
    echo "Ojak.app was not found in ~/Applications or /Applications." >&2
    exit 1
  fi
  if [ "$DRY_RUN" -eq 1 ]; then
    "$aam" deactivate --dry-run
    echo "Dry run: would then remove the Ojak.app copies in ~/Applications and (with --system) /Applications. Nothing was changed."
    echo "OJAK_INSTALL_RESULT=dry-run"
    exit 0
  fi
  "$aam" deactivate
  remove_app "$HOME/Applications/Ojak.app"
  remove_app "$SYSTEM_APPS/Ojak.app"
  echo "Ojak is deactivated. Accounts, profiles, and logs under ~/Library/Application Support/AI Account Manager were kept."
  exit 0
fi

if [ "$SYSTEM" -eq 1 ]; then
  dest="$SYSTEM_APPS"
  other_dest="$HOME/Applications"
else
  dest="$HOME/Applications"
  other_dest="$SYSTEM_APPS"
fi
target="$dest/Ojak.app"
staging="$dest/.Ojak.app.installing"
previous="$dest/.Ojak.app.previous"
aam_home=${AAM_HOME:-"$HOME/Library/Application Support/AI Account Manager"}

# A copy of Ojak in the other scope would own the service. Never guess which one wins.
if [ -d "$other_dest/Ojak.app" ]; then
  if [ "$SYSTEM" -eq 1 ]; then
    other_flags="without --system"
  else
    other_flags="with --system"
  fi
  die 1 "Ojak.app is already installed in $other_dest. Updating $dest alongside it could leave two copies fighting over the service." \
    "Update that copy instead (re-run install.sh ${other_flags}), or remove it first with: install.sh --uninstall ${other_flags#without }. Nothing was changed."
fi

json_get() {
  "$PLUTIL" -extract "$2" raw -o - "$1" 2>/dev/null
}
json_type() {
  "$PLUTIL" -type "$2" "$1" 2>/dev/null
}

# Serialize install/recovery before inspecting the installed version.
if [ "$DRY_RUN" -eq 0 ]; then
  fs /bin/mkdir -p "$dest" || die 1 "Could not create $dest."
  if ! fs /bin/mkdir "$dest/.Ojak.app.install-lock" 2>/dev/null; then
    die 3 "Another installation or an interrupted run owns $dest/.Ojak.app.install-lock. Confirm that installer has ended before recovering its files." "OJAK_INSTALL_RESULT=refused-uncertain"
  fi
  install_lock="$dest/.Ojak.app.install-lock"
  if [ -d "$previous" ] && [ ! -e "$target" ]; then
    fs /bin/mv "$previous" "$target" || die 6 "Could not restore the previous app. Files were preserved." "OJAK_INSTALL_RESULT=rollback-failed"
    echo "Restored the previous app from an interrupted installation."
  fi
  if [ -e "$previous" ] || [ -e "$staging" ]; then
    die 6 "Interrupted installation files were preserved. Inspect $previous and $staging before retrying; do not discard a backup while a service may still use it." "OJAK_INSTALL_RESULT=rollback-failed"
  fi
fi
old_aam=""
installed_version=""
existing=0
if [ -d "$target" ]; then
  existing=1
  installed_version=$(json_get "$target/Contents/Info.plist" CFBundleShortVersionString || true)
  if [ -x "$target/Contents/MacOS/aam" ]; then
    old_aam="$target/Contents/MacOS/aam"
  fi
fi

# --- resolve the release once ---
workdir=$(/usr/bin/mktemp -d "${TMPDIR:-/tmp}/ojak-install.XXXXXX")
api="$workdir/release.json"
if [ -n "$REQ_VERSION" ]; then
  api_url="https://api.github.com/repos/${OJAK_REPO}/releases/tags/v${REQ_VERSION}"
  release_hint="https://github.com/${OJAK_REPO}/releases/tag/v${REQ_VERSION}"
else
  api_url="https://api.github.com/repos/${OJAK_REPO}/releases/latest"
  release_hint="https://github.com/${OJAK_REPO}/releases/latest"
fi
if ! "$CURL" -fsSL --proto '=https' --tlsv1.2 -A "ojak-install" -H "Accept: application/vnd.github+json" \
  "$api_url" -o "$api"; then
  echo "Could not read the GitHub release (${REQ_VERSION:-latest}) for ${OJAK_REPO}." >&2
  echo "If no such public release exists, there is nothing to install (source builds are not supported by this installer). Otherwise open ${release_hint} and retry. Nothing was installed." >&2
  exit 1
fi

tag=$(awk '
  {
    if (match($0, /"tag_name"[[:space:]]*:[[:space:]]*"[^"]*"/)) {
      t = substr($0, RSTART, RLENGTH)
      sub(/^"tag_name"[[:space:]]*:[[:space:]]*"/, "", t)
      sub(/"$/, "", t)
      print t
      exit
    }
  }
' "$api")
case "$tag" in
  v[0-9]*.[0-9]*.[0-9]*) ;;
  *) die 1 "The release has no usable version tag (got '${tag}'). Nothing was installed." ;;
esac
release_version=${tag#v}
if ! printf '%s\n' "$release_version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$'; then
  die 1 "Refusing release tag '${tag}': only plain X.Y.Z releases are installed. Nothing was installed."
fi
if [ -n "$REQ_VERSION" ] && [ "$release_version" != "$REQ_VERSION" ]; then
  die 1 "Asked for ${REQ_VERSION} but GitHub returned ${release_version}. Nothing was installed."
fi
if grep -Eq '"(draft|prerelease)"[[:space:]]*:[[:space:]]*true' "$api"; then
  die 1 "Release ${tag} is a draft or pre-release. Only published stable releases are installed. Nothing was installed."
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
  echo "Release ${tag} is missing an arm64 DMG or SHA256SUMS." >&2
  echo "Open ${release_hint} and confirm both files are attached. Nothing was installed." >&2
  exit 1
fi

case "$dmg_name" in
  *[!A-Za-z0-9._-]*) echo "Refusing an unexpected DMG name: $dmg_name" >&2; exit 1 ;;
esac

release_prefix="https://github.com/${OJAK_REPO}/releases/download/${tag}/"
case "$dmg_url" in
  "$release_prefix"* ) ;;
  *)
    echo "Refusing a DMG URL that is not an ${OJAK_REPO} ${tag} release asset." >&2
    echo "Expected ${release_prefix}<file>. Nothing was installed." >&2
    exit 1
    ;;
esac
case "$sums_url" in
  "${release_prefix}SHA256SUMS") ;;
  *)
    echo "Refusing a SHA256SUMS URL that is not an ${OJAK_REPO} ${tag} release asset." >&2
    echo "Expected ${release_prefix}SHA256SUMS. Nothing was installed." >&2
    exit 1
    ;;
esac
case "$dmg_url$sums_url" in
  *[[:space:]]*|*'..'*)
    echo "Refusing a release URL with whitespace or '..'. Nothing was installed." >&2
    exit 1
    ;;
esac

# --- decide what to do with an existing install ---
version_lt() {
  [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | /usr/bin/sort -t. -k1,1n -k2,2n -k3,3n | sed -n 1p)" = "$1" ]
}

mode=new
if [ "$existing" -eq 1 ]; then
  mode=update
  if [ -z "$installed_version" ]; then
    mode=replace
  elif [ "$installed_version" = "$release_version" ]; then
    mode=same
  elif version_lt "$release_version" "$installed_version"; then
    die 1 "Ojak $installed_version is installed and $release_version is older. Downgrades are not done automatically because the local database may not open in an older version." \
      "Pick a newer --version, or run install.sh --uninstall first. Nothing was changed."
  fi
fi

# --- read-only session probe with the INSTALLED aam (authoritative check is `service stop`) ---
probe_state=""   # none | idle | busy | uncertain
probe_note=""
probe_sessions() {
  probe_state=none
  probe_note=""
  [ "$existing" -eq 1 ] || return 0
  if [ -z "$old_aam" ]; then
    if [ -e "$aam_home/service-install.json" ] || [ -e "$aam_home/state.sqlite3" ]; then
      probe_state=uncertain
      probe_note="The existing Ojak.app has no runnable aam, so active sessions cannot be checked."
    fi
    return 0
  fi
  probe_out="$workdir/probe.json"
  probe_err="$workdir/probe.err"
  if "$old_aam" status --json >"$probe_out" 2>"$probe_err"; then
    probe_count=$(json_get "$probe_out" sessions || true)
    case "$probe_count" in
      ''|*[!0-9]*)
        probe_state=uncertain
        probe_note="The service answered but its session list could not be read."
        return 0
        ;;
    esac
    probe_busy=0
    probe_i=0
    while [ "$probe_i" -lt "$probe_count" ]; do
      probe_s=$(json_get "$probe_out" "sessions.$probe_i.state" || true)
      case "$probe_s" in
        PREPARED|STARTING|ACTIVE|SUSPECT|ORPHANED) probe_busy=$((probe_busy + 1)) ;;
        '') probe_state=uncertain; probe_note="A session state could not be read."; return 0 ;;
      esac
      probe_i=$((probe_i + 1))
    done
    if [ "$probe_busy" -gt 0 ]; then
      probe_state=busy
      probe_note="$probe_busy managed session(s) are active or not confirmed finished."
    else
      probe_state=idle
    fi
  elif grep -q 'DAEMON_UNAVAILABLE' "$probe_err"; then
    probe_state=none
  else
    probe_state=uncertain
    probe_note="The service did not answer cleanly, so active sessions cannot be ruled out."
  fi
}

refuse_busy() {
  die 3 "Not replacing Ojak: $probe_note" \
    "End those Claude Code/Codex/omp sessions yourself (nothing was force-quit), then re-run install.sh. Nothing was changed." \
    "OJAK_INSTALL_RESULT=refused-busy"
}

if [ "$mode" != "new" ] && [ "$mode" != "same" ]; then
  probe_sessions
  if [ "$probe_state" = "busy" ] || [ "$probe_state" = "uncertain" ]; then
    if [ "$DRY_RUN" -eq 0 ]; then
      refuse_busy
    fi
  fi
fi

# --- dry run ---
if [ "$DRY_RUN" -eq 1 ]; then
  echo "Ojak install plan (dry run, nothing was changed):"
  echo "  release:       ${tag} (resolved once from ${release_hint##*/})"
  echo "  asset:         ${dmg_name} + SHA256SUMS (${release_prefix})"
  echo "  destination:   ${target}"
  case "$mode" in
    new) echo "  existing:      none (new install)" ;;
    same) echo "  existing:      ${installed_version} (same version: no replacement, setup is re-run and verified)" ;;
    update|replace) echo "  existing:      ${installed_version:-unknown version} (will be replaced via a verified staging copy; kept until the new app is verified, restored on failure)" ;;
  esac
  if [ "$mode" = "update" ] || [ "$mode" = "replace" ]; then
    case "$probe_state" in
      busy|uncertain) echo "  sessions:      BLOCKED - $probe_note A real run would stop with exit 3." ;;
      idle) echo "  sessions:      none active" ;;
      *) echo "  sessions:      service not running" ;;
    esac
    if [ "$SKIP_SERVICE" -eq 1 ]; then
      echo "  service:       left as is (--skip-service)"
    elif [ "$probe_state" = "none" ]; then
      echo "  service:       nothing to stop; installed by setup"
    else
      echo "  service:       aam service stop (refuses if busy) -> replace -> aam service install"
    fi
  fi
  if [ "$SKIPPING" -eq 1 ]; then
    echo "  setup:         explicit steps only (service: $([ "$SKIP_SERVICE" -eq 1 ] && echo skipped || echo run), integration: $([ "$SKIP_INTEGRATION" -eq 1 ] && echo skipped || echo run), shell: $([ "$SKIP_SHELL" -eq 1 ] && echo skipped || echo run)); NOT verified ready"
  elif [ "$WITH_OMP" -eq 1 ]; then
    echo "  setup:         aam setup --with-omp, then aam setup --check and omp checks"
  else
    echo "  setup:         aam setup, then aam setup --check (omp not connected)"
  fi
  echo "  quarantine:    $([ "$CLEAR_QUARANTINE" -eq 1 ] && echo 'cleared on the copied app (--clear-quarantine)' || echo 'left as is')"
  echo "  not done:      no source build, no Claude Code/Codex/omp install, no credential or auth-setting changes"
  echo "OJAK_INSTALL_RESULT=dry-run"
  exit 0
fi

# --- same version: no replacement ---
installed_ok=0
if [ "$mode" = "same" ]; then
  if [ -x "$old_aam" ] && "$CODESIGN" --verify --deep --strict "$target" >/dev/null 2>&1 \
    && [ "$("$old_aam" --version 2>/dev/null | awk '{ print $2 }')" = "$release_version" ]; then
    echo "Ojak ${release_version} is already installed in ${target} and verifies. Skipping the download."
    installed_ok=1
  else
    echo "Ojak ${release_version} is installed but does not verify; replacing it from the release."
    mode=replace
    probe_sessions
    if [ "$probe_state" = "busy" ] || [ "$probe_state" = "uncertain" ]; then
      refuse_busy
    fi
  fi
fi

verify_app() {
  va_path=$1
  va_aam="$va_path/Contents/MacOS/aam"
  va_service="$va_path/Contents/MacOS/aam-service"
  if [ ! -x "$va_aam" ] || [ ! -x "$va_service" ]; then
    echo "The app is missing Contents/MacOS/aam or aam-service." >&2
    return 1
  fi
  for va_bin in "$va_aam" "$va_service"; do
    va_archs=$("$LIPO" -archs "$va_bin" 2>/dev/null || true)
    case " $va_archs " in
      *" arm64 "*) ;;
      *) echo "$(basename "$va_bin") is not an arm64 binary (architectures: ${va_archs:-unknown})." >&2; return 1 ;;
    esac
  done
  va_version=$(json_get "$va_path/Contents/Info.plist" CFBundleShortVersionString || true)
  if [ "$va_version" != "$release_version" ]; then
    echo "The app reports version '${va_version}' but the release is ${release_version}." >&2
    return 1
  fi
  if ! "$CODESIGN" --verify --deep --strict "$va_path" >/dev/null 2>&1; then
    echo "The app's code signature does not verify." >&2
    return 1
  fi
  va_run=$("$va_aam" --version 2>/dev/null | awk '{ print $2 }' || true)
  if [ "$va_run" != "$release_version" ]; then
    echo "The bundled aam did not run or reported version '${va_run}' (expected ${release_version})." >&2
    echo "macOS may be blocking the app. Official path: System Settings → Privacy & Security → Open Anyway: https://support.apple.com/102445" >&2
    return 1
  fi
  return 0
}

stopped=0
if [ "$installed_ok" -eq 0 ]; then
  if [ "$existing" -eq 1 ] && [ "$SKIP_SERVICE" -eq 1 ]; then
    die 3 "Replacing an existing app requires the guarded service stop. Remove --skip-service, or leave this app unchanged." "OJAK_INSTALL_RESULT=refused-uncertain"
  fi
  # --- download and verify ---
  if ! "$CURL" -fsSL --proto '=https' --tlsv1.2 -A "ojak-install" -L "$sums_url" -o "$workdir/SHA256SUMS"; then
    echo "Could not download SHA256SUMS." >&2
    echo "Check ${release_hint} and retry. Nothing was installed." >&2
    exit 1
  fi
  if ! "$CURL" -fsSL --proto '=https' --tlsv1.2 -A "ojak-install" -L "$dmg_url" -o "$workdir/$dmg_name"; then
    echo "Could not download ${dmg_name}." >&2
    echo "Check ${release_hint} and retry. Nothing was installed." >&2
    exit 1
  fi

  # SHA256SUMS is the release workflow's integrity check for the DMG. It is not a
  # minisign check: stock macOS /usr/bin/openssl cannot verify the Ed25519 updater
  # signature. The in-app updater verifies Ojak.app.tar.gz.sig with the pubkey in
  # tauri.conf.json. Do not skip this hash check.
  expected=$(awk -v name="$dmg_name" '$2 == name { print $1; exit }' "$workdir/SHA256SUMS")
  actual=$("$SHASUM" -a 256 "$workdir/$dmg_name" | awk '{ print $1 }')
  if ! printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{64}$'; then
    echo "SHA256SUMS has no valid SHA-256 line for ${dmg_name}." >&2
    echo "The release is incomplete. Check ${release_hint}. Nothing was installed." >&2
    exit 1
  fi
  if ! printf '%s\n' "$actual" | grep -Eq '^[0-9a-f]{64}$' || [ "$expected" != "$actual" ]; then
    echo "SHA256 mismatch for ${dmg_name}. The download was not installed." >&2
    echo "Expected: ${expected}" >&2
    echo "Actual:   ${actual}" >&2
    echo "Retry, or download the DMG and SHA256SUMS from ${release_hint} and run: shasum -a 256 ${dmg_name}" >&2
    exit 1
  fi

  # --- stage and verify before touching the installed app ---
  mnt="$workdir/mnt"
  /bin/mkdir -p "$mnt"
  if ! "$HDIUTIL" attach -nobrowse -readonly -mountpoint "$mnt" "$workdir/$dmg_name" >/dev/null; then
    mnt=""
    die 1 "Could not mount ${dmg_name}. Nothing was installed."
  fi
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

  fs /bin/mkdir -p "$dest" || die 1 "Could not create $dest. Nothing was installed."
  cleanup_staging=1
  if ! fs "$DITTO" "$app" "$staging"; then
    die 1 "Copying the app failed part-way. The staging copy was discarded; the installed Ojak and its service were not touched."
  fi
  "$HDIUTIL" detach "$mnt" >/dev/null
  mnt=""

  if [ "$CLEAR_QUARANTINE" -eq 1 ]; then
    echo "--clear-quarantine was set. Removing the quarantine attribute."
    echo "This does not notarize Ojak and does not guarantee macOS will allow it to open."
    echo "Official path if macOS blocks the app: System Settings → Privacy & Security → Open Anyway"
    echo "https://support.apple.com/102445"
    fs "$XATTR" -dr com.apple.quarantine "$staging"
  fi

  if ! verify_app "$staging"; then
    die 1 "The downloaded app failed verification. The installed Ojak and its service were not touched."
  fi

  # --- stop the service with the existing safe CLI (refuses when busy/uncertain) ---
  if [ "$existing" -eq 1 ] && [ "$SKIP_SERVICE" -eq 0 ]; then
    probe_sessions
    if [ "$probe_state" = "busy" ] || [ "$probe_state" = "uncertain" ]; then
      refuse_busy
    fi
    need_stop=0
    if [ "$probe_state" = "idle" ]; then
      need_stop=1
    elif [ -e "$aam_home/service-install.json" ]; then
      need_stop=1
    fi
    if [ "$need_stop" -eq 1 ]; then
      if [ -z "$old_aam" ]; then
        refuse_busy
      fi
      stop_out="$workdir/stop.out"
      if "$old_aam" service stop >"$stop_out" 2>&1; then
        stopped=1
      else
        cat "$stop_out" >&2
        if grep -q 'SESSION_BUSY' "$stop_out"; then
          probe_note="A managed session is active or not confirmed finished."
          refuse_busy
        fi
        die 3 "The service could not be stopped safely, so Ojak was not replaced (the app is unchanged; aam reopens new assignments itself if it had closed them)." \
          "Check: \"$old_aam\" service status. If the service is stopped, start it with: \"$old_aam\" service install, then re-run install.sh." \
          "OJAK_INSTALL_RESULT=refused-uncertain"
      fi
    fi
  fi

  # --- replace; the previous app is kept until everything verifies ---
  trap '' INT TERM HUP
  swapped=0
  if [ "$existing" -eq 1 ]; then
    if fs /bin/mv "$target" "$previous"; then
      if fs /bin/mv "$staging" "$target"; then
        swapped=1
      else
        fs /bin/mv "$previous" "$target" || true
      fi
    fi
  else
    if fs /bin/mv "$staging" "$target"; then
      swapped=1
    fi
  fi
  if [ "$swapped" -eq 1 ]; then
    cleanup_staging=0
  fi

  rollback() {
    # $1: reason. Restore the previous app and, if we stopped the service, start the old one.
    echo "Replacement failed: $1" >&2
    if [ "$existing" -eq 1 ] && [ -d "$previous" ]; then
      fs /bin/rm -rf "$target"
      if ! fs /bin/mv "$previous" "$target"; then
        trap - INT TERM HUP
        die 6 "Could not restore the previous Ojak.app from $previous. Move it back to $target by hand." \
          "OJAK_INSTALL_RESULT=rollback-failed"
      fi
      echo "Restored the previous Ojak.app (${installed_version:-unknown version})." >&2
    elif [ "$existing" -eq 0 ]; then
      fs /bin/rm -rf "$target"
      echo "Removed the incomplete new install." >&2
    fi
    if [ "$stopped" -eq 1 ]; then
      if [ -n "$old_aam" ] && "$old_aam" service install >/dev/null 2>&1; then
        echo "Restarted the previous service; new assignments are open again." >&2
      else
        trap - INT TERM HUP
        die 6 "The service is STOPPED and new assignments stay blocked. Start it with: \"$target/Contents/MacOS/aam\" service install" \
          "OJAK_INSTALL_RESULT=rollback-failed"
      fi
    fi
    trap - INT TERM HUP
    die 1 "The previous state was kept. Nothing new is installed." "OJAK_INSTALL_RESULT=rolled-back"
  }

  if [ "$swapped" -eq 0 ]; then
    rollback "could not move the new app into place"
  fi
  if ! verify_app "$target"; then
    rollback "the installed copy failed verification"
  fi
  if [ "$stopped" -eq 1 ]; then
    if ! "$target/Contents/MacOS/aam" service install; then
      rollback "the new service did not start (aam service install failed)"
    fi
  fi
  if [ -d "$previous" ]; then
    fs /bin/rm -rf "$previous"
  fi
  trap - INT TERM HUP
  trap 'exit 130' INT
  trap 'exit 143' TERM HUP
fi

installed="$target"
aam="$installed/Contents/MacOS/aam"
if [ ! -x "$aam" ]; then
  echo "Copied the app, but the bundled aam command is missing: $aam" >&2
  exit 1
fi
echo "Ojak ${release_version} is installed in ${installed} (${mode})."

# --- setup ---
results_dir="$workdir/results"
/bin/mkdir -p "$results_dir"
setup_error=""

run_aam() {
  if ! "$aam" "$@"; then
    echo "aam $* failed." >&2
    echo "macOS may be blocking the app. Official path: System Settings → Privacy & Security → Open Anyway" >&2
    echo "https://support.apple.com/102445" >&2
    echo "After the app opens once, re-run: \"$aam\" $*" >&2
    return 1
  fi
}

if [ "$SKIPPING" -eq 1 ]; then
  if [ "$SKIP_SERVICE" -eq 0 ]; then
    run_aam service install || setup_error="aam service install failed"
  fi
  if [ -z "$setup_error" ] && [ "$SKIP_INTEGRATION" -eq 0 ]; then
    run_aam integration install || setup_error="aam integration install failed"
  fi
  if [ -z "$setup_error" ] && [ "$SKIP_SHELL" -eq 0 ]; then
    run_aam shell install || setup_error="aam shell install failed"
  fi
else
  if [ "$WITH_OMP" -eq 1 ]; then
    setup_args="setup --with-omp"
  else
    setup_args="setup"
  fi
  # shellcheck disable=SC2086
  if ! "$aam" $setup_args >"$results_dir/setup.json" 2>"$results_dir/setup.err"; then
    cat "$results_dir/setup.err" >&2
    setup_error="aam $setup_args failed"
  fi
fi

# --- verify: `ready`, `serviceVersionMismatch`, notices, and omp are judged separately ---
problems=""
add_problem() {
  problems="${problems}
  - $1"
}
ready_value=""
mismatch_value=""
check_json="$results_dir/check.json"
if [ -n "$setup_error" ]; then
  add_problem "$setup_error (installed files are in place; setup did not finish)."
fi

if "$aam" setup --check >"$check_json" 2>"$results_dir/check.err"; then
  ready_value=$(json_get "$check_json" ready || true)
  mismatch_value=$(json_get "$check_json" serviceVersionMismatch || true)
  case "$ready_value" in
    true|false) ;;
    *) ready_value=""; add_problem "aam setup --check did not return a readable ready flag, so readiness could not be verified." ;;
  esac
else
  cat "$results_dir/check.err" >&2
  add_problem "aam setup --check failed, so readiness could not be verified."
fi

tools_summary=""
accounts_found=0
if [ -n "$ready_value" ]; then
  if [ "$ready_value" != "true" ]; then
    add_problem "setup --check reported ready=${ready_value}."
  fi
  if [ "$mismatch_value" != "false" ]; then
    add_problem "serviceVersionMismatch is '${mismatch_value:-missing}': the running service is not ${release_version}. When no managed session is active run: \"$aam\" service restart"
  fi
  n_tools=$(json_get "$check_json" tools || true)
  case "$n_tools" in ''|*[!0-9]*) n_tools=0 ;; esac
  ti=0
  while [ "$ti" -lt "$n_tools" ]; do
    t_name=$(json_get "$check_json" "tools.$ti.tool" || true)
    t_account=$(json_get "$check_json" "tools.$ti.account" || true)
    t_connected=$(json_get "$check_json" "tools.$ti.connected" || true)
    if [ "$(json_type "$check_json" "tools.$ti.verified" || true)" = "bool" ]; then
      t_verified=$(json_get "$check_json" "tools.$ti.verified" || true)
    else
      t_verified="unchecked"
    fi
    tools_summary="${tools_summary}
    ${t_name}: account=${t_account} connected=${t_connected} command-verified=${t_verified}"
    [ "$t_account" = "true" ] && accounts_found=1
    if [ "$t_account" = "true" ] && { [ "$t_connected" != "true" ] || [ "$t_verified" != "true" ]; }; then
      add_problem "A configured CLI account did not pass connection and command verification."
    fi
    ti=$((ti + 1))
  done
  if [ "$accounts_found" -eq 0 ] && [ "$SKIPPING" -eq 0 ]; then
    add_problem "No runnable Claude Code or Codex account was found. Ojak does not install those CLIs: install the official CLI yourself and sign in with its own login, then run: \"$aam\" setup --check"
  fi
  n_notices=$(json_get "$check_json" notices || true)
  case "$n_notices" in ''|*[!0-9]*) n_notices=0 ;; esac
  ni=0
  notices_summary=""
  while [ "$ni" -lt "$n_notices" ]; do
    n_code=$(json_get "$check_json" "notices.$ni.code" || true)
    n_msg=$(json_get "$check_json" "notices.$ni.message" || true)
    case "$n_code" in
      service-missing|service-version-mismatch|missing-shim|command-mismatch|shell-error|shell-timeout|unsupported-shell|path-shadow|windows-mismatch)
        add_problem "notice ${n_code}: ${n_msg}" ;;
      *) notices_summary="${notices_summary}
    ${n_code}: ${n_msg}" ;;
    esac
    ni=$((ni + 1))
  done
fi

omp_failed=0
omp_summary="not requested"
omp_present=""
[ "$(json_type "$check_json" omp 2>/dev/null || true)" = "dictionary" ] && omp_present=1
if [ "$WITH_OMP" -eq 1 ]; then
  omp_summary=""
  if [ -z "$omp_present" ]; then
    omp_failed=1
    omp_summary="no omp result was reported (omp is not detected on this Mac; Ojak does not install omp, so install and sign in to omp first, then re-run with --with-omp)"
  else
    ob=$(json_get "$check_json" omp.broker || true)
    og=$(json_get "$check_json" omp.bridge || true)
    oo=$(json_get "$check_json" omp.observer || true)
    omp_summary="broker=${ob} bridge=${og} observer=${oo}"
    if [ "$ob" != "true" ] || [ "$og" != "true" ] || [ "$oo" != "true" ]; then
      omp_failed=1
    fi
    if [ "$(json_type "$check_json" omp.error 2>/dev/null || true)" = "string" ]; then
      omp_failed=1
      omp_summary="${omp_summary} error=$(json_get "$check_json" omp.error || true)"
    fi
    if [ -f "$results_dir/setup.json" ] && [ "$(json_type "$results_dir/setup.json" omp.error 2>/dev/null || true)" = "string" ]; then
      omp_failed=1
      omp_summary="${omp_summary} setup-error=$(json_get "$results_dir/setup.json" omp.error || true)"
    fi
  fi
elif [ -n "$omp_present" ]; then
  omp_summary="detected on this Mac but NOT connected (not requested; re-run with --with-omp to opt in)"
fi

echo
echo "Ojak ${release_version} in ${installed}"
if [ -n "$tools_summary" ]; then
  echo "  tools:${tools_summary}"
fi
echo "  ready=${ready_value:-unknown} serviceVersionMismatch=${mismatch_value:-unknown}"
echo "  omp: ${omp_summary}"
if [ -n "${notices_summary:-}" ]; then
  echo "  other notices:${notices_summary}"
fi

if [ "$SKIPPING" -eq 1 ] && [ -z "$setup_error" ]; then
  echo
  echo "Installed. Setup was NOT fully run because you passed --skip-* options, so readiness is not verified."
  if [ -n "$problems" ]; then
    echo "Open items:${problems}"
  fi
  echo "OJAK_INSTALL_RESULT=installed-setup-skipped"
  exit 0
fi

if [ -n "$problems" ]; then
  echo
  echo "Installed, setup incomplete. Ojak files are in place but this is NOT ready:${problems}"
  echo "Re-running install.sh is safe once the cause is fixed. Existing logins and settings were not changed."
  echo "OJAK_INSTALL_RESULT=installed-not-ready"
  exit 4
fi

if [ "$omp_failed" -eq 1 ]; then
  echo
  echo "Installed and ready for Claude Code/Codex, but the requested omp connection is NOT complete: ${omp_summary}"
  echo "Check with: \"$aam\" omp-broker status; \"$aam\" omp-bridge status; \"$aam\" omp-observer status"
  echo "OJAK_INSTALL_RESULT=installed-omp-failed"
  exit 5
fi

cat <<EOF

Ojak is installed and verified (ready=true, no service version mismatch).
If the Ojak app was already open, quit and reopen it. Existing terminals keep their old PATH:
open a new terminal, then: aam --help
If macOS says the developer cannot be verified, use
System Settings → Privacy & Security → Open Anyway
https://support.apple.com/102445
This build is not notarized. install.sh does not bypass Gatekeeper.

omp is optional and never connected unless you pass --with-omp. After --with-omp, existing
omp sessions are not migrated: start a new omp session and choose an ojak-* model.

Ojak is an unofficial multi-account usage view and per-account CLI launcher.
It is not affiliated with Anthropic, OpenAI, Google, or xAI.
Use only your own accounts. Do not share accounts.
The omp bridge routes subscription credentials through a local proxy and may
conflict with provider terms:
  https://code.claude.com/docs/en/legal-and-compliance
  https://openai.com/policies/row-terms-of-use/
You are responsible for how you use it. Account suspension is possible.
This installer does not claim that use is legal or safe.
OJAK_INSTALL_RESULT=ready
EOF
