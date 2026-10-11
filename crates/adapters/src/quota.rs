use aam_protocol::{now_ms, CreditsState, ExtraUsageState, QuotaBucket};
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
    identity_from_metadata(&text(report, "provider")?, report.get("metadata")?)
}

pub(crate) fn identity_from_metadata(provider: &str, meta: &Value) -> Option<String> {
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

/// omp 보고서가 말하는 관측 시각. 조회 시각과 헤더 시각 중 더 오래된 값을 쓴다. 없으면 0.
fn report_observed(report: &Value) -> i64 {
    let fetched = millis(report.get("fetchedAt"));
    let headers = millis(
        report
            .get("metadata")
            .and_then(|v| v.get("headersUpdatedAt")),
    );
    match (fetched, headers) {
        (Some(a), Some(b)) => a.min(b),
        (a, b) => a.or(b).unwrap_or(0),
    }
}

/// 추가 사용량(USD 금액) 항목인지. 퍼센트 한도와 섞이지 않게 `omp_buckets`는 이 항목을 건너뛴다.
fn usd_limit(limit: &Value) -> bool {
    limit["amount"]
        .get("unit")
        .and_then(Value::as_str)
        .is_some_and(|unit| unit.eq_ignore_ascii_case("usd"))
}

/// Claude 추가 사용량(`anthropic:extra`, 단위 USD)을 읽는다. omp가 `spend`/`extra_usage` 응답에서 켜져 있고
/// USD일 때만 이 항목을 내보내므로 항목이 있으면 켜진 상태다. 항목이 없으면 `None`(켜졌다고 보지 않는다).
/// 이 Mac에서 켜진 상태는 관측한 적 없고 omp 소스의 필드 형태 기준이다. 금액이 음수·비유한이면 무시한다.
pub(crate) fn omp_extra_usage(report: &Value) -> Option<ExtraUsageState> {
    if text(report, "provider").as_deref() != Some("anthropic") {
        return None;
    }
    let limit = report
        .get("limits")?
        .as_array()?
        .iter()
        .find(|limit| usd_limit(limit) && text(limit, "id").as_deref() == Some("anthropic:extra"))?;
    let amount = &limit["amount"];
    let used = amount.get("used").and_then(Value::as_f64).filter(|v| v.is_finite() && *v >= 0.0)?;
    let cap = match amount.get("limit") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_f64().filter(|v| v.is_finite() && *v > 0.0)?),
    };
    // 상한 항목이 소진 상태로 보고되면 사용한 금액이 아직 상한 아래로 읽혀도 쓸 수 없는 것으로 본다.
    let reached = limit.get("status").and_then(Value::as_str) == Some("exhausted");
    Some(ExtraUsageState {
        enabled: true,
        used_usd: if reached { cap.map_or(used, |cap| used.max(cap)) } else { used },
        limit_usd: cap,
        observed_at: report_observed(report),
    })
}

pub(crate) fn omp_buckets(report: &Value, identity: &str) -> Vec<QuotaBucket> {
    let observed = report_observed(report);
    let provider = text(report, "provider").unwrap_or_else(|| "other".into());
    let mut buckets = Vec::new();
    let Some(limits) = report.get("limits").and_then(Value::as_array) else {
        return buckets;
    };
    for limit in limits {
        // USD 금액은 추가 사용량이지 구독 한도가 아니다. percent 버킷으로 바꾸면 소진 판정이 섞인다.
        if usd_limit(limit) {
            continue;
        }
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

/// 공식 Claude CLI `/usage`에서 얻은 버킷의 출처. `OMP usage` 접두어와 구분한다.
pub(crate) const CLAUDE_USAGE_SOURCE: &str = "Claude /usage · 공식 CLI 조회 시각";

/// `claude -p /usage --output-format stream-json --verbose`가 낸 이벤트에서 구독 한도를 읽는다.
/// 이벤트 계약(설치된 2.1.295 실측): `system/init.apiKeySource`가 `none`(구독 로그인만), 결과 이벤트가
/// `is_error=false`·`num_turns=0`(추론 없음), `assistant.usage_report.rate_limits.limits`가 배열.
/// 네트워크 조회가 실패하면 CLI가 본문 텍스트에는 캐시된 값을 보여 주지만 `limits`는 null이므로,
/// 구조화된 `limits`가 배열일 때만 관측으로 인정한다. 하나라도 어긋나면 빈 목록을 돌려준다(관측 없음).
/// `id_of(upstream_id)`는 같은 한도의 안정적인 버킷 ID를 만드는 호출자 몫이다.
pub(crate) fn claude_usage_buckets(
    events: &[Value],
    observed_at: i64,
    mut id_of: impl FnMut(&str) -> String,
) -> Vec<QuotaBucket> {
    let subscription_only = events.iter().any(|event| {
        event.get("type").and_then(Value::as_str) == Some("system")
            && event.get("subtype").and_then(Value::as_str) == Some("init")
            && event.get("apiKeySource").and_then(Value::as_str) == Some("none")
    });
    let clean_result = events.iter().any(|event| {
        event.get("type").and_then(Value::as_str) == Some("result")
            && event.get("is_error").and_then(Value::as_bool) == Some(false)
            && event.get("num_turns").and_then(Value::as_u64) == Some(0)
    });
    let limits = events.iter().find_map(|event| {
        (event.get("type").and_then(Value::as_str) == Some("assistant")
            && event["local_command_run"]["command"].as_str() == Some("usage"))
        .then(|| event["usage_report"]["rate_limits"]["limits"].as_array())
        .flatten()
    });
    let (true, true, Some(limits)) = (subscription_only, clean_result, limits) else {
        return Vec::new();
    };
    let mut buckets: Vec<QuotaBucket> = Vec::new();
    for limit in limits {
        let Some(used) = limit.get("percent").and_then(Value::as_f64).and_then(percent) else {
            continue;
        };
        let (upstream, label, period, model): (String, String, &str, Option<String>) =
            match limit.get("kind").and_then(Value::as_str) {
                Some("session") => ("anthropic:5h".into(), "Claude 5 Hour".into(), "5시간", None),
                Some("weekly_all") => ("anthropic:7d".into(), "Claude 7 Day".into(), "주간", None),
                Some("weekly_scoped") if limit["scope"]["surface"].is_null() => {
                    let Some(name) = text(&limit["scope"]["model"], "display_name") else {
                        continue;
                    };
                    let model = name.to_ascii_lowercase();
                    if !["fable", "sonnet", "opus", "haiku"].contains(&model.as_str()) {
                        continue;
                    }
                    let label = if model == "fable" {
                        "Fable".to_owned()
                    } else {
                        format!("Claude 7 Day ({name})")
                    };
                    (format!("anthropic:7d:{model}"), label, "주간", Some(model))
                }
                _ => continue,
            };
        let label = format!("{label} · {period}");
        let label = if model.as_deref() == Some("fable") { "Fable 주간".to_owned() } else { label };
        let id = id_of(&upstream);
        if buckets.iter().any(|bucket| bucket.id == id) {
            continue;
        }
        let resets_at = limit
            .get("resets_at")
            .and_then(Value::as_str)
            .and_then(|value| {
                time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
            })
            .and_then(|at| i64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok())
            .filter(|at| *at > 0);
        let status = if resets_at.is_some_and(|reset| reset <= observed_at) {
            "stale"
        } else if used >= 100.0 {
            "exhausted"
        } else {
            "known"
        };
        buckets.push(QuotaBucket {
            id,
            label,
            model,
            used_percent: Some(used),
            resets_at,
            observed_at,
            source: CLAUDE_USAGE_SOURCE.into(),
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

/// 공식 `CreditsSnapshot`(`hasCredits`·`unlimited`·`balance`)을 읽는다. 공급자가 명시한 값만 쓴다.
/// `codex` 한도 묶음의 값을 우선하고, 없으면 단일 `rateLimits`, 그다음 크레딧을 알려 준 다른 묶음 하나를 쓴다.
/// 잔액은 숫자로 읽히는 값만 보관한다. `hasCredits`인데 잔액이 0 이하이면 모순이므로 쓸 수 없다고 본다.
/// 크레딧 정보가 응답에 없으면 `None`이다(있다고 가정하지 않는다).
pub(crate) fn codex_credits(result: &Value, observed_at: i64) -> Option<CreditsState> {
    let by_id = result.get("rateLimitsByLimitId").and_then(Value::as_object);
    let credits = by_id
        .and_then(|map| map.get("codex"))
        .and_then(|snapshot| snapshot.get("credits"))
        .filter(|v| v.is_object())
        .or_else(|| {
            result
                .get("rateLimits")
                .and_then(|snapshot| snapshot.get("credits"))
                .filter(|v| v.is_object())
        })
        .or_else(|| {
            by_id.and_then(|map| {
                map.values()
                    .filter_map(|snapshot| snapshot.get("credits").filter(|v| v.is_object()))
                    .next()
            })
        })?;
    let has_credits = credits.get("hasCredits").and_then(Value::as_bool)?;
    let unlimited = credits.get("unlimited").and_then(Value::as_bool).unwrap_or(false);
    let balance = credits
        .get("balance")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| text.parse::<f64>().is_ok_and(f64::is_finite))
        .map(str::to_owned);
    let empty = balance
        .as_deref()
        .and_then(|text| text.parse::<f64>().ok())
        .is_some_and(|value| value <= 0.0);
    Some(CreditsState {
        available: (has_credits && !empty) || unlimited,
        unlimited,
        balance,
        ordinary_usage_allowed: result.get("ordinaryUsageAllowed").and_then(Value::as_bool),
        observed_at,
    })
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
    fn usage_events(limits: Value) -> Vec<Value> {
        vec![
            json!({"type":"system","subtype":"init","apiKeySource":"none"}),
            json!({"type":"assistant","local_command_run":{"command":"usage","args":""},"usage_report":{"session":{},"rate_limits":{"limits":limits,"extra_usage":{"is_enabled":false}}}}),
            json!({"type":"result","subtype":"success","is_error":false,"num_turns":0}),
        ]
    }
    #[test]
    fn claude_usage_shares_omp_bucket_ids_and_keeps_provider_reset_times() {
        let observed = now_ms();
        let events = usage_events(json!([
            {"kind":"session","group":"session","percent":37,"resets_at":"2099-01-01T05:00:00.000000+00:00","scope":null},
            {"kind":"weekly_all","group":"weekly","percent":100,"resets_at":"2099-01-02T00:00:00+00:00","scope":null},
            {"kind":"weekly_scoped","group":"weekly","percent":40,"resets_at":"2099-01-02T00:00:00+00:00","scope":{"model":{"display_name":"Fable"},"surface":null}},
            {"kind":"weekly_scoped","group":"weekly","percent":10,"resets_at":"2099-01-02T00:00:00+00:00","scope":{"model":null,"surface":"cowork"}},
            {"kind":"session","percent":"not a number"}
        ]));
        let buckets = claude_usage_buckets(&events, observed, |upstream| {
            stable_id(&["omp", "anthropic", "subject|workspace", upstream])
        });
        assert_eq!(buckets.len(), 3);
        let report = json!({"provider":"anthropic","fetchedAt":observed,"limits":[
            {"id":"anthropic:5h","label":"Claude 5 Hour","window":{"id":"5h","durationMs":18000000,"resetsAt":observed+1},"amount":{"usedFraction":0.1},"status":"ok"},
            {"id":"anthropic:7d","label":"Claude 7 Day","window":{"id":"7d","durationMs":604800000,"resetsAt":observed+1},"amount":{"usedFraction":0.1},"status":"ok"},
            {"id":"anthropic:7d:fable","label":"Fable","scope":{"modelId":"fable"},"window":{"id":"7d","durationMs":604800000,"resetsAt":observed+1},"amount":{"usedFraction":0.1},"status":"ok"}
        ]});
        let omp = omp_buckets(&report, "subject|workspace");
        for (native, upstream) in buckets.iter().zip(&omp) {
            assert_eq!(native.id, upstream.id);
            assert_eq!(native.label, upstream.label);
            assert_eq!(native.model, upstream.model);
        }
        assert_eq!(buckets[0].used_percent, Some(37.0));
        assert_eq!(buckets[0].status, "known");
        assert_eq!(buckets[0].observed_at, observed);
        assert_eq!(buckets[0].source, CLAUDE_USAGE_SOURCE);
        assert!(buckets[0].resets_at.is_some_and(|reset| reset > observed));
        assert_eq!(buckets[1].status, "exhausted");
        assert_eq!(buckets[2].model.as_deref(), Some("fable"));
    }
    #[test]
    fn claude_usage_without_structured_limits_or_subscription_proof_is_no_observation() {
        let id = |upstream: &str| upstream.to_owned();
        let limits = json!([{"kind":"session","percent":5,"resets_at":"2099-01-01T05:00:00+00:00","scope":null}]);
        // 네트워크 실패 시 CLI는 캐시된 본문 텍스트만 주고 구조화된 limits는 null이다. 그 값은 관측이 아니다.
        assert!(claude_usage_buckets(&usage_events(json!(null)), now_ms(), id).is_empty());
        let mut api_key = usage_events(limits.clone());
        api_key[0]["apiKeySource"] = json!("ANTHROPIC_API_KEY");
        assert!(claude_usage_buckets(&api_key, now_ms(), id).is_empty());
        let mut errored = usage_events(limits.clone());
        errored[2]["is_error"] = json!(true);
        assert!(claude_usage_buckets(&errored, now_ms(), id).is_empty());
        let mut inferred = usage_events(limits);
        inferred[2]["num_turns"] = json!(1);
        assert!(claude_usage_buckets(&inferred, now_ms(), id).is_empty());
        // 이미 지난 리셋 시각의 값은 새 관측이어도 known으로 두지 않는다.
        let past = usage_events(json!([{"kind":"session","percent":50,"resets_at":"2001-01-01T00:00:00+00:00","scope":null}]));
        assert_eq!(claude_usage_buckets(&past, now_ms(), id)[0].status, "stale");
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
    fn codex_credits_follow_the_official_snapshot_and_stay_conservative() {
        let at = 1_900_000_000_000;
        let shape = |credits: Value| json!({"rateLimits":{"primary":{"usedPercent":100},"credits":credits}});
        let on = codex_credits(&shape(json!({"hasCredits":true,"unlimited":false,"balance":"12.5"})), at).unwrap();
        assert!(on.available && !on.unlimited);
        assert_eq!(on.balance.as_deref(), Some("12.5"));
        assert_eq!(on.observed_at, at);
        let off = codex_credits(&shape(json!({"hasCredits":false,"unlimited":false,"balance":null})), at).unwrap();
        assert!(!off.available && off.balance.is_none());
        let unlimited = codex_credits(&shape(json!({"hasCredits":false,"unlimited":true,"balance":null})), at).unwrap();
        assert!(unlimited.available && unlimited.unlimited);
        // 잔액 0은 쓸 수 없다. 숫자가 아닌 잔액은 보관하지 않지만 공급자의 hasCredits는 그대로 따른다.
        let zero = codex_credits(&shape(json!({"hasCredits":true,"unlimited":false,"balance":"0"})), at).unwrap();
        assert!(!zero.available);
        assert_eq!(zero.balance.as_deref(), Some("0"));
        let garbage = codex_credits(&shape(json!({"hasCredits":true,"unlimited":false,"balance":"약 열두 개"})), at).unwrap();
        assert!(garbage.available && garbage.balance.is_none());
        let nan = codex_credits(&shape(json!({"hasCredits":true,"unlimited":false,"balance":"NaN"})), at).unwrap();
        assert!(nan.balance.is_none());
        // 응답에 크레딧이 없거나 필수 필드가 빠지면 있다고 보지 않는다.
        assert!(codex_credits(&json!({"rateLimits":{"primary":{"usedPercent":1}}}), at).is_none());
        assert!(codex_credits(&shape(json!({"balance":"5"})), at).is_none());
        assert!(codex_credits(&shape(json!(null)), at).is_none());
        // codex 묶음이 단일 묶음보다 우선하고, ordinaryUsageAllowed는 최상위 값을 따른다.
        let both = json!({"ordinaryUsageAllowed":true,"rateLimits":{"credits":{"hasCredits":false,"unlimited":false}},"rateLimitsByLimitId":{"codex":{"credits":{"hasCredits":true,"unlimited":false,"balance":"3"}}}});
        let state = codex_credits(&both, at).unwrap();
        assert!(state.available);
        assert_eq!(state.ordinary_usage_allowed, Some(true));
        assert!(!state.usable(at, 900), "기본 포함 사용량이 허용되는 동안은 크레딧 전환으로 보지 않는다");
    }
    #[test]
    fn claude_extra_usage_is_usd_only_and_never_a_percent_bucket() {
        // omp claude.ts가 내보내는 `anthropic:extra` 항목 형태(소스 기준, 이 Mac에서는 켜진 상태를 관측하지 못했다).
        let observed = now_ms() - 60_000;
        let report = |extra: Value| {
            json!({"provider":"anthropic","fetchedAt":observed,"limits":[
                {"id":"anthropic:7d","label":"Claude 7 Day","scope":{"provider":"anthropic","windowId":"7d"},"window":{"id":"7d","durationMs":604800000,"resetsAt":observed+900000},"amount":{"used":100,"limit":100,"usedFraction":1.0,"unit":"percent"},"status":"exhausted"},
                extra
            ]})
        };
        let capped = report(json!({"id":"anthropic:extra","label":"Claude Extra Usage","scope":{"provider":"anthropic","windowId":"extra"},"amount":{"used":12.4,"unit":"usd","limit":50.0,"remaining":37.6,"usedFraction":0.248,"remainingFraction":0.752},"status":"ok"}));
        let state = omp_extra_usage(&capped).unwrap();
        assert!(state.enabled);
        assert_eq!(state.used_usd, 12.4);
        assert_eq!(state.limit_usd, Some(50.0));
        assert_eq!(state.observed_at, observed);
        assert!(state.usable(now_ms(), 900));
        // USD 항목은 percent 버킷이 되지 않아 소진 판정에 섞이지 않는다.
        let buckets = omp_buckets(&capped, "subject");
        assert_eq!(buckets.len(), 1);
        assert!(buckets.iter().all(|bucket| bucket.label.contains("7 Day")));
        // 상한이 없으면 금액만 있다.
        let open = omp_extra_usage(&report(json!({"id":"anthropic:extra","amount":{"used":3.5,"unit":"usd"}}))).unwrap();
        assert_eq!(open.limit_usd, None);
        assert!(open.usable(now_ms(), 900));
        // 상한에 닿으면(소진 상태) 쓸 수 없다.
        let spent = omp_extra_usage(&report(json!({"id":"anthropic:extra","amount":{"used":50.0,"unit":"usd","limit":50.0,"remaining":0.0,"usedFraction":1.0},"status":"exhausted"}))).unwrap();
        assert!(!spent.usable(now_ms(), 900));
        let reached_but_low = omp_extra_usage(&report(json!({"id":"anthropic:extra","amount":{"used":10.0,"unit":"usd","limit":50.0},"status":"exhausted"}))).unwrap();
        assert!(!reached_but_low.usable(now_ms(), 900));
        // 꺼져 있거나 통화가 USD가 아니면 omp가 항목을 내보내지 않는다. 항목이 없으면 켜졌다고 보지 않는다.
        assert!(omp_extra_usage(&json!({"provider":"anthropic","fetchedAt":observed,"limits":[]})).is_none());
        assert!(omp_extra_usage(&report(json!({"id":"anthropic:extra","amount":{"used":3.0,"unit":"eur"}}))).is_none());
        // 음수·잘못된 값은 무시한다.
        assert!(omp_extra_usage(&report(json!({"id":"anthropic:extra","amount":{"used":-1.0,"unit":"usd"}}))).is_none());
        assert!(omp_extra_usage(&report(json!({"id":"anthropic:extra","amount":{"used":1.0,"unit":"usd","limit":0}}))).is_none());
        // Claude가 아닌 공급자는 대상이 아니다.
        let mut other = capped.clone();
        other["provider"] = json!("openai-codex");
        assert!(omp_extra_usage(&other).is_none());
        // 오래된 관측은 쓸 수 없다.
        assert!(!state.usable(observed + 3_600_000, 900));
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
