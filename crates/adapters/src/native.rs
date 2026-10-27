use crate::{
    process::{base_env, executable, run_json, run_json_within, Probe, USAGE_PROBE_TIMEOUT},
    quota::{codex_buckets, codex_credits, stable_id, text},
    safety::{canonical_profile, check_settings, profile_var},
};
use aam_protocol::{now_ms, Account, ApiError, CreditsState, QuotaBucket};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

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

/// 공식 CLI 전체 점검(등록용). Codex는 신원 확인 뒤 한도 조회까지 같은 app-server에서 최선으로 시도한다.
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
    inspect_core(tool, profile, binary, label, cwd, true, None)
}

/// 계정 신원만 확인한다. 설정·프로필·신원 보호는 `inspect`와 같지만 Codex `account/rateLimits/read`는 부르지 않아
/// 느린 한도 조회가 신원 확인(실행 전 검증·초기 목록)을 막지 않는다. 한도는 `codex_usage`가 따로 가져온다.
pub(crate) fn inspect_identity(
    tool: &str,
    profile: &Path,
    binary: &Path,
    label: &str,
) -> Result<Account, ApiError> {
    inspect_identity_at(tool, profile, binary, label, profile)
}

pub(crate) fn inspect_identity_at(
    tool: &str,
    profile: &Path,
    binary: &Path,
    label: &str,
    cwd: &Path,
) -> Result<Account, ApiError> {
    inspect_core(tool, profile, binary, label, cwd, false, None)
}

/// 인증 갱신의 전후 identity 검사도 같은 전체 작업 deadline 안에서 수행한다.
pub(crate) fn inspect_identity_until(
    tool: &str,
    profile: &Path,
    binary: &Path,
    label: &str,
    deadline: Instant,
) -> Result<Account, ApiError> {
    inspect_core(tool, profile, binary, label, profile, false, Some(deadline))
}

/// 스캔(상태 갱신·한도 조회 뒤 재확인) 전용 identity 확인. Claude 조회는 실측 20초를 넘길 수 있어
/// 이 경로만 50초 절대 deadline(`USAGE_PROBE_TIMEOUT`)을 쓴다. Codex는 `inspect_identity`와 같다.
/// 실행 전 검증·인증 접근·실행 검사는 이 함수를 쓰지 않고 기존 제한을 유지한다.
pub(crate) fn inspect_identity_for_scan(
    tool: &str,
    profile: &Path,
    binary: &Path,
    label: &str,
) -> Result<Account, ApiError> {
    inspect_identity_for_scan_within(tool, profile, binary, label, USAGE_PROBE_TIMEOUT)
}

fn inspect_identity_for_scan_within(
    tool: &str,
    profile: &Path,
    binary: &Path,
    label: &str,
    limit: Duration,
) -> Result<Account, ApiError> {
    if tool != "claude" {
        return inspect_identity(tool, profile, binary, label);
    }
    let account = blank_account(tool, profile, binary, label);
    check_settings(tool, profile, profile)?;
    let value = run_json_within(
        binary,
        &["auth", "status", "--json"],
        &profile_env(tool, profile),
        profile,
        limit,
    )?;
    parse_claude(&value, account, profile)
}

fn inspect_core(
    tool: &str,
    profile: &Path,
    binary: &Path,
    label: &str,
    cwd: &Path,
    with_usage: bool,
    deadline: Option<Instant>,
) -> Result<Account, ApiError> {
    let account = blank_account(tool, profile, binary, label);
    check_settings(tool, profile, cwd)?;
    let env = profile_env(tool, profile);
    match tool {
        "claude" => {
            let args = ["auth", "status", "--json"];
            let value = match deadline {
                Some(deadline) => crate::process::run_json_until(binary, &args, &env, cwd, deadline)?,
                None => run_json(binary, &args, &env, cwd)?,
            };
            parse_claude(&value, account, profile)
        }
        "codex" => {
            let (mut server, value) = codex_session(binary, &env, cwd, profile, deadline)?;
            let mut account = parse_codex(&value, account, profile)?;
            if with_usage && account.auth_status == "authenticated" {
                if let Ok(limits) = server.request(3, "account/rateLimits/read", json!({})) {
                    account.buckets = codex_buckets(
                        &limits,
                        account.identity_key.as_deref().unwrap_or(&account.id),
                        now_ms(),
                    );
                    // 크레딧은 이 응답에 명시된 경우에만 기록한다. 조회에 실패하면 이전 값을 이어 쓰지 않는다.
                    account.credits = codex_credits(&limits, now_ms());
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

/// Codex app-server를 띄워 실제 CODEX_HOME이 선택한 프로필인지 확인하고 `account/read` 응답을 돌려준다.
/// 토큰 갱신은 요청하지 않는다(`refreshToken:false`).
fn codex_session(
    binary: &Path,
    env: &BTreeMap<String, String>,
    cwd: &Path,
    profile: &Path,
    deadline: Option<Instant>,
) -> Result<(Probe, Value), ApiError> {
    let args = ["app-server", "--listen", "stdio://"];
    let mut server = match deadline {
        Some(deadline) => Probe::spawn_until(binary, &args, env, cwd, deadline)?,
        None => Probe::spawn(binary, &args, env, cwd)?,
    };
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
    Ok((server, value))
}

/// 계정에 기록된 프로필·원본 CLI 연결을 다시 해석한다. 프로필 경로가 등록 이후 바뀌었으면 거부하고,
/// 진입 경로(symlink)는 공식 업데이트를 따르도록 매번 실행 파일로 해석한다.
pub(crate) fn bound_paths(account: &Account) -> Result<(PathBuf, PathBuf), ApiError> {
    let profile = account.profile_path.as_deref().ok_or_else(|| ApiError::new("PROFILE_UNBOUND", "이 계정에 공식 프로필이 연결돼 있지 않아요. 공식 CLI에 로그인한 뒤 프로필을 등록해 주세요."))?;
    let canonical = canonical_profile(Path::new(profile))?;
    if canonical != Path::new(profile) {
        return Err(ApiError::new("PROFILE_BINDING_CHANGED", "등록 이후 프로필 경로가 바뀌었어요. 바로가기로 계정을 바꾸지 않아요. 폴더를 원래 경로로 되돌리거나, 이 계정의 연결을 끊은 뒤 새 경로를 연결해 주세요."));
    }
    let binary = account
        .binary_path
        .as_deref()
        .ok_or_else(|| ApiError::new("BINARY_MISSING", "등록된 공식 CLI가 없어요."))?;
    Ok((canonical, executable(Path::new(binary))?))
}

/// 인증된 계정이 기대한 신원과 같은지 확인한다.
fn require_identity(fresh: &Account, expected: &str) -> Result<(), ApiError> {
    if fresh.auth_status != "authenticated" {
        return Err(ApiError::new(
            "AUTH_REQUIRED",
            "선택한 프로필에서 공식 로그인이 필요해요.",
        ));
    }
    if fresh.identity_key.as_deref() != Some(expected) || !fresh.can_launch {
        return Err(ApiError::new("IDENTITY_DRIFT", "실제 계정이 등록된 계정과 같지 않아 한도를 반영하지 않았어요. 원래 계정으로 '다시 로그인'해 주세요."));
    }
    Ok(())
}

/// Codex 한도·크레딧을 공식 app-server에서만 읽는다. 조회 전 같은 서버의 `account/read`로 실제 CODEX_HOME과
/// 인증된 신원이 `account.identity_key`와 같음을 확인하고, 조회 뒤 새 프로세스로 신원을 다시 확인해
/// 조회 중 로그인이 바뀐 경우 결과를 버린다. 토큰은 읽거나 갱신하거나 복사하지 않는다.
/// 조회 실패는 그대로 오류이며 알 수 없는 값을 0으로 채우지 않는다.
pub(crate) fn codex_usage(
    account: &Account,
) -> Result<(Vec<QuotaBucket>, Option<CreditsState>), ApiError> {
    if account.tool != "codex" {
        return Err(ApiError::new(
            "TOOL_UNSUPPORTED",
            "지원하는 공식 CLI를 선택해 주세요.",
        ));
    }
    let expected = account.identity_key.as_deref().ok_or_else(|| {
        ApiError::new(
            "IDENTITY_UNVERIFIED",
            "등록 계정의 안정적인 identity를 확인하지 못했습니다. 공식 로그인 후 재등록해 주세요.",
        )
    })?;
    let (profile, binary) = bound_paths(account)?;
    check_settings("codex", &profile, &profile)?;
    let env = profile_env("codex", &profile);
    let (mut server, value) = codex_session(&binary, &env, &profile, &profile, None)?;
    let current = parse_codex(
        &value,
        blank_account("codex", &profile, &binary, &account.label),
        &profile,
    )?;
    require_identity(&current, expected)?;
    let limits = server.request(3, "account/rateLimits/read", json!({}))?;
    // 조회 프로세스를 먼저 정리한 뒤 새 프로세스로 신원이 그대로인지 확인한다.
    drop(server);
    let after = inspect_identity("codex", &profile, &binary, &account.label)?;
    require_identity(&after, expected)?;
    let observed_at = now_ms();
    let buckets = codex_buckets(&limits, expected, observed_at);
    // 구조화 한도에서 실제 버킷을 하나도 얻지 못하면 성공이 아니다. 빈 목록이 기존 관측을 지우지 않도록 오류로 돌려준다.
    if buckets.is_empty() {
        return Err(ApiError::new(
            "PROBE_SCHEMA",
            "Codex 한도 응답에서 한도 정보를 확인하지 못했어요. CLI 버전을 확인해 주세요.",
        ));
    }
    Ok((buckets, codex_credits(&limits, observed_at)))
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

    /// 결정적인 가짜 Codex app-server. 프로필 폴더의 파일로 응답을 바꾸고, 한도 조회가 호출되면 `rate.log`에 남긴다.
    /// `block` 파일이 있으면 한도 조회에서 응답하지 않아 느린 공급자를 흉내 낸다. `switch` 파일이 있으면 한도 조회 중
    /// 로그인된 계정이 `account-b.json`으로 바뀐다.
    #[cfg(unix)]
    fn fake_codex() -> (PathBuf, PathBuf) {
        use crate::process::test_support::write_script;
        let root = std::env::temp_dir().join(format!("aam-native-{}", aam_protocol::new_id()));
        std::fs::create_dir_all(root.join("profile")).unwrap();
        let root = root.canonicalize().unwrap();
        let profile = root.join("profile");
        let binary = root.join("codex");
        write_script(
            &binary,
            r#"while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":1,"result":{"codexHome":"%s"}}\n' "$CODEX_HOME" ;;
    *'"method":"account/read"'*) cat "$CODEX_HOME/account.json" ;;
    *'"method":"account/rateLimits/read"'*)
      echo rate >> "$CODEX_HOME/rate.log"
      if [ -f "$CODEX_HOME/block" ]; then sleep 120; fi
      if [ -f "$CODEX_HOME/switch" ]; then cp "$CODEX_HOME/account-b.json" "$CODEX_HOME/account.json"; fi
      cat "$CODEX_HOME/rate.json" ;;
  esac
done"#,
        );
        let account = |email: &str| {
            format!(
                "{}\n",
                json!({"id":2,"result":{"account":{"type":"chatgpt","email":email,"planType":"pro"}}})
            )
        };
        std::fs::write(profile.join("account.json"), account("a@invalid.test")).unwrap();
        std::fs::write(profile.join("account-b.json"), account("b@invalid.test")).unwrap();
        std::fs::write(
            profile.join("rate.json"),
            format!(
                "{}\n",
                json!({"id":3,"result":{"rateLimits":{"primary":{"usedPercent":42,"windowDurationMins":300,"resetsAt":4102444800i64}}}})
            ),
        )
        .unwrap();
        (profile, binary)
    }

    #[cfg(unix)]
    fn rate_calls(profile: &Path) -> usize {
        std::fs::read_to_string(profile.join("rate.log")).map(|text| text.lines().count()).unwrap_or(0)
    }

    #[cfg(unix)]
    #[test]
    fn codex_identity_is_returned_without_calling_a_blocked_rate_limit_method() {
        use crate::process::test_support::retry_start;
        let (profile, binary) = fake_codex();
        std::fs::write(profile.join("block"), b"").unwrap();
        let started = std::time::Instant::now();
        let account = retry_start(|| inspect_identity("codex", &profile, &binary, "계정")).unwrap();
        assert_eq!(account.auth_status, "authenticated");
        assert!(account.can_launch);
        assert_eq!(
            account.identity_key.as_deref(),
            Some("openai|email:a@invalid.test|workspace:")
        );
        assert!(account.buckets.is_empty() && account.credits.is_none());
        assert_eq!(rate_calls(&profile), 0);
        // 한도 조회는 신원 확인과 별개이며 막힌 응답을 기다리지 않는다는 뜻의 상한(조회 시간 제한 20초보다 한참 짧다).
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        std::fs::remove_dir_all(profile.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn codex_usage_is_read_only_for_the_same_authenticated_profile() {
        use crate::process::test_support::retry_start;
        let (profile, binary) = fake_codex();
        let account = retry_start(|| inspect_identity("codex", &profile, &binary, "계정")).unwrap();
        let (buckets, credits) = retry_start(|| codex_usage(&account)).unwrap();
        assert_eq!(rate_calls(&profile), 1);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].used_percent, Some(42.0));
        assert_eq!(buckets[0].status, "known");
        assert!(credits.is_none());
        // 한도 버킷은 등록 계정의 신원에 묶인다.
        let expected = codex_buckets(
            &json!({"rateLimits":{"primary":{"usedPercent":42,"windowDurationMins":300,"resetsAt":4102444800i64}}}),
            account.identity_key.as_deref().unwrap(),
            0,
        );
        assert_eq!(buckets[0].id, expected[0].id);
        std::fs::remove_dir_all(profile.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn codex_usage_rejects_identity_that_differs_before_quota_is_requested() {
        use crate::process::test_support::retry_start;
        let (profile, binary) = fake_codex();
        let account = retry_start(|| inspect_identity("codex", &profile, &binary, "계정")).unwrap();
        std::fs::copy(profile.join("account-b.json"), profile.join("account.json")).unwrap();
        let error = retry_start(|| codex_usage(&account)).unwrap_err();
        assert_eq!(error.code, "IDENTITY_DRIFT");
        assert_eq!(rate_calls(&profile), 0);
        std::fs::remove_dir_all(profile.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn codex_usage_rejects_identity_change_during_the_fetch() {
        use crate::process::test_support::retry_start;
        let (profile, binary) = fake_codex();
        let account = retry_start(|| inspect_identity("codex", &profile, &binary, "계정")).unwrap();
        std::fs::write(profile.join("switch"), b"").unwrap();
        let error = retry_start(|| codex_usage(&account)).unwrap_err();
        assert_eq!(error.code, "IDENTITY_DRIFT");
        assert_eq!(rate_calls(&profile), 1);
        std::fs::remove_dir_all(profile.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn codex_usage_without_any_bucket_is_an_error_not_an_empty_success() {
        use crate::process::test_support::retry_start;
        let (profile, binary) = fake_codex();
        let account = retry_start(|| inspect_identity("codex", &profile, &binary, "계정")).unwrap();
        std::fs::write(
            profile.join("rate.json"),
            format!("{}\n", json!({"id":3,"result":{"rateLimits":{}}})),
        )
        .unwrap();
        let error = retry_start(|| codex_usage(&account)).unwrap_err();
        assert_eq!(error.code, "PROBE_SCHEMA");
        std::fs::remove_dir_all(profile.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn codex_usage_requires_a_pinned_identity() {
        let (profile, binary) = fake_codex();
        let mut account = blank_account("codex", &profile, &binary, "계정");
        account.identity_key = None;
        assert_eq!(codex_usage(&account).unwrap_err().code, "IDENTITY_UNVERIFIED");
        assert_eq!(rate_calls(&profile), 0);
        std::fs::remove_dir_all(profile.parent().unwrap()).unwrap();
    }

    #[cfg(unix)]
    fn fake_claude(script: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("aam-native-claude-{}", aam_protocol::new_id()));
        std::fs::create_dir_all(root.join("profile")).unwrap();
        let root = root.canonicalize().unwrap();
        let binary = root.join("claude");
        crate::process::test_support::write_script(&binary, script);
        (root.join("profile"), binary)
    }

    #[cfg(unix)]
    #[test]
    fn claude_scan_identity_is_bounded_by_its_absolute_deadline() {
        use crate::process::test_support::retry_start;
        let (profile, binary) = fake_claude("sleep 30");
        let started = Instant::now();
        let result = retry_start(|| {
            inspect_identity_for_scan_within("claude", &profile, &binary, "계정", Duration::from_millis(500))
        });
        assert_eq!(result.unwrap_err().code, "PROBE_TIMEOUT");
        assert!(started.elapsed() < Duration::from_secs(10));
        std::fs::remove_dir_all(profile.parent().unwrap()).unwrap();
    }
}

