//! Account-bound Claude web reset reads.
//!
//! The service never touches the browser: the Ojak tray app polls `resetWeb.poll`, runs the read inside its isolated
//! claude.ai web profile and posts the sanitized result back with `resetWeb.complete`. This module owns what is
//! trusted: the persisted connection and last observation (`resetWeb:<accountId>` metadata), the in-memory job queue,
//! the schedule, and the identity check against the REGISTERED account. A web observation is read-only information:
//! `quoteId` is always null and nothing here can reach the OAuth redeem path.
use crate::{reset_credits::{downgrade, provider}, reset_provider::{self, claude_inventory}, store::*, Service};
use aam_protocol::{new_id, now_ms, ApiError, ResetCredit, ResetInventory, ResetWebConnection, ResetWebJob, ResetWebStatus};
use parking_lot::{Condvar, Mutex};
use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, sync::Arc, time::{Duration, Instant}};

/// Normal cadence, anchored at the start of an attempt whether it succeeded or failed.
const REFRESH_MS: i64 = 300_000;
/// An observation older than this is no longer current (same window as the existing reset cache).
const STALE_MS: i64 = 300_000;
/// Total life of a job, claimed or not.
const JOB_TTL_MS: i64 = 60_000;
/// A job nobody claimed by now means the tray app is not running.
const UNCLAIMED_MS: i64 = 30_000;
/// A page-load request closer than this to the previous attempt is ignored.
const MIN_REQUEST_GAP_MS: i64 = 10_000;
/// The tray app polls every ~2s; this long without a poll means it is gone.
const COMPANION_ALIVE_MS: i64 = 15_000;
const MAX_RESULT_BYTES: usize = 256 * 1024;
const MAX_CONNECTIONS: usize = 32;
const MAX_JOBS_PER_POLL: usize = 8;
/// How long an explicit read waits for the native read before reporting the current state.
pub(crate) const READ_WAIT: Duration = Duration::from_secs(20);
const ERRORS: [&str; 6] = ["WEB_LOGIN_REQUIRED", "WEB_FETCH_FAILED", "WEB_RESPONSE_INVALID", "WEB_ACCOUNT_MISMATCH", "WEB_BROWSER_UNAVAILABLE", "WEB_UNSUPPORTED_PLATFORM"];

/// A wall reading plus a monotonic one. A job expires when EITHER says its time is up, so neither a sleeping laptop
/// (monotonic stalls) nor a wall clock stepped backwards can keep an old job authoritative.
#[derive(Clone, Copy)]
pub(crate) struct Clock { pub wall: i64, pub mono: Instant }
impl Clock {
    pub(crate) fn now() -> Self { Self { wall: now_ms(), mono: Instant::now() } }
    fn since(&self, earlier: &Clock) -> i64 {
        let mono = self.mono.saturating_duration_since(earlier.mono).as_millis().min(i64::MAX as u128) as i64;
        (self.wall - earlier.wall).max(mono)
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Observation {
    observed_at: i64,
    /// known, restricted, unknown: what the official page said, as the provider parser classified it.
    state: String,
    reason: Option<String>,
    available: Option<u32>,
    redeemable: Option<u32>,
    credits: Vec<ResetCredit>,
}
/// Persisted per account. Holds no email, token or raw payload: the registered account already carries the identity.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Stored {
    account_id: String,
    /// Isolated web profile ID. A new one is minted whenever the connection is created fresh.
    connection_id: String,
    organization_id: String,
    binding: String,
    /// Last settled state: waiting_browser, login_required, ready, error. `syncing` is derived from the live queue.
    state: String,
    last_attempt_at: Option<i64>,
    last_success_at: Option<i64>,
    next_refresh_at: Option<i64>,
    error_code: Option<String>,
    /// The last attempt failed only because the tray app never claimed the job. A returning poll retries at once.
    #[serde(default)]
    absent: bool,
    observation: Option<Observation>,
}

struct Identity { org: String, email: Option<String>, subject: Option<String> }
enum Bound { Identity(Identity), Unmapped, Revoked }
struct Resolved { stored: Stored, identity: Option<Identity> }

struct Pending { request_id: String, connection_id: String, organization_id: String, queued: Clock, claim: Option<String>, taken: bool }
impl Pending {
    fn expires_at(&self) -> i64 { self.queued.wall + JOB_TTL_MS }
    fn expired(&self, c: &Clock) -> bool { c.since(&self.queued) >= JOB_TTL_MS }
    fn overdue(&self, c: &Clock) -> bool { self.claim.is_none() && !self.taken && c.since(&self.queued) >= UNCLAIMED_MS }
}
#[derive(Default)]
struct Inner { jobs: HashMap<String, Pending>, last_poll: Option<Instant> }
/// In-memory only: a job is never replayable authority across a restart.
#[derive(Default)]
pub(crate) struct Runtime { inner: Mutex<Inner>, wake: Condvar }
impl Runtime {
    pub(crate) fn drop_account(&self, account_id: &str) {
        if self.inner.lock().jobs.remove(account_id).is_some() { self.wake.notify_all(); }
    }
    /// (a job is pending, the tray app polled recently)
    fn view(&self, account_id: &str, connection_id: &str, c: &Clock) -> (bool, bool) {
        let inner = self.inner.lock();
        let pending = inner.jobs.get(account_id).is_some_and(|p| p.connection_id == connection_id && !p.expired(c));
        let alive = inner.last_poll.is_some_and(|at| c.mono.saturating_duration_since(at).as_millis() < COMPANION_ALIVE_MS as u128);
        (pending, alive)
    }
    fn live_request(&self, account_id: &str, connection_id: &str, c: &Clock) -> Option<String> {
        self.inner.lock().jobs.get(account_id)
            .filter(|p| p.connection_id == connection_id && !p.expired(c) && !p.overdue(c)).map(|p| p.request_id.clone())
    }
    /// The one place a job is created, so an account never has two outstanding jobs.
    fn enqueue(&self, account_id: &str, connection_id: &str, organization_id: &str, c: Clock) -> String {
        let mut inner = self.inner.lock();
        if let Some(p) = inner.jobs.get(account_id) {
            if p.connection_id == connection_id && !p.expired(&c) && !p.overdue(&c) { return p.request_id.clone(); }
        }
        let request_id = new_id();
        inner.jobs.insert(account_id.into(), Pending { request_id: request_id.clone(), connection_id: connection_id.into(),
            organization_id: organization_id.into(), queued: c, claim: None, taken: false });
        request_id
    }
    /// Marks the exact outstanding claimed job as being answered. It stays queued (so a reader keeps waiting for the
    /// durable result) but can never be answered twice.
    fn take_claimed(&self, request: &CompleteRequest, c: &Clock) -> Option<(String, String, String)> {
        let mut inner = self.inner.lock();
        let key = inner.jobs.iter().find(|(_, p)| p.request_id == request.request_id).map(|(k, _)| k.clone())?;
        let p = inner.jobs.get_mut(&key)?;
        if p.taken || p.connection_id != request.connection_id || p.claim.as_deref() != Some(request.client_id.as_str()) || p.expired(c) { return None; }
        p.taken = true;
        Some((key, p.connection_id.clone(), p.organization_id.clone()))
    }
    fn finish(&self, account_id: &str, request_id: &str) {
        let mut inner = self.inner.lock();
        if inner.jobs.get(account_id).is_some_and(|p| p.request_id == request_id) { inner.jobs.remove(account_id); }
        self.wake.notify_all();
    }
    fn wait_done(&self, account_id: &str, request_id: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut inner = self.inner.lock();
        loop {
            if inner.jobs.get(account_id).map(|p| p.request_id.as_str()) != Some(request_id) { return true; }
            if self.wake.wait_until(&mut inner, deadline).timed_out() {
                return inner.jobs.get(account_id).map(|p| p.request_id.as_str()) != Some(request_id);
            }
        }
    }
}

fn key(account_id: &str) -> String { format!("resetWeb:{account_id}") }
fn read(connection: &Connection, account_id: &str) -> Result<Option<Stored>, ApiError> { metadata(connection, &key(account_id)) }
fn write(connection: &Connection, stored: &Stored) -> Result<(), ApiError> { set_metadata(connection, &key(&stored.account_id), stored) }
fn list(connection: &Connection) -> Result<Vec<Stored>, ApiError> {
    let mut statement = connection.prepare("SELECT body FROM metadata WHERE substr(key,1,9)='resetWeb:'").map_err(db_error)?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0)).map_err(db_error)?;
    let mut all = Vec::new();
    for body in rows { if let Ok(stored) = decode::<Stored>(&body.map_err(db_error)?) { all.push(stored); } }
    Ok(all)
}
/// Removing an account removes its web connection and observation with it.
pub(crate) fn forget(connection: &Connection, account_id: &str) -> Result<(), ApiError> {
    connection.execute("DELETE FROM metadata WHERE key=?1", [key(account_id)]).map_err(db_error)?;
    Ok(())
}

/// True while the account has a web connection (even one that cannot currently be read): such an account never spends through OAuth.
pub(crate) fn is_connected(connection: &Connection, account_id: &str) -> Result<bool, ApiError> {
    Ok(read(connection, account_id)?.is_some())
}
pub(crate) fn disconnected(account_id: &str) -> ResetWebStatus {
    ResetWebStatus { account_id: account_id.into(), connected: false, state: "disconnected".into(),
        last_attempt_at: None, last_success_at: None, next_refresh_at: None, error_code: None }
}
fn check_id(id: &str) -> Result<(), ApiError> {
    if reset_provider::token(id, 128) { Ok(()) } else { Err(ApiError::new("INVALID_PARAMS", "계정 ID가 올바르지 않아요.")) }
}
fn unmapped() -> ApiError {
    ApiError::new("RESET_WEB_IDENTITY_UNMAPPED", "이 계정의 이메일·워크스페이스를 확인할 수 없어 웹 연결을 만들 수 없어요.")
}
#[cfg(not(test))]
fn env_guard() -> Result<(), ApiError> { aam_adapters::check_subscription_auth_env("claude") }
/// The real guard reads the developer's environment and `~/.claude`; unit tests must not depend on either.
#[cfg(test)]
fn env_guard() -> Result<(), ApiError> { Ok(()) }

fn norm(value: &str) -> String {
    let value = value.trim();
    uuid::Uuid::parse_str(value).map(|id| id.to_string()).unwrap_or_else(|_| value.to_lowercase())
}
/// The identity the REGISTERED account proves: the exact workspace plus an email and/or subject UUID. Anything else is
/// unmapped; there is no alias or fuzzy mapping.
fn web_identity(account: &aam_protocol::Account) -> Result<Identity, ApiError> {
    let identity = account.identity_key.as_deref().ok_or_else(unmapped)?;
    let part = |name: &str| identity.split('|').skip(1).find_map(|p| p.strip_prefix(name)).filter(|v| !v.is_empty());
    let org = part("workspace:").filter(|v| reset_provider::token(v, 128)).ok_or_else(unmapped)?.to_owned();
    let email = part("email:").map(str::to_owned).or_else(|| account.email.clone()).map(|e| e.trim().to_lowercase())
        .filter(|e| !e.is_empty() && e.len() <= 512 && !e.chars().any(char::is_control));
    // The email is the stronger, directly comparable proof; a subject UUID is only used when no email is known.
    let subject = if email.is_some() { None } else { part("subject:").map(norm) };
    if email.is_none() && subject.is_none() { return Err(unmapped()); }
    Ok(Identity { org, email, subject })
}
/// A web connection follows the account's IDENTITY, not its allocation settings: enabling, disabling or re-verifying the
/// account must not drop a login the user made on purpose.
fn web_binding(account: &aam_protocol::Account) -> Result<String, ApiError> { fingerprint(&(&account.tool, &account.identity_key)) }
fn bound(connection: &Connection, stored: &Stored) -> Result<Bound, ApiError> {
    let account = match crate::store::account(connection, &stored.account_id) {
        Ok(account) => account,
        Err(error) if error.code == "ACCOUNT_NOT_FOUND" => return Ok(Bound::Revoked),
        Err(error) => return Err(error),
    };
    if provider(&account) != Some("anthropic") { return Ok(Bound::Revoked); }
    // A momentarily unverified account keeps its connection; it just cannot be read or validated until it is identified again.
    if account.identity_key.is_none() { return Ok(Bound::Unmapped); }
    if web_binding(&account)? != stored.binding { return Ok(Bound::Revoked); }
    Ok(match web_identity(&account) { Ok(identity) => Bound::Identity(identity), Err(_) => Bound::Unmapped })
}

/// What the page claimed, after shape validation.
struct Claim { email: String, uuid: String, org: String, member: bool }
enum Parsed { Fail(String), Seen(Claim, Observation) }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Failure { error_code: String }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Success { email: String, account_uuid: String, organization_id: String, member: bool, values: Vec<Value> }
fn invalid() -> Parsed { Parsed::Fail("WEB_RESPONSE_INVALID".into()) }
fn parse_result(raw: &Value, wall: i64) -> Parsed {
    if serde_json::to_vec(raw).map_or(true, |bytes| bytes.len() > MAX_RESULT_BYTES) { return invalid(); }
    if let Ok(failure) = Failure::deserialize(raw) {
        return if ERRORS.contains(&failure.error_code.as_str()) { Parsed::Fail(failure.error_code) } else { invalid() };
    }
    let Ok(success) = Success::deserialize(raw) else { return invalid(); };
    let Success { email, account_uuid, organization_id, member, values } = success;
    if email.is_empty() || email.len() > 512 || email.chars().any(char::is_control)
        || uuid::Uuid::parse_str(&account_uuid).is_err() || !reset_provider::token(&organization_id, 128)
        || values.len() != 2 || !values.iter().all(Value::is_object) { return invalid(); }
    let Ok(inventory) = claude_inventory(&[&values[0], &values[1]], wall) else { return invalid(); };
    // The page never named a program: a missing answer must not replace offers already seen.
    if inventory.state == "unknown" && inventory.credits.is_empty() { return invalid(); }
    Parsed::Seen(Claim { email, uuid: account_uuid, org: organization_id, member }, Observation {
        observed_at: wall, state: inventory.state.into(), reason: inventory.reason.map(Into::into),
        available: inventory.available, redeemable: inventory.redeemable, credits: inventory.credits })
}
fn mismatch() -> String { "WEB_ACCOUNT_MISMATCH".into() }
/// The web session must be this registered account in this exact workspace, and the job's own workspace.
fn verify(identity: &Identity, job_org: &str, claim: &Claim) -> Result<(), String> {
    let org = norm(&claim.org);
    if !claim.member || org != norm(&identity.org) || org != norm(job_org) { return Err(mismatch()); }
    if identity.email.as_ref().is_some_and(|email| claim.email.trim().to_lowercase() != *email) { return Err(mismatch()); }
    if identity.subject.as_ref().is_some_and(|subject| norm(&claim.uuid) != *subject) { return Err(mismatch()); }
    Ok(())
}
fn attempt_base(stored: &Stored, wall: i64) -> i64 { stored.last_attempt_at.filter(|t| *t <= wall).unwrap_or(wall) }
/// How a failure is anchored: a failed READ keeps the start of its attempt; a failure found without any read
/// (the account cannot be identified, an auth override is set) starts a new interval now so it is not re-recorded on every poll.
#[derive(Clone, Copy, PartialEq)]
enum Anchor { Attempt, Now }
fn apply_success(stored: &mut Stored, observation: Observation, wall: i64) {
    stored.state = "ready".into();
    stored.error_code = None;
    stored.absent = false;
    stored.last_success_at = Some(wall);
    stored.next_refresh_at = Some(attempt_base(stored, wall) + REFRESH_MS);
    stored.observation = Some(observation);
}
/// A failure never touches the observation or `last_success_at`.
fn apply_failure(stored: &mut Stored, code: &str, absent: bool, anchor: Anchor, wall: i64) {
    if anchor == Anchor::Now { stored.last_attempt_at = Some(wall); }
    stored.state = match code { "WEB_LOGIN_REQUIRED" => "login_required", "WEB_BROWSER_UNAVAILABLE" => "waiting_browser", _ => "error" }.into();
    stored.error_code = Some(code.into());
    stored.absent = absent;
    stored.last_attempt_at = Some(attempt_base(stored, wall));
    stored.next_refresh_at = Some(attempt_base(stored, wall) + REFRESH_MS);
}
fn due(stored: &Stored, c: &Clock, poll: bool) -> bool {
    // A next attempt further away than one full interval can only come from a wall clock that stepped backwards:
    // it must not postpone the refresh, so it is due now and the next interval restarts from this attempt.
    let due = stored.next_refresh_at.is_none_or(|t| c.wall >= t || t > c.wall + REFRESH_MS);
    due || (poll && stored.absent)
}
/// The inventory a connected account shows. Offers are always the last ones seen; whether they are still a current
/// fact is decided here and never by the page: a failed last attempt or an observation older than five minutes
/// withdraws the totals and every spendable flag at once while keeping the offers and their timestamps.
fn build(stored: &Stored, status: ResetWebStatus, wall: i64) -> ResetInventory {
    let mut inventory = match &stored.observation {
        Some(o) => ResetInventory { account_id: stored.account_id.clone(), state: o.state.clone(), available_count: o.available,
            redeemable_count: o.redeemable, credits: o.credits.clone(), observed_at: o.observed_at, quote_id: None, operation: None,
            lookup_reason: o.reason.clone(), source: "web".into(), web: None },
        None => ResetInventory { account_id: stored.account_id.clone(), state: "unknown".into(), available_count: None, redeemable_count: None,
            credits: vec![], observed_at: 0, quote_id: None, operation: None,
            lookup_reason: Some(stored.error_code.as_ref().map_or("no_observation".into(), |c| c.to_lowercase())), source: "web".into(), web: None },
    };
    if let Some(o) = &stored.observation {
        if let Some(code) = &stored.error_code {
            inventory.lookup_reason = None;
            downgrade(&mut inventory, "unknown", &code.to_lowercase());
        } else if wall < o.observed_at || wall - o.observed_at > STALE_MS {
            inventory.lookup_reason = None;
            downgrade(&mut inventory, "unknown", "stale");
        }
    }
    inventory.quote_id = None;
    inventory.web = Some(status);
    inventory
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AccountRequest { account_id: String }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PollRequest { client_id: String }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CompleteRequest { client_id: String, request_id: String, connection_id: String, result: Value }
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Connected { pub connection: ResetWebConnection, pub status: ResetWebStatus }
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Polled { pub connections: Vec<ResetWebConnection>, pub jobs: Vec<ResetWebJob> }

fn connection_of(stored: &Stored) -> ResetWebConnection {
    ResetWebConnection { account_id: stored.account_id.clone(), connection_id: stored.connection_id.clone(), organization_id: stored.organization_id.clone() }
}

impl Service {
    pub(crate) fn reset_web_rpc(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, ApiError> {
        let c = Clock::now();
        match method {
            "resetWeb.connect" => crate::value(self.web_connect(&crate::parse::<AccountRequest>(params)?.account_id, c)?),
            "resetWeb.disconnect" => crate::value(self.web_disconnect(&crate::parse::<AccountRequest>(params)?.account_id)?),
            "resetWeb.status" => crate::value(self.web_status(&crate::parse::<AccountRequest>(params)?.account_id, c)?),
            "resetWeb.request" => crate::value(self.web_request(&crate::parse::<AccountRequest>(params)?.account_id, c)?),
            "resetWeb.poll" => crate::value(self.web_poll(&crate::parse::<PollRequest>(params)?.client_id, c)?),
            "resetWeb.complete" => crate::value(json!({"accepted": self.web_complete(crate::parse::<CompleteRequest>(params)?, c)?})),
            _ => Err(ApiError::new("INVALID_REQUEST", "알 수 없는 웹 초기화권 요청이에요.")),
        }
    }

    /// The connection a registered account currently has, or None. A binding that no longer matches is revoked here.
    fn web_resolve(&self, account_id: &str) -> Result<Option<Resolved>, ApiError> {
        {
            let store = self.lock()?;
            let Some(stored) = read(&store.connection, account_id)? else { return Ok(None); };
            match bound(&store.connection, &stored)? {
                Bound::Identity(identity) => return Ok(Some(Resolved { stored, identity: Some(identity) })),
                Bound::Unmapped => return Ok(Some(Resolved { stored, identity: None })),
                Bound::Revoked => forget(&store.connection, account_id)?,
            }
        }
        self.reset_web.drop_account(account_id);
        Ok(None)
    }
    fn web_update(&self, account_id: &str, connection_id: &str, change: impl FnOnce(&mut Stored)) -> Result<bool, ApiError> {
        let mut store = self.lock()?;
        let tx = store.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let Some(mut stored) = read(&tx, account_id)? else { return Ok(false); };
        if stored.connection_id != connection_id { return Ok(false); }
        change(&mut stored);
        write(&tx, &stored)?;
        tx.commit().map_err(db_error)?;
        Ok(true)
    }
    fn web_fail(&self, account_id: &str, connection_id: &str, code: &str, absent: bool, anchor: Anchor, c: Clock) -> Result<bool, ApiError> {
        self.web_update(account_id, connection_id, |stored| apply_failure(stored, code, absent, anchor, c.wall))
    }
    fn web_status_of(&self, stored: &Stored, c: Clock) -> ResetWebStatus {
        let (pending, alive) = self.reset_web.view(&stored.account_id, &stored.connection_id, &c);
        let state = if pending { if alive { "syncing" } else { "waiting_browser" } } else { stored.state.as_str() };
        ResetWebStatus { account_id: stored.account_id.clone(), connected: true, state: state.into(),
            last_attempt_at: stored.last_attempt_at, last_success_at: stored.last_success_at,
            next_refresh_at: stored.next_refresh_at.map(|t| t.min(c.wall + REFRESH_MS)), error_code: stored.error_code.clone() }
    }

    pub(crate) fn web_connect(&self, account_id: &str, c: Clock) -> Result<Connected, ApiError> {
        check_id(account_id)?;
        let replaced;
        let connection = {
            let mut store = self.lock()?;
            let tx = store.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
            let acct = crate::store::account(&tx, account_id)?;
            if provider(&acct) != Some("anthropic") {
                return Err(ApiError::new("RESET_UNSUPPORTED", "웹 초기화권은 Claude 계정만 연결할 수 있어요."));
            }
            let identity = web_identity(&acct)?;
            env_guard()?;
            let binding = web_binding(&acct)?;
            let previous = read(&tx, account_id)?;
            let reuse = previous.as_ref().is_some_and(|s| s.binding == binding && norm(&s.organization_id) == norm(&identity.org));
            let mut stored = match previous {
                Some(stored) if reuse => stored,
                _ => Stored { account_id: account_id.into(), connection_id: new_id(), organization_id: identity.org.clone(), binding,
                    state: "waiting_browser".into(), last_attempt_at: None, last_success_at: None, next_refresh_at: None,
                    error_code: None, absent: false, observation: None },
            };
            replaced = !reuse;
            stored.state = "waiting_browser".into();
            stored.error_code = None;
            stored.absent = false;
            stored.next_refresh_at = None;
            write(&tx, &stored)?;
            // A web connection is read-only: no earlier OAuth confirmation may outlive it.
            crate::reset_credits::forget_quote(&tx, account_id)?;
            tx.commit().map_err(db_error)?;
            connection_of(&stored)
        };
        if replaced { self.reset_web.drop_account(account_id); }
        self.web_enqueue(account_id, true, c)?;
        Ok(Connected { connection, status: self.web_status(account_id, c)? })
    }
    /// Explicit disconnect: drops the connection, any pending job and the web observation. Only after this does a
    /// Claude account read through OAuth again.
    pub(crate) fn web_disconnect(&self, account_id: &str) -> Result<ResetWebStatus, ApiError> {
        check_id(account_id)?;
        forget(&self.lock()?.connection, account_id)?;
        self.reset_web.drop_account(account_id);
        Ok(disconnected(account_id))
    }
    pub(crate) fn web_status(&self, account_id: &str, c: Clock) -> Result<ResetWebStatus, ApiError> {
        check_id(account_id)?;
        let existence = crate::store::account(&self.lock()?.connection, account_id);
        if let Err(error) = existence {
            if error.code == "ACCOUNT_NOT_FOUND" { self.web_disconnect(account_id)?; }
            return Err(error);
        }
        Ok(match self.web_resolve(account_id)? {
            Some(resolved) => self.web_status_of(&resolved.stored, c),
            None => disconnected(account_id),
        })
    }
    /// Enqueue only (the tray app calls this after a real page load); an outstanding job is reused.
    pub(crate) fn web_request(&self, account_id: &str, c: Clock) -> Result<ResetWebStatus, ApiError> {
        check_id(account_id)?;
        self.web_enqueue(account_id, false, c)?;
        self.web_status(account_id, c)
    }

    /// Queue a read for a connected account, or reuse the one already outstanding. `force` skips the page-load spacing.
    pub(crate) fn web_enqueue(&self, account_id: &str, force: bool, c: Clock) -> Result<Option<String>, ApiError> {
        self.web_sweep(c)?;
        let Some(resolved) = self.web_resolve(account_id)? else { return Ok(None); };
        let stored = resolved.stored;
        if resolved.identity.is_none() {
            self.web_fail(account_id, &stored.connection_id, "WEB_ACCOUNT_MISMATCH", false, Anchor::Now, c)?;
            return Ok(None);
        }
        if env_guard().is_err() {
            self.web_fail(account_id, &stored.connection_id, "AUTH_OVERRIDE_CONFLICT", false, Anchor::Now, c)?;
            return Ok(None);
        }
        if let Some(request_id) = self.reset_web.live_request(account_id, &stored.connection_id, &c) { return Ok(Some(request_id)); }
        if !force && stored.last_attempt_at.is_some_and(|t| t <= c.wall && c.wall - t < MIN_REQUEST_GAP_MS) { return Ok(None); }
        let request_id = self.reset_web.enqueue(account_id, &stored.connection_id, &stored.organization_id, c);
        let recorded = self.web_update(account_id, &stored.connection_id, |s| { s.last_attempt_at = Some(c.wall); s.absent = false; })?;
        if !recorded {
            self.reset_web.drop_account(account_id);
            return Ok(None);
        }
        Ok(Some(request_id))
    }
    /// Retire jobs that ran out of time and record why, preserving every observation.
    pub(crate) fn web_sweep(&self, c: Clock) -> Result<(), ApiError> {
        let gone: Vec<(String, String, bool)> = {
            let mut inner = self.reset_web.inner.lock();
            let ids: Vec<String> = inner.jobs.iter().filter(|(_, p)| (p.expired(&c) && !p.taken) || p.overdue(&c)).map(|(k, _)| k.clone()).collect();
            let removed: Vec<(String, String, bool)> = ids.into_iter()
                .filter_map(|id| inner.jobs.remove(&id).map(|p| (id, p.connection_id, p.claim.is_some()))).collect();
            removed
        };
        let mut first = None;
        for (account_id, connection_id, claimed) in &gone {
            // Claimed but never answered is a failed read; never claimed means the tray app is not running.
            let (code, absent) = if *claimed { ("WEB_FETCH_FAILED", false) } else { ("WEB_BROWSER_UNAVAILABLE", true) };
            if let Err(error) = self.web_fail(account_id, connection_id, code, absent, Anchor::Attempt, c) { first.get_or_insert(error); }
        }
        if !gone.is_empty() { self.reset_web.wake.notify_all(); }
        first.map_or(Ok(()), Err)
    }
    /// Due refreshes are queued here; it returns the connections that are currently live and identifiable.
    fn web_schedule(&self, c: Clock, poll: bool) -> Result<Vec<Stored>, ApiError> {
        self.web_sweep(c)?;
        let all = list(&self.lock()?.connection)?;
        let mut live = Vec::new();
        for stored in all {
            let Some(resolved) = self.web_resolve(&stored.account_id)? else { continue; };
            if due(&resolved.stored, &c, poll) { let _ = self.web_enqueue(&stored.account_id, false, c); }
            live.push(resolved.stored);
            if live.len() >= MAX_CONNECTIONS { break; }
        }
        Ok(live)
    }
    /// Called by the service's background loop.
    pub(crate) fn reset_web_tick(&self) -> Result<(), ApiError> {
        self.web_schedule(Clock::now(), false).map(|_| ())
    }
    pub(crate) fn web_poll(&self, client_id: &str, c: Clock) -> Result<Polled, ApiError> {
        if uuid::Uuid::parse_str(client_id).is_err() { return Err(ApiError::new("INVALID_PARAMS", "클라이언트 ID가 올바르지 않아요.")); }
        self.reset_web.inner.lock().last_poll = Some(c.mono);
        let live = self.web_schedule(c, true)?;
        let mut jobs = Vec::new();
        {
            // Claim is atomic: a job leaves the queue state `unclaimed` exactly once.
            let mut inner = self.reset_web.inner.lock();
            for stored in &live {
                if jobs.len() >= MAX_JOBS_PER_POLL { break; }
                let Some(p) = inner.jobs.get_mut(&stored.account_id) else { continue; };
                if p.connection_id != stored.connection_id || p.claim.is_some() || p.expired(&c) || p.overdue(&c) { continue; }
                p.claim = Some(client_id.into());
                jobs.push(ResetWebJob { account_id: stored.account_id.clone(), connection_id: p.connection_id.clone(),
                    organization_id: p.organization_id.clone(), request_id: p.request_id.clone(), expires_at: p.expires_at() });
            }
        }
        Ok(Polled { connections: live.iter().map(connection_of).collect(), jobs })
    }

    /// `true` means the result was recorded, as a success or as a failure. A job that is not exactly the outstanding,
    /// claimed one for a still-live binding is rejected without changing anything.
    pub(crate) fn web_complete(&self, request: CompleteRequest, c: Clock) -> Result<bool, ApiError> {
        if [&request.client_id, &request.request_id, &request.connection_id].iter().any(|id| uuid::Uuid::parse_str(id).is_err()) { return Ok(false); }
        let Some((account_id, connection_id, organization_id)) = self.reset_web.take_claimed(&request, &c) else { return Ok(false); };
        let outcome = self.web_apply(&account_id, &connection_id, &organization_id, &request.result, c);
        // Waiting readers wake only after the result is durable (or the attempt is known to have failed to record).
        self.reset_web.finish(&account_id, &request.request_id);
        outcome
    }
    fn web_apply(&self, account_id: &str, connection_id: &str, organization_id: &str, raw: &Value, c: Clock) -> Result<bool, ApiError> {
        let parsed = parse_result(raw, c.wall);
        let mut store = self.lock()?;
        let tx = store.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let Some(mut stored) = read(&tx, account_id)? else { return Ok(false); };
        if stored.connection_id != connection_id || norm(&stored.organization_id) != norm(organization_id) { return Ok(false); }
        let binding = bound(&tx, &stored)?;
        if matches!(binding, Bound::Revoked) {
            forget(&tx, account_id)?;
            tx.commit().map_err(db_error)?;
            return Ok(false);
        }
        let result = match (parsed, binding) {
            (Parsed::Fail(code), _) => Err(code),
            (Parsed::Seen(claim, observation), Bound::Identity(identity)) => verify(&identity, organization_id, &claim).map(|_| observation),
            (Parsed::Seen(..), _) => Err(mismatch()),
        };
        match result {
            Ok(observation) => apply_success(&mut stored, observation, c.wall),
            Err(code) => apply_failure(&mut stored, &code, false, Anchor::Attempt, c.wall),
        }
        write(&tx, &stored)?;
        tx.commit().map_err(db_error)?;
        Ok(true)
    }

    /// A reader gave up waiting for a job nobody claimed: the tray app is not running.
    fn web_abandon(&self, account_id: &str, request_id: &str, c: Clock) -> Result<(), ApiError> {
        let connection_id = {
            let mut inner = self.reset_web.inner.lock();
            if !inner.jobs.get(account_id).is_some_and(|p| p.request_id == request_id && p.claim.is_none()) { return Ok(()); }
            inner.jobs.remove(account_id).map(|p| p.connection_id)
        };
        let result = match connection_id {
            Some(connection_id) => self.web_fail(account_id, &connection_id, "WEB_BROWSER_UNAVAILABLE", true, Anchor::Attempt, c).map(|_| ()),
            None => Ok(()),
        };
        self.reset_web.wake.notify_all();
        result
    }
    /// The inventory for a web-connected account, or None when the account is not connected (the caller then reads OAuth).
    /// `refresh` queues a read and waits at most `wait` for it; a failed or absent read is reported, never replaced by OAuth.
    pub(crate) fn web_inventory(&self, account_id: &str, refresh: bool, wait: Duration, c: Clock) -> Result<Option<ResetInventory>, ApiError> {
        if self.web_resolve(account_id)?.is_none() { return Ok(None); }
        let c = if refresh {
            if let Some(request_id) = self.web_enqueue(account_id, true, c)? {
                if !self.reset_web.wait_done(account_id, &request_id, wait) { self.web_abandon(account_id, &request_id, Clock::now())?; }
            }
            Clock::now()
        } else { c };
        let Some(resolved) = self.web_resolve(account_id)? else { return Ok(None); };
        Ok(Some(build(&resolved.stored, self.web_status_of(&resolved.stored, c), c.wall)))
    }
}

#[cfg(test)]
#[path = "reset_web_tests.rs"]
mod tests;
