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
    /// known, restricted, unknown. Only `known` carries totals and may authorize a use.
    pub state: &'static str,
    /// Fixed local code explaining a non-known state.
    pub reason: Option<&'static str>,
    pub available: Option<u32>,
    pub redeemable: Option<u32>,
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
/// Canonical scope IDs plus whether every name is one the official client supports.
/// An unknown but well-formed name stays visible as `anthropic:reset:<name>` and is never spendable.
fn windows(value: &Value) -> Result<(Vec<String>, bool), ApiError> {
    let array = value.as_array().filter(|v| v.len() <= 64).ok_or_else(shape)?;
    let mut known = true;
    let scopes = array.iter().map(|raw| {
        let name = raw.as_str().filter(|s| token(s, 80)).ok_or_else(shape)?;
        Ok(match name {
            "five_hour" => "anthropic:5h".into(),
            "seven_day" => "anthropic:7d".into(),
            "seven_day_opus" => "anthropic:7d:opus".into(),
            "seven_day_sonnet" => "anthropic:7d:sonnet".into(),
            // The Fable weekly window.
            "seven_day_overage_included" => "anthropic:7d:fable".into(),
            "seven_day_cowork" | "seven_day_omelette" | "seven_day_oauth_apps" => format!("anthropic:reset:{name}"),
            _ => { known = false; format!("anthropic:reset:{name}") }
        })
    }).collect::<Result<Vec<String>, ApiError>>()?;
    Ok((scopes, known))
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
        // Fixtures close each socket; do not let a stale pooled socket replace the intended POST loss.
        Self { client: Client::builder().no_proxy().pool_max_idle_per_host(0).timeout(Duration::from_secs(2))
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
        // Both reads are always made: each may carry Cedar and Juniper, and neither may hide the other.
        let cedar = self.get(auth, "/api/oauth/usage?cedar_ember=1&skip_spend=1")?;
        let at_wall = self.get(auth, "/api/oauth/usage?at_wall=1&skip_spend=1")?;
        claude_inventory(&[&cedar, &at_wall], now_ms())
    }
    /// Only explicit, understood outcomes are terminal. Everything else may have spent a credit.
    pub fn consume(&self, auth: &Auth, credit: &ResetCredit, request_id: &str) -> (&'static str, &'static str) {
        let (path, body) = if auth.provider == "openai-codex" {
            ("/wham/rate-limit-reset-credits/consume".to_owned(),
                json!({"credit_id": credit.id, "redeem_request_id": request_id, "account_id": auth.account_id}))
        } else {
            let Some(org) = auth.org_id.as_ref().filter(|s| token(s, 128)) else { return ("declined", "organization_unavailable"); };
            let body = match credit.program.as_str() {
                "cedar_ember" if matches!(credit.reset_type.as_str(), "session" | "weekly" | "full" | "fable") =>
                    json!({"program":"cedar_ember", "grant_id":credit.id, "request_id":request_id}),
                "juniper_tide" if credit.reset_type == "provider" => json!({"program":"juniper_tide"}),
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
        let remaining_count = u32::from(status == "available");
        let status = if usable { "available" } else if status == "redeemed" { "redeemed" } else if expires_at.is_some_and(|t| t <= now) { "expired" } else { "unavailable" };
        credits.push(ResetCredit { id: id.into(), program: "codex_rate_limits".into(), remaining_count,
            usable, expires_at, starts_at, clears: vec!["openai-codex:rate_limits".into()], requires_limit: true, status: status.into(),
            reset_type: "provider".into(), gives_back_weekly: false, blocking: vec![],
            unavailable_reason: (!usable).then(|| match status { "redeemed" => "redeemed", "expired" => "expired", _ if starts_at.is_some_and(|t| t > now) => "not_started", _ => "unavailable" }.into()),
            cooldown_until: None });
    }
    let available = match value.get("available_count") { Some(v) => count(v)?, None => credits.iter().map(|c| c.remaining_count).sum() };
    let redeemable = credits.iter().filter(|c| c.usable).count() as u32;
    if available < redeemable { return Err(shape()); }
    Ok(Inventory { state: "known", reason: None, available: Some(available), redeemable: Some(redeemable), credits })
}

/// What one program block proves about the account's balance.
#[derive(Clone, Copy, PartialEq)]
enum Verdict {
    /// The program lists offers for this account; its grants are the balance.
    Eligible,
    /// The server says the account holds nothing in this program. The only genuine zero.
    NoGrant,
    /// The server limits what this client may see (surface, cli_version, unavailable, unknown, ...).
    /// Nothing is proven about the balance, and nothing may be spent.
    Restricted(&'static str),
}
struct Observation { program: &'static str, verdict: Verdict, credits: Vec<ResetCredit> }

fn ineligible_token(block: &Value) -> &'static str {
    match block["ineligible_reason"].as_str() {
        Some("no_grant") => "no_grant", Some("surface") => "surface", Some("cli_version") => "cli_version",
        Some("unavailable") => "unavailable", Some("config_off") => "config_off", Some("tier") => "tier",
        Some("seat") => "seat", Some("mobile") => "mobile", Some("tenure") => "tenure",
        Some("other_experiment") => "other_experiment", Some("not_at_wall") => "not_at_wall", _ => "unknown",
    }
}
/// Only `no_grant` and program-membership reasons prove this program holds nothing for the account, and only
/// when no offer contradicts it. Reasons that limit what this client can see prove nothing.
fn verdict_for(token: &'static str, holds_offers: bool) -> Verdict {
    match token {
        "surface" | "cli_version" | "unavailable" | "unknown" | "mobile" | "config_off" | "not_at_wall" => Verdict::Restricted(token),
        _ if holds_offers => Verdict::Restricted("unknown"),
        _ => Verdict::NoGrant,
    }
}
fn combine(a: Verdict, b: Verdict) -> Verdict {
    match (a, b) {
        (Verdict::Restricted(reason), _) | (_, Verdict::Restricted(reason)) => Verdict::Restricted(reason),
        (Verdict::Eligible, Verdict::Eligible) => Verdict::Eligible,
        (Verdict::NoGrant, Verdict::NoGrant) => Verdict::NoGrant,
        // One read lists offers and another denies any: ambiguous, never zero.
        _ => Verdict::Restricted("unknown"),
    }
}

fn cedar_credit(grant: &Value, ineligible: Option<&'static str>, next: Option<&str>, cooldown: Option<i64>, now: i64) -> Result<ResetCredit, ApiError> {
    let id = grant["id"].as_str().filter(|s| token(s, 40)).ok_or_else(shape)?;
    let remaining_count = count(&grant["resets_left"])?;
    let expires_at = timestamp(&grant["ends_at"])?;
    let starts_at = timestamp(&grant["starts_at"])?;
    let paused = boolean(&grant["paused"], false)?;
    let usable_now = boolean(&grant["usable_now"], false)?;
    let requires_limit = boolean(&grant["use_requires_limit"], true)?;
    let gives_back_weekly = boolean(&grant["gives_back_weekly"], false)?;
    let (clears, clears_known) = windows(&grant["clears"])?;
    // A malformed eligibility/blocking description must not authorize a spend.
    let (blocking, _) = windows(&grant["blocking"])?;
    // `type` is authoritative when it is one of the supported values. A missing type is derived from
    // what the grant clears (as the official client does); any other value is an unknown type.
    let (kind, explicit) = match grant.get("type") {
        None | Some(Value::Null) => {
            let session = clears.iter().any(|c| c == "anthropic:5h");
            let other = clears.iter().any(|c| c != "anthropic:5h");
            (match (session, other) { (true, false) => "session", (false, true) => "weekly", (true, true) => "full", _ => "unknown" }, false)
        }
        Some(Value::String(kind)) => match kind.as_str() {
            "session" => ("session", true), "weekly" => ("weekly", true), "full" => ("full", true), _ => ("unknown", false),
        },
        Some(_) => ("unknown", false),
    };
    // gives_back_weekly marks the Fable partial reimbursement: it restores Fable weekly usage only.
    let reset_type = if kind == "unknown" { "unknown" } else if gives_back_weekly { "fable" } else { kind };
    let provider_reason = match grant.get("unavailable_reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(reason)) => Some(match reason.as_str() {
            "cooldown" => "cooldown", "blocked_five_hour" => "blocked_five_hour", "blocked_seven_day" => "blocked_seven_day",
            "blocked_opus" => "blocked_opus", "blocked_sonnet" => "blocked_sonnet", "blocked_other" => "blocked_other",
            "cannot_clear" => "cannot_clear", "needs_limit" => "needs_limit", _ => "unavailable",
        }),
        Some(_) => Some("unavailable"),
    };
    let expired = expires_at.is_some_and(|t| t <= now);
    // `next_grant_id` is the server's default recommendation. A grant that declares a supported
    // explicit type is selectable on its own `usable_now`; a legacy grant must be the recommended one.
    let selected = explicit || next == Some(id);
    let reason: Option<&'static str> = if expired { Some("expired") }
        else if remaining_count == 0 { Some("redeemed") }
        else if paused { Some("paused") }
        else if starts_at.is_some_and(|t| t > now) { Some("not_started") }
        else if cooldown.is_some_and(|t| t > now) { Some("cooldown") }
        else if let Some(reason) = ineligible { Some(reason) }
        else if provider_reason.is_some() { provider_reason }
        else if !blocking.is_empty() { Some("blocked") }
        else if reset_type == "unknown" { Some("unknown_type") }
        else if !clears_known || clears.is_empty() { Some("unknown_scope") }
        else if !selected { Some("not_next_grant") }
        else if !usable_now { Some("unavailable") }
        else { None };
    let usable = reason.is_none();
    Ok(ResetCredit { id: id.into(), program: "cedar_ember".into(), remaining_count, usable, expires_at, starts_at, clears, requires_limit,
        status: if expired { "expired" } else if remaining_count == 0 { "redeemed" } else if paused { "paused" } else if usable { "available" } else { "unavailable" }.into(),
        reset_type: reset_type.into(), gives_back_weekly, blocking, unavailable_reason: reason.map(Into::into), cooldown_until: cooldown })
}

fn observe_cedar(block: &Value, now: i64) -> Result<Observation, ApiError> {
    let eligible = block["eligible"].as_bool().ok_or_else(shape)?;
    let ineligible = (!eligible).then(|| ineligible_token(block));
    let empty = Vec::new();
    let grants = match block.get("grants") { None => &empty, Some(v) => v.as_array().filter(|v| v.len() <= 256).ok_or_else(shape)? };
    let cooldown = timestamp(&block["cooldown_until"])?;
    let next = block["next_grant_id"].as_str();
    if !block["next_grant_id"].is_null() && !next.is_some_and(|s| token(s, 40)) { return Err(shape()); }
    let mut credits: Vec<ResetCredit> = Vec::with_capacity(grants.len());
    for grant in grants {
        let credit = cedar_credit(grant, ineligible, next, cooldown, now)?;
        if credits.iter().any(|c| c.id == credit.id) { return Err(shape()); }
        credits.push(credit);
    }
    let holds = credits.iter().any(|c| c.remaining_count > 0 && c.status != "expired");
    let verdict = match ineligible { None => Verdict::Eligible, Some(token) => verdict_for(token, holds) };
    Ok(Observation { program: "cedar_ember", verdict, credits })
}

fn observe_juniper(block: &Value, now: i64) -> Result<Observation, ApiError> {
    let eligible = block["eligible"].as_bool().ok_or_else(shape)?;
    let arm = match block.get("arm") {
        None | Some(Value::Null) => None,
        Some(Value::String(arm)) if arm == "reset" || arm == "control" => Some(arm.as_str()),
        Some(_) => return Err(shape()),
    };
    let available = boolean(&block["available"], false)?;
    let expires_at = timestamp(&block["weekly_resets_at"])?;
    let cooldown = timestamp(&block["next_available_at"])?;
    let ineligible = (!eligible).then(|| ineligible_token(block));
    let reason: Option<&'static str> = if expires_at.is_some_and(|t| t <= now) { Some("expired") }
        else if cooldown.is_some_and(|t| t > now) { Some("cooldown") }
        else if let Some(reason) = ineligible { Some(reason) }
        else if !available { Some("unavailable") }
        else { None };
    let usable = arm == Some("reset") && reason.is_none();
    let credits = if arm == Some("reset") { vec![ResetCredit { id: "juniper_tide".into(), program: "juniper_tide".into(), remaining_count: u32::from(usable),
        usable, expires_at, starts_at: None, clears: vec!["anthropic:5h".into()], requires_limit: true,
        status: if usable { "available" } else { "unavailable" }.into(), reset_type: "provider".into(), gives_back_weekly: false,
        blocking: vec![], unavailable_reason: reason.map(Into::into), cooldown_until: cooldown }] } else { vec![] };
    let verdict = match ineligible {
        // This conditional experiment has not offered anything yet. Explicit non-participation
        // must not hide grants from other programs; the condition alone proves no balance.
        Some("not_at_wall") if arm.is_none()
            && block.get("in_experiment").and_then(Value::as_bool) == Some(false)
            && block.get("available").and_then(Value::as_bool) == Some(false) => Verdict::NoGrant,
        Some(token) => verdict_for(token, false),
        // Eligible without a stated arm is a proven absence only when the server says the account is not in the experiment.
        None if arm.is_none() && block.get("in_experiment").and_then(Value::as_bool) != Some(false) => Verdict::Restricted("unknown"),
        None if arm.is_none() => Verdict::NoGrant,
        None => Verdict::Eligible,
    };
    Ok(Observation { program: "juniper_tide", verdict, credits })
}

fn mark_ambiguous(credit: &mut ResetCredit) {
    credit.usable = false;
    credit.unavailable_reason = Some("ambiguous".into());
    if credit.status == "available" { credit.status = "unavailable".into(); }
}
/// Fields that identify an offer and its exact boundaries; live eligibility is excluded.
fn same_offer(a: &ResetCredit, b: &ResetCredit) -> bool {
    a.id == b.id && a.program == b.program && a.remaining_count == b.remaining_count && a.expires_at == b.expires_at
        && a.starts_at == b.starts_at && a.clears == b.clears && a.reset_type == b.reset_type
        && a.gives_back_weekly == b.gives_back_weekly && a.requires_limit == b.requires_limit
}
fn merge(kept: &mut ResetCredit, other: ResetCredit) {
    kept.usable &= other.usable;
    for scope in other.blocking { if !kept.blocking.contains(&scope) { kept.blocking.push(scope); } }
    if kept.unavailable_reason.is_none() { kept.unavailable_reason = other.unavailable_reason; }
    kept.cooldown_until = kept.cooldown_until.max(other.cooldown_until);
    if kept.status == "available" && other.status != "available" { kept.status = other.status; }
    if !kept.usable && kept.status == "available" { kept.status = "unavailable".into(); }
    if !kept.usable && kept.unavailable_reason.is_none() { kept.unavailable_reason = Some("unavailable".into()); }
}
/// A second credit that reuses an ID under another program is renamed so IDs stay unique. It is shown but never spendable.
fn disambiguate(credits: &[ResetCredit], credit: &mut ResetCredit) -> Result<(), ApiError> {
    credit.id = format!("{}-{}", credit.program, credit.id);
    if credit.id.len() > 256 || credits.iter().any(|c| c.id == credit.id) { return Err(shape()); }
    mark_ambiguous(credit);
    Ok(())
}

/// Both usage reads may carry both programs. Combine every block seen, never dropping an offer
/// and never counting one twice.
pub(crate) fn claude_inventory(values: &[&Value], now: i64) -> Result<Inventory, ApiError> {
    let mut observations = Vec::new();
    for value in values {
        let object = value.as_object().ok_or_else(shape)?;
        let cedar = object.get("cedar_ember").filter(|v| !v.is_null());
        let juniper = object.get("juniper_tide").filter(|v| !v.is_null());
        if cedar.is_none() && juniper.is_none()
            && !["five_hour", "seven_day", "limits", "extra_usage", "spend"].iter().any(|key| object.contains_key(*key)) { return Err(shape()); }
        if let Some(block) = cedar { observations.push(observe_cedar(block, now)?); }
        if let Some(block) = juniper { observations.push(observe_juniper(block, now)?); }
    }
    if observations.is_empty() {
        // The server never named a program. That is a missing answer, not a zero balance.
        return Ok(Inventory { state: "unknown", reason: Some("program_missing"), available: None, redeemable: None, credits: vec![] });
    }
    let mut verdicts: Vec<(&'static str, Verdict)> = Vec::new();
    let mut credits: Vec<ResetCredit> = Vec::new();
    let mut ambiguous = false;
    for observation in observations {
        match verdicts.iter_mut().find(|(program, _)| *program == observation.program) {
            Some((_, verdict)) => *verdict = combine(*verdict, observation.verdict),
            None => verdicts.push((observation.program, observation.verdict)),
        }
        for mut credit in observation.credits {
            match credits.iter().position(|c| c.id == credit.id) {
                None => credits.push(credit),
                // The same grant seen in both reads is one offer, counted once. Live eligibility may differ
                // between the reads, so the merge keeps the more conservative answer.
                Some(index) if credits[index].program == credit.program && same_offer(&credits[index], &credit) => merge(&mut credits[index], credit),
                Some(index) if credits[index].program == credit.program => { ambiguous = true; mark_ambiguous(&mut credits[index]); }
                Some(index) => {
                    ambiguous = true; mark_ambiguous(&mut credits[index]);
                    disambiguate(&credits, &mut credit)?;
                    credits.push(credit);
                }
            }
        }
    }
    // Any restricted program leaves part of the balance unobserved, so nothing may authorize a use.
    let restricted = verdicts.iter().find_map(|(_, verdict)| match verdict { Verdict::Restricted(reason) => Some(*reason), _ => None })
        .or(ambiguous.then_some("ambiguous"));
    if let Some(reason) = restricted {
        // Offers stay visible as observations; none can authorize a use and no total is claimed.
        for credit in &mut credits {
            credit.usable = false;
            if credit.unavailable_reason.is_none() { credit.unavailable_reason = Some(reason.into()); }
            if credit.status == "available" { credit.status = "unavailable".into(); }
        }
        return Ok(Inventory { state: "restricted", reason: Some(reason), available: None, redeemable: None, credits });
    }
    let mut available = 0u32;
    let mut redeemable = 0u32;
    for credit in &credits {
        if credit.status != "expired" { available = available.checked_add(credit.remaining_count).ok_or_else(shape)?; }
        if credit.usable { redeemable = redeemable.checked_add(credit.remaining_count).ok_or_else(shape)?; }
    }
    Ok(Inventory { state: "known", reason: None, available: Some(available), redeemable: Some(redeemable), credits })
}

#[cfg(test)]
#[path = "reset_test_http.rs"]
pub(crate) mod test_http;
#[cfg(test)]
#[path = "reset_provider_tests.rs"]
mod tests;
