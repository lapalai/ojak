use crate::{reset_provider::{self, Auth, Inventory, Provider}, store::*, Service};
use aam_protocol::{now_ms, new_id, Account, ApiError, ResetCredit, ResetInventory, ResetOperation};
use rusqlite::{Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ReadRequest { account_id: String, #[serde(default)] refresh: bool }
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RedeemRequest {
    account_id: String, quote_id: String, credit_id: String, request_id: String, confirmed: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Quote { owner: String, binding: String, after_request: Option<String>, inventory: ResetInventory }
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record { owner: String, binding: String, service_instance_id: String, credit: ResetCredit, operation: ResetOperation }

fn quote_key(id: &str) -> String { format!("resetQuote:{id}") }
fn request_key(id: &str) -> String { format!("resetRequest:{id}") }
fn gate_key(owner: &str) -> String { format!("resetGate:{owner}") }
fn blocked(state: &str) -> bool { matches!(state, "preparing" | "submitted" | "unknown") }
fn changed() -> ApiError { ApiError::new("RESET_OFFER_CHANGED", "계정 또는 초기화권 조건이 바뀌었어요. 새로 조회하고 다시 확인해 주세요.") }
fn provider(account: &Account) -> Option<&'static str> {
    match account.tool.as_str() {
        "claude" => Some("anthropic"), "codex" => Some("openai-codex"),
        "omp" => match account.identity_key.as_deref()?.split('|').next()? {
            "anthropic" => Some("anthropic"), "openai-codex" => Some("openai-codex"), _ => None,
        },
        _ => None,
    }
}
fn owner(auth: &Auth) -> Result<String, ApiError> {
    let scope = if auth.provider == "openai-codex" {
        auth.account_id.as_deref().filter(|s| reset_provider::token(s, 256)).ok_or_else(changed)?.to_owned()
    } else {
        let email = auth.email.as_deref().filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control)).ok_or_else(changed)?;
        let org = auth.org_id.as_deref().filter(|s| reset_provider::token(s, 128)).ok_or_else(changed)?;
        let org = uuid::Uuid::parse_str(org).map(|id| id.to_string()).unwrap_or_else(|_| org.to_owned());
        format!("{}\0{org}", email.trim().to_lowercase())
    };
    fingerprint(&(auth.provider, scope))
}
fn blank(account_id: &str, state: &str) -> ResetInventory {
    ResetInventory { account_id: account_id.into(), state: state.into(), available_count: None, redeemable_count: None,
        credits: vec![], observed_at: 0, quote_id: None, operation: None }
}
fn gate(connection: &Connection, key: &str) -> Result<Option<Record>, ApiError> {
    let id: Option<String> = metadata(connection, &gate_key(key))?;
    id.map(|id| metadata(connection, &request_key(&id))).transpose().map(Option::flatten)
}
fn save(connection: &Connection, record: &Record) -> Result<(), ApiError> {
    set_metadata(connection, &request_key(&record.operation.request_id), record)
}
/// A restart proves an unsubmitted preparation cannot still dispatch. Submitted is never replayed.
fn after_restart(record: &mut Record, instance_id: &str) -> bool {
    if record.service_instance_id == instance_id { return false; }
    match record.operation.state.as_str() {
        "preparing" => { record.operation.state = "declined".into(); record.operation.code = "preparation_interrupted".into(); }
        "submitted" => { record.operation.state = "unknown".into(); record.operation.code = "result_unknown".into(); }
        _ => return false,
    }
    record.operation.updated_at = now_ms();
    true
}
fn reconcile(record: &mut Record, inventory: &Inventory) -> bool {
    if record.operation.state != "unknown" { return false; }
    let Some(current) = inventory.credits.iter().find(|c| c.id == record.credit.id && c.program == record.credit.program) else { return false; };
    let settled = if record.credit.program == "juniper_tide" {
        // Juniper has no request id and reuses its credit id. A temporary eligibility
        // change cannot settle an ambiguous POST; require the previous period to end.
        record.credit.expires_at.is_some_and(|end| end <= now_ms() && current.expires_at.is_some_and(|next| next > end))
    } else {
        current.remaining_count == 0 || matches!(current.status.as_str(), "redeemed" | "expired")
    };
    if settled {
        // The offer is no longer spendable. Do not claim our request caused the change.
        record.operation.state = "resolved".into();
        record.operation.code = "offer_reconciled".into();
        record.operation.updated_at = now_ms();
        return true;
    }
    false
}

impl Service {
    fn reset_account(&self, id: &str) -> Result<Account, ApiError> {
        if !reset_provider::token(id, 128) { return Err(changed()); }
        account(&self.lock()?.connection, id)
    }
    fn reset_auth(&self, account: &Account) -> Result<(Auth, Provider), ApiError> {
        if !account.enabled || account.auth_status != "authenticated" || account.identity_key.is_none() {
            return Err(ApiError::new("AUTH_REQUIRED", "활성화된 구독 계정의 로그인을 확인해 주세요."));
        }
        let provider = provider(account).ok_or_else(|| ApiError::new("RESET_UNSUPPORTED", "이 공급자의 초기화권은 지원하지 않아요."))?;
        aam_adapters::check_subscription_auth_env(if provider == "anthropic" { "claude" } else { "codex" })?;
        let mut auth = self.bridge.get().ok_or_else(reset_provider::failure)?.reset_auth(account, provider)?;
        let client = Provider::new(provider)?;
        client.organization(&mut auth)?;
        Ok((auth, client))
    }
    pub(crate) fn reset_read(self: &Arc<Self>, request: ReadRequest) -> Result<ResetInventory, ApiError> {
        let account = self.reset_account(&request.account_id)?;
        if provider(&account).is_none() { return Ok(blank(&account.id, "unsupported")); }
        let binding = binding_fingerprint(&account)?;
        if !request.refresh {
            let store = self.lock()?;
            let Some(quote): Option<Quote> = metadata(&store.connection, &quote_key(&account.id))? else { return Ok(blank(&account.id, "unknown")); };
            if quote.binding != binding { return Ok(blank(&account.id, "unknown")); }
            let mut result = quote.inventory;
            if let Some(mut record) = gate(&store.connection, &quote.owner)? {
                if after_restart(&mut record, &self.reset_instance_id) { save(&store.connection, &record)?; }
                if quote.after_request.as_deref() != Some(&record.operation.request_id) {
                    result.quote_id = None;
                    result.state = "unknown".into(); result.available_count = None; result.redeemable_count = None;
                }
                result.operation = Some(record.operation);
            }
            if now_ms().saturating_sub(result.observed_at) > 120_000 || result.operation.as_ref().is_some_and(|o| blocked(&o.state)) {
                result.quote_id = None;
            }
            if now_ms().saturating_sub(result.observed_at) > 300_000 {
                result.state = "unknown".into(); result.available_count = None; result.redeemable_count = None;
            }
            return Ok(result);
        }
        let (auth, client) = self.reset_auth(&account)?;
        let inventory = client.list(&auth)?;
        let owner = owner(&auth)?;
        let mut store = self.lock()?;
        let tx = store.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        if binding_fingerprint(&crate::store::account(&tx, &account.id)?)? != binding { return Err(changed()); }
        let mut operation = None;
        if let Some(mut record) = gate(&tx, &owner)? {
            let restarted = after_restart(&mut record, &self.reset_instance_id);
            if reconcile(&mut record, &inventory) || restarted { save(&tx, &record)?; }
            operation = Some(record.operation);
        }
        let result = ResetInventory { account_id: account.id.clone(), state: "known".into(), available_count: Some(inventory.available),
            redeemable_count: Some(inventory.redeemable), credits: inventory.credits, observed_at: now_ms(),
            quote_id: if operation.as_ref().is_some_and(|o| blocked(&o.state)) { None } else { Some(new_id()) }, operation };
        set_metadata(&tx, &quote_key(&account.id), &Quote { owner, binding,
            after_request: result.operation.as_ref().map(|op| op.request_id.clone()), inventory: result.clone() })?;
        tx.commit().map_err(db_error)?;
        Ok(result)
    }

    /// Claim durably before auth/network I/O. All desktop/CLI clients and duplicate account bindings share this gate.
    fn reset_begin(&self, request: &RedeemRequest) -> Result<(Record, bool), ApiError> {
        if !request.confirmed || uuid::Uuid::parse_str(&request.request_id).is_err() || uuid::Uuid::parse_str(&request.quote_id).is_err()
            || !reset_provider::token(&request.account_id, 128) || !reset_provider::token(&request.credit_id, 256) { return Err(changed()); }
        let mut store = self.lock()?;
        let tx = store.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let account = account(&tx, &request.account_id)?;
        let binding = binding_fingerprint(&account)?;
        if let Some(mut prior) = metadata::<Record>(&tx, &request_key(&request.request_id))? {
            if prior.operation.account_id != request.account_id || prior.operation.credit_id != request.credit_id || prior.binding != binding { return Err(changed()); }
            if after_restart(&mut prior, &self.reset_instance_id) { save(&tx, &prior)?; }
            tx.commit().map_err(db_error)?;
            return Ok((prior, false));
        }
        let mut quote: Quote = metadata(&tx, &quote_key(&account.id))?.ok_or_else(changed)?;
        if quote.binding != binding || quote.inventory.quote_id.as_deref() != Some(&request.quote_id)
            || quote.inventory.observed_at > now_ms() || now_ms().saturating_sub(quote.inventory.observed_at) > 120_000 { return Err(changed()); }
        if let Some(mut active) = gate(&tx, &quote.owner)? {
            if after_restart(&mut active, &self.reset_instance_id) { save(&tx, &active)?; }
            if blocked(&active.operation.state) { return Err(ApiError::new("RESET_IN_PROGRESS", "이 계정의 초기화권 사용 결과를 아직 확인하지 못했어요. 재사용하지 않고 결과를 먼저 조회해 주세요.")); }
            if quote.after_request.as_deref() != Some(&active.operation.request_id) { return Err(changed()); }
        }
        let credit = quote.inventory.credits.iter().find(|c| c.id == request.credit_id && c.usable && c.remaining_count > 0)
            .filter(|c| c.expires_at.is_none_or(|t| t > now_ms())).ok_or_else(changed)?.clone();
        let now = now_ms();
        let record = Record { owner: quote.owner.clone(), binding, service_instance_id: self.reset_instance_id.clone(), credit,
            operation: ResetOperation { request_id: request.request_id.clone(), account_id: request.account_id.clone(), credit_id: request.credit_id.clone(),
                state: "preparing".into(), code: "preparing".into(), started_at: now, updated_at: now } };
        save(&tx, &record)?;
        set_metadata(&tx, &gate_key(&record.owner), &request.request_id)?;
        // A confirmation is single-use, including when preparation is declined.
        quote.inventory.quote_id = None;
        set_metadata(&tx, &quote_key(&account.id), &quote)?;
        tx.commit().map_err(db_error)?;
        Ok((record, true))
    }
    fn reset_finish(&self, record: &mut Record, state: &str, code: &str) -> Result<ResetOperation, ApiError> {
        record.operation.state = state.into(); record.operation.code = code.into(); record.operation.updated_at = now_ms();
        save(&self.lock()?.connection, record)?;
        Ok(record.operation.clone())
    }
    fn reset_send(&self, record: &mut Record, auth: &Auth, client: &Provider) -> Result<ResetOperation, ApiError> {
        let fresh = client.list(auth)?;
        if owner(auth)? != record.owner || !fresh.credits.iter().any(|credit| credit == &record.credit && credit.usable) { return Err(changed()); }
        {
            let store = self.lock()?;
            if binding_fingerprint(&account(&store.connection, &record.operation.account_id)?)? != record.binding { return Err(changed()); }
            // FULL synchronous SQLite commit must precede the one and only POST.
            record.operation.state = "submitted".into(); record.operation.code = "submitted".into();
            record.operation.updated_at = now_ms();
            save(&store.connection, record)?;
        }
        let (state, code) = client.consume(auth, &record.credit, &record.operation.request_id);
        self.reset_finish(record, state, code)
    }
    pub(crate) fn reset_redeem(self: &Arc<Self>, request: RedeemRequest) -> Result<ResetOperation, ApiError> {
        let (mut record, execute) = self.reset_begin(&request)?;
        if !execute { return Ok(record.operation); }
        let result = (|| {
            let account = self.reset_account(&request.account_id)?;
            let (auth, client) = self.reset_auth(&account)?;
            self.reset_send(&mut record, &auth, &client)
        })();
        match result {
            Ok(operation) => { if operation.state == "succeeded" { self.start_refresh_with(true); } Ok(operation) }
            Err(error) if record.operation.state == "preparing" => self.reset_finish(&mut record, "declined", &error.code),
            // A failed result commit is not evidence of a failed spend. Leave the durable submitted fence intact.
            Err(_) => Err(ApiError::new("RESET_RESULT_UNKNOWN", "초기화권 사용 결과를 저장하지 못했어요. 다시 사용하지 말고 결과를 조회해 주세요.")),
        }
    }
}

#[cfg(test)]
#[path = "reset_credits_tests.rs"]
mod tests;
