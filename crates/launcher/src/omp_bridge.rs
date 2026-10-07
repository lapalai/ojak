//! 원본 omp를 AAM 계정 브릿지에 연결한다.
//!
//! 연결하면 AAM 서비스의 브릿지를 켜고 omp 확장(`integrations/omp/aam-accounts.js`)을 설치한다.
//! 실행 중인 omp 계정의 Ojak 공급자 로그인은 서비스가 주기적으로 맞춘다. 사용자는 새 omp 세션에서 `/model`로 `ojak-*` 모델을 고른다.
//! 원래 공급자(`anthropic` 등)와 omp 설정 파일은 바꾸지 않는다.

use crate::{
    omp_broker,
    omp_extension::{self, Extension},
};
use aam_protocol::{call, ApiError, Paths, BRIDGE_PORT};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::PathBuf,
    time::{Duration, Instant},
};

const ACCOUNTS: Extension = Extension {
    source: include_bytes!("../../../integrations/omp/aam-accounts.js"),
    owner: "ai-account-manager.omp-accounts.v1",
    directory: "aam-accounts",
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// 브릿지가 켜져 있고 omp 확장이 최신으로 설치되어 `/login`에 AAM 공급자가 보이는지.
    pub connected: bool,
    pub extension_installed: bool,
    pub extension_path: String,
    pub url: String,
    /// 서비스가 보고한 브릿지 상태. 서비스가 꺼져 있으면 null.
    pub bridge: Value,
}

fn error(code: &str, message: &str) -> ApiError {
    ApiError::new(code, message)
}
#[cfg(unix)]
fn home() -> Result<PathBuf, ApiError> {
    aam_protocol::user_home()
        .filter(|path| path.is_absolute())
        .ok_or_else(|| error("HOME_UNAVAILABLE", "사용자 홈 경로를 확인하지 못했어요."))
}

fn write_settings(paths: &Paths, enabled: bool) -> Result<(), ApiError> {
    fs::create_dir_all(&paths.home).map_err(|_| error("BRIDGE_STATE_FAILED", "Ojak 연결 상태 폴더를 만들지 못했어요."))?;
    let path = paths.bridge_settings();
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json!({ "enabled": enabled }).to_string())
        .and_then(|_| aam_protocol::secure::restrict_file(&tmp))
        .and_then(|_| fs::rename(&tmp, &path))
        .map_err(|_| error("BRIDGE_STATE_FAILED", "Ojak 연결 설정을 쓰지 못했어요."))
}

fn bridge_status(paths: &Paths) -> Value {
    call(paths, "bridge.status", json!({})).unwrap_or(Value::Null)
}

fn healthy() -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&([127, 0, 0, 1], BRIDGE_PORT).into(), Duration::from_millis(500)) else {
        return false;
    };
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
    if stream.write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").is_err() {
        return false;
    }
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    response.starts_with("HTTP/1.1 200") && response.contains("\"enabled\":true")
}

/// omp 확장이 등록하는 Ojak 공급자. `integrations/omp/aam-accounts.js`의 PROVIDERS와 같은 이름이다.
const OJAK_PROVIDERS: [&str; 5] = ["ojak-claude", "ojak-codex", "ojak-antigravity", "ojak-grok", "ojak-zai"];
/// 이름을 바꾸기 전 공급자 ID와 새 ID. 연결할 때 omp 설정과 broker 로그인을 새 이름으로 옮긴다.
const RENAMED: [(&str, &str); 5] = [
    ("aam-claude", "ojak-claude"),
    ("aam-codex", "ojak-codex"),
    ("aam-antigravity", "ojak-antigravity"),
    ("aam-grok", "ojak-grok"),
    ("aam-zai", "ojak-zai"),
];
/// omp에 원래 공급자 계정이 하나도 없을 때. gateway가 생기지 않으므로 기다리지 않는다.
const NO_OMP_ACCOUNT: &str = "omp에 로그인된 계정이 없어요. omp에서 /login으로 Claude·Codex 등 계정을 먼저 추가해 주세요.";
/// Ojak 공급자와 원래 공급자. 연결을 해제하면 역할 모델을 원래 공급자로 되돌린다. `service/src/bridge.rs` ALIASES와 같은 표다.
const UPSTREAM: [(&str, &str); 5] = [
    ("ojak-claude", "anthropic"),
    ("ojak-codex", "openai-codex"),
    ("ojak-antigravity", "google-antigravity"),
    ("ojak-grok", "xai-oauth"),
    ("ojak-zai", "zai"),
];
/// 연결 해제 때 바꾼 역할 모델 기록. 다시 연결하면 사용자가 그 사이 바꾸지 않은 역할만 원래대로 돌린다.
const ROLE_BACKUP: &str = "omp-model-roles.json";

fn omp() -> Result<PathBuf, ApiError> {
    #[cfg(windows)]
    let fallback = std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
        .map(|path| path.join("omp/omp.exe"))
        .ok_or_else(|| error("HOME_UNAVAILABLE", "omp 설치 경로를 확인하지 못했어요."))?;
    #[cfg(unix)]
    let fallback = home()?.join(".local/bin/omp");
    Ok(std::env::var_os("OMP_NATIVE_BIN").map(PathBuf::from).unwrap_or(fallback))
}

/// omp 자신의 설정 명령으로 값을 읽는다. 값이 없으면 `Null`.
fn config_get(key: &str) -> Result<Value, ApiError> {
    let output = std::process::Command::new(omp()?)
        .args(["config", "get", key, "--json"])
        .output()
        .map_err(|_| error("OMP_CONFIG_FAILED", "omp 설정을 읽지 못했어요."))?;
    Ok(serde_json::from_slice::<Value>(&output.stdout).ok().and_then(|value| value.get("value").cloned()).unwrap_or(Value::Null))
}

fn config_set(key: &str, value: &Value) -> Result<(), ApiError> {
    let status = std::process::Command::new(omp()?)
        .args(["config", "set", key, &value.to_string()])
        .output()
        .map_err(|_| error("OMP_CONFIG_FAILED", "omp 설정을 바꾸지 못했어요."))?;
    if !status.status.success() {
        return Err(error("OMP_CONFIG_FAILED", "omp 설정을 바꾸지 못했어요."));
    }
    Ok(())
}

/// `aam-claude/<모델>`처럼 이전 공급자 ID로 적힌 모델 참조를 새 ID로 바꾼다. 해당 없으면 `None`.
pub(crate) fn renamed_model(reference: &str) -> Option<String> {
    let (provider, rest) = reference.split_once('/')?;
    RENAMED.iter().find(|(old, _)| *old == provider).map(|(_, new)| format!("{new}/{rest}"))
}

/// `ojak-claude/<모델>`(또는 이전 `aam-claude/…`)을 원래 공급자 모델 참조로 바꾼다. 해당 없으면 `None`.
pub(crate) fn upstream_model(reference: &str) -> Option<String> {
    let reference = renamed_model(reference).unwrap_or_else(|| reference.to_owned());
    let (provider, rest) = reference.split_once('/')?;
    UPSTREAM.iter().find(|(ojak, _)| *ojak == provider).map(|(_, upstream)| format!("{upstream}/{rest}"))
}

/// 역할 모델 표에서 Ojak 공급자 참조를 원래 공급자로 바꾼다. 바꾼 역할과 원래 값을 돌려준다.
pub(crate) fn detached_roles(roles: &mut serde_json::Map<String, Value>) -> serde_json::Map<String, Value> {
    let mut changed = serde_json::Map::new();
    for (role, value) in roles.iter_mut() {
        if let Some(next) = value.as_str().and_then(upstream_model) {
            changed.insert(role.clone(), value.clone());
            *value = Value::String(next);
        }
    }
    changed
}

/// 기록해 둔 역할 중 해제 뒤 사용자가 바꾸지 않은(아직 원래 공급자 값인) 역할만 Ojak 값으로 돌린다.
pub(crate) fn reattached_roles(roles: &mut serde_json::Map<String, Value>, backup: &serde_json::Map<String, Value>) -> bool {
    let mut changed = false;
    for (role, saved) in backup {
        let Some(saved) = saved.as_str() else { continue };
        let expected = upstream_model(saved);
        if roles.get(role).and_then(Value::as_str).map(str::to_owned) == expected {
            roles.insert(role.clone(), Value::String(renamed_model(saved).unwrap_or_else(|| saved.to_owned())));
            changed = true;
        }
    }
    changed
}

/// 연결 해제: Ojak 모델을 가리키던 전역 역할을 원래 공급자로 되돌리고 원래 값을 기록한다.
fn detach_model_roles(paths: &Paths) -> Result<(), ApiError> {
    let Value::Object(mut roles) = config_get("modelRoles")? else { return Ok(()) };
    let changed = detached_roles(&mut roles);
    if changed.is_empty() {
        return Ok(());
    }
    let path = paths.home.join(ROLE_BACKUP);
    let mut backup = fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Map<String, Value>>(&bytes).ok()).unwrap_or_default();
    backup.extend(changed);
    fs::write(&path, Value::Object(backup).to_string())
        .and_then(|_| aam_protocol::secure::restrict_file(&path))
        .map_err(|_| error("OMP_CONFIG_FAILED", "역할 모델 기록을 저장하지 못해 설정을 바꾸지 않았어요."))?;
    config_set("modelRoles", &Value::Object(roles))
}

/// 다시 연결: 해제 때 기록한 역할을 Ojak 모델로 돌린다.
fn reattach_model_roles(paths: &Paths) -> Result<(), ApiError> {
    let path = paths.home.join(ROLE_BACKUP);
    let Some(backup) = fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Map<String, Value>>(&bytes).ok()) else { return Ok(()) };
    if let Value::Object(mut roles) = config_get("modelRoles")? {
        if reattached_roles(&mut roles, &backup) {
            config_set("modelRoles", &Value::Object(roles))?;
        }
    }
    let _ = fs::remove_file(path);
    Ok(())
}

/// `enabledModels`가 공급자를 제한하고 있으면 `/model`에 Ojak 모델이 보이도록 패턴을 더하거나 뺀다.
/// 목록이 비어 있으면 제한이 없는 상태이므로 건드리지 않는다.
fn scope_ojak_models(include: bool) -> Result<(), ApiError> {
    let current: Vec<String> = serde_json::from_value(config_get("enabledModels")?).unwrap_or_default();
    let Some(next) = scoped_models(&current, include) else { return Ok(()) };
    config_set("enabledModels", &json!(next))
}

/// 바꿀 필요가 있으면 새 `enabledModels` 목록. 제한이 없거나 이미 원하는 상태면 `None`.
/// 이전 공급자 ID의 패턴은 연결할 때 새 ID로 바꾸고, 해제할 때 함께 뺀다.
pub(crate) fn scoped_models(current: &[String], include: bool) -> Option<Vec<String>> {
    let pattern = |provider: &str| format!("{provider}/*");
    let next: Vec<String> = if include {
        if current.is_empty() {
            return None;
        }
        let mut next: Vec<String> = Vec::with_capacity(current.len() + OJAK_PROVIDERS.len());
        for entry in current.iter().map(|entry| renamed_model(entry).unwrap_or_else(|| entry.clone())) {
            if !next.contains(&entry) {
                next.push(entry);
            }
        }
        for entry in OJAK_PROVIDERS.iter().map(|provider| pattern(provider)) {
            if !next.contains(&entry) {
                next.push(entry);
            }
        }
        next
    } else {
        current
            .iter()
            .filter(|entry| !OJAK_PROVIDERS.iter().chain(RENAMED.iter().map(|(old, _)| old)).any(|provider| **entry == pattern(provider)))
            .cloned()
            .collect()
    };
    (next != current).then_some(next)
}

/// 역할별 모델(`modelRoles`)에 남은 이전 공급자 ID를 새 ID로 바꾼다.
fn migrate_model_roles() -> Result<(), ApiError> {
    let Value::Object(mut roles) = config_get("modelRoles")? else { return Ok(()) };
    let mut changed = false;
    for value in roles.values_mut() {
        if let Some(next) = value.as_str().and_then(renamed_model) {
            *value = Value::String(next);
            changed = true;
        }
    }
    if changed { config_set("modelRoles", &Value::Object(roles)) } else { Ok(()) }
}

/// broker에 원래 공급자(anthropic 등) 로그인이 있는지. `ojak-*`·`aam-*`만 있는 경우는 계정이 아니다.
pub(crate) fn has_upstream_account(providers: &[String]) -> bool {
    UPSTREAM.iter().any(|(_, upstream)| providers.iter().any(|item| item == upstream))
}


fn extension_error(error: Box<dyn std::error::Error>) -> ApiError {
    ApiError::new("EXTENSION_FAILED", error.to_string())
}

fn extension_state() -> Result<(PathBuf, bool), ApiError> {
    let directory = omp_extension::extension_directory(&ACCOUNTS).map_err(extension_error)?;
    let current = omp_extension::inspect(&ACCOUNTS, &directory).map_err(extension_error)?.is_some_and(|installed| installed.current);
    Ok((directory, current))
}

pub fn status(paths: &Paths) -> Result<Status, ApiError> {
    let (directory, installed) = extension_state()?;
    let bridge = bridge_status(paths);
    Ok(Status {
        connected: installed && bridge.get("enabled") == Some(&Value::Bool(true)),
        extension_installed: installed,
        extension_path: directory.display().to_string(),
        url: format!("http://127.0.0.1:{BRIDGE_PORT}"),
        bridge,
    })
}

pub fn connect(paths: &Paths) -> Result<Status, ApiError> {
    call(paths, "status.read", json!({}))?;
    if !omp_broker::status(paths)?.connected {
        return Err(error("BROKER_REQUIRED", "이 연결은 omp 로그인 연결의 계정을 써요. 먼저 `aam omp-broker connect`를 실행해 주세요."));
    }
    write_settings(paths, true)?;
    // 계정이 없으면 gateway가 생기지 않는다. 30초를 기다리지 않고 다음 행동을 알려 준다.
    if omp_broker::logged_in_providers().is_ok_and(|providers| !has_upstream_account(&providers)) {
        return Err(error("BRIDGE_NOT_READY", NO_OMP_ACCOUNT));
    }
    // 계정은 있는데 gateway가 아직 뜨는 중이면 기존처럼 기다린다. 오래 걸리면 진행을 보여 준다.
    let started = Instant::now();
    let deadline = started + Duration::from_secs(30);
    let mut announced = false;
    let mut noted = 0u64;
    let ready = loop {
        let status = bridge_status(paths);
        let running = status
            .get("gateways")
            .and_then(Value::as_array)
            .is_some_and(|gateways| gateways.iter().any(|gateway| gateway.get("running") == Some(&Value::Bool(true))));
        if healthy() && running && paths.bridge_token().exists() {
            break true;
        }
        if !announced {
            crate::omp_broker::cli_progress("Ojak 브릿지가 계정 연결을 준비하는 중…");
            announced = true;
        }
        if Instant::now() >= deadline {
            break false;
        }
        let elapsed = started.elapsed().as_secs();
        if elapsed >= 10 && elapsed / 10 > noted {
            noted = elapsed / 10;
            crate::omp_broker::cli_progress(&format!("Ojak 브릿지가 계정 연결을 준비하는 중… {elapsed}초"));
        }
        std::thread::sleep(Duration::from_millis(500));
    };
    if !ready {
        let status = bridge_status(paths);
        let detail = status.get("error").and_then(Value::as_str).unwrap_or("").trim();
        let message = if detail.is_empty() {
            "Ojak 브릿지가 준비되지 않아 omp 확장을 설치하지 않았습니다. 계정 연결이 아직 시작되지 않았습니다. 잠시 뒤 다시 시도해 주세요.".to_owned()
        } else {
            format!("Ojak 브릿지가 준비되지 않아 omp 확장을 설치하지 않았습니다. {detail}")
        };
        return Err(ApiError::new("BRIDGE_NOT_READY", message));
    }
    let (directory, current) = extension_state()?;
    if !current {
        // 이전 판이 설치돼 있으면 소유를 확인한 뒤 교체한다. 실행 중인 omp는 다음 실행부터 새 판을 읽는다.
        omp_extension::uninstall(&ACCOUNTS, &directory).map_err(extension_error)?;
        omp_extension::install(&ACCOUNTS, &directory).map_err(extension_error)?;
    }
    scope_ojak_models(true)?;
    migrate_model_roles()?;
    reattach_model_roles(paths)?;
    crate::omp_broker::cli_progress("Ojak 공급자는 자동으로 로그인됩니다. 새 omp 세션에서 /model로 ojak-* 모델을 고르세요.");
    status(paths)
}

pub fn disconnect(paths: &Paths) -> Result<Status, ApiError> {
    let (directory, _) = extension_state()?;
    omp_extension::uninstall(&ACCOUNTS, &directory).map_err(extension_error)?;
    scope_ojak_models(false)?;
    detach_model_roles(paths)?;
    write_settings(paths, false)?;
    status(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_upstream_account_fails_fast_even_if_ojak_login_exists() {
        let owned = |items: &[&str]| items.iter().map(|item| (*item).to_owned()).collect::<Vec<_>>();
        assert!(!has_upstream_account(&owned(&[])));
        assert!(!has_upstream_account(&owned(&["ojak-claude", "aam-zai"])));
        assert!(has_upstream_account(&owned(&["anthropic"])));
        assert!(has_upstream_account(&owned(&["openai-codex", "ojak-codex"])));
    }


    #[test]
    fn model_scope_gains_and_loses_only_ojak_patterns() {
        let current: Vec<String> = ["anthropic/*", "openai-codex/*"].map(String::from).to_vec();
        let included = scoped_models(&current, true).unwrap();
        assert_eq!(&included[..2], &current[..]);
        assert!(included.contains(&"ojak-claude/*".to_owned()) && included.contains(&"ojak-zai/*".to_owned()));
        // 이미 포함돼 있으면 다시 쓰지 않는다.
        assert_eq!(scoped_models(&included, true), None);
        assert_eq!(scoped_models(&included, false).unwrap(), current);
        // 제한이 없는 설정(빈 목록)은 연결할 때 건드리지 않는다.
        assert_eq!(scoped_models(&[], true), None);
    }

    #[test]
    fn previous_aam_ids_move_to_ojak_without_duplicates() {
        let legacy: Vec<String> = ["anthropic/*", "aam-claude/*", "aam-zai/*"].map(String::from).to_vec();
        let included = scoped_models(&legacy, true).unwrap();
        assert_eq!(included.iter().filter(|entry| entry.starts_with("aam-")).count(), 0);
        assert_eq!(included.iter().filter(|entry| *entry == "ojak-claude/*").count(), 1);
        assert_eq!(included.len(), 6);
        // 해제하면 이전 ID 패턴도 함께 뺀다.
        assert_eq!(scoped_models(&legacy, false).unwrap(), vec!["anthropic/*".to_owned()]);
        assert_eq!(renamed_model("aam-claude/claude-opus-5-5:auto").as_deref(), Some("ojak-claude/claude-opus-5-5:auto"));
        assert_eq!(renamed_model("anthropic/claude-opus-5-5"), None);
        assert_eq!(renamed_model("aam-unknown/x"), None);
    }

    #[test]
    fn disconnect_restores_upstream_roles_and_reconnect_keeps_user_changes() {
        let mut roles = json!({
            "default": "ojak-claude/claude-opus-5-5",
            "task": "aam-claude/claude-fable-5-1:auto",
            "plan": "openai-codex/gpt-6-astra:auto",
            "vision": "ojak-antigravity/gemini-3.8-flash:auto"
        }).as_object().unwrap().clone();
        let backup = detached_roles(&mut roles);
        assert_eq!(roles["default"], "anthropic/claude-opus-5-5");
        assert_eq!(roles["task"], "anthropic/claude-fable-5-1:auto");
        assert_eq!(roles["vision"], "google-antigravity/gemini-3.8-flash:auto");
        // 원래 공급자 역할은 건드리지 않고 기록하지도 않는다.
        assert_eq!(roles["plan"], "openai-codex/gpt-6-astra:auto");
        assert!(!backup.contains_key("plan"));
        // 해제 중에 사용자가 vision을 다른 모델로 바꿨다면 다시 연결해도 그 선택을 유지한다.
        roles.insert("vision".into(), json!("anthropic/claude-sonnet-5"));
        assert!(reattached_roles(&mut roles, &backup));
        assert_eq!(roles["default"], "ojak-claude/claude-opus-5-5");
        assert_eq!(roles["task"], "ojak-claude/claude-fable-5-1:auto");
        assert_eq!(roles["vision"], "anthropic/claude-sonnet-5");
    }
}
