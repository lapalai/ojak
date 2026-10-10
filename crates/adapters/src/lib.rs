mod access;
pub use access::{native_access, NativeAccess};
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
    QuotaBucket, ToolStatus, NATIVE_DEFAULT_MODEL,
};
use native::{blank_account, inspect, profile_env, OMP_GATE};
use quota::{
    claude_usage_buckets, omp_buckets, omp_extra_usage, provider_id, report_identity, stable_id, text,
};
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
        if fresh.auth_status == "auth-required" {
            return Err(ApiError::new("AUTH_REQUIRED", fresh.reason.unwrap_or_else(|| "공식 CLI에서 로그인한 뒤 다시 확인해 주세요.".into())));
        }
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
        // 추가 사용량은 omp 조회에서만 오므로 이 점검이 지우지 않는다. 관측 시각이 오래되면 쓰지 않는다.
        fresh.extra_usage = old.extra_usage.clone();
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

/// 이 Claude 계정의 한도가 이번 스캔의 omp 보고로 이미 신선하게 채워졌는지. 그렇다면 공식 조회를 또 하지 않는다.
fn omp_fresh(account: &Account) -> bool {
    let now = now_ms();
    !account.buckets.is_empty()
        && account.buckets.iter().all(|bucket| {
            bucket.source.starts_with("OMP usage")
                && matches!(bucket.status.as_str(), "known" | "exhausted")
                && bucket.observed_at > 0
                && bucket.observed_at <= now.saturating_add(30_000)
                && now.saturating_sub(bucket.observed_at) <= 900_000
                && bucket.resets_at.is_none_or(|reset| reset > now)
        })
}

/// 정기 조회에서 공식 Claude `/usage`를 다시 부를 때인지. 계정마다 CLI 프로세스를 띄우므로(실측 4–5초)
/// 마지막 공식 관측이 아직 신선하고 리셋 시각이 지나지 않았으면 다시 묻지 않는다.
/// 리셋이 지난 한도는 회복을 바로 확인하도록 즉시 다시 묻는다.
const NATIVE_USAGE_EVERY_MS: i64 = 10 * 60_000;
fn native_due(account: &Account) -> bool {
    let now = now_ms();
    let official: Vec<_> = account.buckets.iter().filter(|bucket| bucket.source == quota::CLAUDE_USAGE_SOURCE).collect();
    official.is_empty()
        || official.iter().any(|bucket| {
            bucket.observed_at <= 0
                || now.saturating_sub(bucket.observed_at) >= NATIVE_USAGE_EVERY_MS
                || bucket.resets_at.is_some_and(|reset| reset <= now)
        })
}

/// Ojak에 연결된 공식 Claude 로그인으로 직접 한도를 조회한다. omp 로그인이 없어도 동작한다.
/// 공식 `claude -p /usage`(추론·도구·훅 없음, 공식 CLI가 자체 잠금으로 OAuth를 갱신)의 구조화 응답만 쓰며
/// 토큰은 읽지도 복사하지도 않는다. 조회 전에 이미 identity가 확인된 계정만 대상이고, 조회 뒤 같은 identity인지
/// 다시 확인한다. 응답이 구조화된 한도를 주지 않으면 오류이며 기존 관측은 그대로 오래된 값으로 남는다.
/// 버킷 ID는 omp 관측과 같은 한도라면 그 ID를 이어 써서 같은 한도가 중복·충돌하지 않게 한다.
fn native_usage(account: &Account, existing: &[Account]) -> Result<Vec<QuotaBucket>, ApiError> {
    let identity = account
        .identity_key
        .as_deref()
        .filter(|_| account.tool == "claude" && account.auth_status == "authenticated" && account.can_launch)
        .ok_or_else(|| ApiError::new("AUTH_REQUIRED", "사용할 수 있는 공식 Claude 로그인이 없어요."))?;
    let (profile, executable) = binding_paths(account)?;
    let events = process::run_ndjson(
        &executable,
        &[
            "-p",
            "/usage",
            "--safe-mode",
            "--tools",
            "",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--output-format",
            "stream-json",
            "--verbose",
        ],
        &profile_env("claude", &profile),
        &profile,
    )?;
    let observed_at = now_ms();
    // omp 관측과 같은 한도는 omp와 같은 버킷 ID를 쓴다(요약이 ID 기준으로 최신 관측만 남긴다). 같은 이메일·워크스페이스의
    // omp identity가 정확히 하나일 때만 그 identity를 쓰고, 없으면 공식 identity로 같은 규칙의 ID를 만든다.
    let workspace = identity.rsplit("|workspace:").next().unwrap_or("");
    let mut omp_identities = existing
        .iter()
        .filter(|row| {
            row.tool == "omp"
                && row.provider == "anthropic"
                && row.email.as_deref().zip(account.email.as_deref())
                    .is_some_and(|(observed, native)| observed.eq_ignore_ascii_case(native))
        })
        .filter_map(|row| row.identity_key.as_deref())
        .filter(|key| key.starts_with("anthropic|subject:") && key.rsplit("|workspace:").next() == Some(workspace));
    let bucket_identity = omp_identities.next()
        .filter(|candidate| omp_identities.all(|other| other == *candidate))
        .unwrap_or(identity);
    let buckets = claude_usage_buckets(&events, observed_at, |upstream| {
        stable_id(&["omp", "anthropic", bucket_identity, upstream])
    });
    if buckets.is_empty() {
        return Err(ApiError::new(
            "NATIVE_USAGE_UNAVAILABLE",
            "공식 Claude가 구조화된 사용량을 주지 않았어요. 이전 조회는 오래된 값으로만 남겨요.",
        ));
    }
    let after = inspect(&account.tool, &profile, &executable, &account.label)?;
    if after.auth_status != "authenticated" || after.identity_key.as_deref() != Some(identity) {
        return Err(ApiError::new(
            "IDENTITY_DRIFT",
            "사용량을 조회하는 사이 공식 Claude 계정이 바뀌었어요. 조회 결과를 쓰지 않았어요.",
        ));
    }
    Ok(buckets)
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
        let extra_usage = omp_extra_usage(report);
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
                // 추가 사용량은 Claude 계정에만 붙인다. 이번 보고에 없으면 켜졌다고 보지 않고 비운다.
                if result.accounts[index].tool == "claude" {
                    result.accounts[index].extra_usage = extra_usage.clone();
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
            extra_usage,
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

/// 명시적으로 무효화된 omp OAuth만 재로그인으로 분류한다. 조회 누락·네트워크 오류는 근거가 아니다.
fn apply_omp_auth_failures(accounts: &mut [Account], usage: &Value) {
    let Some(disabled) = usage.get("disabledCredentials").and_then(Value::as_array) else { return };
    for account in accounts.iter_mut().filter(|account| account.tool == "omp") {
        let Some(identity) = account.identity_key.as_deref() else { continue };
        // 같은 identity의 새 로그인이 있으면 과거의 무효화된 credential 때문에 막지 않는다.
        if usage.get("reports").and_then(Value::as_array).is_some_and(|reports| {
            reports.iter().any(|report| report_identity(report).as_deref() == Some(identity))
        }) { continue; }
        let failed = disabled.iter().any(|entry| {
            let Some(cause) = entry.get("cause").and_then(Value::as_str) else { return false };
            if !cause.contains("invalid_grant") && !cause.contains("invalid_token") { return false; }
            let Some(provider) = entry.get("provider").and_then(Value::as_str) else { return false };
            quota::identity_from_metadata(provider, entry).as_deref() == Some(identity)
        });
        if failed {
            account.auth_status = "auth-required".into();
            account.can_launch = false;
            account.reason = Some("omp 로그인 인증이 만료되거나 무효화됐어요. omp에서 원래 공급자로 다시 로그인한 뒤 다시 확인해 주세요.".into());
            for bucket in &mut account.buckets { bucket.status = "stale".into(); }
        }
    }
}

/// `force`는 사용자가 직접 요청한 새로고침이다. 이때만 omp의 보고 캐시를 비우고 공식 Claude 조회를 다시 한다.
/// 정기 조회는 아직 유효한 관측을 다시 묻지 않는다(공급자 `/usage` 호출량과 조회 시간 제한).
pub fn scan(paths: &Paths, existing: &[Account], force: bool) -> Result<ScanResult, ApiError> {
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
    let mut omp_usage = None;
    if let Some(binary) = binaries.get("omp") {
        if force {
            // 리셋 직후처럼 사용자가 확인을 원하면 omp의 최대 5분 캐시를 비운다. 실패해도 일반 조회로 계속한다.
            let _ = process::run_quiet(binary, &["usage", "invalidate"], &process::base_env(), &home);
        }
        match process::run_json_slow(binary, &["usage", "--json"], &process::base_env(), &home) {
            Ok(value) => {
                observed_omp = add_omp_reports(&mut result, &value, binary, existing);
                omp_usage = Some(value);
            }
            Err(error) => result
                .notices
                .push(notice("OMP_USAGE_UNAVAILABLE", error.message)),
        }
    }
    // omp가 이번에 신선한 한도를 주지 못한(omp 미설치·미로그인·계정 누락) 연결된 Claude 로그인은 공식 CLI로 직접 조회한다.
    // 실패하면 기존 관측을 그대로 두어 오래된 값으로만 남기고, 조회 시각은 갱신하지 않는다.
    let mut native_failed = false;
    for account in result
        .accounts
        .iter_mut()
        .filter(|a| a.tool == "claude" && a.auth_status == "authenticated" && a.can_launch && !omp_fresh(a) && (force || native_due(a)))
    {
        match native_usage(account, existing) {
            Ok(buckets) => account.buckets = buckets,
            Err(_) => native_failed = true,
        }
    }
    if native_failed {
        result.notices.push(notice(
            "NATIVE_USAGE_UNAVAILABLE",
            "일부 Claude 계정의 사용량을 공식 CLI로 조회하지 못했어요. 이전 조회는 오래된 값으로만 남겼어요.",
        ));
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
        if stale.auth_status != "auth-required" {
            stale.auth_status = "unverified".into();
            stale.reason = Some("지금 omp 응답에서 이 계정을 다시 보지 못했어요. 이전 조회이며 공식 CLI 실행에 쓸 수 없어요.".into());
        }
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
    if let Some(usage) = omp_usage { apply_omp_auth_failures(&mut result.accounts, &usage); }
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

/// 공식 CLI로 확인한 native 로그인이 omp가 관측한 로그인(`observed`)과 같은 계정·워크스페이스인지.
/// 이메일만 같아서는 안 되고, 워크스페이스(조직)까지 같아야 한다. omp 계정 ID를 native ID로 쓰지 않고 identity만 비교한다.
pub fn same_login(native: &Account, observed: &Account) -> bool {
    let (Some(n), Some(o)) = (native.identity_key.as_deref(), observed.identity_key.as_deref()) else {
        return false;
    };
    if n == o {
        return true;
    }
    let (Some(native_email), Some(observed_email)) = (native.email.as_deref(), observed.email.as_deref()) else {
        return false;
    };
    if !native_email.eq_ignore_ascii_case(observed_email) {
        return false;
    }
    let workspace = |key: &str| key.rsplit_once("|workspace:").map(|(_, value)| value.to_owned()).unwrap_or_default();
    let native_workspace = workspace(n);
    if native_workspace.is_empty() {
        return false;
    }
    match native.tool.as_str() {
        "claude" => native_workspace == workspace(o),
        "codex" => {
            let subject = o.split('|').find_map(|part| part.strip_prefix("subject:"));
            subject == Some(native_workspace.as_str()) || workspace(o) == native_workspace
        }
        _ => false,
    }
}

/// omp 인증 저장소 항목(공급자·이메일·계정/조직/프로젝트 ID)에서 Ojak이 관측 계정에 쓰는 identity를 만든다.
/// `omp usage --json` 보고서와 같은 규칙이라 로그인 직후 항목을 관측 계정과 정확히 대조할 수 있다.
pub fn omp_credential_identity(
    provider: &str,
    email: Option<&str>,
    account_id: Option<&str>,
    org_id: Option<&str>,
    project_id: Option<&str>,
) -> Option<String> {
    let mut meta = serde_json::Map::new();
    for (key, value) in [("email", email), ("accountId", account_id), ("orgId", org_id), ("projectId", project_id)] {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            meta.insert(key.into(), Value::String(value.into()));
        }
    }
    quota::identity_from_metadata(provider, &Value::Object(meta))
}

#[cfg(test)]
mod login_identity_tests {
    use super::*;

    fn account(tool: &str, email: &str, key: &str) -> Account {
        Account { tool: tool.into(), email: Some(email.into()), identity_key: Some(key.into()), ..Account::default() }
    }

    #[test]
    fn claude_matches_only_same_email_and_workspace() {
        let native = account("claude", "A@x.com", "anthropic|subject:s1|workspace:org1");
        assert!(same_login(&native, &account("omp", "a@x.com", "anthropic|subject:s2|workspace:org1")));
        assert!(!same_login(&native, &account("omp", "a@x.com", "anthropic|subject:s2|workspace:org2")));
        assert!(!same_login(&native, &account("omp", "b@x.com", "anthropic|subject:s2|workspace:org1")));
    }

    #[test]
    fn codex_needs_the_same_workspace_account_id() {
        let native = account("codex", "a@x.com", "openai|email:a@x.com|workspace:acct-1");
        assert!(same_login(&native, &account("omp", "a@x.com", "openai-codex|subject:acct-1|workspace:org-9")));
        assert!(!same_login(&native, &account("omp", "a@x.com", "openai-codex|subject:acct-2|workspace:org-9")));
        let unscoped = account("codex", "a@x.com", "openai|email:a@x.com|workspace:");
        assert!(!same_login(&unscoped, &account("omp", "a@x.com", "openai-codex|subject:acct-1|workspace:org-9")));
    }

    #[test]
    fn credential_identity_matches_usage_report_rules() {
        assert_eq!(
            omp_credential_identity("xai-oauth", Some("A@x.com"), Some("uuid-1"), None, None).as_deref(),
            Some("xai-oauth|subject:uuid-1|workspace:")
        );
        assert_eq!(
            omp_credential_identity("google-antigravity", Some("A@x.com"), None, None, Some("proj")).as_deref(),
            Some("google-antigravity|email:a@x.com|workspace:proj")
        );
        assert_eq!(omp_credential_identity("google-antigravity", Some("a@x.com"), None, None, None), None);
    }
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

    #[cfg(unix)]
    #[test]
    fn logout_preserves_binding_and_requires_login_without_accepting_another_identity() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("aam-refresh-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let profile = root.canonicalize().unwrap();
        let binary = profile.join("claude");
        let write_probe = |value: Value| {
            fs::write(&binary, format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", value)).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        };
        let mut old = native_account(profile.to_str().unwrap());
        old.binary_path = Some(binary.to_string_lossy().into_owned());
        old.id = stable_id(&["claude", old.identity_key.as_deref().unwrap(), profile.to_str().unwrap()]);
        old.auth_status = "authenticated".into();
        old.can_launch = true;
        write_probe(json!({"loggedIn":false}));
        let logged_out = refresh_bound(&old, None);
        let authenticated = json!({"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"person@invalid.test","orgId":"workspace-a"});
        write_probe(authenticated.clone());
        let restored = refresh_bound(&logged_out, None);
        let mut different = authenticated;
        different["orgId"] = json!("workspace-b");
        write_probe(different);
        let drifted = refresh_bound(&restored, None);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(logged_out.auth_status, "auth-required");
        assert!(!logged_out.can_launch);
        assert_eq!(logged_out.id, old.id);
        assert_eq!(logged_out.identity_key, old.identity_key);
        assert_eq!(restored.auth_status, "authenticated");
        assert!(restored.can_launch);
        assert_eq!(restored.id, old.id);
        assert_eq!(drifted.auth_status, "error");
        assert!(!drifted.can_launch);
        assert_eq!(drifted.identity_key, old.identity_key);
        assert_eq!(drifted.id, old.id);
    }

    /// 공식 CLI를 흉내 내는 스크립트: `auth status`는 파일 내용을, 그 밖의 호출(`/usage`)은 이벤트 파일을 낸다.
    /// `/usage`가 실행된 뒤에는 두 번째 인증 상태를 낸다(조회 중 계정이 바뀐 상황).
    #[cfg(unix)]
    fn fake_claude(root: &Path, status: &Value, after: &Value, events: &[Value]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        fs::write(root.join("status.json"), status.to_string()).unwrap();
        fs::write(root.join("after.json"), after.to_string()).unwrap();
        let lines: Vec<String> = events.iter().map(Value::to_string).collect();
        fs::write(root.join("events.ndjson"), lines.join("\n") + "\n").unwrap();
        let binary = root.join("claude");
        fs::write(
            &binary,
            "#!/bin/sh\nd=$(dirname \"$0\")\ncase \"$1\" in\n  auth) if [ -e \"$d/ran\" ]; then cat \"$d/after.json\"; else cat \"$d/status.json\"; fi ;;\n  *) : > \"$d/ran\"; cat \"$d/events.ndjson\" ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        binary
    }

    #[cfg(unix)]
    #[test]
    fn native_claude_usage_refreshes_without_omp_and_reuses_omp_bucket_ids() {
        let root = std::env::temp_dir().join(format!("aam-native-usage-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let profile = root.canonicalize().unwrap();
        let status = json!({"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"person@invalid.test","orgId":"workspace-a"});
        let mut other = status.clone();
        other["orgId"] = json!("workspace-b");
        let limits = json!([
            {"kind":"session","percent":37,"resets_at":"2099-01-01T05:00:00+00:00","scope":null},
            {"kind":"weekly_all","percent":100,"resets_at":"2099-01-02T00:00:00+00:00","scope":null}
        ]);
        let events = |limits: Value| {
            vec![
                json!({"type":"system","subtype":"init","apiKeySource":"none"}),
                json!({"type":"assistant","local_command_run":{"command":"usage"},"usage_report":{"rate_limits":{"limits":limits}}}),
                json!({"type":"result","is_error":false,"num_turns":0}),
            ]
        };
        let binary = fake_claude(&profile, &status, &status, &events(limits.clone()));
        let mut account = native_account(profile.to_str().unwrap());
        account.binary_path = Some(binary.to_string_lossy().into_owned());
        account.email = Some("person@invalid.test".into());
        account.auth_status = "authenticated".into();
        account.can_launch = true;
        // 두 달 가까이 낡은 이전 관측: omp가 이번 스캔에서 신선한 값을 못 줬다.
        let omp_identity = "anthropic|subject:subject-a|workspace:workspace-a";
        let mut omp = Account { tool: "omp".into(), provider: "anthropic".into(), email: account.email.clone(), identity_key: Some(omp_identity.into()), ..Account::default() };
        omp.buckets = omp_buckets(&report("subject-a", "workspace-a"), omp_identity);
        for bucket in &mut omp.buckets {
            bucket.observed_at = now_ms() - 132_000_000;
            bucket.status = "stale".into();
        }
        assert!(!omp_fresh(&account));
        let fresh = native_usage(&account, std::slice::from_ref(&omp)).unwrap();
        assert_eq!(fresh.len(), 2);
        // omp가 같은 한도에 쓰는 ID를 이어 써서 요약이 같은 한도를 하나로 합친다.
        assert_eq!(fresh[0].id, omp.buckets[0].id);
        assert_eq!(fresh[0].used_percent, Some(37.0));
        assert!(now_ms() - fresh[0].observed_at < 60_000);
        assert_eq!(fresh[0].status, "known");
        assert_eq!(fresh[1].status, "exhausted");
        assert_eq!(fresh[0].source, quota::CLAUDE_USAGE_SOURCE);
        // omp 관측이 없으면 공식 identity로 만든 ID를 쓴다(다른 identity와 섞이지 않는다).
        assert_ne!(native_usage(&account, &[]).unwrap()[0].id, fresh[0].id);

        // 구조화된 한도가 없으면(예: 네트워크 실패) 새 관측을 만들지 않는다. 호출자는 기존 값을 그대로 둔다.
        fake_claude(&profile, &status, &status, &events(Value::Null));
        let failed = native_usage(&account, &[]).unwrap_err();
        assert_eq!(failed.code, "NATIVE_USAGE_UNAVAILABLE");
        let _ = fs::remove_file(profile.join("ran"));

        // 조회 도중 다른 워크스페이스 로그인으로 바뀌면 결과를 버린다.
        fake_claude(&profile, &status, &other, &events(limits));
        let _ = fs::remove_file(profile.join("ran"));
        assert_eq!(native_usage(&account, &[]).unwrap_err().code, "IDENTITY_DRIFT");

        // 로그아웃·미검증 계정은 조회 자체를 하지 않는다.
        let mut logged_out = account.clone();
        logged_out.auth_status = "auth-required".into();
        assert_eq!(native_usage(&logged_out, &[]).unwrap_err().code, "AUTH_REQUIRED");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn scheduled_claude_probe_skips_fresh_limits_but_rechecks_after_reset_or_age() {
        let now = now_ms();
        let bucket = |observed_at: i64, resets_at: Option<i64>, source: &str| QuotaBucket {
            id: "b".into(), label: "5시간".into(), model: None, used_percent: Some(40.0),
            resets_at, observed_at, source: source.into(), status: "known".into(),
        };
        let with = |buckets: Vec<QuotaBucket>| Account { tool: "claude".into(), buckets, ..Account::default() };
        let official = quota::CLAUDE_USAGE_SOURCE;
        assert!(!native_due(&with(vec![bucket(now - 60_000, Some(now + 3_600_000), official)])));
        // A limit whose reset time passed is rechecked immediately, so recovery shows without waiting.
        assert!(native_due(&with(vec![bucket(now - 60_000, Some(now - 1), official)])));
        assert!(native_due(&with(vec![bucket(now - NATIVE_USAGE_EVERY_MS, Some(now + 3_600_000), official)])));
        assert!(native_due(&with(vec![bucket(now - 60_000, None, "OMP usage / anthropic")])));
        assert!(native_due(&with(Vec::new())));
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
    fn disabled_oauth_requires_login_only_for_the_same_observed_identity() {
        let observed = Account {
            tool: "omp".into(), auth_status: "unverified".into(),
            identity_key: Some("anthropic|subject:subject-a|workspace:workspace-a".into()),
            buckets: vec![QuotaBucket { status: "known".into(), used_percent: Some(10.0), ..QuotaBucket::default() }],
            ..Account::default()
        };
        let disabled = json!({"provider":"anthropic","accountId":"subject-a","orgId":"workspace-a","cause":"OAuth refresh failed: invalid_grant"});
        let mut native = observed.clone();
        native.tool = "claude".into();
        native.auth_status = "authenticated".into();
        native.can_launch = true;
        let mut other = observed.clone();
        other.identity_key = Some("anthropic|subject:subject-a|workspace:workspace-b".into());
        let mut accounts = vec![observed.clone(), native, other];
        apply_omp_auth_failures(&mut accounts, &json!({"disabledCredentials":[disabled]}));
        assert_eq!(accounts[0].auth_status, "auth-required");
        assert_eq!(accounts[0].buckets[0].status, "stale");
        assert_eq!(accounts[1].auth_status, "authenticated");
        assert!(accounts[1].can_launch);
        assert_eq!(accounts[2].auth_status, "unverified");

        let mut recovered = vec![observed];
        apply_omp_auth_failures(&mut recovered, &json!({
            "reports":[report("subject-a", "workspace-a")], "disabledCredentials":[disabled]
        }));
        assert_eq!(recovered[0].auth_status, "unverified");
        assert_eq!(recovered[0].buckets[0].status, "known");
    }

    #[test]
    fn missing_usage_and_transient_failures_do_not_require_login() {
        for cause in ["network timeout", "429 Too Many Requests", "503 Service Unavailable"] {
            let mut accounts = vec![Account {
                tool: "omp".into(), auth_status: "unverified".into(),
                identity_key: Some("anthropic|subject:subject-a|workspace:workspace-a".into()),
                ..Account::default()
            }];
            apply_omp_auth_failures(&mut accounts, &json!({"disabledCredentials":[{
                "provider":"anthropic","accountId":"subject-a","orgId":"workspace-a","cause":cause
            }]}));
            assert_eq!(accounts[0].auth_status, "unverified");
        }
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
