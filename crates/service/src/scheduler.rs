use aam_protocol::{
    Account, AllocationMode, ApiError, Candidate, Decision, LaunchIntent, Policy, QuotaBucket,
    Session, NATIVE_DEFAULT_MODEL,
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

fn project_allowed(policy: &Policy, account_id: &str, cwd: &Path) -> bool {
    policy
        .project_allowlist
        .get(account_id)
        .is_none_or(|roots| {
            roots.iter().any(|root| {
                let path = Path::new(root);
                // 저장 이후 허용 루트가 다른 위치를 가리키도록 바뀌면 다시 승인을 받아야 합니다.
                canonical_directory(root)
                    .is_ok_and(|resolved| resolved == path && cwd.starts_with(&resolved))
            })
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
pub fn decide(
    accounts: &[Account],
    sessions: &[Session],
    policy: &Policy,
    intent: &LaunchIntent,
    now: i64,
) -> Result<Decision, ApiError> {
    decide_with(accounts, sessions, policy, intent, now, &configured_models(accounts, intent))
}

/// `decide`와 같지만 기본 모델 설정을 미리 읽어 받는다. 서비스 잠금 안에서는 이 함수를 쓴다.
pub fn decide_with(
    accounts: &[Account],
    sessions: &[Session],
    policy: &Policy,
    intent: &LaunchIntent,
    now: i64,
    configured_models: &Configured,
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
    let mut candidates = Vec::new();
    let mut ranked: Vec<(&Account, f64, usize, i64, bool)> = Vec::new();
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
            .min()
            .unwrap_or(account.max_concurrency);
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
        if active >= group_limit as usize {
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
        for bucket in &applicable {
            if bucket.status == "exhausted" || bucket.used_percent.is_some_and(|used| used >= 100.0)
            {
                exclusions.push(reason(
                    "QUOTA_EXHAUSTED",
                    "적용되는 한도를 다 썼어요.",
                ));
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
        let eligible = exclusions.is_empty();
        let score = if eligible {
            // 모델 전용 bucket을 우선하고 공통 bucket은 별도 hard guard로 유지합니다.
            let ranking = applicable
                .iter()
                .copied()
                .filter(|bucket| {
                    bucket.resets_at.is_some_and(|reset| reset > now)
                        && bucket.used_percent.is_some_and(f64::is_finite)
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
                (100.0 - bucket.used_percent.unwrap() - policy.safety_reserve_percent).max(0.0)
                    / hours
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
            ranked.push((account, score.unwrap_or(0.0), active, last, in_reserve));
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
            } else if !manual {
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
    let roomy = ranked.iter().any(|entry| !entry.4);
    for entry in ranked.iter().filter(|entry| entry.4) {
        if let Some(candidate) = candidates.iter_mut().find(|candidate| candidate.account_id == entry.0.id) {
            candidate.reasons.push(if roomy {
                reason("RESERVE_DEPRIORITIZED", "안전 여유량 안쪽입니다. 여유 있는 다른 계정을 먼저 사용합니다.")
            } else {
                reason("RESERVE_FALLBACK", "여유 있는 다른 계정이 없어 남은 한도를 사용합니다.")
            });
        }
    }
    if roomy { ranked.retain(|entry| !entry.4); }
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
    let expiring: Vec<&str> = if policy.expiring_boost && priority_ranks.is_none() {
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
    let selected_account_id = priority_account.or_else(|| {
        // 다른 plan의 percentage는 처리량 비교 근거가 아닙니다. 먼저 plan별 공정 순서를 정합니다.
        let selected_plan = pool
            .iter()
            .min_by(|a, b| (a.2, a.3, &a.0.plan, &a.0.id).cmp(&(b.2, b.3, &b.0.plan, &b.0.id)))
            .map(|entry| &entry.0.plan);
        pool
            .iter()
            .filter(|entry| selected_plan.is_some_and(|plan| &entry.0.plan == plan))
            .min_by(|a, b| {
                b.1.partial_cmp(&a.1)
                    .unwrap_or(Ordering::Equal)
                    .then(a.2.cmp(&b.2))
                    .then(a.3.cmp(&b.3))
                    .then(a.0.id.cmp(&b.0.id))
            })
            .map(|entry| entry.0.id.clone())
    });
    candidates.sort_by(|a, b| a.account_id.cmp(&b.account_id));
    Ok(Decision {
        pin_unavailable: provider_pin.filter(|_| pinned_account.is_none()).map(|pin| pin.id.clone()),
        selected_account_id,
        candidates,
        policy_revision: policy.revision,
    })
}
