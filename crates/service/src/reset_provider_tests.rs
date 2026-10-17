use super::*;
use super::test_http::Server;

fn auth(provider: &'static str) -> Auth {
    Auth { provider, access: "fixture-only".into(), account_id: Some("account-a".into()),
        email: Some("fixture@invalid.test".into()), org_id: Some("org-a".into()) }
}
fn cedar() -> Value {
    json!({"cedar_ember":{"eligible":true,"next_grant_id":"grant-a","grants":[{
        "id":"grant-a","resets_left":3,"usable_now":true,"clears":["five_hour"],"blocking":[],"use_requires_limit":true
    }]}})
}
#[test]
fn cedar_inventory_preserves_partial_scope_and_gates_future_paused_and_cooldown_offers() {
    let mut value = cedar();
    let initial = claude_inventory(&value, "cedar_ember", 1).unwrap().unwrap();
    assert_eq!((initial.available, initial.redeemable), (3, 3));
    assert_eq!(initial.credits[0].clears, vec!["anthropic:5h"]);
    value["cedar_ember"]["grants"][0]["starts_at"] = json!("2030-01-01T00:00:00Z");
    assert!(!claude_inventory(&value, "cedar_ember", 1).unwrap().unwrap().credits[0].usable);
    value["cedar_ember"]["grants"][0]["starts_at"] = Value::Null;
    value["cedar_ember"]["grants"][0]["paused"] = json!(true);
    let paused = claude_inventory(&value, "cedar_ember", 1).unwrap().unwrap();
    assert_eq!((paused.available, paused.redeemable), (3, 0));
    value["cedar_ember"]["grants"][0]["paused"] = json!(false);
    value["cedar_ember"]["cooldown_until"] = json!("2030-01-01T00:00:00Z");
    assert_eq!(claude_inventory(&value, "cedar_ember", 1).unwrap().unwrap().redeemable, 0);
    value["cedar_ember"]["cooldown_until"] = json!("not-a-date");
    assert!(claude_inventory(&value, "cedar_ember", 1).is_err());
}
#[test]
fn malformed_and_contradictory_codex_balances_are_not_zero_or_spend_authority() {
    assert!(codex_inventory(&json!({}), 1).is_err());
    let empty = codex_inventory(&json!({"credits":[],"available_count":0}), 1).unwrap();
    assert_eq!((empty.available, empty.redeemable), (0, 0));
    assert!(codex_inventory(&json!({"credits":[{"id":"credit-a"}],"available_count":0}), 1).is_err());
    assert!(codex_inventory(&json!({"credits":[{"id":"credit-a"},{"id":"credit-a"}]}), 1).is_err());
    let expired = codex_inventory(&json!({"credits":[{"id":"credit-a","status":"available","expires_at":"2000-01-01T00:00:00Z"}],"available_count":1}), now_ms()).unwrap();
    assert_eq!(expired.redeemable, 0);
}
#[test]
fn claude_discovery_falls_through_to_juniper_without_spending() {
    let server = Server::new(vec![Some((200,json!({"five_hour":{},"cedar_ember":null}))),
        Some((200,json!({"juniper_tide":{"eligible":true,"arm":"reset","available":true}})))]);
    let list = Provider::for_test(server.url.clone()).list(&auth("anthropic")).unwrap();
    assert_eq!((list.available, list.redeemable), (1, 1));
    assert_eq!(list.credits[0].clears, vec!["anthropic:5h"]);
    assert_eq!(server.posts(), 0);
    assert!(server.requests.lock()[1].0.contains("at_wall=1&skip_spend=1"));
}
#[test]
fn actual_http_cedar_reset_requires_an_explicit_success_result() {
    let server = Server::new(vec![Some((200,cedar())), Some((200,json!({"result":"reset","cleared":["five_hour"]})))]);
    let client = Provider::for_test(server.url.clone());
    let auth = auth("anthropic");
    let offer = client.list(&auth).unwrap();
    assert_eq!(client.consume(&auth, &offer.credits[0], "request-one"), ("succeeded", "reset"));
    assert_eq!(server.posts(), 1);
    assert!(server.requests.lock()[1].0.starts_with("POST /api/organizations/org-a/reset_rate_limits "));
    let malformed = Server::new(vec![Some((200,json!({})))]);
    assert_eq!(Provider::for_test(malformed.url.clone()).consume(&auth, &offer.credits[0], "request-two"), ("unknown", "result_unknown"));
}
#[test]
fn a_socket_closed_after_post_is_unknown_and_is_never_retried() {
    let server = Server::new(vec![None]);
    let credit = codex_inventory(&json!({"credits":[{"id":"credit-a"}],"available_count":1}), 1).unwrap().credits.remove(0);
    assert_eq!(Provider::for_test(server.url.clone()).consume(&auth("openai-codex"), &credit, "request-one"), ("unknown", "result_unknown"));
    assert_eq!(server.posts(), 1);
}
