use super::*;
use aam_protocol::{CreditsState, ExtraUsageState};

fn codex_account(id: &str, now: i64) -> Account {
    Account {
        provider: "openai".into(),
        tool: "codex".into(),
        plan: Some("Pro".into()),
        ..account_fixture(id, now)
    }
}

fn codex_intent() -> LaunchIntent {
    LaunchIntent { tool: "codex".into(), model: "native-default".into(), ..intent() }
}

fn spend(account: &mut Account) {
    for bucket in &mut account.buckets {
        bucket.used_percent = Some(100.0);
        bucket.status = "exhausted".into();
    }
}

fn credits(now: i64) -> CreditsState {
    CreditsState { available: true, unlimited: false, balance: Some("12.5".into()), ordinary_usage_allowed: None, observed_at: now }
}

fn extra(used: f64, limit: Option<f64>, now: i64) -> ExtraUsageState {
    ExtraUsageState { enabled: true, used_usd: used, limit_usd: limit, observed_at: now }
}

fn on() -> Policy {
    Policy { use_credits_after_limit: true, ..Policy::default() }
}

fn reasons<'a>(decision: &'a aam_protocol::Decision, id: &str) -> &'a [String] {
    &decision.candidates.iter().find(|candidate| candidate.account_id == id).unwrap().reasons
}

#[test]
fn credits_are_never_used_while_the_option_is_off() {
    let now = now_ms();
    let mut paid = codex_account("paid", now);
    spend(&mut paid);
    paid.credits = Some(credits(now));
    let decision = scheduler::decide(&[paid], &[], &Policy::default(), &codex_intent(), now).unwrap();
    assert_eq!(decision.selected_account_id, None);
    assert!(reasons(&decision, "paid").iter().any(|reason| reason.starts_with("QUOTA_EXHAUSTED:")));
    assert!(!scheduler::chose_credits(&decision));
}

#[test]
fn credits_account_is_a_last_resort_behind_every_account_with_subscription_left() {
    let now = now_ms();
    let mut paid = codex_account("paid", now);
    spend(&mut paid);
    paid.credits = Some(credits(now));
    let roomy = codex_account("roomy", now);
    let both = scheduler::decide(&[paid.clone(), roomy.clone()], &[], &on(), &codex_intent(), now).unwrap();
    assert_eq!(both.selected_account_id.as_deref(), Some("roomy"));
    assert!(!scheduler::chose_credits(&both));
    assert!(!both.candidates.iter().find(|candidate| candidate.account_id == "paid").unwrap().eligible);
    // 구독 계정이 하나도 없을 때만 크레딧 계정이 이유와 함께 선택된다.
    let only = scheduler::decide(std::slice::from_ref(&paid), &[], &on(), &codex_intent(), now).unwrap();
    assert_eq!(only.selected_account_id.as_deref(), Some("paid"));
    assert!(scheduler::chose_credits(&only));
    assert!(reasons(&only, "paid").iter().any(|reason| reason.starts_with("CREDITS_FALLBACK:")));
    // 구독 계정이 동시 사용 자리만 없는 경우에도 크레딧으로 넘어가지 않고 자리를 기다린다.
    let held: aam_protocol::Session = serde_json::from_value(serde_json::json!({
        "id":"held", "requestId":"held", "accountId":"roomy", "tool":"codex", "model":"native-default",
        "cwd":test_cwd(), "state":"ACTIVE", "verification":"preflight-verified",
        "startedAt":now, "updatedAt":now, "process":null, "supervisor":null,
        "spawnAttemptId":null, "generation":"generation", "exitCode":null, "reason":null
    }))
    .unwrap();
    let waiting = scheduler::decide(&[paid.clone(), roomy], &[held], &on(), &codex_intent(), now).unwrap();
    assert_eq!(waiting.selected_account_id, None);
    assert!(!scheduler::chose_credits(&waiting));
}

#[test]
fn accounts_the_user_excluded_are_never_billed_even_when_credits_exist() {
    let now = now_ms();
    let mut off = codex_account("off", now);
    spend(&mut off);
    off.credits = Some(credits(now));
    off.enabled = false;
    // 제외한 계정 하나뿐이면 옵트인이 켜져 있어도 선택되지 않는다.
    let alone = scheduler::decide(std::slice::from_ref(&off), &[], &on(), &codex_intent(), now).unwrap();
    assert_eq!(alone.selected_account_id, None);
    assert!(!scheduler::chose_credits(&alone));
    assert!(reasons(&alone, "off").iter().any(|reason| reason.starts_with("ACCOUNT_DISABLED")));
    // 활성 크레딧 계정과 섞여 있으면 활성 계정만 고른다.
    let mut on_account = codex_account("on", now);
    spend(&mut on_account);
    on_account.credits = Some(credits(now));
    let mixed = scheduler::decide(&[off.clone(), on_account.clone()], &[], &on(), &codex_intent(), now).unwrap();
    assert_eq!(mixed.selected_account_id.as_deref(), Some("on"));
    let reversed = scheduler::decide(&[on_account, off], &[], &on(), &codex_intent(), now).unwrap();
    assert_eq!(reversed.selected_account_id.as_deref(), Some("on"));
}

#[test]
fn missing_stale_or_unconfirmed_credits_are_not_available() {
    let now = now_ms();
    let pick = |mutate: &dyn Fn(&mut Account)| {
        let mut paid = codex_account("paid", now);
        spend(&mut paid);
        paid.credits = Some(credits(now));
        mutate(&mut paid);
        scheduler::decide(&[paid], &[], &on(), &codex_intent(), now).unwrap().selected_account_id
    };
    assert_eq!(pick(&|_| ()).as_deref(), Some("paid"));
    assert_eq!(pick(&|account| account.credits = None), None);
    assert_eq!(pick(&|account| account.credits.as_mut().unwrap().available = false), None);
    // 크레딧 관측이 낡으면(stale_after_seconds 초과) 쓰지 않는다.
    assert_eq!(pick(&|account| account.credits.as_mut().unwrap().observed_at = now - 901_000), None);
    assert_eq!(pick(&|account| account.credits.as_mut().unwrap().observed_at = now + 600_000), None);
    // 공급자가 기본 포함 사용량을 허용한다고 알리면 크레딧 전환이 아니다.
    assert_eq!(pick(&|account| account.credits.as_mut().unwrap().ordinary_usage_allowed = Some(true)), None);
    // 한도 소진 관측이 낡았거나 리셋 시각이 지났으면 낡은 값으로 크레딧을 고르지 않는다.
    assert_eq!(pick(&|account| account.buckets[0].observed_at = now - 901_000), None);
    assert_eq!(pick(&|account| account.buckets[0].resets_at = Some(now - 1)), None);
    // 크레딧은 Codex 계정에만 적용된다.
    assert_eq!(pick(&|account| account.tool = "claude".into()), None);
}

#[test]
fn credits_stop_as_soon_as_a_fresh_observation_shows_the_limit_recovered() {
    let now = now_ms();
    let mut paid = codex_account("paid", now);
    paid.credits = Some(credits(now));
    let mut other = codex_account("other", now);
    spend(&mut other);
    other.credits = Some(credits(now));
    spend(&mut paid);
    let chosen = scheduler::decide(&[paid.clone(), other.clone()], &[], &on(), &codex_intent(), now).unwrap();
    assert!(scheduler::chose_credits(&chosen));
    // 리셋 관측이 들어오면 같은 계정이어도 구독 한도로 일반 선택된다. 선택 상태는 저장하지 않는다.
    paid.buckets[0].used_percent = Some(10.0);
    paid.buckets[0].status = "known".into();
    let recovered = scheduler::decide(&[paid, other], &[], &on(), &codex_intent(), now + 1_000).unwrap();
    assert_eq!(recovered.selected_account_id.as_deref(), Some("paid"));
    assert!(!scheduler::chose_credits(&recovered));
}

#[test]
fn extra_usage_follows_its_own_option_and_is_never_mixed_with_credits() {
    let now = now_ms();
    let mut paid = account_fixture("extra", now);
    spend(&mut paid);
    paid.extra_usage = Some(extra(12.4, Some(50.0), now));
    let extra_on = Policy { use_extra_usage_after_limit: true, ..Policy::default() };
    // 꺼져 있으면 그대로 제외, Codex 크레딧 옵트인은 Claude 추가 사용량을 켜지 않는다.
    assert_eq!(scheduler::decide(std::slice::from_ref(&paid), &[], &Policy::default(), &intent(), now).unwrap().selected_account_id, None);
    assert_eq!(scheduler::decide(std::slice::from_ref(&paid), &[], &on(), &intent(), now).unwrap().selected_account_id, None);
    // 구독 한도가 남은 Claude 계정이 있으면 추가 사용량은 마지막 수단이다.
    let roomy = account_fixture("roomy", now);
    let ordered = scheduler::decide(&[paid.clone(), roomy], &[], &extra_on, &intent(), now).unwrap();
    assert_eq!(ordered.selected_account_id.as_deref(), Some("roomy"));
    assert!(!scheduler::chose_extra_usage(&ordered));
    let last = scheduler::decide(std::slice::from_ref(&paid), &[], &extra_on, &intent(), now).unwrap();
    assert_eq!(last.selected_account_id.as_deref(), Some("extra"));
    assert!(scheduler::chose_extra_usage(&last));
    assert!(!scheduler::chose_credits(&last));
    assert!(reasons(&last, "extra").iter().any(|reason| reason.starts_with("EXTRA_USAGE_FALLBACK:")));
    // 상한이 없으면 가능, 상한에 닿았거나 낡은 관측·꺼진 상태면 불가.
    let pick = |state: Option<ExtraUsageState>| {
        let mut account = paid.clone();
        account.extra_usage = state;
        scheduler::decide(&[account], &[], &extra_on, &intent(), now).unwrap().selected_account_id
    };
    assert_eq!(pick(Some(extra(3.0, None, now))).as_deref(), Some("extra"));
    assert_eq!(pick(Some(extra(50.0, Some(50.0), now))), None);
    assert_eq!(pick(Some(extra(60.0, Some(50.0), now))), None);
    assert_eq!(pick(Some(extra(3.0, Some(50.0), now - 901_000))), None);
    assert_eq!(pick(Some(ExtraUsageState { enabled: false, ..extra(3.0, None, now) })), None);
    assert_eq!(pick(None), None);
}

#[test]
fn summary_shows_credits_or_extra_usage_only_while_the_subscription_is_spent() {
    let now = now_ms();
    let mut paid = codex_account("paid", now);
    spend(&mut paid);
    paid.credits = Some(credits(now));
    let hint = crate::quota_summary::summaries(std::slice::from_ref(&paid), &Policy::default(), &[], now);
    // 옵트인이 꺼져 있으면 쉬는 상태 그대로이고 크레딧이 있다는 사실만 알린다.
    assert_eq!(hint[0].kind, "resting");
    let facts = hint[0].credits.as_ref().unwrap();
    assert!(!facts.active);
    assert_eq!(facts.balance.as_deref(), Some("12.5"));
    let active = crate::quota_summary::summaries(std::slice::from_ref(&paid), &on(), &[], now);
    assert_eq!(active[0].kind, "credits");
    assert!(active[0].credits.as_ref().unwrap().active);
    // 다른 Codex 계정에 구독 한도가 남아 있으면 배지는 켜지지 않는다.
    let roomy = codex_account("roomy", now);
    let mixed = crate::quota_summary::summaries(&[paid.clone(), roomy], &on(), &[], now);
    assert!(mixed.iter().all(|summary| summary.kind != "credits"));
    // 리셋 관측이 들어오면 스냅샷을 다시 계산하는 즉시 배지가 사라진다.
    let mut recovered = paid.clone();
    recovered.buckets[0].used_percent = Some(10.0);
    recovered.buckets[0].status = "known".into();
    let after = crate::quota_summary::summaries(&[recovered], &on(), &[], now);
    assert_eq!(after[0].kind, "available");
    assert!(!after[0].credits.as_ref().unwrap().active);
    // 크레딧 관측이 낡으면 크레딧 정보 자체를 내보내지 않는다.
    let mut old = paid;
    old.credits.as_mut().unwrap().observed_at = now - 901_000;
    let stale = crate::quota_summary::summaries(&[old], &on(), &[], now);
    assert_eq!(stale[0].kind, "resting");
    assert!(stale[0].credits.is_none());

    let mut claude = account_fixture("extra", now);
    spend(&mut claude);
    claude.extra_usage = Some(extra(12.4, Some(50.0), now));
    let extra_on = Policy { use_extra_usage_after_limit: true, ..Policy::default() };
    let shown = crate::quota_summary::summaries(std::slice::from_ref(&claude), &extra_on, &[], now);
    assert_eq!(shown[0].kind, "extra");
    let facts = shown[0].extra_usage.as_ref().unwrap();
    assert!(facts.active && !facts.limit_reached);
    assert_eq!((facts.used_usd, facts.limit_usd), (12.4, Some(50.0)));
    assert!(shown[0].credits.is_none());
    claude.extra_usage = Some(extra(50.0, Some(50.0), now));
    let capped = crate::quota_summary::summaries(&[claude], &extra_on, &[], now);
    assert_eq!(capped[0].kind, "resting");
    assert!(capped[0].extra_usage.as_ref().unwrap().limit_reached);
}
