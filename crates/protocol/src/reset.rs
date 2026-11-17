use serde::{Deserialize, Serialize};

fn unknown_reset_type() -> String { "unknown".into() }
fn oauth_source() -> String { "oauth".into() }

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResetCredit {
    pub id: String,
    pub program: String,
    pub remaining_count: u32,
    pub usable: bool,
    pub expires_at: Option<i64>,
    pub starts_at: Option<i64>,
    pub clears: Vec<String>,
    pub requires_limit: bool,
    pub status: String,
    /// session, weekly, full, fable, provider, unknown. Records written before this field
    /// existed deserialize as `unknown`, which can never authorize a spend.
    #[serde(default = "unknown_reset_type")]
    pub reset_type: String,
    /// Claude `gives_back_weekly`: a Fable partial reimbursement, not a full weekly reset.
    #[serde(default)]
    pub gives_back_weekly: bool,
    /// Fixed sanitized scope tokens (`anthropic:5h`, `anthropic:7d`, ...; `unknown_scope` when not recognized).
    #[serde(default)]
    pub blocking: Vec<String>,
    /// Fixed local code explaining why `usable` is false; never provider text.
    #[serde(default)]
    pub unavailable_reason: Option<String>,
    #[serde(default)]
    pub cooldown_until: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetInventory {
    pub account_id: String,
    /// known, unknown, unsupported, restricted. Unknown and restricted are never a zero balance.
    pub state: String,
    pub available_count: Option<u32>,
    pub redeemable_count: Option<u32>,
    pub credits: Vec<ResetCredit>,
    pub observed_at: i64,
    pub quote_id: Option<String>,
    pub operation: Option<ResetOperation>,
    /// Fixed local code for a non-known state (surface, cli_version, unavailable, unknown, program_missing, stale, ...).
    #[serde(default)]
    pub lookup_reason: Option<String>,
    /// `oauth` or `web`. Records written before this field existed deserialize as `oauth`.
    #[serde(default = "oauth_source")]
    pub source: String,
    /// Present for Claude accounts that can use the web route (connected or not); `None` otherwise.
    #[serde(default)]
    pub web: Option<ResetWebStatus>,
}

/// Connection status of the account-bound Claude web reset reader.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResetWebStatus {
    pub account_id: String,
    pub connected: bool,
    /// disconnected, waiting_browser, login_required, syncing, ready, error.
    pub state: String,
    pub last_attempt_at: Option<i64>,
    pub last_success_at: Option<i64>,
    pub next_refresh_at: Option<i64>,
    /// Fixed local code (`WEB_*`, `AUTH_OVERRIDE_CONFLICT`); never provider text.
    pub error_code: Option<String>,
}

/// One isolated web profile bound to a registered account.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResetWebConnection {
    pub account_id: String,
    pub connection_id: String,
    pub organization_id: String,
}

/// A claimed read the native companion must run in the bound web profile.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ResetWebJob {
    pub account_id: String,
    pub connection_id: String,
    pub organization_id: String,
    pub request_id: String,
    pub expires_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetOperation {
    pub request_id: String,
    pub account_id: String,
    pub credit_id: String,
    /// preparing, submitted, unknown, succeeded, declined, resolved.
    pub state: String,
    /// A fixed local code, never a provider's arbitrary response text.
    pub code: String,
    pub started_at: i64,
    pub updated_at: i64,
}
