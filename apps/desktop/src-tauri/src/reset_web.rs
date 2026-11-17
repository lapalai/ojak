//! Account-isolated browser transport. Never exports cookies or grants remote pages IPC.
use aam_protocol::{ApiError, Paths, ResetWebConnection, ResetWebJob, ResetWebStatus};
use parking_lot::Mutex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::{BTreeMap, BTreeSet}, sync::{mpsc, Arc}, thread, time::{Duration, Instant}};
#[cfg(target_os = "macos")]
use std::sync::LazyLock;
use tauri::{AppHandle, Manager, WebviewWindow, WebviewWindowBuilder, WebviewUrl};

const USAGE_URL: &str = "https://claude.ai/settings/usage";
const COLLECTOR: &str = include_str!("reset_web_collect.js");
const MAX_RESULT: usize = 131_072;

struct Runtime {
    paths: Paths,
    client_id: String,
    creation: Mutex<()>,
    active: Mutex<BTreeSet<String>>,
    loaded: Mutex<BTreeSet<String>>,
    page_refreshes: Mutex<BTreeSet<String>>,
    profiles: Mutex<BTreeMap<String, ResetWebConnection>>,
}
#[derive(Deserialize)]
struct Connected { connection: ResetWebConnection, status: ResetWebStatus }
#[derive(Deserialize)]
struct Poll { connections: Vec<ResetWebConnection>, jobs: Vec<ResetWebJob> }

fn unavailable() -> ApiError {
    ApiError::new("WEB_BROWSER_UNAVAILABLE", "웹 조회 창에 연결하지 못했어요. Ojak을 켜 둔 상태에서 다시 확인해 주세요.")
}
fn platform_supported() -> bool {
    #[cfg(target_os = "macos")]
    {
        static SUPPORTED: LazyLock<bool> = LazyLock::new(|| std::process::Command::new("/usr/bin/sw_vers").arg("-productVersion")
            .output().ok().filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|version| version.trim().split('.').next()?.parse::<u32>().ok())
            .is_some_and(|major| major >= 14));
        *SUPPORTED
    }
    #[cfg(windows)]
    { true }
    #[cfg(not(any(target_os = "macos", windows)))]
    { false }
}
fn call<T: serde::de::DeserializeOwned>(runtime: &Runtime, method: &str, params: Value) -> Result<T, ApiError> {
    let value = aam_protocol::call(&runtime.paths, method, params)?;
    serde_json::from_value(value).map_err(|_| ApiError::new("RESET_RESPONSE_INVALID", "서비스의 웹 연결 응답을 확인하지 못했어요."))
}
fn label(connection_id: &str) -> String { format!("reset-web-{connection_id}") }
fn claude_url(url: &tauri::Url) -> bool {
    url.scheme() == "https" && url.host_str() == Some("claude.ai") && url.port_or_known_default() == Some(443)
}
fn window(app: &AppHandle, runtime: &Arc<Runtime>, connection: &ResetWebConnection) -> Result<WebviewWindow, ApiError> {
    if !platform_supported() { return Err(ApiError::new("WEB_UNSUPPORTED_PLATFORM", "계정별 웹 연결은 macOS 14 이상 또는 Windows에서 지원해요.")); }
    let store_id = uuid::Uuid::parse_str(&connection.connection_id).map_err(|_| unavailable())?;
    uuid::Uuid::parse_str(&connection.organization_id).map_err(|_| unavailable())?;
    let _creation = runtime.creation.lock();
    let window_label = label(&connection.connection_id);
    if let Some(window) = app.get_webview_window(&window_label) { return Ok(window); }
    let account_id = connection.account_id.clone();
    let loaded_label = window_label.clone();
    let on_load = Arc::clone(runtime);
    let builder = WebviewWindowBuilder::new(app, &window_label, WebviewUrl::External(USAGE_URL.parse().map_err(|_| unavailable())?))
        .title(crate::tr("Ojak · Claude web connection", "Ojak · Claude 웹 연결", "Ojak · Koneksi web Claude"))
        .inner_size(1000.0, 760.0).visible(false).focused(false)
        // This is a login window, not a general browser. No custom scheme or local-file navigation.
        .on_navigation(|url| url.scheme() == "https" && url.port_or_known_default() == Some(443)
            && matches!(url.host_str(), Some("claude.ai" | "accounts.google.com" | "auth.anthropic.com")))
        .on_page_load(move |_window, payload| {
            if payload.event() == tauri::webview::PageLoadEvent::Started {
                on_load.loaded.lock().remove(&loaded_label);
            } else if claude_url(payload.url()) {
                on_load.loaded.lock().insert(loaded_label.clone());
                if !matches!(payload.url().path(), "/login" | "/signup") {
                    on_load.page_refreshes.lock().insert(account_id.clone());
                }
            }
        });
    #[cfg(target_os = "macos")]
    let builder = builder.data_store_identifier(*store_id.as_bytes());
    #[cfg(windows)]
    let builder = {
        let directory = runtime.paths.home.join("reset-web-profiles").join(store_id.to_string());
        std::fs::create_dir_all(&directory).map_err(|_| unavailable())?;
        builder.data_directory(directory)
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let _ = store_id;
    let window = builder.build().map_err(|_| unavailable())?;
    runtime.profiles.lock().insert(connection.account_id.clone(), connection.clone());
    Ok(window)
}

// Invoked from background workers, never waits on the WebView/UI thread itself.
fn evaluate(window: &WebviewWindow, script: String, deadline: Instant) -> Result<Value, ApiError> {
    let (send, receive) = mpsc::sync_channel(1);
    window.eval_with_callback(script, move |value| { let _ = send.send(value); }).map_err(|_| unavailable())?;
    let remaining = deadline.saturating_duration_since(Instant::now()).min(Duration::from_secs(2));
    let value = receive.recv_timeout(remaining).map_err(|_| unavailable())?;
    if value.len() > MAX_RESULT * 2 { return Err(unavailable()); }
    let parsed: Value = serde_json::from_str(&value).map_err(|_| unavailable())?;
    match parsed {
        Value::String(text) if text.len() <= MAX_RESULT => serde_json::from_str(&text).map_err(|_| unavailable()),
        Value::Null => Ok(Value::Null),
        _ => Err(unavailable()),
    }
}
fn collect(app: &AppHandle, runtime: &Arc<Runtime>, job: &ResetWebJob) -> Value {
    let connection = ResetWebConnection { account_id: job.account_id.clone(), connection_id: job.connection_id.clone(), organization_id: job.organization_id.clone() };
    let window = match window(app, runtime, &connection) {
        Ok(window) => window,
        Err(error) => return json!({"errorCode":error.code}),
    };
    let remaining = job.expires_at.saturating_sub(aam_protocol::now_ms()).clamp(0, 19_000) as u64;
    let deadline = Instant::now() + Duration::from_millis(remaining);
    let window_label = label(&job.connection_id);
    while !runtime.loaded.lock().contains(&window_label) {
        if Instant::now() >= deadline { return json!({"errorCode":"WEB_LOGIN_REQUIRED"}); }
        thread::sleep(Duration::from_millis(100));
    }
    if !window.url().ok().as_ref().is_some_and(claude_url) { return json!({"errorCode":"WEB_LOGIN_REQUIRED"}); }
    let slot = format!("__ojak_reset_{}", job.request_id);
    let Ok(slot_literal) = serde_json::to_string(&slot) else { return json!({"errorCode":"WEB_RESPONSE_INVALID"}); };
    let Ok(org_literal) = serde_json::to_string(&job.organization_id) else { return json!({"errorCode":"WEB_RESPONSE_INVALID"}); };
    if window.eval(format!("({COLLECTOR})({org_literal},{slot_literal})")).is_err() { return json!({"errorCode":"WEB_BROWSER_UNAVAILABLE"}); }
    let script = format!("(()=>{{const s=globalThis[{slot_literal}];if(!s?.done)return null;delete globalThis[{slot_literal}];return JSON.stringify(s.result)}})()");
    while Instant::now() < deadline {
        if !window.url().ok().as_ref().is_some_and(claude_url) { break; }
        match evaluate(&window, script.clone(), deadline) {
            Ok(Value::Null) => thread::sleep(Duration::from_millis(150)),
            Ok(value) if value.is_object() => return value,
            _ => break,
        }
    }
    let _ = window.eval(format!("delete globalThis[{slot_literal}]"));
    json!({"errorCode":"WEB_BROWSER_UNAVAILABLE"})
}
fn forget_window(app: &AppHandle, runtime: &Runtime, connection: &ResetWebConnection) -> Result<(), ApiError> {
    let window_label = label(&connection.connection_id);
    if let Some(window) = app.get_webview_window(&window_label) {
        // The window has its own profile; this never clears the dashboard or another account's session.
        window.clear_all_browsing_data().map_err(|_| unavailable())?;
        window.destroy().map_err(|_| unavailable())?;
    }
    runtime.loaded.lock().remove(&window_label);
    runtime.page_refreshes.lock().remove(&connection.account_id);
    let mut profiles = runtime.profiles.lock();
    if profiles.get(&connection.account_id).is_some_and(|current| current.connection_id == connection.connection_id) {
        profiles.remove(&connection.account_id);
    }
    Ok(())
}

pub fn start(app: &AppHandle) {
    let Ok(paths) = crate::paths() else { return; };
    let runtime = Arc::new(Runtime { paths, client_id: aam_protocol::new_id(), creation: Mutex::new(()),
        active: Mutex::new(BTreeSet::new()), loaded: Mutex::new(BTreeSet::new()), page_refreshes: Mutex::new(BTreeSet::new()),
        profiles: Mutex::new(BTreeMap::new()) });
    app.manage(Arc::clone(&runtime));
    let app = app.clone();
    thread::spawn(move || {
        let mut previous = BTreeMap::<String, ResetWebConnection>::new();
        loop {
            let refreshes = std::mem::take(&mut *runtime.page_refreshes.lock());
            for account_id in refreshes {
                let _: Result<ResetWebStatus, _> = call(&runtime, "resetWeb.request", json!({"accountId":account_id}));
            }
            if let Ok(poll) = call::<Poll>(&runtime, "resetWeb.poll", json!({"clientId":runtime.client_id})) {
                let current: BTreeMap<_, _> = poll.connections.into_iter().map(|connection| (connection.connection_id.clone(), connection)).collect();
                // Only previously polled profiles can be removed. A connection just opened by the UI
                // must not be destroyed because this poll raced its connect RPC.
                for (id, connection) in &previous {
                    if !current.contains_key(id) { let _ = forget_window(&app, &runtime, connection); }
                }
                previous = current;
                for job in poll.jobs {
                    if !previous.get(&job.connection_id).is_some_and(|connection| connection.account_id == job.account_id && connection.organization_id == job.organization_id) { continue; }
                    if !runtime.active.lock().insert(job.connection_id.clone()) { continue; }
                    let runtime = Arc::clone(&runtime);
                    let app = app.clone();
                    thread::spawn(move || {
                        let result = collect(&app, &runtime, &job);
                        // Never retry a completion. An old or late job cannot update a new binding.
                        let _: Result<Value, _> = call(&runtime, "resetWeb.complete", json!({"clientId":runtime.client_id,
                            "requestId":job.request_id,"connectionId":job.connection_id,"result":result}));
                        runtime.active.lock().remove(&job.connection_id);
                    });
                }
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}

#[tauri::command]
pub async fn connect_reset_web(app: AppHandle, account_id: String) -> Result<ResetWebStatus, ApiError> {
    let runtime = app.try_state::<Arc<Runtime>>().map(|state| Arc::clone(&state)).ok_or_else(unavailable)?;
    tauri::async_runtime::spawn_blocking(move || {
        if !platform_supported() { return Err(ApiError::new("WEB_UNSUPPORTED_PLATFORM", "계정별 웹 연결은 macOS 14 이상 또는 Windows에서 지원해요.")); }
        let connected: Connected = call(&runtime, "resetWeb.connect", json!({"accountId":account_id}))?;
        let window = window(&app, &runtime, &connected.connection)?;
        window.show().map_err(|_| unavailable())?;
        window.unminimize().map_err(|_| unavailable())?;
        window.set_focus().map_err(|_| unavailable())?;
        Ok(connected.status)
    }).await.map_err(|_| unavailable())?
}
#[tauri::command]
pub async fn open_reset_web(app: AppHandle, account_id: String) -> Result<Value, ApiError> {
    let runtime = app.try_state::<Arc<Runtime>>().map(|state| Arc::clone(&state)).ok_or_else(unavailable)?;
    tauri::async_runtime::spawn_blocking(move || {
        // An explicit open may reconnect a logged-out profile, but cannot pick another account's browser.
        let connected: Connected = call(&runtime, "resetWeb.connect", json!({"accountId":account_id}))?;
        let window = window(&app, &runtime, &connected.connection)?;
        window.navigate(USAGE_URL.parse().map_err(|_| unavailable())?).map_err(|_| unavailable())?;
        window.show().map_err(|_| unavailable())?;
        window.unminimize().map_err(|_| unavailable())?;
        window.set_focus().map_err(|_| unavailable())?;
        Ok(json!({"opened":true}))
    }).await.map_err(|_| unavailable())?
}
#[tauri::command]
pub async fn disconnect_reset_web(app: AppHandle, account_id: String) -> Result<ResetWebStatus, ApiError> {
    let runtime = app.try_state::<Arc<Runtime>>().map(|state| Arc::clone(&state)).ok_or_else(unavailable)?;
    tauri::async_runtime::spawn_blocking(move || {
        let status: ResetWebStatus = call(&runtime, "resetWeb.disconnect", json!({"accountId":account_id}))?;
        let connection = runtime.profiles.lock().get(&account_id).cloned();
        if let Some(connection) = connection { forget_window(&app, &runtime, &connection)?; }
        Ok(status)
    }).await.map_err(|_| unavailable())?
}
