//! Subscription reset contracts, matching omp's claude-reset/openai-codex-reset adapters.
//! Fixed first-party origins; no API-key fallback, redirects, or mutation retries.
use aam_protocol::{now_ms, ApiError, ResetCredit};
use reqwest::{blocking::Client, Method};
use serde_json::{json, Value};
use std::{borrow::Cow, io::Read, sync::LazyLock, time::Duration};

pub(super) struct Auth {
    pub provider: &'static str,
    pub access: String,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub org_id: Option<String>,
}

pub(super) struct Inventory {
    pub available: u32,
    pub redeemable: u32,
    pub credits: Vec<ResetCredit>,
}

pub(super) fn failure() -> ApiError {
    ApiError::new("RESET_UNAVAILABLE", "초기화권 정보를 확인하지 못했어요. 보유 수량이 0개라는 뜻은 아니에요.")
}
fn shape() -> ApiError { ApiError::new("RESET_RESPONSE_INVALID", "공급자의 초기화권 응답을 확인하지 못했어요.") }
pub(super) fn token(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}
fn timestamp(value: &Value) -> Result<Option<i64>, ApiError> {
    if value.is_null() { return Ok(None); }
    chrono::DateTime::parse_from_rfc3339(value.as_str().ok_or_else(shape)?)
        .map(|date| Some(date.timestamp_millis())).map_err(|_| shape())
}
fn count(value: &Value) -> Result<u32, ApiError> {
    value.as_u64().and_then(|v| u32::try_from(v).ok()).ok_or_else(shape)
}
fn boolean(value: &Value, default: bool) -> Result<bool, ApiError> {
    if value.is_null() { Ok(default) } else { value.as_bool().ok_or_else(shape) }
}
fn windows(value: &Value) -> Result<Vec<String>, ApiError> {
    let array = value.as_array().filter(|v| v.len() <= 64).ok_or_else(shape)?;
    array.iter().map(|raw| {
        let name = raw.as_str().filter(|s| token(s, 80)).ok_or_else(shape)?;
        Ok(match name {
            "five_hour" => "anthropic:5h".into(),
            "seven_day" => "anthropic:7d".into(),
            "seven_day_opus" => "anthropic:7d:opus".into(),
            "seven_day_sonnet" => "anthropic:7d:sonnet".into(),
            _ => format!("anthropic:reset:{name}"),
        })
    }).collect()
}

pub(super) struct Provider {
    client: Client,
    base: Cow<'static, str>,
}
impl Provider {
    pub fn new(provider: &str) -> Result<Self, ApiError> {
        static CLIENT: LazyLock<Result<Client, ()>> = LazyLock::new(|| Client::builder()
            .connect_timeout(Duration::from_secs(3)).timeout(Duration::from_secs(12))
            .redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never())
            .build().map_err(|_| ()));
        let client = CLIENT.as_ref().map_err(|_| failure())?.clone();
        let base = match provider {
            "anthropic" => "https://api.anthropic.com",
            "openai-codex" => "https://chatgpt.com/backend-api",
            _ => return Err(ApiError::new("RESET_UNSUPPORTED", "이 공급자의 초기화권 기능은 지원하지 않아요.")),
        };
        Ok(Self { client, base: Cow::Borrowed(base) })
    }

    #[cfg(test)]
    pub(super) fn for_test(base: String) -> Self {
        Self { client: Client::builder().no_proxy().timeout(Duration::from_secs(2))
            .redirect(reqwest::redirect::Policy::none()).retry(reqwest::retry::never()).build().unwrap(),
            base: Cow::Owned(base) }
    }

    fn request(&self, auth: &Auth, method: Method, path: &str, body: Option<&Value>) -> Result<(u16, Value), ApiError> {
        let mut request = self.client.request(method, format!("{}{path}", self.base))
            .bearer_auth(&auth.access).header("Accept", "application/json")
            .header("User-Agent", concat!("Ojak/", env!("CARGO_PKG_VERSION")));
        if auth.provider == "anthropic" {
            request = request.header("anthropic-beta", "oauth-2025-04-20");
        } else if let Some(account) = &auth.account_id {
            request = request.header("ChatGPT-Account-Id", account);
        }
        if let Some(body) = body { request = request.json(body); }
        let response = request.send().map_err(|_| failure())?;
        let status = response.status().as_u16();
        let mut bytes = Vec::new();
        response.take(1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|_| failure())?;
        if bytes.len() > 1024 * 1024 { return Err(shape()); }
        let value = serde_json::from_slice(&bytes).map_err(|_| shape())?;
        Ok((status, value))
    }
    fn get(&self, auth: &Auth, path: &str) -> Result<Value, ApiError> {
        let (status, value) = self.request(auth, Method::GET, path, None)?;
        match status {
            200..=299 => Ok(value),
            401 | 403 => Err(ApiError::new("AUTH_REQUIRED", "원래 공급자의 로그인을 확인해 주세요.")),
            _ => Err(failure()),
        }
    }
    pub fn organization(&self, auth: &mut Auth) -> Result<(), ApiError> {
        if auth.provider != "anthropic" { return Ok(()); }
        if let Some(org) = &auth.org_id {
            return if token(org, 128) { Ok(()) } else { Err(shape()) };
        }
        let profile = self.get(auth, "/api/oauth/profile")?;
        let org = profile.pointer("/organization/uuid").or_else(|| profile.get("organization_uuid"))
            .and_then(Value::as_str).filter(|s| token(s, 128)).ok_or_else(shape)?;
        auth.org_id = Some(org.to_owned());
        Ok(())
    }
    pub fn list(&self, auth: &Auth) -> Result<Inventory, ApiError> {
        if auth.provider == "openai-codex" {
            return codex_inventory(&self.get(auth, "/wham/rate-limit-reset-credits")?, now_ms());
        }
        let cedar = self.get(auth, "/api/oauth/usage?cedar_ember=1&skip_spend=1")?;
        let cedar_list = claude_inventory(&cedar, "cedar_ember", now_ms())?;
        if cedar_list.as_ref().is_some_and(|list| list.available > 0) { return Ok(cedar_list.unwrap()); }
        let juniper = self.get(auth, "/api/oauth/usage?at_wall=1&skip_spend=1")?;
        Ok(claude_inventory(&juniper, "juniper_tide", now_ms())?.or(cedar_list)
            .unwrap_or(Inventory { available: 0, redeemable: 0, credits: vec![] }))
    }
    /// Only explicit, understood outcomes are terminal. Everything else may have spent a credit.
    pub fn consume(&self, auth: &Auth, credit: &ResetCredit, request_id: &str) -> (&'static str, &'static str) {
        let (path, body) = if auth.provider == "openai-codex" {
            ("/wham/rate-limit-reset-credits/consume".to_owned(),
                json!({"credit_id": credit.id, "redeem_request_id": request_id, "account_id": auth.account_id}))
        } else {
            let Some(org) = auth.org_id.as_ref().filter(|s| token(s, 128)) else { return ("declined", "organization_unavailable"); };
            let body = match credit.program.as_str() {
                "cedar_ember" => json!({"program":"cedar_ember", "grant_id":credit.id, "request_id":request_id}),
                "juniper_tide" => json!({"program":"juniper_tide"}),
                _ => return ("declined", "unsupported_program"),
            };
            (format!("/api/organizations/{org}/reset_rate_limits"), body)
        };
        let Ok((status, value)) = self.request(auth, Method::POST, &path, Some(&body)) else {
            return ("unknown", "result_unknown");
        };
        if !(200..300).contains(&status) { return ("unknown", "result_unknown"); }
        let code = value[if auth.provider == "anthropic" { "result" } else { "code" }].as_str();
        match code {
            Some("reset") => ("succeeded", "reset"),
            Some("already_redeemed" | "already_used") => ("declined", "already_redeemed"),
            Some("no_credit") => ("declined", "no_credit"),
            Some("nothing_to_reset" | "not_limited") => ("declined", "nothing_to_reset"),
            Some("ineligible" | "unavailable") => ("declined", "ineligible"),
            _ => ("unknown", "result_unknown"),
        }
    }
}

fn codex_inventory(value: &Value, now: i64) -> Result<Inventory, ApiError> {
    let rows = value["credits"].as_array().filter(|rows| rows.len() <= 256).ok_or_else(shape)?;
    let mut credits = Vec::with_capacity(rows.len());
    for row in rows {
        let id = row["id"].as_str().filter(|s| token(s, 256)).ok_or_else(shape)?;
        if credits.iter().any(|credit: &ResetCredit| credit.id == id) { return Err(shape()); }
        let status = match row.get("status") { None => "available", Some(value) => value.as_str().ok_or_else(shape)? };
        let expires_at = timestamp(&row["expires_at"])?;
        let starts_at = timestamp(&row["granted_at"])?;
        let usable = status == "available" && expires_at.is_none_or(|t| t > now) && starts_at.is_none_or(|t| t <= now);
        credits.push(ResetCredit { id: id.into(), program: "codex_rate_limits".into(), remaining_count: u32::from(status == "available"),
            usable, expires_at, starts_at, clears: vec!["openai-codex:rate_limits".into()], requires_limit: true,
            status: if usable { "available" } else if status == "redeemed" { "redeemed" } else if expires_at.is_some_and(|t| t <= now) { "expired" } else { "unavailable" }.into() });
    }
    let available = match value.get("available_count") { Some(v) => count(v)?, None => credits.iter().map(|c| c.remaining_count).sum() };
    let redeemable = credits.iter().filter(|c| c.usable).count() as u32;
    if available < redeemable { return Err(shape()); }
    Ok(Inventory { available, redeemable, credits })
}

fn claude_inventory(value: &Value, program: &str, now: i64) -> Result<Option<Inventory>, ApiError> {
    let block = &value[program];
    if block.is_null() {
        if !["five_hour", "seven_day", "limits", "extra_usage", "spend"].iter().any(|key| value.get(key).is_some()) { return Err(shape()); }
        return Ok(None);
    }
    let eligible = block["eligible"].as_bool().ok_or_else(shape)?;
    if program == "juniper_tide" {
        let arm = block["arm"].as_str();
        if !matches!(arm, None | Some("reset" | "control")) { return Err(shape()); }
        let available = boolean(&block["available"], false)?;
        let expires_at = timestamp(&block["weekly_resets_at"])?;
        let cooldown = timestamp(&block["next_available_at"])?;
        let usable = eligible && arm == Some("reset") && available && expires_at.is_none_or(|t| t > now) && cooldown.is_none_or(|t| t <= now);
        let count = u32::from(usable);
        let credits = if arm == Some("reset") { vec![ResetCredit { id: program.into(), program: program.into(), remaining_count: count,
            usable, expires_at, starts_at: None, clears: vec!["anthropic:5h".into()], requires_limit: true,
            status: if usable { "available" } else { "unavailable" }.into() }] } else { vec![] };
        return Ok(Some(Inventory { available: count, redeemable: count, credits }));
    }
    let empty = Vec::new();
    let grants = match block.get("grants") { None => &empty, Some(v) => v.as_array().filter(|v| v.len() <= 256).ok_or_else(shape)? };
    let cooldown = timestamp(&block["cooldown_until"])?;
    let next = block["next_grant_id"].as_str();
    if !block["next_grant_id"].is_null() && !next.is_some_and(|s| token(s, 40)) { return Err(shape()); }
    let mut credits = Vec::with_capacity(grants.len());
    let mut available = 0u32;
    let mut redeemable = 0u32;
    for grant in grants {
        let id = grant["id"].as_str().filter(|s| token(s, 40)).ok_or_else(shape)?;
        if credits.iter().any(|c: &ResetCredit| c.id == id) { return Err(shape()); }
        let remaining_count = count(&grant["resets_left"])?;
        let expires_at = timestamp(&grant["ends_at"])?;
        let starts_at = timestamp(&grant["starts_at"])?;
        let expired = expires_at.is_some_and(|t| t <= now);
        let paused = boolean(&grant["paused"], false)?;
        let usable = eligible && next == Some(id) && boolean(&grant["usable_now"], false)? && !paused && !expired
            && starts_at.is_none_or(|t| t <= now) && cooldown.is_none_or(|t| t <= now) && remaining_count > 0;
        let clears = windows(&grant["clears"])?;
        // A malformed eligibility/blocking description must not authorize a spend.
        windows(&grant["blocking"])?;
        if !expired { available = available.checked_add(remaining_count).ok_or_else(shape)?; }
        if usable { redeemable = remaining_count; }
        credits.push(ResetCredit { id: id.into(), program: program.into(), remaining_count, usable, expires_at, starts_at, clears,
            requires_limit: boolean(&grant["use_requires_limit"], true)?,
            status: if expired { "expired" } else if remaining_count == 0 { "redeemed" } else if paused { "paused" } else if usable { "available" } else { "unavailable" }.into() });
    }
    Ok(Some(Inventory { available, redeemable, credits }))
}

#[cfg(test)]
#[path = "reset_test_http.rs"]
pub(crate) mod test_http;
#[cfg(test)]
#[path = "reset_provider_tests.rs"]
mod tests;
