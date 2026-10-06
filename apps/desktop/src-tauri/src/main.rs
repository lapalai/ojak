#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use aam_protocol::{ApiError, LaunchIntent, Paths, Snapshot};
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, LazyLock,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager,
};

/// 앱 종료를 사용자가 확인했는지. 확인 전 종료 요청(⌘Q·Dock·트레이)은 화면의 확인 창으로 돌린다.
static QUIT_CONFIRMED: AtomicBool = AtomicBool::new(false);
const CONFIRM_QUIT_EVENT: &str = "ojak://confirm-quit";

/// 종료 확인 창을 띄운다. 창이 숨겨져 있으면 먼저 보여 준다.
fn request_quit(app: &tauri::AppHandle) {
    show_main(app);
    let _ = app.emit(CONFIRM_QUIT_EVENT, ());
}

/// 대시보드를 앞으로 가져온다. 창을 닫아 트레이로만 있던 동안 숨겼던 Dock 아이콘도 되살린다.
fn show_main(app: &tauri::AppHandle) {
    if let Some(popover) = app.get_webview_window(POPOVER) {
        let _ = popover.hide();
    }
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

const POPOVER: &str = "popover";
const POPOVER_WIDTH: f64 = 340.0;

/// 트레이(메뉴바) 아이콘 옆에 팝오버를 연다. 열려 있으면 닫는다.
/// 듀얼 모니터에서는 팝오버가 마지막에 있던 모니터가 아니라 **아이콘이 있는 모니터**의 배율과 작업 영역(작업 표시줄 제외)을 기준으로
/// 계산한다. 왼쪽·위쪽 보조 모니터는 좌표가 음수이므로 0이 아니라 그 모니터 경계로 자른다.
fn toggle_popover(app: &tauri::AppHandle, rect: tauri::Rect) {
    let Some(popover) = app.get_webview_window(POPOVER) else { return };
    if popover.is_visible().unwrap_or(false) {
        let _ = popover.hide();
        return;
    }
    let fallback = popover.scale_factor().unwrap_or(1.0);
    // 아이콘 중심을 품은 모니터를 직접 찾는다. 모니터마다 배율이 다르므로 그 모니터 배율로 바꿔 비교한다.
    let contains = |m: &tauri::Monitor| {
        let p = rect.position.to_physical::<f64>(m.scale_factor());
        let s = rect.size.to_physical::<f64>(m.scale_factor());
        let (cx, cy) = (p.x + s.width / 2.0, p.y + s.height / 2.0);
        let (mx, my) = (m.position().x as f64, m.position().y as f64);
        cx >= mx && cx < mx + m.size().width as f64 && cy >= my && cy < my + m.size().height as f64
    };
    let monitor = popover.available_monitors().ok().and_then(|all| all.into_iter().find(|m| contains(m)))
        .or_else(|| popover.primary_monitor().ok().flatten());
    let scale = monitor.as_ref().map_or(fallback, |m| m.scale_factor());
    let position = rect.position.to_physical::<f64>(scale);
    let size = rect.size.to_physical::<f64>(scale);
    let (left, top, right, bottom) = monitor.as_ref().map_or((f64::MIN, f64::MIN, f64::MAX, f64::MAX), |m| {
        let area = m.work_area();
        (area.position.x as f64, area.position.y as f64, area.position.x as f64 + area.size.width as f64, area.position.y as f64 + area.size.height as f64)
    });
    // 창 크기는 대상 모니터 배율로 다시 잡는다(이전 모니터 배율의 물리 크기를 쓰면 높이가 틀어진다).
    let logical_height = popover.outer_size().ok().map_or(460.0, |s| s.height as f64 / fallback);
    let width = POPOVER_WIDTH * scale;
    let height = logical_height * scale;
    let margin = 8.0 * scale;
    // macOS 메뉴바 메뉴는 아이콘의 왼쪽 끝에 맞춰 내려온다. Windows 트레이 플라이아웃은 아이콘 가운데 위에 연다.
    let anchor = if cfg!(target_os = "macos") { position.x } else { position.x + size.width / 2.0 - width / 2.0 };
    let middle = position.y + size.height / 2.0;
    // macOS 메뉴바는 작업 영역 위에 있으므로 옆 열기는 Windows 세로 작업 표시줄에만 쓴다.
    let side = cfg!(windows) && (position.x >= right - 1.0 || position.x + size.width <= left + 1.0);
    let (x, y) = if side {
        // 작업 표시줄이 왼쪽·오른쪽에 있으면 아이콘 옆으로 연다.
        let x = if position.x >= right - 1.0 { position.x - width - 4.0 * scale } else { position.x + size.width + 4.0 * scale };
        (x, middle - height / 2.0)
    } else if middle > (top + bottom) / 2.0 {
        (anchor, position.y - height - 4.0 * scale)
    } else {
        (anchor, position.y + size.height + 4.0 * scale)
    };
    let x = x.min(right - width - margin).max(left + margin);
    let y = y.min(bottom - height - margin).max(top + margin);
    let _ = popover.set_position(tauri::PhysicalPosition::new(x, y));
    let _ = popover.show();
    let _ = popover.set_focus();
    let _ = popover.emit("ojak://popover-shown", ());
}

#[tauri::command]
fn open_dashboard(app: tauri::AppHandle) {
    show_main(&app);
}

#[tauri::command]
fn hide_popover(app: tauri::AppHandle) {
    if let Some(popover) = app.get_webview_window(POPOVER) {
        let _ = popover.hide();
    }
}

/// 브릿지 공급자 ID를 계정 저장소의 provider 값으로 맞춘다(`apps/desktop/src/state.ts` providerAliases와 같은 표).
fn account_provider(bridge: &str) -> &str {
    match bridge {
        "openai-codex" => "openai",
        "google-antigravity" => "google",
        "xai-oauth" => "xai",
        "zai" => "other",
        other => other,
    }
}

/// 계정의 남은 한도(%). 모든 모델에 걸리는 한도와, 지금 쓰는 모델에만 걸리는 한도(Fable 주간 등) 중 가장 적게 남은 값.
/// 신선한 관측이 하나도 없으면 `None`.
fn remaining_percent(account: &aam_protocol::Account, model: &str, stale_ms: i64, now: i64) -> Option<f64> {
    account
        .buckets
        .iter()
        .filter(|bucket| bucket.model.as_deref().is_none_or(|limited| model.to_ascii_lowercase().contains(&limited.to_ascii_lowercase())))
        .filter(|bucket| {
            matches!(bucket.status.as_str(), "known" | "exhausted")
                && now.saturating_sub(bucket.observed_at) <= stale_ms
                && bucket.resets_at.is_none_or(|reset| reset > now)
        })
        .filter_map(|bucket| if bucket.status == "exhausted" { Some(100.0) } else { bucket.used_percent.filter(|used| used.is_finite()) })
        .map(|used| (100.0 - used).clamp(0.0, 100.0))
        .reduce(f64::min)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AppInfo {
    version: String,
    protocol_version: u32,
    integration_version: u32,
    identifier: String,
    platform: String,
    home: String,
    contact: bool,
}

#[tauri::command]
fn app_info(app: tauri::AppHandle) -> Result<AppInfo, ApiError> {
    Ok(AppInfo {
        version: app.package_info().version.to_string(),
        protocol_version: aam_protocol::PROTOCOL_VERSION,
        integration_version: aam_protocol::INTEGRATION_VERSION,
        identifier: app.config().identifier.clone(),
        platform: format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
        home: paths()?.home.to_string_lossy().into_owned(),
        contact: CONTACT_URL.is_some(),
    })
}

/// `tauri.conf.json`의 `plugins.updater.pubkey`와 같은 자리표시자.
/// 이 값이면 업데이트 확인을 하지 않고 화면에서도 업데이트 UI를 숨긴다.
const UPDATER_PUBKEY_PLACEHOLDER: &str = "REPLACE_WITH_TAURI_UPDATER_PUBKEY";
const UPDATE_INTERVAL_MS: i64 = 24 * 60 * 60 * 1000;
#[cfg(unix)]
const LAUNCHCTL: &str = "/bin/launchctl";
#[cfg(unix)]
const SERVICE_LABEL: &str = "ai.aam.service";

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateOffer {
    version: String,
    notes: String,
}

#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateStatus {
    enabled: bool,
    available: Option<UpdateOffer>,
    error: Option<String>,
}

struct UpdateCache {
    at: i64,
    available: Option<UpdateOffer>,
}

static UPDATE_CACHE: std::sync::Mutex<Option<UpdateCache>> = std::sync::Mutex::new(None);

fn updater_pubkey(app: &tauri::AppHandle) -> String {
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|value| value.get("pubkey"))
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn updater_configured(pubkey: &str) -> bool {
    let pubkey = pubkey.trim();
    !pubkey.is_empty() && pubkey != UPDATER_PUBKEY_PLACEHOLDER
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn clean_notes(notes: &str) -> String {
    let mut out = String::new();
    for ch in notes.chars() {
        if ch == '\n' || ch == '\t' || !ch.is_control() {
            out.push(ch);
        }
        if out.len() >= 8_000 {
            break;
        }
    }
    out
}

fn update_failed(error: impl std::fmt::Display) -> ApiError {
    let mut message = error.to_string().replace(['\n', '\r'], " ");
    // 업데이트 서버에 받을 수 있는 릴리스 정보(latest.json)가 없을 때 나오는 updater 문구다. 원문만으로는 다음 행동을 알 수 없다.
    if message == "Could not fetch a valid release JSON from the remote" {
        message = "업데이트 서버에서 공개된 릴리스 정보를 찾지 못했습니다. 아직 공개 배포된 버전이 없거나 다운로드 저장소에 접근할 수 없습니다. 새 설치 파일을 받아 직접 설치해 주세요.".into();
    }
    if message.len() > 400 {
        message.truncate(400);
    }
    ApiError::new("UPDATE_FAILED", message)
}

fn cached_update(now: i64) -> Option<UpdateStatus> {
    let guard = UPDATE_CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let cached = guard.as_ref()?;
    if now.saturating_sub(cached.at) >= UPDATE_INTERVAL_MS {
        return None;
    }
    Some(UpdateStatus {
        enabled: true,
        available: cached.available.clone(),
        error: None,
    })
}

fn store_update(available: Option<UpdateOffer>) {
    let mut guard = UPDATE_CACHE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(UpdateCache { at: unix_ms(), available });
}

#[cfg(unix)]
fn service_kickstart_target(uid: u32) -> String {
    format!("gui/{uid}/{SERVICE_LABEL}")
}

#[cfg(unix)]
fn launch_agent_plist() -> Result<PathBuf, ApiError> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| ApiError::new("HOME_MISSING", "사용자 HOME 절대 경로가 필요합니다."))?;
    Ok(home.join("Library/LaunchAgents").join(format!("{SERVICE_LABEL}.plist")))
}

#[cfg(unix)]
/// 절대 경로의 launchctl만 호출한다. 셸을 거치지 않는다.
fn run_launchctl(args: &[&str]) -> Result<(), ApiError> {
    let mut child = Command::new(LAUNCHCTL)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ApiError::new("LAUNCHCTL_FAILED", "launchctl을 시작하지 못했습니다."))?;
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        match child.try_wait().map_err(|_| ApiError::new("LAUNCHCTL_FAILED", "launchctl 상태를 확인하지 못했습니다."))? {
            Some(status) if status.success() => return Ok(()),
            Some(_) => {
                return Err(ApiError::new(
                    "SERVICE_RESTART_FAILED",
                    "관리 서비스를 다시 시작하지 못했습니다. 앱 파일은 이미 바뀌었을 수 있습니다. 서비스 재시작을 다시 시도하세요.",
                ));
            }
            None if Instant::now() >= until => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ApiError::new("LAUNCHCTL_TIMEOUT", "launchctl 응답 시간이 초과되었습니다."));
            }
            None => thread::sleep(Duration::from_millis(40)),
        }
    }
}

#[cfg(windows)]
fn kickstart_managed_service_sync() -> Result<(), ApiError> {
    aam_launcher::install::service_restart(&paths()?)
}

#[cfg(unix)]
fn kickstart_managed_service_sync() -> Result<(), ApiError> {
    let plist = launch_agent_plist()?;
    if plist.symlink_metadata().is_err() {
        return Ok(());
    }
    let target = service_kickstart_target(unsafe { libc::geteuid() });
    run_launchctl(&["kickstart", "-k", &target])
}

async fn kickstart_managed_service() -> Result<(), ApiError> {
    tauri::async_runtime::spawn_blocking(kickstart_managed_service_sync)
        .await
        .map_err(|_| ApiError::new("INTERNAL_ERROR", "서비스 재시작 처리 중 오류가 발생했습니다"))?
}

fn relaunch(app: tauri::AppHandle) -> ! {
    QUIT_CONFIRMED.store(true, Ordering::SeqCst);
    app.restart();
}

async fn remote_update(app: &tauri::AppHandle) -> Result<Option<UpdateOffer>, ApiError> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(update_failed)?;
    let Some(update) = updater.check().await.map_err(update_failed)? else {
        return Ok(None);
    };
    Ok(Some(UpdateOffer {
        version: update.version,
        notes: clean_notes(update.body.as_deref().unwrap_or("")),
    }))
}

fn publish_update_status(app: &tauri::AppHandle, status: &UpdateStatus) {
    let _ = app.emit("ojak://update-status", status);
}

/// 서명 공개키가 자리표시자면 `enabled: false`만 돌려주고 네트워크 확인을 하지 않는다.
#[tauri::command]
async fn updates_status(app: tauri::AppHandle, force: bool) -> Result<UpdateStatus, ApiError> {
    if !updater_configured(&updater_pubkey(&app)) {
        let status = UpdateStatus { enabled: false, available: None, error: None };
        publish_update_status(&app, &status);
        return Ok(status);
    }
    if !force {
        if let Some(status) = cached_update(unix_ms()) {
            publish_update_status(&app, &status);
            return Ok(status);
        }
    }
    let status = match remote_update(&app).await {
        Ok(available) => {
            store_update(available.clone());
            UpdateStatus { enabled: true, available, error: None }
        }
        Err(error) => UpdateStatus { enabled: true, available: None, error: Some(error.message) },
    };
    publish_update_status(&app, &status);
    Ok(status)
}

/// 서명한 업데이트를 설치한 뒤 LaunchAgent를 다시 시작하고 앱을 재실행한다.
#[tauri::command]
async fn install_update(app: tauri::AppHandle) -> Result<(), ApiError> {
    if !updater_configured(&updater_pubkey(&app)) {
        return Err(ApiError::new("UPDATE_DISABLED", "업데이트 서명이 설정되지 않았습니다"));
    }
    use tauri_plugin_updater::UpdaterExt;
    let updater = app.updater().map_err(update_failed)?;
    let Some(update) = updater.check().await.map_err(update_failed)? else {
        return Err(ApiError::new("UPDATE_NONE", "설치할 업데이트가 없습니다"));
    };
    update.download_and_install(|_, _| {}, || {}).await.map_err(update_failed)?;
    kickstart_managed_service().await?;
    relaunch(app);
}

/// 설치는 끝났는데 서비스 재시작만 실패한 경우. launchctl 후 앱을 재실행한다.
#[tauri::command]
async fn restart_after_update(app: tauri::AppHandle) -> Result<(), ApiError> {
    kickstart_managed_service().await?;
    relaunch(app);
}


/// 종료 방식: `keep`은 앱만 닫고 관리 서비스를 유지, `deactivate`는 Ojak 연결을 모두 되돌린 뒤 닫는다.
#[tauri::command]
async fn quit_app(app: tauri::AppHandle, mode: String) -> Result<String, ApiError> {
    let report = match mode.as_str() {
        "keep" => String::new(),
        "deactivate" => {
            let report = run_management(vec!["deactivate"]).await?;
            // 연결을 모두 끄면 로그인 자동 실행도 끈다. 해제를 readback으로 확인한 뒤에만 종료한다.
            if get_autostart(app.clone())? {
                set_autostart(app.clone(), false)?;
                if get_autostart(app.clone())? {
                    return Err(ApiError::new("AUTOSTART_FAILED", "로그인 자동 실행을 끄지 못했습니다. 앱을 종료하지 않았습니다."));
                }
            }
            report
        }
        _ => return Err(ApiError::new("INVALID_ACTION", "지원하지 않는 종료 방식입니다.")),
    };
    QUIT_CONFIRMED.store(true, Ordering::SeqCst);
    app.exit(0);
    Ok(report)
}

#[tauri::command]
async fn deactivate_plan(app: tauri::AppHandle) -> Result<String, ApiError> {
    let mut plan = run_management(vec!["deactivate", "--dry-run"]).await?;
    if get_autostart(app)? {
        plan.push_str(&format!("\n- {}", tr("Turn off opening Ojak at login", "로그인할 때 Ojak 자동 실행 끄기", "Matikan pembukaan Ojak saat login")));
    }
    Ok(plan)
}

/// 로그인 자동 실행으로 켜졌을 때 넘기는 인수. 이때는 대시보드를 띄우지 않고 메뉴바에만 둔다.
const AUTOSTART_ARG: &str = "--autostart";

#[tauri::command]
fn get_autostart(app: tauri::AppHandle) -> Result<bool, ApiError> {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch()
        .is_enabled()
        .map_err(|_| ApiError::new("AUTOSTART_UNKNOWN", "로그인 자동 실행 상태를 확인하지 못했습니다."))
}

#[tauri::command]
fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<bool, ApiError> {
    use tauri_plugin_autostart::ManagerExt;
    let manager = app.autolaunch();
    let result = if enabled { manager.enable() } else { manager.disable() };
    result.map_err(|_| ApiError::new("AUTOSTART_FAILED", "로그인 자동 실행 설정을 바꾸지 못했습니다."))?;
    // 사용자가 한 번 고른 값은 다음 실행의 첫 실행 기본값보다 우선한다.
    write_ui_setting("autostartChosen", json!(true))?;
    get_autostart(app)
}

/// 설치본을 처음 실행할 때 한 번만 로그인 자동 실행을 켠다. 사용자가 끈 뒤에는 다시 켜지 않는다.
/// 개발 빌드는 target 경로를 등록하지 않도록 건너뛴다.
fn default_autostart(app: &tauri::AppHandle) {
    if cfg!(debug_assertions) || read_ui_settings().get("autostartChosen").is_some() {
        return;
    }
    use tauri_plugin_autostart::ManagerExt;
    if app.autolaunch().enable().is_ok() {
        let _ = write_ui_setting("autostartChosen", json!(true));
    }
}

fn paths() -> Result<Paths, ApiError> {
    Paths::discover().map_err(|e| ApiError::new("PATH_ERROR", e.to_string()))
}
/// GUI 앱이 콘솔 프로그램(aam)을 부를 때 Windows에서 콘솔 창이 깜박이지 않게 한다.
fn hidden(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}
fn launcher() -> Result<PathBuf, ApiError> {
    let executable =
        std::env::current_exe().map_err(|e| ApiError::new("INSTALLATION_ERROR", e.to_string()))?;
    // Windows 실행 파일은 확장자가 붙는다(is_file은 PATHEXT를 보완하지 않는다).
    let sibling = executable.parent().unwrap().join(format!("aam{}", std::env::consts::EXE_SUFFIX));
    if sibling.is_file() {
        return Ok(sibling);
    }
    Err(ApiError::new(
        "INSTALLATION_ERROR",
        "앱에 포함된 aam 실행 파일을 찾을 수 없습니다. 앱을 다시 설치하세요.",
    ))
}
#[tauri::command]
async fn rpc(method: String, params: Value) -> Result<Value, ApiError> {
    // WebView에는 임의 RPC나 프로세스 실행 권한을 제공하지 않습니다.
    if ![
        "status.read",
        "quota.refresh",
        "route.explain",
        "policy.update",
        "account.register",
        "account.update",
    ]
    .contains(&method.as_str())
    {
        return Err(ApiError::new(
            "METHOD_DENIED",
            "화면에서 사용할 수 없는 관리 명령입니다",
        ));
    }
    let p = paths()?;
    let result = tauri::async_runtime::spawn_blocking(move || aam_protocol::call(&p, &method.clone(), params).map(|value| (method, value)))
        .await
        .map_err(|_| ApiError::new("INTERNAL_ERROR", "관리 요청 처리 중 오류가 발생했습니다"))??;
    let (method, value) = result;
    // 연결을 이미 설치했다면 새로 실행 가능해진 도구의 `claude`·`codex` shim을 더한다. 실패해도 등록은 유지한다.
    if method == "account.register" && value.get("canLaunch").and_then(Value::as_bool) == Some(true) {
        let tool = match value.get("tool").and_then(Value::as_str) {
            Some("claude") => Some("claude"),
            Some("codex") => Some("codex"),
            _ => None,
        };
        if let Some(tool) = tool {
            let _ = run_management(vec!["integration", "extend", "--tool", tool]).await;
        }
    }
    Ok(value)
}
#[cfg(unix)]
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}
/// Windows: 새 콘솔 창에서 aam을 직접 실행한다. 인수는 argv로 넘기며 셸·스크립트 파일을 거치지 않는다.
/// 기본 터미널이 Windows Terminal이면 그 창으로 열린다.
#[cfg(windows)]
fn open_terminal(args: Vec<String>) -> Result<Value, ApiError> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
    let p = paths()?;
    p.prepare()
        .map_err(|e| ApiError::new("PATH_ERROR", e.to_string()))?;
    Command::new(launcher()?)
        .args(args)
        .env("AAM_HOME", &p.home)
        .env("AAM_HOLD_ON_ERROR", "1")
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map_err(|_| ApiError::new("LAUNCH_ERROR", "새 터미널 창을 열지 못했습니다. 연결 화면에서 aam 경로를 확인하세요."))?;
    // 창을 연 것과 계정 검증·native CLI 실행 성공은 다릅니다.
    Ok(json!({"opened":true}))
}

#[cfg(unix)]
fn open_terminal(args: Vec<String>) -> Result<Value, ApiError> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let p = paths()?;
    p.prepare()
        .map_err(|e| ApiError::new("PATH_ERROR", e.to_string()))?;
    let dir = p.home.join("launches");
    std::fs::create_dir_all(&dir).map_err(|e| ApiError::new("PATH_ERROR", e.to_string()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| ApiError::new("PATH_ERROR", e.to_string()))?;
    let script = dir.join(format!("{}.command", aam_protocol::new_id()));
    let exe = launcher()?;
    let command = std::iter::once(exe.to_string_lossy().to_string())
        .chain(args)
        .map(|s| shell_quote(&s))
        .collect::<Vec<_>>()
        .join(" ");
    let content = format!(
        "#!/bin/zsh\nexport AAM_HOME={}\n/bin/rm -- \"$0\"\nexec {}\n",
        shell_quote(&p.home.to_string_lossy()),
        command
    );
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(&script)
        .map_err(|e| ApiError::new("LAUNCH_ERROR", e.to_string()))?;
    file.write_all(content.as_bytes())
        .map_err(|e| ApiError::new("LAUNCH_ERROR", e.to_string()))?;
    let status = Command::new("/usr/bin/open")
        .args(["-a", "Terminal"])
        .arg(&script)
        .status()
        .map_err(|e| ApiError::new("LAUNCH_ERROR", e.to_string()))?;
    if !status.success() {
        let _ = std::fs::remove_file(script);
        return Err(ApiError::new(
            "LAUNCH_ERROR",
            "Terminal을 열지 못했습니다. 연결 화면에서 aam 경로를 확인하세요.",
        ));
    }
    // 터미널을 연 것과 계정 검증·native CLI 실행 성공은 다릅니다.
    Ok(json!({"opened":true}))
}
#[tauri::command]
async fn launch_session(intent: LaunchIntent) -> Result<Value, ApiError> {
    if !["claude", "codex"].contains(&intent.tool.as_str())
        || intent.model.trim().is_empty()
        || !std::path::Path::new(&intent.cwd).is_dir()
    {
        return Err(ApiError::new(
            "INVALID_INTENT",
            "도구·모델·작업 폴더를 확인하세요",
        ));
    }
    let p = paths()?;
    let copied = intent.clone();
    let decision = tauri::async_runtime::spawn_blocking(move || {
        aam_protocol::call(&p, "route.explain", json!({"intent":copied}))
    })
    .await
    .map_err(|_| ApiError::new("INTERNAL_ERROR", "배정 확인에 실패했습니다"))??;
    if decision
        .get("selectedAccountId")
        .and_then(Value::as_str)
        .is_none()
    {
        return Err(ApiError::new("NO_ELIGIBLE_ACCOUNT","이 작업을 실행할 수 있는 계정이 없습니다. 계정 인증·사용량·동시 실행 상태를 확인하세요."));
    }
    let mut args = vec![
        "run".into(),
        intent.tool,
        "--model".into(),
        intent.model,
        "--cwd".into(),
        intent.cwd,
    ];
    if let Some(id) = intent.account_id {
        args.extend(["--account".into(), id]);
    }
    if let Some(id) = intent.resume_session_id {
        args.extend(["--resume-session".into(), id]);
    }
    open_terminal(args)
}
#[tauri::command]
async fn login_account(
    tool: String,
    label: String,
    settings_digest: Option<String>,
    account_id: Option<String>,
) -> Result<Value, ApiError> {
    if !["claude", "codex"].contains(&tool.as_str()) {
        return Err(ApiError::new(
            "ADAPTER_UNVERIFIED",
            "지원되는 도구(Claude·Codex)와 계정 이름을 선택하세요.",
        ));
    }
    let mut args = vec![
        "account".into(),
        "login".into(),
        "--tool".into(),
        tool,
    ];
    if let Some(id) = account_id {
        if id.is_empty()
            || id.len() > 128
            || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(ApiError::new(
                "INVALID_PARAMS",
                "다시 로그인할 계정을 확인하지 못했습니다.",
            ));
        }
        args.extend(["--account".into(), id]);
    } else {
        if label.trim().is_empty() || label.len() > 120 {
            return Err(ApiError::new(
                "ADAPTER_UNVERIFIED",
                "지원되는 도구(Claude·Codex)와 계정 이름을 선택하세요.",
            ));
        }
        args.extend(["--label".into(), label]);
        if let Some(digest) = settings_digest {
            if digest.is_empty()
                || digest.len() > 256
                || !digest
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(ApiError::new(
                    "INVALID_SETTINGS_DIGEST",
                    "설정 미리보기를 다시 확인해 주세요.",
                ));
            }
            args.extend(["--settings-digest".into(), digest]);
        } else {
            args.push("--fresh-settings".into());
        }
    }
    open_terminal(args)
}
#[tauri::command]
async fn choose_directory() -> Option<String> {
    rfd::AsyncFileDialog::new()
        .set_title(tr("Choose AI working folder", "AI 작업 폴더 선택", "Pilih folder kerja AI"))
        .pick_folder()
        .await
        .map(|f| f.path().to_string_lossy().to_string())
}
async fn run_management(args: Vec<&'static str>) -> Result<String, ApiError> {
    let exe = launcher()?;
    let p = paths()?;
    tauri::async_runtime::spawn_blocking(move || {
        let result = hidden(&mut Command::new(exe))
            .args(args)
            .env("AAM_HOME", p.home)
            .output()
            .map_err(|e| ApiError::new("INSTALLATION_ERROR", e.to_string()))?;
        let text = String::from_utf8_lossy(if result.status.success() {
            &result.stdout
        } else {
            &result.stderr
        })
        .trim()
        .to_string();
        if result.status.success() {
            Ok(text)
        } else {
            Err(ApiError::new("INSTALLATION_ERROR", text))
        }
    })
    .await
    .map_err(|_| ApiError::new("INTERNAL_ERROR", "설치 작업 처리 중 오류가 발생했습니다"))?
}
#[tauri::command]
async fn settings_preview(tool: String) -> Result<Value, ApiError> {
    if !["claude", "codex"].contains(&tool.as_str()) {
        return Err(ApiError::new(
            "ADAPTER_UNVERIFIED",
            "설정 미리보기를 지원하는 도구를 선택해 주세요.",
        ));
    }
    let exe = launcher()?;
    let p = paths()?;
    tauri::async_runtime::spawn_blocking(move || {
        const LIMIT: u64 = 256 * 1024;
        let mut child = hidden(&mut Command::new(exe))
            .args(["account", "settings-preview", "--tool", &tool])
            .env("AAM_HOME", p.home)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| {
                ApiError::new(
                    "SETTINGS_PREVIEW_FAILED",
                    "설정 미리보기 명령을 시작하지 못했습니다.",
                )
            })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            ApiError::new(
                "SETTINGS_PREVIEW_FAILED",
                "설정 미리보기 출력에 연결하지 못했습니다.",
            )
        })?;
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout
                .take(LIMIT + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        });
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(40)),
                result => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(ApiError::new(
                        if result.is_err() {
                            "SETTINGS_PREVIEW_FAILED"
                        } else {
                            "SETTINGS_PREVIEW_TIMEOUT"
                        },
                        "설정 미리보기를 완료하지 못했습니다. 원본 설정은 변경하지 않았습니다.",
                    ));
                }
            }
        };
        let bytes = reader.join().ok().and_then(Result::ok).ok_or_else(|| {
            ApiError::new(
                "SETTINGS_PREVIEW_FAILED",
                "설정 미리보기 출력을 읽지 못했습니다.",
            )
        })?;
        if !status.success() || bytes.len() > LIMIT as usize {
            return Err(ApiError::new(
                "SETTINGS_PREVIEW_FAILED",
                "설정 미리보기 응답이 실패했거나 허용 크기를 초과했습니다.",
            ));
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
            ApiError::new(
                "INVALID_SETTINGS_PREVIEW",
                "설정 미리보기 응답 형식이 올바르지 않습니다.",
            )
        })?;
        if value.get("tool").and_then(Value::as_str) != Some(tool.as_str())
            || value.get("source").and_then(Value::as_str).is_none()
            || value.get("digest").and_then(Value::as_str).is_none()
            || value.get("canImport").and_then(Value::as_bool).is_none()
            || ["changes", "omitted", "warnings"]
                .iter()
                .any(|key| !value.get(key).is_some_and(Value::is_array))
        {
            return Err(ApiError::new(
                "INVALID_SETTINGS_PREVIEW",
                "설정 미리보기 응답을 확인하지 못했습니다.",
            ));
        }
        Ok(value)
    })
    .await
    .map_err(|_| {
        ApiError::new(
            "INTERNAL_ERROR",
            "설정 미리보기 처리 중 오류가 발생했습니다.",
        )
    })?
}

#[tauri::command]
async fn export_diagnostics(from: Option<i64>, to: Option<i64>) -> Result<Value, ApiError> {
    if from.is_some_and(|v| v < 0)
        || to.is_some_and(|v| v < 0)
        || from.zip(to).is_some_and(|(a, b)| a > b)
    {
        return Err(ApiError::new(
            "INVALID_RANGE",
            "진단 내보내기 기간을 확인해 주세요.",
        ));
    }
    let p = paths()?;
    let data = tauri::async_runtime::spawn_blocking(move || {
        let mut params = json!({"redact":true});
        if let Some(from) = from {
            params["from"] = json!(from);
        }
        if let Some(to) = to {
            params["to"] = json!(to);
        }
        let safe = aam_protocol::call(&p, "diagnostics.export", params)?;
        serde_json::to_vec_pretty(&safe)
            .map_err(|_| ApiError::new("EXPORT_FAILED", "진단 응답을 파일로 변환하지 못했습니다."))
    })
    .await
    .map_err(|_| {
        ApiError::new(
            "INTERNAL_ERROR",
            "진단 내보내기 처리 중 오류가 발생했습니다.",
        )
    })??;
    let Some(file) = rfd::AsyncFileDialog::new()
        .set_title(tr(
            "Save diagnostics file (private data excluded)",
            "개인정보를 제외한 진단 파일 저장", "Simpan file diagnostik (data pribadi dikecualikan)"))
        .set_file_name("aam-diagnostics.json")
        .add_filter("JSON", &["json"])
        .save_file()
        .await
    else {
        return Ok(json!({"saved":false,"cancelled":true}));
    };
    let destination = file.path().to_path_buf();
    tauri::async_runtime::spawn_blocking(move || {
        let fail = || {
            ApiError::new(
                "EXPORT_WRITE_FAILED",
                "진단 파일을 저장하지 못했습니다. 저장 위치와 권한을 확인해 주세요.",
            )
        };
        if std::fs::symlink_metadata(&destination)
            .is_ok_and(|m| !m.is_file() || m.file_type().is_symlink())
        {
            return Err(fail());
        }
        let parent = destination.parent().ok_or_else(fail)?;
        let temporary = parent.join(format!(".aam-export-{}.tmp", aam_protocol::new_id()));
        let mut output = aam_protocol::secure::private_options(std::fs::OpenOptions::new().write(true).create_new(true))
            .open(&temporary)
            .map_err(|_| fail())?;
        let result = output
            .write_all(&data)
            .and_then(|_| output.sync_all())
            .and_then(|_| std::fs::rename(&temporary, &destination));
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
            return Err(fail());
        }
        Ok(json!({"saved":true,"cancelled":false}))
    })
    .await
    .map_err(|_| ApiError::new("INTERNAL_ERROR", "진단 파일 저장 중 오류가 발생했습니다."))?
}

#[tauri::command]
async fn stop_service() -> Result<String, ApiError> {
    run_management(vec!["service", "stop"]).await
}
#[tauri::command]
async fn install_service() -> Result<String, ApiError> {
    run_management(vec!["service", "install"]).await
}
#[tauri::command]
async fn integration_action(action: String) -> Result<String, ApiError> {
    match action.as_str() {
        "install" => run_management(vec!["integration", "install"]).await,
        "uninstall" => run_management(vec!["integration", "uninstall"]).await,
        _ => Err(ApiError::new(
            "INVALID_ACTION",
            "지원하지 않는 연결 작업입니다",
        )),
    }
}

/// 첫 실행 준비 상태. 서비스가 꺼져 있어도 오류 없이 상태로 돌려준다.
#[tauri::command]
async fn setup_status() -> Result<Value, ApiError> {
    let output = run_management(vec!["setup", "--status"]).await?;
    serde_json::from_str(&output).map_err(|_| ApiError::new("PROTOCOL_MISMATCH", "준비 상태를 해석하지 못했습니다."))
}

/// 사용자가 준비 화면에서 [시작하기]를 눌렀을 때만 부른다. 빠진 단계만 설치한다.
#[tauri::command]
async fn setup_install(check: Option<bool>, with_omp: Option<bool>) -> Result<Value, ApiError> {
    // 연결 기록에는 실행한 프로그램이 launcher로 남으므로 앱이 아니라 번들의 aam으로 설치한다.
    let mut args = vec!["setup"];
    if check.unwrap_or(false) { args.push("--check"); }
    else if with_omp.unwrap_or(false) { args.push("--with-omp"); }
    let output = run_management(args).await?;
    serde_json::from_str(&output).map_err(|_| ApiError::new("INTERNAL_ERROR", "준비 결과를 해석하지 못했습니다."))
}

#[tauri::command]
async fn host_connections() -> Result<aam_launcher::hosts::HostConnections, ApiError> {
    let p = paths()?;
    tauri::async_runtime::spawn_blocking(move || aam_launcher::hosts::host_connections(&p))
        .await
        .map_err(|_| ApiError::new("INTERNAL_ERROR", "호스트 연결 설정을 확인하지 못했습니다."))?
}

#[tauri::command]
async fn omp_observer_action(action: String) -> Result<Value, ApiError> {
    let action = match action.as_str() {
        "status" => "status",
        "install" => "install",
        "uninstall" => "uninstall",
        _ => {
            return Err(ApiError::new(
                "INVALID_ACTION",
                "지원하지 않는 OMP 관측 연결 작업입니다.",
            ))
        }
    };
    let result = run_management(vec!["omp-observer", action]).await?;
    serde_json::from_str(&result).map_err(|_| {
        ApiError::new(
            "OMP_OBSERVER_RESPONSE",
            "OMP 관측 연결 상태를 확인하지 못했습니다.",
        )
    })
}
#[tauri::command]
async fn omp_broker_action(action: String) -> Result<Value, ApiError> {
    let args: Vec<&'static str> = match action.as_str() {
        "status" => vec!["omp-broker", "status", "--json"],
        "connect" => vec!["omp-broker", "connect"],
        "disconnect" => vec!["omp-broker", "disconnect"],
        _ => return Err(ApiError::new("INVALID_ACTION", "지원하지 않는 OMP broker 작업입니다.")),
    };
    let result = run_management(args).await?;
    serde_json::from_str(&result).map_err(|_| ApiError::new("OMP_BROKER_RESPONSE", "OMP broker 상태 응답을 해석하지 못했습니다."))
}
#[tauri::command]
async fn omp_bridge_action(action: String) -> Result<Value, ApiError> {
    let args: Vec<&'static str> = match action.as_str() {
        "status" => vec!["omp-bridge", "status", "--json"],
        "connect" => vec!["omp-bridge", "connect"],
        "disconnect" => vec!["omp-bridge", "disconnect"],
        _ => return Err(ApiError::new("INVALID_ACTION", "지원하지 않는 OMP 브릿지 작업입니다.")),
    };
    let result = run_management(args).await?;
    serde_json::from_str(&result).map_err(|_| ApiError::new("OMP_BRIDGE_RESPONSE", "OMP 브릿지 상태 응답을 해석하지 못했습니다."))
}

/// 사용 현황 시계열의 구간 폭. 화면은 5분 단위로 모아 보여 준다.
const USAGE_BIN_MS: i64 = 5 * 60_000;
/// bridge.log는 서비스가 4MB에서 순환시키므로 그보다 많이 읽을 일이 없다.
const USAGE_READ_LIMIT: u64 = 4 * 1024 * 1024;

/// bridge.log 한 줄(`<ms> <provider>/<model> session=<id> turn=<bool> tools=<n> -> <accountKey>`).
/// 계정 칸은 identity sha256의 앞 12 hex다. 예전에 남은 `provider|email:…|org:…` 줄도 읽는다.
/// (시각, 공급자, 모델, 계정 토큰, 세션, 대화 요청 여부). 형식이 다른 줄은 건너뛴다.
fn parse_usage_line(line: &str) -> Option<(i64, &str, &str, &str, &str, bool)> {
    let mut parts = line.split(' ').filter(|part| !part.is_empty());
    let at = parts.next()?.parse::<i64>().ok()?;
    let (provider, model) = parts.next()?.split_once('/')?;
    let mut session = "-";
    let mut turn = false;
    let mut key = None;
    let mut after_arrow = false;
    for part in parts {
        if after_arrow {
            key = Some(part);
            break;
        }
        if let Some(value) = part.strip_prefix("session=") {
            session = value;
        } else if let Some(value) = part.strip_prefix("turn=") {
            turn = value == "true";
        } else if part == "->" {
            after_arrow = true;
        }
    }
    let token = key?;
    // 이전 형식 `provider|email:<e>|org:<o>` 또는 `provider|account:<id>`는 화면이 맞추던 값으로 되돌린다.
    let account = token.split('|').nth(1).map(|part| part.strip_prefix("email:").unwrap_or(part)).unwrap_or(token);
    // 새 형식의 계정 칸은 정확히 12 hex다. 동시 쓰기로 두 줄이 붙은 예전 줄은 계정을 지어내지 않고 버린다.
    let legacy = token.contains('|');
    if provider.is_empty() || model.is_empty() || account.is_empty() || (!legacy && !(account.len() == 12 && account.bytes().all(|b| b.is_ascii_hexdigit()))) {
        return None;
    }
    Some((at, provider, model, account, session, turn))
}

/// (공급자, 모델, 계정)별 5분 구간 시계열. `series`·`total`은 대화 요청(turn=true)만 세고,
/// judge 같은 보조 요청(turn=false)은 `auxiliary`에 따로 센다. 세션 목록은 둘 다 포함한다. 구간은 `now`에서 끝나도록 정렬한다.
/// `aliases`는 `provider|accountKey` → 이메일/`account:<id>`. 로그의 별칭을 화면이 쓰던 계정 문자열로 바꾼다.
fn usage_from_text(text: &str, now: i64, window_minutes: u32, aliases: &std::collections::HashMap<String, String>) -> Value {
    use std::collections::BTreeMap;
    let bins = (i64::from(window_minutes) * 60_000 + USAGE_BIN_MS - 1) / USAGE_BIN_MS;
    let from = now - bins * USAGE_BIN_MS;
    let mut buckets: BTreeMap<(String, String, String), (Vec<u64>, u64, Vec<String>)> = BTreeMap::new();
    for line in text.lines() {
        let Some((at, provider, model, account, session, turn)) = parse_usage_line(line) else {
            continue;
        };
        if at < from || at > now {
            continue;
        }
        let account = aliases.get(&format!("{provider}|{account}")).map(String::as_str).unwrap_or(account);
        let entry = buckets
            .entry((provider.to_owned(), model.to_owned(), account.to_owned()))
            .or_insert_with(|| (vec![0; bins as usize], 0, Vec::new()));
        if turn {
            entry.0[((at - from) / USAGE_BIN_MS).clamp(0, bins - 1) as usize] += 1;
        } else {
            entry.1 += 1;
        }
        if session != "-" && !entry.2.iter().any(|known| known == session) {
            entry.2.push(session.to_owned());
        }
    }
    let buckets = buckets
        .into_iter()
        .map(|((provider, model, account), (series, auxiliary, sessions))| {
            json!({
                "provider": provider, "model": model, "account": account,
                "total": series.iter().sum::<u64>(), "auxiliary": auxiliary, "series": series, "sessions": sessions,
            })
        })
        .collect::<Vec<_>>();
    json!({ "binMinutes": USAGE_BIN_MS / 60_000, "from": from, "to": now, "buckets": buckets })
}

/// 제작자 문의 메일을 기본 메일 앱으로 연다. 주소는 고정이라 웹뷰가 임의 URL을 열 수 없다.
/// 빌드 시 지정한 문의 주소. 없으면 화면에서 문의 버튼을 숨긴다.
const CONTACT_URL: Option<&str> = option_env!("OJAK_CONTACT_URL");

/// 프로젝트 홈페이지(GitHub 저장소). 고정 주소라 웹뷰가 임의 URL을 열 수 없다.
const HOMEPAGE_URL: &str = "https://github.com/lapalai/ojak";

#[tauri::command]
fn open_homepage() -> Result<Value, ApiError> {
    open_url(HOMEPAGE_URL)
}

#[tauri::command]
fn contact_author() -> Result<Value, ApiError> {
    // 연락처는 빌드할 때 `OJAK_CONTACT_URL`(예: 저장소 Issues 주소)로 넣는다. 소스에 개인 연락처를 두지 않는다.
    let Some(url) = CONTACT_URL.filter(|url| url.starts_with("https://") || url.starts_with("mailto:")) else {
        return Err(ApiError::new("CONTACT_UNSET", "이 빌드에는 연락처가 설정되지 않았습니다."));
    };
    open_url(url)
}

fn open_url(url: &str) -> Result<Value, ApiError> {
    // 셸을 거치지 않고 OS 기본 처리기로 연다. Windows explorer는 성공해도 종료 코드 1을 돌려줄 수 있어 시작 여부만 본다.
    #[cfg(windows)]
    {
        Command::new("explorer.exe")
            .arg(url)
            .spawn()
            .map_err(|e| ApiError::new("OPEN_ERROR", e.to_string()))?;
    }
    #[cfg(unix)]
    {
        let status = Command::new("/usr/bin/open")
            .arg(url)
            .status()
            .map_err(|e| ApiError::new("OPEN_ERROR", e.to_string()))?;
        if !status.success() {
            return Err(ApiError::new("OPEN_ERROR", "링크를 열지 못했습니다."));
        }
    }
    Ok(json!({ "opened": true }))
}

/// bridge.status의 gateway 목록에서 로그 별칭을 화면용 계정 문자열로 바꾼다.
fn account_aliases(status: &Value) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for gateway in status.get("gateways").and_then(Value::as_array).into_iter().flatten() {
        let (Some(provider), Some(key), Some(email)) = (
            gateway.get("provider").and_then(Value::as_str),
            gateway.get("accountKey").and_then(Value::as_str),
            gateway.get("email").and_then(Value::as_str),
        ) else {
            continue;
        };
        if provider.is_empty() || key.is_empty() || email.is_empty() {
            continue;
        }
        map.insert(format!("{provider}|{key}"), email.to_owned());
    }
    map
}

/// 브릿지 요청 로그를 읽어 최근 `window_minutes` 동안의 계정·모델별 대화 요청 수를 5분 단위로 돌려준다.
/// 로그의 계정 칸은 별칭이고, 이메일은 bridge.status에서만 맞춘다. 프롬프트나 토큰은 없다.
#[tauri::command]
async fn bridge_usage(window_minutes: u32) -> Result<Value, ApiError> {
    if !(5..=2880).contains(&window_minutes) {
        return Err(ApiError::new("INVALID_RANGE", "사용 현황 기간은 5분에서 48시간 사이여야 합니다."));
    }
    let p = paths()?;
    tauri::async_runtime::spawn_blocking(move || {
        use std::io::Seek;
        let aliases = aam_protocol::call(&p, "bridge.status", json!({}))
            .ok()
            .map(|status| account_aliases(&status))
            .unwrap_or_default();
        let path = p.home.join("logs/bridge.log");
        let now = aam_protocol::now_ms();
        let mut file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(usage_from_text("", now, window_minutes, &aliases));
            }
            Err(error) => return Err(ApiError::new("USAGE_LOG_UNREADABLE", error.to_string())),
        };
        let length = file.metadata().map_err(|e| ApiError::new("USAGE_LOG_UNREADABLE", e.to_string()))?.len();
        let truncated = length > USAGE_READ_LIMIT;
        if truncated {
            file.seek(std::io::SeekFrom::Start(length - USAGE_READ_LIMIT))
                .map_err(|e| ApiError::new("USAGE_LOG_UNREADABLE", e.to_string()))?;
        }
        let mut bytes = Vec::with_capacity(length.min(USAGE_READ_LIMIT) as usize);
        file.take(USAGE_READ_LIMIT)
            .read_to_end(&mut bytes)
            .map_err(|e| ApiError::new("USAGE_LOG_UNREADABLE", e.to_string()))?;
        let text = String::from_utf8_lossy(&bytes);
        // 중간에서 읽기 시작했으면 첫 줄은 잘린 줄이므로 버린다.
        let text = if truncated { text.split_once('\n').map_or("", |(_, rest)| rest) } else { &text };
        Ok(usage_from_text(text, now, window_minutes, &aliases))
    })
    .await
    .map_err(|_| ApiError::new("INTERNAL_ERROR", "사용 현황을 읽는 중 오류가 발생했습니다."))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_counts_turns_per_bin_and_account() {
        let now = 1_000_000_000;
        let log = format!(
            "{} anthropic/claude-fable-5-1 session=aaaaaaaa turn=true tools=10 -> anthropic|email:a@x.io|org:o1\n\
             {} anthropic/claude-fable-5-1 session=aaaaaaaa turn=false tools=10 -> anthropic|email:a@x.io|org:o1\n\
             {} anthropic/claude-fable-5-1 session=bbbbbbbb turn=true tools=4 -> anthropic|email:a@x.io|org:o1 reserve-fallback\n\
             {} xai-oauth/grok-4.6 session=cccccccc turn=true tools=1 -> xai-oauth|account:d18b\n\
             {} anthropic/claude-fable-5-1 session=dddddddd turn=true tools=1 -> anthropic|email:a@x.io|org:o1\n\
             garbage line\n",
            now - 14 * 60_000, now - 13 * 60_000, now - 2 * 60_000, now - 1_000, now - 16 * 60_000,
        );
        let value = usage_from_text(&log, now, 15, &std::collections::HashMap::new());
        assert_eq!(value["binMinutes"], 5);
        assert_eq!(value["from"], now - 15 * 60_000);
        let buckets = value["buckets"].as_array().unwrap();
        assert_eq!(buckets.len(), 2);
        let claude = &buckets[0];
        assert_eq!(claude["account"], "a@x.io");
        assert_eq!(claude["series"], json!([1, 0, 1]));
        assert_eq!(claude["total"], 2);
        assert_eq!(claude["auxiliary"], 1);
        assert_eq!(claude["sessions"], json!(["aaaaaaaa", "bbbbbbbb"]));
        let grok = &buckets[1];
        assert_eq!(grok["account"], "account:d18b");
        assert_eq!(grok["series"], json!([0, 0, 1]));
    }

    #[test]
    fn usage_window_rounds_up_to_whole_bins() {
        let value = usage_from_text("", 0, 17, &std::collections::HashMap::new());
        assert_eq!(value["from"], -4 * USAGE_BIN_MS);
        assert!(value["buckets"].as_array().unwrap().is_empty());
    }

    #[test]
    fn usage_maps_account_key_without_email_in_the_log() {
        let now = 1_000_000_000;
        let key = "0123456789ab";
        let log = format!(
            "{now} anthropic/claude-opus-4.6 session=aaaaaaaa turn=true tools=2 -> {key} reserve-fallback\n\
             {now} xai-oauth/grok-4.6 session=bbbbbbbb turn=true tools=1 -> xai-oauth|account:d18b\n"
        );
        assert!(!log.contains('@'));
        assert!(!log.contains("email:"));
        let mut aliases = std::collections::HashMap::new();
        aliases.insert(format!("anthropic|{key}"), "person@example.com".into());
        let value = usage_from_text(&log, now, 15, &aliases);
        let buckets = value["buckets"].as_array().unwrap();
        assert_eq!(buckets[0]["account"], "person@example.com");
        assert_eq!(buckets[0]["provider"], "anthropic");
        assert_eq!(buckets[1]["account"], "account:d18b");
        let status = json!({ "gateways": [{ "provider": "anthropic", "accountKey": key, "email": "person@example.com", "port": 4101, "running": true }] });
        assert_eq!(account_aliases(&status).get(&format!("anthropic|{key}")).map(String::as_str), Some("person@example.com"));
    }
    #[test]
    fn glued_concurrent_lines_do_not_invent_an_account() {
        let now = 1_000_000_000;
        // 2026-10-05 실측: 동시 요청 두 줄이 줄바꿈 없이 붙어 계정 칸이 `<key><다음 줄 시각>`이 됐다.
        let log = format!("{now} openai-codex/gpt-6-astra session=a turn=true tools=1 -> 4e92316d5e86{now} openai-codex/gpt-6-astra session=b turn=true tools=1 -> 4e92316d5e86\n");
        let value = usage_from_text(&log, now, 15, &std::collections::HashMap::new());
        assert!(value["buckets"].as_array().unwrap().is_empty());
    }


    #[test]
    fn first_apple_language_reads_plist_array_output() {
        assert_eq!(
            first_apple_language("(\n    \"ko-KR\",\n    \"en-KR\"\n)\n").as_deref(),
            Some("ko-KR")
        );
        assert_eq!(first_apple_language("(\n)\n"), None);
        assert_eq!(first_apple_language(""), None);
    }

    #[test]
    fn language_tag_maps_indonesian_and_legacy_in() {
        assert_eq!(lang_from_tag("id-ID"), Lang::Id);
        assert_eq!(lang_from_tag("id"), Lang::Id);
        assert_eq!(lang_from_tag("in"), Lang::Id);
        assert_eq!(lang_from_tag("IN-ID"), Lang::Id);
        assert_eq!(lang_from_tag("ko-KR"), Lang::Ko);
        assert_eq!(lang_from_tag("en-US"), Lang::En);
        assert_eq!(lang_from_tag("fr-FR"), Lang::En);
        assert_eq!(language_name(3), "id");
        assert_eq!(language_name(0), "system");
    }

    #[test]
    fn updater_placeholder_disables_checks() {
        assert!(!updater_configured(UPDATER_PUBKEY_PLACEHOLDER));
        assert!(!updater_configured("  REPLACE_WITH_TAURI_UPDATER_PUBKEY  "));
        assert!(!updater_configured(""));
        assert!(!updater_configured("   "));
        assert!(updater_configured("RWTtExamplePublicKey"));
        assert_eq!(UPDATE_INTERVAL_MS, 24 * 60 * 60 * 1000);
    }

    #[cfg(unix)]
    #[test]
    fn service_restart_uses_absolute_launchctl_without_a_shell() {
        assert_eq!(LAUNCHCTL, "/bin/launchctl");
        assert!(!LAUNCHCTL.contains(' '));
        let target = service_kickstart_target(501);
        assert_eq!(target, "gui/501/ai.aam.service");
        let args = ["kickstart", "-k", target.as_str()];
        assert_eq!(args, ["kickstart", "-k", "gui/501/ai.aam.service"]);
        assert!(args.iter().all(|arg| !arg.contains([' ', ';', '|', '&', '$', '`'])));
    }
}
fn tray_image() -> tauri::image::Image<'static> {
    // 오작교 글리프(까치+아치) 44×44 템플릿 PNG. macOS가 알파만 사용해 메뉴바 색에 맞춘다.
    tauri::image::Image::from_bytes(include_bytes!("../icons/tray-icon.png"))
        .expect("icons/tray-icon.png는 빌드 시 포함되는 유효한 PNG여야 합니다")
}

fn tray_status() -> Result<Snapshot, ApiError> {
    let value = aam_protocol::call(&paths()?, "status.read", json!({}))?;
    let snapshot: Snapshot = serde_json::from_value(value)
        .map_err(|_| ApiError::new("INVALID_STATUS", "서비스 상태 응답을 확인하지 못했습니다."))?;
    if snapshot.version != aam_protocol::PROTOCOL_VERSION {
        return Err(ApiError::new(
            "PROTOCOL_MISMATCH",
            "앱과 서비스의 통신 버전이 다릅니다.",
        ));
    }
    Ok(snapshot)
}

fn tray_error_code(error: &ApiError) -> &str {
    if !error.code.is_empty()
        && error.code.len() <= 64
        && error
            .code
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
    {
        &error.code
    } else {
        "REQUEST_FAILED"
    }
}

/// 트레이·앱 메뉴 표시 언어. 설정에서 고른 언어(`AAM_HOME/ui-settings.json`)가 있으면 그 값을, 없으면
/// macOS 선호 언어(AppleLanguages) 첫 항목을, 그것도 없으면 LC_ALL/LC_MESSAGES/LANG을 쓴다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lang {
    En,
    Ko,
    Id,
}

/// `defaults read -g AppleLanguages` 출력(`(\n    "ko-KR",\n    "en-KR"\n)`)에서 첫 언어 태그를 꺼낸다.
fn first_apple_language(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| line.starts_with('"'))
        .map(|line| line.trim_matches(|c| c == '"' || c == ',').to_string())
}

/// Windows 사용자 로캘 이름(예: `ko-KR`, `id-ID`).
#[cfg(windows)]
fn windows_locale() -> Option<String> {
    use windows_sys::Win32::Globalization::GetUserDefaultLocaleName;
    let mut buffer = [0u16; 85];
    let len = unsafe { GetUserDefaultLocaleName(buffer.as_mut_ptr(), buffer.len() as i32) };
    (len > 1).then(|| String::from_utf16_lossy(&buffer[..len as usize - 1]))
}
#[cfg(not(windows))]
fn windows_locale() -> Option<String> {
    None
}

static SYSTEM_LANG: LazyLock<Lang> = LazyLock::new(|| {
    let preferred = windows_locale().or_else(|| Command::new("/usr/bin/defaults")
        .args(["read", "-g", "AppleLanguages"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .and_then(|text| first_apple_language(&text)))
        .or_else(|| {
            ["LC_ALL", "LC_MESSAGES", "LANG"]
                .iter()
                .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
        });
    preferred.as_deref().map(lang_from_tag).unwrap_or(Lang::En)
});

/// AppleLanguages·로케일 태그에서 표시 언어를 고른다. 인도네시아어는 `id`와 예전 ISO 코드 `in`을 받는다.
/// 알 수 없는 태그는 영어다.
fn lang_from_tag(value: &str) -> Lang {
    let value = value.to_ascii_lowercase();
    if value.starts_with("ko") {
        Lang::Ko
    } else if value.starts_with("id") || value.starts_with("in") {
        Lang::Id
    } else {
        Lang::En
    }
}

/// 설정 언어: 0 = 시스템, 1 = English, 2 = 한국어, 3 = Bahasa Indonesia.
static LANG_CHOICE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(u8::MAX);

fn ui_settings_path() -> Option<PathBuf> {
    paths().ok().map(|p| p.home.join("ui-settings.json"))
}

fn read_ui_settings() -> serde_json::Map<String, Value> {
    ui_settings_path()
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| serde_json::from_slice::<serde_json::Map<String, Value>>(&bytes).ok())
        .unwrap_or_default()
}

/// 화면 설정 한 항목만 바꾸고 나머지 항목은 보존한다.
fn write_ui_setting(key: &str, value: Value) -> Result<(), ApiError> {
    let path = ui_settings_path().ok_or_else(|| ApiError::new("PATH_ERROR", "설정 폴더를 찾지 못했습니다."))?;
    let mut settings = read_ui_settings();
    settings.insert(key.to_owned(), value);
    std::fs::write(&path, Value::Object(settings).to_string())
        .map_err(|_| ApiError::new("SETTINGS_WRITE_FAILED", "설정을 저장하지 못했습니다."))
}

/// 메뉴바 숫자를 띄우는 기준(남은 %). 지금 쓰는 계정의 가장 적은 잔여가 이 값 이하일 때만 숫자를 보인다.
/// 0 = 숫자 끔, 100 = 항상 표시. 기본 30%.
const DEFAULT_TRAY_THRESHOLD: u8 = 30;
static TRAY_THRESHOLD: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(u8::MAX);

fn tray_threshold() -> u8 {
    let cached = TRAY_THRESHOLD.load(Ordering::Relaxed);
    if cached != u8::MAX {
        return cached;
    }
    let value = read_ui_settings()
        .get("trayThreshold")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_TRAY_THRESHOLD, |value| value.min(100) as u8);
    TRAY_THRESHOLD.store(value, Ordering::Relaxed);
    value
}

#[tauri::command]
fn get_tray_threshold() -> u8 {
    tray_threshold()
}

#[tauri::command]
fn set_tray_threshold(value: u8) -> Result<u8, ApiError> {
    let value = value.min(100);
    write_ui_setting("trayThreshold", json!(value))?;
    TRAY_THRESHOLD.store(value, Ordering::Relaxed);
    Ok(value)
}

/// 곧 리셋 알림을 보낼지. 기본은 켬이며, 실제 표시 여부는 OS 알림 허용을 따른다.
fn expiring_notify() -> bool {
    read_ui_settings().get("expiringNotify").and_then(Value::as_bool).unwrap_or(true)
}

#[tauri::command]
fn get_expiring_notify() -> bool {
    expiring_notify()
}

#[tauri::command]
fn set_expiring_notify(value: bool) -> Result<bool, ApiError> {
    write_ui_setting("expiringNotify", json!(value))?;
    Ok(value)
}

/// 터미널 안내가 앱과 같이 이메일을 가릴지. 없으면 가림(앱 기본값).
#[tauri::command]
fn set_privacy(masked: bool) -> Result<(), ApiError> {
    write_ui_setting("privacy", json!(if masked { "masked" } else { "visible" }))
}

/// 이미 알린 (계정 묶음, 리셋 시각). 리셋 주기마다 한 번만 알리고, 앱을 다시 켜도 반복하지 않는다.
/// 계정 ID와 시각만 저장하며 이메일 같은 개인정보는 넣지 않는다.
fn notified_expiring() -> Vec<String> {
    read_ui_settings()
        .get("expiringNotified")
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(|item| item.as_str().map(str::to_owned)).collect())
        .unwrap_or_default()
}

fn hours_text(ms: i64) -> String {
    let hours = (ms.max(0) as f64 / 3_600_000.0).round() as i64;
    let (days, rest) = (hours / 24, hours % 24);
    match (lang(), days) {
        (Lang::Ko, 0) => format!("{hours}시간 뒤"),
        (Lang::Ko, _) => format!("{days}일 {rest}시간 뒤"),
        (Lang::Id, 0) => format!("dalam {hours} jam"),
        (Lang::Id, _) => format!("dalam {days} hari {rest} jam"),
        (Lang::En, 0) => format!("in {hours}h"),
        (Lang::En, _) => format!("in {days}d {rest}h"),
    }
}

fn language_choice() -> u8 {
    let cached = LANG_CHOICE.load(Ordering::Relaxed);
    if cached != u8::MAX {
        return cached;
    }
    let stored = read_ui_settings().get("language").and_then(Value::as_str).map(str::to_owned);
    let choice = match stored.as_deref() {
        Some("en") => 1,
        Some("ko") => 2,
        Some("id") => 3,
        _ => 0,
    };
    LANG_CHOICE.store(choice, Ordering::Relaxed);
    choice
}

fn lang() -> Lang {
    match language_choice() {
        1 => Lang::En,
        2 => Lang::Ko,
        3 => Lang::Id,
        _ => *SYSTEM_LANG,
    }
}

fn language_name(choice: u8) -> &'static str {
    match choice {
        1 => "en",
        2 => "ko",
        3 => "id",
        _ => "system",
    }
}

/// 설정 화면과 트레이가 같은 언어를 쓰도록 네이티브가 기준을 가진다. `resolved`는 실제 표시 언어다.
#[tauri::command]
fn get_language() -> Value {
    json!({ "choice": language_name(language_choice()), "resolved": match lang() { Lang::Ko => "ko", Lang::Id => "id", Lang::En => "en" } })
}

#[tauri::command]
fn set_language(app: tauri::AppHandle, choice: String) -> Result<Value, ApiError> {
    let value = match choice.as_str() {
        "system" => 0,
        "en" => 1,
        "ko" => 2,
        "id" => 3,
        _ => return Err(ApiError::new("INVALID_ACTION", "지원하지 않는 언어입니다.")),
    };
    write_ui_setting("language", json!(choice))?;
    LANG_CHOICE.store(value, Ordering::Relaxed);
    if let Some(labels) = app.try_state::<MenuLabels>() {
        labels.apply();
    }
    Ok(get_language())
}

/// 언어를 바꿀 때 다시 써야 하는 고정 메뉴 항목과 (영어, 한국어, 인도네시아어) 문구.
struct MenuLabels(Vec<(MenuItem<tauri::Wry>, &'static str, &'static str, &'static str)>);
impl MenuLabels {
    fn apply(&self) {
        for (item, en, ko, id) in &self.0 {
            let _ = item.set_text(tr(*en, *ko, *id));
        }
    }
}

fn tr(en: &'static str, ko: &'static str, id: &'static str) -> &'static str {
    match lang() {
        Lang::Ko => ko,
        Lang::Id => id,
        Lang::En => en,
    }
}

struct TrayStatus {
    service: MenuItem<tauri::Wry>,
    urgent: MenuItem<tauri::Wry>,
    reset: MenuItem<tauri::Wry>,
    sessions: MenuItem<tauri::Wry>,
    assignment: MenuItem<tauri::Wry>,
    feedback: MenuItem<tauri::Wry>,
    now: [MenuItem<tauri::Wry>; 3],
    tray: tauri::tray::TrayIcon<tauri::Wry>,
    app: tauri::AppHandle,
}

impl TrayStatus {
    fn update(&self, snapshot: &Snapshot) {
        let now = aam_protocol::now_ms();
        let stale_ms = snapshot
            .policy
            .stale_after_seconds
            .saturating_mul(1000)
            .min(i64::MAX as u64) as i64;
        let mut auth = 0;
        let mut low = 0;
        let mut unknown = 0;
        let mut elapsed = 0;
        let mut next_reset: Option<i64> = None;
        for account in &snapshot.accounts {
            if matches!(account.auth_status.as_str(), "auth-required" | "error") {
                auth += 1;
            }
            if account.buckets.is_empty() {
                unknown += 1;
            }
            for bucket in &account.buckets {
                let fresh = bucket.observed_at > 0
                    && now.saturating_sub(bucket.observed_at) <= stale_ms
                    && bucket.resets_at.is_none_or(|reset| reset > now);
                if bucket.resets_at.is_some_and(|reset| reset <= now) {
                    elapsed += 1;
                }
                if bucket.status == "exhausted"
                    || (fresh
                        && bucket.status == "known"
                        && bucket.used_percent.is_some_and(|used| {
                            used.is_finite()
                                && used >= 100.0 - snapshot.policy.safety_reserve_percent
                        }))
                {
                    low += 1;
                }
                if !fresh
                    || !matches!(bucket.status.as_str(), "known" | "exhausted")
                    || bucket.used_percent.is_none()
                {
                    unknown += 1;
                }
                if fresh && matches!(bucket.status.as_str(), "known" | "exhausted") {
                    if let Some(reset) = bucket.resets_at {
                        next_reset = Some(next_reset.map_or(reset, |current| current.min(reset)));
                    }
                }
            }
        }
        let mut active = 0;
        let mut preparing = 0;
        let mut uncertain = 0;
        let mut account_context = Vec::new();
        for session in &snapshot.sessions {
            match session.state.as_str() {
                "ACTIVE" => active += 1,
                "PREPARED" | "STARTING" => preparing += 1,
                "SUSPECT" | "ORPHANED" => uncertain += 1,
                _ => continue,
            }
            if let Some(index) = snapshot
                .accounts
                .iter()
                .position(|account| account.id == session.account_id)
            {
                if !account_context.contains(&(index + 1)) {
                    account_context.push(index + 1);
                }
            }
        }
        account_context.sort_unstable();
        let context = account_context
            .iter()
            .take(4)
            .map(|index| format!("{} {index}", tr("Account", "계정", "Akun")))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = self.service.set_text(tr(
            "Management service connected · checked every 5 s",
            "관리 서비스 연결됨 · 5초마다 확인합니다", "Layanan pengelolaan terhubung · dicek setiap 5 dtk"));
        let _ = self.urgent.set_text(match lang() {
            Lang::Ko => format!("인증 확인 {auth}개 · 사용량 주의 {low}개 · 미확인 {unknown}개"),
            Lang::Id => format!("Perlu autentikasi {auth} · Peringatan pemakaian {low} · Tidak diketahui {unknown}"),
            Lang::En => format!("Auth needed {auth} · Usage warnings {low} · Unknown {unknown}"),
        });
        let reset = next_reset.map_or_else(
            || tr("No known upcoming reset", "확인된 다음 리셋 없음", "Tidak ada reset berikutnya yang diketahui").to_string(),
            |time| {
                let minutes = (time.saturating_sub(now) + 59_999) / 60_000;
                match lang() {
                    Lang::Ko => format!("다음 관측 리셋 약 {minutes}분 후"),
                    Lang::Id => format!("Reset teramati berikutnya sekitar {minutes} mnt lagi"),
                    Lang::En => format!("Next observed reset in about {minutes} min"),
                }
            },
        );
        let _ = self.reset.set_text(match lang() {
            Lang::Ko => format!("{reset} · 리셋 경과 {elapsed}개는 재조회가 필요합니다"),
            Lang::Id => format!("{reset} · {elapsed} yang lewat reset perlu dicek ulang"),
            Lang::En => format!("{reset} · {elapsed} past reset need a re-check"),
        });
        let more = account_context.len().saturating_sub(4);
        let _ = self.sessions.set_text(format!(
            "{}{}{}",
            match lang() {
                Lang::Ko => format!("실행 {active} · 준비 {preparing} · 불확실 {uncertain}"),
                Lang::Id => format!("Berjalan {active} · Menyiapkan {preparing} · Tidak pasti {uncertain}"),
                Lang::En => format!("Running {active} · Preparing {preparing} · Uncertain {uncertain}"),
            },
            if context.is_empty() {
                String::new()
            } else {
                format!(" · {context}")
            },
            if more > 0 {
                match lang() {
                    Lang::Ko => format!(" 외 {more}개"),
                    Lang::Id => format!(" +{more} lagi"),
                    Lang::En => format!(" +{more} more"),
                }
            } else {
                String::new()
            },
        ));
        let _ = self.assignment.set_text(if snapshot.policy.automatic {
            tr(
                "Pause automatic allocation for new sessions",
                "새 세션 자동 배정 중지", "Jeda alokasi otomatis untuk sesi baru")
        } else {
            tr(
                "Resume automatic allocation for new sessions",
                "새 세션 자동 배정 재개", "Lanjutkan alokasi otomatis untuk sesi baru")
        });
        let _ = self.assignment.set_enabled(true);
    }

    /// 지금 쓰는 계정(최근 15분 omp 브릿지 요청 또는 실행 중인 관리 세션)의 남은 한도를 가장 적은 순으로 3줄 보여 주고,
    /// 메뉴바 아이콘 옆에는 그중 가장 먼저 바닥날 값 하나만 띄운다. 트레이는 누구나 보는 곳이라 이메일은 쓰지 않는다.
    fn update_now(&self, snapshot: &Snapshot) {
        let now = aam_protocol::now_ms();
        let stale_ms = snapshot.policy.stale_after_seconds.saturating_mul(1000).min(i64::MAX as u64) as i64;
        let bridge_sessions = paths()
            .and_then(|p| aam_protocol::call(&p, "bridge.status", json!({})))
            .ok()
            .and_then(|status| status.get("sessions").and_then(Value::as_array).cloned())
            .unwrap_or_default();
        // (provider, email 소문자, model)
        let mut used: Vec<(String, String, String)> = Vec::new();
        for session in &bridge_sessions {
            if now.saturating_sub(session.get("lastUsedAt").and_then(Value::as_i64).unwrap_or(0)) > 15 * 60_000 {
                continue;
            }
            let text = |key: &str| session.get(key).and_then(Value::as_str).unwrap_or_default().to_owned();
            used.push((account_provider(&text("provider")).to_owned(), text("email").to_lowercase(), text("model")));
        }
        for session in snapshot.sessions.iter().filter(|session| session.state == "ACTIVE") {
            if let Some(account) = snapshot.accounts.iter().find(|account| account.id == session.account_id) {
                let email = account.email.clone().unwrap_or_else(|| account.id.clone()).to_lowercase();
                used.push((account.provider.clone(), email, session.model.clone()));
            }
        }
        // 같은 계정·모델은 한 줄로. 같은 계정을 여러 도구로 관측했으면 가장 적게 남은 값을 쓴다.
        let mut lines: Vec<(String, String, Option<f64>)> = Vec::new();
        for (provider, email, model) in used {
            let remaining = snapshot
                .accounts
                .iter()
                .filter(|account| account.provider == provider && account.email.as_deref().unwrap_or(&account.id).eq_ignore_ascii_case(&email))
                .filter_map(|account| remaining_percent(account, &model, stale_ms, now))
                .reduce(f64::min);
            let key = format!("{provider}\u{0}{email}\u{0}{model}");
            if !lines.iter().any(|(existing, _, _)| *existing == key) {
                lines.push((key, format!("{provider}\u{0}{model}"), remaining));
            }
        }
        lines.sort_by(|a, b| a.2.unwrap_or(f64::MAX).total_cmp(&b.2.unwrap_or(f64::MAX)));
        let provider_name = |id: &str| match id {
            "anthropic" => "Anthropic",
            "openai" => "Codex",
            "google" => "Gemini",
            "xai" => "xAI",
            "other" => "Z.AI",
            other => other,
        }
        .to_owned();
        let reserve = snapshot.policy.safety_reserve_percent;
        for (index, item) in self.now.iter().enumerate() {
            let text = match lines.get(index) {
                Some((_, label, remaining)) => {
                    let (provider, model) = label.split_once('\u{0}').unwrap_or((label, ""));
                    let value = remaining.map_or_else(
                        || tr("unknown", "확인 필요", "perlu dicek").to_owned(),
                        |value| match lang() {
                            Lang::Ko => format!("{value:.0}% 남음"),
                            Lang::Id => format!("{value:.0}% tersisa"),
                            Lang::En => format!("{value:.0}% left"),
                        },
                    );
                    let mark = if remaining.is_some_and(|value| value <= reserve) { "▲" } else { "●" };
                    format!("{mark} {} · {model} · {value}", provider_name(provider))
                }
                None if index == 0 => tr("Nothing in use in the last 15 min", "최근 15분 동안 쓰는 계정 없음", "Tidak ada akun yang dipakai dalam 15 menit terakhir").to_owned(),
                None => String::new(),
            };
            let _ = item.set_text(text);
        }
        // macOS 메뉴바만 아이콘 옆에 숫자를 띄운다. Windows 트레이 아이콘에는 제목이 없어 set_title이 아무 효과가 없다.
        let tightest = lines
            .iter()
            .filter_map(|(_, label, remaining)| remaining.map(|value| (label, value)))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        #[cfg(target_os = "macos")]
        {
            let threshold = f64::from(tray_threshold());
            let title = tightest.filter(|(_, value)| *value <= threshold).map(|(_, value)| {
                if value <= reserve { format!("⚠︎{value:.0}%") } else { format!("{value:.0}%") }
            });
            // macOS tray-icon은 `None`을 무시하므로 숨길 때는 빈 문자열로 지운다.
            let _ = self.tray.set_title(Some(title.unwrap_or_default()));
        }
        let tooltip = tightest.map_or_else(
            || "Ojak".to_owned(),
            |(label, value)| {
                let (provider, model) = label.split_once('\u{0}').unwrap_or((label, ""));
                match lang() {
                    Lang::Ko => format!("Ojak · 가장 적게 남은 한도: {} {model} {value:.0}%", provider_name(provider)),
                    Lang::Id => format!("Ojak · Sisa terendah: {} {model} {value:.0}%", provider_name(provider)),
                    Lang::En => format!("Ojak · Lowest remaining: {} {model} {value:.0}%", provider_name(provider)),
                }
            },
        );
        let _ = self.tray.set_tooltip(Some(tooltip));
    }

    /// 곧 리셋되는데 많이 남은 계정을 리셋 주기마다 한 번 시스템 알림으로 알린다.
    /// 서비스가 계산한 `expiring`만 쓰며 화면과 같은 기준이다. OS가 알림을 막았으면 조용히 표시되지 않는다.
    fn notify_expiring(&self, snapshot: &Snapshot) {
        use tauri_plugin_notification::NotificationExt;
        if !expiring_notify() {
            return;
        }
        let now = aam_protocol::now_ms();
        let sent = notified_expiring();
        let mut kept: Vec<String> = sent
            .iter()
            .filter(|key| key.rsplit_once('@').and_then(|(_, at)| at.parse::<i64>().ok()).is_some_and(|at| at > now))
            .cloned()
            .collect();
        let mut changed = kept.len() != sent.len();
        for summary in &snapshot.quota_summaries {
            let Some(expiring) = &summary.expiring else { continue };
            let Some(id) = summary.account_ids.iter().min() else { continue };
            let key = format!("{id}@{}", expiring.resets_at);
            if kept.contains(&key) {
                continue;
            }
            // 계정은 공급자 이름으로만 밝힌다. 알림은 잠금 화면·화면 공유에도 보일 수 있다.
            let provider = snapshot
                .accounts
                .iter()
                .find(|account| summary.account_ids.contains(&account.id))
                .map(|account| aam_protocol::pin_provider(&account.provider))
                .unwrap_or("");
            let name = match provider {
                "anthropic" => "Claude",
                "openai" => "Codex",
                "google" => "Gemini",
                "xai" => "Grok",
                _ => "Ojak",
            };
            let label = expiring.label.rsplit(" · ").next().unwrap_or(&expiring.label);
            let when = hours_text(expiring.resets_at - now);
            let percent = expiring.usable_percent.round();
            let body = match lang() {
                Lang::Ko => format!("{name} 계정의 {label} 한도 {percent:.0}%가 {when} 리셋됩니다. 리셋 전에 쓰면 한도를 아낄 수 있어요."),
                Lang::Id => format!("{percent:.0}% batas {label} akun {name} direset {when}. Pakai sebelum direset agar tidak terbuang."),
                Lang::En => format!("{percent:.0}% of a {name} account's {label} limit resets {when}. Use it before then so it isn't lost."),
            };
            let shown = self
                .app
                .notification()
                .builder()
                .title(tr("Use it before it resets", "리셋 전에 쓰는 게 좋아요", "Pakai sebelum direset"))
                .body(body)
                .show();
            // 보내지 못했으면 기록하지 않아 다음 확인 때 다시 시도한다.
            if shown.is_ok() {
                kept.push(key);
                changed = true;
            }
        }
        if changed {
            let _ = write_ui_setting("expiringNotified", json!(kept));
        }
    }

    fn poll(self, receiver: mpsc::Receiver<()>) {
        loop {
            let _ = self.service.set_text(tr(
                "Checking management service",
                "관리 서비스 상태 확인 중", "Memeriksa layanan pengelolaan"));
            let _ = self.assignment.set_enabled(false);
            let displayed_policy = match tray_status() {
                Ok(snapshot) => {
                    self.update(&snapshot);
                    self.update_now(&snapshot);
                    self.notify_expiring(&snapshot);
                    Some(snapshot.policy)
                }
                Err(error) => {
                    let _ = self.service.set_text(format!(
                        "{} · {}",
                        tr(
                            "Management service connection needs attention",
                            "관리 서비스 연결 확인 필요"
                        , "Koneksi layanan pengelolaan perlu dicek"),
                        tray_error_code(&error)
                    ));
                    let _ = self.urgent.set_text(tr(
                        "Auth and usage status unavailable",
                        "인증·사용량 상태를 확인할 수 없습니다", "Status autentikasi dan pemakaian tidak tersedia"));
                    let _ = self
                        .reset
                        .set_text(tr("Reset times unavailable", "리셋 시각을 확인할 수 없습니다", "Waktu reset tidak tersedia"));
                    let _ = self.sessions.set_text(tr(
                        "Active sessions and accounts unavailable",
                        "활성 세션과 계정을 확인할 수 없습니다", "Sesi aktif dan akun tidak tersedia"));
                    let _ = self.assignment.set_text(tr(
                        "Automatic allocation status unknown",
                        "자동 배정 상태 미확인", "Status alokasi otomatis belum diketahui"));
                    None
                }
            };
            match receiver.recv_timeout(Duration::from_secs(5)) {
                Ok(()) => {
                    let _ = self.assignment.set_enabled(false);
                    let _ = self.feedback.set_text(tr(
                        "Confirming automatic allocation change",
                        "자동 배정 변경을 확인하고 있습니다", "Mengonfirmasi perubahan alokasi otomatis"));
                    let result = (|| {
                        let desired = !displayed_policy
                            .as_ref()
                            .ok_or_else(|| {
                                ApiError::new("DAEMON_UNAVAILABLE", "서비스 연결을 확인해 주세요.")
                            })?
                            .automatic;
                        let current = tray_status()?;
                        let value = aam_protocol::call(
                            &paths()?,
                            "policy.update",
                            json!({
                                "expectedRevision":current.policy.revision, "automatic":desired,
                            }),
                        )?;
                        let policy: aam_protocol::Policy =
                            serde_json::from_value(value).map_err(|_| {
                                ApiError::new(
                                    "INVALID_POLICY",
                                    "변경된 정책 응답을 확인하지 못했습니다.",
                                )
                            })?;
                        if policy.automatic != desired {
                            return Err(ApiError::new(
                                "INVALID_POLICY",
                                "자동 배정 변경을 확인하지 못했습니다.",
                            ));
                        }
                        Ok(policy.automatic)
                    })();
                    match result {
                        Ok(automatic) => {
                            let _ = self.feedback.set_text(if automatic {
                                tr(
                                    "Automatic allocation resumed · existing sessions are kept",
                                    "자동 배정을 재개했습니다 · 기존 세션은 유지됩니다", "Alokasi otomatis dilanjutkan · sesi yang sedang berjalan tetap")
                            } else {
                                tr(
                                    "Automatic allocation paused · existing sessions are kept",
                                    "자동 배정을 중지했습니다 · 기존 세션은 유지됩니다", "Alokasi otomatis dijeda · sesi yang sedang berjalan tetap")
                            });
                        }
                        Err(error) => {
                            let _ = self.feedback.set_text(match lang() {
                                Lang::Ko => format!(
                                    "정책 변경 실패 · {} · 대시보드에서 확인해 주세요",
                                    tray_error_code(&error)
                                ),
                                Lang::Id => format!(
                                    "Gagal mengubah kebijakan · {} · periksa dasbor",
                                    tray_error_code(&error)
                                ),
                                Lang::En => format!(
                                    "Policy change failed · {} · check the dashboard",
                                    tray_error_code(&error)
                                ),
                            });
                        }
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    }
}
fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            Some(vec![AUTOSTART_ARG]),
        ))
        .invoke_handler(tauri::generate_handler![
            rpc,
            launch_session,
            login_account,
            choose_directory,
            settings_preview,
            export_diagnostics,
            stop_service,
            install_service,
            integration_action,
            host_connections,
            omp_observer_action,
            omp_broker_action,
            omp_bridge_action,
            bridge_usage,
            contact_author,
            open_homepage,
            setup_status,
            setup_install,
            get_language,
            set_language,
            app_info,
            quit_app,
            deactivate_plan,
            open_dashboard,
            hide_popover,
            get_tray_threshold,
            set_tray_threshold,
            get_expiring_notify,
            set_expiring_notify,
            set_privacy,
            get_autostart,
            set_autostart,
            updates_status,
            install_update,
            restart_after_update
        ])
        .setup(|app| {
            // 대시보드 창은 설정(`create: false`)을 따라 여기서 만든다. 로그인 자동 실행이면 숨긴 채 만들어
            // 메뉴바에만 두고, 직접 연 경우에만 보인다. 만든 뒤 show()로 바꾸면 Windows에서 반영되지 않는다.
            let autostart = std::env::args().any(|arg| arg == AUTOSTART_ARG);
            if let Some(config) = app.config().app.windows.iter().find(|w| w.label == "main").cloned() {
                tauri::WebviewWindowBuilder::from_config(app.handle(), &config)?
                    .visible(!autostart)
                    .build()?;
            }
            // 언어를 바꿀 때 다시 쓸 고정 메뉴 문구.
            let mut labels: Vec<(MenuItem<tauri::Wry>, &'static str, &'static str, &'static str)> = Vec::new();
            // 기본 앱 메뉴의 ⌘Q는 NSApp terminate로 바로 끝나 확인 창을 거치지 않는다.
            // 앱 메뉴를 직접 만들어 ⌘Q를 종료 확인 항목으로 보낸다. 편집 단축키는 기본 항목을 그대로 쓴다.
            #[cfg(target_os = "macos")]
            {
                use tauri::menu::{PredefinedMenuItem, Submenu};
                let about = MenuItem::with_id(app, "about", tr("About Ojak", "Ojak 정보", "Tentang Ojak"), true, None::<&str>)?;
                let settings = MenuItem::with_id(app, "settings", tr("Settings…", "설정…", "Pengaturan…"), true, Some("CmdOrCtrl+,"))?;
                let app_quit = MenuItem::with_id(app, "app-quit", tr("Quit Ojak…", "Ojak 종료…", "Keluar dari Ojak…"), true, Some("CmdOrCtrl+Q"))?;
                labels.push((about.clone(), "About Ojak", "Ojak 정보", "Tentang Ojak"));
                labels.push((settings.clone(), "Settings…", "설정…", "Pengaturan…"));
                labels.push((app_quit.clone(), "Quit Ojak…", "Ojak 종료…", "Keluar dari Ojak…"));
                let app_menu = Submenu::with_items(
                    app,
                    "Ojak",
                    true,
                    &[
                        &about,
                        &settings,
                        &PredefinedMenuItem::separator(app)?,
                        &PredefinedMenuItem::hide(app, None)?,
                        &PredefinedMenuItem::hide_others(app, None)?,
                        &PredefinedMenuItem::show_all(app, None)?,
                        &PredefinedMenuItem::separator(app)?,
                        &app_quit,
                    ],
                )?;
                let edit_menu = Submenu::with_items(
                    app,
                    tr("Edit", "편집", "Edit"),
                    true,
                    &[
                        &PredefinedMenuItem::undo(app, None)?,
                        &PredefinedMenuItem::redo(app, None)?,
                        &PredefinedMenuItem::separator(app)?,
                        &PredefinedMenuItem::cut(app, None)?,
                        &PredefinedMenuItem::copy(app, None)?,
                        &PredefinedMenuItem::paste(app, None)?,
                        &PredefinedMenuItem::select_all(app, None)?,
                    ],
                )?;
                let window_menu = Submenu::with_items(
                    app,
                    tr("Window", "윈도우", "Jendela"),
                    true,
                    &[&PredefinedMenuItem::minimize(app, None)?, &PredefinedMenuItem::close_window(app, None)?],
                )?;
                app.set_menu(Menu::with_items(app, &[&app_menu, &edit_menu, &window_menu])?)?;
                app.on_menu_event(|app, event| match event.id.as_ref() {
                    "app-quit" => request_quit(app),
                    "about" | "settings" => {
                        show_main(app);
                        let _ = app.emit("ojak://settings", ());
                    }
                    _ => {}
                });
            }
            let show = MenuItem::with_id(
                app,
                "show",
                tr("Open dashboard", "대시보드 열기", "Buka dasbor"),
                true,
                None::<&str>,
            )?;
            let now_title = MenuItem::with_id(app, "now-title", tr("Now in use", "지금 사용 중", "Sedang dipakai"), false, None::<&str>)?;
            let now = [
                MenuItem::with_id(app, "now-1", tr("Checking…", "확인 중…", "Memeriksa…"), false, None::<&str>)?,
                MenuItem::with_id(app, "now-2", "", false, None::<&str>)?,
                MenuItem::with_id(app, "now-3", "", false, None::<&str>)?,
            ];
            let separator = tauri::menu::PredefinedMenuItem::separator(app)?;
            let separator_2 = tauri::menu::PredefinedMenuItem::separator(app)?;
            let version = MenuItem::with_id(
                app,
                "version",
                format!("Ojak {}", app.package_info().version),
                false,
                None::<&str>,
            )?;
            let quit = MenuItem::with_id(
                app,
                "quit",
                tr("Quit Ojak…", "Ojak 종료…", "Keluar dari Ojak…"),
                true,
                None::<&str>,
            )?;
            labels.push((show.clone(), "Open dashboard", "대시보드 열기", "Buka dasbor"));
            labels.push((now_title.clone(), "Now in use", "지금 사용 중", "Sedang dipakai"));
            labels.push((quit.clone(), "Quit Ojak…", "Ojak 종료…", "Keluar dari Ojak…"));
            let service = MenuItem::with_id(
                app,
                "service-status",
                tr("Checking management service", "관리 서비스 상태 확인 중", "Memeriksa layanan pengelolaan"),
                false,
                None::<&str>,
            )?;
            let urgent = MenuItem::with_id(
                app,
                "urgent-status",
                tr("Checking auth and usage", "인증·사용량 확인 중", "Memeriksa autentikasi dan pemakaian"),
                false,
                None::<&str>,
            )?;
            let reset = MenuItem::with_id(
                app,
                "reset-status",
                tr("Checking reset times", "리셋 시각 확인 중", "Memeriksa waktu reset"),
                false,
                None::<&str>,
            )?;
            let sessions = MenuItem::with_id(
                app,
                "session-status",
                tr("Checking active sessions", "활성 세션 확인 중", "Memeriksa sesi aktif"),
                false,
                None::<&str>,
            )?;
            let assignment = MenuItem::with_id(
                app,
                "assignment",
                tr("Checking automatic allocation", "자동 배정 상태 확인 중", "Memeriksa alokasi otomatis"),
                false,
                None::<&str>,
            )?;
            let feedback = MenuItem::with_id(
                app,
                "action-status",
                tr(
                    "Private info hidden · existing sessions are never moved",
                    "개인정보를 숨깁니다 · 기존 세션은 이동하지 않습니다", "Info pribadi disembunyikan · sesi yang ada tidak pernah dipindah"),
                false,
                None::<&str>,
            )?;
            labels.push((
                feedback.clone(),
                "Private info hidden · existing sessions are never moved",
                "개인정보를 숨깁니다 · 기존 세션은 이동하지 않습니다",
                "Info pribadi disembunyikan · sesi yang ada tidak pernah dipindah",
            ));
            app.manage(MenuLabels(labels));
            let menu = Menu::with_items(
                app,
                &[
                    &now_title,
                    &now[0],
                    &now[1],
                    &now[2],
                    &separator,
                    &show,
                    &separator_2,
                    &service,
                    &urgent,
                    &reset,
                    &sessions,
                    &assignment,
                    &feedback,
                    &version,
                    &quit,
                ],
            )?;
            let (sender, receiver) = mpsc::sync_channel(1);
            let assignment_action = assignment.clone();
            // 왼쪽 클릭 = 잔여 한도 팝오버, 오른쪽 클릭 = 기존 메뉴(상태·종료). macOS 메뉴바 앱 관례다.
            let tray = TrayIconBuilder::with_id("ojak")
                .icon(tray_image())
                .icon_as_template(true)
                .tooltip("Ojak")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "show" => show_main(app),
                    "assignment" => {
                        let _ = assignment_action.set_enabled(false);
                        let _ = sender.try_send(());
                    }
                    "quit" => request_quit(app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click {
                        rect,
                        button: tauri::tray::MouseButton::Left,
                        button_state: tauri::tray::MouseButtonState::Up,
                        ..
                    } = event
                    {
                        toggle_popover(tray.app_handle(), rect);
                    }
                })
                .build(app)?;
            tauri::WebviewWindowBuilder::new(app, POPOVER, tauri::WebviewUrl::App("index.html".into()))
                .title("Ojak")
                .inner_size(POPOVER_WIDTH, 460.0)
                .decorations(false)
                .resizable(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .transparent(true)
                .shadow(true)
                .visible(false)
                .focused(false)
                .build()?;
            #[cfg(target_os = "macos")]
            if let Some(popover) = app.get_webview_window(POPOVER) {
                let _ = window_vibrancy::apply_vibrancy(&popover, window_vibrancy::NSVisualEffectMaterial::Popover, None, Some(12.0));
            }
            let app_handle = app.handle().clone();
            thread::spawn(move || {
                TrayStatus {
                    service,
                    urgent,
                    reset,
                    sessions,
                    assignment,
                    feedback,
                    now,
                    tray,
                    app: app_handle,
                }
                .poll(receiver)
            });
            #[cfg(target_os = "macos")]
            if let Some(w) = app.get_webview_window("main") {
                let _ = window_vibrancy::apply_vibrancy(
                    &w,
                    window_vibrancy::NSVisualEffectMaterial::Sidebar,
                    None,
                    None,
                );
            }
            default_autostart(app.handle());
            #[cfg(target_os = "macos")]
            if autostart {
                let _ = app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
            eprintln!("AAM_DESKTOP_READY");
            Ok(())
        })
        .on_window_event(|window, event| match event {
            // 팝오버는 다른 곳을 누르면 닫힌다. 메뉴바 팝오버 관례다.
            tauri::WindowEvent::Focused(false) if window.label() == POPOVER => {
                let _ = window.hide();
            }
            // 창을 닫으면 앱은 트레이로만 남는다. Dock 아이콘도 숨겨 메뉴바 앱처럼 동작한다.
            tauri::WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
                #[cfg(target_os = "macos")]
                if window.label() == "main" {
                    let _ = window.app_handle().set_activation_policy(tauri::ActivationPolicy::Accessory);
                }
            }
            _ => {}
        })
        .build(tauri::generate_context!())
        .expect("macOS 앱을 실행하지 못했습니다")
        .run(|app, event| match event {
            // 사용자가 시작한 종료(⌘Q·Dock)는 code가 없다. 확인 전이면 막고 확인 창을 띄운다.
            tauri::RunEvent::ExitRequested { code: None, api, .. } if !QUIT_CONFIRMED.load(Ordering::SeqCst) => {
                api.prevent_exit();
                request_quit(app);
            }
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen {
                has_visible_windows: false,
                ..
            } => {
                show_main(app);
            }
            _ => {}
        });
}
