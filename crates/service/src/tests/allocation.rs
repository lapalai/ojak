use super::*;

#[test]
fn priority_precedes_smart_score_and_plan_partition_while_smart_ignores_saved_order() {
    let now = now_ms();
    let mut smart = account_fixture("smart", now);
    smart.plan = Some("A-plan".into());
    let mut ordered = account_fixture("ordered", now);
    ordered.buckets[0].used_percent = Some(80.0);
    let mut policy = Policy {
        account_priority: vec![ordered.id.clone(), smart.id.clone()],
        ..Policy::default()
    };
    // 같은 플랜의 점수와 서로 다른 플랜의 공정 순서 모두 소비 순서보다 뒤입니다.
    for plan in ["A-plan", "Z-plan"] {
        ordered.plan = Some(plan.into());
        let accounts = [smart.clone(), ordered.clone()];
        policy.allocation_mode = AllocationMode::Priority;
        let priority = scheduler::decide(&accounts, &[], &policy, &intent(), now).unwrap();
        assert_eq!(priority.selected_account_id.as_deref(), Some("ordered"));
        let ordered_candidate = priority
            .candidates
            .iter()
            .find(|c| c.account_id == "ordered")
            .unwrap();
        let smart_candidate = priority
            .candidates
            .iter()
            .find(|c| c.account_id == "smart")
            .unwrap();
        assert!(ordered_candidate.score < smart_candidate.score);
        assert!(ordered_candidate
            .reasons
            .iter()
            .any(|reason| reason.starts_with("ALLOCATION_PRIORITY:")));
        policy.allocation_mode = AllocationMode::Smart;
        let decision = scheduler::decide(&accounts, &[], &policy, &intent(), now).unwrap();
        assert_eq!(decision.selected_account_id.as_deref(), Some("smart"));
        assert!(decision.candidates.iter().all(|candidate| candidate
            .reasons
            .iter()
            .any(|reason| reason.starts_with("ALLOCATION_SMART:"))));
    }
}

#[test]
fn priority_skips_hard_exclusions_before_using_unranked_smart_fallback() {
    let fixture = Fixture::new();
    let now = now_ms();
    let full = account_fixture("full", now);
    fixture.save(&full);
    let grant = fixture.service.acquire(request("held-slot"), now).unwrap();
    let mut exhausted = account_fixture("exhausted", now);
    exhausted.buckets[0].used_percent = Some(100.0);
    let mut disabled = account_fixture("disabled", now);
    disabled.enabled = false;
    let mut stale = account_fixture("stale", now);
    stale.buckets[0].status = "stale".into();
    let restricted = account_fixture("restricted", now);
    let mut unverified = account_fixture("unverified", now);
    unverified.can_launch = false;
    let mut other_tool = account_fixture("other-tool", now);
    other_tool.tool = "codex".into();
    let mut ranked = account_fixture("ranked", now);
    ranked.buckets[0].used_percent = Some(80.0);
    let mut unranked_slow = account_fixture("unranked-slow", now);
    unranked_slow.buckets[0].used_percent = Some(70.0);
    let unranked_fast = account_fixture("unranked-fast", now);
    let mut policy = Policy {
        allocation_mode: AllocationMode::Priority,
        account_priority: vec![
            "full",
            "exhausted",
            "disabled",
            "stale",
            "restricted",
            "unverified",
            "other-tool",
            "ranked",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        ..Policy::default()
    };
    policy.project_allowlist.insert("restricted".into(), vec![]);
    let mut accounts = vec![
        full,
        exhausted,
        disabled,
        stale,
        restricted,
        unverified,
        other_tool,
        ranked,
        unranked_slow,
        unranked_fast,
    ];
    let sessions = [grant.session];
    let decision = scheduler::decide(&accounts, &sessions, &policy, &intent(), now).unwrap();
    assert_eq!(decision.selected_account_id.as_deref(), Some("ranked"));
    for (id, code) in [
        ("full", "CAPACITY_RESERVED:"),
        ("exhausted", "QUOTA_EXHAUSTED:"),
        ("disabled", "ACCOUNT_DISABLED:"),
        ("stale", "QUOTA_STALE:"),
        ("restricted", "PROJECT_NOT_ALLOWED:"),
        ("unverified", "ADAPTER_UNVERIFIED:"),
    ] {
        let candidate = decision
            .candidates
            .iter()
            .find(|candidate| candidate.account_id == id)
            .unwrap();
        assert!(!candidate.eligible);
        assert!(candidate
            .reasons
            .iter()
            .any(|reason| reason.starts_with(code)));
    }
    assert!(!decision
        .candidates
        .iter()
        .any(|candidate| candidate.account_id == "other-tool"));
    accounts
        .iter_mut()
        .find(|account| account.id == "ranked")
        .unwrap()
        .enabled = false;
    let fallback = scheduler::decide(&accounts, &sessions, &policy, &intent(), now).unwrap();
    assert_eq!(
        fallback.selected_account_id.as_deref(),
        Some("unranked-fast")
    );
    let candidate = fallback
        .candidates
        .iter()
        .find(|candidate| candidate.account_id == "unranked-fast")
        .unwrap();
    assert!(candidate
        .reasons
        .iter()
        .any(|reason| reason.starts_with("ALLOCATION_UNRANKED:")));
}

#[test]
fn priority_preserves_pause_explicit_default_and_project_pins_without_fallback() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut accounts = vec![
        account_fixture("first", now),
        account_fixture("pinned", now),
    ];
    let mut policy = Policy {
        allocation_mode: AllocationMode::Priority,
        account_priority: vec!["first".into(), "pinned".into()],
        automatic: false,
        ..Policy::default()
    };
    let mut launch = intent();
    assert!(scheduler::decide(&accounts, &[], &policy, &launch, now)
        .unwrap()
        .selected_account_id
        .is_none());
    launch.account_id = Some("pinned".into());
    assert_eq!(
        scheduler::decide(&accounts, &[], &policy, &launch, now)
            .unwrap()
            .selected_account_id
            .as_deref(),
        Some("pinned")
    );
    launch.account_id = None;
    policy
        .provider_pins
        .insert("anthropic".into(), "pinned".into());
    assert_eq!(
        scheduler::decide(&accounts, &[], &policy, &launch, now)
            .unwrap()
            .selected_account_id
            .as_deref(),
        Some("pinned")
    );
    policy
        .provider_pins
        .insert("anthropic".into(), "first".into());
    launch.cwd = fixture.directory.to_string_lossy().into_owned();
    policy.project_routes.push(ProjectRoute {
        path: std::fs::canonicalize(&fixture.directory)
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        scope: RouteScope::Directory,
        tool: "claude".into(),
        mode: RouteMode::Pinned,
        account_id: Some("pinned".into()),
        model: None,
    });
    assert_eq!(
        scheduler::decide(&accounts, &[], &policy, &launch, now)
            .unwrap()
            .selected_account_id
            .as_deref(),
        Some("pinned")
    );
    accounts[1].enabled = false;
    assert!(scheduler::decide(&accounts, &[], &policy, &launch, now)
        .unwrap()
        .selected_account_id
        .is_none());
}

#[test]
fn changing_priority_does_not_move_existing_lease_replay_or_resume() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    fixture.save(&account_fixture("two", now));
    let original = fixture
        .service
        .acquire(request("original-allocation"), now)
        .unwrap();
    assert_eq!(original.account.id, "one");
    fixture.service.dispatch("policy.update", serde_json::json!({
        "expectedRevision": 1, "allocationMode": "priority", "accountPriority": ["two", "one"]
    })).unwrap();
    let replay = fixture
        .service
        .acquire(request("original-allocation"), now + 1)
        .unwrap();
    assert_eq!(replay.session.id, original.session.id);
    assert_eq!(replay.account.id, "one");
    let fresh = fixture
        .service
        .acquire(request("fresh-allocation"), now + 1)
        .unwrap();
    assert_eq!(fresh.account.id, "two");
    fixture
        .service
        .finish(abort(&original), true, now + 2)
        .unwrap();
    fixture
        .service
        .finish(abort(&fresh), true, now + 2)
        .unwrap();
    let mut resume = request("resume-allocation");
    resume.intent.resume_session_id = Some(original.session.id.clone());
    let resumed = fixture.service.acquire(resume, now + 3).unwrap();
    assert_eq!(resumed.account.id, "one");
    assert_eq!(
        resumed.session.native_session_id,
        original.session.native_session_id
    );
}

#[test]
fn allocation_patch_rejects_invalid_policy_atomically_and_replaces_order_with_cas() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    fixture.save(&account_fixture("two", now));
    let original = fixture.service.snapshot().unwrap().policy;
    for invalid in [
        serde_json::json!({"allocationMode": "ordered"}),
        serde_json::json!({"allocationMode": "priority", "accountPriority": ["one", "one"]}),
        serde_json::json!({"allocationMode": "priority", "accountPriority": ["missing"]}),
        serde_json::json!({"accountPriority": "one"}),
    ] {
        let mut patch = invalid;
        patch["expectedRevision"] = original.revision.into();
        patch["automatic"] = false.into();
        assert!(fixture.service.dispatch("policy.update", patch).is_err());
        assert_eq!(
            serde_json::to_value(fixture.service.snapshot().unwrap().policy).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
    }
    let saved: Policy = serde_json::from_value(fixture.service.dispatch("policy.update", serde_json::json!({
        "expectedRevision": original.revision, "automatic": false,
        "allocationMode": "priority", "accountPriority": ["two", "one"],
        "providerPins": {"anthropic": "one"}, "projectAllowlist": {"one": []},
        "projectRoutes": [{"path": fixture.directory, "scope": "directory", "tool": "claude", "mode": "pinned", "accountId": "one"}]
    })).unwrap()).unwrap();
    assert_eq!(
        fixture
            .service
            .dispatch(
                "policy.update",
                serde_json::json!({
                    "expectedRevision": original.revision, "accountPriority": ["one"]
                })
            )
            .unwrap_err()
            .code,
        "POLICY_CONFLICT"
    );
    let replaced: Policy = serde_json::from_value(fixture.service.dispatch("policy.update", serde_json::json!({
        "expectedRevision": saved.revision, "accountPriority": ["one"], "allocationMode": "smart"
    })).unwrap()).unwrap();
    assert_eq!(replaced.account_priority, vec!["one"]);
    assert_eq!(replaced.allocation_mode, AllocationMode::Smart);
    assert!(!replaced.automatic);
    assert_eq!(replaced.provider_pins, saved.provider_pins);
    assert_eq!(replaced.project_allowlist, saved.project_allowlist);
    assert_eq!(
        serde_json::to_value(&replaced.project_routes).unwrap(),
        serde_json::to_value(&saved.project_routes).unwrap()
    );
    let cleared: Policy = serde_json::from_value(
        fixture
            .service
            .dispatch(
                "policy.update",
                serde_json::json!({
                    "expectedRevision": replaced.revision, "accountPriority": []
                }),
            )
            .unwrap(),
    )
    .unwrap();
    assert!(cleared.account_priority.is_empty());
    let reopened = Service::open(fixture.service.paths.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(reopened.snapshot().unwrap().policy).unwrap(),
        serde_json::to_value(cleared).unwrap()
    );
}

#[test]
fn scan_prunes_deleted_rows_from_priority_but_keeps_rows_with_session_history() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut removed = account_fixture("removed-observation", now);
    removed.tool = "omp".into();
    removed.can_launch = false;
    let mut legacy = account_fixture("legacy-grok", now);
    legacy.tool = "grok".into();
    legacy.provider = "xai".into();
    let mut retained = account_fixture("retained-history", now);
    let missing = account_fixture("missing-scan", now);
    let available = account_fixture("available", now);
    for account in [&removed, &legacy, &retained, &missing, &available] {
        fixture.save(account);
    }
    let mut launch = request("retained-history");
    launch.intent.account_id = Some(retained.id.clone());
    let grant = fixture.service.acquire(launch, now).unwrap();
    fixture
        .service
        .finish(abort(&grant), true, now + 1)
        .unwrap();
    // 세션 이력이 남은 행은 병합 관측이어도 삭제되지 않습니다.
    retained.tool = "omp".into();
    retained.omp_credential_pins = vec![OmpCredentialPin {
        provider: "anthropic".into(),
        hash: "a".repeat(64),
    }];
    fixture.save(&retained);
    fixture.service.dispatch("policy.update", serde_json::json!({
        "expectedRevision": 1, "allocationMode": "priority",
        "accountPriority": [removed.id, legacy.id, retained.id, missing.id, available.id],
        "projectRoutes": [{"path": fixture.directory, "scope": "directory", "tool": "claude", "mode": "pinned", "accountId": available.id}]
    })).unwrap();
    fixture
        .service
        .apply_scan(
            vec![
                removed.clone(),
                legacy,
                retained.clone(),
                missing,
                available.clone(),
            ],
            aam_adapters::ScanResult {
                accounts: vec![available],
                tools: vec![],
                notices: vec![],
                merged_observation_ids: vec![removed.id, retained.id],
            },
        )
        .unwrap();
    let snapshot = fixture.service.snapshot().unwrap();
    assert_eq!(
        snapshot.policy.account_priority,
        vec!["retained-history", "missing-scan", "available"]
    );
    assert!(snapshot
        .accounts
        .iter()
        .all(|account| account.tool != "grok" && account.id != "removed-observation"));
    assert_eq!(
        snapshot.policy.project_routes[0].account_id.as_deref(),
        Some("available")
    );
    let mut launch = intent();
    launch.cwd = fixture.directory.to_string_lossy().into_owned();
    assert_eq!(
        scheduler::decide(
            &snapshot.accounts,
            &snapshot.sessions,
            &snapshot.policy,
            &launch,
            now + 2
        )
        .unwrap()
        .selected_account_id
        .as_deref(),
        Some("available")
    );
}

/// 공급자 수동 배정: 고정 계정이 입장 조건을 통과하면 그 계정, 쓸 수 없으면 자동 배정으로 넘어가고 그 사실을 알린다.
/// 자동 배정을 꺼 두었으면 고정 계정만 쓰고 넘기지 않는다. 옛 도구별 기본 계정은 공급자별로 옮겨 읽는다.
#[test]
fn provider_pin_prefers_the_pinned_account_and_falls_back_when_it_cannot_take_work() {
    let now = now_ms();
    let mut spent = account_fixture("pinned", now);
    let accounts_ok = vec![account_fixture("other", now), account_fixture("pinned", now)];
    let mut policy = Policy::default();
    policy.provider_pins.insert("anthropic".into(), "pinned".into());
    let chosen = scheduler::decide(&accounts_ok, &[], &policy, &intent(), now).unwrap();
    assert_eq!(chosen.selected_account_id.as_deref(), Some("pinned"));
    assert_eq!(chosen.pin_unavailable, None);
    for bucket in &mut spent.buckets {
        bucket.used_percent = Some(100.0);
        bucket.status = "exhausted".into();
    }
    let accounts = vec![account_fixture("other", now), spent];
    let fallback = scheduler::decide(&accounts, &[], &policy, &intent(), now).unwrap();
    assert_eq!(fallback.selected_account_id.as_deref(), Some("other"));
    assert_eq!(fallback.pin_unavailable.as_deref(), Some("pinned"));
    policy.automatic = false;
    let held = scheduler::decide(&accounts, &[], &policy, &intent(), now).unwrap();
    assert_eq!(held.selected_account_id, None);
    assert_eq!(scheduler::decide(&accounts_ok, &[], &policy, &intent(), now).unwrap().selected_account_id.as_deref(), Some("pinned"));
    // Codex 요청에는 Anthropic 고정이 적용되지 않는다.
    let mut codex = intent();
    codex.tool = "codex".into();
    assert_eq!(scheduler::decide(&accounts_ok, &[], &policy, &codex, now).unwrap().pin_unavailable, None);
}

#[test]
fn legacy_tool_preferred_account_is_read_as_a_provider_pin() {
    let fixture = Fixture::new();
    {
        let store = fixture.service.lock().unwrap();
        let mut raw = serde_json::to_value(Policy::default()).unwrap();
        raw.as_object_mut().unwrap().remove("providerPins");
        raw["preferredAccounts"] = serde_json::json!({"claude": "one", "codex": "two"});
        crate::store::set_metadata(&store.connection, "policy", &raw).unwrap();
    }
    let policy = fixture.service.snapshot().unwrap().policy;
    assert_eq!(policy.provider_pins.get("anthropic").map(String::as_str), Some("one"));
    assert_eq!(policy.provider_pins.get("openai").map(String::as_str), Some("two"));
}

#[test]
fn reserve_is_last_resort_not_a_hard_admission_stop() {
    let now = now_ms();
    let mut tight = account_fixture("tight", now);
    tight.buckets[0].used_percent = Some(95.0);
    let mut full = account_fixture("spent", now);
    full.buckets[0].used_percent = Some(100.0);
    let roomy = account_fixture("roomy", now);
    let mut policy = Policy { safety_reserve_percent: 10.0, account_priority: vec!["tight".into()], allocation_mode: AllocationMode::Priority, ..Policy::default() };
    policy.provider_pins.insert(aam_protocol::pin_provider(&tight.tool).into(), tight.id.clone());
    let fallback = scheduler::decide(&[tight.clone(), full.clone()], &[], &policy, &intent(), now).unwrap();
    assert_eq!(fallback.selected_account_id.as_deref(), Some("tight"));
    assert!(fallback.candidates.iter().find(|c| c.account_id == "tight").unwrap().reasons.iter().any(|r| r.starts_with("RESERVE_FALLBACK:")));
    let preferred = scheduler::decide(&[tight.clone(), roomy.clone()], &[], &policy, &intent(), now).unwrap();
    assert_eq!(preferred.selected_account_id.as_deref(), Some("roomy"));
    let pinned = LaunchIntent { account_id: Some(tight.id.clone()), ..intent() };
    assert_eq!(scheduler::decide(&[tight.clone(), roomy], &[], &policy, &pinned, now).unwrap().selected_account_id.as_deref(), Some("tight"));
    for status in ["stale", "unknown", "exhausted"] {
        let mut invalid = tight.clone();
        invalid.buckets[0].status = status.into();
        assert!(scheduler::decide(&[invalid, full.clone()], &[], &policy, &intent(), now).unwrap().selected_account_id.is_none());
    }
    tight.enabled = false;
    assert!(scheduler::decide(&[tight, full], &[], &policy, &intent(), now).unwrap().selected_account_id.is_none());
}

#[test]
fn expiring_weekly_quota_is_reported_and_preferred_only_when_enabled() {
    let now = now_ms();
    // soon: 20시간 뒤 리셋, 여유분 뺀 30% → 시간당 1.5%. later: 50시간 뒤 리셋(기준 밖), 90% → 시간당 1.8%.
    // 기본 스마트 점수로는 later가 이기므로, 옵션만이 선택을 바꾼다.
    let mut soon = account_fixture("soon", now);
    soon.buckets[0].used_percent = Some(60.0);
    soon.buckets[0].resets_at = Some(now + 20 * 3_600_000);
    let mut later = account_fixture("later", now);
    later.buckets[0].used_percent = Some(0.0);
    later.buckets[0].resets_at = Some(now + 50 * 3_600_000);
    let mut policy = Policy { safety_reserve_percent: 10.0, ..Policy::default() };

    let summary = crate::quota_summary::summaries(&[soon.clone()], &policy, &[], now);
    let expiring = summary[0].expiring.as_ref().expect("30% usable within 48h is expiring");
    assert_eq!(expiring.usable_percent, 30.0);
    assert_eq!(expiring.resets_at, now + 20 * 3_600_000);
    // 경계: 기준 미만 잔여량, 기준 밖 리셋, 짧은 한도는 알리지 않는다.
    let mut below = soon.clone();
    below.buckets[0].used_percent = Some(61.0);
    assert!(crate::quota_summary::summaries(&[below], &policy, &[], now)[0].expiring.is_none());
    let mut far = soon.clone();
    far.buckets[0].resets_at = Some(now + 49 * 3_600_000);
    assert!(crate::quota_summary::summaries(&[far], &policy, &[], now)[0].expiring.is_none());
    let mut short = soon.clone();
    short.buckets[0].label = "5시간".into();
    assert!(crate::quota_summary::summaries(&[short], &policy, &[], now)[0].expiring.is_none());

    let off = scheduler::decide(&[soon.clone(), later.clone()], &[], &policy, &intent(), now).unwrap();
    assert_eq!(off.selected_account_id.as_deref(), Some("later"));
    policy.expiring_boost = true;
    let on = scheduler::decide(&[soon.clone(), later.clone()], &[], &policy, &intent(), now).unwrap();
    assert_eq!(on.selected_account_id.as_deref(), Some("soon"));
    assert!(on.candidates.iter().find(|c| c.account_id == "soon").unwrap().reasons.iter().any(|r| r.starts_with("EXPIRING_PREFERRED:")));
    // 소비 순서 모드와 수동 고정은 곧 리셋 우선보다 먼저다.
    let ordered = Policy { allocation_mode: AllocationMode::Priority, account_priority: vec!["later".into()], ..policy.clone() };
    assert_eq!(scheduler::decide(&[soon.clone(), later.clone()], &[], &ordered, &intent(), now).unwrap().selected_account_id.as_deref(), Some("later"));
    let mut pinned = policy.clone();
    pinned.provider_pins.insert(aam_protocol::pin_provider(&later.tool).into(), later.id.clone());
    assert_eq!(scheduler::decide(&[soon, later], &[], &pinned, &intent(), now).unwrap().selected_account_id.as_deref(), Some("later"));
}

#[test]
fn service_quota_summary_distinguishes_reserve_exhaustion_and_stale_evidence() {
    let now = now_ms();
    let policy = Policy { safety_reserve_percent: 10.0, ..Policy::default() };
    let mut account = account_fixture("summary", now);
    account.buckets[0].used_percent = Some(95.0);
    let result = crate::quota_summary::summaries(&[account.clone()], &policy, &[], now);
    assert_eq!(result[0].kind, "reserve");
    assert_eq!(result[0].until, None);
    account.buckets[0].used_percent = Some(100.0);
    assert_eq!(crate::quota_summary::summaries(&[account.clone()], &policy, &[], now)[0].kind, "resting");
    account.buckets[0].resets_at = Some(now - 1);
    assert_eq!(crate::quota_summary::summaries(&[account.clone()], &policy, &[], now)[0].kind, "unknown");
    account.enabled = false;
    assert_eq!(crate::quota_summary::summaries(&[account], &policy, &[], now)[0].kind, "excluded");
}

#[test]
fn quota_summary_shares_latest_evidence_and_preserves_workspace_boundaries() {
    let now = now_ms();
    let policy = Policy::default();
    let mut original = account_fixture("original", now);
    original.identity_key = Some("claude|workspace:one".into());
    original.email = Some("same@example.test".into());
    let mut observed = original.clone();
    observed.id = "observed".into();
    observed.buckets[0].observed_at = now + 1;
    observed.buckets[0].used_percent = Some(100.0);
    let mut separate = original.clone();
    separate.id = "separate".into();
    separate.identity_key = Some("claude|workspace:two".into());
    let summaries = crate::quota_summary::summaries(&[original, observed, separate], &policy, &[], now);
    assert_eq!(summaries.len(), 2);
    let exhausted = summaries.iter().find(|summary| summary.account_ids.contains(&"original".into())).unwrap();
    assert_eq!(exhausted.kind, "resting");
    assert!(exhausted.account_ids.contains(&"observed".into()));
    assert_eq!(summaries.iter().find(|summary| summary.account_ids == ["separate"]).unwrap().kind, "available");
}

#[test]
fn quota_summary_preserves_model_only_quota_and_live_scoped_blocks() {
    let now = now_ms();
    let policy = Policy::default();
    let mut account = account_fixture("models", now);
    account.email = Some("model@example.test".into());
    account.buckets[0].model = Some("gemini".into());
    let mut restricted = account.buckets[0].clone();
    restricted.id = "scoped".into();
    restricted.model = Some("claude-gpt".into());
    restricted.used_percent = Some(100.0);
    account.buckets.push(restricted);
    let result = crate::quota_summary::summaries(&[account.clone()], &policy, &[], now);
    assert_eq!(result[0].kind, "partial");
    assert_eq!(result[0].models, ["Claude·GPT"]);
    let block = crate::bridge::BlockStatus { provider: account.provider.clone(), email: account.email.clone().unwrap(), scope: None, until: now + 60_000, reason: "rate-limit".into() };
    let result = crate::quota_summary::summaries(std::slice::from_ref(&account), &policy, std::slice::from_ref(&block), now);
    assert_eq!(result[0].kind, "resting");
    assert!(result[0].rate);
    assert_eq!(result[0].until, Some(now + 60_000));
    let expired = crate::bridge::BlockStatus { until: now - 1, ..block };
    assert_eq!(crate::quota_summary::summaries(&[account], &policy, &[expired], now)[0].kind, "partial");
}

#[test]
fn reserve_fallback_preserves_auth_project_reset_and_zero_reserve_contracts() {
    let now = now_ms();
    let mut tight = account_fixture("tight", now);
    tight.buckets[0].used_percent = Some(95.0);
    let policy = Policy { safety_reserve_percent: 10.0, ..Policy::default() };
    let mut unauthenticated = tight.clone();
    unauthenticated.auth_status = "auth-required".into();
    assert!(scheduler::decide(&[unauthenticated], &[], &policy, &intent(), now).unwrap().selected_account_id.is_none());
    let mut denied = policy.clone();
    denied.project_allowlist.insert(tight.id.clone(), vec![]);
    assert!(scheduler::decide(std::slice::from_ref(&tight), &[], &denied, &intent(), now).unwrap().selected_account_id.is_none());
    let mut zero = policy.clone();
    zero.safety_reserve_percent = 0.0;
    let decision = scheduler::decide(std::slice::from_ref(&tight), &[], &zero, &intent(), now).unwrap();
    assert_eq!(decision.selected_account_id.as_deref(), Some("tight"));
    assert!(decision.candidates[0].reasons.iter().all(|reason| !reason.starts_with("RESERVE_")));
    tight.buckets[0].resets_at = Some(now - 1);
    assert!(scheduler::decide(&[tight], &[], &policy, &intent(), now).unwrap().selected_account_id.is_none());
}

#[test]
fn summary_global_rate_block_wins_over_reserve_but_not_shared_exhaustion() {
    let now = now_ms();
    let policy = Policy { safety_reserve_percent: 10.0, ..Policy::default() };
    let mut account = account_fixture("rate", now);
    account.email = Some("rate@example.test".into());
    account.buckets[0].used_percent = Some(95.0);
    let block = crate::bridge::BlockStatus { provider: account.provider.clone(), email: account.email.clone().unwrap(), scope: None, until: now + 60_000, reason: "rate-limit".into() };
    let summary = crate::quota_summary::summaries(std::slice::from_ref(&account), &policy, std::slice::from_ref(&block), now);
    assert_eq!(summary[0].kind, "resting");
    assert!(summary[0].rate);
    account.buckets[0].used_percent = Some(100.0);
    let summary = crate::quota_summary::summaries(&[account], &policy, &[block], now);
    assert_eq!(summary[0].kind, "resting");
    assert!(!summary[0].rate);
}

#[test]
fn ambiguous_email_block_does_not_contaminate_distinct_workspaces() {
    let now = now_ms();
    let mut one = account_fixture("one", now);
    one.email = Some("shared@example.test".into());
    one.identity_key = Some("claude|workspace:one".into());
    let mut two = one.clone();
    two.id = "two".into();
    two.identity_key = Some("claude|workspace:two".into());
    let block = crate::bridge::BlockStatus { provider: one.provider.clone(), email: one.email.clone().unwrap(), scope: None, until: now + 60_000, reason: "rate-limit".into() };
    let summaries = crate::quota_summary::summaries(&[one, two], &Policy::default(), &[block], now);
    assert_eq!(summaries.len(), 2);
    assert!(summaries.iter().all(|summary| summary.kind == "available"));
}
