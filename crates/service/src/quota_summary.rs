//! Current quota facts, not an admission promise: model/project/capacity checks remain in the scheduler.
use aam_protocol::{Account, AccountQuotaSummary, CreditsSummary, ExpiringQuota, ExtraUsageSummary, Policy, QuotaBucket};
use crate::bridge::BlockStatus;
use std::collections::{BTreeMap, BTreeSet};

fn tokens(account: &Account) -> Vec<String> {
    let mut result: Vec<String> = account.omp_credential_pins.iter().map(|pin| format!("pin:{}:{}", pin.provider, pin.hash)).collect();
    let identity = account.identity_key.as_deref().unwrap_or("");
    let part = |name: &str| identity.split('|').skip(1).find_map(|segment| segment.strip_prefix(name)).filter(|v| !v.is_empty());
    let workspace = part("workspace:");
    if let Some(value) = workspace { result.push(format!("workspace:{}:{value}", account.provider)); }
    if let Some(value) = part("subject:") { result.push(format!("subject:{}:{value}", account.provider)); }
    if workspace.is_none() {
        if let Some(email) = &account.email { result.push(format!("email:{}:{}", account.provider, email.to_lowercase())); }
    }
    result
}

fn root(parents: &[usize], mut index: usize) -> usize {
    while parents[index] != index { index = parents[index]; }
    index
}

/// Uses the desktop account identity tokens; all member IDs are returned so rendering never guesses a verdict.
fn groups(accounts: &[Account]) -> Vec<Vec<&Account>> {
    let mut parents: Vec<usize> = (0..accounts.len()).collect();
    let mut owners = BTreeMap::new();
    for (index, account) in accounts.iter().enumerate() {
        for token in tokens(account) {
            if let Some(&owner) = owners.get(&token) {
                let from = root(&parents, index);
                parents[from] = root(&parents, owner);
            } else { owners.insert(token, index); }
        }
    }
    let mut verified: BTreeMap<(&str, String), BTreeSet<usize>> = BTreeMap::new();
    for (index, account) in accounts.iter().enumerate() {
        if account.identity_key.as_ref().is_some_and(|key| !key.is_empty()) {
            if let Some(email) = &account.email { verified.entry((&account.provider, email.to_lowercase())).or_default().insert(root(&parents, index)); }
        }
    }
    for (index, account) in accounts.iter().enumerate() {
        if account.identity_key.as_ref().is_none_or(|key| key.is_empty()) && account.omp_credential_pins.is_empty() {
            if let Some(email) = &account.email {
                if let Some(owners) = verified.get(&(account.provider.as_str(), email.to_lowercase())) {
                    if owners.len() == 1 { let from = root(&parents, index); parents[from] = *owners.first().unwrap(); }
                }
            }
        }
    }
    let mut grouped: BTreeMap<usize, Vec<&Account>> = BTreeMap::new();
    for (index, account) in accounts.iter().enumerate() { grouped.entry(root(&parents, index)).or_default().push(account); }
    grouped.into_values().collect()
}

fn state(bucket: &QuotaBucket, policy: &Policy, now: i64) -> &'static str {
    if bucket.status == "stale" { return "unknown"; }
    if !matches!(bucket.status.as_str(), "known" | "exhausted") || bucket.observed_at <= 0
        || bucket.observed_at > now.saturating_add(30_000)
        || now.saturating_sub(bucket.observed_at) > policy.stale_after_seconds.saturating_mul(1000).min(i64::MAX as u64) as i64
        || bucket.resets_at.is_some_and(|reset| reset <= now) { return "unknown"; }
    if bucket.status == "exhausted" { return "exhausted"; }
    match bucket.used_percent {
        Some(used) if used.is_finite() && (0.0..=100.0).contains(&used) => if used >= 100.0 { "exhausted" } else { "known" },
        _ => "unknown",
    }
}

/// 곧 리셋되는데 안전 여유량을 빼고도 많이 남은 긴 주기 한도. 5시간처럼 짧은 한도는 금방 다시
/// 채워지므로 보지 않는다. 같은 근거를 화면 알림과 스마트 배정 우선순위가 함께 쓴다.
pub(crate) fn expiring(buckets: &[&QuotaBucket], policy: &Policy, now: i64) -> Option<ExpiringQuota> {
    let window = i64::from(policy.expiring_window_hours).saturating_mul(3_600_000);
    buckets.iter().copied()
        .filter(|bucket| bucket.model.is_none() && !crate::scheduler::short_window(bucket) && state(bucket, policy, now) == "known")
        .filter_map(|bucket| {
            let resets_at = bucket.resets_at?;
            let left = resets_at - now;
            if left <= 0 || left > window { return None; }
            let usable = 100.0 - bucket.used_percent? - policy.safety_reserve_percent;
            (usable >= policy.expiring_min_percent && usable > 0.0).then(|| ExpiringQuota {
                label: bucket.label.clone(),
                resets_at,
                usable_percent: usable,
                per_hour: usable / (left as f64 / 3_600_000.0).max(1.0 / 60.0),
            })
        })
        // 가장 급한(시간당 써야 할 양이 가장 큰) 한도 하나만 고른다.
        .max_by(|a, b| a.per_hour.total_cmp(&b.per_hour))
}

fn scope_name(scope: &str) -> String {
    match scope {
        "claude-gpt" => "Claude·GPT".into(),
        _ => { let mut chars = scope.chars(); chars.next().map(|first| first.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default() }
    }
}

fn matches_block(account: &Account, block: &BlockStatus) -> bool {
    if aam_protocol::pin_provider(&account.provider) != aam_protocol::pin_provider(&block.provider) { return false; }
    account.email.as_ref().is_some_and(|email| email.eq_ignore_ascii_case(&block.email))
        || account.identity_key.as_deref().unwrap_or("").split('|').find_map(|part| part.strip_prefix("subject:"))
            .is_some_and(|subject| block.email.eq_ignore_ascii_case(&format!("account:{subject}")))
}

/// 같은 실제 계정 묶음에서 신선한 추가 사용량(USD) 관측 중 가장 최근 것. 꺼져 있거나 오래됐으면 `None`.
fn extra_usage_of(members: &[&Account], policy: &Policy, now: i64) -> Option<ExtraUsageSummary> {
    let freshness = policy.stale_after_seconds.saturating_mul(1000).min(i64::MAX as u64) as i64;
    members
        .iter()
        .filter(|account| account.provider == "anthropic" && account.enabled)
        .filter_map(|account| account.extra_usage.as_ref())
        .filter(|extra| {
            extra.enabled
                && extra.observed_at > 0
                && extra.observed_at <= now.saturating_add(30_000)
                && now.saturating_sub(extra.observed_at) <= freshness
        })
        .max_by_key(|extra| extra.observed_at)
        .map(|extra| ExtraUsageSummary {
            active: false,
            used_usd: extra.used_usd,
            limit_usd: extra.limit_usd,
            limit_reached: !extra.usable(now, policy.stale_after_seconds),
        })
}

/// 같은 실제 계정 묶음에서 신선하고 명시된 크레딧 관측 중 가장 최근 것. 오래됐거나 없으면 `None`.
fn credits_of(members: &[&Account], policy: &Policy, now: i64) -> Option<CreditsSummary> {
    members
        .iter()
        .filter(|account| account.tool == "codex" && account.enabled)
        .filter_map(|account| account.credits.as_ref())
        .filter(|credits| credits.usable(now, policy.stale_after_seconds))
        .max_by_key(|credits| credits.observed_at)
        .map(|credits| CreditsSummary { active: false, unlimited: credits.unlimited, balance: credits.balance.clone() })
}

/// 구독 한도가 남아 새 작업을 받을 수 있어 보이는 묶음인지.
fn has_subscription_left(summary: &AccountQuotaSummary) -> bool {
    matches!(summary.kind.as_str(), "available" | "reserve" | "partial")
}

/// 스냅샷마다 새로 계산한다. 크레딧 사용 상태를 저장하지 않으므로 한도가 리셋되거나 관측이 오래되면 배지가 곧 사라진다.
pub(crate) fn summaries(accounts: &[Account], policy: &Policy, blocks: &[BlockStatus], now: i64) -> Vec<AccountQuotaSummary> {
    let groups = groups(accounts);
    let mut results: Vec<AccountQuotaSummary> = groups.iter().map(|members| {
        let mut result = AccountQuotaSummary { account_ids: members.iter().map(|a| a.id.clone()).collect(), kind: "unknown".into(), until: None, models: vec![], label: None, rate: false, expiring: None, credits: None, extra_usage: None };
        result.credits = credits_of(members, policy, now);
        result.extra_usage = extra_usage_of(members, policy, now);
        if members.iter().all(|a| !a.enabled) { result.kind = "excluded".into(); return result; }
        if members.iter().filter(|a| a.enabled).all(|a| a.auth_status == "auth-required") { result.kind = "login".into(); return result; }
        // The newest observation wins for the same upstream bucket; different buckets remain independent.
        let mut buckets: BTreeMap<&str, &QuotaBucket> = BTreeMap::new();
        for account in members {
            for bucket in &account.buckets {
                let entry = buckets.entry(&bucket.id).or_insert(bucket);
                if bucket.observed_at > entry.observed_at { *entry = bucket; }
            }
        }
        let mut known = false;
        let mut unsure = false;
        let mut reserve = None;
        let mut models = BTreeSet::new();
        result.expiring = expiring(&buckets.values().copied().collect::<Vec<_>>(), policy, now);
        for bucket in buckets.into_values() {
            let status = state(bucket, policy, now);
            known |= status == "known";
            if let Some(model) = &bucket.model {
                if status != "known" { models.insert(scope_name(model)); }
            } else {
                if status == "exhausted" {
                    result.kind = "resting".into();
                    result.label.get_or_insert_with(|| bucket.label.clone());
                    result.until = result.until.max(bucket.resets_at);
                } else if status != "known" { unsure = true; }
            }
            if status == "known" && policy.safety_reserve_percent > 0.0
                && bucket.used_percent.is_some_and(|used| used >= 100.0 - policy.safety_reserve_percent) { reserve.get_or_insert_with(|| bucket.label.clone()); }
        }
        for block in blocks.iter().filter(|block| block.until > now && members.iter().any(|a| matches_block(a, block))) {
            // Do not attach email-only evidence to multiple workspace identities.
            if groups.iter().filter(|group| group.iter().any(|a| matches_block(a, block))).count() != 1 { continue; }
            match block.scope.as_deref() {
                None | Some("chat") => { result.kind = "resting".into(); result.until = result.until.max(Some(block.until)); result.rate = result.label.is_none(); }
                Some(scope) => { models.insert(scope_name(scope)); }
            }
        }
        if result.kind == "resting" { result.expiring = None; return result; }
        if !known || unsure { result.expiring = None; return result; }
        if !models.is_empty() { result.kind = "partial".into(); result.models = models.into_iter().collect(); }
        else if let Some(label) = reserve { result.kind = "reserve".into(); result.label = Some(label); }
        else { result.kind = "available".into(); }
        result
    }).collect();
    // 구독 한도가 남은 Codex 묶음이 하나라도 있으면 스케줄러는 크레딧 계정을 고르지 않는다. 표시도 같은 기준이다.
    let codex_left = groups.iter().zip(&results).any(|(members, summary)| members.iter().any(|a| a.tool == "codex") && has_subscription_left(summary));
    if policy.use_credits_after_limit && !codex_left {
        for summary in &mut results {
            // 속도 제한(rate)만으로 쉬는 묶음은 구독 한도 소진이 아니므로 크레딧 사용으로 보지 않는다.
            if summary.kind == "resting" && !summary.rate {
                if let Some(credits) = summary.credits.as_mut() {
                    credits.active = true;
                    summary.kind = "credits".into();
                }
            }
        }
    }
    // Claude 추가 사용량은 Codex 크레딧과 단위(USD)·의미가 달라 따로 계산한다. 상한에 닿은 묶음은 쓸 수 없으므로 켜지 않는다.
    let claude_left = groups.iter().zip(&results).any(|(members, summary)| members.iter().any(|a| a.provider == "anthropic") && has_subscription_left(summary));
    if policy.use_extra_usage_after_limit && !claude_left {
        for summary in &mut results {
            if summary.kind == "resting" && !summary.rate {
                if let Some(extra) = summary.extra_usage.as_mut().filter(|extra| !extra.limit_reached) {
                    extra.active = true;
                    summary.kind = "extra".into();
                }
            }
        }
    }
    results
}
