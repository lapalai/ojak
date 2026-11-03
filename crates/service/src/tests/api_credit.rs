use super::*;
use crate::reset_provider::test_http::Server;
use aam_adapters::api_credit as keys;
use aam_protocol::{ApiCreditState, ExtraUsageState};
use serde_json::json;

const KEY: &str = "sk-ant-api03-TESTONLYTESTONLYTESTONLYTESTONLYTESTONLY";

fn api_account(id: &str, used: f64, now: i64) -> Account {
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
        api_credit: Some(ApiCreditState { period_start: keys::period_start(now), used_usd: used, unmetered_requests: 0, updated_at: now }),
        ..Default::default()
    }
}

fn spend(account: &mut Account) {
    for bucket in &mut account.buckets {
        bucket.used_percent = Some(100.0);
        bucket.status = "exhausted".into();
    }
}

fn on() -> Policy {
    Policy { use_api_credit_after_limit: true, ..Policy::default() }
}

fn headless() -> LaunchIntent {
    LaunchIntent { non_interactive: true, ..intent() }
}

fn decide(accounts: &[Account], policy: &Policy, intent: &LaunchIntent, now: i64) -> aam_protocol::Decision {
    scheduler::decide(accounts, &[], policy, intent, now).unwrap()
}

fn reasons<'a>(decision: &'a aam_protocol::Decision, id: &str) -> &'a [String] {
    &decision.candidates.iter().find(|candidate| candidate.account_id == id).unwrap().reasons
}

#[test]
fn subscription_with_room_always_wins_and_api_keys_are_never_ordinary_candidates() {
    let now = now_ms();
    let roomy = account_fixture("roomy", now);
    let api = api_account("api", 0.0, now);
    let decision = decide(&[roomy.clone(), api.clone()], &on(), &headless(), now);
    assert_eq!(decision.selected_account_id.as_deref(), Some("roomy"));
    assert!(!scheduler::chose_api_credit(&decision));
    assert!(decision.candidates.iter().all(|candidate| candidate.account_id != "api"));
    // 구독 후보가 아예 없고 관측도 없으면 API 키는 일반 후보로 올라오지 않는다.
    let alone = decide(std::slice::from_ref(&api), &on(), &headless(), now);
    assert_eq!(alone.selected_account_id, None);
    // 일부만 소진돼도(다른 계정이 한도를 남김) API로 넘어가지 않는다.
    let mut spent = account_fixture("spent", now);
    spend(&mut spent);
    let partial = decide(&[spent, roomy, api], &on(), &headless(), now);
    assert_eq!(partial.selected_account_id.as_deref(), Some("roomy"));
}

#[test]
fn api_key_is_chosen_only_after_every_subscription_is_freshly_exhausted() {
    let now = now_ms();
    let mut one = account_fixture("one", now);
    let mut two = account_fixture("two", now);
    spend(&mut one);
    spend(&mut two);
    let api = api_account("api", 1.0, now);
    let decision = decide(&[one.clone(), two.clone(), api.clone()], &on(), &headless(), now);
    assert_eq!(decision.selected_account_id.as_deref(), Some("api"));
    assert!(scheduler::chose_api_credit(&decision));
    assert!(reasons(&decision, "api").iter().any(|reason| reason.starts_with("API_CREDIT_FALLBACK:")));
    // 계정이 하나 줄어도 구독이 모두 소진이면 같다. 사용자가 제외한 구독 계정은 소진 판정에서 뺀다.
    let mut off = account_fixture("off", now);
    off.enabled = false;
    let with_off = decide(&[one.clone(), off, api.clone()], &on(), &headless(), now);
    assert!(scheduler::chose_api_credit(&with_off));
    // 소진 관측이 낡았거나 리셋 시각이 지났으면 쓰지 않는다.
    let mut stale = one.clone();
    stale.buckets[0].observed_at = now - 901_000;
    assert_eq!(decide(&[stale, api.clone()], &on(), &headless(), now).selected_account_id, None);
    let mut reset = one.clone();
    reset.buckets[0].resets_at = Some(now - 1);
    assert_eq!(decide(&[reset, api.clone()], &on(), &headless(), now).selected_account_id, None);
    // 한 계정은 소진, 다른 계정은 관측이 낡았다면(회복했을 수 있음) 쓰지 않는다.
    let mut unknown = two.clone();
    unknown.buckets[0].status = "stale".into();
    assert_eq!(decide(&[one.clone(), unknown, api.clone()], &on(), &headless(), now).selected_account_id, None);
    // 모델 전용 한도만 소진됐다면 API로 넘어가지 않는다(다른 모델은 구독으로 갈 수 있다).
    let mut model_only = account_fixture("model-only", now);
    model_only.buckets.push(bucket("sonnet-only", Some("fable"), 100.0, now + 86_400_000, now));
    model_only.buckets[1].status = "exhausted".into();
    assert_eq!(decide(&[model_only, api.clone()], &on(), &headless(), now).selected_account_id, None);
    // 동시 사용 자리만 없는 구독 계정이 있으면 기다린다.
    let held: aam_protocol::Session = serde_json::from_value(json!({
        "id":"held", "requestId":"held", "accountId":"roomy", "tool":"claude", "model":"claude-fable-5-1",
        "cwd":test_cwd(), "state":"ACTIVE", "verification":"preflight-verified",
        "startedAt":now, "updatedAt":now, "process":null, "supervisor":null,
        "spawnAttemptId":null, "generation":"generation", "exitCode":null, "reason":null
    }))
    .unwrap();
    let roomy = account_fixture("roomy", now);
    let waiting = scheduler::decide(&[one, roomy, api], &[held], &on(), &headless(), now).unwrap();
    assert_eq!(waiting.selected_account_id, None);
}

#[test]
fn api_credit_needs_opt_in_non_interactive_and_headroom_under_the_monthly_limit() {
    let now = now_ms();
    let mut spent = account_fixture("spent", now);
    spend(&mut spent);
    let api = api_account("api", 10.0, now);
    let accounts = [spent.clone(), api.clone()];
    assert_eq!(decide(&accounts, &Policy::default(), &headless(), now).selected_account_id, None);
    // 대화형 실행에는 쓰지 않는다.
    assert_eq!(decide(&accounts, &on(), &intent(), now).selected_account_id, None);
    // codex 의도에는 적용되지 않는다.
    let codex = LaunchIntent { tool: "codex".into(), ..headless() };
    assert_eq!(decide(&accounts, &on(), &codex, now).selected_account_id, None);
    // 월 상한에 닿으면 쓰지 않고, 사유를 남긴다.
    let capped = Policy { api_credit_monthly_limit_usd: 10.0, ..on() };
    let blocked = decide(&accounts, &capped, &headless(), now);
    assert_eq!(blocked.selected_account_id, None);
    assert!(reasons(&blocked, "api").iter().any(|reason| reason.starts_with("API_CREDIT_LIMIT:")));
    // 지난달 기록은 이번 달 사용액 0으로 본다.
    let mut old = api.clone();
    old.api_credit.as_mut().unwrap().period_start -= 40 * 86_400_000;
    assert_eq!(decide(&[spent.clone(), old], &capped, &headless(), now).selected_account_id.as_deref(), Some("api"));
    // 제외했거나 로그인이 필요한 키, 단가를 모르는 모델은 쓰지 않는다.
    let mut disabled = api.clone();
    disabled.enabled = false;
    assert_eq!(decide(&[spent.clone(), disabled], &on(), &headless(), now).selected_account_id, None);
    let mut auth = api.clone();
    auth.auth_status = "auth-required".into();
    assert_eq!(decide(&[spent.clone(), auth], &on(), &headless(), now).selected_account_id, None);
    let unpriced = LaunchIntent { model: "mystery-model-9".into(), ..headless() };
    assert_eq!(decide(&accounts, &on(), &unpriced, now).selected_account_id, None);
    // 가장 덜 쓴 키를 먼저 쓴다.
    let cheap = api_account("cheap", 1.0, now);
    assert_eq!(decide(&[spent, api, cheap], &on(), &headless(), now).selected_account_id.as_deref(), Some("cheap"));
}

#[test]
fn extra_usage_is_used_before_the_api_key() {
    let now = now_ms();
    let mut paid = account_fixture("paid", now);
    spend(&mut paid);
    paid.extra_usage = Some(ExtraUsageState { enabled: true, used_usd: 1.0, limit_usd: Some(50.0), observed_at: now });
    let api = api_account("api", 0.0, now);
    let both = Policy { use_extra_usage_after_limit: true, use_api_credit_after_limit: true, ..Policy::default() };
    let decision = decide(&[paid.clone(), api.clone()], &both, &headless(), now);
    assert_eq!(decision.selected_account_id.as_deref(), Some("paid"));
    assert!(scheduler::chose_extra_usage(&decision));
    assert!(!scheduler::chose_api_credit(&decision));
    // extra usage가 상한에 닿으면 그다음에 API 키로 넘어간다.
    paid.extra_usage.as_mut().unwrap().used_usd = 50.0;
    let next = decide(&[paid, api], &both, &headless(), now);
    assert_eq!(next.selected_account_id.as_deref(), Some("api"));
    assert!(scheduler::chose_api_credit(&next));
}

#[test]
fn pinned_requests_never_land_on_an_api_key() {
    let now = now_ms();
    let mut spent = account_fixture("spent", now);
    spend(&mut spent);
    let api = api_account("api", 0.0, now);
    let pinned = LaunchIntent { account_id: Some("spent".into()), ..headless() };
    assert_eq!(decide(&[spent.clone(), api.clone()], &on(), &pinned, now).selected_account_id, None);
    let to_api = LaunchIntent { account_id: Some("api".into()), ..headless() };
    assert_eq!(decide(&[spent, api], &on(), &to_api, now).selected_account_id, None);
}

#[test]
fn summary_groups_api_keys_separately_and_activates_only_when_subscriptions_are_spent() {
    let now = now_ms();
    let api = api_account("api", 12.4, now);
    let roomy = account_fixture("roomy", now);
    let mut spent = account_fixture("spent", now);
    spend(&mut spent);
    let kind = |accounts: &[Account], policy: &Policy| {
        let summaries = crate::quota_summary::summaries(accounts, policy, &[], now);
        let api = summaries.iter().find(|summary| summary.account_ids == ["api".to_owned()]).unwrap().clone();
        (api, summaries)
    };
    let (waiting, all) = kind(&[roomy.clone(), api.clone()], &on());
    assert_eq!(waiting.kind, "available");
    let facts = waiting.api_credit.unwrap();
    assert!(!facts.active && !facts.limit_reached && !facts.unmetered);
    assert_eq!((facts.used_usd, facts.limit_usd), (12.4, 200.0));
    // 구독 묶음에는 API 크레딧 사실이 섞이지 않는다.
    assert!(all.iter().filter(|summary| summary.account_ids != ["api".to_owned()]).all(|summary| summary.api_credit.is_none()));
    // 구독이 모두 소진이고 옵트인이면 활성.
    let (active, all) = kind(&[spent.clone(), api.clone()], &on());
    assert_eq!(active.kind, "api-credit");
    assert!(active.api_credit.unwrap().active);
    assert!(all.iter().any(|summary| summary.kind == "resting"));
    // 옵트인이 없으면 대기 상태 그대로.
    assert_eq!(kind(&[spent.clone(), api.clone()], &Policy::default()).0.kind, "available");
    // 구독 계정이 없으면(관측 부족) 활성이 아니다.
    assert_eq!(kind(std::slice::from_ref(&api), &on()).0.kind, "available");
    // 상한 도달은 쉬는 상태, 비활성·로그인 필요는 제외.
    let capped = Policy { api_credit_monthly_limit_usd: 12.0, ..on() };
    let (limited, _) = kind(&[spent.clone(), api.clone()], &capped);
    assert_eq!(limited.kind, "resting");
    assert!(limited.api_credit.unwrap().limit_reached);
    let mut off = api.clone();
    off.enabled = false;
    assert_eq!(kind(&[spent.clone(), off], &on()).0.kind, "excluded");
    let mut auth = api.clone();
    auth.auth_status = "auth-required".into();
    assert_eq!(kind(&[spent.clone(), auth], &on()).0.kind, "excluded");
    // API 키가 구독 소진 표시(extra usage 배지 등)를 바꾸지 않는다: 구독이 소진이어도 API 키가 있다고 claude_left가 되지 않는다.
    let extra_on = Policy { use_extra_usage_after_limit: true, ..Policy::default() };
    let mut paid = spent;
    paid.extra_usage = Some(ExtraUsageState { enabled: true, used_usd: 1.0, limit_usd: None, observed_at: now });
    let shown = crate::quota_summary::summaries(&[paid, api], &extra_on, &[], now);
    assert!(shown.iter().any(|summary| summary.kind == "extra"));
}

/// 키가 여럿이면 상한은 모든 키의 합계로 본다(같은 Console 조직의 키는 잔액을 공유). 각각 $150인 키 둘이면 $300이라
/// 상한 $200에서 어느 키도 고르지 않고, 요약도 두 키 모두 상한 도달로 보인다. 상한 안이면 덜 쓴 키를 먼저 고른다.
#[test]
fn monthly_cap_is_combined_across_keys_and_least_used_key_goes_first() {
    let now = now_ms();
    let mut spent = account_fixture("spent", now);
    spend(&mut spent);
    let pair = [spent.clone(), api_account("a", 150.0, now), api_account("b", 150.0, now)];
    let blocked = decide(&pair, &on(), &headless(), now);
    assert_eq!(blocked.selected_account_id, None);
    for id in ["a", "b"] {
        assert!(reasons(&blocked, id).iter().any(|reason| reason.starts_with("API_CREDIT_LIMIT:")), "{id}");
    }
    let summaries = crate::quota_summary::summaries(&pair, &on(), &[], now);
    for id in ["a", "b"] {
        let summary = summaries.iter().find(|summary| summary.account_ids == [id.to_owned()]).unwrap();
        assert_eq!(summary.kind, "resting", "{id}");
        assert!(summary.api_credit.as_ref().unwrap().limit_reached, "{id}");
    }
    let roomy = [spent, api_account("a", 90.0, now), api_account("b", 20.0, now)];
    assert_eq!(decide(&roomy, &on(), &headless(), now).selected_account_id.as_deref(), Some("b"));
}

/// API 키는 이름을 바꾸고 켜고 끌 수 있다. 구독 계정 이름은 바꾸지 않고, 잘못된 이름은 거절한다.
#[test]
fn api_keys_can_be_renamed_and_toggled_but_subscription_labels_cannot() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&api_account("api", 0.0, now));
    let update = |params: serde_json::Value| fixture.service.dispatch("account.update", params);
    let renamed: Account = serde_json::from_value(update(json!({"id": "api", "label": "  업무용 키  "})).unwrap()).unwrap();
    assert_eq!(renamed.label, "업무용 키");
    let off: Account = serde_json::from_value(update(json!({"id": "api", "enabled": false})).unwrap()).unwrap();
    assert!(!off.enabled && off.label == "업무용 키");
    for bad in ["", "   ", &"x".repeat(121), "line\nbreak"] {
        assert_eq!(update(json!({"id": "api", "label": bad})).unwrap_err().code, "INVALID_PARAMS", "{bad:?}");
    }
    let sub = account_fixture("sub", now);
    fixture.save(&sub);
    assert_eq!(update(json!({"id": "sub", "label": "rename"})).unwrap_err().code, "INVALID_PARAMS");
}

#[test]
fn lease_grant_for_an_api_key_is_flagged_and_uses_unlimited_concurrency() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut spent = account_fixture("spent", now);
    spend(&mut spent);
    fixture.save(&spent);
    fixture.save(&api_account("api", 0.0, now));
    fixture.service.dispatch("policy.update", json!({"expectedRevision": 1, "useApiCreditAfterLimit": true})).unwrap();
    let mut first = request("api-1");
    first.intent.non_interactive = true;
    let grant = fixture.service.acquire(first, now).unwrap();
    assert!(grant.api_credit_fallback);
    assert_eq!((grant.account.tool.as_str(), grant.account.id.as_str()), ("api", "api"));
    // max_concurrency 0은 무제한: 열린 세션이 있어도 다음 요청이 같은 키를 받는다.
    let mut second = request("api-2");
    second.intent.non_interactive = true;
    let again = fixture.service.acquire(second, now).unwrap();
    assert!(again.api_credit_fallback);
    // 멱등 재요청도 같은 표식을 유지한다.
    let mut replay = request("api-1");
    replay.intent.non_interactive = true;
    assert!(fixture.service.acquire(replay, now).unwrap().api_credit_fallback);
    // 대화형 요청은 NO_ELIGIBLE_ACCOUNT.
    assert_eq!(fixture.service.acquire(request("tty"), now).unwrap_err().code, "NO_ELIGIBLE_ACCOUNT");
}

#[test]
fn policy_update_validates_the_api_credit_limit_and_rejects_api_pins() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&api_account("api", 0.0, now));
    let update = |body: serde_json::Value| fixture.service.dispatch("policy.update", body);
    for bad in [0.5, -1.0, 100_001.0] {
        assert_eq!(update(json!({"expectedRevision": 1, "apiCreditMonthlyLimitUsd": bad})).unwrap_err().code, "INVALID_PARAMS");
    }
    assert_eq!(update(json!({"expectedRevision": 1, "apiCreditMonthlyLimitUsd": "x"})).unwrap_err().code, "INVALID_PARAMS");
    let ok = update(json!({"expectedRevision": 1, "useApiCreditAfterLimit": true, "apiCreditMonthlyLimitUsd": 250.0})).unwrap();
    assert_eq!(ok["useApiCreditAfterLimit"], true);
    assert_eq!(ok["apiCreditMonthlyLimitUsd"], 250.0);
    assert_eq!(update(json!({"expectedRevision": 2, "accountPriority": ["api"]})).unwrap_err().code, "INVALID_PARAMS");
    assert_eq!(update(json!({"expectedRevision": 2, "providerPins": {"anthropic": "api"}})).unwrap_err().code, "INVALID_PARAMS");
    assert_eq!(fixture.service.dispatch("account.update", json!({"id": "api", "maxConcurrency": 3})).unwrap_err().code, "INVALID_PARAMS");
}

#[test]
fn api_key_add_rejects_before_touching_keychain_and_never_echoes_the_key() {
    let fixture = Fixture::new();
    let add = |label: &str, key: &str, confirmed: bool| {
        fixture.service.dispatch("account.apiKeyAdd", json!({"label": label, "key": key, "confirmedNoAutoReload": confirmed}))
    };
    let unconfirmed = add("work", KEY, false).unwrap_err();
    assert_eq!(unconfirmed.code, "API_CREDIT_CONFIRM_REQUIRED");
    let format = add("work", "sk-ant-oat01-NOTANAPIKEYNOTANAPIKEYNOTANAPIKEYNOTANAPIKEY", true).unwrap_err();
    assert_eq!(format.code, "API_KEY_FORMAT");
    assert_eq!(add("", KEY, true).unwrap_err().code, "INVALID_PARAMS");
    for error in [&unconfirmed, &format] {
        assert!(!error.message.contains("TESTONLY") && !error.message.contains("NOTANAPIKEY"));
    }
    // 이미 같은 키(identity)가 있으면 네트워크 확인 전에 거절한다.
    let now = now_ms();
    let mut existing = api_account("existing", 0.0, now);
    existing.identity_key = Some(keys::identity_key(keys::ApiProvider::Anthropic, KEY));
    fixture.save(&existing);
    let duplicate = add("again", KEY, true).unwrap_err();
    assert_eq!(duplicate.code, "ACCOUNT_ALREADY_CONNECTED");
    assert!(!duplicate.message.contains("TESTONLY"));
    assert_eq!(accounts(&fixture.service.lock().unwrap().connection).unwrap().len(), 1);
}

#[test]
fn api_key_verification_maps_http_failures_without_leaking_the_key() {
    let fixture = Fixture::new();
    let add = |label: &str| {
        fixture.service.dispatch("account.apiKeyAdd", json!({"label": label, "key": KEY, "confirmedNoAutoReload": true}))
    };
    for (status, code) in [(401, "API_KEY_INVALID"), (403, "API_KEY_INVALID"), (500, "API_KEY_CHECK_FAILED"), (429, "API_KEY_CHECK_FAILED")] {
        let server = Server::new(vec![Some((status, json!({"error": {"message": KEY}})))]);
        fixture.service.set_api_base_for_test(keys::ApiProvider::Anthropic, &server.url);
        let error = add("work").unwrap_err();
        assert_eq!(error.code, code, "status {status}");
        assert!(!error.message.contains("TESTONLY"), "{}", error.message);
        assert!(server.requests.lock()[0].0.starts_with("GET /v1/models "));
    }
    // 서버가 연결을 끊어도 확인 실패이며, 어떤 경우에도 계정이 저장되지 않는다.
    let server = Server::new(vec![None]);
    fixture.service.set_api_base_for_test(keys::ApiProvider::Anthropic, &server.url);
    assert_eq!(add("work").unwrap_err().code, "API_KEY_CHECK_FAILED");
    assert!(accounts(&fixture.service.lock().unwrap().connection).unwrap().is_empty());
    // 성공 응답이면 확인만 통과한다(Keychain 저장 단계는 이 테스트에서 건드리지 않는다).
    let ok = Server::new(vec![Some((200, json!({"data": []})))]);
    assert!(crate::api_keys::verify_key(keys::ApiProvider::Anthropic, &ok.url, KEY).is_ok());
}

#[test]
fn opaque_api_key_is_validated_by_provider_before_any_storage() {
    let fixture = Fixture::new();
    let key = format!("opaque.v2:+/={}", "x".repeat(300));
    let server = Server::new(vec![Some((401, json!({"error": {"message": key}})))]);
    fixture.service.set_api_base_for_test(keys::ApiProvider::Anthropic, &server.url);
    let error = fixture.service.dispatch("account.apiKeyAdd", json!({
        "label": "opaque", "key": key, "confirmedNoAutoReload": true
    })).unwrap_err();
    assert_eq!(error.code, "API_KEY_INVALID");
    assert!(!error.message.contains(&key));
    assert!(server.requests.lock()[0].0.starts_with("GET /v1/models "));
    assert!(accounts(&fixture.service.lock().unwrap().connection).unwrap().is_empty());
}

#[test]
fn record_adds_estimated_cost_for_api_accounts_only() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&api_account("api", 1.0, now));
    fixture.save(&account_fixture("sub", now));
    let record = |account: &str, usage: serde_json::Value| {
        fixture.service.dispatch("apiCredit.record", json!({"accountId": account, "model": "claude-sonnet-5-5", "usage": usage}))
    };
    let used = || {
        let store = fixture.service.lock().unwrap();
        account(&store.connection, "api").unwrap().api_credit.unwrap()
    };
    // sonnet-5-5: $2/MTok input, $10/MTok output.
    record("api", json!({"input": 1_000_000, "output": 100_000, "cacheRead": 0, "cacheWrite": 0})).unwrap();
    assert!((used().used_usd - 4.0).abs() < 1e-9, "{}", used().used_usd);
    // usage를 못 읽은 실행은 금액을 더하지 않고 미확인으로 센다.
    record("api", serde_json::Value::Null).unwrap();
    assert_eq!((used().used_usd, used().unmetered_requests), (4.0 + 0.0, 1));
    // 단가를 모르는 모델도 미확인.
    fixture.service.dispatch("apiCredit.record", json!({"accountId": "api", "model": "mystery-model-9", "usage": {"input": 5, "output": 5}})).unwrap();
    assert_eq!(used().unmetered_requests, 2);
    assert_eq!(record("sub", json!(null)).unwrap_err().code, "INVALID_PARAMS");
    assert_eq!(record("missing", json!(null)).unwrap_err().code, "ACCOUNT_NOT_FOUND");
}

#[test]
fn refresh_scan_leaves_api_accounts_untouched() {
    let fixture = Fixture::new();
    let now = now_ms();
    let api = api_account("api", 3.0, now);
    fixture.save(&api);
    let scan = aam_adapters::ScanResult { accounts: vec![], tools: vec![], notices: vec![], merged_observation_ids: Default::default() };
    fixture.service.apply_scan(vec![api.clone()], scan).unwrap();
    let stored = account(&fixture.service.lock().unwrap().connection, "api").unwrap();
    assert_eq!(stored, api, "스캔에 나타나지 않아도 API 키 계정은 stale·auth 변경 없이 그대로여야 한다");
}

fn spent_claude(now: i64) -> Account {
    let mut spent = account_fixture("spent", now);
    spend(&mut spent);
    spent
}

fn headless_request(id: &str) -> Acquire {
    let mut request = request(id);
    request.intent.non_interactive = true;
    request
}

/// 키를 지워도 이번 달 사용액은 월 상한 합계에 남는다: A가 쓰고 → A를 지우고 → B를 두어도 상한은 A의 사용액을 포함한다.
/// 배정(`lease.acquire`)·경로 설명(`route.explain`)·스냅샷 요약과 합계가 모두 같은 원장 값을 쓰고, 지운 키는 후보·계정 목록에 나타나지 않는다.
#[test]
fn removed_key_spend_stays_in_the_combined_cap_for_allocation_and_snapshot() {
    let fixture = Fixture::new();
    let now = now_ms();
    let removed_id = format!("ledger-removal-test-{}", new_id());
    fixture.save(&spent_claude(now));
    fixture.save(&api_account(&removed_id, 150.0, now));
    fixture.service.dispatch("policy.update", json!({"expectedRevision": 1, "useApiCreditAfterLimit": true, "apiCreditMonthlyLimitUsd": 200.0})).unwrap();
    fixture.service.dispatch("account.remove", json!({"id": removed_id})).unwrap();
    assert!(accounts(&fixture.service.lock().unwrap().connection).unwrap().iter().all(|account| account.id != removed_id));
    // 새 키 B는 합계($150) 안이라 배정된다.
    fixture.save(&api_account("b", 0.0, now));
    let grant = fixture.service.acquire(headless_request("after-remove"), now).unwrap();
    assert!(grant.api_credit_fallback);
    assert_eq!(grant.account.id, "b");
    // B가 $60을 쓰면 합계는 $210이다. A가 지워졌어도 상한($200)에 닿아 더는 배정하지 않는다.
    record_api_credit(&mut fixture.service.lock().unwrap().connection, "b", Some(60.0), now).unwrap();
    assert_eq!(fixture.service.acquire(headless_request("over-cap"), now).unwrap_err().code, "NO_ELIGIBLE_ACCOUNT");
    let explained = fixture.service.dispatch(
        "route.explain",
        json!({"intent": headless()}),
    ).unwrap();
    assert!(explained["candidates"].as_array().unwrap().iter().any(|candidate| candidate["accountId"] == "b"
        && candidate["reasons"].as_array().unwrap().iter().any(|reason| reason.as_str().unwrap().starts_with("API_CREDIT_LIMIT:"))));
    assert!(explained["candidates"].as_array().unwrap().iter().all(|candidate| candidate["accountId"] != removed_id));
    // 스냅샷: 합계 원장은 지운 키 몫만, 요약은 B의 사용액에 상한 도달을 알린다. 지운 키의 가짜 계정은 없다.
    let snapshot = fixture.service.snapshot().unwrap();
    assert_eq!(snapshot.accounts.iter().filter(|account| account.tool == "api").map(|account| account.id.as_str()).collect::<Vec<_>>(), ["b"]);
    let removed = snapshot.api_credit_removed.expect("지운 키의 이번 달 사용액이 스냅샷에 실려야 한다");
    assert_eq!((removed.period_start, removed.used_usd), (keys::period_start(now), 150.0));
    let b = snapshot.quota_summaries.iter().find(|summary| summary.account_ids == ["b".to_owned()]).unwrap();
    let facts = b.api_credit.as_ref().unwrap();
    assert_eq!(facts.used_usd, 60.0);
    assert!(facts.limit_reached);
    assert_eq!(b.kind, "resting");
}

/// 꺼 둔(enabled=false) 키의 사용액도 합계에 센다. 꺼 둔 키를 지워도 마찬가지다. 지운 뒤 같은 기간의 다음 달 값은 0부터다.
#[test]
fn disabled_and_removed_disabled_keys_still_count_and_months_roll_over() {
    let fixture = Fixture::new();
    let now = now_ms();
    let capped = Policy { api_credit_monthly_limit_usd: 200.0, ..on() };
    let removed_id = format!("ledger-disabled-test-{}", new_id());
    let mut off = api_account(&removed_id, 150.0, now);
    off.enabled = false;
    let live = api_account("live", 60.0, now);
    let spent = spent_claude(now);
    // 꺼 둔 키 $150 + 살아 있는 키 $60 = $210 > $200: 꺼 둔 키도 합계에 든다.
    let blocked = decide(&[spent.clone(), off.clone(), live.clone()], &capped, &headless(), now);
    assert_eq!(blocked.selected_account_id, None);
    assert!(reasons(&blocked, "live").iter().any(|reason| reason.starts_with("API_CREDIT_LIMIT:")));
    // 꺼 둔 키를 지운 뒤에도 같은 합계다.
    fixture.save(&spent);
    fixture.save(&off);
    fixture.save(&live);
    fixture.service.dispatch("policy.update", json!({"expectedRevision": 1, "useApiCreditAfterLimit": true, "apiCreditMonthlyLimitUsd": 200.0})).unwrap();
    fixture.service.dispatch("account.remove", json!({"id": removed_id})).unwrap();
    assert_eq!(fixture.service.acquire(headless_request("disabled-removed"), now).unwrap_err().code, "NO_ELIGIBLE_ACCOUNT");
    let mut store = fixture.service.lock().unwrap();
    assert_eq!(api_credit_removed(&store.connection, keys::ApiProvider::Anthropic, now).unwrap().used_usd, 150.0);
    // 월 경계: 다음 달에는 지운 키의 이전 달 사용액이 합계에서 빠지고, 이전 달 행은 원장에 그대로 남는다.
    let next = keys::period_start(now) + 32 * 86_400_000;
    assert_eq!(api_credit_removed(&store.connection, keys::ApiProvider::Anthropic, next).unwrap().used_usd, 0.0);
    record_api_credit(&mut store.connection, "live", Some(5.0), next).unwrap();
    let this_month = account(&store.connection, "live").unwrap().api_credit.unwrap();
    assert_eq!((this_month.period_start, this_month.used_usd), (keys::period_start(next), 5.0));
    assert_eq!(api_credit_removed(&store.connection, keys::ApiProvider::Anthropic, now).unwrap().used_usd, 150.0);
    let rows: i64 = store.connection.query_row("SELECT COUNT(*) FROM api_credit_usage WHERE account_id='live'", [], |row| row.get(0)).unwrap();
    assert_eq!(rows, 2);
}

/// 0.18.0이 계정 본문에 쌓은 사용액은 시작할 때 한 번만 원장으로 옮긴다(두 번 세지 않는다). 이미 옮겨 둔 행이 있으면 더하지 않는다.
#[test]
fn legacy_account_body_usage_migrates_once_without_double_counting() {
    let path = std::env::temp_dir().join(format!("aam-api-ledger-{}.db", new_id()));
    let now = now_ms();
    let legacy = api_account("legacy", 12.5, now);
    let partial = api_account("partial", 7.0, now);
    {
        let store = Store::open(&path).unwrap();
        for account in [&legacy, &partial] {
            store.connection.execute("INSERT INTO accounts(id,body) VALUES (?1,?2)", rusqlite::params![account.id, encode(account).unwrap()]).unwrap();
        }
        // 일부는 이미 옮겨 둔 상태(이전 시작이 중간에 끝난 경우)와 같다.
        store::seed_api_credit(&store.connection, "partial", "anthropic", partial.api_credit.as_ref().unwrap());
    }
    for _ in 0..2 {
        let store = Store::open(&path).unwrap();
        for (id, expected) in [("legacy", 12.5), ("partial", 7.0)] {
            assert_eq!(account(&store.connection, id).unwrap().api_credit.unwrap().used_usd, expected, "{id}");
            let body: String = store.connection.query_row("SELECT body FROM accounts WHERE id=?1", [id], |row| row.get(0)).unwrap();
            assert!(!body.contains("apiCredit"), "계정 본문에는 사용액을 남기지 않는다: {id}");
        }
        let rows: i64 = store.connection.query_row("SELECT COUNT(*) FROM api_credit_usage", [], |row| row.get(0)).unwrap();
        assert_eq!(rows, 2);
    }
    let _ = std::fs::remove_file(&path);
}

/// 계정 본문을 다시 저장(이름 변경·켜기 끄기)해도 사용액은 되돌아가거나 두 배가 되지 않는다.
#[test]
fn saving_an_api_account_never_touches_the_ledger() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&api_account("api", 4.0, now));
    fixture.service.dispatch("account.update", json!({"id": "api", "enabled": false})).unwrap();
    fixture.service.dispatch("account.update", json!({"id": "api", "label": "renamed"})).unwrap();
    record_api_credit(&mut fixture.service.lock().unwrap().connection, "api", Some(1.0), now).unwrap();
    assert_eq!(account(&fixture.service.lock().unwrap().connection, "api").unwrap().api_credit.unwrap().used_usd, 5.0);
}
