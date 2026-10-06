use super::*;
use serde_json::json;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExportRequest {
    redact: Option<bool>,
    from: Option<i64>,
    to: Option<i64>,
}

fn category<'a>(input: &str, allowed: &'a [&'a str]) -> &'a str {
    allowed
        .iter()
        .copied()
        .find(|item| *item == input)
        .unwrap_or("unknown")
}

fn tool(input: &str) -> &str {
    category(input, &["claude", "codex", "omp"])
}

fn provider(input: &str) -> &str {
    category(input, &["anthropic", "openai", "google", "xai"])
}

fn verification(input: &str) -> &str {
    category(
        input,
        &[
            "observed",
            "configured",
            "preflight-verified",
            "request-verified",
        ],
    )
}

fn model_scope(model: Option<&str>) -> &'static str {
    match model {
        None => "shared",
        Some("fable" | "opus" | "sonnet" | "haiku") => "claude-family",
        Some(_) => "model-scoped",
    }
}

pub(crate) fn export(service: &Service, request: ExportRequest) -> Result<Value, ApiError> {
    // 원문을 직렬화한 뒤 지우지 않고 허용된 진단 필드만 새로 구성합니다.
    const MAX_TIMESTAMP: i64 = 253_402_300_799_999;
    if request.redact == Some(false)
        || request
            .from
            .is_some_and(|time| !(0..=MAX_TIMESTAMP).contains(&time))
        || request
            .to
            .is_some_and(|time| !(0..=MAX_TIMESTAMP).contains(&time))
        || matches!((request.from, request.to), (Some(from), Some(to)) if from > to)
    {
        return Err(ApiError::new("INVALID_PARAMS", "진단은 민감정보 제거 상태로만 내보낼 수 있습니다. 올바른 시작·종료 시각을 지정해 주세요."));
    }
    let store = service.lock()?;
    let connection = &store.connection;
    let accounts = accounts(connection)?;
    let policy = policy(connection)?;
    let records = leases(connection)?;
    let mut aliases = BTreeMap::new();
    for account in &accounts {
        let alias = format!("account-{}", aliases.len() + 1);
        aliases.insert(account.id.clone(), alias);
    }
    let sessions: Vec<_> = records.iter()
        .filter(|record| request.from.is_none_or(|from| record.session.started_at >= from)
            && request.to.is_none_or(|to| record.session.started_at <= to))
        .enumerate()
        .map(|(index, record)| {
            let session = &record.session;
            let next = format!("account-{}", aliases.len() + 1);
            let account_alias = aliases.entry(session.account_id.clone()).or_insert(next);
            json!({
                "id": format!("session-{}", index + 1),
                "accountId": account_alias,
                "tool": tool(&session.tool),
                "modelScope": model_scope(Some(&session.model)),
                "state": category(&session.state, &["PREPARED", "STARTING", "ACTIVE", "SUSPECT", "ORPHANED", "EXITED", "ABORTED", "FAILED"]),
                "verification": verification(&session.verification),
                "startedAt": session.started_at,
                "updatedAt": session.updated_at,
                "hasProcess": session.process.is_some(),
                "hasSupervisor": session.supervisor.is_some(),
                "hasNativeSession": session.native_session_id.is_some(),
                "hasExitCode": session.exit_code.is_some(),
                "holdsCapacity": scheduler::holds_capacity(&session.state)
            })
        }).collect();
    let safe_accounts: Vec<_> = accounts.iter().map(|account| {
        let roots = policy.project_allowlist.get(&account.id);
        let buckets: Vec<_> = account.buckets.iter().map(|bucket| json!({
            "modelScope": model_scope(bucket.model.as_deref()),
            "usedPercent": bucket.used_percent.filter(|used| used.is_finite() && (0.0..=100.0).contains(used)),
            "resetsAt": bucket.resets_at,
            "observedAt": bucket.observed_at,
            "status": category(&bucket.status, &["known", "unknown", "stale", "exhausted", "quota-unavailable", "auth-required"])
        })).collect();
        json!({
            "id": aliases.get(&account.id),
            "provider": provider(&account.provider),
            "tool": tool(&account.tool),
            "authStatus": category(&account.auth_status, &["authenticated", "auth-required", "unverified", "error"]),
            "verification": verification(&account.verification),
            "canLaunch": account.can_launch,
            "enabled": account.enabled,
            "maxConcurrency": account.max_concurrency,
            "lastCheckedAt": account.last_checked_at,
            "projectRestricted": roots.is_some(),
            "allowedProjectCount": roots.map(Vec::len),
            "buckets": buckets
        })
    }).collect();
    let safe_tools: Vec<_> = tools(connection)?.iter().map(|status| json!({
        "tool": tool(&status.id),
        "provider": provider(&status.provider),
        "installed": status.installed,
        "hasVersion": status.version.is_some(),
        "isolation": category(&status.isolation, &["verified", "unverified", "unsupported", "requires-policy-review"])
    })).collect();
    Ok(json!({
        "version": PROTOCOL_VERSION,
        "redacted": true,
        "generatedAt": now_ms(),
        "serviceStartedAt": service.started_at,
        "sessionTimeField": "startedAt",
        "from": request.from,
        "to": request.to,
        "accounts": safe_accounts,
        "sessions": sessions,
        "tools": safe_tools,
        "policy": {
            "revision": policy.revision,
            "automatic": policy.automatic,
            "safetyReservePercent": policy.safety_reserve_percent,
            "staleAfterSeconds": policy.stale_after_seconds,
            "providerPinCount": policy.provider_pins.len(),
            "restrictedAccountCount": policy.project_allowlist.len()
        },
        "noticeCount": notices(connection)?.len(),
        "refreshing": service.refreshing.load(Ordering::Acquire),
        "lastRefreshAt": metadata::<i64>(connection, "lastRefreshAt")?
    }))
}
