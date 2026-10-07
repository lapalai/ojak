use crate::{
    process::{base_env, run_json, Probe},
    quota::{codex_buckets, stable_id, text},
    safety::{check_settings, profile_var},
};
use aam_protocol::{now_ms, Account, ApiError};
use serde_json::{json, Value};
use std::{collections::BTreeMap, path::Path};

/// omp 계정은 Ojak이 직접 실행하지 않습니다. omp에서 `/model`로 `ojak-*` 모델을 고르면
/// 계정 브릿지가 배정하며, 여기서는 사용량만 관측합니다.
pub(crate) const OMP_GATE: &str = "omp 계정은 Ojak이 직접 실행하지 않아요. omp에서 /model로 ojak-* 모델을 고르면 Ojak이 계정을 골라요. 이 연결은 한도를 보는 데만 써요.";

pub(crate) fn profile_env(tool: &str, profile: &Path) -> BTreeMap<String, String> {
    let mut env = base_env();
    // 기본 경로를 명시해도 Claude의 전역 .claude.json 위치가 달라지므로 기본 프로필은 unset으로 실행합니다.
    let native_default = tool == "claude"
        && std::env::var_os("CLAUDE_CONFIG_DIR").is_none()
        && crate::user_home()
            .ok()
            .and_then(|home| home.join(".claude").canonicalize().ok())
            .as_deref()
            == Some(profile);
    if !native_default {
        if let Some(key) = profile_var(tool) {
            env.insert(key.into(), profile.to_string_lossy().into_owned());
        }
    }
    env
}

pub(crate) fn blank_account(tool: &str, profile: &Path, binary: &Path, label: &str) -> Account {
    let provider = match tool {
        "claude" => "anthropic",
        "codex" => "openai",
        _ => "other",
    };
    Account {
        id: stable_id(&[tool, "configured", &profile.to_string_lossy()]),
        provider: provider.into(),
        tool: tool.into(),
        label: label.into(),
        profile_path: Some(profile.to_string_lossy().into_owned()),
        binary_path: Some(binary.to_string_lossy().into_owned()),
        auth_status: "unverified".into(),
        verification: "configured".into(),
        enabled: true,
        max_concurrency: 1,
        last_checked_at: now_ms(),
        ..Account::default()
    }
}

pub(crate) fn claude_identity(value: &Value) -> Option<String> {
    let org = text(value, "orgId")?;
    if let Some(subject) = text(value, "accountId").or_else(|| text(value, "userId")) {
        return Some(format!("anthropic|subject:{subject}|workspace:{org}"));
    }
    let email = text(value, "email")?.to_lowercase();
    Some(format!("anthropic|email:{email}|workspace:{org}"))
}

fn apply_identity(account: &mut Account, identity: Option<String>, profile: &Path) {
    account.identity_key = identity;
    if let Some(identity) = account.identity_key.as_ref() {
        account.id = stable_id(&[&account.tool, identity, &profile.to_string_lossy()]);
        account.verification = "preflight-verified".into();
        account.can_launch = true;
    } else {
        account.reason = Some("공식 상태 응답에 안정적인 계정 식별자가 없어 이메일만으로 계정을 고정하지 않아요. 식별자를 주는 CLI 버전이 필요해요.".into());
    }
}

pub(crate) fn parse_claude(
    value: &Value,
    mut account: Account,
    profile: &Path,
) -> Result<Account, ApiError> {
    let Some(logged_in) = value.get("loggedIn").and_then(Value::as_bool) else {
        return Err(ApiError::new(
            "PROBE_SCHEMA",
            "Claude 로그인 상태 응답 형식이 바뀌었어요. CLI 버전을 확인해 주세요.",
        ));
    };
    if !logged_in {
        account.auth_status = "auth-required".into();
        account.reason = Some("선택한 Claude 프로필에서 공식 로그인이 필요해요.".into());
        return Ok(account);
    }
    if value.get("authMethod").and_then(Value::as_str) != Some("claude.ai")
        || value.get("apiProvider").and_then(Value::as_str) != Some("firstParty")
    {
        return Err(ApiError::new("AUTH_OVERRIDE_CONFLICT", "Claude 공식 상태가 구독 로그인이 아니에요. API 키나 다른 공급자로 자동 바꾸지 않아요."));
    }
    if let Some(reported) = text(value, "configDirectory") {
        if Path::new(&reported).canonicalize().ok().as_deref() != Some(profile) {
            return Err(ApiError::new(
                "PROFILE_IDENTITY_MISMATCH",
                "Claude가 선택한 프로필과 다른 인증 폴더를 보고했어요.",
            ));
        }
    }
    account.auth_status = "authenticated".into();
    account.email = text(value, "email");
    account.organization = text(value, "orgName").or_else(|| text(value, "orgId"));
    account.plan = text(value, "subscriptionType");
    apply_identity(&mut account, claude_identity(value), profile);
    Ok(account)
}

/// 공식 auth 파일에서 비밀이 아닌 account_id만 읽습니다. 토큰 값은 읽지 않습니다.
fn codex_workspace(profile: &Path) -> Option<String> {
    use std::io::Read;
    let file = std::fs::File::open(profile.join("auth.json")).ok()?;
    let mut text = String::new();
    file.take(1024 * 1024).read_to_string(&mut text).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let id = value.get("tokens")?.get("account_id")?.as_str()?;
    (!id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
    .then(|| id.to_owned())
}

pub(crate) fn parse_codex(
    value: &Value,
    mut account: Account,
    profile: &Path,
) -> Result<Account, ApiError> {
    let Some(native) = value.get("account") else {
        return Err(ApiError::new(
            "PROBE_SCHEMA",
            "Codex 계정 응답 형식이 바뀌었어요.",
        ));
    };
    if native.is_null() {
        account.auth_status = "auth-required".into();
        account.reason = Some("선택한 CODEX_HOME에서 공식 Codex 로그인이 필요해요. omp 로그인은 Codex 로그인을 대신하지 않아요.".into());
        return Ok(account);
    }
    if native.get("type").and_then(Value::as_str) != Some("chatgpt") {
        return Err(ApiError::new("AUTH_OVERRIDE_CONFLICT", "Codex가 ChatGPT 구독이 아닌 로그인을 보고했어요. API 결제로 자동 바꾸지 않아요."));
    }
    account.auth_status = "authenticated".into();
    account.email = text(native, "email");
    account.plan = text(native, "planType");
    // 설치된 Codex는 account/read에 계정 식별자를 넣지 않습니다. 공식 auth 파일의 비밀 없는 account_id만 추가로 확인합니다.
    let workspace = text(native, "chatgptAccountId")
        .or_else(|| text(native, "accountId"))
        .or_else(|| text(&value["workspaceRouting"], "chatgptAccountId"))
        .or_else(|| codex_workspace(profile));
    account.organization = workspace.clone();
    let Some(email) = account.email.as_ref().map(|email| email.to_lowercase()) else {
        apply_identity(&mut account, None, profile);
        return Ok(account);
    };
    let identity = format!(
        "openai|email:{email}|workspace:{}",
        workspace.clone().unwrap_or_default()
    );
    apply_identity(&mut account, Some(identity), profile);
    if workspace.is_none() {
        // 계정 구분은 이메일과 프로필 격리로만 보장합니다. 같은 이메일의 여러 워크스페이스는 구분하지 않습니다.
        account.reason = Some("설치된 Codex CLI가 계정 식별자를 주지 않아 이메일과 선택한 CODEX_HOME으로만 구분해요. 같은 이메일의 여러 워크스페이스는 구분하지 않습니다.".into());
    }
    Ok(account)
}

pub(crate) fn inspect(
    tool: &str,
    profile: &Path,
    binary: &Path,
    label: &str,
) -> Result<Account, ApiError> {
    inspect_at(tool, profile, binary, label, profile)
}

pub(crate) fn inspect_at(
    tool: &str,
    profile: &Path,
    binary: &Path,
    label: &str,
    cwd: &Path,
) -> Result<Account, ApiError> {
    let account = blank_account(tool, profile, binary, label);
    check_settings(tool, profile, cwd)?;
    let env = profile_env(tool, profile);
    match tool {
        "claude" => {
            let value = run_json(binary, &["auth", "status", "--json"], &env, cwd)?;
            parse_claude(&value, account, profile)
        }
        "codex" => {
            let mut server =
                Probe::spawn(binary, &["app-server", "--listen", "stdio://"], &env, cwd)?;
            let initialized = server.request(1,"initialize",json!({"clientInfo":{"name":"aam_metadata","title":"AI Account Manager","version":"0.1.0"},"capabilities":{"experimentalApi":true}}))?;
            if let Some(home) = text(&initialized, "codexHome") {
                if Path::new(&home).canonicalize().ok().as_deref() != Some(profile) {
                    return Err(ApiError::new(
                        "PROFILE_IDENTITY_MISMATCH",
                        "Codex가 선택한 CODEX_HOME과 다른 프로필을 보고했어요.",
                    ));
                }
            } else {
                return Err(ApiError::new(
                    "PROFILE_UNVERIFIED",
                    "Codex가 실제 CODEX_HOME을 확인해 주지 않았어요.",
                ));
            }
            server.send(&json!({"method":"initialized"}))?;
            let value = server.request(2, "account/read", json!({"refreshToken":false}))?;
            let mut account = parse_codex(&value, account, profile)?;
            if account.auth_status == "authenticated" {
                if let Ok(limits) = server.request(3, "account/rateLimits/read", json!({})) {
                    account.buckets = codex_buckets(
                        &limits,
                        account.identity_key.as_deref().unwrap_or(&account.id),
                        now_ms(),
                    );
                }
            }
            Ok(account)
        }
        _ => Err(ApiError::new(
            "TOOL_UNSUPPORTED",
            "지원하는 공식 CLI를 선택해 주세요.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_subscription_and_workspace_are_both_required() {
        let path = Path::new("/sanitized/profile");
        let account = blank_account("claude", path, Path::new("/sanitized/claude"), "계정");
        let value = json!({"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"person@invalid.test","orgId":"workspace-a","subscriptionType":"max"});
        assert!(
            parse_claude(&value, account.clone(), path)
                .unwrap()
                .can_launch
        );
        let mut paid = value.clone();
        paid["authMethod"] = json!("api_key");
        assert_eq!(
            parse_claude(&paid, account.clone(), path).unwrap_err().code,
            "AUTH_OVERRIDE_CONFLICT"
        );
        let mut missing = value;
        missing.as_object_mut().unwrap().remove("orgId");
        assert!(!parse_claude(&missing, account, path).unwrap().can_launch);
    }
    #[test]
    fn codex_accounts_are_distinguished_by_email_and_profile() {
        let directory = std::env::temp_dir().join(format!("aam-codex-{}", aam_protocol::new_id()));
        std::fs::create_dir_all(&directory).unwrap();
        let account = blank_account("codex", &directory, Path::new("/sanitized/codex"), "계정");
        let value =
            json!({"account":{"type":"chatgpt","email":"Person@Invalid.Test","planType":"pro"}});
        // 계정 식별자를 제공하지 않는 CLI에서도 이메일과 프로필로 계정을 구분합니다.
        let email_only = parse_codex(&value, account.clone(), &directory).unwrap();
        assert!(email_only.can_launch);
        assert_eq!(
            email_only.identity_key.as_deref(),
            Some("openai|email:person@invalid.test|workspace:")
        );
        assert!(email_only
            .reason
            .is_some_and(|reason| reason.contains("여러 워크스페이스는 구분하지 않습니다")));
        // 공식 auth 파일의 account_id가 있으면 워크스페이스까지 구분합니다.
        std::fs::write(
            directory.join("auth.json"),
            json!({"tokens":{"account_id":"workspace-1"}}).to_string(),
        )
        .unwrap();
        let scoped = parse_codex(&value, account.clone(), &directory).unwrap();
        assert_eq!(
            scoped.identity_key.as_deref(),
            Some("openai|email:person@invalid.test|workspace:workspace-1")
        );
        assert_ne!(email_only.id, scoped.id);
        // 이메일이 없으면 계정을 고정하지 않습니다.
        let anonymous = parse_codex(
            &json!({"account":{"type":"chatgpt","planType":"pro"}}),
            account,
            &directory,
        )
        .unwrap();
        assert!(!anonymous.can_launch);
        std::fs::remove_dir_all(&directory).unwrap();
    }
}
