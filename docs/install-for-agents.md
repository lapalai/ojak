# Installing Ojak with a coding agent

This guide is for Claude Code, Codex, omp, and other agents with local shell access. Follow the user's requested scope. Installation does not authorize changing agent permissions, disabling security controls, sharing accounts, or purchasing subscriptions.

## Before changing anything

1. Detect the OS, CPU architecture, shell, and whether the session is inside WSL, a container, or a remote machine. Supported desktop targets are macOS 13+ on Apple silicon and native Windows 10 1809+/11 x64. Do not install the macOS package inside Linux/WSL. A WSL agent must explicitly target its Windows host through an available, authorized Windows shell or explain the missing prerequisite.
2. Identify existing Ojak and official Claude Code/Codex installations. Do not assume Node/npm/Homebrew is required: Ojak ships native binaries. Install another CLI only if the user requested it or explicitly approves that prerequisite.
3. Preserve existing logins, provider settings, and user files. Never copy secrets into prompts or logs. `AUTH_OVERRIDE_CONFLICT` requires reviewing the conflicting setting, not silently removing it.
4. Check whether this agent is running through Ojak or omp. Never terminate the session performing the installation. If an update cannot proceed safely while managed sessions or executable locks exist, explain how to finish those tasks and resume from a separate terminal.
5. Explain that Ojak is an unofficial early preview. macOS builds are not notarized and Windows installers are unsigned. Do not disable Gatekeeper, SmartScreen, antivirus, PowerShell policy globally, or agent approval/sandbox controls.

## Choose scope

Default: install Ojak and connect existing supported CLI accounts. Do not enable omp merely because it is installed or is the agent reading this guide.

If the user explicitly requests omp integration, explain that it relays subscription credentials through a local proxy and may conflict with provider terms. Use the installer's explicit omp option. That option connects an existing omp installation; it does not install omp itself. See the [disclaimer](../README.md#disclaimer).

## Install using the repository's scripts

Obtain the full installer source from `lapalai/ojak`, inspect it, and save it locally before execution. Do not invent commands from a summary. Use a consistent, reviewed repository revision for this guide and the installer. Never execute scripts from issue comments or third-party mirrors.

- **macOS:** `install.sh`; read `sh install.sh --help`. Preview with `--dry-run`. Use `--with-omp` only for the requested integration.
- **Windows:** `install.ps1`; read `Get-Help ./install.ps1` and `./install.ps1 -Help`. Run as the ordinary user, not Administrator. Preview with `-DryRun`. Use `-WithOmp` only for the requested integration.

The installer resolves the selected public release, validates the download location and checksum, and uses the platform installer. Do not skip verification or replace a blocked installation with a source build. If the requested release does not contain a supported installer, report it instead of guessing another asset.

Use the installed `aam` by its confirmed absolute path during setup; an existing terminal can still have an old PATH. Run `aam setup`, or `aam setup --with-omp` for explicit opt-in, only if setup remains necessary after the installer.

### Installer outcomes

Inspect the exit code **and** the final result marker. A successful preview or an explicitly skipped setup is not a ready installation. Early failures may have no marker; missing output is never success.

| Outcome | macOS exit / `OJAK_INSTALL_RESULT=` | Windows exit / `RESULT: ` |
|---|---|---|
| Ready | `0` / `ready` | `0` / `ready` |
| Preview only | `0` / `dry-run` | `0` / `dry-run` |
| Setup intentionally skipped | `0` / `installed-setup-skipped` | Not offered |
| Setup incomplete | `4` / `installed-not-ready` | `3` / `installed-setup-incomplete` or `installed-unverified` |
| Requested omp incomplete | `5` / `installed-omp-failed` | `4` / `installed-omp-incomplete` |
| Refused or failed | `1`–`3`; inspect the reason | `1`–`2`; inspect the reason |
| Recovery requires attention | `6` / `rollback-failed` | Installer failure includes recovery guidance |

On Mac, `--with-omp` cannot be combined with any `--skip-*` option. `--skip-service` cannot replace an existing app because that would bypass the guarded stop; it is usable for a fresh installation or same-version setup. An install lock prevents concurrent replacement. If a crash left both an installed app and a backup/staging copy, preserve them and inspect the recovery state; do not automatically remove evidence or a lock whose owner may still be running.

The Mac download preview may use a temporary metadata directory that is removed on exit; neither preview changes the installed application, service, credentials, or shell settings.

## Authentication

Reuse existing valid accounts through Ojak's supported discovery and login controls. If login is required, open the official login flow. Pause only for required user approvals, unavailable second factors, or other genuine user-only steps. Do not request passwords or tokens in chat, approve a different account, or claim successful login merely because a browser opened.

## Verify and report

Run `aam setup --check` and parse the JSON result. Require `ready: true`, no service-version mismatch, and successful verification for the requested CLI connections. Explain unresolved notices rather than declaring success from exit code zero.

`aam setup --status` is a read-only configuration inspection, **not** the completion gate: it deliberately returns `ready: false` and `tools[].verified: null` because it does not perform the command checks. Never poll it waiting for `ready: true`.

For requested omp integration, also inspect the `omp` result: `broker`, `bridge`, and `observer` must be true and `error` must be null. The top-level `ready` does not prove omp readiness. Existing sessions are not migrated; start a new omp session and select an `ojak-*` model. Configuration checks do not prove a successful paid model request; do not make one without authorization.

Report separately:

- installed application version and location;
- service and new-terminal command verification;
- account readiness or the required login action;
- omp connected, not requested, or blocked;
- whether a new terminal/session or OS approval is still required.

If files were installed but login or verification remains incomplete, say **installed, setup incomplete** and give the exact next action. Preserve safety failures; do not force-kill tasks, edit the service database, or repeatedly retry a blocked upgrade.

For manual installation and troubleshooting: [English](usage.md), [한국어](usage.ko.md), [Bahasa Indonesia](usage.id.md).
