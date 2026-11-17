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
fn inv(values: &[&Value], now: i64) -> Inventory { claude_inventory(values, now).unwrap() }
fn grant(id: &str, kind: &str, gives_back: bool, clears: Value) -> Value {
    json!({"id":id,"type":kind,"gives_back_weekly":gives_back,"resets_left":1,"usable_now":true,"clears":clears,"blocking":[],
        "use_requires_limit":false,"starts_at":"2026-11-09T18:00:00Z","ends_at":"2027-03-09T07:59:00Z"})
}
fn web_like() -> Value {
    json!({"cedar_ember":{"eligible":true,"next_grant_id":"full-reset-2611","grants":[
        grant("full-reset-2611","weekly",false,json!(["seven_day","seven_day_overage_included"])),
        grant("launch-2610-fable","full",true,json!(["seven_day_overage_included"]))]}})
}
fn credit_by<'a>(inventory: &'a Inventory, id: &str) -> &'a ResetCredit { inventory.credits.iter().find(|c| c.id == id).unwrap() }
// Inside every grant window of `web_like`.
fn inside() -> i64 { chrono::DateTime::parse_from_rfc3339("2026-11-16T00:00:00Z").unwrap().timestamp_millis() }
#[test]
fn cedar_inventory_preserves_partial_scope_and_gates_future_paused_and_cooldown_offers() {
    let mut value = cedar();
    let initial = inv(&[&value], 1);
    assert_eq!((initial.available, initial.redeemable), (Some(3), Some(3)));
    assert_eq!(initial.credits[0].clears, vec!["anthropic:5h"]);
    assert_eq!(initial.credits[0].reset_type, "session");
    value["cedar_ember"]["grants"][0]["starts_at"] = json!("2030-01-01T00:00:00Z");
    let future = inv(&[&value], 1);
    assert!(!future.credits[0].usable);
    assert_eq!(future.credits[0].unavailable_reason.as_deref(), Some("not_started"));
    value["cedar_ember"]["grants"][0]["starts_at"] = Value::Null;
    value["cedar_ember"]["grants"][0]["paused"] = json!(true);
    let paused = inv(&[&value], 1);
    assert_eq!((paused.available, paused.redeemable), (Some(3), Some(0)));
    value["cedar_ember"]["grants"][0]["paused"] = json!(false);
    value["cedar_ember"]["cooldown_until"] = json!("2030-01-01T00:00:00Z");
    let cooling = inv(&[&value], 1);
    assert_eq!(cooling.redeemable, Some(0));
    assert_eq!(cooling.credits[0].cooldown_until, Some(1_893_456_000_000));
    assert_eq!(cooling.credits[0].unavailable_reason.as_deref(), Some("cooldown"));
    value["cedar_ember"]["cooldown_until"] = json!("not-a-date");
    assert!(claude_inventory(&[&value], 1).is_err());
}
#[test]
fn fable_partial_reimbursement_is_not_a_full_weekly_and_modern_grants_are_not_gated_by_next_grant_id() {
    let value = web_like();
    let list = inv(&[&value], inside());
    // Both buttons are usable even though only one is the recommended next grant.
    assert_eq!((list.available, list.redeemable), (Some(2), Some(2)));
    let weekly = credit_by(&list, "full-reset-2611");
    assert_eq!((weekly.reset_type.as_str(), weekly.gives_back_weekly), ("weekly", false));
    assert_eq!(weekly.clears, vec!["anthropic:7d", "anthropic:7d:fable"]);
    assert_eq!(weekly.starts_at, Some(chrono::DateTime::parse_from_rfc3339("2026-11-09T18:00:00Z").unwrap().timestamp_millis()));
    let fable = credit_by(&list, "launch-2610-fable");
    assert_eq!((fable.reset_type.as_str(), fable.gives_back_weekly), ("fable", true));
    assert_eq!(fable.clears, vec!["anthropic:7d:fable"], "Fable restores only the Fable weekly window");
    assert!(fable.usable && !fable.requires_limit);
    assert_eq!(fable.expires_at, Some(chrono::DateTime::parse_from_rfc3339("2027-03-09T07:59:00Z").unwrap().timestamp_millis()));
}
#[test]
fn conditional_experiment_nonparticipation_preserves_other_grants_without_weakening_restrictions() {
    let mut first = web_like();
    first["juniper_tide"] = Value::Null;
    let mut second = first.clone();
    second["juniper_tide"] = json!({"eligible":false,"ineligible_reason":"not_at_wall","arm":null,
        "available":false,"in_experiment":false,"weekly_resets_at":null,"next_available_at":null});
    let known = inv(&[&first, &second], inside());
    assert_eq!(known.state, "known");
    assert_eq!((known.available, known.redeemable), (Some(2), Some(2)));

    second["juniper_tide"]["in_experiment"] = json!(true);
    let uncertain = inv(&[&first, &second], inside());
    assert_eq!(uncertain.state, "restricted");
    assert_eq!((uncertain.available, uncertain.redeemable), (None, None));
    assert!(uncertain.credits.iter().all(|credit| !credit.usable));

    second["juniper_tide"]["in_experiment"] = json!(false);
    second["juniper_tide"]["ineligible_reason"] = json!("surface");
    let hidden = inv(&[&first, &second], inside());
    assert_eq!(hidden.reason, Some("surface"));
    assert_eq!((hidden.available, hidden.redeemable), (None, None));
}
#[test]
fn legacy_untyped_grants_still_require_next_grant_id_and_missing_type_is_derived() {
    let mut value = web_like();
    for grant in value["cedar_ember"]["grants"].as_array_mut().unwrap() { grant.as_object_mut().unwrap().remove("type"); }
    let list = inv(&[&value], inside());
    assert_eq!(credit_by(&list, "full-reset-2611").reset_type, "weekly");
    assert!(credit_by(&list, "full-reset-2611").usable);
    let other = credit_by(&list, "launch-2610-fable");
    assert_eq!(other.reset_type, "fable");
    assert!(!other.usable);
    assert_eq!(other.unavailable_reason.as_deref(), Some("not_next_grant"));
}
#[test]
fn unknown_types_scopes_and_blocking_stay_visible_but_cannot_be_spent() {
    let mut value = web_like();
    let mut blocked_grant = grant("blocked", "weekly", false, json!(["seven_day"]));
    blocked_grant["blocking"] = json!(["seven_day_opus"]);
    value["cedar_ember"]["grants"] = json!([
        grant("future-type","quarterly",false,json!(["seven_day"])),
        grant("future-scope","weekly",false,json!(["seven_day_telescope"])),
        grant("cowork","weekly",false,json!(["seven_day_cowork","seven_day_omelette","seven_day_oauth_apps"])),
        blocked_grant]);
    let list = inv(&[&value], inside());
    let kind = credit_by(&list, "future-type");
    assert_eq!((kind.reset_type.as_str(), kind.usable, kind.unavailable_reason.as_deref()), ("unknown", false, Some("unknown_type")));
    let scope = credit_by(&list, "future-scope");
    assert_eq!(scope.clears, vec!["anthropic:reset:seven_day_telescope"]);
    assert_eq!((scope.usable, scope.unavailable_reason.as_deref()), (false, Some("unknown_scope")));
    assert!(credit_by(&list, "cowork").usable);
    assert_eq!(credit_by(&list, "cowork").clears, vec!["anthropic:reset:seven_day_cowork", "anthropic:reset:seven_day_omelette", "anthropic:reset:seven_day_oauth_apps"]);
    let blocked = credit_by(&list, "blocked");
    assert_eq!((blocked.usable, blocked.blocking.clone(), blocked.unavailable_reason.as_deref()), (false, vec!["anthropic:7d:opus".to_string()], Some("blocked")));
    assert_eq!(list.redeemable, Some(1));
    // Malformed shapes are still errors, not zero.
    let mut bad = web_like();
    bad["cedar_ember"]["grants"][0]["clears"] = json!("seven_day");
    assert!(claude_inventory(&[&bad], inside()).is_err());
}
#[test]
fn malformed_and_contradictory_codex_balances_are_not_zero_or_spend_authority() {
    assert!(codex_inventory(&json!({}), 1).is_err());
    let empty = codex_inventory(&json!({"credits":[],"available_count":0}), 1).unwrap();
    assert_eq!((empty.available, empty.redeemable), (Some(0), Some(0)));
    assert!(codex_inventory(&json!({"credits":[{"id":"credit-a"}],"available_count":0}), 1).is_err());
    assert!(codex_inventory(&json!({"credits":[{"id":"credit-a"},{"id":"credit-a"}]}), 1).is_err());
    let expired = codex_inventory(&json!({"credits":[{"id":"credit-a","status":"available","expires_at":"2000-01-01T00:00:00Z"}],"available_count":1}), now_ms()).unwrap();
    assert_eq!(expired.redeemable, Some(0));
    let live = codex_inventory(&json!({"credits":[{"id":"credit-a"}],"available_count":1}), 1).unwrap();
    assert_eq!((live.credits[0].reset_type.as_str(), live.credits[0].usable), ("provider", true));
}
#[test]
fn surface_restriction_is_not_a_zero_balance_and_cannot_authorize_use() {
    let cedar_surface = json!({"five_hour":{},"cedar_ember":{"eligible":false,"ineligible_reason":"surface"}});
    let wall_surface = json!({"five_hour":{},"juniper_tide":{"eligible":false,"ineligible_reason":"surface"}});
    let list = inv(&[&cedar_surface, &wall_surface], 1);
    assert_eq!((list.state, list.reason), ("restricted", Some("surface")));
    assert_eq!((list.available, list.redeemable), (None, None));
    // Restricted offers stay visible as observations but are not spendable.
    let mut offered = cedar();
    offered["cedar_ember"]["eligible"] = json!(false);
    offered["cedar_ember"]["ineligible_reason"] = json!("cli_version");
    let seen = inv(&[&offered, &wall_surface], 1);
    assert_eq!(seen.state, "restricted");
    assert_eq!(seen.credits.len(), 1);
    assert!(!seen.credits[0].usable);
    assert_eq!(seen.credits[0].unavailable_reason.as_deref(), Some("cli_version"));
    // A genuine no_grant is the only known zero.
    let none = json!({"five_hour":{},"cedar_ember":{"eligible":false,"ineligible_reason":"no_grant"}});
    let zero = inv(&[&none, &json!({"five_hour":{}})], 1);
    assert_eq!((zero.state, zero.available, zero.redeemable), ("known", Some(0), Some(0)));
    // No program named at all is a missing answer.
    let missing = inv(&[&json!({"five_hour":{}}), &json!({"seven_day":{}})], 1);
    assert_eq!((missing.state, missing.reason, missing.available), ("unknown", Some("program_missing"), None));
    // A no_grant contradicted by listed offers is ambiguous, never zero.
    let mut contradicted = cedar();
    contradicted["cedar_ember"]["eligible"] = json!(false);
    contradicted["cedar_ember"]["ineligible_reason"] = json!("no_grant");
    assert_eq!(inv(&[&contradicted], 1).state, "restricted");
    assert!(claude_inventory(&[&json!("nope")], 1).is_err());
    assert!(claude_inventory(&[&json!({"cedar_ember":{"eligible":"yes"}})], 1).is_err());
}
#[test]
fn cedar_in_at_wall_and_juniper_coexist_without_losing_or_duplicating_offers() {
    let cedar_read = json!({"five_hour":{},"cedar_ember":null});
    let mut wall = cedar();
    wall["juniper_tide"] = json!({"eligible":true,"arm":"reset","available":true});
    let list = inv(&[&cedar_read, &wall], 1);
    assert_eq!(list.state, "known");
    assert_eq!(list.credits.len(), 2);
    assert_eq!((list.available, list.redeemable), (Some(4), Some(4)));
    assert_eq!(credit_by(&list, "grant-a").program, "cedar_ember");
    assert_eq!(credit_by(&list, "juniper_tide").reset_type, "provider");
    // The same grant in both reads is one offer; the more conservative live answer wins.
    let mut second = cedar();
    second["cedar_ember"]["grants"][0]["usable_now"] = json!(false);
    let twice = inv(&[&cedar(), &second], 1);
    assert_eq!(twice.credits.len(), 1);
    assert_eq!((twice.available, twice.redeemable), (Some(3), Some(0)));
    // The same id with different boundaries, or under another program, is ambiguous and not spendable.
    let mut changed = cedar();
    changed["cedar_ember"]["grants"][0]["resets_left"] = json!(2);
    let conflict = inv(&[&cedar(), &changed], 1);
    assert_eq!((conflict.state, conflict.reason), ("restricted", Some("ambiguous")));
    assert_eq!(conflict.credits.len(), 1);
    let mut same_id = json!({"juniper_tide":{"eligible":true,"arm":"reset","available":true}});
    let mut cedar_named = cedar();
    cedar_named["cedar_ember"]["grants"][0]["id"] = json!("juniper_tide");
    cedar_named["cedar_ember"]["next_grant_id"] = json!("juniper_tide");
    same_id["cedar_ember"] = cedar_named["cedar_ember"].clone();
    let clash = inv(&[&same_id], 1);
    assert_eq!(clash.credits.len(), 2);
    assert!(clash.credits.iter().all(|c| !c.usable));
    assert_ne!(clash.credits[0].id, clash.credits[1].id);
}
#[test]
fn claude_discovery_reads_both_usage_responses_without_spending() {
    let server = Server::new(vec![Some((200,json!({"five_hour":{},"cedar_ember":null}))),
        Some((200,json!({"juniper_tide":{"eligible":true,"arm":"reset","available":true}})))]);
    let list = Provider::for_test(server.url.clone()).list(&auth("anthropic")).unwrap();
    assert_eq!((list.available, list.redeemable), (Some(1), Some(1)));
    assert_eq!(list.credits[0].clears, vec!["anthropic:5h"]);
    assert_eq!(server.posts(), 0);
    assert!(server.requests.lock()[0].0.contains("cedar_ember=1&skip_spend=1"));
    assert!(server.requests.lock()[1].0.contains("at_wall=1&skip_spend=1"));
    // A positive Cedar answer no longer short-circuits the second read.
    let both = Server::new(vec![Some((200,cedar())), Some((200,json!({"five_hour":{},"juniper_tide":{"eligible":true,"arm":"reset","available":true}})))]);
    let merged = Provider::for_test(both.url.clone()).list(&auth("anthropic")).unwrap();
    assert_eq!(merged.credits.len(), 2);
    assert_eq!(both.requests.lock().len(), 2);
}
#[test]
fn actual_http_cedar_reset_requires_an_explicit_success_result() {
    let server = Server::new(vec![Some((200,cedar())), Some((200,cedar())), Some((200,json!({"result":"reset","cleared":["five_hour"]})))]);
    let client = Provider::for_test(server.url.clone());
    let auth = auth("anthropic");
    let offer = client.list(&auth).unwrap();
    assert_eq!(offer.credits.len(), 1, "the same grant in both reads is one offer");
    assert_eq!(client.consume(&auth, &offer.credits[0], "request-one"), ("succeeded", "reset"));
    assert_eq!(server.posts(), 1);
    assert!(server.requests.lock()[2].0.starts_with("POST /api/organizations/org-a/reset_rate_limits "));
    // A credit of unknown type or program is never dispatched.
    let mut unknown = offer.credits[0].clone();
    unknown.reset_type = "unknown".into();
    assert_eq!(client.consume(&auth, &unknown, "request-three"), ("declined", "unsupported_program"));
    assert_eq!(server.posts(), 1);
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
