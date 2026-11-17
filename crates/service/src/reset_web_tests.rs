use super::*;
use aam_protocol::{Account, Paths};
use std::path::PathBuf;

const EMAIL: &str = "person@example.test";
const ORG: &str = "11111111-2222-4333-8444-555555555555";
const SUBJECT: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

struct Fixture { service: Option<Arc<Service>>, paths: Paths, directory: PathBuf, t0: Clock }
impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("ojak-reset-web-test-{}", new_id()));
        std::fs::create_dir_all(&directory).unwrap();
        let paths = Paths { home: directory.clone(), database: directory.join("state.sqlite3"), socket: directory.join("control.sock"), profiles: directory.join("profiles") };
        Self { service: Some(Service::open(paths.clone()).unwrap()), paths, directory, t0: Clock::now() }
    }
    fn service(&self) -> &Arc<Service> { self.service.as_ref().unwrap() }
    fn at(&self, ms: i64) -> Clock { Clock { wall: self.t0.wall + ms, mono: self.t0.mono + Duration::from_millis(ms as u64) } }
    fn account(&self, id: &str, identity: &str, email: Option<&str>) {
        let account = Account { id: id.into(), tool: "claude".into(), provider: "anthropic".into(), identity_key: Some(identity.into()),
            email: email.map(Into::into), auth_status: "authenticated".into(), enabled: true, ..Default::default() };
        save_account(&self.service().lock().unwrap().connection, &account).unwrap();
    }
    fn standard(&self, id: &str) { self.account(id, &format!("anthropic|email:{EMAIL}|workspace:{ORG}"), Some(EMAIL)); }
    fn restart(&mut self) {
        self.service.take();
        self.service = Some(Service::open(self.paths.clone()).unwrap());
    }
    fn claim(&self, ms: i64) -> (String, Vec<ResetWebJob>) {
        let client = new_id();
        let polled = self.service().web_poll(&client, self.at(ms)).unwrap();
        (client, polled.jobs)
    }
    fn pending(&self) -> usize { self.service().reset_web.inner.lock().jobs.len() }
    fn stored(&self, id: &str) -> Option<Stored> { read(&self.service().lock().unwrap().connection, id).unwrap() }
    fn complete(&self, client: &str, job: &ResetWebJob, result: Value, ms: i64) -> bool {
        self.service().web_complete(CompleteRequest { client_id: client.into(), request_id: job.request_id.clone(),
            connection_id: job.connection_id.clone(), result }, self.at(ms)).unwrap()
    }
    fn inventory(&self, id: &str, ms: i64) -> ResetInventory {
        self.service().web_inventory(id, false, Duration::ZERO, self.at(ms)).unwrap().expect("web connected")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) { self.service.take(); let _ = std::fs::remove_dir_all(&self.directory); }
}

fn grant(count: u64) -> Value {
    json!({"id":"weekly-1","resets_left":count,"usable_now":true,"clears":["seven_day"],"blocking":[],"use_requires_limit":false,"type":"weekly"})
}
fn values(count: u64) -> Value {
    let cedar = json!({"eligible":true,"next_grant_id":"weekly-1","grants":[grant(count)]});
    json!([{"cedar_ember":cedar,"juniper_tide":null},
        {"cedar_ember":cedar,"juniper_tide":{"eligible":false,"ineligible_reason":"not_at_wall","arm":null,
            "available":false,"in_experiment":false,"weekly_resets_at":null,"next_available_at":null}}])
}
fn success_for(email: &str, subject: &str, org: &str, member: bool, count: u64) -> Value {
    json!({"email":email,"accountUuid":subject,"organizationId":org,"member":member,"values":values(count)})
}
fn success(count: u64) -> Value { success_for(EMAIL, SUBJECT, ORG, true, count) }

#[test]
fn connect_queues_one_job_and_repeated_requests_and_polls_are_deduplicated() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    let connected = fixture.service().web_connect("acc", fixture.at(0)).unwrap();
    assert!(connected.status.connected);
    assert_eq!(connected.status.state, "waiting_browser");
    assert_eq!(connected.connection.organization_id, ORG);
    // A page-load request and a second connect reuse the outstanding job.
    fixture.service().web_request("acc", fixture.at(1_000)).unwrap();
    fixture.service().web_connect("acc", fixture.at(2_000)).unwrap();
    assert_eq!(fixture.pending(), 1);
    let (_, first) = fixture.claim(3_000);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].connection_id, connected.connection.connection_id);
    assert!(first[0].expires_at > fixture.at(3_000).wall);
    // A claimed job is never handed out again.
    assert!(fixture.claim(3_500).1.is_empty());
    fixture.service().web_request("acc", fixture.at(4_000)).unwrap();
    assert_eq!(fixture.pending(), 1);
    assert_eq!(fixture.service().web_status("acc", fixture.at(4_000)).unwrap().state, "syncing");
    // Reconnecting the same binding keeps the isolated profile.
    let again = fixture.service().web_connect("acc", fixture.at(5_000)).unwrap();
    assert_eq!(again.connection.connection_id, connected.connection.connection_id);
}

#[test]
fn only_the_exact_claimed_job_can_complete_once_and_replays_never_overwrite() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    fixture.service().web_connect("acc", fixture.at(0)).unwrap();
    // Not claimed yet: nobody can answer it.
    let (client, jobs) = fixture.claim(1_000);
    let job = &jobs[0];
    assert!(!fixture.complete(&new_id(), job, success(1), 1_500), "another client");
    let mut wrong_connection = job.clone();
    wrong_connection.connection_id = new_id();
    assert!(!fixture.complete(&client, &wrong_connection, success(1), 1_500));
    let mut wrong_request = job.clone();
    wrong_request.request_id = new_id();
    assert!(!fixture.complete(&client, &wrong_request, success(1), 1_500));
    assert!(fixture.stored("acc").unwrap().observation.is_none());
    assert!(fixture.complete(&client, job, success(2), 2_000));
    let inventory = fixture.inventory("acc", 2_100);
    assert_eq!((inventory.source.as_str(), inventory.state.as_str(), inventory.available_count), ("web", "known", Some(2)));
    assert!(inventory.quote_id.is_none());
    assert_eq!(inventory.credits[0].id, "weekly-1");
    assert_eq!(inventory.web.as_ref().unwrap().state, "ready");
    // Replaying the same job with other content changes nothing.
    assert!(!fixture.complete(&client, job, success(9), 2_500));
    assert!(!fixture.complete(&client, job, json!({"errorCode":"WEB_FETCH_FAILED"}), 2_500));
    assert_eq!(fixture.inventory("acc", 2_600).available_count, Some(2));
}

#[test]
fn another_accounts_session_is_rejected_and_never_replaces_the_observation() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    fixture.service().web_connect("acc", fixture.at(0)).unwrap();
    let (client, jobs) = fixture.claim(100);
    assert!(fixture.complete(&client, &jobs[0], success(1), 200));
    let seen = fixture.inventory("acc", 300);
    assert_eq!(seen.available_count, Some(1));
    let cases = [
        ("email", success_for("other@example.test", SUBJECT, ORG, true, 7)),
        ("workspace", success_for(EMAIL, SUBJECT, "99999999-2222-4333-8444-555555555555", true, 7)),
        ("membership", success_for(EMAIL, SUBJECT, ORG, false, 7)),
        ("case-insensitive email is the same person", success_for(&EMAIL.to_uppercase(), SUBJECT, ORG, true, 3)),
    ];
    let mut ms = 300_000;
    for (name, result) in cases {
        let expect_mismatch = name != "case-insensitive email is the same person";
        fixture.service().web_schedule(fixture.at(ms), false).unwrap();
        let (client, jobs) = fixture.claim(ms + 10);
        assert_eq!(jobs.len(), 1, "{name}");
        assert!(fixture.complete(&client, &jobs[0], result, ms + 20), "{name}: a recorded failure is accepted");
        let status = fixture.service().web_status("acc", fixture.at(ms + 30)).unwrap();
        if expect_mismatch {
            assert_eq!((status.state.as_str(), status.error_code.as_deref()), ("error", Some("WEB_ACCOUNT_MISMATCH")), "{name}");
            let inventory = fixture.inventory("acc", ms + 40);
            assert_eq!(inventory.credits.len(), 1, "{name}: old offers are kept as a reference");
            assert_eq!(inventory.credits[0].remaining_count, 1, "{name}");
            assert_eq!((inventory.state.as_str(), inventory.available_count, inventory.redeemable_count), ("unknown", None, None), "{name}");
            assert!(!inventory.credits[0].usable, "{name}");
        } else {
            assert_eq!(status.state, "ready", "{name}");
        }
        ms += 300_000;
    }
}

#[test]
fn subject_only_identities_are_matched_exactly() {
    let fixture = Fixture::new();
    fixture.account("acc", &format!("anthropic|subject:{SUBJECT}|workspace:{ORG}"), None);
    fixture.service().web_connect("acc", fixture.at(0)).unwrap();
    let (client, jobs) = fixture.claim(10);
    assert!(fixture.complete(&client, &jobs[0], success_for(EMAIL, "ffffffff-bbbb-4ccc-8ddd-eeeeeeeeeeee", ORG, true, 1), 20));
    assert_eq!(fixture.stored("acc").unwrap().error_code.as_deref(), Some("WEB_ACCOUNT_MISMATCH"));
    fixture.service().web_schedule(fixture.at(300_000), false).unwrap();
    let (client, jobs) = fixture.claim(300_010);
    assert!(fixture.complete(&client, &jobs[0], success_for(EMAIL, &SUBJECT.to_uppercase(), ORG, true, 1), 300_020));
    assert_eq!(fixture.inventory("acc", 300_030).available_count, Some(1));
}

#[test]
fn an_account_without_a_workspace_or_identity_cannot_connect() {
    let fixture = Fixture::new();
    fixture.account("no-workspace", "anthropic|email:person@example.test", Some(EMAIL));
    assert_eq!(fixture.service().web_connect("no-workspace", fixture.at(0)).err().unwrap().code, "RESET_WEB_IDENTITY_UNMAPPED");
    fixture.account("no-person", "anthropic|workspace:org-a", None);
    assert_eq!(fixture.service().web_connect("no-person", fixture.at(0)).err().unwrap().code, "RESET_WEB_IDENTITY_UNMAPPED");
    assert!(fixture.stored("no-workspace").is_none() && fixture.stored("no-person").is_none());
    let mut codex = Account { id: "codex".into(), tool: "codex".into(), provider: "openai".into(), identity_key: Some(format!("openai-codex|workspace:{ORG}")), ..Default::default() };
    codex.auth_status = "authenticated".into();
    save_account(&fixture.service().lock().unwrap().connection, &codex).unwrap();
    assert_eq!(fixture.service().web_connect("codex", fixture.at(0)).err().unwrap().code, "RESET_UNSUPPORTED");
}

#[test]
fn disconnect_revokes_jobs_and_observation_and_a_new_connection_gets_a_new_profile() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    let first = fixture.service().web_connect("acc", fixture.at(0)).unwrap();
    let (client, jobs) = fixture.claim(10);
    assert!(fixture.complete(&client, &jobs[0], success(1), 20));
    fixture.service().web_schedule(fixture.at(300_000), false).unwrap();
    let (client, jobs) = fixture.claim(300_010);
    assert_eq!(jobs.len(), 1);
    // Disconnect while the native read is in flight.
    let status = fixture.service().web_disconnect("acc").unwrap();
    assert_eq!((status.connected, status.state.as_str()), (false, "disconnected"));
    assert!(!fixture.complete(&client, &jobs[0], success(5), 300_020), "a revoked job is not authority");
    assert!(fixture.stored("acc").is_none());
    assert!(fixture.service().web_inventory("acc", false, Duration::ZERO, fixture.at(300_030)).unwrap().is_none(), "reads fall back to OAuth only after disconnect");
    assert_eq!(fixture.service().web_poll(&new_id(), fixture.at(300_040)).unwrap().connections.len(), 0);
    let second = fixture.service().web_connect("acc", fixture.at(400_000)).unwrap();
    assert_ne!(second.connection.connection_id, first.connection.connection_id);
    assert!(fixture.stored("acc").unwrap().observation.is_none(), "no observation survives a disconnect");
}

#[test]
fn binding_changes_and_account_removal_revoke_the_connection() {
    let fixture = Fixture::new();
    fixture.standard("moved");
    fixture.standard("removed");
    fixture.service().web_connect("moved", fixture.at(0)).unwrap();
    fixture.service().web_connect("removed", fixture.at(0)).unwrap();
    let (client, jobs) = fixture.claim(10);
    assert_eq!(jobs.len(), 2);
    let moved = jobs.iter().find(|job| job.account_id == "moved").unwrap();
    // The account is re-logged-in as someone else.
    fixture.account("moved", &format!("anthropic|email:other@example.test|workspace:{ORG}"), Some("other@example.test"));
    assert!(!fixture.complete(&client, moved, success(1), 20));
    assert!(fixture.stored("moved").is_none());
    assert!(fixture.service().web_inventory("moved", false, Duration::ZERO, fixture.at(30)).unwrap().is_none());
    // Removing an account removes its connection and pending job.
    fixture.service().dispatch("account.remove", json!({"id":"removed"})).unwrap();
    assert!(fixture.stored("removed").is_none());
    assert_eq!(fixture.pending(), 0);
    let removed = jobs.iter().find(|job| job.account_id == "removed").unwrap();
    assert!(!fixture.complete(&client, removed, success(1), 40));
}

#[test]
fn allocation_changes_do_not_drop_the_connection_but_cross_workspace_does() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    fixture.service().web_connect("acc", fixture.at(0)).unwrap();
    let mut account = crate::store::account(&fixture.service().lock().unwrap().connection, "acc").unwrap();
    account.enabled = false;
    account.auth_status = "auth-required".into();
    save_account(&fixture.service().lock().unwrap().connection, &account).unwrap();
    assert!(fixture.service().web_status("acc", fixture.at(10)).unwrap().connected);
    account.identity_key = Some(format!("anthropic|email:{EMAIL}|workspace:another-org"));
    save_account(&fixture.service().lock().unwrap().connection, &account).unwrap();
    assert!(!fixture.service().web_status("acc", fixture.at(20)).unwrap().connected);
}

#[test]
fn the_schedule_is_five_minutes_from_the_attempt_and_stale_offers_are_withdrawn_but_kept() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    fixture.service().web_connect("acc", fixture.at(0)).unwrap();
    let (client, jobs) = fixture.claim(1_000);
    assert!(fixture.complete(&client, &jobs[0], success(1), 2_000));
    let status = fixture.service().web_status("acc", fixture.at(2_000)).unwrap();
    assert_eq!((status.last_success_at, status.next_refresh_at), (Some(fixture.t0.wall + 2_000), Some(fixture.t0.wall + 300_000)));
    fixture.service().web_schedule(fixture.at(299_999), false).unwrap();
    assert_eq!(fixture.pending(), 0, "not due yet");
    fixture.service().web_schedule(fixture.at(300_000), false).unwrap();
    assert_eq!(fixture.pending(), 1, "due exactly five minutes after the attempt began");
    fixture.service().web_schedule(fixture.at(300_500), false).unwrap();
    fixture.service().web_schedule(fixture.at(301_000), true).unwrap();
    assert_eq!(fixture.pending(), 1, "no duplicate outstanding job");
    // Fresh until five minutes after the observation, then withdrawn without losing the offers.
    let fresh = fixture.inventory("acc", 302_000);
    assert_eq!((fresh.state.as_str(), fresh.redeemable_count), ("known", Some(1)));
    let stale = fixture.inventory("acc", 302_001);
    assert_eq!((stale.state.as_str(), stale.available_count, stale.redeemable_count, stale.lookup_reason.as_deref()), ("unknown", None, None, Some("stale")));
    assert_eq!(stale.credits.len(), 1);
    assert!(!stale.credits[0].usable && stale.quote_id.is_none());
    assert_eq!(stale.observed_at, fixture.t0.wall + 2_000);
}

#[test]
fn a_backwards_wall_clock_does_not_postpone_the_next_attempt_beyond_five_minutes() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    fixture.service().web_connect("acc", fixture.at(0)).unwrap();
    let (client, jobs) = fixture.claim(10);
    assert!(fixture.complete(&client, &jobs[0], success(1), 20));
    // The wall clock jumped an hour back after the attempt: the refresh is due at once, not an hour later.
    let stored = fixture.stored("acc").unwrap();
    let earlier = Clock { wall: fixture.t0.wall - 3_600_000, mono: fixture.at(30).mono };
    assert!(due(&stored, &earlier, false));
    // Normal timing is unaffected: not due before five minutes, due at five minutes.
    assert!(!due(&stored, &fixture.at(299_999), false));
    assert!(due(&stored, &fixture.at(300_000), false));
    // And the status never promises an attempt further away than one interval.
    let status = fixture.service().web_status("acc", earlier).unwrap();
    assert_eq!(status.next_refresh_at, Some(earlier.wall + 300_000));
}

#[test]
fn failures_preserve_the_last_observation_and_never_look_like_zero_or_spendable() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    fixture.service().web_connect("acc", fixture.at(0)).unwrap();
    let (client, jobs) = fixture.claim(10);
    assert!(fixture.complete(&client, &jobs[0], success(1), 20));
    let good = fixture.stored("acc").unwrap();
    let mut ms = 300_000;
    for (code, state) in [("WEB_FETCH_FAILED", "error"), ("WEB_LOGIN_REQUIRED", "login_required"), ("WEB_BROWSER_UNAVAILABLE", "waiting_browser"),
        ("WEB_RESPONSE_INVALID", "error"), ("WEB_UNSUPPORTED_PLATFORM", "error"), ("WEB_ACCOUNT_MISMATCH", "error")] {
        fixture.service().web_schedule(fixture.at(ms), false).unwrap();
        let (client, jobs) = fixture.claim(ms + 10);
        assert!(fixture.complete(&client, &jobs[0], json!({"errorCode":code}), ms + 20), "{code}");
        let stored = fixture.stored("acc").unwrap();
        assert_eq!(stored.state, state, "{code}");
        assert_eq!(stored.last_success_at, good.last_success_at, "{code}");
        assert_eq!(stored.observation.as_ref().unwrap().observed_at, good.observation.as_ref().unwrap().observed_at, "{code}");
        assert_eq!(stored.next_refresh_at, Some(fixture.t0.wall + ms + 300_000), "{code}: the next attempt stays on the normal cadence");
        let inventory = fixture.inventory("acc", ms + 30);
        assert_eq!((inventory.state.as_str(), inventory.available_count, inventory.redeemable_count), ("unknown", None, None), "{code}");
        assert_eq!(inventory.credits[0].remaining_count, 1, "{code}");
        assert!(!inventory.credits[0].usable && inventory.quote_id.is_none(), "{code}");
        assert_eq!(inventory.web.as_ref().unwrap().error_code.as_deref(), Some(code));
        ms += 300_000;
    }
    // A later success clears the error and restores the current balance.
    fixture.service().web_schedule(fixture.at(ms), false).unwrap();
    let (client, jobs) = fixture.claim(ms + 10);
    assert!(fixture.complete(&client, &jobs[0], success(4), ms + 20));
    let healed = fixture.inventory("acc", ms + 30);
    assert_eq!((healed.state.as_str(), healed.available_count, healed.web.unwrap().error_code), ("known", Some(4), None));
}

#[test]
fn malformed_results_are_recorded_as_invalid_without_replacing_the_observation() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    fixture.service().web_connect("acc", fixture.at(0)).unwrap();
    let (client, jobs) = fixture.claim(10);
    assert!(fixture.complete(&client, &jobs[0], success(1), 20));
    let mut huge = success(1);
    huge["padding"] = json!("x".repeat(MAX_RESULT_BYTES));
    let bad = [
        json!({"errorCode":"SOMETHING_ELSE"}),
        json!({"errorCode":"WEB_FETCH_FAILED","email":EMAIL}),
        json!("nope"),
        json!({"email":EMAIL,"accountUuid":SUBJECT,"organizationId":ORG,"member":true,"values":[{"five_hour":{}}]}),
        json!({"email":EMAIL,"accountUuid":"not-a-uuid","organizationId":ORG,"member":true,"values":values(1)}),
        json!({"email":EMAIL,"accountUuid":SUBJECT,"organizationId":ORG,"member":true,"values":[{"unrelated":1},{"unrelated":2}]}),
        json!({"email":EMAIL,"accountUuid":SUBJECT,"organizationId":ORG,"member":true,"values":[{"five_hour":{}},{"five_hour":{}}]}),
        huge,
    ];
    let mut ms = 300_000;
    for (index, result) in bad.into_iter().enumerate() {
        fixture.service().web_schedule(fixture.at(ms), false).unwrap();
        let (client, jobs) = fixture.claim(ms + 10);
        assert!(fixture.complete(&client, &jobs[0], result, ms + 20), "{index}");
        let stored = fixture.stored("acc").unwrap();
        assert_eq!(stored.error_code.as_deref(), Some("WEB_RESPONSE_INVALID"), "{index}");
        assert_eq!(stored.observation.as_ref().unwrap().credits.len(), 1, "{index}");
        ms += 300_000;
    }
}

#[test]
fn an_absent_or_silent_browser_is_bounded_stale_and_a_returning_poll_retries_at_once() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    fixture.service().web_connect("acc", fixture.at(0)).unwrap();
    let (client, jobs) = fixture.claim(1_000);
    assert!(fixture.complete(&client, &jobs[0], success(1), 1_500));
    fixture.service().web_schedule(fixture.at(300_000), false).unwrap();
    assert_eq!(fixture.pending(), 1);
    // Nobody claims it: after the unclaimed window it becomes a recorded absence, not a hang.
    fixture.service().web_sweep(fixture.at(300_000 + UNCLAIMED_MS - 1)).unwrap();
    assert_eq!(fixture.pending(), 1);
    fixture.service().web_sweep(fixture.at(300_000 + UNCLAIMED_MS)).unwrap();
    assert_eq!(fixture.pending(), 0);
    let stored = fixture.stored("acc").unwrap();
    assert_eq!((stored.state.as_str(), stored.error_code.as_deref()), ("waiting_browser", Some("WEB_BROWSER_UNAVAILABLE")));
    assert_eq!(stored.next_refresh_at, Some(fixture.t0.wall + 600_000), "normal cadence, not a retry storm");
    assert_eq!(fixture.inventory("acc", 331_000).credits.len(), 1);
    // The browser comes back well before the next interval: the poll schedules the missed refresh immediately.
    let (_, jobs) = fixture.claim(340_000);
    assert_eq!(jobs.len(), 1);
    // A claimed job that is never answered fails as a fetch failure after its lifetime.
    fixture.service().web_sweep(fixture.at(340_000 + JOB_TTL_MS)).unwrap();
    let stored = fixture.stored("acc").unwrap();
    assert_eq!(stored.error_code.as_deref(), Some("WEB_FETCH_FAILED"));
    assert!(!stored.absent);
}

#[test]
fn an_explicit_read_waits_for_the_result_and_reports_an_absent_browser_without_oauth() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    fixture.service().web_connect("acc", Clock::now()).unwrap();
    // Nobody polls: the bounded wait ends and the state says why; it is still the web source.
    let inventory = fixture.service().web_inventory("acc", true, Duration::from_millis(50), Clock::now()).unwrap().unwrap();
    assert_eq!((inventory.source.as_str(), inventory.state.as_str()), ("web", "unknown"));
    assert_eq!(inventory.web.as_ref().unwrap().error_code.as_deref(), Some("WEB_BROWSER_UNAVAILABLE"));
    assert_eq!(fixture.pending(), 0);
    // A companion that answers inside the wait makes the same call return the fresh observation.
    let service = fixture.service().clone();
    let companion = std::thread::spawn(move || {
        let client = new_id();
        for _ in 0..200 {
            let polled = service.web_poll(&client, Clock::now()).unwrap();
            if let Some(job) = polled.jobs.first() {
                return service.web_complete(CompleteRequest { client_id: client, request_id: job.request_id.clone(),
                    connection_id: job.connection_id.clone(), result: success(3) }, Clock::now()).unwrap();
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    });
    let inventory = fixture.service().web_inventory("acc", true, Duration::from_secs(10), Clock::now()).unwrap().unwrap();
    assert!(companion.join().unwrap());
    assert_eq!((inventory.state.as_str(), inventory.available_count), ("known", Some(3)));
    assert_eq!(inventory.web.unwrap().state, "ready");
}

#[test]
fn restart_keeps_the_observation_but_never_a_pending_or_claimed_job() {
    let mut fixture = Fixture::new();
    fixture.standard("acc");
    fixture.service().web_connect("acc", fixture.at(0)).unwrap();
    let (client, jobs) = fixture.claim(10);
    assert!(fixture.complete(&client, &jobs[0], success(1), 20));
    fixture.service().web_schedule(fixture.at(300_000), false).unwrap();
    let (client, claimed) = fixture.claim(300_010);
    assert_eq!(claimed.len(), 1);
    let connection_id = fixture.stored("acc").unwrap().connection_id;
    fixture.restart();
    assert_eq!(fixture.pending(), 0);
    assert!(!fixture.complete(&client, &claimed[0], success(9), 300_020), "a job from before the restart is not authority");
    let inventory = fixture.inventory("acc", 300_030);
    assert_eq!(inventory.credits.len(), 1);
    assert_eq!(inventory.credits[0].remaining_count, 1);
    assert_eq!(fixture.stored("acc").unwrap().connection_id, connection_id, "the profile identity persists");
    // Still due: the schedule survives and a fresh job is issued.
    let (_, jobs) = fixture.claim(311_000);
    assert_eq!(jobs.len(), 1);
    assert_ne!(jobs[0].request_id, claimed[0].request_id);
}

#[test]
fn polling_validates_the_client_and_lists_only_live_connections() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    assert_eq!(fixture.service().web_poll("not-a-uuid", fixture.at(0)).err().unwrap().code, "INVALID_PARAMS");
    assert!(fixture.service().web_poll(&new_id(), fixture.at(0)).unwrap().connections.is_empty());
    fixture.service().web_connect("acc", fixture.at(1)).unwrap();
    let polled = fixture.service().web_poll(&new_id(), fixture.at(2)).unwrap();
    assert_eq!((polled.connections.len(), polled.jobs.len()), (1, 1));
    assert!(fixture.service().web_complete(CompleteRequest { client_id: "x".into(), request_id: "y".into(), connection_id: "z".into(), result: json!({}) }, fixture.at(3)).unwrap() == false);
}

#[test]
fn a_web_connected_account_cannot_redeem_through_oauth() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    fixture.service().web_connect("acc", fixture.at(0)).unwrap();
    let error = fixture.service().dispatch("resetCredits.redeem", json!({"accountId":"acc","quoteId":new_id(),"creditId":"weekly-1","requestId":new_id(),"confirmed":true})).unwrap_err();
    assert_eq!(error.code, "RESET_WEB_READ_ONLY");
}

#[test]
fn rpc_dispatch_round_trips_the_contract_shapes() {
    let fixture = Fixture::new();
    fixture.standard("acc");
    let service = fixture.service();
    let connected = service.dispatch("resetWeb.connect", json!({"accountId":"acc"})).unwrap();
    assert_eq!(connected["status"]["connected"], true);
    assert_eq!(connected["connection"]["accountId"], "acc");
    let client = new_id();
    let polled = service.dispatch("resetWeb.poll", json!({"clientId":client})).unwrap();
    let job = polled["jobs"][0].clone();
    assert_eq!(job["connectionId"], connected["connection"]["connectionId"]);
    let done = service.dispatch("resetWeb.complete", json!({"clientId":client,"requestId":job["requestId"],"connectionId":job["connectionId"],"result":success(1)})).unwrap();
    assert_eq!(done["accepted"], true);
    let status = service.dispatch("resetWeb.status", json!({"accountId":"acc"})).unwrap();
    assert_eq!(status["state"], "ready");
    let read = service.dispatch("resetCredits.read", json!({"accountId":"acc","refresh":false})).unwrap();
    assert_eq!((read["source"].as_str(), read["quoteId"].is_null(), read["web"]["connected"].as_bool()), (Some("web"), true, Some(true)));
    assert_eq!(service.dispatch("resetWeb.request", json!({"accountId":"acc"})).unwrap()["connected"], true);
    let off = service.dispatch("resetWeb.disconnect", json!({"accountId":"acc"})).unwrap();
    assert_eq!((off["connected"].as_bool(), off["state"].as_str()), (Some(false), Some("disconnected")));
    assert_eq!(service.dispatch("resetWeb.status", json!({"accountId":"acc","extra":1})).unwrap_err().code, "INVALID_PARAMS");
}
