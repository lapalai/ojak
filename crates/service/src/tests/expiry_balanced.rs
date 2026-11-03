use super::*;
use aam_adapters::api_credit as keys;
use aam_protocol::{ApiCreditGrant, ApiCreditState};
use serde_json::json;

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

fn api_account(id: &str, now: i64) -> Account {
    Account {
        id: id.into(),
        provider: "anthropic".into(),
        tool: "api".into(),
        label: id.into(),
        identity_key: Some(format!("apikey:{id}")),
        auth_status: "authenticated".into(),
        verification: "api-key".into(),
        can_launch: true,
        enabled: true,
        max_concurrency: 0,
        last_checked_at: now,
        api_credit: Some(ApiCreditState { period_start: keys::period_start(now), used_usd: 0.0, unmetered_requests: 0, updated_at: now }),
        ..Default::default()
    }
}

fn spent_claude(id: &str, now: i64) -> Account {
    let mut account = account_fixture(id, now);
    for bucket in &mut account.buckets {
        bucket.used_percent = Some(100.0);
        bucket.status = "exhausted".into();
    }
    account
}

fn headless() -> LaunchIntent {
    LaunchIntent { non_interactive: true, ..intent() }
}

fn grant(id: &str, budget: f64, starts_at: i64, expires_at: i64) -> ApiCreditGrant {
    ApiCreditGrant { id: id.into(), budget_usd: budget, starts_at, expires_at, confirmed_at: starts_at, source: "manual".into(), expiry_source: Default::default(), billing_estimate: None }
}

fn update(fixture: &Fixture, mut patch: Value) -> Result<Policy, ApiError> {
    let revision = policy(&fixture.service.lock().unwrap().connection).unwrap().revision;
    patch["expectedRevision"] = json!(revision);
    fixture.service.dispatch("policy.update", patch).map(|value| serde_json::from_value(value).unwrap())
}

fn stored_policy(fixture: &Fixture) -> Policy {
    policy(&fixture.service.lock().unwrap().connection).unwrap()
}

fn spend(fixture: &Fixture, id: &str, cost: Option<f64>) {
    record_api_credit(&mut fixture.service.lock().unwrap().connection, id, cost, now_ms()).unwrap();
}

fn reasons<'a>(decision: &'a aam_protocol::Decision, id: &str) -> &'a [String] {
    &decision.candidates.iter().find(|candidate| candidate.account_id == id).unwrap().reasons
}

/// 허용 기간은 사용자가 확인했을 때만 저장되고, 잘못된 값은 정책을 바꾸지 않으며, 같은 id만 쓴 금액을 이어 간다.
#[test]
fn grant_input_needs_confirmation_valid_dates_and_keeps_spend_only_for_the_same_id() {
    let fixture = Fixture::new();
    let now = now_ms();
    let input = |confirmed: bool| json!({"apiCreditGrant": {"budgetUsd": 50.0, "startsAt": now - HOUR, "expiresAt": now + 10 * DAY, "confirmed": confirmed}});
    let unconfirmed = update(&fixture, input(false)).unwrap_err();
    assert_eq!(unconfirmed.code, "API_CREDIT_GRANT_CONFIRM_REQUIRED");
    assert!(stored_policy(&fixture).api_credit_grant.is_none());
    for bad in [
        json!({"budgetUsd": 0.0, "startsAt": now, "expiresAt": now + DAY, "confirmed": true}),
        json!({"budgetUsd": 5.0, "startsAt": now, "expiresAt": now, "confirmed": true}),
        json!({"budgetUsd": 5.0, "startsAt": now - 2 * DAY, "expiresAt": now - DAY, "confirmed": true}),
        json!({"budgetUsd": 5.0, "startsAt": now, "expiresAt": now + 400 * DAY, "confirmed": true}),
        json!({"budgetUsd": 5.0, "startsAt": now, "expiresAt": now + DAY, "confirmed": true, "id": "unknown"}),
    ] {
        assert_eq!(update(&fixture, json!({"apiCreditGrant": bad})).unwrap_err().code, "INVALID_PARAMS");
    }
    assert!(stored_policy(&fixture).api_credit_grant.is_none());

    let first = update(&fixture, input(true)).unwrap().api_credit_grant.unwrap();
    assert_eq!((first.budget_usd, first.source.as_str()), (50.0, "manual"));
    assert!(first.confirmed_at >= now);
    // 필드를 빼면 변경하지 않는다.
    update(&fixture, json!({"balanceApiCredit": true})).unwrap();
    assert_eq!(stored_policy(&fixture).api_credit_grant.unwrap().id, first.id);
    fixture.save(&api_account("k", now));
    spend(&fixture, "k", Some(12.0));
    // 같은 id의 수정은 쓴 금액을 이어 간다. id를 빼면 새 기간이고 0부터 센다.
    let edited = update(&fixture, json!({"apiCreditGrant": {"id": first.id, "budgetUsd": 80.0, "startsAt": now - HOUR, "expiresAt": now + 20 * DAY, "confirmed": true}})).unwrap();
    assert_eq!(edited.api_credit_grant.as_ref().unwrap().id, first.id);
    assert_eq!(fixture.service.snapshot().unwrap().api_credit_grant.unwrap().used_usd, 12.0);
    let fresh = update(&fixture, input(true)).unwrap().api_credit_grant.unwrap();
    assert_ne!(fresh.id, first.id);
    let status = fixture.service.snapshot().unwrap().api_credit_grant.unwrap();
    assert_eq!((status.state.as_str(), status.used_usd, status.remaining_usd), ("active", 0.0, Some(50.0)));
    // null은 삭제이고, 삭제하면 판정할 기간이 없어 `unknown`이다.
    assert!(update(&fixture, json!({"apiCreditGrant": null})).unwrap().api_credit_grant.is_none());
    let none = fixture.service.snapshot().unwrap().api_credit_grant.unwrap();
    assert_eq!((none.state.as_str(), none.budget_usd, none.proactive.allowed), ("unknown", None, false));
}

/// 사용액은 허용 기간 id로 합산해 쌓는다: 키를 지워도 남고, 기간 밖 사용은 예산에 넣지 않고, 미확인 요청은 선제 사용을 막는다.
#[test]
fn grant_spend_is_pooled_survives_key_removal_and_unmetered_or_exhausted_close_proactive_use() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&spent_claude("sub", now));
    fixture.save(&api_account("a", now));
    fixture.save(&api_account("b", now));
    update(&fixture, json!({"allocationMode": "expiry-balanced", "useApiCreditAfterLimit": true, "balanceApiCredit": true,
        "apiCreditGrant": {"budgetUsd": 50.0, "startsAt": now - HOUR, "expiresAt": now + 10 * DAY, "confirmed": true}})).unwrap();
    // 확인 직후에는 일정보다 덜 쓴 상태라 선제 사용이 열려 있다.
    let opened = fixture.service.snapshot().unwrap().api_credit_grant.unwrap();
    assert!(opened.proactive.allowed, "{:?}", opened.proactive);
    // 두 키가 같은 조직 잔액을 나눠 쓴다: 키마다 예산이 생기지 않는다.
    spend(&fixture, "a", Some(10.0));
    spend(&fixture, "b", Some(15.0));
    let status = fixture.service.snapshot().unwrap().api_credit_grant.unwrap();
    assert_eq!((status.state.as_str(), status.used_usd, status.remaining_usd), ("active", 25.0, Some(25.0)));
    // 기간 초반에 절반을 썼으니 일정보다 앞서 쓴 것이라 새 선제 사용은 시간이 지날 때까지 열리지 않는다.
    assert_eq!(status.proactive.blocked_by.as_deref(), Some("AHEAD_OF_PACE"));
    assert!(status.per_day_usd.unwrap() > 2.0);
    // 키를 지워도 합산 사용액과 늦게 끝나는 요청의 기록은 같은 기간에 남는다.
    fixture.service.dispatch("account.remove", json!({"id": "a"})).unwrap();
    spend(&fixture, "a", Some(5.0));
    let status = fixture.service.snapshot().unwrap().api_credit_grant.unwrap();
    assert_eq!((status.used_usd, status.remaining_usd), (30.0, Some(20.0)));
    // usage를 읽지 못한 요청이 하나라도 있으면 남은 금액을 믿을 수 없어 선제 사용을 막는다.
    spend(&fixture, "b", None);
    let status = fixture.service.snapshot().unwrap().api_credit_grant.unwrap();
    assert!(status.unmetered && status.unmetered_requests == 1);
    assert_eq!((status.proactive.allowed, status.proactive.blocked_by.as_deref()), (false, Some("GRANT_UNMETERED")));
    // 예산을 넘기면 소진이고 기존 한도 뒤 fallback도 닫힌다(허용 기간이 있을 때).
    spend(&fixture, "b", Some(30.0));
    let status = fixture.service.snapshot().unwrap().api_credit_grant.unwrap();
    assert_eq!((status.state.as_str(), status.remaining_usd, status.proactive.blocked_by.as_deref()), ("exhausted", Some(0.0), Some("GRANT_EXHAUSTED")));
    assert_eq!(fixture.service.acquire({ let mut request = request("exhausted"); request.intent.non_interactive = true; request }, now).unwrap_err().code, "NO_ELIGIBLE_ACCOUNT");
    // 아직 시작하지 않은 기간의 사용은 예산에 넣지 않는다.
    update(&fixture, json!({"apiCreditGrant": {"budgetUsd": 40.0, "startsAt": now + HOUR, "expiresAt": now + 5 * DAY, "confirmed": true}})).unwrap();
    spend(&fixture, "b", Some(7.0));
    let status = fixture.service.snapshot().unwrap().api_credit_grant.unwrap();
    assert_eq!((status.state.as_str(), status.used_usd, status.proactive.blocked_by.as_deref()), ("upcoming", 0.0, Some("GRANT_PENDING")));
    // 월 합계 원장은 기간과 무관하게 계속 센다.
    let monthly = account(&fixture.service.lock().unwrap().connection, "b").unwrap().api_credit.unwrap().used_usd;
    assert!((monthly - 15.0 - 30.0 - 7.0).abs() < 1e-9, "{monthly}");
}

/// 확인해 둔 허용 기간은 진행 중일 때만 한도 뒤 API fallback을 연다(`API_GRANT_*`). 기간이 없으면 기존 동작 그대로다.
#[test]
fn an_existing_grant_gates_the_after_limit_api_fallback_and_no_grant_keeps_it_unchanged() {
    let now = now_ms();
    let accounts = [spent_claude("sub", now), api_account("api", now)];
    let policy = Policy { use_api_credit_after_limit: true, ..Policy::default() };
    let pick = |policy: &Policy, ledger: &scheduler::ApiLedger| scheduler::decide_with(&accounts, &[], policy, &headless(), now, &Default::default(), ledger).unwrap();
    let open = pick(&policy, &scheduler::ApiLedger::default());
    assert_eq!(open.selected_account_id.as_deref(), Some("api"));
    for (grant, ledger, code) in [
        (grant("g", 50.0, now - 2 * DAY, now - DAY), scheduler::ApiLedger::default(), "API_GRANT_EXPIRED"),
        (grant("g", 50.0, now + DAY, now + 5 * DAY), scheduler::ApiLedger::default(), "API_GRANT_PENDING"),
        (grant("g", 50.0, now - DAY, now + 5 * DAY), scheduler::ApiLedger { grant_used_usd: 50.0, ..Default::default() }, "API_GRANT_EXHAUSTED"),
    ] {
        let gated = Policy { api_credit_grant: Some(grant), ..policy.clone() };
        let decision = pick(&gated, &ledger);
        assert_eq!(decision.selected_account_id, None, "{code}");
        assert!(reasons(&decision, "api").iter().any(|reason| reason.starts_with(code)), "{code}");
    }
    let active = Policy { api_credit_grant: Some(grant("g", 50.0, now - DAY, now + 5 * DAY)), ..policy.clone() };
    assert_eq!(pick(&active, &scheduler::ApiLedger { grant_used_usd: 49.0, ..Default::default() }).selected_account_id.as_deref(), Some("api"));
    // 월 상한은 허용 기간과 별개로 계속 지킨다.
    let capped = Policy { api_credit_monthly_limit_usd: 5.0, ..active.clone() };
    let over = scheduler::ApiLedger { removed_usd: 6.0, ..Default::default() };
    assert!(reasons(&pick(&capped, &over), "api").iter().any(|reason| reason.starts_with("API_CREDIT_LIMIT")));
}

/// 소비 균형은 새 대화의 구독 순서만 바꾼다: 플랜이 달라도 일정보다 덜 쓴 계정이 먼저이고, 소비 순서(priority)는 쓰지 않으며,
/// 수동 배정과 안전 장치(소진·여유량)는 그대로다. 구독이 남은 요청에는 API 키가 후보에도 오르지 않는다(비대화형 포함).
#[test]
fn expiry_balanced_orders_only_new_subscription_sessions_and_never_uses_api_when_a_subscription_has_room() {
    let now = now_ms();
    let mut max = account_fixture("max", now);
    max.plan = Some("Max".into());
    max.buckets = vec![bucket("weekly", None, 90.0, now + 6 * DAY, now)];
    let mut pro = account_fixture("pro", now);
    pro.plan = Some("Pro".into());
    pro.buckets = vec![bucket("weekly", None, 30.0, now + DAY, now)];
    let accounts = [max.clone(), pro.clone()];
    let smart = Policy::default();
    let balanced = Policy { allocation_mode: AllocationMode::ExpiryBalanced, ..Policy::default() };
    let pick = |policy: &Policy, accounts: &[Account]| scheduler::decide(accounts, &[], policy, &intent(), now).unwrap();
    // 기존 스마트는 플랜별 공정 순서가 먼저라 Max가 이기고, 소비 균형은 플랜과 무관하게 일정보다 덜 쓴 계정이 이긴다.
    assert_eq!(pick(&smart, &accounts).selected_account_id.as_deref(), Some("max"));
    let decision = pick(&balanced, &accounts);
    assert_eq!(decision.selected_account_id.as_deref(), Some("pro"));
    assert!(reasons(&decision, "pro").iter().any(|reason| reason.starts_with("EXPIRY_BALANCED:")));
    // 소비 순서(priority)·곧 리셋 우선 스위치는 이 모드에서 쓰지 않는다.
    let with_order = Policy { account_priority: vec!["max".into()], expiring_boost: true, ..balanced.clone() };
    assert_eq!(pick(&with_order, &accounts).selected_account_id.as_deref(), Some("pro"));
    // 공급자 수동 배정은 그대로 먼저다.
    let mut pinned = balanced.clone();
    pinned.provider_pins.insert("anthropic".into(), "max".into());
    assert_eq!(pick(&pinned, &accounts).selected_account_id.as_deref(), Some("max"));
    // 소진·안전 여유량 안쪽인 계정은 같은 가드로 뒤로 간다(리셋이 `expiringWindowHours`보다 멀 때. 가까우면 리셋 전 사용 규칙이 먼저다).
    let mut tight = pro.clone();
    tight.buckets = vec![bucket("weekly", None, 98.0, now + 4 * DAY, now)];
    assert_eq!(pick(&balanced, &[max.clone(), tight]).selected_account_id.as_deref(), Some("max"));
    assert_eq!(pick(&balanced, &[max.clone(), spent_claude("pro", now)]).selected_account_id.as_deref(), Some("max"));
    // 구독이 남아 있으면 API 키는 어떤 정책 조합에서도 후보가 아니다.
    let api = api_account("api", now);
    let everything = Policy {
        use_api_credit_after_limit: true,
        balance_api_credit: true,
        api_credit_grant: Some(grant("g", 500.0, now - HOUR, now + 30 * DAY)),
        ..balanced.clone()
    };
    for non_interactive in [false, true] {
        let request = LaunchIntent { non_interactive, ..intent() };
        let decision = scheduler::decide(&[max.clone(), pro.clone(), api.clone()], &[], &everything, &request, now).unwrap();
        assert_eq!(decision.selected_account_id.as_deref(), Some("pro"), "non_interactive={non_interactive}");
        assert!(decision.candidates.iter().all(|candidate| candidate.account_id != "api"));
    }
}

/// 상태 표시: 선제 사용이 막힌 이유는 모드 → 옵트인 → 기간 → 키 → 월 상한 → 일정 순으로 한 가지를 알려 준다.
#[test]
fn proactive_status_names_the_first_blocker() {
    let fixture = Fixture::new();
    let now = now_ms();
    let blocked = |fixture: &Fixture| fixture.service.snapshot().unwrap().api_credit_grant.unwrap().proactive.blocked_by;
    let grant_json = json!({"budgetUsd": 100.0, "startsAt": now - HOUR, "expiresAt": now + 10 * DAY, "confirmed": true});
    assert_eq!(blocked(&fixture).as_deref(), Some("MODE"));
    update(&fixture, json!({"allocationMode": "expiry-balanced"})).unwrap();
    assert_eq!(blocked(&fixture).as_deref(), Some("OPT_IN_OFF"));
    update(&fixture, json!({"useApiCreditAfterLimit": true, "balanceApiCredit": true})).unwrap();
    assert_eq!(blocked(&fixture).as_deref(), Some("GRANT_NONE"));
    update(&fixture, json!({"apiCreditGrant": grant_json})).unwrap();
    assert_eq!(blocked(&fixture).as_deref(), Some("NO_KEY"));
    fixture.save(&api_account("k", now));
    assert_eq!(blocked(&fixture), None);
    // 이미 일정보다 많이 쓴 허용 기간은 아직 열리지 않는다(예산 100달러 중 90달러를 하루 만에 썼다).
    spend(&fixture, "k", Some(90.0));
    assert_eq!(blocked(&fixture).as_deref(), Some("AHEAD_OF_PACE"));
    update(&fixture, json!({"apiCreditMonthlyLimitUsd": 50.0})).unwrap();
    assert_eq!(blocked(&fixture).as_deref(), Some("MONTHLY_LIMIT"));
}

fn status(fixture: &Fixture) -> aam_protocol::ApiCreditGrantStatus {
    fixture.service.snapshot().unwrap().api_credit_grant.unwrap()
}

/// 결제 정보는 Anthropic 구독(claude) 계정에만 연결되고, 날짜는 미래이며 주기별 범위 안이어야 한다. 거절된 요청은 정책을 바꾸지 않는다.
#[test]
fn billing_links_only_a_subscription_account_with_a_valid_future_date() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("sub", now));
    fixture.save(&api_account("key", now));
    let mut codex = account_fixture("codex", now);
    codex.provider = "openai".into();
    codex.tool = "codex".into();
    fixture.save(&codex);
    let mut observed = account_fixture("observed", now);
    observed.tool = "omp".into();
    fixture.save(&observed);
    let billing = |account: &str, cycle: &str, at: i64| json!({"apiCreditBilling": {"accountId": account, "cycle": cycle, "nextBillingAt": at}});
    for (account, cycle, at) in [
        ("key", "monthly", now + 5 * DAY),
        ("codex", "monthly", now + 5 * DAY),
        ("observed", "monthly", now + 5 * DAY),
        ("sub", "monthly", now),
        ("sub", "monthly", now - DAY),
        ("sub", "monthly", now + 36 * DAY),
        ("sub", "annual", now + 368 * DAY),
    ] {
        assert_eq!(update(&fixture, billing(account, cycle, at)).unwrap_err().code, "INVALID_PARAMS", "{account} {cycle} {at}");
    }
    assert_eq!(update(&fixture, billing("missing", "monthly", now + DAY)).unwrap_err().code, "ACCOUNT_NOT_FOUND");
    assert_eq!(update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "weekly", "nextBillingAt": now + DAY}})).unwrap_err().code, "INVALID_PARAMS");
    assert!(stored_policy(&fixture).api_credit_billing.is_none());

    let saved = update(&fixture, billing("sub", "monthly", now + 35 * DAY)).unwrap().api_credit_billing.unwrap();
    assert_eq!((saved.account_id.as_str(), saved.cycle, saved.next_billing_at), ("sub", aam_protocol::BillingCycle::Monthly, now + 35 * DAY));
    assert!(saved.updated_at >= now);
    // 필드를 빼면 변경하지 않고 null은 삭제다.
    update(&fixture, json!({"balanceApiCredit": true})).unwrap();
    assert!(stored_policy(&fixture).api_credit_billing.is_some());
    assert!(update(&fixture, json!({"apiCreditBilling": null})).unwrap().api_credit_billing.is_none());
    assert!(status(&fixture).billing.is_none());
}

/// 월 결제만 추정 만료일을 만든다. 연 결제·지난 날짜·유효하지 않은 계정은 이유와 함께 추정이 없고, 서비스는 날짜를 굴려 앞으로 옮기지 않는다.
#[test]
fn only_monthly_future_billing_yields_an_estimated_expiry() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("sub", now));
    let at = now + 20 * DAY;
    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "monthly", "nextBillingAt": at}})).unwrap();
    let billing = status(&fixture).billing.unwrap();
    assert_eq!((billing.estimated_expires_at, billing.estimate_blocked_by), (Some(at), None));
    assert_eq!(billing.next_billing_at, at);

    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "annual", "nextBillingAt": now + 200 * DAY}})).unwrap();
    let annual = status(&fixture).billing.unwrap();
    assert_eq!((annual.estimated_expires_at, annual.estimate_blocked_by.as_deref()), (None, Some("ANNUAL")));

    // 저장 뒤 날짜가 지나면(저장된 값은 그대로) 추정이 사라진다. 다음 달로 굴려 만들어 내지 않는다.
    let mut stored = stored_policy(&fixture);
    stored.api_credit_billing = Some(aam_protocol::ApiCreditBilling { account_id: "sub".into(), cycle: aam_protocol::BillingCycle::Monthly, next_billing_at: now - HOUR, updated_at: now });
    set_metadata(&fixture.service.lock().unwrap().connection, "policy", &stored).unwrap();
    let passed = status(&fixture).billing.unwrap();
    assert_eq!((passed.next_billing_at, passed.estimated_expires_at, passed.estimate_blocked_by.as_deref()), (now - HOUR, None, Some("DATE_PASSED")));

    // 저장 뒤 계정이 구독 계정이 아니게 되면(행이 없어도 마찬가지) 추정을 막는다.
    stored.api_credit_billing.as_mut().unwrap().next_billing_at = at;
    stored.api_credit_billing.as_mut().unwrap().account_id = "ghost".into();
    set_metadata(&fixture.service.lock().unwrap().connection, "policy", &stored).unwrap();
    assert_eq!(status(&fixture).billing.unwrap().estimate_blocked_by.as_deref(), Some("ACCOUNT_INVALID"));
}

/// 추정 만료일 허용 기간은 월 결제·미래 결제일·사용자 동의·정확한 일치가 모두 있어야 저장되고, 출처가 저장·표시되며, 확정으로 몰래 바뀌지 않는다.
#[test]
fn billing_estimate_grant_needs_monthly_billing_consent_and_never_becomes_exact_silently() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("sub", now));
    fixture.save(&api_account("key", now));
    let at = now + 12 * DAY;
    let estimate = |budget: f64, expires: i64, consent: bool| json!({"apiCreditGrant": {"budgetUsd": budget, "startsAt": now - HOUR, "expiresAt": expires, "confirmed": true, "expirySource": "billing-estimate", "estimateConfirmed": consent}});
    // 결제 정보 없이는 추정할 수 없다.
    assert_eq!(update(&fixture, estimate(40.0, at, true)).unwrap_err().code, "API_CREDIT_ESTIMATE_UNAVAILABLE");
    // 연 결제는 월 크레딧 만료로 쓰지 않는다.
    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "annual", "nextBillingAt": at}})).unwrap();
    assert_eq!(update(&fixture, estimate(40.0, at, true)).unwrap_err().code, "API_CREDIT_ESTIMATE_UNAVAILABLE");
    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "monthly", "nextBillingAt": at}})).unwrap();
    // 동의 없음·날짜 불일치는 거절하고 정책을 바꾸지 않는다.
    assert_eq!(update(&fixture, estimate(40.0, at, false)).unwrap_err().code, "API_CREDIT_ESTIMATE_CONFIRM_REQUIRED");
    assert_eq!(update(&fixture, estimate(40.0, at + DAY, true)).unwrap_err().code, "INVALID_PARAMS");
    assert!(stored_policy(&fixture).api_credit_grant.is_none());
    // `confirmed`가 없으면 추정이라도 저장하지 않는다.
    let mut unconfirmed = estimate(40.0, at, true);
    unconfirmed["apiCreditGrant"]["confirmed"] = json!(false);
    assert_eq!(update(&fixture, unconfirmed).unwrap_err().code, "API_CREDIT_GRANT_CONFIRM_REQUIRED");

    let saved = update(&fixture, estimate(40.0, at, true)).unwrap().api_credit_grant.unwrap();
    assert_eq!((saved.expiry_source, saved.expires_at), (aam_protocol::ExpirySource::BillingEstimate, at));
    assert_eq!(saved.billing_estimate, Some(aam_protocol::ApiCreditBillingBasis { account_id: "sub".into(), next_billing_at: at }));
    let shown = status(&fixture);
    assert_eq!((shown.expiry_source, shown.expires_at, shown.estimate_drift), (aam_protocol::ExpirySource::BillingEstimate, Some(at), false));

    // 사용액·id는 같은 id의 재저장에서 이어 가고, 추정으로 다시 동의하면 계속 추정이다.
    update(&fixture, json!({"allocationMode": "expiry-balanced", "useApiCreditAfterLimit": true, "balanceApiCredit": true})).unwrap();
    spend(&fixture, "key", Some(7.0));
    let mut again = estimate(35.0, at, true);
    again["apiCreditGrant"]["id"] = json!(saved.id);
    let kept = update(&fixture, again).unwrap().api_credit_grant.unwrap();
    assert_eq!((kept.id.as_str(), kept.expiry_source), (saved.id.as_str(), aam_protocol::ExpirySource::BillingEstimate));
    assert_eq!(status(&fixture).used_usd, 7.0);
    // 같은 날짜를 `exact`로 다시 저장해 추정을 확정으로 바꾸는 것은 거절한다. 날짜를 직접 확인해 바꾸면 exact가 된다.
    let exact = |expires: i64| json!({"apiCreditGrant": {"id": saved.id, "budgetUsd": 35.0, "startsAt": now - HOUR, "expiresAt": expires, "confirmed": true}});
    assert_eq!(update(&fixture, exact(at)).unwrap_err().code, "API_CREDIT_EXPIRY_SOURCE_REQUIRED");
    assert_eq!(stored_policy(&fixture).api_credit_grant.unwrap().expiry_source, aam_protocol::ExpirySource::BillingEstimate);
    let confirmed = update(&fixture, exact(at + 2 * DAY)).unwrap().api_credit_grant.unwrap();
    assert_eq!((confirmed.expiry_source, confirmed.billing_estimate, confirmed.id.as_str()), (aam_protocol::ExpirySource::Exact, None, saved.id.as_str()));
    assert_eq!(status(&fixture).used_usd, 7.0);
}

/// 결제 정보 변경·계정 삭제는 저장된 허용 기간(만료일·예산·사용액·id)을 바꾸지 않고 `estimateDrift`로만 드러난다. 기존 허용 기간은 exact다.
#[test]
fn billing_changes_never_move_a_saved_grant_and_old_grants_stay_exact() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("sub", now));
    fixture.save(&api_account("key", now));
    // 이전 버전이 저장한 허용 기간(새 필드 없음)은 exact로 읽힌다.
    let mut legacy = serde_json::to_value(stored_policy(&fixture)).unwrap();
    legacy["apiCreditGrant"] = json!({"id": "old", "budgetUsd": 20.0, "startsAt": now - HOUR, "expiresAt": now + 9 * DAY, "confirmedAt": now});
    set_metadata(&fixture.service.lock().unwrap().connection, "policy", &serde_json::from_value::<Policy>(legacy).unwrap()).unwrap();
    let old = status(&fixture);
    assert_eq!((old.expiry_source, old.billing_estimate, old.estimate_drift, old.billing.is_none()), (aam_protocol::ExpirySource::Exact, None, false, true));

    let at = now + 10 * DAY;
    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "monthly", "nextBillingAt": at}})).unwrap();
    assert_eq!(status(&fixture).expires_at, Some(now + 9 * DAY));
    update(&fixture, json!({"apiCreditGrant": null})).unwrap();
    let saved = update(&fixture, json!({"apiCreditGrant": {"budgetUsd": 40.0, "startsAt": now - HOUR, "expiresAt": at, "confirmed": true, "expirySource": "billing-estimate", "estimateConfirmed": true}})).unwrap().api_credit_grant.unwrap();
    spend(&fixture, "key", Some(3.0));

    // 다음 결제일을 바꿔도 허용 기간은 그대로이고, 근거가 달라졌다는 표시만 켜진다.
    let moved = at + 2 * DAY;
    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "monthly", "nextBillingAt": moved}})).unwrap();
    let drifted = status(&fixture);
    assert_eq!((drifted.id.as_deref(), drifted.expires_at, drifted.used_usd, drifted.estimate_drift), (Some(saved.id.as_str()), Some(at), 3.0, true));
    assert_eq!(drifted.billing.unwrap().estimated_expires_at, Some(moved));
    // 연 결제로 바꿔도 마찬가지다.
    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "annual", "nextBillingAt": moved}})).unwrap();
    assert_eq!((status(&fixture).expires_at, status(&fixture).estimate_drift), (Some(at), true));
    // 연결한 구독 계정을 지우면 결제 정보가 정리되고 허용 기간은 남는다.
    fixture.service.dispatch("account.remove", json!({"id": "sub"})).unwrap();
    let after = stored_policy(&fixture);
    assert!(after.api_credit_billing.is_none());
    assert_eq!(after.api_credit_grant.unwrap().expires_at, at);
    let orphan = status(&fixture);
    assert_eq!((orphan.billing.is_none(), orphan.estimate_drift, orphan.used_usd), (true, true, 3.0));
}

/// 리셋 전 사용(소비 균형 한정): 주간 한도가 96% 찬 계정이 곧(기본 48시간) 리셋되면 안전 여유량 10% 안쪽이어도 새 대화를 먼저 받는다.
/// 스마트·소비 순서(priority)는 그대로 여유 있는 계정을 고르고, 리셋이 멀거나 소진·5시간 과열 계정은 이 규칙으로 고르지 않는다.
#[test]
fn expiry_balanced_uses_reserve_leftover_near_reset_while_other_modes_keep_the_reserve() {
    let now = now_ms();
    let leftover = |used: f64, reset_in: i64| {
        let mut account = account_fixture("leftover", now);
        account.buckets = vec![bucket("weekly", None, used, now + reset_in, now)];
        account
    };
    let mut roomy = account_fixture("roomy", now);
    roomy.buckets = vec![bucket("weekly", None, 40.0, now + 5 * DAY, now)];
    let balanced = Policy { allocation_mode: AllocationMode::ExpiryBalanced, safety_reserve_percent: 10.0, ..Policy::default() };
    let pick = |policy: &Policy, accounts: &[Account]| scheduler::decide(accounts, &[], policy, &intent(), now).unwrap();
    let near = leftover(96.0, 40 * HOUR);

    // 소비 균형: 96% 사용·40시간 뒤 리셋 → 여유 계정이 있어도 남은 4%를 먼저 쓴다. 이유 코드도 남는다.
    let decision = pick(&balanced, &[near.clone(), roomy.clone()]);
    assert_eq!(decision.selected_account_id.as_deref(), Some("leftover"));
    assert!(reasons(&decision, "leftover").iter().any(|reason| reason.starts_with("USE_BEFORE_RESET:")));
    assert!(!reasons(&decision, "leftover").iter().any(|reason| reason.starts_with("RESERVE_")));
    // 같은 입력의 스마트·소비 순서는 기존대로 안전 여유량을 지킨다.
    let smart = Policy { allocation_mode: AllocationMode::Smart, ..balanced.clone() };
    let smart_decision = pick(&smart, &[near.clone(), roomy.clone()]);
    assert_eq!(smart_decision.selected_account_id.as_deref(), Some("roomy"));
    assert!(reasons(&smart_decision, "leftover").iter().any(|reason| reason.starts_with("RESERVE_DEPRIORITIZED:")));
    let priority = Policy { allocation_mode: AllocationMode::Priority, account_priority: vec!["leftover".into(), "roomy".into()], ..balanced.clone() };
    assert_eq!(pick(&priority, &[near.clone(), roomy.clone()]).selected_account_id.as_deref(), Some("roomy"));
    // 곧 리셋 우선 스위치를 켠 스마트도 안전 여유량 안쪽 계정을 고르지 않는다.
    let boosted = Policy { expiring_boost: true, ..smart.clone() };
    assert_eq!(pick(&boosted, &[near.clone(), roomy.clone()]).selected_account_id.as_deref(), Some("roomy"));

    // 리셋이 멀면(기본 48시간 밖) 여유량 규칙 그대로다.
    let far = leftover(96.0, 6 * DAY);
    let far_decision = pick(&balanced, &[far, roomy.clone()]);
    assert_eq!(far_decision.selected_account_id.as_deref(), Some("roomy"));
    assert!(reasons(&far_decision, "leftover").iter().any(|reason| reason.starts_with("RESERVE_DEPRIORITIZED:")));
    // 기준은 정책의 `expiringWindowHours`를 따른다: 60시간 뒤 리셋은 48시간이면 밖, 72시간이면 안이다.
    let sixty = leftover(96.0, 60 * HOUR);
    assert_eq!(pick(&balanced, &[sixty.clone(), roomy.clone()]).selected_account_id.as_deref(), Some("roomy"));
    let wide = Policy { expiring_window_hours: 72, ..balanced.clone() };
    assert_eq!(pick(&wide, &[sixty, roomy.clone()]).selected_account_id.as_deref(), Some("leftover"));
    // 안전 여유량 밖(여유가 충분한) 계정끼리는 기존 소비 균형 순서를 지킨다: 규칙은 여유량 안쪽 계정에만 적용된다.
    let mut plenty = account_fixture("plenty", now);
    plenty.buckets = vec![bucket("weekly", None, 20.0, now + DAY, now)];
    assert_eq!(pick(&balanced, &[plenty.clone(), roomy.clone()]).selected_account_id.as_deref(), Some("plenty"));

    // 소진(100%)·리셋 지난 관측·5시간 한도 과열·여유량 안쪽인 계정은 이 규칙으로 고르지 않는다.
    assert_eq!(pick(&balanced, &[spent_claude("leftover", now), roomy.clone()]).selected_account_id.as_deref(), Some("roomy"));
    let mut hot = leftover(96.0, 40 * HOUR);
    hot.buckets.push(bucket("5h", None, 90.0, now + 2 * HOUR, now));
    assert_eq!(pick(&balanced, &[hot, roomy.clone()]).selected_account_id.as_deref(), Some("roomy"));
    let mut warm = leftover(96.0, 40 * HOUR);
    warm.buckets.push(bucket("5h", None, 60.0, now + 2 * HOUR, now));
    assert_eq!(pick(&balanced, &[warm, roomy.clone()]).selected_account_id.as_deref(), Some("leftover"));
    let mut stale = leftover(96.0, 40 * HOUR);
    stale.buckets[0].observed_at = now - HOUR;
    assert_eq!(pick(&balanced, &[stale, roomy.clone()]).selected_account_id.as_deref(), Some("roomy"));
    // 새 배정에서 뺀 계정은 후보가 아니다.
    let mut off = leftover(96.0, 40 * HOUR);
    off.enabled = false;
    assert_eq!(pick(&balanced, &[off, roomy.clone()]).selected_account_id.as_deref(), Some("roomy"));

    // 안전 여유량이 0이면 규칙이 필요 없고 기존 소비 균형 점수가 그대로 이긴다(여유 있는 계정).
    let no_reserve = Policy { safety_reserve_percent: 0.0, ..balanced.clone() };
    let plain = pick(&no_reserve, &[near.clone(), roomy.clone()]);
    assert_eq!(plain.selected_account_id.as_deref(), Some("roomy"));
    assert!(!reasons(&plain, "leftover").iter().any(|reason| reason.starts_with("USE_BEFORE_RESET:")));

    // 여러 계정이 해당하면 리셋이 가장 이른 계정이 먼저다.
    let mut sooner = leftover(97.0, 10 * HOUR);
    sooner.id = "sooner".into();
    sooner.identity_key = Some("identity-sooner".into());
    sooner.profile_path = Some("/isolated/sooner".into());
    assert_eq!(pick(&balanced, &[near.clone(), sooner.clone(), roomy.clone()]).selected_account_id.as_deref(), Some("sooner"));
    assert_eq!(pick(&balanced, &[sooner, near.clone(), roomy.clone()]).selected_account_id.as_deref(), Some("sooner"));

    // 공급자 수동 배정은 기존대로 먼저다(리셋 전 사용 계정이 아니어도).
    let mut pinned = balanced.clone();
    pinned.provider_pins.insert("anthropic".into(), "roomy".into());
    assert_eq!(pick(&pinned, &[near.clone(), roomy.clone()]).selected_account_id.as_deref(), Some("roomy"));
    // 유일한 계정이면 모드와 무관하게 선택은 같고, 규칙은 다른 계정이 없을 때도 이유를 바꾸지 않는다(RESERVE_FALLBACK 대신 USE_BEFORE_RESET).
    let alone = pick(&balanced, std::slice::from_ref(&near));
    assert_eq!(alone.selected_account_id.as_deref(), Some("leftover"));
    assert!(reasons(&alone, "leftover").iter().any(|reason| reason.starts_with("USE_BEFORE_RESET:")));
}

/// 리셋 전 사용은 구독 안에서만 순서를 바꾼다: 모든 구독이 소진이어도 이 규칙이 API 키·유료 경로를 열지 않는다.
#[test]
fn use_before_reset_never_opens_paid_fallbacks_and_rule_boundaries_hold() {
    let now = now_ms();
    let window = 48 * HOUR;
    let weekly = |used: f64, reset_in: i64| vec![bucket("weekly", None, used, now + reset_in, now)];
    let rule = |buckets: &[QuotaBucket], reserve: f64| scheduler::use_before_reset(buckets.iter(), reserve, window, now);
    // 경계: 정확히 창 끝은 포함, 1ms 넘으면 제외. 리셋이 지났거나 시각이 없으면 제외.
    assert_eq!(rule(&weekly(96.0, window), 10.0), Some(now + window));
    assert_eq!(rule(&weekly(96.0, window + 1), 10.0), None);
    assert_eq!(rule(&weekly(96.0, -1), 10.0), None);
    let mut no_reset = weekly(96.0, HOUR);
    no_reset[0].resets_at = None;
    assert_eq!(rule(&no_reset, 10.0), None);
    // 소진(100%)·상태 exhausted·여유량 0·창 0은 제외.
    assert_eq!(rule(&weekly(100.0, HOUR), 10.0), None);
    let mut flagged = weekly(99.0, HOUR);
    flagged[0].status = "exhausted".into();
    assert_eq!(rule(&flagged, 10.0), None);
    assert_eq!(rule(&weekly(96.0, HOUR), 0.0), None);
    assert_eq!(scheduler::use_before_reset(weekly(96.0, HOUR).iter(), 10.0, 0, now), None);
    // 긴 주기 한도가 없으면(5시간만) 제외. 5시간 한도는 안전 여유량 안쪽이거나 85% 이상이면 제외.
    assert_eq!(rule(&[bucket("5h", None, 30.0, now + HOUR, now)], 10.0), None);
    let mut both = weekly(96.0, HOUR);
    both.push(bucket("5h", None, 84.0, now + HOUR, now));
    assert_eq!(rule(&both, 10.0), Some(now + HOUR));
    both[1].used_percent = Some(85.0);
    assert_eq!(rule(&both, 10.0), None);
    // 가장 많이 쓴 긴 주기 한도가 기준이다: 더 빡빡한 한도가 멀리 리셋되면 가까운 쪽이 곧 풀려도 제외한다.
    let mut two = vec![bucket("weekly", None, 96.0, now + 5 * DAY, now), bucket("weekly-opus", None, 50.0, now + HOUR, now)];
    assert_eq!(rule(&two, 10.0), None);
    two[1].used_percent = Some(97.0);
    assert_eq!(rule(&two, 10.0), Some(now + HOUR));

    // 모든 구독이 소진이면 규칙은 아무것도 열지 않는다: 크레딧·API 키는 기존 옵트인 경로로만 열린다.
    let spent = [spent_claude("a", now), spent_claude("b", now)];
    let api = api_account("api", now);
    let policy = Policy { allocation_mode: AllocationMode::ExpiryBalanced, safety_reserve_percent: 10.0, ..Policy::default() };
    let none = scheduler::decide(&[spent[0].clone(), spent[1].clone(), api.clone()], &[], &policy, &intent(), now).unwrap();
    assert_eq!(none.selected_account_id, None);
    assert!(!scheduler::chose_api_credit(&none));
    // 곧 리셋될 계정(96%)과 소진 계정이 섞이면 소진 계정이 아니라 남은 계정이 받는다. API 키는 후보에도 오르지 않는다.
    let mut near = account_fixture("near", now);
    near.buckets = weekly(96.0, 20 * HOUR);
    let mixed_policy = Policy { use_api_credit_after_limit: true, ..policy.clone() };
    let mixed = scheduler::decide(&[spent[0].clone(), near, api], &[], &mixed_policy, &intent(), now).unwrap();
    assert_eq!(mixed.selected_account_id.as_deref(), Some("near"));
    assert!(!scheduler::chose_api_credit(&mixed));
    assert!(mixed.candidates.iter().all(|candidate| candidate.account_id != "api"));
}
