mod native;
mod observed;
#[cfg(any(target_os = "macos", windows))]
mod observed_metadata;
pub use observed::{discover_sessions, open_jsonl_writers, ObservedScan};
mod process;
mod quota;
mod safety;
mod settings;
mod shared;
mod takeover;
pub use takeover::{configured_model, session_owner, SessionOwner};
pub use settings::{
    apply_settings_import, prepare_settings_import, preview_settings, SettingsImport,
    SettingsPreview,
};

use aam_protocol::{
    new_id, now_ms, Account, ApiError, IdentityEvidence, LaunchIntent, LaunchPlan, Notice, Paths,
    ToolStatus, NATIVE_DEFAULT_MODEL,
};
use native::{blank_account, inspect, profile_env, OMP_GATE};
use quota::{omp_buckets, provider_id, report_identity, stable_id, text};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

/// 같은 도구·공급자에 여러 계정이 연결될 수 있으므로 확인된 이메일을 라벨에 함께 표시합니다.
pub(crate) fn account_label(base: &str, email: Option<&str>) -> String {
    match email.map(str::trim).filter(|email| !email.is_empty()) {
        // 이미 이메일이 들어간 라벨에는 다시 붙이지 않습니다.
        Some(email) if !base.contains(email) => format!("{base} · {email}"),
        _ => base.to_owned(),
    }
}

pub struct ScanResult {
    pub accounts: Vec<Account>,
    pub tools: Vec<ToolStatus>,
    pub notices: Vec<Notice>,
    pub merged_observation_ids: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EnrollmentPlan {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub profile_path: String,
    pub tool: String,
    pub label: String,
}

fn user_home() -> Result<PathBuf, ApiError> {
    aam_protocol::user_home()
        .ok_or_else(|| {
            ApiError::new(
                "HOME_MISSING",
                "지금 사용자의 홈 경로를 확인하지 못했어요.",
            )
        })
}

fn tool_details(
    tool: &str,
) -> (
    &'static str,
    &'static str,
    &'static str,
    Option<&'static str>,
) {
    match tool {
        "claude" => ("Claude Code", "anthropic", "profile", None),
        "codex" => ("Codex", "openai", "profile", None),
        _ => ("Oh My Pi", "other", "observation-only", Some(OMP_GATE)),
    }
}

fn known_tool(tool: &str) -> Result<(), ApiError> {
    if ["claude", "codex"].contains(&tool) {
        Ok(())
    } else {
        Err(ApiError::new(
            "TOOL_UNSUPPORTED",
            "Claude 또는 Codex를 선택해 주세요.",
        ))
    }
}

fn disabled(mut account: Account, error: ApiError) -> Account {
    account.can_launch = false;
    account.auth_status = if error.code == "AUTH_REQUIRED" {
        "auth-required"
    } else {
        "error"
    }
    .into();
    account.verification = "configured".into();
    account.reason = Some(error.message);
    account.last_checked_at = now_ms();
    for bucket in &mut account.buckets {
        if bucket.used_percent.is_some() {
            bucket.status = "stale".into();
        }
    }
    account
}

fn preserve_preferences(mut fresh: Account, old: &Account) -> Account {
    fresh.enabled = old.enabled;
    fresh.max_concurrency = old.max_concurrency;
    // 자동 생성 라벨은 이메일 표시를 덧붙일 수 있게 갱신하고, 사용자가 바꾼 이름은 그대로 유지합니다.
    if !fresh.label.starts_with(&old.label) {
        fresh.label = old.label.clone();
    }
    fresh
}

/// 계정은 도구의 현재 원본 CLI 진입 경로를 따릅니다. 원본을 찾지 못하면 기존 연결을 그대로 검사합니다.
fn refresh_bound(old: &Account, binary: Option<&Path>) -> Account {
    let mut bound = old.clone();
    if let Some(binary) = binary {
        bound.binary_path = Some(binary.to_string_lossy().into_owned());
    }
    let refreshed = (|| {
        let (profile, executable) = binding_paths(&bound)?;
        let mut fresh = inspect(&old.tool, &profile, &executable, &old.label)?;
        fresh.binary_path = bound.binary_path.clone();
        if let Some(expected) = old.identity_key.as_ref() {
            if fresh.identity_key.as_ref() != Some(expected) {
                return Err(ApiError::new("IDENTITY_DRIFT", "이 프로필의 계정이 등록된 계정과 다르거나 로그아웃됐어요. 기존 연결은 바꾸지 않았어요. 공식 로그인 후 다시 등록해 주세요."));
            }
        }
        // 미로그인 프로필은 account.register에서만 새 identity binding으로 전환합니다.
        if old.identity_key.is_none() && fresh.identity_key.is_some() {
            return Err(ApiError::new("REGISTRATION_REQUIRED", "공식 로그인이 끝났어요. 프로필을 등록해 새 계정 연결을 확정해 주세요."));
        }
        fresh.id = old.id.clone();
        fresh.omp_credential_pins = old.omp_credential_pins.clone();
        if fresh.buckets.is_empty() {
            fresh.buckets = old.buckets.clone();
            for bucket in &mut fresh.buckets {
                if now_ms().saturating_sub(bucket.observed_at) > 900_000
                    || bucket.resets_at.is_some_and(|v| v <= now_ms())
                {
                    bucket.status = if bucket.used_percent.is_some() {
                        "stale"
                    } else {
                        "unknown"
                    }
                    .into();
                }
            }
        }
        // 등록된 프로필도 확인된 이메일로 구분되게 표시합니다. 이미 이름에 있으면 그대로 둡니다.
        fresh.label = account_label(&fresh.label, fresh.email.as_deref());
        Ok(preserve_preferences(fresh, old))
    })();
    refreshed.unwrap_or_else(|error| disabled(bound, error))
}

fn notice(code: &str, message: impl Into<String>) -> Notice {
    Notice {
        id: code.into(),
        level: "warning".into(),
        title: "연결 확인이 필요해요".into(),
        message: message.into(),
    }
}

fn matches_report(account: &Account, report: &Value) -> bool {
    let Some(identity) = account.identity_key.as_deref() else {
        return false;
    };
    let meta = &report["metadata"];
    match account.tool.as_str() {
        "claude" if report["provider"].as_str() == Some("anthropic") => {
            if native::claude_identity(meta).as_deref() == Some(identity) {
                return true;
            }
            // auth status가 subject를 생략하는 버전은 이메일+조직의 일치만 연결합니다.
            let (Some(email), Some(org)) = (text(meta, "email"), text(meta, "orgId")) else {
                return false;
            };
            identity == format!("anthropic|email:{}|workspace:{org}", email.to_lowercase())
        }
        "codex" if provider_id(report["provider"].as_str().unwrap_or("")) == "openai" => {
            let (Some(email), Some(workspace)) = (text(meta, "email"), text(meta, "accountId"))
            else {
                return false;
            };
            identity
                == format!(
                    "openai|email:{}|workspace:{workspace}",
                    email.to_lowercase()
                )
        }
        _ => false,
    }
}

fn add_omp_reports(
    result: &mut ScanResult,
    value: &Value,
    binary: &Path,
    existing: &[Account],
) -> BTreeSet<String> {
    let mut observed = BTreeSet::new();
    let Some(reports) = value.get("reports").and_then(Value::as_array) else {
        result.notices.push(notice(
            "OMP_SCHEMA",
            "omp 사용량 응답 형식을 확인하지 못했어요. 기존 조회 시각은 바꾸지 않았어요.",
        ));
        return observed;
    };
    let linkable: Vec<_> = result
        .accounts
        .iter_mut()
        .map(|account| {
            let count = reports
                .iter()
                .filter(|report| matches_report(account, report))
                .take(2)
                .count();
            if count > 1 {
                account
                    .buckets
                    .retain(|bucket| !bucket.source.starts_with("OMP usage"));
            }
            count == 1
        })
        .collect();
    for report in reports {
        let Some(identity) = report_identity(report) else {
            result.notices.push(notice("OMP_IDENTITY_INCOMPLETE", "일부 omp 사용량은 계정 정보가 부족해 연결하지 않았어요."));
            continue;
        };
        let id = stable_id(&["omp", &identity, "observation"]);
        if !observed.insert(id.clone()) {
            if let (Some(account), Some(pin)) = (
                result.accounts.iter_mut().find(|account| account.id == id),
                quota::credential_pin(report),
            ) {
                if !account.omp_credential_pins.contains(&pin) {
                    account.omp_credential_pins.push(pin);
                }
            }
            continue;
        }
        let buckets = omp_buckets(report, &identity);
        let matching: Vec<_> = result
            .accounts
            .iter()
            .enumerate()
            .filter(|(index, a)| linkable.get(*index) == Some(&true) && matches_report(a, report))
            .map(|(i, _)| i)
            .collect();
        if !matching.is_empty() {
            // 동일 identity의 여러 native binding은 같은 관측 bucket ID를 공유합니다.
            for index in matching {
                if let Some(pin) = quota::credential_pin(report) {
                    if !result.accounts[index].omp_credential_pins.contains(&pin) {
                        result.accounts[index].omp_credential_pins.push(pin);
                    }
                }
                if result.accounts[index].tool != "codex"
                    || result.accounts[index].buckets.is_empty()
                {
                    result.accounts[index].buckets = buckets.clone();
                }
            }
            if !existing
                .iter()
                .any(|a| a.id == id && a.profile_path.is_some())
            {
                result.merged_observation_ids.push(id);
                continue;
            }
        }
        let provider = text(report, "provider").unwrap_or_else(|| "other".into());
        let meta = &report["metadata"];
        let old = existing.iter().find(|a| a.id == id);
        let mut account = Account {
            id,
            provider: provider_id(&provider).into(),
            tool: "omp".into(),
            label: account_label(&format!("OMP · {provider}"), text(meta, "email").as_deref()),
            email: text(meta, "email"),
            organization: text(meta, "orgName")
                .or_else(|| text(meta, "orgId"))
                .or_else(|| text(meta, "projectId")),
            plan: text(meta, "planType").or_else(|| text(meta, "subscriptionType")),
            binary_path: Some(binary.to_string_lossy().into_owned()),
            identity_key: Some(identity),
            auth_status: "unverified".into(),
            verification: "observed".into(),
            can_launch: false,
            reason: Some(OMP_GATE.into()),
            enabled: true,
            max_concurrency: 1,
            last_checked_at: buckets.iter().map(|b| b.observed_at).max().unwrap_or(0),
            omp_credential_pins: quota::credential_pin(report).into_iter().collect(),
            buckets,
            ..Account::default()
        };
        if let Some(old) = old {
            account = preserve_preferences(account, old);
            // 일시적인 실행본 부재·검사 실패로 관리 binding의 사용자 설정을 삭제하지 않습니다.
            if old.profile_path.is_some() {
                account.profile_path = old.profile_path.clone();
                account.binary_path = old.binary_path.clone();
            }
        }
        result.accounts.push(account);
    }
    if let Some(unavailable) = value.get("accountsWithoutUsage").and_then(Value::as_array) {
        for entry in unavailable {
            let Some(provider) = text(entry, "provider") else {
                continue;
            };
            let authorized = entry
                .get("authorizedAt")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let email = text(entry, "email");
            let id = stable_id(&[
                "omp-unverified",
                &provider,
                &authorized.to_string(),
                email.as_deref().unwrap_or(""),
            ]);
            let account = Account {
                id, provider: provider_id(&provider).into(), tool: "omp".into(), label: account_label(&format!("OMP · {provider}"), email.as_deref()), email,
                binary_path: Some(binary.to_string_lossy().into_owned()), auth_status: "unverified".into(), verification: "observed".into(),
                reason: Some("omp에 로그인 항목은 있지만 계정·한도 정보가 없어요. 사용량을 0으로 추정하거나 공식 CLI 로그인으로 복사하지 않아요.".into()),
                enabled: true, max_concurrency: 1, ..Account::default()
            };
            result.accounts.push(account);
        }
    }
    observed
}

/// 공식 CLI 자체 업데이트 명령입니다. 인증·프로필을 바꾸지 않으므로 계정을 배정하지 않습니다.
fn self_update(tool: &str, args: &[std::ffi::OsString]) -> bool {
    let first = args.first().and_then(|arg| arg.to_str());
    match tool {
        "claude" => matches!(first, Some("install" | "update" | "upgrade")),
        "codex" => first == Some("update"),
        _ => false,
    }
}

fn informational(tool: &str, args: &[std::ffi::OsString]) -> bool {
    let Some(arg) = args.first() else {
        return false;
    };
    args.len() == 1
        && (arg == "--help"
            || arg == "--version"
            || arg == "-h"
            || (tool == "claude" && arg == "-v")
            || (tool == "codex" && arg == "-V"))
}

fn auth_status(tool: &str, args: &[std::ffi::OsString]) -> bool {
    tool == "claude" && (args == ["auth", "status"] || args == ["auth", "status", "--json"])
}

/// 모델 세션을 시작하지 않고 인증을 바꾸지 않는 공식 CLI 관리 명령.
/// `writes_profile`이면 계정 프로필이 아니라 공식 CLI 기본 설정 폴더에서 실행한다.
fn utility_command(tool: &str, args: &[std::ffi::OsString]) -> Option<bool> {
    let first = args.first().and_then(|arg| arg.to_str())?;
    match (tool, first) {
        ("claude", "mcp" | "plugin" | "plugins") => Some(true),
        ("claude", "doctor" | "help") => Some(false),
        ("codex", "mcp" | "plugin" | "features") => Some(true),
        ("codex", "doctor" | "completion" | "help") => Some(false),
        _ => None,
    }
}

/// 설정 폴더를 쓰는 관리 명령을 어느 프로필에서 실행하는지 한 줄로 알린다.
/// 환경 변수 값은 넣지 않는다.
fn profile_notice_line(tool: &str) -> Option<&'static str> {
    match (tool, cfg!(unix)) {
        ("claude", true) => Some(
            "aam: 이 명령은 Ojak 계정을 고르지 않고 공식 CLI 기본 설정 폴더(~/.claude)에서 실행해요. 플러그인 폴더는 계정 대화와 연결돼요. 사용자 MCP는 복사되지 않아요.",
        ),
        ("claude", false) => Some(
            "aam: 이 명령은 Ojak 계정을 고르지 않고 공식 CLI 기본 설정 폴더에서 실행해요. 사용자 MCP와 플러그인 폴더는 계정 대화로 복사되지 않아요.",
        ),
        ("codex", true) => Some(
            "aam: 이 명령은 Ojak 계정을 고르지 않고 공식 CLI 기본 설정 폴더(~/.codex)에서 실행해요. MCP, 플러그인, 기능 설정은 계정 대화를 시작할 때 그 폴더에서 가져와요.",
        ),
        ("codex", false) => Some(
            "aam: 이 명령은 Ojak 계정을 고르지 않고 공식 CLI 기본 설정 폴더에서 실행해요. MCP, 플러그인, 기능 설정은 계정 대화를 시작할 때 그 폴더에서 가져와요.",
        ),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum DirectCli {
    Plain,
    Utility { writes_profile: bool },
}

/// 서비스 없이 원본 CLI로 넘길 명령. `None`이면 계정 배정 경로로 보낸다.
/// 인증 명령(`auth login`, `login`, `logout`, `setup-token`)은 넘기지 않는다.
fn direct_cli(tool: &str, args: &[std::ffi::OsString]) -> Option<DirectCli> {
    if informational(tool, args) || auth_status(tool, args) || self_update(tool, args) {
        return Some(DirectCli::Plain);
    }
    utility_command(tool, args).map(|writes_profile| DirectCli::Utility { writes_profile })
}

/// 설치 확인, 공식 자체 업데이트, 모델 세션을 시작하지 않는 관리 명령은 계정을 배정하지 않고 원본 CLI로 처리합니다.
/// `None`이면 이 명령이 아니므로 호출자가 서비스 경로로 보내야 합니다.
pub fn inspect_cli(
    paths: &Paths,
    tool: &str,
    args: &[std::ffi::OsString],
) -> Option<Result<std::process::ExitStatus, ApiError>> {
    let kind = direct_cli(tool, args)?;
    Some((|| {
        known_tool(tool)?;
        let binary = process::discover(paths, tool).ok_or_else(|| {
            ApiError::new(
                "CLI_NOT_FOUND",
                "Ojak 명령이 아닌 공식 CLI 원본을 찾지 못했어요.",
            )
        })?;
        if let DirectCli::Utility { writes_profile: true } = kind {
            if let Some(line) = profile_notice_line(tool) {
                eprintln!("{line}");
            }
        }
        let mut command = std::process::Command::new(binary);
        command.args(args);
        // 상속된 프로필 변수가 한 Ojak 계정 폴더를 가리키면 그 계정만 조용히 바뀐다.
        // 관리 명령은 항상 공식 CLI 기본 폴더를 쓰게 변수를 지운다. 값은 출력하지 않는다.
        if let DirectCli::Utility { writes_profile: true } = kind {
            if let Some(var) = safety::profile_var(tool) {
                command.env_remove(var);
            }
        }
        command.status().map_err(|_| {
            ApiError::new(
                "CLI_INSPECTION_FAILED",
                "공식 CLI 조회 명령을 실행하지 못했어요.",
            )
        })
    })())
}

pub fn scan(paths: &Paths, existing: &[Account]) -> Result<ScanResult, ApiError> {
    let home = user_home()?;
    let mut result = ScanResult {
        accounts: Vec::new(),
        tools: Vec::new(),
        notices: Vec::new(),
        merged_observation_ids: Vec::new(),
    };
    let mut binaries = BTreeMap::new();
    for tool in ["claude", "codex", "omp"] {
        let binary = process::discover(paths, tool);
        let (name, provider, isolation, reason) = tool_details(tool);
        let version = binary
            .as_ref()
            .and_then(|binary| process::version(binary, &home));
        result.tools.push(ToolStatus {
            id: tool.into(),
            name: name.into(),
            provider: provider.into(),
            binary_path: binary.as_ref().map(|p| p.to_string_lossy().into_owned()),
            version,
            installed: binary.is_some(),
            isolation: isolation.into(),
            reason: reason.map(str::to_owned),
        });
        if let Some(binary) = binary {
            binaries.insert(tool, binary);
        }
    }
    // 실행 어댑터가 없는 도구(grok·agy)의 남은 행은 서비스가 정리하므로 다시 조회하지 않습니다.
    for old in existing
        .iter()
        .filter(|a| matches!(a.tool.as_str(), "claude" | "codex"))
    {
        result.accounts.push(refresh_bound(old, binaries.get(old.tool.as_str()).map(PathBuf::as_path)));
    }
    for (tool, folder) in [("claude", ".claude"), ("codex", ".codex")] {
        let Some(binary) = binaries.get(tool) else {
            continue;
        };
        let profile = home.join(folder);
        let Ok(profile) = safety::canonical_profile(&profile) else {
            continue;
        };
        if result
            .accounts
            .iter()
            .any(|a| a.tool == tool && a.profile_path.as_deref() == profile.to_str())
        {
            continue;
        }
        let label = format!("{} · 기본 프로필", tool_details(tool).0);
        let mut account = inspect(tool, &profile, binary, &label)
            .unwrap_or_else(|error| disabled(blank_account(tool, &profile, binary, &label), error));
        // 자동 발견한 기본 프로필도 확인된 이메일로 구분합니다. 사용자가 지정한 라벨은 바꾸지 않습니다.
        account.label = account_label(&label, account.email.as_deref());
        result.accounts.push(account);
    }
    let mut observed_omp = BTreeSet::new();
    if let Some(binary) = binaries.get("omp") {
        match process::run_json_slow(binary, &["usage", "--json"], &process::base_env(), &home) {
            Ok(value) => {
                observed_omp = add_omp_reports(&mut result, &value, binary, existing);
            }
            Err(error) => result
                .notices
                .push(notice("OMP_USAGE_UNAVAILABLE", error.message)),
        }
    }
    let current: BTreeSet<_> = result.accounts.iter().map(|a| a.id.clone()).collect();
    for old in existing
        .iter()
        .filter(|a| a.tool == "omp" && !current.contains(&a.id) && !observed_omp.contains(&a.id))
    {
        // 조회 실패와 identity 소멸을 구별할 수 없으므로 이전 관측을 stale로 남깁니다.
        let mut stale = old.clone();
        stale.can_launch = false;
        stale.verification = "observed".into();
        stale.auth_status = "unverified".into();
        stale.reason = Some("지금 omp 응답에서 이 계정을 다시 보지 못했어요. 이전 조회이며 공식 CLI 실행에 쓸 수 없어요.".into());
        for bucket in &mut stale.buckets {
            bucket.status = if bucket.used_percent.is_some() {
                "stale"
            } else {
                "unknown"
            }
            .into();
        }
        result.accounts.push(stale);
    }
    Ok(result)
}

fn binding_paths(account: &Account) -> Result<(PathBuf, PathBuf), ApiError> {
    let profile = account.profile_path.as_deref().ok_or_else(|| ApiError::new("PROFILE_UNBOUND", "이 계정에 공식 프로필이 연결돼 있지 않아요. 공식 CLI에 로그인한 뒤 프로필을 등록해 주세요."))?;
    let canonical = safety::canonical_profile(Path::new(profile))?;
    if canonical != Path::new(profile) {
        return Err(ApiError::new("PROFILE_BINDING_CHANGED", "등록 이후 프로필 경로가 바뀌었어요. 바로가기로 계정을 바꾸지 않아요. 다시 등록해 주세요."));
    }
    let binary = account
        .binary_path
        .as_deref()
        .ok_or_else(|| ApiError::new("BINARY_MISSING", "등록된 공식 CLI가 없어요."))?;
    // 진입 경로(symlink)는 공식 업데이트로 대상이 바뀔 수 있으므로 매번 해석한 실행 파일을 씁니다.
    let executable = process::executable(Path::new(binary))?;
    Ok((canonical, executable))
}

pub fn verify(account: &Account) -> Result<IdentityEvidence, ApiError> {
    verify_at(account, None)
}

fn verify_at(account: &Account, cwd: Option<&Path>) -> Result<IdentityEvidence, ApiError> {
    known_tool(&account.tool)?;
    let (profile, binary) = binding_paths(account)?;
    let fresh = native::inspect_at(
        &account.tool,
        &profile,
        &binary,
        &account.label,
        cwd.unwrap_or(&profile),
    )?;
    if fresh.auth_status != "authenticated" {
        return Err(ApiError::new(
            "AUTH_REQUIRED",
            "선택한 프로필에서 공식 로그인이 필요해요.",
        ));
    }
    let expected = account.identity_key.as_ref().ok_or_else(|| {
        ApiError::new(
            "IDENTITY_UNVERIFIED",
            "등록 계정의 안정적인 identity를 확인하지 못했습니다. 공식 로그인 후 재등록해 주세요.",
        )
    })?;
    if fresh.identity_key.as_ref() != Some(expected) || !fresh.can_launch {
        return Err(ApiError::new("IDENTITY_DRIFT", "실제 계정이 등록된 계정과 같지 않아 실행을 막았어요. 프로필을 확인하고 다시 등록해 주세요."));
    }
    Ok(IdentityEvidence {
        identity_key: expected.clone(),
        tier: "preflight-verified".into(),
        observed_at: now_ms(),
    })
}
/// 공식 CLI가 공유 native 인증을 관리합니다. 자체 토큰 복제나 갱신은 하지 않습니다.
/// Claude의 여러 터미널 세션 지원: https://code.claude.com/docs/en/worktrees
pub fn supports_shared_profile_concurrency(account: &Account) -> bool {
    account.tool == "claude"
        && account.can_launch
        && account.verification == "preflight-verified"
}

pub fn build_launch_plan(account: &Account, intent: &LaunchIntent) -> Result<LaunchPlan, ApiError> {
    if intent.tool != account.tool
        || intent
            .account_id
            .as_ref()
            .is_some_and(|id| id != &account.id)
    {
        return Err(ApiError::new(
            "ACCOUNT_MISMATCH",
            "요청한 도구 또는 계정이 선택한 binding과 일치하지 않습니다.",
        ));
    }
    if !account.enabled {
        return Err(ApiError::new(
            "ACCOUNT_DISABLED",
            "이 계정은 새 실행에서 제외되어 있습니다.",
        ));
    }
    if intent.resume_session_id.is_some() && !matches!(account.tool.as_str(), "claude" | "codex") {
        return Err(ApiError::new(
            "RESUME_UNVERIFIED",
            "이 도구는 검증된 native 대화 매핑을 아직 제공하지 않습니다.",
        ));
    }
    if matches!(account.tool.as_str(), "claude" | "codex")
        && intent
            .resume_session_id
            .as_ref()
            .is_some_and(|id| uuid::Uuid::parse_str(id).is_err())
    {
        return Err(ApiError::new(
            "SESSION_MAPPING_INVALID",
            "native 대화 ID가 유효한 UUID가 아닙니다.",
        ));
    }
    if !intent
        .model
        .starts_with(|c: char| c.is_ascii_alphanumeric())
        || intent.model.len() > 160
        || !intent.model.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '[' | ']')
        })
    {
        return Err(ApiError::new("MODEL_REQUIRED", "공식 CLI의 정확한 모델 ID를 적어 주세요. 모델이나 결제 경로를 자동으로 바꾸지 않아요."));
    }
    let cwd = safety::canonical_profile(Path::new(&intent.cwd)).map_err(|_| {
        ApiError::new(
            "PROJECT_INVALID",
            "작업 디렉터리가 없거나 접근할 수 없습니다. 기존 폴더의 절대 경로를 선택해 주세요.",
        )
    })?;
    let (profile, binary) = binding_paths(account)?;
    safety::check_inherited_profile(&account.tool, &profile)?;
    verify_at(account, Some(&cwd))?;
    let mut args = if intent.model == NATIVE_DEFAULT_MODEL {
        Vec::new()
    } else {
        vec!["--model".into(), intent.model.clone()]
    };
    if let Some(native_id) = &intent.resume_session_id {
        if account.tool == "codex" {
            // `codex resume <id>`는 하위 명령이라 모델 옵션보다 앞에 둔다.
            args.splice(0..0, ["resume".to_owned(), native_id.clone()]);
        } else {
            args.extend(["--resume".into(), native_id.clone()]);
        }
    }
    Ok(LaunchPlan {
        program: binary.to_string_lossy().into_owned(),
        args,
        env: profile_env(&account.tool, &profile),
        account: account.clone(),
    })
}

pub fn register(
    paths: &Paths,
    tool: &str,
    label: &str,
    profile_path: &str,
) -> Result<Account, ApiError> {
    known_tool(tool)?;
    validate_label(label)?;
    let profile = safety::canonical_profile(Path::new(profile_path))?;
    let binary = process::discover(paths, tool).ok_or_else(|| {
        ApiError::new(
            "BINARY_MISSING",
            "공식 CLI가 설치되어 있지 않습니다. 먼저 공식 CLI를 설치해 주세요.",
        )
    })?;
    let account = inspect(tool, &profile, &binary, label)?;
    if account.auth_status == "auth-required" {
        return Err(ApiError::new(
            "AUTH_REQUIRED",
            "이 프로필에서 공식 로그인을 완료한 뒤 다시 등록해 주세요.",
        ));
    }
    if account.identity_key.is_none() {
        return Err(ApiError::new(
            "IDENTITY_UNVERIFIED",
            account
                .reason
                .as_deref()
                .unwrap_or("공식 identity 정보를 확인하지 못했습니다."),
        ));
    }
    Ok(account)
}

fn validate_label(label: &str) -> Result<(), ApiError> {
    if label.trim().is_empty() || label.len() > 120 || label.chars().any(char::is_control) {
        return Err(ApiError::new(
            "LABEL_INVALID",
            "계정 이름은 제어 문자가 없는 1~120바이트 문자열로 입력해 주세요.",
        ));
    }
    Ok(())
}

pub fn enrollment_plan(paths: &Paths, tool: &str, label: &str) -> Result<EnrollmentPlan, ApiError> {
    known_tool(tool)?;
    validate_label(label)?;
    safety::check_env(tool)?;
    let binary = process::discover(paths, tool).ok_or_else(|| {
        ApiError::new(
            "BINARY_MISSING",
            "공식 CLI를 설치한 뒤 다시 로그인해 주세요.",
        )
    })?;
    let args = login_args(tool)?;
    // 프로필에는 자격 증명이 평문으로 들어간다(Windows Claude 실측). 현재 사용자 전용으로 만든다.
    aam_protocol::secure::restrict_dir(&paths.profiles)
        .map_err(|_| {
            ApiError::new(
                "PROFILE_CREATE_FAILED",
                "앱 전용 프로필 경로를 만들지 못했습니다. 앱 데이터 폴더 권한을 확인해 주세요.",
            )
        })?;
    let parent = paths.profiles.canonicalize().map_err(|_| {
        ApiError::new(
            "PROFILE_CREATE_FAILED",
            "앱 전용 프로필 경로를 확인하지 못했습니다.",
        )
    })?;
    let profile = parent.join(format!("{tool}-{}", new_id()));
    fs::create_dir(&profile)
        .and_then(|_| aam_protocol::secure::restrict_dir(&profile))
        .map_err(|_| {
            ApiError::new(
                "PROFILE_CREATE_FAILED",
                "새 계정의 독립 프로필 디렉터리를 만들지 못했습니다.",
            )
        })?;
    // 로그인은 호출자가 사용자 터미널에서 수행합니다. 기존 설정이나 credential은 복사하지 않습니다.
    Ok(EnrollmentPlan {
        program: binary.to_string_lossy().into_owned(),
        args,
        env: profile_env(tool, &profile),
        profile_path: profile.to_string_lossy().into_owned(),
        tool: tool.into(),
        label: label.into(),
    })
}

fn login_args(tool: &str) -> Result<Vec<String>, ApiError> {
    match tool {
        "claude" => Ok(vec!["auth".into(), "login".into()]),
        "codex" => Ok(vec!["login".into()]),
        _ => Err(ApiError::new(
            "ENROLLMENT_UNSUPPORTED",
            "이 도구의 독립 프로필 로그인을 지원하지 않습니다.",
        )),
    }
}

/// 이미 등록된 프로필에서 공식 로그인을 다시 연다.
/// 새 디렉터리를 만들지 않고, 기존 설정·자격 증명을 복사하지 않는다.
pub fn relogin_plan(
    paths: &Paths,
    tool: &str,
    label: &str,
    profile: &Path,
) -> Result<EnrollmentPlan, ApiError> {
    known_tool(tool)?;
    // 이미 저장된 이름이다. 새 계정용 120바이트 제한으로 다시 로그인을 막지 않는다.
    if label.trim().is_empty() || label.chars().any(char::is_control) || label.chars().count() > 120 {
        return Err(ApiError::new(
            "LABEL_INVALID",
            "다시 로그인할 계정 이름을 확인하지 못했습니다.",
        ));
    }
    safety::check_env(tool)?;
    let binary = process::discover(paths, tool).ok_or_else(|| {
        ApiError::new(
            "BINARY_MISSING",
            "공식 CLI를 설치한 뒤 다시 로그인해 주세요.",
        )
    })?;
    let profile = safety::canonical_profile(profile)?;
    Ok(EnrollmentPlan {
        program: binary.to_string_lossy().into_owned(),
        args: login_args(tool)?,
        env: profile_env(tool, &profile),
        profile_path: profile.to_string_lossy().into_owned(),
        tool: tool.into(),
        label: label.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn report(subject: &str, workspace: &str) -> Value {
        json!({"provider":"anthropic","fetchedAt":now_ms(),"metadata":{"accountId":subject,"orgId":workspace,"email":"person@invalid.test"},"limits":[{"id":"anthropic:5h","amount":{"usedFraction":0.43},"status":"ok"}]})
    }

    fn native_account(profile: &str) -> Account {
        let mut account = blank_account(
            "claude",
            Path::new(profile),
            Path::new("/native/claude"),
            "계정",
        );
        account.identity_key =
            Some("anthropic|email:person@invalid.test|workspace:workspace-a".into());
        account
    }

    #[test]
    fn only_official_self_update_commands_bypass_assignment() {
        let args = |values: &[&str]| values.iter().map(Into::into).collect::<Vec<std::ffi::OsString>>();
        for values in [&["install"][..], &["install", "latest", "--force"], &["update"], &["upgrade"]] {
            assert!(self_update("claude", &args(values)), "{values:?}");
        }
        assert!(self_update("codex", &args(&["update"])));
        // 인증·설정 명령이나 인수 위치의 같은 단어, 다른 도구의 이름은 통과시키지 않습니다.
        for (tool, values) in [
            ("claude", &["setup-token"][..]),
            ("claude", &["-p", "update"]),
            ("codex", &["install"]),
            ("codex", &["upgrade"]),
            ("omp", &["update"]),
        ] {
            assert!(!self_update(tool, &args(values)), "{tool} {values:?}");
        }
    }

    #[test]
    fn utility_commands_pass_through_but_auth_commands_do_not() {
        let args = |values: &[&str]| values.iter().map(Into::into).collect::<Vec<std::ffi::OsString>>();
        assert!(matches!(
            direct_cli("claude", &args(&["mcp", "list"])),
            Some(DirectCli::Utility { writes_profile: true })
        ));
        assert!(matches!(
            direct_cli("claude", &args(&["plugin", "install", "name"])),
            Some(DirectCli::Utility { writes_profile: true })
        ));
        assert!(matches!(
            direct_cli("claude", &args(&["doctor"])),
            Some(DirectCli::Utility { writes_profile: false })
        ));
        assert!(matches!(
            direct_cli("codex", &args(&["mcp", "list"])),
            Some(DirectCli::Utility { writes_profile: true })
        ));
        assert!(matches!(
            direct_cli("codex", &args(&["completion", "zsh"])),
            Some(DirectCli::Utility { writes_profile: false })
        ));
        assert!(direct_cli("claude", &args(&["auth", "status"])).is_some());
        assert!(direct_cli("claude", &args(&["auth", "login"])).is_none());
        assert!(direct_cli("claude", &args(&["setup-token"])).is_none());
        assert!(direct_cli("claude", &args(&["logout"])).is_none());
        assert!(direct_cli("codex", &args(&["login"])).is_none());
        assert!(direct_cli("codex", &args(&["logout"])).is_none());
        assert!(direct_cli("claude", &args(&["-p", "mcp list"])).is_none());
        assert!(direct_cli("codex", &args(&["resume"])).is_none());
        let notice = profile_notice_line("claude").unwrap();
        assert!(notice.contains("기본 설정 폴더"), "{notice}");
        assert!(!notice.contains("super-secret-value"));
        assert!(profile_notice_line("codex").unwrap().contains("기본 설정 폴더"));
    }


    /// Codex 대화 ID는 이 실행이 쓰기로 연 rollout에서만 찾는다. 같은 폴더의 다른 대화(외부 Codex)나
    /// 프로필 밖 파일은 근거가 아니며, 후보가 둘 이상이면 매핑하지 않는다.
    #[cfg(unix)]
    #[test]
    fn codex_session_comes_only_from_rollouts_this_run_wrote() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("aam-codex-map-{}", uuid::Uuid::new_v4()));
        let profile = root.join("profile");
        let day = profile.join("sessions/2026/09/29");
        let work = root.join("work");
        let other = root.join("other");
        for dir in [&day, &work, &other] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o700)).unwrap();
        let profile = profile.canonicalize().unwrap();
        let rollout = |name: &str, id: &str, cwd: &Path| {
            let path = day.join(format!("rollout-{name}-{id}.jsonl"));
            let meta = json!({"type":"session_meta","payload":{"id":id,"cwd":cwd}});
            fs::write(&path, format!("{meta}\n")).unwrap();
            path
        };
        let mine = rollout("a", "01a0ed71-c17c-7582-b494-8870b3aba085", &work);
        let external = rollout("b", "01a0ed71-c17c-7582-b494-8870b3aba086", &work);
        let elsewhere = rollout("c", "01a0ed71-c17c-7582-b494-8870b3aba087", &other);
        let outside = root.join("rollout-d-01a0ed71-c17c-7582-b494-8870b3aba088.jsonl");
        fs::copy(&mine, &outside).unwrap();
        let mut account = blank_account("codex", &profile, Path::new("/usr/bin/true"), "계정");
        account.binary_path = Some("/usr/bin/true".into());
        let cwd = work.to_str().unwrap();
        let from = |written: &[&PathBuf]| {
            codex_session_from(&account, cwd, &written.iter().map(|p| (*p).clone()).collect::<Vec<_>>())
        };
        assert_eq!(from(&[&mine, &elsewhere, &outside]).as_deref(), Some("01a0ed71-c17c-7582-b494-8870b3aba085"));
        assert_eq!(from(&[&mine, &external]), None);
        assert_eq!(from(&[&elsewhere, &outside]), None);
        assert_eq!(from(&[]), None);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn native_profiles_share_observation_without_duplicate_observer() {
        let mut result = ScanResult {
            accounts: vec![native_account("/profile/a"), native_account("/profile/b")],
            tools: vec![],
            notices: vec![],
            merged_observation_ids: vec![],
        };
        let observed = add_omp_reports(
            &mut result,
            &json!({"reports":[report("subject-a","workspace-a")]}),
            Path::new("/native/omp"),
            &[],
        );
        assert_eq!(result.accounts.len(), 2);
        assert_eq!(
            result.accounts[0].buckets[0].id,
            result.accounts[1].buckets[0].id
        );
        assert!(observed.contains(&stable_id(&[
            "omp",
            "anthropic|subject:subject-a|workspace:workspace-a",
            "observation"
        ])));
        let pin = quota::credential_pin(&report("subject-a", "workspace-a")).unwrap();
        assert!(result
            .accounts
            .iter()
            .all(|account| account.omp_credential_pins.contains(&pin)));
    }

    #[test]
    fn equal_percentages_and_ambiguous_emails_do_not_merge_subjects() {
        let mut result = ScanResult {
            accounts: vec![native_account("/profile/a")],
            tools: vec![],
            notices: vec![],
            merged_observation_ids: vec![],
        };
        add_omp_reports(
            &mut result,
            &json!({"reports":[report("subject-a","workspace-a"),report("subject-b","workspace-a")]}),
            Path::new("/native/omp"),
            &[],
        );
        assert!(result.accounts[0].buckets.is_empty());
        assert_eq!(result.accounts.len(), 3);
        assert_ne!(
            result.accounts[1].buckets[0].id,
            result.accounts[2].buckets[0].id
        );
    }
}

/// 다른 계정에서 이어 가기: 원래 계정 프로필의 Claude 대화 기록 한 개(`projects/<폴더>/<native>.jsonl`)를
/// 대상 계정 프로필의 같은 자리로 복사한다. 로그인 정보·설정은 건드리지 않고 대화 기록만 옮긴다.
/// 원본을 정확히 하나 찾지 못하면 추측하지 않고 멈춘다. 대상 파일은 원자적으로 교체하고 사용자 전용으로 만든다.
pub fn copy_claude_transcript(source: &Account, target: &Account, native_id: &str) -> Result<(), ApiError> {
    let missing = || ApiError::new("CONTINUE_TRANSCRIPT_MISSING", "원래 계정에서 이 대화 기록을 찾지 못했습니다. 다른 계정으로 옮기지 않았습니다.");
    if source.tool != "claude" || target.tool != "claude" || uuid::Uuid::parse_str(native_id).is_err() {
        return Err(missing());
    }
    let (from_profile, _) = binding_paths(source)?;
    let (to_profile, _) = binding_paths(target)?;
    let name = format!("{native_id}.jsonl");
    let mut found = Vec::new();
    for entry in fs::read_dir(from_profile.join("projects")).map_err(|_| missing())?.flatten().take(8192) {
        let candidate = entry.path().join(&name);
        if fs::symlink_metadata(&candidate).is_ok_and(|meta| meta.is_file()) {
            found.push((entry.file_name(), candidate));
        }
    }
    let [(folder, from)] = found.as_slice() else {
        return Err(missing());
    };
    copy_private(from, &to_profile.join("projects").join(folder).join(&name))
}

/// Codex `CODEX_HOME/sessions/<년>/<월>/<일>/rollout-*.jsonl` 파일 목록. 최근 날짜 폴더 몇 개만 본다.
fn codex_rollouts(profile: &Path, days: usize) -> Vec<PathBuf> {
    let mut day_dirs = Vec::new();
    let list = |dir: &Path| -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
        entries.sort();
        entries
    };
    for year in list(&profile.join("sessions")).into_iter().rev().take(2) {
        for month in list(&year).into_iter().rev().take(2) {
            day_dirs.extend(list(&month));
        }
    }
    day_dirs.sort();
    day_dirs
        .into_iter()
        .rev()
        .take(days)
        .flat_map(|day| fs::read_dir(day).into_iter().flatten().flatten().map(|e| e.path()).take(4096).collect::<Vec<_>>())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl") && path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-")))
        .collect()
}

/// rollout 첫 줄(`session_meta`)의 대화 ID와 작업 폴더.
fn codex_meta(path: &Path) -> Option<(String, PathBuf)> {
    use std::io::BufRead;
    let file = aam_protocol::secure::open_read_no_follow(path).ok()?;
    let mut line = String::new();
    std::io::BufReader::new(std::io::Read::take(file, 4 * 1024 * 1024)).read_line(&mut line).ok()?;
    let value: Value = serde_json::from_str(&line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?;
    let id = uuid::Uuid::parse_str(payload.get("id")?.as_str()?).ok()?.to_string();
    let cwd = Path::new(payload.get("cwd")?.as_str()?).canonicalize().ok()?;
    Some((id, cwd))
}

/// 관리 Codex 실행의 native 대화 ID. `written`은 실행기가 실행 중 이 실행의 프로세스 트리가 쓰기 모드로 열고 있던
/// 파일 경로다. 그중 이 계정 프로필의 `sessions/` 안 rollout이고 작업 폴더가 같은 대화가 정확히 하나일 때만 돌려준다.
/// 시각·폴더만 맞는 다른 파일(같은 프로필을 쓰는 외부 Codex)은 근거가 아니므로 쓰지 않는다.
pub fn codex_session_from(account: &Account, cwd: &str, written: &[PathBuf]) -> Option<String> {
    let (profile, _) = binding_paths(account).ok()?;
    let sessions = profile.join("sessions").canonicalize().ok()?;
    let cwd = Path::new(cwd).canonicalize().ok()?;
    let mut found: Vec<String> = written
        .iter()
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| {
            path.starts_with(&sessions)
                && path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"))
        })
        .filter_map(|path| codex_meta(&path).filter(|(_, dir)| dir == &cwd).map(|(id, _)| id))
        .collect();
    found.sort();
    found.dedup();
    match found.as_slice() {
        [id] => Some(id.clone()),
        _ => None,
    }
}

/// 격리 프로필에 기본 설정 폴더(`~/.claude`·`~/.codex`)의 스킬·에이전트·지침·플러그인·hooks와 확장 설정 키를 연결한다.
/// 기본 폴더를 그대로 쓰는 계정은 건드리지 않는다. `env`·인증 키·로그인 파일은 공유하지 않는다.
pub fn share_extensions(account: &Account) -> Result<(), ApiError> {
    let (spec, folder) = match account.tool.as_str() {
        "claude" => (&shared::CLAUDE, ".claude"),
        "codex" => (&shared::CODEX, ".codex"),
        _ => return Ok(()),
    };
    let (profile, _) = binding_paths(account)?;
    shared::share(spec, &user_home()?.join(folder), &profile)
}

/// 다른 계정에서 이어 가기: 도구별 대화 기록만 대상 계정 프로필로 옮긴다.
pub fn copy_conversation(source: &Account, target: &Account, native_id: &str) -> Result<(), ApiError> {
    match source.tool.as_str() {
        "claude" => copy_claude_transcript(source, target, native_id),
        "codex" => copy_codex_rollout(source, target, native_id),
        _ => Err(ApiError::new("CONTINUE_UNSUPPORTED", "이 도구는 다른 계정에서 이어 가기를 지원하지 않습니다.")),
    }
}

/// Codex rollout(`sessions/<년>/<월>/<일>/rollout-...-<id>.jsonl`)을 같은 상대 경로로 복사한다.
/// 같은 대화 ID의 파일이 여러 개(재개 기록)면 모두 옮긴다. 로그인(`auth.json`)·설정은 건드리지 않는다.
fn copy_codex_rollout(source: &Account, target: &Account, native_id: &str) -> Result<(), ApiError> {
    let missing = || ApiError::new("CONTINUE_TRANSCRIPT_MISSING", "원래 계정에서 이 대화 기록을 찾지 못했습니다. 다른 계정으로 옮기지 않았습니다.");
    if target.tool != "codex" || uuid::Uuid::parse_str(native_id).is_err() {
        return Err(missing());
    }
    let (from_profile, _) = binding_paths(source)?;
    let (to_profile, _) = binding_paths(target)?;
    let suffix = format!("-{native_id}.jsonl");
    let files: Vec<PathBuf> = codex_rollouts(&from_profile, 62)
        .into_iter()
        .filter(|path| path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(&suffix)))
        .take(16)
        .collect();
    if files.is_empty() {
        return Err(missing());
    }
    for from in files {
        let relative = from.strip_prefix(&from_profile).map_err(|_| missing())?;
        copy_private(&from, &to_profile.join(relative))?;
    }
    Ok(())
}

/// 파일 하나를 링크를 따라가지 않고 읽어, 대상 폴더에 임시 파일로 쓴 뒤 원자적으로 바꾼다(사용자 전용).
fn copy_private(from: &Path, destination: &Path) -> Result<(), ApiError> {
    let failed = || ApiError::new("CONTINUE_TRANSCRIPT_COPY", "대화 기록을 다른 계정으로 복사하지 못했습니다. 원본은 그대로입니다.");
    let mut input = aam_protocol::secure::open_read_no_follow(from).map_err(|_| failed())?;
    let dir = destination.parent().ok_or_else(failed)?;
    fs::create_dir_all(dir).map_err(|_| failed())?;
    if fs::symlink_metadata(destination).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(failed());
    }
    let name = destination.file_name().and_then(|n| n.to_str()).ok_or_else(failed)?;
    let temporary = dir.join(format!(".{name}.{}.tmp", aam_protocol::new_id()));
    let result = (|| {
        let mut output = aam_protocol::secure::private_options(fs::OpenOptions::new().write(true).create_new(true)).open(&temporary)?;
        std::io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        drop(output);
        // 승격 실행에서도 소유자를 현재 사용자로 남겨 takeover의 소유권 검사를 통과하게 한다.
        #[cfg(windows)]
        aam_protocol::secure::restrict_file(&temporary)?;
        fs::rename(&temporary, destination)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err(failed());
    }
    Ok(())
}
