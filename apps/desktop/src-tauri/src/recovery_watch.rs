//! 계정별 한도 회복 알림(일회성 감시).
//!
//! 서비스가 계산한 `quotaSummaries`의 실제 상태만 믿는다. 시계가 리셋 시각을 지났다는 사실만으로는 알리지 않고,
//! 감시를 켠 뒤 새로 관측한 한도가 인증·검증된 계정에서 실제로 쓸 수 있다고 나와야 한다.
//! 알림 문구는 "한도 회복 확인"까지만 말하며 특정 실행이 시작된다는 약속은 하지 않는다(모델·프로젝트·동시 실행 수는 별도).
//! 저장 항목에는 계정 ID·시각·한도 항목 ID만 넣고 이메일 같은 개인정보는 넣지 않는다.
use super::{lang, provider_notice_name, read_ui_settings, write_ui_setting, Lang};
use aam_protocol::{Account, AccountQuotaSummary, ApiError, QuotaBucket, Snapshot};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};

const SETTING_KEY: &str = "recoveryWatches";
const MAX_ID_LEN: usize = 256;
/// 관측 시각이 이 정도(ms)까지 미래면 시계 오차로 보고 받아들인다. 서비스 `quota_summary`와 같은 기준이다.
const FUTURE_SKEW_MS: i64 = 30_000;

/// 저장되는 감시 한 건. 같은 계정 묶음의 어느 구성원 ID로든 걸 수 있다.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecoveryWatch {
    pub account_id: String,
    /// 감시를 켠(또는 마지막으로 다시 확인한) 시각. 회복 근거 관측은 이보다 늦어야 한다.
    pub armed_at: i64,
    /// 제한이 풀릴 것으로 알려진 가장 늦은 시각. 한도 항목으로 설명되지 않는 제한(브릿지 응답 보류)은 이 시각 이후 관측이 필요하다.
    #[serde(default)]
    pub until: Option<i64>,
    /// 제한 중 관측한 한도 항목 ID. 각각이 감시 이후 새로 관측돼 다시 쓸 수 있어야 한다.
    #[serde(default)]
    pub pending: Vec<String>,
}

/// 감시 중 상태가 제한 쪽인지. 크레딧·추가 사용량은 구독 한도를 다 쓴 뒤의 대체 사용이라 여전히 제한 상태다.
fn is_limited(kind: &str) -> bool {
    matches!(kind, "resting" | "partial" | "credits" | "extra")
}

/// 보수적으로 `available`·`reserve`(남은 한도가 안전 여유량 안쪽이지만 쓸 수 있음)만 회복으로 본다.
fn is_recovered(kind: &str) -> bool {
    matches!(kind, "available" | "reserve")
}

fn stale_ms(snapshot: &Snapshot) -> i64 {
    snapshot.policy.stale_after_seconds.saturating_mul(1000).min(i64::MAX as u64) as i64
}

/// 서비스 `quota_summary::state`와 같은 기준으로 한도 항목 하나를 known/exhausted/unknown으로 나눈다.
fn bucket_state(bucket: &QuotaBucket, stale_ms: i64, now: i64) -> &'static str {
    if bucket.status == "stale" {
        return "unknown";
    }
    if !matches!(bucket.status.as_str(), "known" | "exhausted")
        || bucket.observed_at <= 0
        || bucket.observed_at > now.saturating_add(FUTURE_SKEW_MS)
        || now.saturating_sub(bucket.observed_at) > stale_ms
        || bucket.resets_at.is_some_and(|reset| reset <= now)
    {
        return "unknown";
    }
    if bucket.status == "exhausted" {
        return "exhausted";
    }
    match bucket.used_percent {
        Some(used) if used.is_finite() && (0.0..=100.0).contains(&used) => {
            if used >= 100.0 { "exhausted" } else { "known" }
        }
        _ => "unknown",
    }
}

fn summary_for<'a>(snapshot: &'a Snapshot, account_id: &str) -> Option<&'a AccountQuotaSummary> {
    snapshot.quota_summaries.iter().find(|summary| summary.account_ids.iter().any(|id| id == account_id))
}

fn members<'a>(snapshot: &'a Snapshot, summary: &AccountQuotaSummary) -> Vec<&'a Account> {
    snapshot.accounts.iter().filter(|account| summary.account_ids.contains(&account.id)).collect()
}

/// 같은 한도 항목은 가장 최근 관측이 이긴다(서비스 묶음 요약과 같은 규칙).
fn newest_buckets<'a>(members: &[&'a Account]) -> BTreeMap<&'a str, &'a QuotaBucket> {
    let mut newest: BTreeMap<&str, &QuotaBucket> = BTreeMap::new();
    for account in members {
        for bucket in &account.buckets {
            let entry = newest.entry(bucket.id.as_str()).or_insert(bucket);
            if bucket.observed_at > entry.observed_at {
                *entry = bucket;
            }
        }
    }
    newest
}

/// 지금 제한을 만드는 한도 항목 ID: 소진된 항목과, 쓸 수 있다고 확인되지 않은 모델별 항목.
fn limited_bucket_ids(members: &[&Account], stale_ms: i64, now: i64) -> BTreeSet<String> {
    newest_buckets(members)
        .into_iter()
        .filter(|(_, bucket)| {
            let state = bucket_state(bucket, stale_ms, now);
            state == "exhausted" || (bucket.model.is_some() && state != "known")
        })
        .map(|(id, _)| id.to_owned())
        .collect()
}

/// 알림의 근거가 될 수 있는 구성원: 켜져 있고 인증됐고 실행 전 검증을 통과한 실제 계정 식별자를 가진 계정.
fn is_qualifying(account: &Account) -> bool {
    account.enabled
        && account.auth_status == "authenticated"
        && account.verification == "preflight-verified"
        && account.can_launch
        && account.identity_key.as_deref().is_some_and(|key| !key.is_empty())
}

pub(crate) fn validate_id(account_id: &str) -> Result<(), ApiError> {
    if account_id.is_empty() || account_id.len() > MAX_ID_LEN || account_id.chars().any(char::is_control) {
        return Err(ApiError::new("INVALID_PARAMS", "계정 ID가 올바르지 않습니다."));
    }
    Ok(())
}

fn ensure_account(snapshot: &Snapshot, account_id: &str) -> Result<(), ApiError> {
    if snapshot.accounts.iter().any(|account| account.id == account_id) {
        Ok(())
    } else {
        Err(ApiError::new("ACCOUNT_NOT_FOUND", "등록된 계정을 찾지 못했습니다."))
    }
}

/// 실제 현재 상태가 제한일 때만 감시를 만든다. 가짜 감시를 만들지 않도록 아닌 경우는 이유와 함께 거절한다.
fn arm(snapshot: &Snapshot, account_id: &str, now: i64) -> Result<RecoveryWatch, ApiError> {
    ensure_account(snapshot, account_id)?;
    let summary = summary_for(snapshot, account_id).ok_or_else(|| {
        ApiError::new("RECOVERY_WATCH_UNSUPPORTED", "서비스가 이 계정의 한도 요약을 제공하지 않아 회복을 감시할 수 없습니다.")
    })?;
    if is_recovered(&summary.kind) {
        return Err(ApiError::new("RECOVERY_WATCH_NOT_LIMITED", "지금 쓸 수 있는 한도가 있어 회복을 기다릴 필요가 없습니다."));
    }
    if !is_limited(&summary.kind) {
        return Err(ApiError::new(
            "RECOVERY_WATCH_UNSUPPORTED",
            match summary.kind.as_str() {
                "login" => "로그인이 필요한 계정은 한도 회복을 감시할 수 없습니다.",
                "excluded" => "배정에서 제외된 계정은 한도 회복을 감시할 수 없습니다.",
                _ => "한도 상태를 확인하지 못한 계정은 회복을 감시할 수 없습니다.",
            },
        ));
    }
    let group = members(snapshot, summary);
    if !group.iter().any(|account| is_qualifying(account)) {
        return Err(ApiError::new(
            "RECOVERY_WATCH_UNSUPPORTED",
            "인증·검증된 계정이 없는 묶음은 한도 회복을 확인할 수 없어 감시할 수 없습니다.",
        ));
    }
    Ok(RecoveryWatch {
        account_id: account_id.to_owned(),
        armed_at: now,
        until: summary.until,
        pending: limited_bucket_ids(&group, stale_ms(snapshot), now).into_iter().collect(),
    })
}

/// 감시를 켜거나 끈다. 켤 때는 같은 묶음에 이미 감시가 있으면 그대로 두고, 끌 때는 묶음 전체의 감시를 지운다.
pub(crate) fn toggle(
    mut watches: Vec<RecoveryWatch>,
    snapshot: Option<&Snapshot>,
    account_id: &str,
    enabled: bool,
    now: i64,
) -> Result<Vec<RecoveryWatch>, ApiError> {
    validate_id(account_id)?;
    let group: Vec<String> = snapshot
        .and_then(|snapshot| summary_for(snapshot, account_id))
        .map_or_else(|| vec![account_id.to_owned()], |summary| summary.account_ids.clone());
    if !enabled {
        watches.retain(|watch| watch.account_id != account_id && !group.contains(&watch.account_id));
        return Ok(watches);
    }
    let snapshot =
        snapshot.ok_or_else(|| ApiError::new("DAEMON_UNAVAILABLE", "서비스 상태를 확인하지 못해 감시를 켤 수 없습니다."))?;
    ensure_account(snapshot, account_id)?;
    if watches.iter().any(|watch| group.contains(&watch.account_id)) {
        return Ok(watches);
    }
    watches.push(arm(snapshot, account_id, now)?);
    Ok(watches)
}

enum Verdict {
    Remove,
    Keep(RecoveryWatch),
    Recovered { group: Vec<String>, provider: String },
}

/// 감시 하나를 현재 스냅샷으로 판정한다.
fn evaluate(watch: &RecoveryWatch, snapshot: &Snapshot, now: i64) -> Verdict {
    // 계정이 삭제됐으면 감시를 지운다. 계정 목록이 통째로 비었으면 일시적 응답일 수 있어 건드리지 않는다.
    if !snapshot.accounts.is_empty() && !snapshot.accounts.iter().any(|account| account.id == watch.account_id) {
        return Verdict::Remove;
    }
    let Some(summary) = summary_for(snapshot, &watch.account_id) else { return Verdict::Keep(watch.clone()) };
    let group = members(snapshot, summary);
    let stale = stale_ms(snapshot);
    if is_limited(&summary.kind) {
        // 아직 제한 중이면 새로 드러난 제한 근거를 이어 받아, 나중에 시계만 지난 경우를 회복으로 오인하지 않게 한다.
        let mut next = watch.clone();
        next.until = watch.until.max(summary.until);
        let mut pending: BTreeSet<String> = watch.pending.iter().cloned().collect();
        pending.extend(limited_bucket_ids(&group, stale, now));
        next.pending = pending.into_iter().collect();
        return Verdict::Keep(next);
    }
    // 인증·검증된 켜진 구성원이 없으면(로그아웃·꺼짐·검증 상실) 요약이 회복으로 보여도 알리지 않는다.
    if !is_recovered(&summary.kind) || !group.iter().any(|account| is_qualifying(account)) || !recovery_evidenced(watch, &group, stale, now) {
        return Verdict::Keep(watch.clone());
    }
    let provider = group
        .first()
        .map_or_else(String::new, |account| aam_protocol::pin_provider(&account.provider).to_owned());
    Verdict::Recovered { group: summary.account_ids.clone(), provider }
}

/// 요약이 회복으로 보여도 감시 이후의 새 관측이 뒷받침할 때만 인정한다.
fn recovery_evidenced(watch: &RecoveryWatch, group: &[&Account], stale: i64, now: i64) -> bool {
    let newest = newest_buckets(group);
    // 제한 중이던 한도 항목은 감시 이후에 다시 관측돼 지금 쓸 수 있어야 한다. 관측에서 사라진 항목은 확인할 수 없으므로 기다린다.
    for id in &watch.pending {
        match newest.get(id.as_str()) {
            Some(bucket) if bucket_state(bucket, stale, now) == "known" && bucket.observed_at > watch.armed_at => {}
            _ => return false,
        }
    }
    // 한도 항목으로 설명되지 않는 제한(브릿지 응답 보류)은 풀릴 시각 이후의 관측이 있어야 한다.
    let floor = if watch.pending.is_empty() { watch.armed_at.max(watch.until.unwrap_or(0)) } else { watch.armed_at };
    newest.values().any(|bucket| bucket_state(bucket, stale, now) == "known" && bucket.observed_at > floor)
}

/// 감시 목록을 한 번 진행한다. 반환값은 (남은 감시, 알림을 보내 지운 계정 ID).
/// `notify`가 false를 돌려주면 그 감시는 그대로 남아 다음 확인 때 다시 시도한다.
fn step(
    watches: Vec<RecoveryWatch>,
    snapshot: &Snapshot,
    now: i64,
    notify: &mut dyn FnMut(&str) -> bool,
) -> (Vec<RecoveryWatch>, Vec<String>) {
    let mut next = Vec::with_capacity(watches.len());
    let mut fired_ids = Vec::new();
    let mut fired_groups: Vec<Vec<String>> = Vec::new();
    let mut failed_groups: Vec<Vec<String>> = Vec::new();
    for watch in watches {
        if fired_groups.iter().any(|group| group.contains(&watch.account_id)) {
            fired_ids.push(watch.account_id);
            continue;
        }
        if failed_groups.iter().any(|group| group.contains(&watch.account_id)) {
            next.push(watch);
            continue;
        }
        match evaluate(&watch, snapshot, now) {
            Verdict::Remove => {}
            Verdict::Keep(updated) => next.push(updated),
            Verdict::Recovered { group, provider } => {
                if notify(&provider) {
                    fired_ids.push(watch.account_id);
                    fired_groups.push(group);
                } else {
                    failed_groups.push(group);
                    next.push(watch);
                }
            }
        }
    }
    // 앞에서 이미 남겨 둔 같은 묶음의 감시도 한 번의 알림으로 함께 끝낸다.
    next.retain(|watch| {
        let done = fired_groups.iter().any(|group| group.contains(&watch.account_id));
        if done {
            fired_ids.push(watch.account_id.clone());
        }
        !done
    });
    (next, fired_ids)
}

/// 저장값에서 감시를 읽는다. 형식이 틀린 항목과 같은 ID 중복은 버린다.
fn parse(value: Option<&Value>) -> Vec<RecoveryWatch> {
    let mut seen = BTreeSet::new();
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| serde_json::from_value::<RecoveryWatch>(item.clone()).ok())
        .filter(|watch| validate_id(&watch.account_id).is_ok() && watch.armed_at > 0)
        .filter(|watch| seen.insert(watch.account_id.clone()))
        .collect()
}

fn encode(watches: &[RecoveryWatch]) -> Value {
    serde_json::to_value(watches).unwrap_or_else(|_| Value::Array(Vec::new()))
}

/// 알림은 보냈지만 저장이 실패해 아직 지우지 못한 계정 ID. 같은 알림을 반복하지 않도록 읽을 때 걸러낸다.
/// 이 잠금이 저장소 읽기·쓰기 전체(트레이 폴링과 명령)를 직렬화한다.
static STATE: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn lock() -> MutexGuard<'static, Vec<String>> {
    STATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn read_all(unsaved_removals: &[String]) -> Vec<RecoveryWatch> {
    let mut watches = parse(read_ui_settings().get(SETTING_KEY));
    watches.retain(|watch| !unsaved_removals.contains(&watch.account_id));
    watches
}

fn write_all(watches: &[RecoveryWatch]) -> Result<(), ApiError> {
    write_ui_setting(SETTING_KEY, encode(watches))
}

/// 감시 중인 계정 ID 목록(저장된 그대로, 묶음의 어느 구성원일 수 있다).
pub(crate) fn ids() -> Vec<String> {
    let unsaved = lock();
    read_all(&unsaved).into_iter().map(|watch| watch.account_id).collect()
}

/// `set_recovery_watch`의 저장 부분. 켤 때는 `snapshot`이 필요하고, 끌 때는 없어도 ID 자체는 지운다.
pub(crate) fn update(
    account_id: &str,
    enabled: bool,
    snapshot: Option<&Snapshot>,
    now: i64,
) -> Result<Vec<String>, ApiError> {
    let mut unsaved = lock();
    let current = read_all(&unsaved);
    let next = toggle(current.clone(), snapshot, account_id, enabled, now)?;
    if next != current || !unsaved.is_empty() {
        write_all(&next)?;
        unsaved.clear();
    }
    Ok(next.into_iter().map(|watch| watch.account_id).collect())
}

/// 트레이 폴링마다 부른다. 회복이 확인된 감시마다 `notify(공급자)`를 호출하고, 성공했을 때만 그 감시를 지운다.
pub(crate) fn run_check(snapshot: &Snapshot, now: i64, notify: &mut dyn FnMut(&str) -> bool) {
    let mut unsaved = lock();
    let stored = read_all(&unsaved);
    if stored.is_empty() && unsaved.is_empty() {
        return;
    }
    let (next, fired) = step(stored.clone(), snapshot, now, notify);
    if next == stored && unsaved.is_empty() {
        return;
    }
    if write_all(&next).is_ok() {
        unsaved.clear();
    } else {
        unsaved.extend(fired);
    }
}

/// 알림 문구. 공급자 이름만 밝히고 "회복 확인"까지만 말한다.
pub(crate) fn message(provider: &str) -> (&'static str, String) {
    let name = provider_notice_name(provider);
    match lang() {
        Lang::Ko => (
            "한도 회복 확인",
            format!("{name} 계정의 사용 한도가 회복된 것으로 확인됐어요. 모델·프로젝트별 제한과 동시 실행 수에 따라 실제 실행 여부는 달라질 수 있어요."),
        ),
        Lang::Id => (
            "Pemulihan batas terkonfirmasi",
            format!("Batas pemakaian akun {name} terlihat sudah pulih. Apakah sesi tertentu bisa dimulai tetap bergantung pada batas model, proyek, dan jumlah sesi bersamaan."),
        ),
        Lang::En => (
            "Limit recovery confirmed",
            format!("A {name} account's usage limit now shows as recovered. Whether a specific run starts still depends on model, project, and concurrency limits."),
        ),
    }
}

/// 시스템 알림 권한을 확인한다. 거부됐거나 확인할 수 없으면 오류로 알려 가짜 구독 성공을 만들지 않는다.
pub(crate) fn ensure_notification_permission(app: &tauri::AppHandle) -> Result<(), ApiError> {
    use tauri_plugin_notification::{NotificationExt, PermissionState};
    let unavailable = || {
        ApiError::new(
            "NOTIFICATION_PERMISSION_UNAVAILABLE",
            "알림 권한을 확인하지 못했습니다. 시스템 설정에서 Ojak 알림을 허용했는지 확인해 주세요.",
        )
    };
    let notification = app.notification();
    let mut state = notification.permission_state().map_err(|_| unavailable())?;
    if matches!(state, PermissionState::Prompt | PermissionState::PromptWithRationale) {
        state = notification.request_permission().map_err(|_| unavailable())?;
    }
    if state == PermissionState::Granted {
        Ok(())
    } else {
        Err(ApiError::new(
            "NOTIFICATION_PERMISSION_DENIED",
            "Ojak 알림이 꺼져 있어 한도 회복을 알려 드릴 수 없습니다. 시스템 설정에서 Ojak 알림을 허용한 뒤 다시 켜 주세요.",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aam_protocol::Policy;
    use serde_json::json;

    const NOW: i64 = 1_000_000_000_000;
    const MIN: i64 = 60_000;

    fn account(id: &str, qualifying: bool, buckets: Vec<QuotaBucket>) -> Account {
        Account {
            id: id.into(),
            provider: "anthropic".into(),
            tool: "claude".into(),
            email: Some(format!("{id}@example.test")),
            identity_key: Some(format!("anthropic|subject:{id}|workspace:w")),
            auth_status: if qualifying { "authenticated" } else { "unverified" }.into(),
            verification: if qualifying { "preflight-verified" } else { "observed" }.into(),
            can_launch: qualifying,
            enabled: true,
            max_concurrency: 1,
            buckets,
            ..Account::default()
        }
    }

    fn bucket(id: &str, model: Option<&str>, status: &str, used: f64, resets_at: Option<i64>, observed_at: i64) -> QuotaBucket {
        QuotaBucket {
            id: id.into(),
            label: id.into(),
            model: model.map(str::to_owned),
            used_percent: Some(used),
            resets_at,
            observed_at,
            source: "test".into(),
            status: status.into(),
        }
    }

    fn summary(ids: &[&str], kind: &str, until: Option<i64>) -> AccountQuotaSummary {
        AccountQuotaSummary {
            account_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            kind: kind.into(),
            until,
            models: Vec::new(),
            label: None,
            rate: false,
            expiring: None,
            credits: None,
            extra_usage: None,
        }
    }

    fn snapshot(accounts: Vec<Account>, quota_summaries: Vec<AccountQuotaSummary>) -> Snapshot {
        Snapshot {
            version: aam_protocol::PROTOCOL_VERSION,
            generated_at: NOW,
            service_started_at: NOW - 3_600_000,
            accounts,
            tools: Vec::new(),
            sessions: Vec::new(),
            observed_sessions: Vec::new(),
            policy: Policy::default(),
            notices: Vec::new(),
            refreshing: false,
            last_refresh_at: None,
            takeovers: Vec::new(),
            quota_summaries,
            service_version: None,
        }
    }

    /// 주간 한도를 다 쓴 계정 한 개짜리 스냅샷(리셋은 NOW+60분).
    fn resting() -> Snapshot {
        snapshot(
            vec![account("a", true, vec![bucket("weekly", None, "exhausted", 100.0, Some(NOW + 60 * MIN), NOW - MIN)])],
            vec![summary(&["a"], "resting", Some(NOW + 60 * MIN))],
        )
    }

    fn watch() -> RecoveryWatch {
        RecoveryWatch { account_id: "a".into(), armed_at: NOW, until: Some(NOW + 60 * MIN), pending: vec!["weekly".into()] }
    }

    fn run(watches: Vec<RecoveryWatch>, snapshot: &Snapshot, now: i64, delivered: bool) -> (Vec<RecoveryWatch>, Vec<String>, usize) {
        let mut sent = 0;
        let (next, fired) = step(watches, snapshot, now, &mut |_| {
            sent += 1;
            delivered
        });
        (next, fired, sent)
    }

    fn code(result: Result<Vec<RecoveryWatch>, ApiError>) -> String {
        result.unwrap_err().code
    }

    #[test]
    fn arms_only_from_a_real_limited_state() {
        let limited = resting();
        let armed = toggle(Vec::new(), Some(&limited), "a", true, NOW).unwrap();
        assert_eq!(armed, vec![watch()]);

        for (kind, expected) in [
            ("available", "RECOVERY_WATCH_NOT_LIMITED"),
            ("reserve", "RECOVERY_WATCH_NOT_LIMITED"),
            ("login", "RECOVERY_WATCH_UNSUPPORTED"),
            ("excluded", "RECOVERY_WATCH_UNSUPPORTED"),
            ("unknown", "RECOVERY_WATCH_UNSUPPORTED"),
        ] {
            let mut snapshot = resting();
            snapshot.quota_summaries[0].kind = kind.into();
            assert_eq!(code(toggle(Vec::new(), Some(&snapshot), "a", true, NOW)), expected, "{kind}");
        }
        for kind in ["partial", "credits", "extra"] {
            let mut snapshot = resting();
            snapshot.quota_summaries[0].kind = kind.into();
            assert!(toggle(Vec::new(), Some(&snapshot), "a", true, NOW).is_ok(), "{kind}");
        }
        assert_eq!(code(toggle(Vec::new(), Some(&limited), "missing", true, NOW)), "ACCOUNT_NOT_FOUND");
        assert_eq!(code(toggle(Vec::new(), Some(&limited), "", true, NOW)), "INVALID_PARAMS");
        assert_eq!(code(toggle(Vec::new(), Some(&limited), &"x".repeat(MAX_ID_LEN + 1), true, NOW)), "INVALID_PARAMS");
        assert_eq!(code(toggle(Vec::new(), None, "a", true, NOW)), "DAEMON_UNAVAILABLE");

        // 인증·검증된 구성원이 없는 묶음은 회복을 확인할 수 없어 거절한다.
        let observed_only = snapshot_with(vec![account("a", false, Vec::new())]);
        assert_eq!(code(toggle(Vec::new(), Some(&observed_only), "a", true, NOW)), "RECOVERY_WATCH_UNSUPPORTED");
        // 요약이 없는 서비스(이전 버전)도 거절한다.
        let mut old_service = resting();
        old_service.quota_summaries.clear();
        assert_eq!(code(toggle(Vec::new(), Some(&old_service), "a", true, NOW)), "RECOVERY_WATCH_UNSUPPORTED");
    }

    fn snapshot_with(accounts: Vec<Account>) -> Snapshot {
        snapshot(accounts, vec![summary(&["a"], "resting", Some(NOW + MIN))])
    }

    #[test]
    fn clock_expiry_alone_never_notifies() {
        // 리셋 시각이 지났지만 새 관측이 없으면 서비스 요약은 unknown이다.
        let later = NOW + 61 * MIN;
        let mut expired = resting();
        expired.quota_summaries[0].kind = "unknown".into();
        let (next, fired, sent) = run(vec![watch()], &expired, later, true);
        assert_eq!((next.len(), fired.len(), sent), (1, 0, 0));

        // 요약이 available이라도 제한 중이던 항목을 감시 이후 다시 관측하지 못했으면 알리지 않는다.
        let mut unobserved = resting();
        unobserved.quota_summaries[0].kind = "available".into();
        unobserved.accounts[0].buckets[0] = bucket("weekly", None, "known", 10.0, Some(later + 7 * 24 * 60 * MIN), NOW - MIN);
        assert_eq!(run(vec![watch()], &unobserved, later, true).2, 0);

        // 오래된 관측(신선도 창 밖)이나 stale 표시도 회복 근거가 아니다.
        let mut stale = unobserved.clone();
        stale.accounts[0].buckets[0] = bucket("weekly", None, "known", 10.0, None, later - 16 * MIN);
        assert_eq!(run(vec![watch()], &stale, later, true).2, 0);
        stale.accounts[0].buckets[0] = bucket("weekly", None, "stale", 10.0, None, later);
        assert_eq!(run(vec![watch()], &stale, later, true).2, 0);

        // 오류·미확인(숫자 없음)도 마찬가지다.
        let mut broken = unobserved.clone();
        broken.accounts[0].buckets[0] = bucket("weekly", None, "error", 0.0, None, later);
        broken.accounts[0].buckets[0].used_percent = None;
        assert_eq!(run(vec![watch()], &broken, later, true).2, 0);
    }

    #[test]
    fn fresh_authenticated_recovery_notifies_once_and_clears() {
        let later = NOW + 61 * MIN;
        let mut recovered = resting();
        recovered.quota_summaries[0].kind = "available".into();
        recovered.quota_summaries[0].until = None;
        recovered.accounts[0].buckets[0] = bucket("weekly", None, "known", 5.0, Some(later + 7 * 24 * 60 * MIN), later - MIN);
        let mut providers = Vec::new();
        let (next, fired) = step(vec![watch()], &recovered, later, &mut |provider| {
            providers.push(provider.to_owned());
            true
        });
        assert!(next.is_empty());
        assert_eq!(fired, vec!["a".to_owned()]);
        assert_eq!(providers, vec!["anthropic".to_owned()]);
        // 이미 지워졌으므로 다음 확인에서는 다시 알리지 않는다.
        assert_eq!(run(next, &recovered, later + MIN, true).2, 0);
    }

    #[test]
    fn reserve_counts_as_recovered_but_unverified_evidence_does_not() {
        let later = NOW + 61 * MIN;
        let mut reserve = resting();
        reserve.quota_summaries[0].kind = "reserve".into();
        reserve.accounts[0].buckets[0] = bucket("weekly", None, "known", 97.0, Some(later + 7 * 24 * 60 * MIN), later - MIN);
        assert_eq!(run(vec![watch()], &reserve, later, true).2, 1);

        // 근거 관측이 인증·검증되지 않은 구성원에서만 왔다면 알리지 않는다.
        let mut unverified = reserve.clone();
        unverified.accounts[0] = account("a", false, vec![bucket("weekly", None, "known", 5.0, None, later - MIN)]);
        assert_eq!(run(vec![watch()], &unverified, later, true).2, 0);
        // 꺼진 계정도 마찬가지다.
        let mut disabled = reserve.clone();
        disabled.accounts[0].enabled = false;
        assert_eq!(run(vec![watch()], &disabled, later, true).2, 0);
    }

    #[test]
    fn failed_delivery_is_kept_and_retried() {
        let later = NOW + 61 * MIN;
        let mut recovered = resting();
        recovered.quota_summaries[0].kind = "available".into();
        recovered.accounts[0].buckets[0] = bucket("weekly", None, "known", 5.0, None, later - MIN);
        let (next, fired, sent) = run(vec![watch()], &recovered, later, false);
        assert_eq!((next.clone(), fired.len(), sent), (vec![watch()], 0, 1));
        let (next, fired, sent) = run(next, &recovered, later + MIN, true);
        assert!(next.is_empty());
        assert_eq!((fired.len(), sent), (1, 1));
    }

    #[test]
    fn partial_watch_waits_until_the_watched_restriction_clears() {
        let restricted = bucket("fable-weekly", Some("fable"), "exhausted", 100.0, Some(NOW + 30 * MIN), NOW - MIN);
        let open = bucket("weekly", None, "known", 10.0, None, NOW - MIN);
        let mut snapshot = snapshot(
            vec![account("a", true, vec![open.clone(), restricted])],
            vec![summary(&["a"], "partial", None)],
        );
        snapshot.quota_summaries[0].models = vec!["Fable".into()];
        let armed = toggle(Vec::new(), Some(&snapshot), "a", true, NOW).unwrap();
        assert_eq!(armed[0].pending, vec!["fable-weekly".to_owned()]);

        // 여전히 partial이면 다른 한도가 새로 관측돼도 알리지 않는다.
        let later = NOW + 14 * MIN;
        snapshot.accounts[0].buckets[0] = bucket("weekly", None, "known", 11.0, None, later - MIN);
        snapshot.accounts[0].buckets[1] = bucket("fable-weekly", Some("fable"), "exhausted", 100.0, Some(later + 60 * MIN), later - MIN);
        let (next, _, sent) = run(armed.clone(), &snapshot, later, true);
        assert_eq!((next.len(), sent), (1, 0));

        // 요약이 available로 돌아왔어도 제한 모델 항목이 새로 관측되지 않았다면 알리지 않는다.
        snapshot.quota_summaries[0].kind = "available".into();
        snapshot.accounts[0].buckets[1] = bucket("fable-weekly", Some("fable"), "known", 0.0, None, NOW - MIN);
        assert_eq!(run(armed.clone(), &snapshot, later, true).2, 0);

        // 제한 모델 항목이 감시 이후 다시 관측돼 쓸 수 있을 때만 알린다.
        snapshot.accounts[0].buckets[1] = bucket("fable-weekly", Some("fable"), "known", 0.0, None, later - MIN);
        assert_eq!(run(armed, &snapshot, later, true).2, 1);
    }

    #[test]
    fn response_hold_without_bucket_evidence_needs_an_observation_after_it_lifts() {
        // 한도 항목 없이 브릿지 응답 보류(속도 제한)만 있는 묶음.
        let open = bucket("weekly", None, "known", 10.0, None, NOW - MIN);
        let mut held = snapshot(vec![account("a", true, vec![open])], vec![summary(&["a"], "resting", Some(NOW + 5 * MIN))]);
        held.quota_summaries[0].rate = true;
        let armed = toggle(Vec::new(), Some(&held), "a", true, NOW).unwrap();
        assert!(armed[0].pending.is_empty());
        assert_eq!(armed[0].until, Some(NOW + 5 * MIN));

        // 보류가 시계로 끝났지만 그 뒤 관측이 없다면 알리지 않는다.
        let after = NOW + 6 * MIN;
        held.quota_summaries[0].kind = "available".into();
        held.accounts[0].buckets[0] = bucket("weekly", None, "known", 10.0, None, NOW + 2 * MIN);
        assert_eq!(run(armed.clone(), &held, after, true).2, 0);
        // 보류가 풀린 뒤의 새 관측이 있으면 알린다.
        held.accounts[0].buckets[0] = bucket("weekly", None, "known", 10.0, None, NOW + 5 * MIN + 1);
        assert_eq!(run(armed, &held, after, true).2, 1);
    }

    #[test]
    fn limit_extension_is_carried_while_still_limited() {
        let mut extended = resting();
        extended.quota_summaries[0].until = Some(NOW + 120 * MIN);
        extended.accounts[0].buckets.push(bucket("daily", None, "exhausted", 100.0, Some(NOW + 120 * MIN), NOW));
        let (next, fired, sent) = run(vec![watch()], &extended, NOW + MIN, true);
        assert_eq!((fired.len(), sent), (0, 0));
        assert_eq!(next[0].until, Some(NOW + 120 * MIN));
        assert_eq!(next[0].pending, vec!["daily".to_owned(), "weekly".to_owned()]);
        assert_eq!(next[0].armed_at, NOW);
    }

    #[test]
    fn removed_accounts_drop_their_watch_but_an_empty_snapshot_does_not() {
        let mut gone = resting();
        gone.accounts.clear();
        gone.quota_summaries.clear();
        let (next, _, sent) = run(vec![watch()], &gone, NOW + MIN, true);
        assert_eq!((next.len(), sent), (1, 0), "빈 계정 목록은 일시적 응답일 수 있다");
        gone.accounts.push(account("other", true, Vec::new()));
        let (next, _, sent) = run(vec![watch()], &gone, NOW + MIN, true);
        assert_eq!((next.len(), sent), (0, 0));
    }

    #[test]
    fn group_members_share_one_watch_one_notification_and_one_cancel() {
        let later = NOW + 61 * MIN;
        let mut grouped = snapshot(
            vec![
                account("a", true, vec![bucket("weekly", None, "exhausted", 100.0, Some(NOW + 60 * MIN), NOW - MIN)]),
                account("b", true, Vec::new()),
            ],
            vec![summary(&["a", "b"], "resting", Some(NOW + 60 * MIN))],
        );
        // 같은 묶음의 다른 구성원 ID로 켜도 중복 감시가 생기지 않는다.
        let first = toggle(Vec::new(), Some(&grouped), "a", true, NOW).unwrap();
        assert_eq!(toggle(first.clone(), Some(&grouped), "b", true, NOW).unwrap(), first);

        // 구성원 ID 두 개가 따로 저장돼 있어도 알림은 한 번이고 둘 다 지워진다.
        let both = vec![watch(), RecoveryWatch { account_id: "b".into(), ..watch() }];
        grouped.quota_summaries[0].kind = "available".into();
        grouped.accounts[0].buckets[0] = bucket("weekly", None, "known", 5.0, None, later - MIN);
        let (next, fired, sent) = run(both.clone(), &grouped, later, true);
        assert!(next.is_empty());
        assert_eq!((fired.len(), sent), (2, 1));
        // 실패하면 둘 다 남고 시도도 한 번뿐이다.
        let (next, fired, sent) = run(both, &grouped, later, false);
        assert_eq!((next.len(), fired.len(), sent), (2, 0, 1));

        // 어느 구성원 ID로 끄든 묶음 전체가 해제된다. 서비스가 없어도 그 ID 자체는 지운다.
        grouped.quota_summaries[0].kind = "resting".into();
        assert!(toggle(first.clone(), Some(&grouped), "b", false, NOW).unwrap().is_empty());
        assert!(toggle(first, None, "a", false, NOW).unwrap().is_empty());
    }

    #[test]
    fn watches_survive_a_restart_through_the_stored_json() {
        let stored = encode(&[watch(), RecoveryWatch { account_id: "b".into(), armed_at: NOW + 1, until: None, pending: Vec::new() }]);
        // 이메일 같은 개인정보는 저장하지 않는다.
        assert!(!stored.to_string().contains('@'));
        let restored = parse(Some(&stored));
        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0], watch());
        // 다시 켠 앱이 같은 감시로 회복을 판정한다.
        let later = NOW + 61 * MIN;
        let mut recovered = resting();
        recovered.quota_summaries[0].kind = "available".into();
        recovered.accounts[0].buckets[0] = bucket("weekly", None, "known", 5.0, None, later - MIN);
        assert_eq!(run(restored, &recovered, later, true).2, 1);

        // 깨진 항목·중복·잘못된 ID는 버린다.
        let dirty = json!([
            {"accountId": "a", "armedAt": NOW},
            {"accountId": "a", "armedAt": NOW + 5},
            {"accountId": "", "armedAt": NOW},
            {"accountId": "c", "armedAt": 0},
            {"accountId": "d"},
            "e",
            42,
        ]);
        let cleaned = parse(Some(&dirty));
        assert_eq!(cleaned.len(), 1);
        assert_eq!((cleaned[0].account_id.as_str(), cleaned[0].armed_at), ("a", NOW));
        assert!(parse(None).is_empty());
        assert!(parse(Some(&json!({"accountId": "a"}))).is_empty());
    }

    #[test]
    fn notification_text_stays_within_what_the_summary_proves() {
        for provider in ["anthropic", "openai", "google", "xai", ""] {
            let (title, body) = message(provider);
            assert!(!title.is_empty());
            assert!(!body.contains('@'));
            assert!(!body.to_lowercase().contains("all slots"));
        }
        assert_eq!(provider_notice_name("anthropic"), "Claude");
    }
}
