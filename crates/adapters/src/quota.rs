use aam_protocol::{now_ms, QuotaBucket};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(crate) fn stable_id(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())[..24].to_owned()
}

pub(crate) fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 512 && !s.chars().any(char::is_control))
        .map(str::to_owned)
}

pub(crate) fn credential_pin(report: &Value) -> Option<aam_protocol::OmpCredentialPin> {
    let provider = text(report, "provider")?;
    let meta = report.get("metadata")?;
    let fields = ["accountId", "email", "orgId", "projectId"];
    let mut values = Vec::with_capacity(5);
    values.push(provider.clone());
    for key in fields {
        values.push(match meta.get(key) {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(value)) if value.is_empty() => String::new(),
            Some(_) => text(meta, key)?,
        });
    }
    if values[1].is_empty() && values[2].is_empty() {
        return None;
    }
    Some(aam_protocol::OmpCredentialPin {
        provider,
        hash: format!("{:x}", Sha256::digest(values.join("\0").as_bytes())),
    })
}

fn percent(value: f64) -> Option<f64> {
    (value.is_finite() && (0.0..=100.0).contains(&value)).then_some(value)
}
fn millis(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_i64).filter(|v| *v > 0)
}

pub(crate) fn provider_id(raw: &str) -> &'static str {
    match raw {
        "anthropic" => "anthropic",
        "openai" | "openai-codex" => "openai",
        "google" | "google-antigravity" | "google-gemini-cli" => "google",
        "xai" | "xai-oauth" => "xai",
        _ => "other",
    }
}

pub(crate) fn report_identity(report: &Value) -> Option<String> {
    let meta = report.get("metadata")?;
    let provider = text(report, "provider")?;
    let subject = text(meta, "accountId").or_else(|| text(meta, "subject"));
    let workspace = text(meta, "orgId")
        .or_else(|| text(meta, "organizationId"))
        .or_else(|| text(meta, "projectId"));
    let email = text(meta, "email").map(|v| v.to_lowercase());
    match (subject, workspace, email) {
        (Some(subject), workspace, _) => Some(format!(
            "{provider}|subject:{subject}|workspace:{}",
            workspace.unwrap_or_default()
        )),
        (None, Some(workspace), Some(email)) => {
            Some(format!("{provider}|email:{email}|workspace:{workspace}"))
        }
        _ => None,
    }
}

pub(crate) fn omp_buckets(report: &Value, identity: &str) -> Vec<QuotaBucket> {
    let fetched = millis(report.get("fetchedAt"));
    let headers = millis(
        report
            .get("metadata")
            .and_then(|v| v.get("headersUpdatedAt")),
    );
    let observed = match (fetched, headers) {
        (Some(a), Some(b)) => a.min(b),
        (a, b) => a.or(b).unwrap_or(0),
    };
    let provider = text(report, "provider").unwrap_or_else(|| "other".into());
    let mut buckets = Vec::new();
    let Some(limits) = report.get("limits").and_then(Value::as_array) else {
        return buckets;
    };
    for limit in limits {
        let Some(upstream_id) = text(limit, "id") else {
            continue;
        };
        let amount = &limit["amount"];
        let used = amount
            .get("usedFraction")
            .and_then(Value::as_f64)
            .and_then(|v| percent(v * 100.0))
            .or_else(|| {
                amount
                    .get("remainingFraction")
                    .and_then(Value::as_f64)
                    .and_then(|v| percent((1.0 - v) * 100.0))
            })
            .or_else(|| {
                let used = amount.get("used").and_then(Value::as_f64)?;
                let total = amount.get("limit").and_then(Value::as_f64)?;
                (total > 0.0 && total.is_finite())
                    .then(|| used / total * 100.0)
                    .and_then(percent)
            });
        let raw_label = text(limit, "label").unwrap_or_else(|| "사용 한도".into());
        let model = text(&limit["scope"], "modelId")
            .or_else(|| text(&limit["scope"], "model"))
            .or_else(|| {
                let hint = format!("{upstream_id} {raw_label}").to_ascii_lowercase();
                ["fable", "sonnet", "opus", "haiku"]
                    .iter()
                    .find(|name| hint.contains(**name))
                    .map(|s| (*s).to_owned())
            });
        let window_id = text(&limit["window"], "id").or_else(|| text(&limit["scope"], "windowId"));
        let duration = limit["window"]["durationMs"].as_i64();
        let period = match (window_id.as_deref(), duration) {
            (Some("5h"), _) | (_, Some(18_000_000)) => Some("5시간"),
            (Some("7d" | "1w" | "weekly"), _) | (_, Some(604_800_000)) => Some("주간"),
            _ => None,
        };
        let label = if model
            .as_deref()
            .is_some_and(|v| v.to_lowercase().contains("fable"))
        {
            format!("Fable {}", period.unwrap_or("모델 한도"))
        } else if let Some(period) = period {
            format!("{raw_label} · {period}")
        } else {
            raw_label
        };
        let resets_at = millis(limit.get("window").and_then(|v| v.get("resetsAt")));
        let upstream_status = limit.get("status").and_then(Value::as_str).unwrap_or("ok");
        let failed = matches!(
            upstream_status,
            "unknown" | "error" | "unavailable" | "auth-required"
        );
        let used = if failed { None } else { used };
        let stale = observed == 0
            || now_ms().saturating_sub(observed) > 900_000
            || observed > now_ms() + 60_000
            || resets_at.is_some_and(|t| t <= now_ms());
        let status = if used.is_none() {
            "unknown"
        } else if stale {
            "stale"
        } else if used.is_some_and(|p| p >= 100.0) || upstream_status == "exhausted" {
            "exhausted"
        } else {
            "known"
        };
        buckets.push(QuotaBucket {
            // shared=true는 계정 내부 모델 공유일 수 있습니다. 계정 간 동일 pool로 추정하지 않습니다.
            id: stable_id(&["omp", &provider, identity, &upstream_id]),
            label,
            model,
            used_percent: used,
            resets_at,
            observed_at: observed,
            source: format!("OMP usage / {provider} · 공유 관계 미확정"),
            status: status.into(),
        });
    }
    buckets
}

pub(crate) fn codex_buckets(result: &Value, identity: &str, observed_at: i64) -> Vec<QuotaBucket> {
    let mut buckets = Vec::new();
    let mut scopes = Vec::new();
    if let Some(by_id) = result.get("rateLimitsByLimitId").and_then(Value::as_object) {
        scopes.extend(by_id.iter().map(|(id, data)| (id.as_str(), data)));
    }
    if scopes.is_empty() {
        if let Some(data) = result.get("rateLimits").filter(|v| v.is_object()) {
            scopes.push(("codex", data));
        }
    }
    for (scope, snapshot) in scopes {
        for window in ["primary", "secondary"] {
            let Some(data) = snapshot.get(window).filter(|v| v.is_object()) else {
                continue;
            };
            let used = data
                .get("usedPercent")
                .and_then(Value::as_f64)
                .and_then(percent);
            let resets_at = data
                .get("resetsAt")
                .and_then(Value::as_i64)
                .and_then(|v| v.checked_mul(1000))
                .filter(|v| *v > 0);
            let minutes = data.get("windowDurationMins").and_then(Value::as_i64);
            let label = match minutes {
                Some(300) => "5시간".into(),
                Some(10080) => "주간".into(),
                Some(m) => format!("{m}분 한도"),
                None => format!("Codex {window}"),
            };
            let model =
                text(snapshot, "limitName").filter(|s| s.to_ascii_lowercase().starts_with("gpt-"));
            let status = if used.is_none() {
                "unknown"
            } else if resets_at.is_some_and(|v| v <= observed_at) {
                "stale"
            } else if used.is_some_and(|v| v >= 100.0) {
                "exhausted"
            } else {
                "known"
            };
            buckets.push(QuotaBucket {
                id: stable_id(&["codex", identity, scope, window]),
                label,
                model,
                used_percent: used,
                resets_at,
                observed_at,
                source: "Codex app-server account/rateLimits/read · 조회 응답 시각".into(),
                status: status.into(),
            });
        }
    }
    buckets
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn credential_digest_preserves_provider_raw_case_and_every_scope() {
        let base = json!({"provider":"openai-codex","metadata":{"accountId":"acct","email":"Name@Example.test","orgId":"org","projectId":"project"}});
        let pin = credential_pin(&base).unwrap();
        assert_eq!(
            pin.hash,
            format!(
                "{:x}",
                Sha256::digest(b"openai-codex\0acct\0Name@Example.test\0org\0project")
            )
        );
        for (field, replacement) in [
            ("accountId", "other"),
            ("email", "name@example.test"),
            ("orgId", "other"),
            ("projectId", "other"),
        ] {
            let mut changed = base.clone();
            changed["metadata"][field] = json!(replacement);
            assert_ne!(credential_pin(&changed).unwrap().hash, pin.hash);
        }
        let mut provider = base.clone();
        provider["provider"] = json!("openai");
        assert_ne!(credential_pin(&provider).unwrap().hash, pin.hash);
        assert!(credential_pin(
            &json!({"provider":"anthropic","metadata":{"orgId":"org","projectId":"project"}})
        )
        .is_none());
        assert!(credential_pin(
            &json!({"provider":"anthropic","metadata":{"email":"Name@Example.test"}})
        )
        .is_some());
    }
    #[test]
    fn real_usage_report_shape_keeps_source_time_and_fable_meter() {
        let observed = now_ms() - 120_000;
        let report = json!({"provider":"anthropic","fetchedAt":observed,"limits":[{"id":"anthropic:7d_fable","label":"Claude Fable 7 Day","scope":{"provider":"anthropic","windowId":"7d","shared":true},"window":{"id":"7d","durationMs":604800000,"resetsAt":observed+900000},"amount":{"used":31,"limit":100,"remaining":69,"usedFraction":0.31,"unit":"percent"},"status":"ok"}]});
        let buckets = omp_buckets(&report, "subject-a|workspace-a");
        assert_eq!(buckets[0].used_percent, Some(31.0));
        assert_eq!(buckets[0].observed_at, observed);
        assert_eq!(buckets[0].model.as_deref(), Some("fable"));
        assert_ne!(
            buckets[0].id,
            omp_buckets(&report, "subject-b|workspace-b")[0].id
        );
    }
    #[test]
    fn missing_observation_never_becomes_fresh_zero() {
        let report = json!({"provider":"anthropic","generatedAt":now_ms(),"limits":[{"id":"anthropic:7d","status":"ok","amount":{}}]});
        let bucket = &omp_buckets(&report, "account")[0];
        assert_eq!(bucket.used_percent, None);
        assert_eq!(bucket.observed_at, 0);
        assert_eq!(bucket.status, "unknown");
    }
    #[test]
    fn codex_seconds_become_milliseconds_and_maps_do_not_duplicate_legacy() {
        let report = json!({"rateLimits":{"primary":{"usedPercent":1}},"rateLimitsByLimitId":{"codex":{"primary":{"usedPercent":73,"windowDurationMins":300,"resetsAt":1999999999}}}});
        let buckets = codex_buckets(&report, "subject", 1_900_000_000_000);
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].resets_at, Some(1_999_999_999_000));
        assert_eq!(buckets[0].used_percent, Some(73.0));
    }
    #[test]
    fn email_alone_is_not_a_provider_identity() {
        assert!(report_identity(
            &json!({"provider":"anthropic","metadata":{"email":"example@invalid.test"}})
        )
        .is_none());
        assert!(report_identity(&json!({"provider":"anthropic","metadata":{"email":"example@invalid.test","orgId":"workspace-a"}})).is_some());
    }
}
