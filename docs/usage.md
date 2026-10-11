# Ojak user guide

[Back to README](../README.md) · **English** · [한국어](usage.ko.md) · [Bahasa Indonesia](usage.id.md)

Start with the [download and setup steps](../README.md#download). This guide covers account selection, optional integrations, updates, and troubleshooting. Ojak is an early preview; use only your own accounts and review the [disclaimer](../README.md#disclaimer).

## Account selection

Ojak selects an account for a new managed CLI session, not for every message. A running conversation stays with that account. Available quota is not the only condition: login status, fresh usage data, concurrency slots, and your allocation settings also matter. This is not a reservation of quota with the provider.

If the terminal reports `NO_ELIGIBLE_ACCOUNT` and says it is running the original CLI, that launch is **not managed by Ojak** and will not appear as a managed session. This fallback applies to the connected `claude`/`codex` commands; `aam run` does not fall back to an unmanaged launch. Authentication overrides and unsupported options are not silently bypassed.

Inspect the reasons without reserving an account:

```sh
aam explain --tool claude
# For Codex:
aam explain --tool codex
```

| Code | What it means |
|---|---|
| `QUOTA_EXHAUSTED` | An applicable usage limit has been reached. |
| `QUOTA_STALE` / `QUOTA_UNKNOWN` | Usage data is too old or unavailable for automatic selection. Refresh usage and check the account's login. |
| `CAPACITY_RESERVED` | Another session holds a slot, or Ojak has not confirmed that it ended. |
| `MODEL_LIMIT_GUARD` | A model-specific limit is exhausted. This is a warning when the CLI chooses the model; explicitly requesting that model can exclude the account. |
| `RESERVE_DEPRIORITIZED` / `RESERVE_FALLBACK` | The account is within the safety reserve. Ojak prefers other eligible accounts with more quota, but can use the remaining quota if none are available. These are priority explanations, not a hard ban. |

### Continue after a limit is reached

When an interactive managed session exits after using up its quota, Ojak can offer to reopen the same conversation on another eligible account. Accept the `[Y/n]` prompt to proceed. If usage data has not caught up, or the prompt cannot be shown, run this in the same project folder:

```sh
aam continue
```

Ojak copies the conversation history, not the login credentials. The conversation reopens in a new process; it is not an uninterrupted switch inside the running CLI. The first request may consume more tokens because the prompt cache is rebuilt. Claude Code may also show its first-run screens for a profile that has not been used interactively before.

### Shared settings

Account credentials are not shared. On macOS, Ojak links supported extension folders from `~/.claude` and `~/.codex` into managed profiles and copies selected settings. On Windows, it copies selected settings but does not link those folders. Do not assume every skill or plugin folder is shared on Windows. The exact allowlists are in the [launcher documentation](domains/launcher.md).

### Authentication conflicts

`AUTH_OVERRIDE_CONFLICT` means an API key, custom base URL, auth helper, or other conflicting option would change how the account authenticates. Ojak stops rather than silently rewriting your configuration. Review the named setting; never paste its secret value into an issue. Manage account logins through Ojak's account controls rather than auth-changing commands through the managed shim.

## omp integration

**Optional, off by default.** The usage view and managed Claude Code/Codex launches do not require the bridge. Enabling it passes subscription credentials through a local proxy and may conflict with provider terms. Read the [disclaimer](../README.md#disclaimer) first.

```sh
aam omp-broker connect && aam omp-bridge connect
```

Ojak registers providers named `ojak-*` for the available logins. Open a **new omp session** and select an `ojak-*` model with `/model`. Requests using the original providers do not go through Ojak's account assignment.

- **Claude / Codex:** use either an official CLI account registered in Ojak or the original provider login in omp. You do not need to sign in twice.
- **Gemini (Antigravity), Grok, Z.AI:** use the original provider login in omp.
- Accounts added later are normally picked up within about a minute.
- Claude uses its original keychain or credential file; Codex uses file-backed auth profiles. Official CLIs handle renewal. Ojak does not automatically change a different Codex credential store to file storage.

See the [bridge documentation](domains/omp-bridge.md) for implementation details.

## Token estimates and provider charges

The token-and-cost view reads local omp, Claude Code, and Codex records for today, this month, and all time. API-price estimates are **not your subscription bill, a credit balance, or a spending cap**. Unknown model prices are shown separately. The first scan can continue in the background after the window closes; the characters shown during scanning are not a usage chart.

Two optional settings let Ojak assign new work to **Codex credits** or **Claude extra usage** after subscription quota is exhausted. Both are off by default. When enabled, eligible accounts with subscription quota still take priority; after a reset, new assignments can use subscription quota again.

The provider may charge you. Ojak cannot see the amount charged or stop an already-running official CLI from switching to a paid usage path. A confirmation before an unmanaged launch is not a guarantee against all provider charges. Credit-balance support depends on what Codex reports, and the credit/extra-usage path has not yet been confirmed with a real paid account.

## Updates

Use the update check in **Settings → About**. Builds configured with the updater public key also check at startup and every 24 hours, and show an update badge in the menu bar/tray popover. Builds without a configured key do not show the updater UI. Updater signatures are separate from Apple notarization and Windows code signing.

**Finish your work before updating.** Installing an update restarts the Ojak service and can briefly interrupt omp bridge sessions. If there are managed sessions or recent bridge requests, the app offers an update/later choice. The Windows installer stops if it finds active or uncertain managed sessions.

If in-app updating is unavailable, download the new DMG or setup.exe from [Releases](https://github.com/lapalai/ojak/releases/latest). A signing-key change can require manual installation; the 0.1.0 → 0.1.1 transition was one such case.

### After a manual reinstall

- **Mac:** replacing `Ojak.app` does not necessarily replace the running service. Open the app and use **Restart service** if a version-mismatch notice appears. The safe restart refuses while managed sessions are active or uncertain and warns about recent omp activity. The CLI equivalent is `aam service restart`; check the result with `aam service status` or `aam setup --check` (`serviceVersionMismatch`).
- **Windows:** run setup.exe as the same normal user. It checks sessions, stops the app-owned service, replaces and verifies the files, and restarts the service if it was registered to start at sign-in. Quit Orca and omp first.

For maintainers only, a direct macOS restart is `launchctl kickstart -k gui/$(id -u)/ai.aam.service`, followed by `aam integration install`. Unlike the safe restart action, this does **not** check managed sessions first.

## Windows installation

Do not install or run the installer as administrator. Verify that the installer came from [this repository's releases](https://github.com/lapalai/ojak/releases/latest). If antivirus quarantines `aam.exe` or `aam-service.exe`, review the detection and source before considering a file-specific exception; do not turn off protection globally. If unsure, report the detection without credentials or personal data.

If setup says it could not safely stop Ojak:

1. Finish running Claude Code/Codex tasks, then quit **Orca and every omp process** normally. Closing only an Orca window may leave its observer running and lock `aam.exe`.
2. Retry setup. When it stops at this safety check, it has not replaced the application files.
3. If the installer details mention `installer recover`, an earlier install may have been interrupted. With Orca/omp closed, [report the issue](https://github.com/lapalai/ojak/issues) rather than editing the database or forcing file replacement.

Inspect the installer's details and `%TEMP%\Ojak-installer.log`. Starting with 0.6.2, failures record the stage (`stage`), helper exit result (`exit`), and actual error code (`code`). Older generic messages do not establish that running work caused the failure. Review and redact any log before sharing it.

- `SESSION_BUSY` / `INSTALLER_FILES_BUSY`: finish running work, quit Orca/omp normally, then retry.
- `OMP_CONFIG_FAILED`: previous omp connection settings could not be restored. Check the omp installation and executable. An installation never connected to omp can be removed without omp.
- `LEASE_STATE_UNKNOWN`: the service is stopped and saved session safety cannot be established. Restart the Ojak service and check running work; do not delete the database or leases manually.
- `HELPER_RESULT_UNAVAILABLE`: the helper did not leave an error-code file. `exit=error` means launch failed; `exit=timeout` means nsExec timed out. Check installer details and antivirus quarantine history.

## A session's slot stays reserved

Ojak does not release a slot merely because time has passed. Check the session state:

- **`SUSPECT`:** the launch result or process liveness is not confirmed. This often resolves while the launcher is still alive.
- **`ORPHANED`:** supervision was lost before the launcher could report completion. Descendant processes may still exist.

A suspended `claude -p` process or a launcher terminated with `kill -9` can cause this. Finish the specific task normally first. On **macOS**, you can inspect suspended processes; `T` in `STAT` means suspended:

```sh
ps -o pid,ppid,stat,command -ax | grep '[c]laude -p'
```

Only after identifying your own stopped process and deciding to end it, use `kill -TERM <pid> && kill -CONT <pid>`. Do not paste a PID from someone else's example or terminate unrelated tasks. `CONT` lets the stopped process handle the termination signal. If the launcher also dies, its completion report may still be lost.

An orphaned slot with recorded descendants can be released once those descendants and the original process group are confirmed gone. Without descendant records, Ojak may need a reboot to establish that nothing remains. After a reboot, the service can release it. **Do not edit the service database to free a slot.** Other eligible accounts can still receive work.

## Advanced macOS installation

Read [install.sh](../install.sh), then save a copy in a new working directory and review it before running:

```sh
curl -fsSL https://raw.githubusercontent.com/lapalai/ojak/main/install.sh -o install.sh
```

```sh
sh install.sh --dry-run
sh install.sh
```

The script resolves a public release, checks the arm64 DMG against `SHA256SUMS`, and installs to `~/Applications/Ojak.app`. Existing installations must pass the safe service-stop check before replacement; active or uncertain managed sessions block the update. Setup is verified with `aam setup --check`, not `--status`. A checksum is not Apple notarization. An installed app with incomplete setup is reported separately from a ready installation.

| Option | Effect |
|---|---|
| `--version 0.5.8` | Select that public release instead of the latest release. |
| `--dry-run` | Show the plan without installing or changing settings. |
| `--with-omp` | Also connect an existing omp installation; explicit opt-in, subject to the [integration risks](#omp-integration). |
| `--system` | Install in `/Applications`; only this installation mode uses sudo. |
| `--skip-service` | Skip service setup for a new or same-version install. Replacing an existing app requires a guarded stop, so this option cannot be used for replacement. |
| `--skip-integration` | Skip `aam integration install`. |
| `--skip-shell` | Skip `aam shell install`. |
| `--clear-quarantine` | Explicitly remove the copied app's quarantine attribute. Off by default; not notarization. Prefer the normal macOS approval flow. |

To set up the service and terminal integration without the desktop setup flow, use `aam setup`. Automatic shell PATH setup is for zsh; follow the setup diagnostics for other shells.

## Windows terminal installation

Read [install.ps1](../install.ps1), then save it in a new working directory using ordinary-user PowerShell:

```powershell
Invoke-WebRequest -UseBasicParsing https://raw.githubusercontent.com/lapalai/ojak/main/install.ps1 -OutFile install.ps1
```

Review the downloaded script before running:

```powershell
.\install.ps1 -DryRun
.\install.ps1
```

Use `-Version 0.5.8` to choose a public release, `-WithOmp` to opt in to an existing omp integration, and `-Help` for usage. The script verifies the x64 installer checksum and invokes the normal NSIS installer with its session/file-lock safeguards. Quit Orca and omp before updating; do not let an agent terminate its own session. If local script execution is blocked, use the manual setup.exe installation rather than disabling security policy. The unsigned-installer warning still applies.

For agent-driven installation on either OS, use the [shared agent guide](install-for-agents.md). Login approval can still require your input. `aam setup --status` does not check commands and must not be polled for `ready: true`.


## Uninstall and data locations

**Mac:** run `aam deactivate` to undo the connections Ojak added. To remove the app bundle too, run `sh install.sh --uninstall` from a repository checkout or a directory containing the reviewed install script. Use `--uninstall --system` for a copy installed in `/Applications`.

**Windows:** use **Settings → Apps → Ojak → Uninstall**. Ojak restores its omp connections before stopping the service. Removal stops if managed tasks are still running; finish them or restart the PC and try again. `aam deactivate` also restores the integrations without removing the app.

Accounts, profiles, and logs are retained after uninstalling:

| Platform | Ojak data directory |
|---|---|
| macOS | `~/Library/Application Support/AI Account Manager` |
| Windows | `%APPDATA%\AI Account Manager` |

This directory includes SQLite account/allocation records, `integration.json`, `ui-settings.json`, and logs. Credentials remain in the relevant official CLI credential stores or profiles; do not share profile folders. Ojak does not send usage telemetry or upload diagnostics. Provider requests and GitHub update checks still use the network. See [Security](../SECURITY.md) for credential handling and private reporting.
