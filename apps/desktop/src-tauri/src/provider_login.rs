use aam_protocol::ApiError;
use serde::Serialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, io::{BufRead, BufReader}, process::{ChildStdin, Command, Stdio}, sync::{Arc, LazyLock, Mutex}, thread};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    id: String,
    provider: String,
    target_account_id: Option<String>,
    account_id: Option<String>,
    state: String,
    identity: Option<Value>,
    error: Option<ApiError>,
    started_at: i64,
    updated_at: i64,
}
struct Job { status: Status, input: Option<ChildStdin> }
static JOBS: LazyLock<Mutex<BTreeMap<String, Arc<Mutex<Job>>>>> = LazyLock::new(|| Mutex::new(BTreeMap::new()));
fn terminal(state: &str) -> bool { matches!(state, "succeeded" | "failed" | "canceled") }
fn unavailable() -> ApiError { ApiError::new("LOGIN_FAILED", "로그인 작업을 확인하지 못했어요.") }
fn lookup(id: &str) -> Result<Arc<Mutex<Job>>, ApiError> {
    JOBS.lock().map_err(|_| unavailable())?.get(id).cloned()
        .ok_or_else(|| ApiError::new("LOGIN_JOB_NOT_FOUND", "로그인 작업의 결과를 확인할 수 없어요."))
}
fn token(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && value.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

#[tauri::command]
pub fn start_provider_login(provider: String, account_id: Option<String>, label: Option<String>, settings_digest: Option<String>) -> Result<Status, ApiError> {
    if !matches!(provider.as_str(), "anthropic" | "openai-codex" | "xai-oauth" | "google-antigravity")
        || account_id.as_ref().is_some_and(|id| !token(id, 128))
        || settings_digest.as_ref().is_some_and(|digest| !token(digest, 256))
        || label.as_ref().is_some_and(|name| name.trim().is_empty() || name.len() > 120 || name.chars().any(char::is_control)) {
        return Err(ApiError::new("INVALID_PARAMS", "로그인 공급자와 계정을 확인해 주세요."));
    }
    let mut jobs = JOBS.lock().map_err(|_| unavailable())?;
    for job in jobs.values() {
        let job = job.lock().map_err(|_| unavailable())?;
        if !terminal(&job.status.state) && job.status.provider == provider {
            if job.status.target_account_id == account_id { return Ok(job.status.clone()); }
            return Err(ApiError::new("LOGIN_BUSY", "이 공급자의 다른 로그인이 진행 중이에요. 먼저 완료하거나 취소해 주세요."));
        }
    }
    let mut command = Command::new(super::launcher()?);
    command.args(["provider-login", "--provider", &provider]);
    if let Some(id) = &account_id { command.args(["--account", id]); }
    if let Some(name) = &label { command.args(["--label", name]); }
    if let Some(digest) = &settings_digest { command.args(["--settings-digest", digest]); }
    command.env("AAM_HOME", super::paths()?.home).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    super::hidden(&mut command);
    #[cfg(unix)] {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|_| ApiError::new("LOGIN_FAILED", "공식 로그인 실행을 시작하지 못했어요."))?;
    let output = child.stdout.take().ok_or_else(unavailable)?;
    let now = aam_protocol::now_ms();
    let status = Status { id: aam_protocol::new_id(), provider, target_account_id: account_id, account_id: None, state: "starting".into(), identity: None, error: None, started_at: now, updated_at: now };
    let job = Arc::new(Mutex::new(Job { status: status.clone(), input: child.stdin.take() }));
    jobs.insert(status.id.clone(), job.clone());
    thread::spawn(move || {
        let mut final_event = None;
        for line in BufReader::new(output).lines() {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str::<Value>(&line) else { continue };
            let Some(state) = value.get("state").and_then(Value::as_str) else { continue };
            if !matches!(state, "starting" | "waiting" | "verifying" | "syncing" | "succeeded" | "failed" | "canceled") { continue; }
            if terminal(state) { final_event = Some(value); continue; }
            if let Ok(mut job) = job.lock() {
                job.status.state = state.into();
                job.status.updated_at = aam_protocol::now_ms();
            }
        }
        let exited_ok = child.wait().is_ok_and(|exit| exit.success());
        if let Ok(mut job) = job.lock() {
            let event = final_event.unwrap_or(Value::Null);
            let state = event.get("state").and_then(Value::as_str).unwrap_or("failed");
            let verified_id = event.get("accountId").and_then(Value::as_str).filter(|id| token(id, 128));
            job.status.state = if state == "succeeded" && exited_ok && verified_id.is_some() { "succeeded" } else if state == "canceled" { "canceled" } else { "failed" }.into();
            if job.status.state == "succeeded" {
                job.status.account_id = verified_id.map(str::to_owned);
                if let Some(identity) = event.get("identity") {
                    if let Some(label) = identity.get("label").and_then(Value::as_str).filter(|s| s.len() <= 512 && !s.chars().any(char::is_control)) {
                        let workspace = identity.get("workspace").and_then(Value::as_str).filter(|s| s.len() <= 512 && !s.chars().any(char::is_control));
                        job.status.identity = Some(json!({"label": label, "workspace": workspace}));
                    }
                }
            } else if job.status.state == "failed" {
                let code = event.get("error").and_then(|e| e.get("code")).and_then(Value::as_str)
                    .filter(|s| s.len() <= 64 && !s.is_empty() && s.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')).unwrap_or("LOGIN_FAILED");
                job.status.error = Some(ApiError::new(code, "공식 로그인을 완료하거나 계정을 확인하지 못했어요."));
            }
            job.status.updated_at = aam_protocol::now_ms();
            job.input.take();
        }
    });
    Ok(status)
}

#[tauri::command]
pub fn provider_login_status(id: String) -> Result<Status, ApiError> {
    Ok(lookup(&id)?.lock().map_err(|_| unavailable())?.status.clone())
}
#[tauri::command]
pub fn list_provider_logins() -> Result<Vec<Status>, ApiError> {
    JOBS.lock().map_err(|_| unavailable())?.values().map(|job| Ok(job.lock().map_err(|_| unavailable())?.status.clone())).collect()
}
#[tauri::command]
pub fn cancel_provider_login(id: String) -> Result<Status, ApiError> {
    let job = lookup(&id)?;
    let mut job = job.lock().map_err(|_| unavailable())?;
    if !terminal(&job.status.state) {
        job.input.take();
    }
    Ok(job.status.clone())
}
