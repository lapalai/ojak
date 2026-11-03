use aam_adapters::api_credit::ApiProvider;
use aam_protocol::{
    project_allowed, Account, AllocationMode, ApiError, Candidate, Decision, LaunchIntent, Policy,
    QuotaBucket, Session, NATIVE_DEFAULT_MODEL,
};
use std::{
    cmp::Ordering,
    collections::HashMap,
    path::{Path, PathBuf},
};

pub fn holds_capacity(state: &str) -> bool {
    matches!(
        state,
        "PREPARED" | "STARTING" | "ACTIVE" | "SUSPECT" | "ORPHANED"
    )
}

#[derive(PartialEq, Eq)]
enum Scope {
    /// 요청한 모델에 확실히 적용되는 한도입니다. 입장 판정에 사용합니다.
    Applies,
    /// 실제 모델을 모르는 요청에 걸린 모델 전용 한도입니다. 안내만 하고 실행을 막지 않습니다.
    ModelOnly,
}

/// 공식 CLI가 실행 시점에 모델을 결정하는 요청입니다.
fn default_model(model: &str) -> bool {
    matches!(
        model,
        NATIVE_DEFAULT_MODEL | "default" | "best" | "opusplan"
    )
}

fn scope(bucket: &QuotaBucket, model: &str) -> Option<Scope> {
    // 공식 CLI가 모델을 결정하는 요청에서는 모델 전용 한도를 요청 모델로 단정하지 않습니다.
    if default_model(model) {
        return Some(if bucket.model.is_none() {
            Scope::Applies
        } else {
            Scope::ModelOnly
        });
    }
    let model = model.split_once('/').map_or(model, |(_, id)| id);
    match bucket.model.as_deref() {
        None => Some(Scope::Applies),
        Some(scoped) => (scoped == model
            || (["fable", "opus", "sonnet", "haiku"].contains(&scoped)
                && model
                    .strip_prefix("claude-")
                    .unwrap_or(model)
                    .strip_prefix(scoped)
                    .is_some_and(|suffix| suffix.starts_with('-'))))
            .then_some(Scope::Applies),
    }
}

/// 요청 모델에 확실히 적용되는 한도인지 판단합니다. 공용 한도와 해당 모델 전용 한도가 여기에 해당합니다.
pub(crate) fn applies(bucket: &QuotaBucket, model: &str) -> bool {
    scope(bucket, model) == Some(Scope::Applies)
}

pub(crate) fn short_window(bucket: &QuotaBucket) -> bool {
    let text = format!("{} {}", bucket.id, bucket.label).to_lowercase();
    [
        "5h",
        "5-hour",
        "5 hour",
        "five_hour",
        "five-hour",
        "5시간",
        "short",
    ]
    .iter()
    .any(|part| text.contains(part))
}

/// "리셋 전 사용"에서 5시간 한도가 이만큼(%) 이상 찼으면 안전 여유량을 건너뛰지 않는다. 단기 한도가 뜨거운 계정은 리셋 임박 여부와 무관하게 뒤로 둔다.
pub(crate) const SHORT_WINDOW_HOT_PERCENT: f64 = 85.0;

/// 소비 균형 모드의 "리셋 전 사용": 안전 여유량 안쪽이어도 이 계정의 남은 긴 주기 한도가 곧 리셋돼 사라질 것이면, 그 리셋 시각(ms)을 돌려준다.
/// 안전 여유량은 새 한도가 오기 전에 다 써 버려 대화가 중간에 끊기는 걸 막는 완충이다. 그러나 가장 빡빡한 긴 주기 한도가 `window_ms` 안에 리셋되면
/// 남겨 둔 완충은 어차피 리셋과 함께 사라지므로, 새 대화를 먼저 받아 남은 양을 쓴다(진행 중 대화는 고정 규칙 그대로라 옮기지 않는다).
/// `applicable`은 요청 모델에 적용되는 한도만 넣는다. 다음 중 하나라도 어긋나면 `None`이다(= 기존 안전 여유량 규칙 그대로):
/// 여유량·리셋 시각이 없음, 소진(100%)된 한도가 있음, 5시간 한도가 안전 여유량 안쪽이거나 `SHORT_WINDOW_HOT_PERCENT` 이상,
/// 긴 주기 한도가 없음, 가장 많이 쓴 긴 주기 한도의 리셋이 지났거나 `window_ms`보다 멂.
/// 모델 전용 한도가 가장 빡빡하면 그 한도의 리셋이 기준이다(공용 한도가 곧 리셋돼도 그 모델은 계속 막힌다).
pub(crate) fn use_before_reset<'a>(applicable: impl IntoIterator<Item = &'a QuotaBucket>, reserve: f64, window_ms: i64, now: i64) -> Option<i64> {
    if reserve <= 0.0 || window_ms <= 0 {
        return None;
    }
    let mut binding: Option<(f64, Option<i64>)> = None;
    for bucket in applicable {
        let Some(used) = bucket.used_percent.filter(|used| used.is_finite()) else { continue };
        if bucket.status == "exhausted" || used >= 100.0 {
            return None;
        }
        if short_window(bucket) {
            if used >= 100.0 - reserve || used >= SHORT_WINDOW_HOT_PERCENT {
                return None;
            }
        } else if binding.is_none_or(|(tightest, _)| used > tightest) {
            binding = Some((used, bucket.resets_at));
        }
    }
    let reset = binding?.1?;
    (reset > now && reset - now <= window_ms).then_some(reset)
}

/// 크레딧 fallback(codex)으로 고른 후보에 붙는 이유 코드.
pub(crate) const CREDITS_FALLBACK: &str = "CREDITS_FALLBACK";
/// 추가 사용량 fallback(claude)으로 고른 후보에 붙는 이유 코드.
pub(crate) const EXTRA_USAGE_FALLBACK: &str = "EXTRA_USAGE_FALLBACK";
/// API 크레딧 fallback(claude, Ojak에 등록한 Console API 키)으로 고른 후보에 붙는 이유 코드.
pub(crate) const API_CREDIT_FALLBACK: &str = "API_CREDIT_FALLBACK";
/// 월 상한에 닿아 API 키를 쓰지 못할 때 후보에 붙는 이유 코드.
pub(crate) const API_CREDIT_LIMIT: &str = "API_CREDIT_LIMIT";
/// 금액을 읽지 못한 사용이 있어 OpenAI API 키를 이번 달 더는 쓰지 않을 때 후보에 붙는 이유 코드.
pub(crate) const API_UNMETERED: &str = "API_UNMETERED";

fn chose_with(decision: &Decision, code: &str) -> bool {
    decision.selected_account_id.as_ref().is_some_and(|id| {
        decision.candidates.iter().any(|candidate| {
            &candidate.account_id == id && candidate.reasons.iter().any(|reason| reason.starts_with(code) && reason[code.len()..].starts_with(':'))
        })
    })
}

/// 이 판정이 크레딧 fallback으로 계정을 골랐는지.
pub(crate) fn chose_credits(decision: &Decision) -> bool {
    chose_with(decision, CREDITS_FALLBACK)
}

/// 이 판정이 추가 사용량 fallback으로 계정을 골랐는지.
pub(crate) fn chose_extra_usage(decision: &Decision) -> bool {
    chose_with(decision, EXTRA_USAGE_FALLBACK)
}

/// 이 판정이 API 크레딧 fallback으로 계정을 골랐는지.
pub(crate) fn chose_api_credit(decision: &Decision) -> bool {
    chose_with(decision, API_CREDIT_FALLBACK)
}

/// 이번 달(UTC 달력월) 추정 사용액. 저장된 기간이 지났으면 0으로 본다.
pub(crate) fn api_credit_used(account: &Account, now: i64) -> f64 {
    let period = aam_adapters::api_credit::period_start(now);
    account
        .api_credit
        .as_ref()
        .filter(|state| state.period_start == period)
        .map_or(0.0, |state| state.used_usd)
}

/// API 크레딧 사용액 원장 중 판정에 필요한 값. `removed_usd`·`openai_removed_usd`는 지운 키의 이번 달(UTC) 추정 사용액(공급자별),
/// `grant_*`는 현재 허용 기간(`Policy.api_credit_grant`, Claude 전용)에 기록된 합산 추정 사용액과 금액을 읽지 못한 요청 수다(키와 무관하게 허용 기간 id로 쌓는다).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ApiLedger {
    pub removed_usd: f64,
    pub openai_removed_usd: f64,
    /// 지운 OpenAI 키가 이번 달에 남긴 "금액을 읽지 못한 요청" 수. 하나라도 있으면 OpenAI 키는 이번 달 더는 쓰지 않는다.
    pub openai_removed_unmetered: u64,
    pub grant_used_usd: f64,
    pub grant_unmetered: u64,
}

impl ApiLedger {
    /// 공급자별 지운 키의 이번 달 사용액.
    pub(crate) fn removed(&self, provider: aam_adapters::api_credit::ApiProvider) -> f64 {
        match provider {
            aam_adapters::api_credit::ApiProvider::Anthropic => self.removed_usd,
            aam_adapters::api_credit::ApiProvider::OpenAi => self.openai_removed_usd,
        }
    }
}

/// 이 공급자 키 전체의 이번 달 추정 사용액 합계(지운 키 포함). 다른 공급자의 키는 섞지 않는다.
pub(crate) fn api_total_used(accounts: &[Account], provider: aam_adapters::api_credit::ApiProvider, removed_usd: f64, now: i64) -> f64 {
    removed_usd + accounts.iter().filter(|account| provider.owns(account)).map(|account| api_credit_used(account, now)).sum::<f64>()
}

/// 이번 달에 금액을 읽지 못한 요청(미계량)이 있는 키가 이 공급자에 하나라도 있는지(지운 키 포함). OpenAI 키는 미계량이 하나라도 있으면 이번 달 더는 쓰지 않는다
/// (`codex exec`는 사용량을 읽을 수 없어 첫 실행부터 미계량이다). Claude 키는 기존대로 이 값으로 막지 않는다.
pub(crate) fn api_unmetered(accounts: &[Account], provider: aam_adapters::api_credit::ApiProvider, removed_unmetered: u64, now: i64) -> bool {
    let period = aam_adapters::api_credit::period_start(now);
    removed_unmetered > 0
        || accounts
            .iter()
            .filter(|account| provider.owns(account))
            .any(|account| account.api_credit.as_ref().is_some_and(|state| state.period_start == period && state.unmetered_requests > 0))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GrantState {
    Upcoming,
    Active,
    Exhausted,
    Expired,
}
impl GrantState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Upcoming => "upcoming",
            Self::Active => "active",
            Self::Exhausted => "exhausted",
            Self::Expired => "expired",
        }
    }
}

/// 사용자가 확인한 API 크레딧 허용 기간을 `now` 기준으로 읽은 값. 저장하지 않고 매번 다시 계산한다.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GrantView {
    pub state: GrantState,
    pub budget: f64,
    pub used: f64,
    pub remaining: f64,
    pub starts_at: i64,
    pub expires_at: i64,
    /// 예산을 확인한 시각과 시작 시각 중 늦은 쪽. 소비 속도(pace) 계산의 기준이다.
    pub effective_start: i64,
    pub unmetered: u64,
}
/// 허용 기간 입력의 복사 가능한 값. 브릿지가 요청마다 `now`로 상태를 다시 계산할 수 있게 한다.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct GrantTerms {
    pub budget_usd: f64,
    pub starts_at: i64,
    pub expires_at: i64,
    pub confirmed_at: i64,
}
impl GrantTerms {
    pub(crate) fn of(policy: &Policy) -> Option<Self> {
        policy.api_credit_grant.as_ref().map(|grant| Self {
            budget_usd: grant.budget_usd,
            starts_at: grant.starts_at,
            expires_at: grant.expires_at,
            confirmed_at: grant.confirmed_at,
        })
    }
}
impl GrantView {
    pub(crate) fn of(policy: &Policy, ledger: &ApiLedger, now: i64) -> Option<Self> {
        GrantTerms::of(policy).map(|terms| Self::at(terms, ledger, now))
    }
    pub(crate) fn at(grant: GrantTerms, ledger: &ApiLedger, now: i64) -> Self {
        let used = if ledger.grant_used_usd.is_finite() { ledger.grant_used_usd.max(0.0) } else { 0.0 };
        let remaining = (grant.budget_usd - used).max(0.0);
        let state = if now >= grant.expires_at {
            GrantState::Expired
        } else if remaining <= 0.0 {
            GrantState::Exhausted
        } else if now < grant.starts_at {
            GrantState::Upcoming
        } else {
            GrantState::Active
        };
        Self {
            state,
            budget: grant.budget_usd,
            used,
            remaining,
            starts_at: grant.starts_at,
            expires_at: grant.expires_at,
            effective_start: grant.starts_at.max(grant.confirmed_at),
            unmetered: ledger.grant_unmetered,
        }
    }
    fn hours_left(&self, now: i64) -> f64 {
        ((self.expires_at - now) as f64 / 3_600_000.0).max(1.0 / 60.0)
    }
    /// (남은 예산 비율) ÷ (남은 기간 비율). 1 이상이면 일정보다 덜 썼다. 확인 직후는 1이다.
    pub(crate) fn pace(&self, now: i64) -> f64 {
        let span = (self.expires_at - self.effective_start).max(1) as f64;
        let time_left = ((self.expires_at - now) as f64 / span).clamp(1e-6, 1.0);
        if self.budget > 0.0 { self.remaining / self.budget / time_left } else { 0.0 }
    }
}

/// 허용 기간이 있으면 진행 중(`Active`)이어야 한도 뒤 API fallback도 열린다. 입력이 없으면 기존 동작 그대로 열려 있다.
pub(crate) fn grant_allows_fallback(policy: &Policy, ledger: &ApiLedger, now: i64) -> Result<(), GrantState> {
    match GrantView::of(policy, ledger, now) {
        None => Ok(()),
        Some(view) if view.state == GrantState::Active => Ok(()),
        Some(view) => Err(view.state),
    }
}

/// 소비 균형 모드에서 API를 구독 대신 선제적으로 쓰는 정책 조건(모드·옵트인·허용 기간·사용량 확인)을 만족하는지.
/// 막힌 이유를 `blockedBy` 코드로 돌려준다. 키 유무·월 상한은 호출자가 이어서 본다.
pub(crate) fn proactive_block(policy: &Policy, view: Option<&GrantView>) -> Option<&'static str> {
    if policy.allocation_mode != AllocationMode::ExpiryBalanced {
        return Some("MODE");
    }
    if !(policy.balance_api_credit && policy.use_api_credit_after_limit) {
        return Some("OPT_IN_OFF");
    }
    let Some(view) = view else { return Some("GRANT_NONE") };
    match view.state {
        GrantState::Upcoming => Some("GRANT_PENDING"),
        GrantState::Expired => Some("GRANT_EXPIRED"),
        GrantState::Exhausted => Some("GRANT_EXHAUSTED"),
        GrantState::Active if view.unmetered > 0 => Some("GRANT_UNMETERED"),
        GrantState::Active => None,
    }
}

/// 결제 정보로 추정 만료일을 만들 수 없는 이유. 월 결제이고 연결 계정이 유효하며 다음 결제일이 미래일 때만 `None`이다.
/// 지난 결제일을 한 달씩 굴려 앞으로 옮기지 않는다(확인하지 않은 갱신을 만들어 내지 않기 위해).
pub(crate) fn billing_estimate_block(billing: &aam_protocol::ApiCreditBilling, accounts: &[Account], now: i64) -> Option<&'static str> {
    if billing.cycle == aam_protocol::BillingCycle::Annual {
        Some("ANNUAL")
    } else if !accounts.iter().any(|account| account.id == billing.account_id && is_billing_account(account)) {
        Some("ACCOUNT_INVALID")
    } else if billing.next_billing_at <= now {
        Some("DATE_PASSED")
    } else {
        None
    }
}
/// 결제 정보를 연결할 수 있는 계정: Anthropic 구독(공식 Claude Code) 계정. API 키·다른 공급자·omp 관측 행은 아니다.
pub(crate) fn is_billing_account(account: &Account) -> bool {
    account.provider == "anthropic" && account.tool == "claude"
}

/// 스냅샷의 허용 기간 상태. 키 유무·월 상한까지 합쳐 선제 사용 가능 여부를 설명한다.
pub(crate) fn grant_status(policy: &Policy, ledger: &ApiLedger, accounts: &[Account], now: i64) -> aam_protocol::ApiCreditGrantStatus {
    let view = GrantView::of(policy, ledger, now);
    let claude = aam_adapters::api_credit::ApiProvider::Anthropic;
    let monthly_total = api_total_used(accounts, claude, ledger.removed_usd, now);
    let monthly_limit_reached = !(monthly_total < policy.api_credit_monthly_limit_usd);
    let has_key = accounts
        .iter()
        .any(|account| claude.owns(account) && account.enabled && account.auth_status == "authenticated");
    let blocked = proactive_block(policy, view.as_ref())
        .or(if !has_key { Some("NO_KEY") } else { None })
        .or(if monthly_limit_reached { Some("MONTHLY_LIMIT") } else { None })
        .or(match &view {
            Some(view) if view.pace(now) < 1.0 => Some("AHEAD_OF_PACE"),
            _ => None,
        });
    let grant = policy.api_credit_grant.as_ref();
    let billing = policy.api_credit_billing.as_ref().map(|billing| {
        let blocked = billing_estimate_block(billing, accounts, now);
        aam_protocol::ApiCreditBillingStatus {
            account_id: billing.account_id.clone(),
            cycle: billing.cycle,
            next_billing_at: billing.next_billing_at,
            estimated_expires_at: blocked.is_none().then_some(billing.next_billing_at),
            estimate_blocked_by: blocked.map(str::to_owned),
        }
    });
    let estimate_drift = grant.is_some_and(|grant| {
        grant.expiry_source == aam_protocol::ExpirySource::BillingEstimate
            && grant.billing_estimate.as_ref().is_none_or(|basis| {
                policy.api_credit_billing.as_ref().is_none_or(|billing| {
                    billing.cycle != aam_protocol::BillingCycle::Monthly
                        || billing.account_id != basis.account_id
                        || billing.next_billing_at != basis.next_billing_at
                })
            })
    });
    aam_protocol::ApiCreditGrantStatus {
        id: grant.map(|grant| grant.id.clone()),
        expiry_source: grant.map_or_else(Default::default, |grant| grant.expiry_source),
        billing_estimate: grant.and_then(|grant| grant.billing_estimate.clone()),
        estimate_drift,
        billing,
        source: "manual".into(),
        state: view.as_ref().map_or("unknown", |view| view.state.as_str()).into(),
        budget_usd: view.as_ref().map(|view| view.budget),
        used_usd: view.as_ref().map_or(0.0, |view| view.used),
        remaining_usd: view.as_ref().map(|view| view.remaining),
        starts_at: view.as_ref().map(|view| view.starts_at),
        expires_at: view.as_ref().map(|view| view.expires_at),
        unmetered_requests: ledger.grant_unmetered,
        unmetered: view.is_some() && ledger.grant_unmetered > 0,
        per_day_usd: view
            .as_ref()
            .filter(|view| view.state == GrantState::Active)
            .map(|view| view.remaining / (view.hours_left(now) / 24.0).max(1.0 / 24.0)),
        monthly_limit_reached,
        proactive: aam_protocol::ApiCreditProactive { allowed: blocked.is_none(), blocked_by: blocked.map(str::to_owned) },
    }
}

/// 한도를 다 썼다는 관측이 지금도 믿을 만한지. 신선하고 리셋 시각이 남아 있어야 한다.
/// 크레딧 fallback과 브릿지가 같은 기준을 쓴다. 리셋 시각이 지났거나 관측이 오래되면 false다.
pub(crate) fn exhausted_observation_fresh(bucket: &QuotaBucket, now: i64, freshness_ms: i64) -> bool {
    bucket.status == "exhausted"
        && bucket.observed_at > 0
        && bucket.observed_at <= now.saturating_add(30_000)
        && now.saturating_sub(bucket.observed_at) <= freshness_ms
        && bucket.resets_at.is_some_and(|reset| reset > now)
}

/// 같은 실제 계정을 본 여러 관측 행(공식 CLI·omp 등)의 한도를 한 목록으로 모을 때, 사라진 5시간 창을 뺀다.
/// 인증된 Ultra·ChatGPT Pro처럼 5시간 창이 없는 계정이거나 플랜이 바뀌어 창이 없어졌다면, 예전 관측 행에 남은 5시간 한도는
/// 더는 존재하지 않는 한도다. 그대로 두면 소진 기록이 계정을 막거나(`exhausted`) 낡은 값이 판정을 `unknown`으로 만든다.
/// 5시간 한도 `b`는 **다른 행**에 (1) 같은 한도 ID가 없고, (2) 같은 범위(공용·같은 모델 전용)에 5시간 한도가 하나도 없으며,
/// (3) 그 범위에 `b`보다 새롭고 신선한 관측(`known`·`exhausted`, 리셋 전)이 있을 때만 사라진 것으로 본다.
/// 같은 ID가 있으면 가장 최근 관측이 이기는 기존 규칙이 처리하고, 새 관측이 5시간 한도를 실패(`unknown`)로 보고하면 그 ID가
/// 있으므로 건드리지 않는다. 긴 주기 한도와 새 관측이 없는 행(정말 관측하지 못한 계정)의 값은 그대로 남는다.
pub(crate) fn without_vanished_short_windows<'a>(rows: &[&'a [QuotaBucket]], now: i64, freshness_ms: i64) -> Vec<&'a QuotaBucket> {
    let live = |bucket: &QuotaBucket| {
        matches!(bucket.status.as_str(), "known" | "exhausted")
            && bucket.observed_at > 0
            && bucket.observed_at <= now.saturating_add(30_000)
            && now.saturating_sub(bucket.observed_at) <= freshness_ms
            && bucket.resets_at.is_none_or(|reset| reset > now)
    };
    let vanished = |row: usize, bucket: &QuotaBucket| {
        short_window(bucket)
            && rows.iter().enumerate().any(|(other, set)| {
                other != row
                    && !set.iter().any(|item| item.id == bucket.id)
                    && !set.iter().any(|item| item.model == bucket.model && short_window(item))
                    && set.iter().any(|item| item.model == bucket.model && live(item) && item.observed_at > bucket.observed_at)
            })
    };
    rows.iter()
        .enumerate()
        .flat_map(|(row, set)| set.iter().filter(move |bucket| !vanished(row, bucket)).collect::<Vec<_>>())
        .collect()
}

/// 크레딧 fallback을 켠 정책에서 이 계정의 크레딧을 쓸 수 있는지. 신선한 관측이 명시적으로 알려 줄 때만 true다.
pub(crate) fn credits_usable(account: &Account, policy: &Policy, now: i64) -> bool {
    policy.use_credits_after_limit
        && account.tool == "codex"
        && account.credits.as_ref().is_some_and(|credits| credits.usable(now, policy.stale_after_seconds))
}

/// 추가 사용량 fallback을 켠 정책에서 이 Claude 계정의 추가 사용량을 쓸 수 있는지.
/// 신선한 관측에서 켜져 있고 상한이 없거나 아직 안 찼을 때만 true다.
pub(crate) fn extra_usage_usable(account: &Account, policy: &Policy, now: i64) -> bool {
    policy.use_extra_usage_after_limit
        && account.tool == "claude"
        && account.extra_usage.as_ref().is_some_and(|extra| extra.usable(now, policy.stale_after_seconds))
}

fn reason(code: &str, text: &str) -> String {
    format!("{code}: {text}")
}

pub(crate) fn canonical_directory(directory: &str) -> Result<PathBuf, ApiError> {
    let path = Path::new(directory);
    if directory.len() > 4096 || directory.contains('\0') || !path.is_absolute() {
        return Err(ApiError::new(
            "INVALID_PROJECT",
            "프로젝트는 있는 폴더의 절대 경로여야 해요.",
        ));
    }
    std::fs::canonicalize(path)
        .ok()
        .filter(|resolved| resolved.is_dir())
        .ok_or_else(|| {
            ApiError::new(
                "INVALID_PROJECT",
                "프로젝트 폴더를 확인하지 못했어요. 폴더 접근 권한과 경로를 확인해 주세요.",
            )
        })
}


/// 계정 id → Claude 공식 설정의 기본 모델(모델, 출처). 파일을 읽으므로 서비스 잠금 밖에서 만든다.
pub type Configured = HashMap<String, (String, String)>;

/// 공식 CLI가 모델을 정하는 요청에서만 계정별 기본 모델 설정을 읽는다.
/// 프로젝트 폴더의 설정 파일은 macOS 권한 확인 등으로 여는 데 오래 걸릴 수 있어, 잠금을 쥔 채 읽지 않는다.
pub fn configured_models(accounts: &[Account], intent: &LaunchIntent) -> Configured {
    if !default_model(&intent.model) {
        return Configured::new();
    }
    let cwd = PathBuf::from(&intent.cwd);
    accounts
        .iter()
        .filter(|account| account.tool == intent.tool)
        .filter_map(|account| {
            aam_adapters::configured_model(&account.tool, &cwd, account.profile_path.as_deref())
                .map(|found| (account.id.clone(), found))
        })
        .collect()
}

/// 같은 실제 계정(도구가 달라도)인지. 확인된 identity_key나 omp 자격 증명 pin이 같아야 한다.
pub(crate) fn same_identity(account: &Account, other: &Account) -> bool {
    other.id == account.id
        || (aam_protocol::pin_provider(&other.provider) == aam_protocol::pin_provider(&account.provider)
            && ((account.identity_key.as_ref().is_some_and(|key| !key.is_empty())
                && other.identity_key == account.identity_key)
                || account
                    .omp_credential_pins
                    .iter()
                    .any(|pin| other.omp_credential_pins.contains(pin))))
}

/// 소비 예측이 없으므로 percentage 부채를 만들지 않고 실제 동시 슬롯만 셉니다.
/// 지운 API 키의 이번 달 사용액은 0으로 본다. 서비스 경로는 원장 값을 넘기는 `decide_with`를 쓴다.
#[cfg(test)]
pub fn decide(
    accounts: &[Account],
    sessions: &[Session],
    policy: &Policy,
    intent: &LaunchIntent,
    now: i64,
) -> Result<Decision, ApiError> {
    decide_with(accounts, sessions, policy, intent, now, &configured_models(accounts, intent), &ApiLedger::default())
}

/// `decide`와 같지만 기본 모델 설정과 API 크레딧 원장(지운 키의 이번 달 사용액·허용 기간 합산 사용액)을 미리 받는다. 서비스 잠금 안에서는 이 함수를 쓴다.
/// 지운 키는 후보가 아니고 월 상한 합계에만 더한다. 허용 기간이 있으면 한도 뒤 API fallback도 그 기간이 진행 중일 때만 열린다.
pub fn decide_with(
    accounts: &[Account],
    sessions: &[Session],
    policy: &Policy,
    intent: &LaunchIntent,
    now: i64,
    configured_models: &Configured,
    ledger: &ApiLedger,
) -> Result<Decision, ApiError> {
    let resolution = crate::routes::resolve(accounts, sessions, policy, intent)?;
    if resolution.mode == aam_protocol::RouteMode::Unmanaged {
        return Err(ApiError::new(
            "UNMANAGED_ROUTE",
            "관리하지 않기로 한 경로에는 사용을 열지 않아요.",
        ));
    }
    let effective = crate::routes::effective(intent, &resolution);
    let intent = &effective;
    let cwd = canonical_directory(&intent.cwd)?;
    let pinned = resolution.account_id.as_deref();
    let manual = pinned.is_some();
    let excluded = resolution
        .excluded_account_id
        .as_ref()
        .and_then(|id| accounts.iter().find(|account| &account.id == id));
    // 공급자 수동 배정: 명시 계정이 없을 때만 쓰며, 그 계정이 입장 조건을 통과하면 먼저 고른다.
    let provider_pin = (!manual)
        .then(|| policy.provider_pins.get(aam_protocol::pin_provider(&intent.tool)))
        .flatten()
        .and_then(|id| accounts.iter().find(|account| &account.id == id));
    let priority_ranks: Option<HashMap<&str, usize>> =
        (!manual && policy.allocation_mode == AllocationMode::Priority).then(|| {
            policy
                .account_priority
                .iter()
                .enumerate()
                .map(|(rank, id)| (id.as_str(), rank))
                .collect()
        });
    // 크레딧(codex)·추가 사용량(claude) fallback 후보: 구독 한도만 막혀 있고 해당 과금 경로가 명시된 계정.
    // 구독 계정이 하나도 없을 때만 쓴다. 두 경로는 단위가 달라 서로 섞지 않는다.
    let credits_on = policy.use_credits_after_limit && !manual && intent.tool == "codex";
    let extra_on = policy.use_extra_usage_after_limit && !manual && intent.tool == "claude";
    let mut fallback_pool: Vec<(&Account, usize, i64, usize, bool)> = Vec::new();
    let mut busy_subscription = false;
    // API 키 fallback 판정: 사용자가 제외하지 않은 구독 계정이 모두 신선한 소진 관측(공용 한도)으로 막혔는지. 공급자마다 옵트인이 따로다.
    let api_provider = ApiProvider::of_tool(&intent.tool).filter(|provider| {
        !manual
            && intent.non_interactive
            && match provider {
                ApiProvider::Anthropic => policy.use_api_credit_after_limit,
                ApiProvider::OpenAi => policy.use_openai_api_after_limit,
            }
    });
    let mut subscription_spent = 0usize;
    let mut subscription_open = false;
    let mut candidates = Vec::new();
    let mut ranked: Vec<(&Account, f64, usize, i64, bool, Option<i64>)> = Vec::new();
    for account in accounts
        .iter()
        .filter(|account| account.tool == intent.tool)
    {
        let group: Vec<&Account> = accounts.iter().filter(|other| same_identity(account, other)).collect();
        let belongs = |session: &&Session| group.iter().any(|other| other.id == session.account_id);
        let active = sessions
            .iter()
            .filter(belongs)
            .filter(|session| holds_capacity(&session.state))
            .count();
        let last = sessions
            .iter()
            .filter(belongs)
            .map(|session| session.started_at)
            .max()
            .unwrap_or(0);
        let group_limit = group
            .iter()
            .filter(|other| other.can_launch)
            .map(|other| other.max_concurrency)
            .filter(|limit| *limit > 0)
            .min()
            .unwrap_or(0);
        let mut exclusions = Vec::new();
        if !project_allowed(policy, &account.id, &cwd) {
            exclusions.push(reason("PROJECT_NOT_ALLOWED", "이 계정에 허용되지 않은 프로젝트예요. 설정의 프로젝트 허용 목록을 확인해 주세요."));
        }
        if excluded.is_some_and(|source| same_identity(source, account)) {
            exclusions.push(reason("CONTINUE_SOURCE", "이어 가기 전에 쓰던 계정이에요."));
        }
        if pinned.is_some_and(|id| id != account.id) {
            exclusions.push(reason("ACCOUNT_PINNED", "지정한 계정만 확인해요."));
        }
        // 공급자 수동 배정 계정은 사용자가 직접 고른 것이므로 자동 배정을 꺼도 쓴다. 다른 계정으로 넘기지는 않는다.
        if !manual
            && !policy.automatic
            && resolution.source == "global"
            && !provider_pin.is_some_and(|pin| same_identity(pin, account))
        {
            exclusions.push(reason(
                "AUTOMATIC_PAUSED",
                "자동 고르기가 잠시 멈췄어요. 계정을 직접 선택해 주세요.",
            ));
        }
        if !account.enabled {
            exclusions.push(reason("ACCOUNT_DISABLED", "새 배정에서 뺀 계정이에요."));
        }
        if account.auth_status != "authenticated" {
            exclusions.push(reason(
                "AUTH_REQUIRED",
                "공식 CLI에서 로그인 상태를 확인해 주세요.",
            ));
        }
        if !account.can_launch
            || account
                .identity_key
                .as_ref()
                .is_none_or(|key| key.is_empty())
            || account.verification != "preflight-verified"
        {
            exclusions.push(reason(
                "ADAPTER_UNVERIFIED",
                "프로필 분리와 실제 계정 확인이 아직 검증되지 않았어요.",
            ));
        }
        if group_limit > 0 && active >= group_limit as usize {
            exclusions.push(reason(
                "CAPACITY_RESERVED",
                "실행 중이거나 시작 결과가 불확실한 대화가 동시 사용 자리를 쓰고 있어요.",
            ));
        }
        let mut applicable = Vec::new();
        let mut model_only = Vec::new();
        for bucket in &account.buckets {
            match scope(bucket, &intent.model) {
                Some(Scope::Applies) => applicable.push(bucket),
                Some(Scope::ModelOnly) => model_only.push(bucket),
                None => (),
            }
        }
        if applicable.is_empty() && !manual {
            exclusions.push(reason(
                "QUOTA_UNKNOWN",
                "요청한 모델의 한도 정보가 없어요.",
            ));
        }
        // 공식 설정에 저장된 기본 모델을 읽어 어떤 모델 전용 한도가 실제로 걸리는지 구분합니다.
        let configured = (!applicable.is_empty() || manual)
            .then(|| {
                default_model(&intent.model)
                    .then(|| configured_models.get(&account.id).cloned())
                    .flatten()
            })
            .flatten();
        let mut deprioritized = false;
        let mut guards = Vec::new();
        let mut in_reserve = false;
        for bucket in &model_only {
            if !(bucket.status == "exhausted"
                || bucket.used_percent.is_some_and(|used| used >= 100.0))
            {
                continue;
            }
            match &configured {
                // 설정된 기본 모델에 걸리는 한도라면 실행은 막지 않고 순위만 뒤로 보냅니다.
                Some((model, source))
                    if scope(bucket, model) == Some(Scope::Applies) =>
                {
                    deprioritized = true;
                    guards.push(reason("MODEL_LIMIT_EXPECTED", &format!(
                        "{source}의 기본 모델 '{model}'에 적용되는 한도 '{}'이(가) 소진되었습니다. 이 계정을 마지막 순위로 두지만 실행은 막지 않습니다. 공식 CLI가 실행 시점에 다른 모델을 선택하면 결과는 달라질 수 있습니다.",
                        bucket.label
                    )));
                }
                _ => guards.push(reason("MODEL_LIMIT_GUARD", &format!(
                    "모델 전용 한도 '{}'이(가) 소진되었습니다. 공식 CLI가 모델을 결정하는 이 요청은 막지 않으며, 해당 모델을 직접 요청하면 제외합니다.",
                    bucket.label
                ))),
            }
        }
        let freshness_ms = policy
            .stale_after_seconds
            .saturating_mul(1000)
            .min(i64::MAX as u64) as i64;
        let mut any_exhausted = false;
        let mut credits_cover = true;
        for bucket in &applicable {
            if bucket.status == "exhausted" || bucket.used_percent.is_some_and(|used| used >= 100.0)
            {
                exclusions.push(reason(
                    "QUOTA_EXHAUSTED",
                    "적용되는 한도를 다 썼어요.",
                ));
                any_exhausted = true;
                credits_cover &= bucket.model.is_none() && exhausted_observation_fresh(bucket, now, freshness_ms);
                continue;
            }
            if policy.safety_reserve_percent > 0.0 && bucket.used_percent.is_some_and(|used| {
                used.is_finite() && (0.0..100.0).contains(&used) && 100.0 - used <= policy.safety_reserve_percent
            }) {
                in_reserve = true;
            }
            if !manual {
                if bucket.status == "stale"
                    || bucket.observed_at <= 0
                    || bucket.observed_at > now.saturating_add(30_000)
                    || now.saturating_sub(bucket.observed_at) > freshness_ms
                {
                    exclusions.push(reason(
                        "QUOTA_STALE",
                        "실제 한도 정보가 오래됐어요. 다시 조회해 주세요.",
                    ));
                } else if bucket.status != "known"
                    || bucket
                        .used_percent
                        .is_none_or(|used| !used.is_finite() || !(0.0..=100.0).contains(&used))
                {
                    exclusions.push(reason(
                        "QUOTA_UNKNOWN",
                        "한도를 확인하지 못해 자동으로 고르지 않아요.",
                    ));
                }
                if bucket.resets_at.is_none_or(|reset| reset <= now) {
                    exclusions.push(reason(
                        "RESET_UNCONFIRMED",
                        "리셋 시각이 없거나 지났어요. 새 한도 정보가 필요해요.",
                    ));
                }
            }
        }
        exclusions.sort();
        exclusions.dedup();
        // 한도는 남았지만 동시 사용 자리만 없는 계정이 있으면 크레딧으로 넘어가지 않고 자리를 기다린다.
        busy_subscription |= exclusions.len() == 1 && exclusions[0].starts_with("CAPACITY_RESERVED:");
        if account.enabled {
            if any_exhausted && credits_cover && exclusions.len() == 1 && exclusions[0].starts_with("QUOTA_EXHAUSTED:") {
                subscription_spent += 1;
            } else {
                subscription_open = true;
            }
        }
        let fallback_kind = (credits_on && credits_usable(account, policy, now))
            .then_some(false)
            .or_else(|| (extra_on && extra_usage_usable(account, policy, now)).then_some(true));
        if let Some(extra) = fallback_kind.filter(|_| {
            any_exhausted && credits_cover && exclusions.len() == 1 && exclusions[0].starts_with("QUOTA_EXHAUSTED:")
        }) {
            fallback_pool.push((account, active, last, candidates.len(), extra));
        }
        let eligible = exclusions.is_empty();
        let score = if eligible {
            // 모델 전용 bucket을 우선하고 공통 bucket은 별도 hard guard로 유지합니다.
            let balanced = policy.allocation_mode == AllocationMode::ExpiryBalanced && !manual;
            let ranking = applicable
                .iter()
                .copied()
                .filter(|bucket| {
                    bucket.resets_at.is_some_and(|reset| reset > now)
                        && bucket.used_percent.is_some_and(f64::is_finite)
                        // 소비 균형은 긴 주기(5시간이 아닌) 한도가 있으면 그것으로만 비교한다. 없으면 기존 선택을 따른다.
                        && (!balanced || !short_window(bucket) || applicable.iter().all(|other| short_window(other)))
                })
                .max_by_key(|bucket| {
                    (
                        bucket.model.is_some(),
                        !short_window(bucket),
                        bucket.resets_at.unwrap_or(0),
                    )
                });
            let drain = ranking.map(|bucket| {
                let hours =
                    ((bucket.resets_at.unwrap() - now) as f64 / 3_600_000.0).max(1.0 / 60.0);
                let usable = (100.0 - bucket.used_percent.unwrap() - policy.safety_reserve_percent).max(0.0);
                if balanced {
                    // 남은 허용량 비율 ÷ 남은 기간 비율(주 168시간 기준). 클수록 덜 쓰고 있어 리셋 전에 쓰는 게 이득이다.
                    (usable / 100.0) / (hours.min(168.0) / 168.0)
                } else {
                    usable / hours
                }
            });
            let pressure = applicable
                .iter()
                .copied()
                .filter(|bucket| short_window(bucket))
                .filter_map(|bucket| bucket.used_percent)
                .fold(1.0_f64, |factor, used| {
                    factor.min(if used >= 85.0 {
                        ((100.0 - used) / 15.0).clamp(0.0, 1.0)
                    } else {
                        1.0
                    })
                });
            // 설정된 기본 모델의 한도가 소진된 계정은 같은 조건의 다른 계정보다 뒤에 둡니다.
            let score = drain.map(|value| if deprioritized { 0.0 } else { value * pressure });
            // 소비 균형에서만: 안전 여유량 안쪽이어도 긴 주기 한도가 곧 리셋되면 새 대화를 먼저 받는다(`use_before_reset`). 그 밖의 모드·명시 계정은 해당 없음.
            let rush = if balanced && in_reserve && !deprioritized {
                use_before_reset(applicable.iter().copied(), policy.safety_reserve_percent, i64::from(policy.expiring_window_hours).saturating_mul(3_600_000), now)
            } else {
                None
            };
            ranked.push((account, score.unwrap_or(0.0), active, last, in_reserve, rush));
            score
        } else {
            None
        };
        if eligible {
            if intent.model == NATIVE_DEFAULT_MODEL {
                exclusions.push("모델은 공식 CLI의 기존 설정을 유지해요. 공통 한도로 입장만 판단하고, 모델 전용 한도는 안내로만 보여요.".into());
            }
            exclusions.push(if manual {
                "고정된 계정만 선택하며 배정 모드와 소비 순서를 적용하지 않습니다. 미확인 사용량은 보장하지 않습니다.".into()
            } else {
                "신선한 관측과 동시 슬롯을 확인했습니다. 안전 여유량은 선택 우선순위에 적용합니다.".into()
            });
            if let Some(ranks) = &priority_ranks {
                exclusions.push(match ranks.get(account.id.as_str()) {
                    Some(rank) => reason(
                        "ALLOCATION_PRIORITY",
                        &format!("소비 순서 {}순위입니다. 사용 가능한 앞선 계정을 먼저 선택합니다.", rank + 1),
                    ),
                    None => reason(
                        "ALLOCATION_UNRANKED",
                        "지정 순서 밖의 계정입니다. 지정 계정이 모두 불가하면 스마트 배정을 적용합니다.",
                    ),
                });
            } else if manual {
            } else if policy.allocation_mode == AllocationMode::ExpiryBalanced {
                exclusions.push(reason(
                    "EXPIRY_BALANCED",
                    "새 대화는 남은 허용량 ÷ 남은 기간이 가장 큰 계정을 먼저 씁니다. 진행 중 대화·고정 계정·안전 여유량·동시 슬롯은 그대로예요.",
                ));
            } else {
                exclusions.push(reason(
                    "ALLOCATION_SMART",
                    "저장된 소비 순서는 사용하지 않고 플랜별 공정 순서와 잔여량·리셋·단기 한도를 비교합니다.",
                ));
            }
            exclusions.push(
                "소비 예측은 미확인입니다. 공급자 quota 예약이 아닌 동시 실행 입장 제어입니다."
                    .into(),
            );
        }
        guards.sort();
        guards.dedup();
        exclusions.append(&mut guards);
        candidates.push(Candidate {
            account_id: account.id.clone(),
            eligible,
            score,
            reasons: exclusions,
        });
    }
    // 하드 제약을 통과한 후보만 비교한다. 명시 pin/재개 제한도 위에서 이미 적용됐다.
    // 리셋 전 사용(소비 균형 한정)으로 여유량을 건너뛴 계정은 roomy와 같은 대우를 받는다.
    let roomy = ranked.iter().any(|entry| !entry.4 || entry.5.is_some());
    for entry in ranked.iter().filter(|entry| entry.4) {
        if let Some(candidate) = candidates.iter_mut().find(|candidate| candidate.account_id == entry.0.id) {
            candidate.reasons.push(if entry.5.is_some() {
                reason("USE_BEFORE_RESET", "안전 여유량 안쪽이지만 긴 주기 한도가 곧 리셋돼 남은 양이 사라집니다. 새 대화는 이 계정을 먼저 사용합니다.")
            } else if roomy {
                reason("RESERVE_DEPRIORITIZED", "안전 여유량 안쪽입니다. 여유 있는 다른 계정을 먼저 사용합니다.")
            } else {
                reason("RESERVE_FALLBACK", "여유 있는 다른 계정이 없어 남은 한도를 사용합니다.")
            });
        }
    }
    if roomy { ranked.retain(|entry| !entry.4 || entry.5.is_some()); }
    let pinned_account = provider_pin.and_then(|pin| {
        ranked
            .iter()
            .find(|entry| same_identity(pin, entry.0))
            .map(|entry| entry.0.id.clone())
    });
    // 모든 입장 조건을 통과한 지정 계정의 순서는 plan 구분보다 먼저 적용합니다.
    let priority_account = pinned_account.clone().or_else(|| priority_ranks.as_ref().and_then(|ranks| {
        ranked
            .iter()
            .filter_map(|entry| ranks.get(entry.0.id.as_str()).map(|rank| (rank, entry.0)))
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, account)| account.id.clone())
    }));
    // 곧 리셋돼 사라질 한도: 옵션을 켠 스마트 배정에서만, 입장 조건을 통과한 계정 중 먼저 고른다.
    // 계정 고정·소비 순서가 정한 계정은 그대로 이긴다.
    let balanced_mode = policy.allocation_mode == AllocationMode::ExpiryBalanced;
    let expiring: Vec<&str> = if policy.expiring_boost && priority_ranks.is_none() && !balanced_mode {
        ranked
            .iter()
            .filter(|entry| {
                let buckets: Vec<&QuotaBucket> = entry.0.buckets.iter().collect();
                crate::quota_summary::expiring(&buckets, policy, now).is_some()
            })
            .map(|entry| entry.0.id.as_str())
            .collect()
    } else {
        Vec::new()
    };
    for id in &expiring {
        if let Some(candidate) = candidates.iter_mut().find(|candidate| candidate.account_id == *id) {
            candidate.reasons.push(reason("EXPIRING_PREFERRED", "곧 리셋될 남은 한도를 먼저 사용합니다."));
        }
    }
    let pool: Vec<_> = if expiring.is_empty() {
        ranked.iter().collect()
    } else {
        ranked.iter().filter(|entry| expiring.contains(&entry.0.id.as_str())).collect()
    };
    let mut selected_account_id = priority_account.or_else(|| {
        // 다른 plan의 percentage는 처리량 비교 근거가 아닙니다. 먼저 plan별 공정 순서를 정합니다.
        // 소비 균형의 점수는 단위 없는 비율(남은 허용량 ÷ 남은 기간)이라 플랜이 달라도 비교할 수 있다. 그래서 플랜별 공정 순서를 건너뛴다.
        let selected_plan = if balanced_mode {
            None
        } else {
            pool.iter()
                .min_by(|a, b| (a.2, a.3, &a.0.plan, &a.0.id).cmp(&(b.2, b.3, &b.0.plan, &b.0.id)))
                .map(|entry| &entry.0.plan)
        };
        pool
            .iter()
            .filter(|entry| balanced_mode || selected_plan.is_some_and(|plan| &entry.0.plan == plan))
            .min_by(|a, b| {
                // 리셋 전 사용 계정은 점수와 무관하게 먼저, 그중 리셋이 가장 빠른 순서다. 해당 없는 후보끼리는 기존 순서 그대로다.
                let rush = |entry: &&&(&Account, f64, usize, i64, bool, Option<i64>)| entry.5.map_or((1, 0), |reset| (0, reset));
                rush(a).cmp(&rush(b))
                    .then(b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal))
                    .then(a.2.cmp(&b.2))
                    .then(a.3.cmp(&b.3))
                    .then(a.0.id.cmp(&b.0.id))
            })
            .map(|entry| entry.0.id.clone())
    });
    // 크레딧 fallback: 구독 한도가 남아 입장 가능한 계정이 하나도 없을 때만, 이번 판정에서 새로 계산해 고른다.
    // 선택 상태를 저장하지 않으므로 리셋·회복 관측이 들어오면 다음 판정부터 곧바로 구독 계정이 이긴다.
    if selected_account_id.is_none() && !busy_subscription {
        let rank = |id: &str| priority_ranks.as_ref().and_then(|ranks| ranks.get(id).copied()).unwrap_or(usize::MAX);
        if let Some((account, _, _, index, extra)) = fallback_pool
            .iter()
            .min_by(|a, b| (rank(&a.0.id), a.1, a.2, &a.0.id).cmp(&(rank(&b.0.id), b.1, b.2, &b.0.id)))
        {
            let candidate = &mut candidates[*index];
            candidate.reasons.retain(|reason| !reason.starts_with("QUOTA_EXHAUSTED:"));
            candidate.reasons.push(if *extra {
                reason(
                    EXTRA_USAGE_FALLBACK,
                    "구독 한도를 다 써서 추가 사용량으로 배정해요. API 요금으로 과금될 수 있고, Ojak은 쓴 금액을 막을 수 없어요.",
                )
            } else {
                reason(
                    CREDITS_FALLBACK,
                    "구독 한도를 다 써서 크레딧으로 배정해요. 크레딧은 OpenAI가 과금할 수 있고, Ojak은 쓴 금액을 볼 수 없어요.",
                )
            });
            candidate.eligible = true;
            candidate.score = Some(0.0);
            selected_account_id = Some(account.id.clone());
        }
    }
    // API 키 fallback: 구독 후보가 하나도 없고(위 두 fallback 포함), 사용자가 제외하지 않은 구독 계정이 전부 신선한
    // 소진 관측으로 막혔을 때만 별도 패스로 고른다. 모델 전용 한도만 소진됐거나 관측이 낡았으면 여기까지 오지 않는다.
    // 이 도구의 공급자 키만 본다(Claude 키는 Codex 실행에, OpenAI 키는 Claude 실행에 쓰이지 않는다).
    // API 키 계정은 위 일반 후보 루프(`account.tool == intent.tool`)에 들어가지 않으므로 어떤 경로로도 일반 후보가 되지 않는다.
    if let (Some(provider), true) = (api_provider, selected_account_id.is_none() && !busy_subscription && subscription_spent > 0 && !subscription_open) {
        let openai = provider == ApiProvider::OpenAi;
        // 공식 CLI가 모델을 정하는 실행은 Claude에서만 단가를 모른 채 허용한다(Claude는 키 실행 뒤 미계량으로 센다). OpenAI 키는 명시한 단가 있는 모델에만 쓴다.
        let priced = (!openai && default_model(&intent.model)) || aam_adapters::api_credit::priced(provider, &intent.model);
        let mut usable: Vec<(&Account, f64)> = Vec::new();
        // 상한은 같은 공급자로 등록한 모든 키의 합계로 본다. 같은 Console·Platform 조직의 키는 크레딧 잔액을 공유하므로 키 수가 상한을 늘리지 않는다.
        let total = api_total_used(accounts, provider, ledger.removed(provider), now);
        let limit_usd = if openai { policy.openai_api_monthly_limit_usd } else { policy.api_credit_monthly_limit_usd };
        // 허용 기간은 Claude 크레딧의 개념이다. OpenAI 키에는 허용 기간·선제 사용이 없다.
        let gate = if openai { Ok(()) } else { grant_allows_fallback(policy, ledger, now) };
        // OpenAI 실행(`codex exec`)은 사용량을 읽을 수 없어 첫 실행부터 미계량이다. 금액을 모르는 사용이 있으면 상한을 지킬 수 없으므로 이번 달 더는 쓰지 않는다.
        let unmetered = openai && api_unmetered(accounts, provider, ledger.openai_removed_unmetered, now);
        for account in accounts.iter().filter(|account| provider.owns(account)) {
            let used = api_credit_used(account, now);
            let mut exclusions = Vec::new();
            if !account.enabled {
                exclusions.push(reason("ACCOUNT_DISABLED", "새 배정에서 뺀 API 키예요."));
            }
            if account.auth_status != "authenticated" {
                exclusions.push(reason("AUTH_REQUIRED", "API 키를 다시 확인해 주세요."));
            }
            if !priced {
                exclusions.push(reason("MODEL_UNPRICED", "단가를 모르는 모델은 API 키로 쓰지 않아요."));
            }
            if !(total < limit_usd) {
                exclusions.push(reason(API_CREDIT_LIMIT, "등록한 API 키 전체의 이번 달 사용액이 상한에 닿았어요."));
            }
            if unmetered {
                exclusions.push(reason(API_UNMETERED, "금액을 읽지 못한 사용이 있어 이번 달에는 이 API 키를 더 쓰지 않아요."));
            }
            if let Err(state) = gate {
                exclusions.push(reason(
                    match state {
                        GrantState::Upcoming => "API_GRANT_PENDING",
                        GrantState::Expired => "API_GRANT_EXPIRED",
                        _ => "API_GRANT_EXHAUSTED",
                    },
                    "확인해 둔 API 크레딧 허용 기간이 진행 중이 아니라서 쓰지 않아요.",
                ));
            }
            if exclusions.is_empty() {
                usable.push((account, used));
            }
            candidates.push(Candidate {
                account_id: account.id.clone(),
                eligible: exclusions.is_empty(),
                score: exclusions.is_empty().then_some(0.0),
                reasons: exclusions,
            });
        }
        if let Some((account, _)) = usable
            .iter()
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal).then(a.0.id.cmp(&b.0.id)))
        {
            let candidate = candidates.iter_mut().find(|candidate| candidate.account_id == account.id).unwrap();
            candidate.reasons.push(reason(
                API_CREDIT_FALLBACK,
                if openai {
                    "Codex 구독 한도를 모두 써서 등록한 OpenAI API 키로 배정해요. OpenAI API 사용은 포함 크레딧 없이 실제 요금이 청구되고, Ojak은 쓴 금액을 막을 수 없어요."
                } else {
                    "구독 한도를 모두 써서 등록한 API 키(월 API 크레딧)로 배정해요. 크레딧이 떨어지면 Console 설정에 따라 API 요금으로 과금될 수 있고, Ojak은 쓴 금액을 막을 수 없어요."
                },
            ));
            selected_account_id = Some(account.id.clone());
        }
    }
    candidates.sort_by(|a, b| a.account_id.cmp(&b.account_id));
    Ok(Decision {
        pin_unavailable: provider_pin.filter(|_| pinned_account.is_none()).map(|pin| pin.id.clone()),
        selected_account_id,
        candidates,
        policy_revision: policy.revision,
    })
}
