use super::*;
use crate::reset_provider::test_http::Server;
use aam_protocol::Paths;
use serde_json::json;
use std::{path::PathBuf, sync::Barrier, thread};

struct Fixture { service: Option<Arc<Service>>, directory: PathBuf }
impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("ojak-reset-test-{}", new_id()));
        std::fs::create_dir_all(&directory).unwrap();
        let paths = Paths { home: directory.clone(), database: directory.join("state.sqlite3"), socket: directory.join("control.sock"), profiles: directory.join("profiles") };
        Self { service: Some(Service::open(paths).unwrap()), directory }
    }
    fn service(&self) -> &Arc<Service> { self.service.as_ref().unwrap() }
    fn quote(&self, id: &str) -> RedeemRequest {
        let account = Account { id: id.into(), tool: "codex".into(), provider: "openai".into(),
            identity_key: Some("fixture-identity".into()), auth_status: "authenticated".into(), enabled: true,
            ..Default::default() };
        let store = self.service().lock().unwrap();
        save_account(&store.connection, &account).unwrap();
        let quote_id = new_id();
        let quote = Quote { owner: owner(&auth()).unwrap(), binding: binding_fingerprint(&account).unwrap(), after_request: None,
            inventory: ResetInventory { account_id: id.into(), state: "known".into(), available_count: Some(1), redeemable_count: Some(1),
                credits: vec![credit()], observed_at: now_ms(), quote_id: Some(quote_id.clone()), operation: None } };
        set_metadata(&store.connection, &quote_key(id), &quote).unwrap();
        RedeemRequest { account_id: id.into(), quote_id, credit_id: "credit-a".into(), request_id: new_id(), confirmed: true }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) { self.service.take(); std::fs::remove_dir_all(&self.directory).unwrap(); }
}
fn auth() -> Auth { Auth { provider: "openai-codex", access: "fixture-only".into(), account_id: Some("physical-account".into()), email: None, org_id: None } }
fn credit() -> ResetCredit {
    ResetCredit { id: "credit-a".into(), program: "codex_rate_limits".into(), remaining_count: 1, usable: true,
        expires_at: None, starts_at: None, clears: vec!["openai-codex:rate_limits".into()], requires_limit: true, status: "available".into() }
}
fn inventory_body() -> serde_json::Value { json!({"credits":[{"id":"credit-a","status":"available"}],"available_count":1}) }

#[test]
fn concurrent_aliases_claim_one_physical_account_and_repeats_do_not_dispatch() {
    let fixture = Fixture::new();
    let requests = [fixture.quote("alias-a"), fixture.quote("alias-b")];
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = requests.into_iter().map(|request| {
        let service = fixture.service().clone(); let barrier = barrier.clone();
        thread::spawn(move || { barrier.wait(); let result = service.reset_begin(&request); (request, result) })
    }).collect();
    let results: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|(_, result)| result.as_ref().is_ok_and(|(_, execute)| *execute)).count(), 1);
    assert_eq!(results.iter().filter_map(|(_, result)| result.as_ref().err()).next().unwrap().code, "RESET_IN_PROGRESS");
    let (request, _) = results.iter().find(|(_, result)| result.is_ok()).unwrap();
    let (record, execute) = fixture.service().reset_begin(request).unwrap();
    assert!(!execute);
    assert_eq!(record.operation.state, "preparing");
}

#[test]
fn explicit_confirmation_binding_and_single_use_quote_are_required() {
    let fixture = Fixture::new(); let service = fixture.service();
    let mut request = fixture.quote("account-a");
    request.confirmed = false;
    assert_eq!(service.reset_begin(&request).err().unwrap().code, "RESET_OFFER_CHANGED");
    request.confirmed = true;
    let (mut record, _) = service.reset_begin(&request).unwrap();
    service.reset_finish(&mut record, "declined", "nothing_to_reset").unwrap();
    request.request_id = new_id();
    assert_eq!(service.reset_begin(&request).err().unwrap().code, "RESET_OFFER_CHANGED");
    let changed = fixture.quote("account-b");
    { let store = service.lock().unwrap(); let mut account = account(&store.connection, "account-b").unwrap();
      account.enabled = false; save_account(&store.connection, &account).unwrap(); }
    assert_eq!(service.reset_begin(&changed).err().unwrap().code, "RESET_OFFER_CHANGED");
}

#[test]
fn transport_reset_persists_unknown_and_survives_restart_without_another_post() {
    let fixture = Fixture::new(); let service = fixture.service();
    let request = fixture.quote("account-a"); let alias = fixture.quote("alias-a");
    let (mut record, _) = service.reset_begin(&request).unwrap();
    let server = Server::new(vec![Some((200, inventory_body())), None]);
    let outcome = service.reset_send(&mut record, &auth(), &Provider::for_test(server.url.clone())).unwrap();
    assert_eq!(outcome.state, "unknown");
    assert_eq!(server.posts(), 1);
    let reopened = Service::open(service.paths.clone()).unwrap();
    let repeated = reopened.reset_redeem(request).unwrap();
    assert_eq!(repeated.state, "unknown");
    assert_eq!(reopened.reset_begin(&alias).err().unwrap().code, "RESET_IN_PROGRESS");
    let cached = reopened.reset_read(ReadRequest { account_id: "account-a".into(), refresh: false }).unwrap();
    assert_eq!(cached.state, "unknown");
    assert!(cached.quote_id.is_none());
    assert_eq!(server.posts(), 1);
}

#[test]
fn changed_provider_offer_is_rejected_before_post_and_success_is_idempotent() {
    let fixture = Fixture::new(); let service = fixture.service();
    let request = fixture.quote("account-a"); let (mut record, _) = service.reset_begin(&request).unwrap();
    let changed = Server::new(vec![Some((200,json!({"credits":[{"id":"credit-a","status":"redeemed"}],"available_count":0})))]);
    assert_eq!(service.reset_send(&mut record, &auth(), &Provider::for_test(changed.url.clone())).unwrap_err().code, "RESET_OFFER_CHANGED");
    assert_eq!(changed.posts(), 0);
    let server = Server::new(vec![Some((200,inventory_body())), Some((200,json!({"code":"reset"})))]);
    assert_eq!(service.reset_send(&mut record, &auth(), &Provider::for_test(server.url.clone())).unwrap().state, "succeeded");
    assert_eq!(service.reset_redeem(request).unwrap().state, "succeeded");
    assert_eq!(server.posts(), 1);
    let cached = service.reset_read(ReadRequest { account_id: "account-a".into(), refresh: false }).unwrap();
    assert_eq!(cached.available_count, None);
}

#[test]
fn restart_distinguishes_unsubmitted_work_from_an_ambiguous_sent_request() {
    let fixture = Fixture::new(); let service = fixture.service();
    let request = fixture.quote("account-a"); let (mut record, _) = service.reset_begin(&request).unwrap();
    let next_instance = new_id();
    assert!(after_restart(&mut record, &next_instance));
    assert_eq!((record.operation.state.as_str(), record.operation.code.as_str()), ("declined", "preparation_interrupted"));
    record.operation.state = "submitted".into();
    assert!(after_restart(&mut record, &next_instance));
    assert_eq!(record.operation.state, "unknown");
    let mut offer = credit(); offer.program = "cedar_ember".into(); offer.remaining_count = 3;
    record.credit = offer.clone(); offer.remaining_count = 2;
    let mut current = Inventory { available: 2, redeemable: 2, credits: vec![offer] };
    assert!(!reconcile(&mut record, &current), "a partial balance decrease does not prove this request completed");
    current.credits[0].remaining_count = 0;
    assert!(reconcile(&mut record, &current));
    assert_eq!(record.operation.state, "resolved");
    record.operation.state = "unknown".into(); record.credit.program = "juniper_tide".into();
    record.credit.expires_at = Some(now_ms() - 1); current.credits[0] = record.credit.clone();
    assert!(!reconcile(&mut record, &current));
    current.credits[0].expires_at = Some(now_ms() + 86_400_000);
    assert!(reconcile(&mut record, &current));
}
