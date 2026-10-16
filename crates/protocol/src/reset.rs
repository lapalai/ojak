use serde::{Deserialize, Serialize};

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
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetInventory {
    pub account_id: String,
    /// known, unknown, unsupported. Unknown is never a zero balance.
    pub state: String,
    pub available_count: Option<u32>,
    pub redeemable_count: Option<u32>,
    pub credits: Vec<ResetCredit>,
    pub observed_at: i64,
    pub quote_id: Option<String>,
    pub operation: Option<ResetOperation>,
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
