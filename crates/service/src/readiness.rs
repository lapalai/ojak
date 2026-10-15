//! `lease.acquire` 직전에 조회를 시작·기다릴지 판단한다. 판단만 하고 배정 정책은 건드리지 않는다.
//! "지금 이 요청을 받을 수 있는 계정이 있는가"는 서비스가 `scheduler::decide_with`로 구해 `selectable`로 넘긴다.
//! 여기서는 그 결과가 새 조회로 바뀔 수 있는지만 본다.
use aam_protocol::{Account, Policy, QuotaBucket};

/// 계정이 없거나 한도를 모르는 상태에서 실행이 조회를 다시 시작하기까지의 최소 간격.
/// 마지막 전체 조회 시각이 아니라 마지막 배정 요청이 시작한 조회 시각을 기준으로 해서, 방금 끝난 조회 뒤의 첫 실행도 복구를 시도한다.
pub(crate) const PROBE_COOLDOWN_MS: i64 = 15_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RefreshPlan {
    /// 조회를 시작한다(이미 돌고 있으면 서비스가 합친다).
    pub start: bool,
    /// 이 시작이 계정·한도 미확인 때문에 배정 요청이 일으킨 것이다. 서비스가 시각을 기록해 반복을 막는다.
    pub probe: bool,
    /// 이 요청을 받을 수 있는 계정이 갖춰질 때까지(또는 조회가 끝날 때까지) 기다린다.
    pub wait: bool,
}

fn needs_refresh(bucket: &QuotaBucket, now: i64, freshness: i64) -> bool {
    !matches!(bucket.status.as_str(), "known" | "exhausted")
        || bucket.observed_at <= 0
        || now.saturating_sub(bucket.observed_at) > freshness
        || bucket.resets_at.is_some_and(|reset| reset <= now)
}

/// 이 요청의 결과를 바꿀 수 있는 계정: 같은 도구, 계정이 고정(지정·이어 가기)됐다면 그 실제 계정만.
fn relevant<'a>(accounts: &'a [Account], tool: &'a str, pin: Option<&'a str>) -> impl Iterator<Item = &'a Account> {
    let pinned = pin.and_then(|id| accounts.iter().find(|account| account.id == id));
    accounts
        .iter()
        .filter(move |account| account.tool == tool)
        .filter(move |account| pin.is_none() || pinned.is_some_and(|pin| crate::scheduler::same_identity(pin, account)))
}

/// 새 조회가 있어야 이 계정을 쓸 수 있는가. 새 배정에서 뺀 계정은 조회해도 달라지지 않는다.
fn pending(account: &Account, now: i64, freshness: i64) -> bool {
    account.enabled
        && (!account.can_launch
            || account.auth_status != "authenticated"
            || account.buckets.is_empty()
            || account.buckets.iter().any(|bucket| needs_refresh(bucket, now, freshness)))
}

/// `selectable`: 스케줄러가 이 요청에 계정을 골랐는가. `None`이면 판단할 수 없다(잘못된 경로·정책 오류).
/// 이때는 기다리지 않고 배정이 정확한 오류를 돌려주게 한다. `pin`은 요청이 묶인 계정 ID다.
pub(crate) fn plan(
    accounts: &[Account],
    policy: &Policy,
    tool: &str,
    pin: Option<&str>,
    selectable: Option<bool>,
    last_refresh_at: Option<i64>,
    last_probe_at: i64,
    refreshing: bool,
    now: i64,
) -> RefreshPlan {
    let freshness = policy.stale_after_seconds.saturating_mul(1000).min(i64::MAX as u64) as i64;
    // 오래된 한도는 선택 가능한 계정이 있어도 갱신을 시작한다(기존 동작). 기다리지는 않는다.
    let stale = relevant(accounts, tool, pin).any(|account| {
        account.can_launch && account.buckets.iter().any(|bucket| bucket.status != "unknown" && needs_refresh(bucket, now, freshness))
    });
    // 선택 가능한 계정이 없고, 계정이 아예 없거나 조회로 달라질 수 있는 계정이 있을 때만 미확인 상태다.
    let waiting = selectable == Some(false)
        && (relevant(accounts, tool, pin).next().is_none() || relevant(accounts, tool, pin).any(|account| pending(account, now, freshness)));
    let cooled = last_probe_at > now || now.saturating_sub(last_probe_at) >= PROBE_COOLDOWN_MS;
    let probe = waiting && !refreshing && cooled;
    RefreshPlan {
        start: last_refresh_at.is_none() || stale || probe,
        probe,
        wait: waiting && (probe || refreshing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn account(id: &str, tool: &str) -> Account {
        Account {
            id: id.into(), tool: tool.into(), identity_key: Some(id.into()), enabled: true, can_launch: true,
            auth_status: "authenticated".into(), verification: "preflight-verified".into(), max_concurrency: 1,
            buckets: vec![QuotaBucket {
                id: "w".into(), label: "w".into(), used_percent: Some(10.0), resets_at: Some(NOW + 3_600_000),
                observed_at: NOW - 1_000, source: "t".into(), status: "known".into(), ..Default::default()
            }],
            ..Default::default()
        }
    }
    fn plan_for(accounts: &[Account], tool: &str, selectable: Option<bool>, last_probe_at: i64, refreshing: bool) -> RefreshPlan {
        plan(accounts, &Policy::default(), tool, None, selectable, Some(NOW - 2_000), last_probe_at, refreshing, NOW)
    }

    #[test]
    fn missing_accounts_and_unknown_quota_probe_even_right_after_a_full_scan() {
        let outcome = plan_for(&[], "claude", Some(false), 0, false);
        assert!(outcome.start && outcome.probe && outcome.wait, "{outcome:?}");
        let mut empty = account("a", "claude");
        empty.buckets.clear();
        let outcome = plan_for(&[empty], "claude", Some(false), 0, false);
        assert!(outcome.start && outcome.probe && outcome.wait, "{outcome:?}");
    }

    #[test]
    fn a_selectable_request_never_waits_for_another_accounts_missing_state() {
        let mut slow = account("slow", "claude");
        slow.buckets.clear();
        let outcome = plan_for(&[account("ready", "claude"), slow], "claude", Some(true), 0, true);
        assert!(!outcome.wait && !outcome.start, "{outcome:?}");
    }

    #[test]
    fn probes_are_rate_limited_by_the_last_probe_but_an_active_scan_is_joined() {
        let throttled = plan_for(&[], "claude", Some(false), NOW - 2_000, false);
        assert!(!throttled.start && !throttled.wait, "{throttled:?}");
        let joined = plan_for(&[], "claude", Some(false), NOW - 2_000, true);
        assert!(!joined.probe && joined.wait);
        let later = plan_for(&[], "claude", Some(false), NOW - PROBE_COOLDOWN_MS, false);
        assert!(later.probe && later.wait);
    }

    #[test]
    fn excluded_accounts_and_undecidable_requests_do_not_wait() {
        let mut excluded = account("excluded", "claude");
        excluded.enabled = false;
        excluded.buckets.clear();
        let outcome = plan_for(&[excluded], "claude", Some(false), 0, false);
        assert!(!outcome.wait && !outcome.probe, "a user-excluded account is not a reason to scan: {outcome:?}");
        let unknown = plan_for(&[], "claude", None, 0, true);
        assert!(!unknown.wait && !unknown.probe);
    }

    #[test]
    fn a_known_account_that_the_scheduler_rejects_still_waits_for_an_unknown_one() {
        // 소진·슬롯 점유·프로젝트 제외 계정은 `selectable == Some(false)`로 온다. 다른 미확인 계정이 있으면 기다린다.
        let mut unknown = account("new", "claude");
        unknown.buckets.clear();
        let outcome = plan_for(&[account("busy", "claude"), unknown], "claude", Some(false), 0, false);
        assert!(outcome.probe && outcome.wait, "{outcome:?}");
    }
}

#[cfg(test)]
mod pinned_tests {
    use super::*;

    #[test]
    fn a_pinned_request_only_waits_for_its_own_account() {
        let now = 1_800_000_000_000;
        let mut stale = Account { id: "orig".into(), tool: "claude".into(), identity_key: Some("o".into()), enabled: true, can_launch: true, auth_status: "authenticated".into(), ..Default::default() };
        stale.buckets = vec![QuotaBucket { status: "stale".into(), observed_at: 1, ..Default::default() }];
        let mut other = stale.clone();
        other.id = "other".into();
        other.identity_key = Some("x".into());
        other.buckets.clear();
        let accounts = [stale.clone(), other];
        let own = plan(&accounts, &Policy::default(), "claude", Some("orig"), Some(false), Some(now - 2_000), 0, false, now);
        assert!(own.probe && own.wait, "{own:?}");
        stale.enabled = false;
        let other_only = [stale, accounts[1].clone()];
        let blocked = plan(&other_only, &Policy::default(), "claude", Some("orig"), Some(false), Some(now - 2_000), 0, false, now);
        assert!(!blocked.wait, "another account's missing state cannot help a request pinned elsewhere: {blocked:?}");
    }
}
