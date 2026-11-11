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

fn grant(id: &str, keys: &[&str], budget: f64, starts_at: i64, expires_at: i64) -> ApiCreditGrant {
    ApiCreditGrant { id: id.into(), account_ids: keys.iter().map(|key| (*key).into()).collect(), budget_usd: budget, starts_at, expires_at, confirmed_at: starts_at, source: "manual".into(), expiry_source: Default::default(), billing_estimate: None }
}

fn pool_input(keys: &[&str], budget: f64, starts_at: i64, expires_at: i64) -> Value {
    json!({"apiCreditGrant": {"accountIds": keys, "budgetUsd": budget, "startsAt": starts_at, "expiresAt": expires_at, "confirmed": true}})
}

fn pools(fixture: &Fixture) -> Vec<aam_protocol::ApiCreditGrantStatus> {
    fixture.service.snapshot().unwrap().api_credit_grants
}

fn pool(fixture: &Fixture, id: &str) -> aam_protocol::ApiCreditGrantStatus {
    pools(fixture).into_iter().find(|pool| pool.id == id).unwrap()
}

fn ledger_of(id: &str, used_usd: f64) -> scheduler::ApiLedger {
    scheduler::ApiLedger { grants: [(id.to_owned(), scheduler::GrantUsage { used_usd, unmetered: 0 })].into(), ..Default::default() }
}

fn update(fixture: &Fixture, mut patch: Value) -> Result<Policy, ApiError> {
    let revision = policy(&fixture.service.lock().unwrap().connection).unwrap().revision;
    patch["expectedRevision"] = json!(revision);
    fixture.service.dispatch("policy.update", patch).map(|value| serde_json::from_value(value).unwrap())
}

fn stored_policy(fixture: &Fixture) -> Policy {
    policy(&fixture.service.lock().unwrap().connection).unwrap()
}

/// 세션 없이 기록한다(지금 그 키가 속한 풀). 세션의 입장 풀로 기록하는 경로는 `apiCredit.record`(sessionId)다.
fn spend(fixture: &Fixture, id: &str, cost: Option<f64>) {
    let pool = scheduler::pool_of(&stored_policy(fixture), id).map(|grant| grant.id.clone());
    record_api_credit(&mut fixture.service.lock().unwrap().connection, id, pool.as_deref(), cost, now_ms()).unwrap();
}

fn reasons<'a>(decision: &'a aam_protocol::Decision, id: &str) -> &'a [String] {
    &decision.candidates.iter().find(|candidate| candidate.account_id == id).unwrap().reasons
}

/// 풀은 사용자가 확인했을 때만 저장되고, 잘못된 값은 정책을 바꾸지 않으며, 같은 id만 쓴 금액을 이어 간다. 키 하나는 풀 하나에만 들어간다.
#[test]
fn pool_input_needs_confirmation_valid_dates_registered_keys_and_one_pool_per_key() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&api_account("k", now));
    fixture.save(&api_account("j", now));
    fixture.save(&account_fixture("sub", now));
    let mut unconfirmed = pool_input(&["k"], 50.0, now - HOUR, now + 10 * DAY);
    unconfirmed["apiCreditGrant"]["confirmed"] = json!(false);
    assert_eq!(update(&fixture, unconfirmed).unwrap_err().code, "API_CREDIT_GRANT_CONFIRM_REQUIRED");
    for bad in [
        json!({"accountIds": ["k"], "budgetUsd": 0.0, "startsAt": now, "expiresAt": now + DAY, "confirmed": true}),
        json!({"accountIds": ["k"], "budgetUsd": 5.0, "startsAt": now, "expiresAt": now, "confirmed": true}),
        json!({"accountIds": ["k"], "budgetUsd": 5.0, "startsAt": now - 2 * DAY, "expiresAt": now - DAY, "confirmed": true}),
        json!({"accountIds": ["k"], "budgetUsd": 5.0, "startsAt": now, "expiresAt": now + 400 * DAY, "confirmed": true}),
        json!({"accountIds": ["k"], "budgetUsd": 5.0, "startsAt": now, "expiresAt": now + DAY, "confirmed": true, "id": "unknown"}),
        // 키 없음·없는 키·구독 계정은 풀에 넣지 못한다.
        json!({"accountIds": [], "budgetUsd": 5.0, "startsAt": now, "expiresAt": now + DAY, "confirmed": true}),
        json!({"accountIds": ["ghost"], "budgetUsd": 5.0, "startsAt": now, "expiresAt": now + DAY, "confirmed": true}),
        json!({"accountIds": ["sub"], "budgetUsd": 5.0, "startsAt": now, "expiresAt": now + DAY, "confirmed": true}),
    ] {
        assert_eq!(update(&fixture, json!({"apiCreditGrant": bad})).unwrap_err().code, "INVALID_PARAMS", "{bad}");
    }
    assert!(stored_policy(&fixture).api_credit_grants.is_empty());

    let first = update(&fixture, pool_input(&["k"], 50.0, now - HOUR, now + 10 * DAY)).unwrap().api_credit_grants;
    assert_eq!(first.len(), 1);
    let first = first[0].clone();
    assert_eq!((first.budget_usd, first.source.as_str(), first.account_ids.clone()), (50.0, "manual", vec!["k".to_owned()]));
    assert!(first.confirmed_at >= now);
    // 같은 키를 다른 새 풀에 넣으면 거절한다(같은 잔액을 두 번 세지 않도록).
    assert_eq!(update(&fixture, pool_input(&["k", "j"], 30.0, now - HOUR, now + 5 * DAY)).unwrap_err().code, "API_CREDIT_KEY_IN_OTHER_POOL");
    // 필드를 빼면 변경하지 않는다.
    update(&fixture, json!({"balanceApiCredit": true})).unwrap();
    assert_eq!(stored_policy(&fixture).api_credit_grants, vec![first.clone()]);
    spend(&fixture, "k", Some(12.0));
    // 같은 id의 수정은 쓴 금액을 이어 가고, 자기 풀의 키는 그대로 다시 보낼 수 있다.
    let edited = update(&fixture, json!({"apiCreditGrant": {"id": first.id, "accountIds": ["k"], "budgetUsd": 80.0, "startsAt": now - HOUR, "expiresAt": now + 20 * DAY, "confirmed": true}})).unwrap();
    assert_eq!(edited.api_credit_grants.len(), 1);
    assert_eq!((pool(&fixture, &first.id).used_usd, pool(&fixture, &first.id).remaining_usd), (12.0, 68.0));
    // 다른 키로 새 풀을 만들면 0부터 센다.
    let second = update(&fixture, pool_input(&["j"], 30.0, now - HOUR, now + 5 * DAY)).unwrap().api_credit_grants;
    assert_eq!(second.len(), 2);
    let second = second.into_iter().find(|grant| grant.id != first.id).unwrap();
    assert_eq!((pool(&fixture, &second.id).state.as_str(), pool(&fixture, &second.id).used_usd, pool(&fixture, &second.id).remaining_usd), ("active", 0.0, 30.0));
    // 삭제는 그 풀만 지우고, 없는 id는 거절한다. 다시 만든 풀은 0부터 센다.
    assert_eq!(update(&fixture, json!({"apiCreditGrantRemove": "unknown"})).unwrap_err().code, "INVALID_PARAMS");
    let left = update(&fixture, json!({"apiCreditGrantRemove": first.id})).unwrap().api_credit_grants;
    assert_eq!(left.iter().map(|grant| grant.id.as_str()).collect::<Vec<_>>(), vec![second.id.as_str()]);
    let again = update(&fixture, pool_input(&["k"], 50.0, now - HOUR, now + 10 * DAY)).unwrap().api_credit_grants;
    let again = again.into_iter().find(|grant| grant.id != second.id).unwrap();
    assert_eq!(pool(&fixture, &again.id).used_usd, 0.0);
}

/// 사용액은 그 키가 속한 풀에만 쌓인다: 다른 조직(다른 풀)의 사용은 섞이지 않고, 같은 풀의 키는 잔액을 나눠 쓰며,
/// 지운 키의 늦은 기록도 같은 풀에 남는다. 기간 밖 사용은 예산에 넣지 않고, 미확인 요청은 그 풀의 선제 사용만 막는다.
#[test]
fn spend_goes_only_to_the_keys_own_pool_and_closes_proactive_use_per_pool() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&spent_claude("sub", now));
    for key in ["a1", "a2", "b"] {
        fixture.save(&api_account(key, now));
    }
    update(&fixture, json!({"allocationMode": "expiry-balanced", "useApiCreditAfterLimit": true, "balanceApiCredit": true})).unwrap();
    let org_a = update(&fixture, pool_input(&["a1", "a2"], 50.0, now - HOUR, now + 10 * DAY)).unwrap().api_credit_grants[0].id.clone();
    let org_b = update(&fixture, pool_input(&["b"], 100.0, now - HOUR, now + 20 * DAY)).unwrap().api_credit_grants.into_iter().find(|grant| grant.id != org_a).unwrap().id;
    // 확인 직후에는 일정보다 덜 쓴 상태라 두 풀 모두 선제 사용이 열려 있다.
    assert!(pools(&fixture).iter().all(|pool| pool.proactive.allowed), "{:?}", pools(&fixture));
    // 같은 조직의 두 키는 한 잔액을 나눠 쓰고, 다른 조직 풀은 그대로다.
    spend(&fixture, "a1", Some(10.0));
    spend(&fixture, "a2", Some(15.0));
    let a = pool(&fixture, &org_a);
    assert_eq!((a.state.as_str(), a.used_usd, a.remaining_usd), ("active", 25.0, 25.0));
    assert_eq!(a.proactive.blocked_by.as_deref(), Some("AHEAD_OF_PACE"));
    assert!(a.per_day_usd.unwrap() > 2.0);
    let b = pool(&fixture, &org_b);
    assert_eq!((b.used_usd, b.remaining_usd, b.proactive.allowed), (0.0, 100.0, true));
    // 키를 지워도 풀과 그 키의 늦게 끝나는 요청 기록은 같은 풀에 남는다.
    fixture.service.dispatch("account.remove", json!({"id": "a1"})).unwrap();
    assert!(stored_policy(&fixture).api_credit_grants.iter().any(|grant| grant.id == org_a && grant.account_ids.contains(&"a1".to_owned())));
    spend(&fixture, "a1", Some(5.0));
    assert_eq!((pool(&fixture, &org_a).used_usd, pool(&fixture, &org_a).remaining_usd), (30.0, 20.0));
    // 미확인 요청은 그 키의 풀만 닫는다.
    spend(&fixture, "a2", None);
    let a = pool(&fixture, &org_a);
    assert!(a.unmetered && a.unmetered_requests == 1);
    assert_eq!((a.proactive.allowed, a.proactive.blocked_by.as_deref()), (false, Some("GRANT_UNMETERED")));
    assert!(pool(&fixture, &org_b).proactive.allowed);
    // 풀 A의 예산을 넘기면 A만 소진이다. 한도 뒤 fallback은 A의 키를 빼고 B의 키로 간다.
    spend(&fixture, "a2", Some(30.0));
    let a = pool(&fixture, &org_a);
    assert_eq!((a.state.as_str(), a.remaining_usd, a.proactive.blocked_by.as_deref()), ("exhausted", 0.0, Some("GRANT_EXHAUSTED")));
    let grant = fixture.service.acquire({ let mut request = request("after-limit"); request.intent.non_interactive = true; request }, now).unwrap();
    assert_eq!(grant.account.id, "b");
    // 아직 시작하지 않은 기간의 사용은 예산에 넣지 않는다.
    update(&fixture, json!({"apiCreditGrant": {"id": org_b, "accountIds": ["b"], "budgetUsd": 40.0, "startsAt": now + HOUR, "expiresAt": now + 5 * DAY, "confirmed": true}})).unwrap();
    spend(&fixture, "b", Some(7.0));
    let b = pool(&fixture, &org_b);
    assert_eq!((b.state.as_str(), b.used_usd, b.proactive.blocked_by.as_deref()), ("upcoming", 0.0, Some("GRANT_PENDING")));
    // 월 합계 원장은 풀과 무관하게 계속 센다.
    let monthly = account(&fixture.service.lock().unwrap().connection, "a2").unwrap().api_credit.unwrap().used_usd;
    assert!((monthly - 15.0 - 30.0).abs() < 1e-9, "{monthly}");
}

/// 키가 속한 풀은 진행 중일 때만 그 키의 한도 뒤 API fallback을 연다(`API_GRANT_*`). 풀에 없는 키는 기존 동작 그대로다.
#[test]
fn a_keys_pool_gates_its_after_limit_api_fallback_and_keys_outside_pools_keep_it_unchanged() {
    let now = now_ms();
    let accounts = [spent_claude("sub", now), api_account("api", now)];
    let policy = Policy { use_api_credit_after_limit: true, ..Policy::default() };
    let pick = |policy: &Policy, ledger: &scheduler::ApiLedger| scheduler::decide_with(&accounts, &[], policy, &headless(), now, &Default::default(), ledger).unwrap();
    let open = pick(&policy, &scheduler::ApiLedger::default());
    assert_eq!(open.selected_account_id.as_deref(), Some("api"));
    for (grant, ledger, code) in [
        (grant("g", &["api"], 50.0, now - 2 * DAY, now - DAY), scheduler::ApiLedger::default(), "API_GRANT_EXPIRED"),
        (grant("g", &["api"], 50.0, now + DAY, now + 5 * DAY), scheduler::ApiLedger::default(), "API_GRANT_PENDING"),
        (grant("g", &["api"], 50.0, now - DAY, now + 5 * DAY), ledger_of("g", 50.0), "API_GRANT_EXHAUSTED"),
    ] {
        let gated = Policy { api_credit_grants: vec![grant], ..policy.clone() };
        let decision = pick(&gated, &ledger);
        assert_eq!(decision.selected_account_id, None, "{code}");
        assert!(reasons(&decision, "api").iter().any(|reason| reason.starts_with(code)), "{code}");
    }
    let active = Policy { api_credit_grants: vec![grant("g", &["api"], 50.0, now - DAY, now + 5 * DAY)], ..policy.clone() };
    assert_eq!(pick(&active, &ledger_of("g", 49.0)).selected_account_id.as_deref(), Some("api"));
    // 다른 키의 풀이 끝났어도 이 키는 막히지 않는다.
    let other = Policy { api_credit_grants: vec![grant("g", &["other"], 50.0, now - 2 * DAY, now - DAY)], ..policy.clone() };
    assert_eq!(pick(&other, &scheduler::ApiLedger::default()).selected_account_id.as_deref(), Some("api"));
    // 월 상한은 풀과 별개로 계속 지키되 키마다 따로다: 지운 키의 원장 사용액은 이 키를 막지 않고, 이 키 자신이 상한에 닿아야 막힌다.
    let capped = Policy { api_credit_monthly_limit_usd: 5.0, ..active.clone() };
    let removed_only = scheduler::ApiLedger { removed_usd: 6.0, ..ledger_of("g", 0.0) };
    assert_eq!(pick(&capped, &removed_only).selected_account_id.as_deref(), Some("api"));
    let mut used = api_account("api", now);
    used.api_credit.as_mut().unwrap().used_usd = 6.0;
    let over = scheduler::decide_with(&[spent_claude("sub", now), used], &[], &capped, &headless(), now, &Default::default(), &ledger_of("g", 0.0)).unwrap();
    assert_eq!(over.selected_account_id, None);
    assert!(reasons(&over, "api").iter().any(|reason| reason.starts_with("API_CREDIT_LIMIT")));
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
        api_credit_grants: vec![grant("g", &["api"], 500.0, now - HOUR, now + 30 * DAY)],
        ..balanced.clone()
    };
    for non_interactive in [false, true] {
        let request = LaunchIntent { non_interactive, ..intent() };
        let decision = scheduler::decide(&[max.clone(), pro.clone(), api.clone()], &[], &everything, &request, now).unwrap();
        assert_eq!(decision.selected_account_id.as_deref(), Some("pro"), "non_interactive={non_interactive}");
        assert!(decision.candidates.iter().all(|candidate| candidate.account_id != "api"));
    }
}

/// 상태 표시: 풀의 선제 사용이 막힌 이유는 모드 → 옵트인 → 기간 → 키 → 월 상한 → 일정 순으로 한 가지를 알려 준다.
#[test]
fn proactive_status_names_the_first_blocker() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&api_account("k", now));
    let id = update(&fixture, pool_input(&["k"], 100.0, now - HOUR, now + 10 * DAY)).unwrap().api_credit_grants[0].id.clone();
    let blocked = |fixture: &Fixture| pool(fixture, &id).proactive.blocked_by;
    assert_eq!(blocked(&fixture).as_deref(), Some("MODE"));
    update(&fixture, json!({"allocationMode": "expiry-balanced"})).unwrap();
    assert_eq!(blocked(&fixture).as_deref(), Some("OPT_IN_OFF"));
    update(&fixture, json!({"useApiCreditAfterLimit": true, "balanceApiCredit": true})).unwrap();
    assert_eq!(blocked(&fixture), None);
    // 풀의 키를 배정에서 빼면 쓸 키가 없다.
    fixture.service.dispatch("account.update", json!({"id": "k", "enabled": false})).unwrap();
    assert_eq!(blocked(&fixture).as_deref(), Some("NO_KEY"));
    fixture.service.dispatch("account.update", json!({"id": "k", "enabled": true})).unwrap();
    // 이미 일정보다 많이 쓴 풀은 아직 열리지 않는다(예산 100달러 중 90달러를 하루 만에 썼다).
    spend(&fixture, "k", Some(90.0));
    assert_eq!(blocked(&fixture).as_deref(), Some("AHEAD_OF_PACE"));
    update(&fixture, json!({"apiCreditMonthlyLimitUsd": 50.0})).unwrap();
    assert_eq!(blocked(&fixture).as_deref(), Some("MONTHLY_LIMIT"));
}

/// 같은 풀의 키들은 월 상한을 각자 따로 적용한다: 각각 $150인 두 키(합계 $300 > 상한 $200)는 둘 다 쓸 수 있어 풀의 선제 사용이 막히지 않고,
/// 한 키가 상한에 닿아도 다른 키가 있으면 풀은 `monthlyLimitReached`가 아니다. 풀의 모든 키가 각자 상한에 닿았을 때만 `MONTHLY_LIMIT`다.
/// 풀에 속한 키가 각자 다른 풀이어도 서로의 상한을 공유하지 않는다.
#[test]
fn pool_monthly_limit_reached_only_when_every_member_key_is_at_its_own_cap() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&api_account("a", now));
    fixture.save(&api_account("b", now));
    update(&fixture, json!({"allocationMode": "expiry-balanced", "useApiCreditAfterLimit": true, "balanceApiCredit": true, "apiCreditMonthlyLimitUsd": 200.0})).unwrap();
    let shared = update(&fixture, pool_input(&["a", "b"], 1000.0, now - HOUR, now + 10 * DAY)).unwrap().api_credit_grants[0].id.clone();
    spend(&fixture, "a", Some(150.0));
    spend(&fixture, "b", Some(150.0));
    let status = pool(&fixture, &shared);
    assert!(!status.monthly_limit_reached);
    assert_ne!(status.proactive.blocked_by.as_deref(), Some("MONTHLY_LIMIT"));
    spend(&fixture, "a", Some(50.0));
    let status = pool(&fixture, &shared);
    assert!(!status.monthly_limit_reached, "b는 아직 상한 아래다");
    assert_ne!(status.proactive.blocked_by.as_deref(), Some("MONTHLY_LIMIT"));
    spend(&fixture, "b", Some(50.0));
    let status = pool(&fixture, &shared);
    assert!(status.monthly_limit_reached);
    assert_eq!(status.proactive.blocked_by.as_deref(), Some("MONTHLY_LIMIT"));
    // 각자 자기 풀을 가진 두 키($150씩)도 둘 다 상한($200) 아래라 막히지 않는다.
    let other = Fixture::new();
    other.save(&api_account("c", now));
    other.save(&api_account("d", now));
    update(&other, json!({"allocationMode": "expiry-balanced", "useApiCreditAfterLimit": true, "balanceApiCredit": true, "apiCreditMonthlyLimitUsd": 200.0})).unwrap();
    update(&other, pool_input(&["c"], 200.0, now - HOUR, now + 10 * DAY)).unwrap();
    update(&other, pool_input(&["d"], 200.0, now - HOUR, now + 10 * DAY)).unwrap();
    spend(&other, "c", Some(150.0));
    spend(&other, "d", Some(150.0));
    assert!(pools(&other).iter().all(|pool| !pool.monthly_limit_reached && pool.proactive.blocked_by.as_deref() != Some("MONTHLY_LIMIT")), "{:?}", pools(&other));
}

fn billing_of(fixture: &Fixture, account: &str) -> Option<aam_protocol::ApiCreditBillingStatus> {
    fixture.service.snapshot().unwrap().api_credit_billings.into_iter().find(|billing| billing.account_id == account)
}

/// 결제 정보는 Anthropic 구독(claude) 계정마다 하나이고, 날짜는 미래이며 주기별 범위 안이어야 한다. 거절된 요청은 정책을 바꾸지 않는다.
#[test]
fn billing_links_only_a_subscription_account_with_a_valid_future_date() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("sub", now));
    let mut sub2 = account_fixture("sub2", now);
    sub2.identity_key = Some("identity-sub2".into());
    sub2.profile_path = Some("/isolated/sub2".into());
    fixture.save(&sub2);
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
    assert!(stored_policy(&fixture).api_credit_billings.is_empty());

    let saved = update(&fixture, billing("sub", "monthly", now + 35 * DAY)).unwrap().api_credit_billings;
    assert_eq!(saved.len(), 1);
    assert_eq!((saved[0].account_id.as_str(), saved[0].cycle, saved[0].next_billing_at), ("sub", aam_protocol::BillingCycle::Monthly, now + 35 * DAY));
    assert!(saved[0].updated_at >= now);
    // 다른 구독 계정은 따로 저장되고, 같은 계정은 덮어쓴다.
    update(&fixture, billing("sub2", "monthly", now + 10 * DAY)).unwrap();
    let replaced = update(&fixture, billing("sub", "monthly", now + 20 * DAY)).unwrap().api_credit_billings;
    assert_eq!(replaced.iter().map(|billing| (billing.account_id.as_str(), billing.next_billing_at)).collect::<Vec<_>>(), vec![("sub", now + 20 * DAY), ("sub2", now + 10 * DAY)]);
    // 필드를 빼면 변경하지 않고, 삭제는 그 계정만 지운다.
    update(&fixture, json!({"balanceApiCredit": true})).unwrap();
    assert_eq!(stored_policy(&fixture).api_credit_billings.len(), 2);
    assert_eq!(update(&fixture, json!({"apiCreditBillingRemove": "missing"})).unwrap_err().code, "INVALID_PARAMS");
    let left = update(&fixture, json!({"apiCreditBillingRemove": "sub"})).unwrap().api_credit_billings;
    assert_eq!(left.iter().map(|billing| billing.account_id.as_str()).collect::<Vec<_>>(), vec!["sub2"]);
    assert!(billing_of(&fixture, "sub").is_none());
}

/// 월 결제만 추정 만료일을 만든다. 연 결제·지난 날짜·유효하지 않은 계정은 이유와 함께 추정이 없고, 서비스는 날짜를 굴려 앞으로 옮기지 않는다.
#[test]
fn only_monthly_future_billing_yields_an_estimated_expiry() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("sub", now));
    let at = now + 20 * DAY;
    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "monthly", "nextBillingAt": at}})).unwrap();
    let billing = billing_of(&fixture, "sub").unwrap();
    assert_eq!((billing.estimated_expires_at, billing.estimate_blocked_by), (Some(at), None));
    assert_eq!(billing.next_billing_at, at);

    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "annual", "nextBillingAt": now + 200 * DAY}})).unwrap();
    let annual = billing_of(&fixture, "sub").unwrap();
    assert_eq!((annual.estimated_expires_at, annual.estimate_blocked_by.as_deref()), (None, Some("ANNUAL")));

    // 저장 뒤 날짜가 지나면(저장된 값은 그대로) 추정이 사라진다. 다음 달로 굴려 만들어 내지 않는다.
    let mut stored = stored_policy(&fixture);
    stored.api_credit_billings = vec![aam_protocol::ApiCreditBilling { account_id: "sub".into(), cycle: aam_protocol::BillingCycle::Monthly, next_billing_at: now - HOUR, updated_at: now }];
    set_metadata(&fixture.service.lock().unwrap().connection, "policy", &stored).unwrap();
    let passed = billing_of(&fixture, "sub").unwrap();
    assert_eq!((passed.next_billing_at, passed.estimated_expires_at, passed.estimate_blocked_by.as_deref()), (now - HOUR, None, Some("DATE_PASSED")));

    // 저장 뒤 계정이 구독 계정이 아니게 되면(행이 없어도 마찬가지) 추정을 막는다.
    stored.api_credit_billings[0].next_billing_at = at;
    stored.api_credit_billings[0].account_id = "ghost".into();
    set_metadata(&fixture.service.lock().unwrap().connection, "policy", &stored).unwrap();
    assert_eq!(billing_of(&fixture, "ghost").unwrap().estimate_blocked_by.as_deref(), Some("ACCOUNT_INVALID"));
}

/// 추정 만료일 풀은 고른 구독 계정의 월 결제·미래 결제일·사용자 동의·정확한 일치가 모두 있어야 저장되고, 출처가 저장·표시되며, 확정으로 몰래 바뀌지 않는다.
#[test]
fn billing_estimate_pool_needs_monthly_billing_consent_and_never_becomes_exact_silently() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("sub", now));
    fixture.save(&api_account("key", now));
    let at = now + 12 * DAY;
    let estimate = |budget: f64, expires: i64, consent: bool, billing: Option<&str>| json!({"apiCreditGrant": {"accountIds": ["key"], "budgetUsd": budget, "startsAt": now - HOUR, "expiresAt": expires, "confirmed": true, "expirySource": "billing-estimate", "estimateConfirmed": consent, "billingAccountId": billing}});
    // 결제 정보 없이는 추정할 수 없다.
    assert_eq!(update(&fixture, estimate(40.0, at, true, Some("sub"))).unwrap_err().code, "API_CREDIT_ESTIMATE_UNAVAILABLE");
    // 연 결제는 월 크레딧 만료로 쓰지 않는다.
    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "annual", "nextBillingAt": at}})).unwrap();
    assert_eq!(update(&fixture, estimate(40.0, at, true, Some("sub"))).unwrap_err().code, "API_CREDIT_ESTIMATE_UNAVAILABLE");
    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "monthly", "nextBillingAt": at}})).unwrap();
    // 근거 계정을 고르지 않음·동의 없음·날짜 불일치는 거절하고 정책을 바꾸지 않는다.
    assert_eq!(update(&fixture, estimate(40.0, at, true, None)).unwrap_err().code, "API_CREDIT_ESTIMATE_UNAVAILABLE");
    assert_eq!(update(&fixture, estimate(40.0, at, false, Some("sub"))).unwrap_err().code, "API_CREDIT_ESTIMATE_CONFIRM_REQUIRED");
    assert_eq!(update(&fixture, estimate(40.0, at + DAY, true, Some("sub"))).unwrap_err().code, "INVALID_PARAMS");
    assert!(stored_policy(&fixture).api_credit_grants.is_empty());
    // `confirmed`가 없으면 추정이라도 저장하지 않는다.
    let mut unconfirmed = estimate(40.0, at, true, Some("sub"));
    unconfirmed["apiCreditGrant"]["confirmed"] = json!(false);
    assert_eq!(update(&fixture, unconfirmed).unwrap_err().code, "API_CREDIT_GRANT_CONFIRM_REQUIRED");

    let saved = update(&fixture, estimate(40.0, at, true, Some("sub"))).unwrap().api_credit_grants[0].clone();
    assert_eq!((saved.expiry_source, saved.expires_at), (aam_protocol::ExpirySource::BillingEstimate, at));
    assert_eq!(saved.billing_estimate, Some(aam_protocol::ApiCreditBillingBasis { account_id: "sub".into(), next_billing_at: at }));
    let shown = pool(&fixture, &saved.id);
    assert_eq!((shown.expiry_source, shown.expires_at, shown.estimate_drift), (aam_protocol::ExpirySource::BillingEstimate, at, false));

    // 사용액·id는 같은 id의 재저장에서 이어 가고, 추정으로 다시 동의하면 계속 추정이다.
    update(&fixture, json!({"allocationMode": "expiry-balanced", "useApiCreditAfterLimit": true, "balanceApiCredit": true})).unwrap();
    spend(&fixture, "key", Some(7.0));
    let mut again = estimate(35.0, at, true, Some("sub"));
    again["apiCreditGrant"]["id"] = json!(saved.id);
    let kept = update(&fixture, again).unwrap().api_credit_grants[0].clone();
    assert_eq!((kept.id.as_str(), kept.expiry_source), (saved.id.as_str(), aam_protocol::ExpirySource::BillingEstimate));
    assert_eq!(pool(&fixture, &saved.id).used_usd, 7.0);
    // 같은 날짜를 `exact`로 다시 저장해 추정을 확정으로 바꾸는 것은 거절한다. 날짜를 직접 확인해 바꾸면 exact가 된다.
    let exact = |expires: i64| json!({"apiCreditGrant": {"id": saved.id, "accountIds": ["key"], "budgetUsd": 35.0, "startsAt": now - HOUR, "expiresAt": expires, "confirmed": true}});
    assert_eq!(update(&fixture, exact(at)).unwrap_err().code, "API_CREDIT_EXPIRY_SOURCE_REQUIRED");
    assert_eq!(stored_policy(&fixture).api_credit_grants[0].expiry_source, aam_protocol::ExpirySource::BillingEstimate);
    let confirmed = update(&fixture, exact(at + 2 * DAY)).unwrap().api_credit_grants[0].clone();
    assert_eq!((confirmed.expiry_source, confirmed.billing_estimate, confirmed.id.as_str()), (aam_protocol::ExpirySource::Exact, None, saved.id.as_str()));
    assert_eq!(pool(&fixture, &saved.id).used_usd, 7.0);
}

/// 결제 정보 변경·계정 삭제는 저장된 풀(만료일·예산·사용액·id)을 바꾸지 않고 `estimateDrift`로만 드러난다.
#[test]
fn billing_changes_never_move_a_saved_pool() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("sub", now));
    fixture.save(&api_account("key", now));
    let at = now + 10 * DAY;
    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "monthly", "nextBillingAt": at}})).unwrap();
    let saved = update(&fixture, json!({"apiCreditGrant": {"accountIds": ["key"], "budgetUsd": 40.0, "startsAt": now - HOUR, "expiresAt": at, "confirmed": true, "expirySource": "billing-estimate", "estimateConfirmed": true, "billingAccountId": "sub"}})).unwrap().api_credit_grants[0].clone();
    spend(&fixture, "key", Some(3.0));

    // 다음 결제일을 바꿔도 풀은 그대로이고, 근거가 달라졌다는 표시만 켜진다.
    let moved = at + 2 * DAY;
    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "monthly", "nextBillingAt": moved}})).unwrap();
    let drifted = pool(&fixture, &saved.id);
    assert_eq!((drifted.expires_at, drifted.used_usd, drifted.estimate_drift), (at, 3.0, true));
    assert_eq!(billing_of(&fixture, "sub").unwrap().estimated_expires_at, Some(moved));
    // 연 결제로 바꿔도 마찬가지다.
    update(&fixture, json!({"apiCreditBilling": {"accountId": "sub", "cycle": "annual", "nextBillingAt": moved}})).unwrap();
    assert_eq!((pool(&fixture, &saved.id).expires_at, pool(&fixture, &saved.id).estimate_drift), (at, true));
    // 연결한 구독 계정을 지우면 그 결제 정보가 정리되고 풀은 남는다.
    fixture.service.dispatch("account.remove", json!({"id": "sub"})).unwrap();
    let after = stored_policy(&fixture);
    assert!(after.api_credit_billings.is_empty());
    assert_eq!(after.api_credit_grants[0].expires_at, at);
    let orphan = pool(&fixture, &saved.id);
    assert_eq!((orphan.estimate_drift, orphan.used_usd), (true, 3.0));
}

/// 0.23 이전 판이 저장한 단일 허용 기간·결제 정보는 모든 Anthropic 키를 담은 풀 하나·결제 목록 하나로 읽히고, 같은 id라 쓴 금액이 이어진다.
/// 키가 없을 때 남은 이전 허용 기간은 풀을 만들 수 없어 버린다. OpenAI 키는 풀에 넣지 않는다.
#[test]
fn legacy_single_grant_and_billing_become_one_pool_with_the_same_spend() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("sub", now));
    fixture.save(&api_account("a", now));
    fixture.save(&api_account("b", now));
    let mut openai = api_account("o", now);
    openai.provider = "openai".into();
    openai.identity_key = Some("openai-apikey:o".into());
    fixture.save(&openai);
    let write_legacy = |fixture: &Fixture| {
        let mut legacy = serde_json::to_value(stored_policy(fixture)).unwrap();
        let object = legacy.as_object_mut().unwrap();
        object.remove("apiCreditGrants");
        object.remove("apiCreditBillings");
        object.insert("apiCreditGrant".into(), json!({"id": "old", "budgetUsd": 20.0, "startsAt": now - HOUR, "expiresAt": now + 9 * DAY, "confirmedAt": now}));
        object.insert("apiCreditBilling".into(), json!({"accountId": "sub", "cycle": "monthly", "nextBillingAt": now + 9 * DAY, "updatedAt": now}));
        set_metadata(&fixture.service.lock().unwrap().connection, "policy", &legacy).unwrap();
    };
    write_legacy(&fixture);
    fixture.service.lock().unwrap().connection
        .execute("INSERT INTO api_credit_grant_usage(grant_id,used_usd,unmetered_requests,updated_at) VALUES ('old',4.0,0,?1)", [now])
        .unwrap();
    let migrated = stored_policy(&fixture);
    assert_eq!(migrated.api_credit_grants.len(), 1);
    assert_eq!((migrated.api_credit_grants[0].id.as_str(), migrated.api_credit_grants[0].account_ids.clone()), ("old", vec!["a".to_owned(), "b".to_owned()]));
    assert_eq!(migrated.api_credit_grants[0].expiry_source, aam_protocol::ExpirySource::Exact);
    assert_eq!(migrated.api_credit_billings.iter().map(|billing| billing.account_id.as_str()).collect::<Vec<_>>(), vec!["sub"]);
    let old = pool(&fixture, "old");
    assert_eq!((old.used_usd, old.remaining_usd, old.estimate_drift), (4.0, 16.0, false));
    spend(&fixture, "b", Some(1.0));
    assert_eq!(pool(&fixture, "old").used_usd, 5.0);
    // 저장하면 새 모양으로 남는다.
    let saved = update(&fixture, json!({"balanceApiCredit": true})).unwrap();
    let raw: Value = metadata(&fixture.service.lock().unwrap().connection, "policy").unwrap().unwrap();
    assert!(raw.get("apiCreditGrant").is_none() && raw.get("apiCreditBilling").is_none());
    assert_eq!(saved.api_credit_grants[0].id, "old");

    // 키가 하나도 없으면 이전 허용 기간은 버린다.
    let empty = Fixture::new();
    write_legacy(&empty);
    assert!(stored_policy(&empty).api_credit_grants.is_empty());
}

/// 사용액은 요청이 **입장할 때**의 풀에 쌓인다. 실행 도중 키를 다른 풀로 옮기거나, 원래 풀을 지우고 새 풀로 다시 등록해도
/// 시작한 풀에만 더하고 새 풀에는 더하지 않는다. 시작한 풀이 지워졌으면 어느 풀에도 더하지 않는다(월 합계에는 남는다).
#[test]
fn spend_stays_with_the_pool_captured_at_admission_even_if_pools_change_mid_request() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&spent_claude("sub", now));
    fixture.save(&api_account("k", now));
    fixture.save(&api_account("other", now));
    update(&fixture, json!({"useApiCreditAfterLimit": true})).unwrap();
    let a = update(&fixture, pool_input(&["k"], 50.0, now - HOUR, now + 10 * DAY)).unwrap().api_credit_grants[0].id.clone();
    let b = update(&fixture, pool_input(&["other"], 50.0, now - HOUR, now + 10 * DAY)).unwrap().api_credit_grants.into_iter().find(|grant| grant.id != a).unwrap().id;
    let start = |name: &str| {
        let grant = fixture.service.acquire({ let mut request = request(name); request.intent.non_interactive = true; request }, now).unwrap();
        assert_eq!(grant.account.id, "k");
        assert_eq!(grant.session.api_credit_pool.as_deref(), Some(a.as_str()));
        grant
    };
    // 입력 1M 토큰 × $5 = $5. 진행 중 풀 변경과 무관하게 입장 때의 풀(A)에만 쌓여야 한다.
    let record = |grant: &aam_protocol::LeaseGrant| {
        fixture.service.dispatch("apiCredit.record", json!({"sessionId": grant.session.id, "capability": grant.capability, "accountId": "k", "model": "claude-opus-4-5", "usage": {"input": 1_000_000}})).unwrap();
    };
    let used = |fixture: &Fixture, id: &str| pools(fixture).into_iter().find(|pool| pool.id == id).map(|pool| pool.used_usd);
    let monthly = |fixture: &Fixture| account(&fixture.service.lock().unwrap().connection, "k").unwrap().api_credit.unwrap().used_usd;

    // 1) A에서 시작한 요청 도중 키를 A에서 빼고 B로 옮겨도 사용액은 A에 쌓이고 B에는 쌓이지 않는다.
    let first = start("moved");
    update(&fixture, json!({"apiCreditGrant": {"id": a, "accountIds": ["other"], "budgetUsd": 50.0, "startsAt": now - HOUR, "expiresAt": now + 10 * DAY, "confirmed": true}})).unwrap_err();
    update(&fixture, json!({"apiCreditGrantRemove": b})).unwrap();
    update(&fixture, json!({"apiCreditGrant": {"id": a, "accountIds": ["other"], "budgetUsd": 50.0, "startsAt": now - HOUR, "expiresAt": now + 10 * DAY, "confirmed": true}})).unwrap();
    let moved_to = update(&fixture, pool_input(&["k"], 50.0, now - HOUR, now + 10 * DAY)).unwrap().api_credit_grants.into_iter().find(|grant| grant.id != a).unwrap().id;
    record(&first);
    assert_eq!((used(&fixture, &a), used(&fixture, &moved_to)), (Some(5.0), Some(0.0)));

    // 2) 새 입장은 지금 키가 속한 풀을 캡처한다(덜 쓴 키가 먼저라 `other`는 배정에서 빼 둔다).
    fixture.service.dispatch("account.update", json!({"id": "other", "enabled": false})).unwrap();
    let second = fixture.service.acquire({ let mut request = request("after-move"); request.intent.non_interactive = true; request }, now).unwrap();
    assert_eq!((second.account.id.as_str(), second.session.api_credit_pool.as_deref()), ("k", Some(moved_to.as_str())));
    // 그 풀을 지우고 같은 키로 다시 등록해도(새 id) 진행 중 요청은 지운 풀에 속한다: 어느 풀에도 더하지 않고 월 합계에만 남는다.
    update(&fixture, json!({"apiCreditGrantRemove": moved_to})).unwrap();
    let fresh = update(&fixture, pool_input(&["k"], 50.0, now - HOUR, now + 10 * DAY)).unwrap().api_credit_grants.into_iter().find(|grant| grant.id != a).unwrap().id;
    record(&second);
    assert_eq!((used(&fixture, &a), used(&fixture, &fresh)), (Some(5.0), Some(0.0)));
    assert!((monthly(&fixture) - 10.0).abs() < 1e-9, "{}", monthly(&fixture));
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
