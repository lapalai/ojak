mod access;
pub use access::{native_access, native_access_staged, NativeAccess, STAGES as NATIVE_ACCESS_STAGES};
mod native;
mod observed;
#[cfg(any(target_os = "macos", windows))]
mod observed_metadata;
pub use observed::{discover_sessions, open_jsonl_writers, ObservedScan};
mod process;
mod profiles;
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
    new_id, now_ms, Account, ApiError, DismissedProfile, IdentityEvidence, LaunchIntent, LaunchPlan, Notice, Paths,
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
use std::sync::Arc;

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

enum ScanEvent {
    Native(Result<(Option<Account>, bool), ApiError>),
    Inventory(Result<Option<Vec<Account>>, ApiError>),
    Omp(Result<Value, ApiError>),
    Version(&'static str, Option<String>),
    Failed(ApiError),
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

/// Check inherited native auth configuration without probing or changing the login.
pub fn check_native_auth_context(tool: &str, profile: &Path, cwd: &Path) -> Result<(), ApiError> {
    known_tool(tool)?;
    safety::check_settings(tool, profile, cwd)?;
    match profile.canonicalize() {
        Ok(canonical) => safety::check_inherited_profile(tool, &canonical),
        Err(_) => safety::check_inherited_profile(tool, profile),
    }
}

/// Subscription account APIs share the native CLI's inherited auth-override policy.
pub fn check_subscription_auth_env(tool: &str) -> Result<(), ApiError> {
    known_tool(tool)?;
    safety::check_env(tool)
}

fn failed_inspection(mut account: Account, error: ApiError, elapsed_ms: u64) -> Account {
    // 조회 프로세스의 일시 장애는 기존 로그인 폐기의 근거가 아니다. 새 로그인으로 승격하지 않고,
    // 이미 확인한 binding만 보존한다. 실제 실행·토큰 제공 때의 identity 검사는 그대로 수행한다.
    let preserve_binding = account.can_launch
        && account.auth_status == "authenticated"
        && account.verification == "preflight-verified"
        && account.identity_key.as_ref().is_some_and(|key| !key.is_empty())
        && matches!(error.code.as_str(), "PROBE_TIMEOUT" | "PROBE_IO" | "PROBE_START_FAILED" | "PROBE_CLOSED");
    if !preserve_binding {
        account.can_launch = false;
        account.auth_status = if error.code == "AUTH_REQUIRED" { "auth-required" } else { "error" }.into();
        account.verification = "configured".into();
    }
    account.reason = Some(format!("{} ({}, 경과: {elapsed_ms}ms)", error.message, error.code));
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
/// 두 번째 값은 이번 신원 조회의 성공 여부다. 정상 계정의 안내 문구(`reason`)와 구분한다.
fn refresh_bound(old: &Account, binary: Option<&Path>) -> (Account, bool) {
    let mut bound = old.clone();
    if let Some(binary) = binary {
        bound.binary_path = Some(binary.to_string_lossy().into_owned());
    }
    let started = std::time::Instant::now();
    let refreshed = (|| {
        let (profile, executable) = native::bound_paths(&bound)?;
        let mut fresh = native::inspect_identity_for_scan(&old.tool, &profile, &executable, &old.label)?;
        if fresh.auth_status == "auth-required" {
            return Err(ApiError::new("AUTH_REQUIRED", fresh.reason.unwrap_or_else(|| "공식 CLI에서 로그인한 뒤 다시 확인해 주세요.".into())));
        }
        fresh.binary_path = bound.binary_path.clone();
        if let Some(expected) = old.identity_key.as_ref() {
            if fresh.identity_key.as_ref() != Some(expected) {
                return Err(ApiError::new("IDENTITY_DRIFT", "이 프로필의 계정이 등록된 계정과 다르거나 로그아웃됐어요. 기존 연결은 바꾸지 않았어요. 원래 계정으로 '다시 로그인'해 주세요."));
            }
        }
        // 식별자가 없던 기본 프로필도 공식 CLI가 로그인을 확인하면 같은 행에서 연결을 확정한다.
        // 이미 고정된 identity는 위 검사로 계속 보호한다.
        fresh.id = old.id.clone();
        fresh.omp_credential_pins = old.omp_credential_pins.clone();
        // 추가 사용량은 omp 조회에서만 오므로 이 점검이 지우지 않는다. 관측 시각이 오래되면 쓰지 않는다.
        fresh.extra_usage = old.extra_usage.clone();
        fresh.credits = old.credits.clone();
        // 사용량 조회 실패 안내는 이번 신원 확인이 성공해도 이후 조회나 omp 관측이 해소할 때까지 이어 둔다.
        if fresh.reason.is_none() && old.reason.as_deref().is_some_and(is_usage_failure) {
            fresh.reason = old.reason.clone();
        }
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
    match refreshed {
        Ok(account) => (account, true),
        Err(error) => (failed_inspection(bound, error, started.elapsed().as_millis() as u64), false),
    }
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
    let mut official = account.buckets.iter().filter(|bucket| bucket.source == quota::CLAUDE_USAGE_SOURCE);
    let Some(first) = official.next() else { return true };
    // 조회 실패·낡은 관측으로 stale/unknown이 된 한도는 10분을 기다리지 않고 다음 정기 스캔에서 다시 묻는다.
    let due = |bucket: &QuotaBucket| bucket.observed_at <= 0
        || now.saturating_sub(bucket.observed_at) >= NATIVE_USAGE_EVERY_MS
        || bucket.resets_at.is_some_and(|reset| reset <= now)
        || matches!(bucket.status.as_str(), "stale" | "unknown");
    due(first) || official.any(due)
}

/// 공식 사용량 조회 실패의 안전한 요약. 단계·신뢰하는 오류 코드·경과 시간만 담고 원문 메시지는 버린다.
#[derive(Debug)]
struct UsageFailure {
    stage: &'static str,
    error: ApiError,
    elapsed_ms: u64,
}

const USAGE_FAILURE_PREFIX: &str = "공식 Claude 사용량 조회에 실패했어요";

fn is_usage_failure(reason: &str) -> bool {
    reason.starts_with(USAGE_FAILURE_PREFIX)
}

fn usage_failure_reason(failure: &UsageFailure) -> String {
    let code = failure.error.code.as_str();
    let code = if code.len() <= 48 && code.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_') {
        code
    } else {
        "UNKNOWN"
    };
    format!(
        "{USAGE_FAILURE_PREFIX} (단계: {}, 코드: {code}, 경과: {}ms). 이전 관측은 오래된 값으로만 남겨요.",
        failure.stage, failure.elapsed_ms
    )
}

/// 조회 실패를 계정에 남긴다. 관측 시각은 그대로 두고 한도만 오래된 값으로 표시한다.
fn record_usage_failure(account: &mut Account, failure: &UsageFailure) {
    for bucket in &mut account.buckets {
        bucket.status = if bucket.used_percent.is_some() { "stale" } else { "unknown" }.into();
    }
    account.reason = Some(usage_failure_reason(failure));
}

fn record_usage_success(account: &mut Account, buckets: Vec<QuotaBucket>) {
    account.buckets = buckets;
    if account.reason.as_deref().is_some_and(is_usage_failure) {
        account.reason = None;
    }
}

/// omp 관측이 한도를 다시 신선하게 채웠다면 해소된 사용량 조회 실패 안내를 지운다.
fn clear_resolved_usage_failure(account: &mut Account) {
    if account.tool == "claude" && account.reason.as_deref().is_some_and(is_usage_failure) && omp_fresh(account) {
        account.reason = None;
    }
}

/// Ojak에 연결된 공식 Claude 로그인으로 직접 한도를 조회한다. omp 로그인이 없어도 동작한다.
/// 공식 `claude -p /usage`(추론·도구·훅 없음, 공식 CLI가 자체 잠금으로 OAuth를 갱신)의 구조화 응답만 쓰며
/// 토큰은 읽지도 복사하지도 않는다. 조회 전에 이미 identity가 확인된 계정만 대상이고, 조회 뒤 같은 identity인지
/// 다시 확인한다. 응답이 구조화된 한도를 주지 않으면 오류이며 기존 관측은 그대로 오래된 값으로 남는다.
/// 버킷 ID는 omp 관측과 같은 한도라면 그 ID를 이어 써서 같은 한도가 중복·충돌하지 않게 한다.
fn native_usage(account: &Account, existing: &[Account]) -> Result<Vec<QuotaBucket>, UsageFailure> {
    let started = std::time::Instant::now();
    let fail = |stage: &'static str, error: ApiError| UsageFailure {
        stage,
        error,
        elapsed_ms: started.elapsed().as_millis() as u64,
    };
    let identity = account
        .identity_key
        .as_deref()
        .filter(|_| account.tool == "claude" && account.auth_status == "authenticated" && account.can_launch)
        .ok_or_else(|| fail("preflight", ApiError::new("AUTH_REQUIRED", "사용할 수 있는 공식 Claude 로그인이 없어요.")))?;
    let (profile, executable) = native::bound_paths(account).map_err(|error| fail("preflight", error))?;
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
    )
    .map_err(|error| fail("usage", error))?;
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
        return Err(fail(
            "parse",
            ApiError::new(
                "NATIVE_USAGE_UNAVAILABLE",
                "공식 Claude가 구조화된 사용량을 주지 않았어요. 이전 조회는 오래된 값으로만 남겨요.",
            ),
        ));
    }
    let after = native::inspect_identity_for_scan(&account.tool, &profile, &executable, &account.label)
        .map_err(|error| fail("identity-recheck", error))?;
    if after.auth_status != "authenticated" || after.identity_key.as_deref() != Some(identity) {
        return Err(fail(
            "identity-recheck",
            ApiError::new(
                "IDENTITY_DRIFT",
                "사용량을 조회하는 사이 공식 Claude 계정이 바뀌었어요. 조회 결과를 쓰지 않았어요.",
            ),
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
                let current = &result.accounts[index];
                let newer = buckets.iter().map(|bucket| bucket.observed_at).max()
                    >= current.buckets.iter().map(|bucket| bucket.observed_at).max();
                if current.buckets.is_empty() || (current.tool != "codex" && newer) {
                    result.accounts[index].buckets = buckets.clone();
                }
            }
            if !existing
                .iter()
                .any(|a| a.id == id && (a.profile_path.is_some() || !a.enabled))
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
        if let Some(previous) = result.accounts.iter_mut().find(|previous| previous.id == account.id) {
            *previous = account;
        } else {
            result.accounts.push(account);
        }
    }
    if let Some(unavailable) = value.get("accountsWithoutUsage").and_then(Value::as_array) {
        for entry in unavailable {
            let Some(provider) = text(entry, "provider") else {
                continue;
            };
            let mut account = omp_observation(&provider, entry, existing);
            account.binary_path = Some(binary.to_string_lossy().into_owned());
            if let Some(previous) = result.accounts.iter_mut().find(|previous| previous.id == account.id) {
                // 명시적인 identity가 같은 경우에만 누락 응답을 연결한다. 다른 workspace는 별도 행이다.
                if !matches!(previous.auth_status.as_str(), "auth-required" | "error") {
                    previous.reason = Some("omp 로그인 항목은 확인했지만 현재 사용량 응답이 없어요. 사용량을 0으로 추정하지 않아요.".into());
                }
            } else {
                result.accounts.push(account);
            }
        }
    }
    result.accounts.retain(|account| !result.merged_observation_ids.contains(&account.id));
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

/// broker의 로그인 목록을 한도 조회와 분리한다. 인증 값은 결과에 옮기지 않는다.
pub fn omp_inventory(snapshot: &Value, existing: &[Account]) -> Result<Vec<Account>, ApiError> {
    let rows = snapshot.get("credentials").and_then(Value::as_array)
        .ok_or_else(|| ApiError::new("OMP_SCHEMA", "omp 로그인 목록 형식을 확인하지 못했어요."))?;
    let mut accounts: Vec<Account> = Vec::new();
    for row in rows {
        let Some(provider) = text(row, "provider").filter(|provider|
            matches!(provider.as_str(), "anthropic" | "openai-codex" | "google-antigravity" | "xai-oauth" | "zai")) else { continue };
        let Some(metadata) = row.get("credential").filter(|credential| credential["type"] == "oauth") else { continue };
        let account = omp_observation(&provider, metadata, existing);
        if !accounts.iter().any(|previous| previous.id == account.id) {
            accounts.push(account);
        }
    }
    Ok(accounts)
}

fn omp_observation(provider: &str, metadata: &Value, existing: &[Account]) -> Account {
    let identity = quota::identity_from_metadata(provider, metadata);
    let email = text(metadata, "email");
    let id = identity.as_deref().map(|identity| stable_id(&["omp", identity, "observation"]))
        .unwrap_or_else(|| stable_id(&["omp-unverified", provider,
            &metadata.get("authorizedAt").and_then(Value::as_i64).unwrap_or(0).to_string(),
            email.as_deref().unwrap_or("")]));
    let mut account = existing.iter().find(|account| account.id == id).cloned().unwrap_or_else(|| Account {
        id, enabled: true, max_concurrency: 1, ..Account::default()
    });
    account.provider = provider_id(provider).into();
    account.tool = "omp".into();
    account.label = account_label(&format!("OMP · {provider}"), email.as_deref());
    account.email = email;
    account.organization = text(metadata, "orgName").or_else(|| text(metadata, "orgId")).or_else(|| text(metadata, "projectId"));
    account.plan = text(metadata, "planType").or_else(|| text(metadata, "subscriptionType"));
    account.identity_key = identity;
    if !matches!(account.auth_status.as_str(), "auth-required" | "error") {
        account.auth_status = "unverified".into();
        account.reason = Some(OMP_GATE.into());
    }
    account.verification = "observed".into();
    account.can_launch = false;
    if let Some(old) = existing.iter().find(|old| old.id == account.id) {
        account = preserve_preferences(account, old);
    }
    account
}


/// 아직 등록되지 않은 native 프로필 후보: 기본 프로필(`~/.claude`, `~/.codex`)과 Ojak이 만든 관리 프로필.
/// 이미 등록된 경로와 사용자가 연결을 끊은(`dismissed`) 경로는 뺀다.
/// 등록되지 않은 폴더의 복구 검사는 명시적 새로고침/빈 카탈로그에서만 하며, 정기 사용량 갱신마다
/// 이미 대체된 관리 프로필까지 CLI로 재검사하지 않는다. 기본 프로필은 항상 후보다.
pub fn discovery_candidates(
    home: &Path,
    paths: &Paths,
    existing: &[Account],
    dismissed: &[DismissedProfile],
    force: bool,
) -> Vec<(String, PathBuf)> {
    let mut candidates = if force || existing.is_empty() { profiles::managed_profiles(paths) } else { Vec::new() };
    for (tool, folder) in [("claude", ".claude"), ("codex", ".codex")] {
        if let Ok(profile) = safety::canonical_profile(&home.join(folder)) {
            candidates.push((tool.to_owned(), profile));
        }
    }
    candidates.retain(|(tool, profile)| {
        let path = profile.to_str();
        !existing.iter().any(|account| &account.tool == tool && account.profile_path.as_deref() == path)
            // 사용자가 연결을 끊은 폴더는 명시적으로 다시 연결하기 전까지 자동 탐색하지 않는다.
            && !dismissed.iter().any(|entry| &entry.tool == tool && Some(entry.path.as_str()) == path)
    });
    candidates
}

/// 각 native 프로필의 인증→사용량을 이어서 실행하고, 독립 소스 결과는 완료 즉시 반영한다.
/// CLI 조회는 최대 4개 native worker를 사용하며, 정기 조회는 신선한 관측을 재사용한다.
pub fn scan(
    paths: &Paths,
    existing: &[Account],
    dismissed: &[DismissedProfile],
    force: bool,
    inventory: impl FnOnce() -> Result<Option<Vec<Account>>, ApiError> + Send,
    publish: impl FnMut(&Account) -> Result<(), ApiError> + Send,
) -> Result<ScanResult, ApiError> {
    use parking_lot::Mutex;
    use std::sync::mpsc;
    let home = user_home()?;
    let started = now_ms();
    let binaries: BTreeMap<_, _> = ["claude", "codex", "omp"].into_iter()
        .filter_map(|tool| process::discover(paths, tool).map(|binary| (tool, binary)))
        .collect();
    let managed_root = paths.profiles.canonicalize().ok();
    let mut discovered = Vec::new();
    for (tool, profile) in discovery_candidates(&home, paths, existing, dismissed, force) {
        let Some(binary) = binaries.get(tool.as_str()) else { continue };
        let managed = profile.parent() == managed_root.as_deref();
        let label = if managed { tool_details(&tool).0.to_owned() } else { format!("{} · 기본 프로필", tool_details(&tool).0) };
        discovered.push((blank_account(&tool, &profile, binary, &label), managed));
    }
    let native: Vec<_> = existing.iter()
        .filter(|account| matches!(account.tool.as_str(), "claude" | "codex"))
        .map(|account| (account, false))
        .chain(discovered.iter().map(|(account, managed)| (account, *managed)))
        .collect();
    let publisher = Mutex::new(publish);
    let omp_cache: Mutex<Option<Arc<Value>>> = Mutex::new(None);
    std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        let inventory_sender = sender.clone();
        scope.spawn(move || { let _ = inventory_sender.send(ScanEvent::Inventory(inventory())); });
        if let Some(binary) = binaries.get("omp") {
            let sender = sender.clone();
            let home = &home;
            scope.spawn(move || {
                if force {
                    let _ = process::run_quiet(binary, &["usage", "invalidate"], &process::base_env(), home);
                }
                let _ = sender.send(ScanEvent::Omp(process::run_json_slow(binary, &["usage", "--json"], &process::base_env(), home)));
            });
        }
        for (&tool, binary) in &binaries {
            let sender = sender.clone();
            let home = &home;
            scope.spawn(move || { let _ = sender.send(ScanEvent::Version(tool, process::version(binary, home))); });
        }
        let native_sender = sender.clone();
        let publisher_ref = &publisher;
        let cache = &omp_cache;
        let binaries_ref = &binaries;
        scope.spawn(move || {
            let result = probe_each(native, |(old, managed)| {
                let inspect = || -> Result<(Option<Account>, bool), ApiError> {
                    let (mut account, confirmed) = refresh_bound(old, binaries_ref.get(old.tool.as_str()).map(PathBuf::as_path));
                    let already_bound = managed && existing.iter().any(|old| old.tool == account.tool
                        && old.identity_key.is_some() && old.identity_key == account.identity_key);
                    if managed && (!account.can_launch || already_bound) { return Ok((None, false)); }
                    let cached = cache.lock().as_ref().map(Arc::clone);
                    let omp_ready = cached.as_ref().is_some_and(|value| refresh_native_from_omp(&mut account, value, force, started).0);
                    clear_resolved_usage_failure(&mut account);
                    publisher_ref.lock()(&account)?;
                    let mut failed = false;
                    // binding을 보존한 일시적 신원 조회 실패 뒤에는 별도 사용량 조회를 중복 실행하지 않는다.
                    if confirmed && account.auth_status == "authenticated" && account.can_launch {
                        if account.tool == "claude" && !omp_ready && (force || (!omp_fresh(&account) && native_due(&account))) {
                            match native_usage(&account, existing) {
                                Ok(buckets) => record_usage_success(&mut account, buckets),
                                Err(failure) => record_usage_failure(&mut account, &failure),
                            }
                            let cached = cache.lock().as_ref().map(Arc::clone);
                            if let Some(value) = cached { refresh_native_from_omp(&mut account, &value, false, started); }
                            clear_resolved_usage_failure(&mut account);
                            failed = account.reason.as_deref().is_some_and(is_usage_failure);
                            publisher_ref.lock()(&account)?;
                        } else if account.tool == "codex" {
                            let updated = match native::codex_usage(&account) {
                                Ok((buckets, credits)) => { account.buckets = buckets; account.credits = credits; true }
                                Err(_) => account.credits.take().is_some(),
                            };
                            if updated { publisher_ref.lock()(&account)?; }
                        }
                    }
                    Ok((Some(account), failed))
                };
                inspect()
            }, |result| {
                native_sender.send(ScanEvent::Native(result))
                    .map_err(|_| ApiError::new("PROBE_CLOSED", "계정 조회 결과의 수신 경로가 닫혔어요."))
            });
            if let Err(error) = result { let _ = native_sender.send(ScanEvent::Failed(error)); }
        });
        drop(sender);
        let mut result = ScanResult { accounts: Vec::new(), tools: Vec::new(), notices: Vec::new(), merged_observation_ids: Vec::new() };
        let mut versions = BTreeMap::new();
        let mut omp_usage: Option<Arc<Value>> = None;
        let mut observed_omp = BTreeSet::new();
        let mut native_failed = false;
        for event in receiver {
            match event {
                ScanEvent::Native(outcome) => {
                    let (account, failed) = outcome?;
                    native_failed |= failed;
                    if let Some(mut account) = account {
                        if let Some(value) = &omp_usage {
                            let updated = refresh_native_from_omp(&mut account, value, false, started).1;
                            let before = account.reason.clone();
                            clear_resolved_usage_failure(&mut account);
                            if updated || account.reason != before { publisher.lock()(&account)?; }
                        }
                        result.accounts.push(account);
                    }
                }
                ScanEvent::Inventory(outcome) => match outcome {
                    Ok(Some(accounts)) => for mut account in accounts {
                        if let Some(current) = result.accounts.iter_mut().find(|current| current.id == account.id) {
                            // 늦게 온 목록이 더 새로운 사용량/인증 근거를 덮지 않는다.
                            if current.email.is_none() { current.email = account.email.take(); }
                            if current.organization.is_none() { current.organization = account.organization.take(); }
                            if current.plan.is_none() { current.plan = account.plan.take(); }
                        } else {
                            account.binary_path = binaries.get("omp").map(|binary| binary.to_string_lossy().into_owned());
                            publisher.lock()(&account)?;
                            result.accounts.push(account);
                        }
                    },
                    Ok(None) => {},
                    Err(error) => result.notices.push(notice("OMP_INVENTORY_UNAVAILABLE", error.message)),
                },
                ScanEvent::Omp(outcome) => match outcome {
                    Ok(value) => {
                        let value = Arc::new(value);
                        *omp_cache.lock() = Some(Arc::clone(&value));
                        observed_omp = add_omp_reports(&mut result, &value, &binaries["omp"], existing);
                        for account in &mut result.accounts {
                            if account.tool == "omp" || value["reports"].as_array().is_some_and(|reports| reports.iter().any(|report| matches_report(account, report))) {
                                clear_resolved_usage_failure(account);
                                publisher.lock()(account)?;
                            }
                        }
                        omp_usage = Some(value);
                    }
                    Err(error) => result.notices.push(notice("OMP_USAGE_UNAVAILABLE", error.message)),
                },
                ScanEvent::Version(tool, version) => { versions.insert(tool, version); },
                ScanEvent::Failed(error) => return Err(error),
            }
        }
        // 완성된 native 목록에서 같은 identity의 관측만 통합한다. 이메일-only 기록은 삭제 근거로 쓰지 않는다.
        if let Some(value) = &omp_usage {
            observed_omp = add_omp_reports(&mut result, value, &binaries["omp"], existing);
        }
        for account in &mut result.accounts { clear_resolved_usage_failure(account); }
        if native_failed && result.accounts.iter().any(|account| account.reason.as_deref().is_some_and(is_usage_failure)) {
            result.notices.push(notice("NATIVE_USAGE_UNAVAILABLE", "일부 Claude 계정의 사용량을 공식 CLI로 조회하지 못했어요. 이전 관측 시각은 갱신하지 않았어요."));
        }
        let current: BTreeSet<_> = result.accounts.iter().map(|account| account.id.as_str()).collect();
        let stale: Vec<_> = existing.iter()
            .filter(|account| account.tool == "omp" && !current.contains(account.id.as_str()) && !observed_omp.contains(&account.id))
            .map(|old| {
                let mut stale = old.clone();
                stale.can_launch = false;
                stale.verification = "observed".into();
                if !matches!(stale.auth_status.as_str(), "auth-required" | "error") {
                    stale.auth_status = "unverified".into();
                    stale.reason = Some("지금 omp 응답에서 이 계정을 다시 보지 못했어요. 이전 조회이며 공식 CLI 실행에 쓸 수 없어요.".into());
                }
                for bucket in &mut stale.buckets { bucket.status = if bucket.used_percent.is_some() { "stale" } else { "unknown" }.into(); }
                stale
            }).collect();
        result.accounts.extend(stale);
        if let Some(value) = &omp_usage { apply_omp_auth_failures(&mut result.accounts, value); }
        for tool in ["claude", "codex", "omp"] {
            let binary = binaries.get(tool);
            let (name, provider, isolation, reason) = tool_details(tool);
            result.tools.push(ToolStatus {
                id: tool.into(), name: name.into(), provider: provider.into(),
                binary_path: binary.map(|binary| binary.to_string_lossy().into_owned()),
                version: versions.remove(tool).flatten(), installed: binary.is_some(),
                isolation: isolation.into(), reason: reason.map(str::to_owned),
            });
        }
        Ok(result)
    })
}

/// 첫 값은 사용 가능한 omp 관측인지, 두 번째 값은 계정에 더 새로운 사실을 반영했는지다.
fn refresh_native_from_omp(account: &mut Account, value: &Value, force: bool, started: i64) -> (bool, bool) {
    if account.tool != "claude" || !account.can_launch { return (false, false); }
    let Some(reports) = value["reports"].as_array() else { return (false, false) };
    let mut matching = reports.iter().filter(|report| matches_report(account, report));
    let Some(report) = matching.next() else { return (false, false) };
    if matching.next().is_some() { return (false, false); }
    let Some(identity) = report_identity(report) else { return (false, false) };
    let buckets = omp_buckets(report, &identity);
    let observed = buckets.iter().map(|bucket| bucket.observed_at).max();
    let previous = account.buckets.iter().map(|bucket| bucket.observed_at).max();
    let updated = !buckets.is_empty() && (observed > previous || (observed == previous && !omp_fresh(account)));
    if updated {
        account.buckets = buckets;
        account.extra_usage = omp_extra_usage(report);
        if let Some(pin) = quota::credential_pin(report) {
            if !account.omp_credential_pins.contains(&pin) { account.omp_credential_pins.push(pin); }
        }
    }
    (omp_fresh(account) && (!force || account.buckets.iter().all(|bucket| bucket.observed_at >= started)), updated)
}

/// 최대 4개의 독립 CLI 조회를 실행하고, 느린 프로필과 무관하게 완료 순서로 반영한다.
fn probe_each<T: Send, R: Send>(
    items: Vec<T>,
    probe: impl Fn(T) -> R + Sync,
    mut receive: impl FnMut(R) -> Result<(), ApiError>,
) -> Result<(), ApiError> {
    use parking_lot::Mutex;
    use std::sync::mpsc;
    let workers = items.len().min(4);
    let next = Mutex::new(items.into_iter());
    std::thread::scope(|scope| {
        let (sender, receiver) = mpsc::channel();
        for _ in 0..workers {
            let sender = sender.clone();
            let probe = &probe;
            let next = &next;
            scope.spawn(move || {
                loop {
                    let Some(item) = next.lock().next() else { break };
                    if sender.send(probe(item)).is_err() { break; }
                }
            });
        }
        drop(sender);
        for result in receiver { receive(result)?; }
        Ok(())
    })
}


pub fn verify(account: &Account) -> Result<IdentityEvidence, ApiError> {
    verify_at(account, None)
}

fn verify_at(account: &Account, cwd: Option<&Path>) -> Result<IdentityEvidence, ApiError> {
    known_tool(&account.tool)?;
    let (profile, binary) = native::bound_paths(account)?;
    let fresh = native::inspect_identity_at(
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
        return Err(ApiError::new("IDENTITY_DRIFT", "실제 계정이 등록된 계정과 같지 않아 실행을 막았어요. 원래 계정으로 '다시 로그인'해 주세요."));
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
    let (profile, binary) = native::bound_paths(account)?;
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

/// 선두 `~/`(또는 `~`만)를 사용자 홈으로 풀어 준다. 그 밖의 상대 경로는 그대로 두어 `canonical_profile`이 거절한다.
fn expand_home(path: &str, home: impl FnOnce() -> Result<PathBuf, ApiError>) -> Result<PathBuf, ApiError> {
    let rest = if path == "~" {
        Some("")
    } else {
        path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\").filter(|_| cfg!(windows)))
    };
    match rest {
        Some(rest) => Ok(home()?.join(rest)),
        None => Ok(PathBuf::from(path)),
    }
}

pub fn register(
    paths: &Paths,
    tool: &str,
    label: &str,
    profile_path: &str,
) -> Result<Account, ApiError> {
    known_tool(tool)?;
    validate_label(label)?;
    let profile = safety::canonical_profile(&expand_home(profile_path, user_home)?)?;
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
mod home_expansion_tests {
    use super::*;

    fn home() -> Result<PathBuf, ApiError> {
        Ok(PathBuf::from("/home/person"))
    }

    #[test]
    fn only_a_leading_tilde_slash_or_bare_tilde_is_expanded() {
        assert_eq!(expand_home("~", home).unwrap(), PathBuf::from("/home/person"));
        assert_eq!(expand_home("~/.claude-work", home).unwrap(), PathBuf::from("/home/person/.claude-work"));
        for untouched in ["/abs/path", "foo/bar", "./x", "~other/x", "a/~/b", "~x"] {
            assert_eq!(expand_home(untouched, home).unwrap(), PathBuf::from(untouched));
        }
    }

    #[test]
    fn a_missing_home_is_reported_only_when_it_is_needed() {
        let missing = || Err(ApiError::new("HOME_MISSING", "홈 없음"));
        assert_eq!(expand_home("~/x", missing).unwrap_err().code, "HOME_MISSING");
        assert!(expand_home("/abs", missing).is_ok());
    }
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

    #[test]
    fn temporary_identity_probe_failure_preserves_only_a_verified_binding() {
        let mut verified = native_account("/fixture");
        verified.can_launch = true;
        verified.auth_status = "authenticated".into();
        verified.verification = "preflight-verified".into();
        verified.buckets.push(QuotaBucket {
            used_percent: Some(10.0), status: "known".into(), ..Default::default()
        });
        for code in ["PROBE_TIMEOUT", "PROBE_IO", "PROBE_START_FAILED", "PROBE_CLOSED"] {
            let retained = failed_inspection(verified.clone(), ApiError::new(code, "조회 실패"), 7);
            assert!(retained.can_launch);
            assert_eq!(retained.auth_status, "authenticated");
            assert_eq!(retained.verification, "preflight-verified");
            assert_eq!(retained.identity_key, verified.identity_key);
            assert_eq!(retained.buckets[0].status, "stale");
        }
        let unknown = failed_inspection(native_account("/fixture"), ApiError::new("PROBE_TIMEOUT", "조회 지연"), 7);
        assert!(!unknown.can_launch);
        assert_ne!(unknown.auth_status, "authenticated");
        for code in ["AUTH_REQUIRED", "IDENTITY_DRIFT", "AUTH_OVERRIDE_CONFLICT", "PROBE_SCHEMA"] {
            let blocked = failed_inspection(verified.clone(), ApiError::new(code, "명시적 실패"), 7);
            assert!(!blocked.can_launch, "{code}");
            assert_ne!(blocked.auth_status, "authenticated", "{code}");
            assert_eq!(blocked.identity_key, verified.identity_key);
        }
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
        let (logged_out, confirmed) = refresh_bound(&old, None);
        assert!(!confirmed);
        let authenticated = json!({"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"person@invalid.test","orgId":"workspace-a"});
        write_probe(authenticated.clone());
        let (restored, confirmed) = refresh_bound(&logged_out, None);
        assert!(confirmed);
        let mut different = authenticated;
        different["orgId"] = json!("workspace-b");
        write_probe(different);
        let (drifted, confirmed) = refresh_bound(&restored, None);
        assert!(!confirmed);
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

    #[test]
    fn completed_profiles_are_published_before_a_slow_profile_finishes() {
        use parking_lot::Mutex;
        use std::sync::mpsc;
        use std::time::Duration;
        let (release, gate) = mpsc::channel();
        let gate = Mutex::new(gate);
        let mut completed = Vec::new();
        probe_each(vec![0, 1], |profile| {
            if profile == 0 {
                gate.lock().recv_timeout(Duration::from_secs(3)).unwrap();
            }
            profile
        }, |profile| {
            completed.push(profile);
            if profile == 1 { release.send(()).unwrap(); }
            Ok(())
        }).unwrap();
        assert_eq!(completed, [1, 0]);
    }

    #[cfg(unix)]
    #[test]
    fn previously_unidentified_profile_adopts_verified_login_without_changing_preferences() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("aam-adopt-{}", new_id()));
        fs::create_dir(&root).unwrap();
        let profile = root.canonicalize().unwrap();
        let binary = profile.join("claude");
        fs::write(&binary, "#!/bin/sh\nprintf '%s\\n' '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"apiProvider\":\"firstParty\",\"email\":\"person@invalid.test\",\"orgId\":\"workspace-a\"}'\n").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let mut old = blank_account("claude", &profile, &binary, "Personal");
        old.auth_status = "auth-required".into();
        old.enabled = false;
        old.max_concurrency = 3;
        let (fresh, confirmed) = refresh_bound(&old, None);
        assert!(confirmed);
        assert!(fresh.can_launch);
        assert_eq!(fresh.auth_status, "authenticated");
        assert_eq!(fresh.identity_key.as_deref(), Some("anthropic|email:person@invalid.test|workspace:workspace-a"));
        assert_eq!(fresh.id, old.id);
        assert!(!fresh.enabled);
        assert_eq!(fresh.max_concurrency, 3);
        fs::remove_dir_all(root).unwrap();
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
        assert_eq!(failed.error.code, "NATIVE_USAGE_UNAVAILABLE");
        assert_eq!(failed.stage, "parse");
        let _ = fs::remove_file(profile.join("ran"));

        // 조회 도중 다른 워크스페이스 로그인으로 바뀌면 결과를 버린다.
        fake_claude(&profile, &status, &other, &events(limits));
        let _ = fs::remove_file(profile.join("ran"));
        let drift = native_usage(&account, &[]).unwrap_err();
        assert_eq!((drift.error.code.as_str(), drift.stage), ("IDENTITY_DRIFT", "identity-recheck"));

        // 로그아웃·미검증 계정은 조회 자체를 하지 않는다.
        let mut logged_out = account.clone();
        logged_out.auth_status = "auth-required".into();
        assert_eq!(native_usage(&logged_out, &[]).unwrap_err().error.code, "AUTH_REQUIRED");
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

    fn usage_bucket(status: &str, observed_at: i64, source: &str) -> QuotaBucket {
        QuotaBucket {
            id: "b".into(), label: "5시간".into(), model: None, used_percent: Some(40.0),
            resets_at: Some(now_ms() + 3_600_000), observed_at, source: source.into(), status: status.into(),
        }
    }

    #[test]
    fn recent_stale_or_unknown_official_limits_are_requeried_but_healthy_ones_keep_the_cadence() {
        let now = now_ms();
        let official = quota::CLAUDE_USAGE_SOURCE;
        let with = |status: &str, age: i64| Account {
            tool: "claude".into(),
            buckets: vec![usage_bucket(status, now - age, official)],
            ..Account::default()
        };
        assert!(native_due(&with("stale", 60_000)));
        assert!(native_due(&with("unknown", 60_000)));
        assert!(!native_due(&with("known", 60_000)));
        assert!(!native_due(&with("exhausted", 60_000)));
        assert!(native_due(&with("known", NATIVE_USAGE_EVERY_MS)));
        // 여러 한도 중 하나만 낡아도 다시 묻는다.
        let mut mixed = with("known", 60_000);
        mixed.buckets.push(usage_bucket("stale", now - 60_000, official));
        assert!(native_due(&mixed));
    }

    #[test]
    fn usage_failure_is_persisted_safely_marks_buckets_stale_and_keeps_observed_at() {
        let observed_at = now_ms() - 60_000;
        let mut account = native_account("/fixture");
        account.buckets = vec![
            usage_bucket("known", observed_at, quota::CLAUDE_USAGE_SOURCE),
            QuotaBucket { used_percent: None, status: "known".into(), ..usage_bucket("known", observed_at, quota::CLAUDE_USAGE_SOURCE) },
        ];
        let secret = "sk-ant-oat01-SECRET person@invalid.test private prompt";
        let failure = UsageFailure {
            stage: "usage",
            error: ApiError::new("PROBE_TIMEOUT", secret),
            elapsed_ms: 22_400,
        };
        record_usage_failure(&mut account, &failure);
        let reason = account.reason.clone().unwrap();
        assert!(reason.contains("usage") && reason.contains("PROBE_TIMEOUT") && reason.contains("22400ms"), "{reason}");
        assert!(!reason.contains("SECRET") && !reason.contains("person@") && !reason.contains("prompt"), "{reason}");
        assert!(is_usage_failure(&reason));
        assert_eq!(account.buckets[0].status, "stale");
        assert_eq!(account.buckets[1].status, "unknown");
        assert!(account.buckets.iter().all(|bucket| bucket.observed_at == observed_at));
        // 신뢰할 수 없는 형태의 코드는 그대로 싣지 않는다.
        let hostile = UsageFailure { stage: "usage", error: ApiError::new(secret, "x"), elapsed_ms: 1 };
        assert!(!usage_failure_reason(&hostile).contains("SECRET"));
        assert!(usage_failure_reason(&hostile).contains("UNKNOWN"));
        // 오래된 값으로 남은 한도는 다음 정기 스캔에서 10분을 기다리지 않고 다시 조회된다.
        assert!(native_due(&account));
    }

    #[cfg(unix)]
    #[test]
    fn usage_failure_survives_identity_refresh_and_is_cleared_by_recovery_or_fresh_omp() {
        let root = std::env::temp_dir().join(format!("aam-usage-reason-{}", new_id()));
        fs::create_dir(&root).unwrap();
        let profile = root.canonicalize().unwrap();
        let status = json!({"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"person@invalid.test","orgId":"workspace-a"});
        let binary = fake_claude(&profile, &status, &status, &[]);
        let mut account = native_account(profile.to_str().unwrap());
        account.binary_path = Some(binary.to_string_lossy().into_owned());
        account.id = stable_id(&["claude", account.identity_key.as_deref().unwrap(), profile.to_str().unwrap()]);
        account.auth_status = "authenticated".into();
        account.can_launch = true;
        account.verification = "preflight-verified".into();
        account.buckets = vec![usage_bucket("known", now_ms() - 60_000, quota::CLAUDE_USAGE_SOURCE)];
        record_usage_failure(&mut account, &UsageFailure {
            stage: "usage", error: ApiError::new("PROBE_TIMEOUT", "raw"), elapsed_ms: 20_000,
        });
        let failed_reason = account.reason.clone();

        // 다음 스캔의 신원 확인이 성공해도 해소 전에는 실패 이유와 오래된 한도 상태가 남는다.
        let (refreshed, confirmed) = refresh_bound(&account, None);
        assert!(confirmed && refreshed.can_launch);
        assert_eq!(refreshed.reason, failed_reason);
        assert_eq!(refreshed.buckets[0].status, "stale");

        // 성공한 공식 조회가 해소한다.
        let mut recovered = refreshed.clone();
        let fresh = vec![usage_bucket("known", now_ms(), quota::CLAUDE_USAGE_SOURCE)];
        record_usage_success(&mut recovered, fresh);
        assert_eq!(recovered.reason, None);
        assert_eq!(recovered.buckets[0].status, "known");

        // 신선한 omp 관측이 한도를 채우면 해소된 실패 이유를 지운다. 신선하지 않으면 남긴다.
        let mut via_omp = refreshed.clone();
        clear_resolved_usage_failure(&mut via_omp);
        assert_eq!(via_omp.reason, failed_reason);
        via_omp.buckets = vec![usage_bucket("known", now_ms(), "OMP usage / anthropic")];
        clear_resolved_usage_failure(&mut via_omp);
        assert_eq!(via_omp.reason, None);

        // 관련 없는 안내 문구는 지우지 않는다.
        let mut other = via_omp.clone();
        other.reason = Some("다른 안내".into());
        clear_resolved_usage_failure(&mut other);
        record_usage_success(&mut other, vec![usage_bucket("known", now_ms(), quota::CLAUDE_USAGE_SOURCE)]);
        assert_eq!(other.reason.as_deref(), Some("다른 안내"));
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn identity_failure_keeps_safe_fixed_message_with_code_and_elapsed_time() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("aam-identity-fail-{}", new_id()));
        fs::create_dir(&root).unwrap();
        let profile = root.canonicalize().unwrap();
        let binary = profile.join("claude");
        let write_probe = |value: Value| {
            fs::write(&binary, format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", value)).unwrap();
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        };
        let mut old = native_account(profile.to_str().unwrap());
        old.binary_path = Some(binary.to_string_lossy().into_owned());
        old.auth_status = "authenticated".into();
        old.can_launch = true;
        let authenticated = json!({"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty","email":"person@invalid.test","orgId":"workspace-a"});
        let mut different = authenticated;
        different["orgId"] = json!("workspace-b");
        write_probe(different);
        let (drifted, confirmed) = refresh_bound(&old, None);
        fs::remove_dir_all(&root).unwrap();
        assert!(!confirmed);
        assert!(!drifted.can_launch);
        assert_eq!(drifted.auth_status, "error");
        let reason = drifted.reason.unwrap();
        assert!(reason.contains("IDENTITY_DRIFT") && reason.contains("경과:") && reason.contains("ms"), "{reason}");
        assert!(!reason.contains("workspace-b") && !reason.contains("person@"), "{reason}");
        assert!(!is_usage_failure(&reason));
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
    fn broker_inventory_preserves_known_usage_without_needing_a_usage_response_or_copying_secrets() {
        let snapshot = json!({"credentials":[
            {"provider":"anthropic","credential":{"type":"oauth","accountId":"subject-a","orgId":"workspace-a","email":"person@invalid.test","access":"secret-access","refresh":"secret-refresh"}},
            {"provider":"ojak-claude","credential":{"type":"oauth","access":"bridge-only"}},
            {"provider":"anthropic","credential":{"type":"api_key","key":"api-key"}}
        ]});
        let initial = omp_inventory(&snapshot, &[]).unwrap();
        assert_eq!(initial.len(), 1);
        let mut old = initial[0].clone();
        old.enabled = false;
        old.buckets = omp_buckets(&report("subject-a", "workspace-a"), old.identity_key.as_deref().unwrap());
        let observed = old.buckets[0].observed_at;
        let inventory = omp_inventory(&snapshot, &[old]).unwrap();
        assert_eq!(inventory[0].email.as_deref(), Some("person@invalid.test"));
        assert_eq!(inventory[0].identity_key.as_deref(), Some("anthropic|subject:subject-a|workspace:workspace-a"));
        assert!(!inventory[0].enabled);
        assert!(!inventory[0].can_launch);
        assert_eq!(inventory[0].buckets[0].used_percent, Some(43.0));
        assert_eq!(inventory[0].buckets[0].observed_at, observed);
        let serialized = serde_json::to_string(&inventory).unwrap();
        assert!(!serialized.contains("secret-access") && !serialized.contains("secret-refresh"));
        assert!(omp_inventory(&json!({"credentials":null}), &[]).is_err());
    }

    #[test]
    fn inventory_and_usage_share_one_row_for_the_same_identity() {
        let inventory = omp_inventory(&json!({"credentials":[
            {"provider":"anthropic","credential":{"type":"oauth","accountId":"subject-a","orgId":"workspace-a","email":"person@invalid.test"}}
        ]}), &[]).unwrap();
        let id = inventory[0].id.clone();
        let mut result = ScanResult { accounts: inventory, tools: vec![], notices: vec![], merged_observation_ids: vec![] };
        add_omp_reports(&mut result, &json!({"reports":[report("subject-a","workspace-a")]}), Path::new("/native/omp"), &[]);
        assert_eq!(result.accounts.len(), 1);
        assert_eq!(result.accounts[0].id, id);
        assert_eq!(result.accounts[0].buckets[0].used_percent, Some(43.0));
    }

    #[test]
    fn unavailable_usage_with_another_workspace_keeps_both_accounts() {
        let mut result = ScanResult { accounts: vec![], tools: vec![], notices: vec![], merged_observation_ids: vec![] };
        add_omp_reports(&mut result, &json!({
            "reports":[report("subject-a", "workspace-a")],
            "accountsWithoutUsage":[{"provider":"anthropic","accountId":"subject-b","orgId":"workspace-b","email":"person@invalid.test","authorizedAt":123}]
        }), Path::new("/native/omp"), &[]);
        assert_eq!(result.accounts.len(), 2);
        let missing = result.accounts.iter().find(|account| account.identity_key.as_deref() == Some("anthropic|subject:subject-b|workspace:workspace-b")).unwrap();
        assert!(!missing.can_launch);
        assert!(missing.buckets.is_empty());
        assert_eq!(result.accounts.iter().find(|account| account.identity_key.as_deref() == Some("anthropic|subject:subject-a|workspace:workspace-a")).unwrap().buckets[0].used_percent, Some(43.0));
    }

    #[test]
    fn inventory_does_not_clear_auth_failure_without_successful_usage() {
        let snapshot = json!({"credentials":[{"provider":"anthropic","credential":{"type":"oauth","accountId":"subject-a","orgId":"workspace-a","email":"person@invalid.test"}}]});
        let mut expired = omp_inventory(&snapshot, &[]).unwrap().remove(0);
        expired.auth_status = "auth-required".into();
        expired.reason = Some("expired login".into());
        let inventory = omp_inventory(&snapshot, &[expired.clone()]).unwrap();
        assert_eq!(inventory[0].auth_status, "auth-required");
        assert_eq!(inventory[0].reason, expired.reason);
        let mut result = ScanResult { accounts: inventory, tools: vec![], notices: vec![], merged_observation_ids: vec![] };
        add_omp_reports(&mut result, &json!({"reports":[report("subject-a","workspace-a")]}), Path::new("/native/omp"), &[expired]);
        assert_eq!(result.accounts[0].auth_status, "unverified");
        assert_eq!(result.accounts[0].buckets[0].used_percent, Some(43.0));
    }


    #[test]
    fn older_omp_usage_does_not_replace_newer_official_usage() {
        let now = now_ms();
        let mut native = native_account("/profile/a");
        native.buckets = vec![QuotaBucket {
            id: "session".into(), label: "5시간".into(), model: None,
            used_percent: Some(90.0), resets_at: Some(now + 60_000),
            observed_at: now, source: quota::CLAUDE_USAGE_SOURCE.into(), status: "known".into(),
        }];
        let mut result = ScanResult { accounts: vec![native], tools: vec![], notices: vec![], merged_observation_ids: vec![] };
        let mut cached = report("subject-a", "workspace-a");
        cached["fetchedAt"] = json!(now - 60_000);
        add_omp_reports(&mut result, &json!({"reports":[cached]}), Path::new("/native/omp"), &[]);
        assert_eq!(result.accounts[0].buckets[0].used_percent, Some(90.0));
        assert_eq!(result.accounts[0].buckets[0].observed_at, now);
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
    let (from_profile, _) = native::bound_paths(source)?;
    let (to_profile, _) = native::bound_paths(target)?;
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
    let (profile, _) = native::bound_paths(account).ok()?;
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
    let (profile, _) = native::bound_paths(account)?;
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
    let (from_profile, _) = native::bound_paths(source)?;
    let (to_profile, _) = native::bound_paths(target)?;
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
