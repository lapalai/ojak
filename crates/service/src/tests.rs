mod allocation;
mod routing;
mod takeover;
use super::*;
use std::{path::PathBuf, sync::Barrier};

struct Fixture {
    service: Arc<Service>,
    directory: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let directory = std::env::temp_dir().join(format!("aam-service-test-{}", new_id()));
        std::fs::create_dir_all(&directory).unwrap();
        let paths = Paths {
            home: directory.clone(),
            database: directory.join("state.sqlite3"),
            socket: directory.join("control.sock"),
            profiles: directory.join("profiles"),
        };
        Self {
            service: Service::open(paths).unwrap(),
            directory,
        }
    }
    fn save(&self, account: &Account) {
        save_account(&self.service.lock().unwrap().connection, account).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
fn bucket(id: &str, model: Option<&str>, used: f64, reset: i64, now: i64) -> QuotaBucket {
    QuotaBucket {
        id: id.into(),
        label: id.into(),
        model: model.map(str::to_owned),
        used_percent: Some(used),
        resets_at: Some(reset),
        observed_at: now,
        source: "regression-fixture".into(),
        status: "known".into(),
    }
}
fn account_fixture(id: &str, now: i64) -> Account {
    Account {
        id: id.into(),
        provider: "anthropic".into(),
        tool: "claude".into(),
        label: id.into(),
        identity_key: Some(format!("identity-{id}")),
        profile_path: Some(format!("/isolated/{id}")),
        binary_path: Some("/usr/bin/true".into()),
        auth_status: "authenticated".into(),
        verification: "preflight-verified".into(),
        can_launch: true,
        enabled: true,
        max_concurrency: 1,
        plan: Some("Max".into()),
        buckets: vec![bucket("weekly", None, 30.0, now + 86_400_000, now)],
        last_checked_at: now,
        ..Default::default()
    }
}
/// 실제로 있는 절대 디렉터리. macOS는 기존처럼 `/tmp`, Windows는 사용자 임시 폴더다.
fn test_cwd() -> String {
    if cfg!(windows) {
        std::env::temp_dir().to_string_lossy().trim_end_matches('\\').to_owned()
    } else {
        "/tmp".into()
    }
}

fn intent() -> LaunchIntent {
    LaunchIntent {
        tool: "claude".into(),
        model: "claude-fable-5-1".into(),
        cwd: test_cwd(),
        ..Default::default()
    }
}

#[test]
fn relogin_of_same_identity_supersedes_other_profile_binding() {
    let now = now_ms();
    let old = account_fixture("old", now);
    let mut renewed = account_fixture("renewed", now);
    renewed.identity_key = old.identity_key.clone();
    // 만료 후 새 프로필로 다시 로그인한 같은 계정은 기존 행을 대체합니다.
    assert!(supersedes(&renewed, &old));
    // 비활성화해 둔 중복도 같은 계정이므로 흡수 대상입니다.
    let mut disabled = old.clone();
    disabled.enabled = false;
    disabled.can_launch = false;
    assert!(supersedes(&renewed, &disabled));
}

#[test]
fn supersede_is_limited_to_same_tool_and_identity() {
    let now = now_ms();
    let old = account_fixture("old", now);
    let other = account_fixture("other", now);
    assert!(!supersedes(&other, &old), "다른 계정은 공존해야 합니다");
    let mut codex = account_fixture("codex", now);
    codex.tool = "codex".into();
    codex.identity_key = old.identity_key.clone();
    assert!(!supersedes(&codex, &old), "도구가 다르면 별개 binding입니다");
    let mut unverified = account_fixture("unverified", now);
    unverified.identity_key = None;
    assert!(!supersedes(&unverified, &old), "identity 없는 등록은 다른 프로필을 흡수하지 않습니다");
    // 같은 프로필에 다른 identity가 있던 경우는 이력 보존 대상입니다.
    let mut switched = account_fixture("switched", now);
    switched.profile_path = old.profile_path.clone();
    assert!(!supersedes(&switched, &old));
}
fn request(id: &str) -> Acquire {
    Acquire {
        request_id: id.into(),
        client_instance_id: format!("client-{id}"),
        intent: intent(),
        parent_capability: None,
    }
}
fn abort(grant: &LeaseGrant) -> LeaseAction {
    LeaseAction {
        session_id: grant.session.id.clone(),
        capability: grant.capability.clone(),
        exit_code: None,
        reason: None,
        session_persisted: None,
        background_processes: None,
        native_session_id: None,
    }
}

#[test]
fn concurrent_clients_cannot_reserve_the_same_last_slot() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for id in ["left", "right"] {
        let service = Arc::clone(&fixture.service);
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            service.acquire(request(id), now)
        }));
    }
    barrier.wait();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let loser = results
        .iter()
        .find_map(|result| result.as_ref().err())
        .unwrap();
    assert_eq!(loser.code, "NO_ELIGIBLE_ACCOUNT");
    assert_eq!(
        fixture
            .service
            .snapshot()
            .unwrap()
            .sessions
            .iter()
            .filter(|session| scheduler::holds_capacity(&session.state))
            .count(),
        1
    );
}

#[test]
fn request_replay_is_persistent_and_returns_current_terminal_state() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let first = fixture.service.acquire(request("stable"), now).unwrap();
    let repeated = fixture.service.acquire(request("stable"), now + 1).unwrap();
    assert_eq!(first.session.id, repeated.session.id);
    assert_eq!(first.capability, repeated.capability);
    let mut changed = request("stable");
    changed.intent.model = "claude-sonnet-4".into();
    assert_eq!(
        fixture.service.acquire(changed, now + 1).unwrap_err().code,
        "IDEMPOTENCY_CONFLICT"
    );
    fixture
        .service
        .finish(abort(&first), true, now + 2)
        .unwrap();
    let reopened = Service::open(fixture.service.paths.clone()).unwrap();
    let persisted = reopened.acquire(request("stable"), now + 3).unwrap();
    assert_eq!(persisted.session.id, first.session.id);
    assert_eq!(persisted.session.state, "ABORTED");
    assert_eq!(persisted.capability, first.capability);
    assert!(reopened.acquire(request("new-attempt"), now + 4).is_ok());
}

#[test]
fn concurrent_replay_creates_one_reservation_and_one_capability() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = (0..2)
        .map(|_| {
            let service = Arc::clone(&fixture.service);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                service.acquire(request("same-request"), now).unwrap()
            })
        })
        .collect();
    barrier.wait();
    let grants: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(grants[0].session.id, grants[1].session.id);
    assert_eq!(grants[0].capability, grants[1].capability);
    assert_eq!(fixture.service.snapshot().unwrap().sessions.len(), 1);
}

#[cfg(any(target_os = "macos", windows))]
fn starting_request(grant: &LeaseGrant, now: i64) -> Starting {
    Starting {
        session_id: grant.session.id.clone(),
        capability: grant.capability.clone(),
        generation: grant.session.generation.clone(),
        spawn_attempt_id: "one-spawn-attempt".into(),
        supervisor: process_identity(std::process::id()).unwrap(),
        evidence: IdentityEvidence {
            identity_key: grant.account.identity_key.clone().unwrap(),
            tier: "preflight-verified".into(),
            observed_at: now,
        },
    }
}

#[cfg(target_os = "macos")]
#[test]
fn starting_and_prepared_expiry_share_a_fence() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let grant = fixture.service.acquire(request("racing"), now).unwrap();
    let expiration = now + fixture.service.prepared_ms;
    let barrier = Arc::new(Barrier::new(3));
    let service = Arc::clone(&fixture.service);
    let start_barrier = Arc::clone(&barrier);
    let starting = starting_request(&grant, expiration - 1);
    let start_worker = std::thread::spawn(move || {
        start_barrier.wait();
        service.starting(starting, expiration - 1)
    });
    let service = Arc::clone(&fixture.service);
    let expiry_barrier = Arc::clone(&barrier);
    let expiry_worker = std::thread::spawn(move || {
        expiry_barrier.wait();
        let mut store = service.lock().unwrap();
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        expire(&tx, expiration).unwrap();
        tx.commit().unwrap();
    });
    barrier.wait();
    let started = start_worker.join().unwrap();
    expiry_worker.join().unwrap();
    let state = fixture
        .service
        .acquire(request("racing"), expiration + 1)
        .unwrap()
        .session
        .state;
    match started {
        Ok(session) => {
            assert_eq!(session.state, "STARTING");
            assert_eq!(state, "STARTING");
            assert!(fixture
                .service
                .acquire(request("cannot-steal"), expiration + 1)
                .is_err());
        }
        Err(error) => {
            assert_eq!(error.code, "LEASE_NOT_PREPARED");
            assert_eq!(state, "ABORTED");
            assert!(fixture
                .service
                .acquire(request("new-after-expiry"), expiration + 1)
                .is_ok());
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn expired_grant_never_receives_start_permission_and_unknown_spawn_keeps_slot() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let expired = fixture.service.acquire(request("expired"), now).unwrap();
    let after_timeout = now + fixture.service.prepared_ms;
    assert_eq!(
        fixture
            .service
            .starting(starting_request(&expired, after_timeout), after_timeout)
            .unwrap_err()
            .code,
        "LEASE_EXPIRED"
    );
    let next = fixture
        .service
        .acquire(request("replacement"), after_timeout)
        .unwrap();
    fixture
        .service
        .starting(starting_request(&next, after_timeout), after_timeout)
        .unwrap();
    let uncertain = fixture
        .service
        .finish(abort(&next), true, after_timeout + 1)
        .unwrap();
    assert_eq!(uncertain.state, "SUSPECT");
    fixture.service.reconcile(true).unwrap();
    assert!(fixture
        .service
        .acquire(request("steal-after-restart"), after_timeout + 2)
        .is_err());
    let mut known_failure = abort(&next);
    known_failure.reason = Some("spawn-failed".into());
    assert_eq!(
        fixture
            .service
            .finish(known_failure, true, after_timeout + 3)
            .unwrap()
            .state,
        "FAILED"
    );
    assert!(fixture
        .service
        .acquire(request("after-confirmed-failure"), after_timeout + 4)
        .is_ok());
}

#[test]
fn quota_scope_reset_and_short_window_pressure_do_not_invent_capacity() {
    let now = now_ms();
    let policy = Policy::default();
    let mut urgent = account_fixture("urgent", now);
    urgent.buckets = vec![
        bucket("weekly", None, 20.0, now + 7_200_000, now),
        bucket("opus-weekly", Some("opus"), 100.0, now + 86_400_000, now),
    ];
    let mut later = account_fixture("later", now);
    later.buckets = vec![bucket("weekly", None, 20.0, now + 86_400_000, now)];
    let selected = scheduler::decide(
        &[urgent.clone(), later.clone()],
        &[],
        &policy,
        &intent(),
        now,
    )
    .unwrap();
    assert_eq!(selected.selected_account_id.as_deref(), Some("urgent"));
    urgent.buckets.push(bucket(
        "fable-weekly",
        Some("fable"),
        100.0,
        now + 86_400_000,
        now,
    ));
    assert_eq!(
        scheduler::decide(
            &[urgent.clone(), later.clone()],
            &[],
            &policy,
            &intent(),
            now
        )
        .unwrap()
        .selected_account_id
        .as_deref(),
        Some("later")
    );
    urgent.buckets.pop();
    urgent.buckets[0].resets_at = Some(now);
    assert_eq!(
        scheduler::decide(
            &[urgent.clone(), later.clone()],
            &[],
            &policy,
            &intent(),
            now
        )
        .unwrap()
        .selected_account_id
        .as_deref(),
        Some("later")
    );
    urgent.buckets[0].resets_at = Some(now + 72_000_000);
    later.buckets[0].resets_at = Some(now + 75_600_000);
    urgent
        .buckets
        .push(bucket("5h", None, 94.0, now + 3_600_000, now));
    assert_eq!(
        scheduler::decide(&[urgent, later], &[], &policy, &intent(), now)
            .unwrap()
            .selected_account_id
            .as_deref(),
        Some("later")
    );
}

#[test]
fn exhausted_model_bucket_blocks_only_that_model_and_is_reported_for_default_runs() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut account = account_fixture("default-account", now);
    account.buckets.push(bucket(
        "fable-weekly",
        Some("fable"),
        100.0,
        now + 86_400_000,
        now,
    ));
    fixture.save(&account);
    // 공식 CLI가 모델을 결정하는 실행은 모델 전용 소진으로 막지 않고 안내만 남깁니다.
    let mut launch = request("native-default-runs");
    launch.intent.model = aam_protocol::NATIVE_DEFAULT_MODEL.into();
    let grant = fixture.service.acquire(launch, now).unwrap();
    assert_eq!(grant.account.id, account.id);
    let mut default_intent = intent();
    default_intent.model = aam_protocol::NATIVE_DEFAULT_MODEL.into();
    let decision =
        scheduler::decide(&[account.clone()], &[], &Policy::default(), &default_intent, now)
            .unwrap();
    assert_eq!(
        decision.selected_account_id.as_deref(),
        Some("default-account")
    );
    assert!(decision.candidates[0]
        .reasons
        .iter()
        .any(|reason| reason.starts_with("MODEL_LIMIT_GUARD:") && reason.contains("fable-weekly")));
    // 소진된 모델을 직접 요청하면 그 계정을 제외합니다.
    let mut exhausted = intent();
    exhausted.model = "claude-fable-5-1".into();
    let blocked =
        scheduler::decide(&[account.clone()], &[], &Policy::default(), &exhausted, now).unwrap();
    assert!(blocked.selected_account_id.is_none());
    assert!(blocked.candidates[0]
        .reasons
        .iter()
        .any(|reason| reason.starts_with("QUOTA_EXHAUSTED:")));
    let sessions = fixture.service.snapshot().unwrap().sessions;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].account_id, account.id);
}

#[test]
fn manual_unknown_is_explicit_and_resume_never_uses_new_default() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut one = account_fixture("one", now);
    let two = account_fixture("two", now);
    one.buckets.clear();
    fixture.save(&one);
    fixture.save(&two);
    let mut manual = request("manual");
    manual.intent.account_id = Some("one".into());
    let grant = fixture.service.acquire(manual, now).unwrap();
    fixture
        .service
        .finish(abort(&grant), true, now + 1)
        .unwrap();
    let mut policy = Policy::default();
    policy
        .provider_pins
        .insert("anthropic".into(), "two".into());
    let mut resume = intent();
    resume.resume_session_id = Some(grant.session.id.clone());
    let snapshot = fixture.service.snapshot().unwrap();
    assert_eq!(
        scheduler::decide(
            &snapshot.accounts,
            &snapshot.sessions,
            &policy,
            &resume,
            now + 1
        )
        .unwrap()
        .selected_account_id
        .as_deref(),
        Some("one")
    );
    resume.account_id = Some("two".into());
    assert_eq!(
        scheduler::decide(
            &snapshot.accounts,
            &snapshot.sessions,
            &policy,
            &resume,
            now + 1
        )
        .unwrap_err()
        .code,
        "SWITCH_UNSUPPORTED"
    );
    one.enabled = false;
    let mut pinned = intent();
    pinned.account_id = Some("one".into());
    assert!(
        scheduler::decide(&[one, two], &[], &Policy::default(), &pinned, now)
            .unwrap()
            .selected_account_id
            .is_none()
    );
}

#[test]
fn same_identity_profiles_share_slots_but_equal_percent_identities_do_not() {
    let fixture = Fixture::new();
    let now = now_ms();
    let one = account_fixture("one", now);
    let mut duplicate = account_fixture("same-identity-new-profile", now);
    duplicate.identity_key = one.identity_key.clone();
    let independent = account_fixture("different-identity", now);
    fixture.save(&one);
    fixture.save(&duplicate);
    fixture.save(&independent);
    let mut first = request("first-profile");
    first.intent.account_id = Some(one.id.clone());
    fixture.service.acquire(first, now).unwrap();
    let mut second = request("second-profile");
    second.intent.account_id = Some(duplicate.id);
    assert_eq!(
        fixture.service.acquire(second, now).unwrap_err().code,
        "CAPACITY_RESERVED"
    );
    let mut separate = request("independent");
    separate.intent.account_id = Some(independent.id);
    assert!(fixture.service.acquire(separate, now).is_ok());
}

#[test]
fn rows_of_removed_tools_are_dropped_with_policy_references_on_scan() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut legacy = account_fixture("grok-legacy", now);
    legacy.tool = "grok".into();
    legacy.provider = "xai".into();
    let kept = account_fixture("claude-kept", now);
    fixture.save(&legacy);
    fixture.save(&kept);
    let revision = fixture.service.snapshot().unwrap().policy.revision;
    fixture
        .service
        .dispatch(
            "policy.update",
            serde_json::json!({
                "expectedRevision": revision,
                "providerPins": {"xai": "grok-legacy", "anthropic": "claude-kept"},
                "accountPriority": ["grok-legacy", "claude-kept"],
                "projectAllowlist": {"grok-legacy": [fixture.directory.to_string_lossy()]}
            }),
        )
        .unwrap();
    let before = vec![legacy, kept.clone()];
    let scan = aam_adapters::ScanResult {
        accounts: vec![kept],
        tools: vec![],
        notices: vec![],
        merged_observation_ids: Default::default(),
    };
    fixture.service.apply_scan(before, scan).unwrap();
    let snapshot = fixture.service.snapshot().unwrap();
    assert!(snapshot.accounts.iter().all(|account| account.tool != "grok"));
    assert_eq!(snapshot.policy.revision, revision + 2);
    assert_eq!(snapshot.policy.account_priority, vec!["claude-kept".to_owned()]);
    assert!(!snapshot.policy.provider_pins.values().any(|id| id == "grok-legacy"));
    assert!(!snapshot.policy.project_allowlist.contains_key("grok-legacy"));
}

#[test]
fn uninstall_and_admission_are_atomic_and_permit_is_required_to_cancel() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let barrier = Arc::new(Barrier::new(3));
    let service = Arc::clone(&fixture.service);
    let acquire_barrier = Arc::clone(&barrier);
    let acquire = std::thread::spawn(move || {
        acquire_barrier.wait();
        service.acquire(request("during-uninstall"), now)
    });
    let service = Arc::clone(&fixture.service);
    let uninstall_barrier = Arc::clone(&barrier);
    let uninstall = std::thread::spawn(move || {
        uninstall_barrier.wait();
        service.prepare_uninstall()
    });
    barrier.wait();
    let grant = acquire.join().unwrap();
    let permit = uninstall.join().unwrap();
    assert_ne!(grant.is_ok(), permit.is_ok());
    if let Ok(permit) = permit {
        assert_eq!(grant.unwrap_err().code, "ADMISSION_DISABLED");
        assert_eq!(
            fixture
                .service
                .cancel_uninstall(serde_json::json!({"permit":"not-the-permit"}))
                .unwrap_err()
                .code,
            "UNINSTALL_PERMIT_MISMATCH"
        );
        fixture
            .service
            .cancel_uninstall(serde_json::json!({"permit":permit["permit"]}))
            .unwrap();
        assert!(fixture
            .service
            .acquire(request("after-cancel"), now + 1)
            .is_ok());
    } else {
        assert_eq!(permit.unwrap_err().code, "SESSION_BUSY");
    }
}

/// 자손 기록이 없는 ORPHANED 세션은 같은 부팅에서는 유지하고, 기록된 identity가 모두 이전 부팅이면 반환한다.
#[cfg(target_os = "macos")]
#[test]
fn orphan_without_descendant_records_is_released_only_after_reboot() {
    use std::os::unix::process::CommandExt;

    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let grant = fixture.service.acquire(request("orphan"), now).unwrap();
    fixture
        .service
        .starting(starting_request(&grant, now), now)
        .unwrap();
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("0.1")
        .process_group(0)
        .spawn()
        .unwrap();
    let process = process_identity(child.id()).unwrap();
    assert!(child.wait().unwrap().success());
    let _ = fixture.service.started(
        Started {
            session_id: grant.session.id.clone(),
            capability: grant.capability.clone(),
            spawn_attempt_id: "one-spawn-attempt".into(),
            process,
        },
        now_ms(),
    );
    let mut unknown = abort(&grant);
    unknown.reason = Some("native-exit-background-unverified".into());
    unknown.exit_code = Some(0);
    assert_eq!(fixture.service.finish(unknown, false, now_ms()).unwrap().state, "ORPHANED");
    let rewrite = |boot: Option<&str>| {
        let store = fixture.service.lock().unwrap();
        let mut body =
            serde_json::to_value(lease(&store.connection, &grant.session.id).unwrap().session)
                .unwrap();
        let object = body.as_object_mut().unwrap();
        object.remove("backgroundProcesses");
        if let Some(boot) = boot {
            for key in ["process", "supervisor"] {
                if let Some(identity) = object.get_mut(key).and_then(|v| v.as_object_mut()) {
                    identity.insert("bootId".into(), boot.into());
                }
            }
        }
        store
            .connection
            .execute(
                "UPDATE leases SET body=?1 WHERE id=?2",
                rusqlite::params![serde_json::to_string(&body).unwrap(), &grant.session.id],
            )
            .unwrap();
    };
    // 같은 부팅: 남은 자손이 없다는 근거가 없으므로 계속 점유한다.
    rewrite(None);
    let reopened = Service::open(fixture.service.paths.clone()).unwrap();
    reopened.reconcile(true).unwrap();
    assert_eq!(reopened.snapshot().unwrap().sessions[0].state, "ORPHANED");
    assert_eq!(
        reopened.acquire(request("same-boot"), now_ms()).unwrap_err().code,
        "NO_ELIGIBLE_ACCOUNT"
    );
    drop(reopened);
    // 이전 부팅: 재부팅을 넘어 살아남는 프로세스는 없으므로 반환한다.
    rewrite(Some("00000000-0000-0000-0000-000000000000"));
    let rebooted = Service::open(fixture.service.paths.clone()).unwrap();
    rebooted.reconcile(true).unwrap();
    assert_eq!(rebooted.snapshot().unwrap().sessions[0].state, "EXITED");
    assert!(rebooted.acquire(request("after-reboot"), now_ms()).is_ok());
}

/// Windows 관리 시작 경로: 실제 자식 프로세스로 starting→started(ACTIVE)→release(EXITED).
#[cfg(windows)]
#[test]
fn windows_child_is_verified_started_and_released() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let grant = fixture.service.acquire(request("win-child"), now).unwrap();
    fixture.service.starting(starting_request(&grant, now), now).unwrap();
    // 실행기처럼 이름 있는 Job에 넣어 시작한다.
    let mut command = std::process::Command::new("ping");
    command.args(["-n", "3", "127.0.0.1"]).stdout(std::process::Stdio::null());
    let (mut child, _launcher_job) = aam_protocol::spawn_in_job(&mut command, false, false, true).unwrap();
    let process = process_identity(child.id()).unwrap();
    let session = fixture
        .service
        .started(
            Started {
                session_id: grant.session.id.clone(),
                capability: grant.capability.clone(),
                spawn_attempt_id: "one-spawn-attempt".into(),
                process: process.clone(),
            },
            now_ms(),
        )
        .unwrap();
    assert_eq!(session.state, "ACTIVE");
    // 부모가 아닌 identity(다른 자식의 PID를 supervisor로 쓴 경우)는 자식으로 인정하지 않는다.
    assert!(!process::is_child(&process, &process));
    assert!(child.wait().unwrap().success());
    let mut release = abort(&grant);
    release.reason = Some("native-exit-foreground-confirmed".into());
    release.exit_code = Some(0);
    release.background_processes = Some(Vec::new());
    assert_eq!(fixture.service.finish(release, false, now_ms()).unwrap().state, "EXITED");
}

/// Windows 실행기 유실: release 보고 없이 실행기가 사라져도, 서비스가 쥔 Job의 활성 프로세스가 0이 된 뒤에만
/// 슬롯을 반환한다. 손자가 살아 있는 동안에는 반환하지 않는다.
#[cfg(windows)]
#[test]
fn windows_lost_launcher_releases_only_after_every_job_process_exits() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let grant = fixture.service.acquire(request("win-job"), now).unwrap();
    fixture.service.starting(starting_request(&grant, now), now).unwrap();
    // cmd가 ping(약 3초)을 백그라운드로 띄우고 1초쯤 뒤 끝난다. 실행기처럼 이름 있는 Job에 넣는다.
    let mut command = std::process::Command::new("cmd");
    command
        .args(["/d", "/c", "start /b ping -n 4 127.0.0.1 & ping -n 2 127.0.0.1"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let (mut child, launcher_job) = aam_protocol::spawn_in_job(&mut command, false, false, true).unwrap();
    let process = process_identity(child.id()).unwrap();
    let session = fixture
        .service
        .started(
            Started {
                session_id: grant.session.id.clone(),
                capability: grant.capability.clone(),
                spawn_attempt_id: "one-spawn-attempt".into(),
                process,
            },
            now_ms(),
        )
        .unwrap();
    assert_eq!(session.state, "ACTIVE");
    child.wait().unwrap();
    // 실행기가 보고 없이 사라진 상황: 실행기 쪽 Job 핸들만 닫는다.
    drop(launcher_job);
    drop(child);
    fixture.service.reconcile(false).unwrap();
    let held = fixture.service.snapshot().unwrap().sessions[0].state.clone();
    assert!(held != "EXITED", "손자가 살아 있는 동안 반환하면 안 된다: {held}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        fixture.service.reconcile(false).unwrap();
        if fixture.service.snapshot().unwrap().sessions[0].state == "EXITED" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "Job이 비었는데 반환하지 않았다");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(target_os = "macos")]
#[test]
fn a_fast_child_report_preserves_identity_until_confirmed_release() {
    use std::os::unix::process::CommandExt;

    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let grant = fixture
        .service
        .acquire(request("short-child"), now)
        .unwrap();
    fixture
        .service
        .starting(starting_request(&grant, now), now)
        .unwrap();
    let mut child = std::process::Command::new("/bin/sleep")
        .arg("0.1")
        .process_group(0)
        .spawn()
        .unwrap();
    let process = process_identity(child.id()).unwrap();
    assert!(child.wait().unwrap().success());
    let reported = fixture.service.started(
        Started {
            session_id: grant.session.id.clone(),
            capability: grant.capability.clone(),
            spawn_attempt_id: "one-spawn-attempt".into(),
            process: process.clone(),
        },
        now_ms(),
    );
    assert_eq!(reported.unwrap_err().code, "PROCESS_IDENTITY_UNVERIFIED");
    let snapshot = fixture.service.snapshot().unwrap();
    assert_eq!(snapshot.sessions[0].state, "SUSPECT");
    assert_eq!(snapshot.sessions[0].process.as_ref(), Some(&process));
    let mut unknown = abort(&grant);
    unknown.reason = Some("native-exit-background-unverified".into());
    unknown.exit_code = Some(0);
    let unknown = fixture.service.finish(unknown, false, now_ms()).unwrap();
    assert_eq!(unknown.state, "ORPHANED");
    // 이전 버전 JSON처럼 근거 필드가 없는 세션도 계속 용량을 점유해야 합니다.
    {
        let store = fixture.service.lock().unwrap();
        let mut body =
            serde_json::to_value(lease(&store.connection, &grant.session.id).unwrap().session)
                .unwrap();
        body.as_object_mut().unwrap().remove("backgroundProcesses");
        store
            .connection
            .execute(
                "UPDATE leases SET body=?1 WHERE id=?2",
                rusqlite::params![serde_json::to_string(&body).unwrap(), &grant.session.id],
            )
            .unwrap();
    }
    let reopened = Service::open(fixture.service.paths.clone()).unwrap();
    reopened.reconcile(true).unwrap();
    assert_eq!(reopened.snapshot().unwrap().sessions[0].state, "ORPHANED");
    assert_eq!(
        reopened
            .acquire(request("without-background-evidence"), now_ms())
            .unwrap_err()
            .code,
        "NO_ELIGIBLE_ACCOUNT"
    );
    let mut release = abort(&grant);
    release.reason = Some("native-exit-foreground-confirmed".into());
    release.exit_code = Some(0);
    release.background_processes = Some(Vec::new());
    assert_eq!(
        fixture
            .service
            .finish(release, false, now_ms())
            .unwrap()
            .state,
        "EXITED"
    );
    assert!(fixture
        .service
        .acquire(request("next-child"), now_ms())
        .is_ok());
}

#[cfg(target_os = "macos")]
#[test]
fn background_exit_evidence_survives_restart_and_waits_for_the_entire_group() {
    use std::{
        io::{BufRead, BufReader, Write},
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };

    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let grant = fixture.service.acquire(request("background"), now).unwrap();
    fixture
        .service
        .starting(starting_request(&grant, now), now)
        .unwrap();
    let first_exit = fixture.directory.join("first-exit");
    let final_exit = fixture.directory.join("final-exit");
    // 파일 게이트로 순서를 고정하고, 실패한 테스트의 자손도 스스로 종료하게 제한합니다.
    let descendant_script = r#"
printf '%s\n' "$$"
n=0
while [ ! -e "$1" ] && [ "$n" -lt 500 ]; do n=$((n + 1)); /bin/sleep 0.02; done
/bin/sh -c '
printf "%s\n" "$$"
n=0
while [ ! -e "$1" ] && [ "$n" -lt 500 ]; do n=$((n + 1)); /bin/sleep 0.02; done
' successor "$2" &
"#;
    let mut native = Command::new("/bin/sh")
        .args([
            "-c",
            "\"$@\" & read done",
            "native",
            "/bin/sh",
            "-c",
            descendant_script,
            "descendant",
        ])
        .arg(&first_exit)
        .arg(&final_exit)
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let native_identity = process_identity(native.id()).unwrap();
    let mut output = BufReader::new(native.stdout.take().unwrap());
    let mut line = String::new();
    output.read_line(&mut line).unwrap();
    let descendant = process_identity(line.trim().parse().unwrap()).unwrap();
    fixture
        .service
        .started(
            Started {
                session_id: grant.session.id.clone(),
                capability: grant.capability.clone(),
                spawn_attempt_id: "one-spawn-attempt".into(),
                process: native_identity,
            },
            now_ms(),
        )
        .unwrap();
    writeln!(native.stdin.take().unwrap(), "exit").unwrap();
    assert!(native.wait().unwrap().success());
    let mut release = abort(&grant);
    release.reason = Some("native-exit-background-unverified".into());
    release.background_processes = Some(vec![descendant.clone()]);
    release.exit_code = Some(17);
    assert_eq!(
        fixture
            .service
            .finish(release, false, now_ms())
            .unwrap()
            .state,
        "ORPHANED"
    );
    let reopened = Service::open(fixture.service.paths.clone()).unwrap();
    reopened.reconcile(true).unwrap();
    assert_eq!(process::inspect(&descendant), Liveness::Alive);
    assert_eq!(
        reopened
            .acquire(request("while-descendant-live"), now_ms())
            .unwrap_err()
            .code,
        "NO_ELIGIBLE_ACCOUNT"
    );

    std::fs::write(&first_exit, b"").unwrap();
    line.clear();
    output.read_line(&mut line).unwrap();
    let successor = process_identity(line.trim().parse().unwrap()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while process::inspect(&descendant) != Liveness::Dead && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(process::inspect(&descendant), Liveness::Dead);
    // 기록에 없던 후속 자손이 원래 그룹에 남아 있으면 스냅샷만으로 반환하지 않습니다.
    let reopened = Service::open(fixture.service.paths.clone()).unwrap();
    reopened.reconcile(true).unwrap();
    assert_eq!(process::inspect(&successor), Liveness::Alive);
    assert_eq!(
        reopened
            .acquire(request("while-group-live"), now_ms())
            .unwrap_err()
            .code,
        "NO_ELIGIBLE_ACCOUNT"
    );

    std::fs::write(&final_exit, b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while process::inspect(&successor) != Liveness::Dead && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(process::inspect(&successor), Liveness::Dead);
    let reopened = Service::open(fixture.service.paths.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        reopened.reconcile(true).unwrap();
        let session = &reopened.snapshot().unwrap().sessions[0];
        if session.state == "EXITED" {
            assert_eq!(session.exit_code, Some(17));
            break;
        }
        assert!(
            Instant::now() < deadline,
            "종료된 그룹의 슬롯이 반환되지 않았습니다."
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(reopened
        .acquire(request("after-background-exit"), now_ms())
        .is_ok());
}

#[cfg(target_os = "macos")]
#[test]
fn fresh_exhaustion_after_acquire_prevents_spawn() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut account = account_fixture("one", now);
    fixture.save(&account);
    let grant = fixture
        .service
        .acquire(request("quota-changed"), now)
        .unwrap();
    account.buckets[0].used_percent = Some(100.0);
    account.buckets[0].status = "exhausted".into();
    fixture.save(&account);
    assert_eq!(
        fixture
            .service
            .starting(starting_request(&grant, now), now)
            .unwrap_err()
            .code,
        "ADMISSION_CHANGED"
    );
}

#[test]
fn managed_resume_retains_native_identity_and_fences_parallel_resumes() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut account = account_fixture("one", now);
    account.max_concurrency = 2;
    fixture.save(&account);
    let original = fixture.service.acquire(request("original"), now).unwrap();
    assert_eq!(
        original.session.native_session_id.as_deref(),
        Some(original.session.id.as_str())
    );
    let mut premature = request("premature-resume");
    premature.intent.resume_session_id = Some(original.session.id.clone());
    assert_eq!(
        fixture.service.acquire(premature, now).unwrap_err().code,
        "SESSION_BUSY"
    );
    fixture
        .service
        .finish(abort(&original), true, now + 1)
        .unwrap();
    let mut resumed = request("first-resume");
    resumed.intent.resume_session_id = Some(original.session.id.clone());
    let grant = fixture.service.acquire(resumed, now + 2).unwrap();
    assert_ne!(grant.session.id, original.session.id);
    assert_eq!(
        grant.session.native_session_id,
        original.session.native_session_id
    );
    let mut simultaneous = request("parallel-resume");
    simultaneous.intent.resume_session_id = Some(original.session.id.clone());
    assert_eq!(
        fixture
            .service
            .acquire(simultaneous, now + 3)
            .unwrap_err()
            .code,
        "SESSION_BUSY"
    );
}

fn project_policy(fixture: &Fixture, allowlist: Value) -> Policy {
    let revision = fixture.service.snapshot().unwrap().policy.revision;
    serde_json::from_value(
        fixture
            .service
            .dispatch(
                "policy.update",
                serde_json::json!({
                    "expectedRevision": revision, "projectAllowlist": allowlist
                }),
            )
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn project_policy_canonicalizes_roots_and_preserves_cas_and_other_preferences() {
    let fixture = Fixture::new();
    fixture.save(&account_fixture("one", now_ms()));
    let root = fixture.directory.join("project");
    std::fs::create_dir(&root).unwrap();
    let before = fixture.service.snapshot().unwrap().policy;
    let updated = project_policy(&fixture, serde_json::json!({"one": [root, root.join(".")]}));
    assert_eq!(
        updated.project_allowlist["one"],
        vec![std::fs::canonicalize(&root).unwrap().to_str().unwrap()]
    );
    assert_eq!(updated.revision, before.revision + 1);
    assert_eq!(
        fixture
            .service
            .dispatch(
                "policy.update",
                serde_json::json!({
                    "expectedRevision": before.revision, "projectAllowlist": {}
                })
            )
            .unwrap_err()
            .code,
        "POLICY_CONFLICT"
    );
    let changed: Policy = serde_json::from_value(fixture.service.dispatch("policy.update", serde_json::json!({
        "expectedRevision": updated.revision, "automatic": false, "providerPins": {"anthropic": "one"}
    })).unwrap()).unwrap();
    assert_eq!(changed.project_allowlist, updated.project_allowlist);
    let unrestricted = project_policy(&fixture, serde_json::json!({}));
    assert!(!unrestricted.automatic);
    assert_eq!(unrestricted.provider_pins["anthropic"], "one");
    assert!(!unrestricted.project_allowlist.contains_key("one"));
}

#[test]
fn project_policy_rejects_unknown_accounts_and_invalid_directory_roots_atomically() {
    let fixture = Fixture::new();
    fixture.save(&account_fixture("one", now_ms()));
    let file = fixture.directory.join("not-a-directory");
    std::fs::write(&file, b"").unwrap();
    let original = fixture.service.snapshot().unwrap().policy;
    for allowlist in [
        serde_json::json!({"missing-account": []}),
        serde_json::json!({"one": ["relative"]}),
        serde_json::json!({"one": [file]}),
        serde_json::json!({"one": [fixture.directory.join("absent")]}),
        serde_json::json!({"one": [format!("{}\u{0}", test_cwd())]}),
        serde_json::json!({"one": "not-an-array"}),
        serde_json::json!({"one": vec![test_cwd(); 65]}),
    ] {
        assert!(fixture.service.dispatch("policy.update", serde_json::json!({
            "expectedRevision": original.revision, "automatic": false, "projectAllowlist": allowlist
        })).is_err());
        let after = fixture.service.snapshot().unwrap().policy;
        assert_eq!(after.revision, original.revision);
        assert!(after.automatic);
        assert!(after.project_allowlist.is_empty());
    }
}

#[cfg(unix)]
#[test]
fn project_boundaries_reject_prefix_dotdot_and_symlink_escapes_even_when_manual() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let root = fixture.directory.join("project");
    let child = root.join("child");
    let outside = fixture.directory.join("project-other");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    project_policy(&fixture, serde_json::json!({"one": [root]}));
    for manual in [false, true] {
        for path in [&root, &child] {
            let mut launch = request(&new_id());
            launch.intent.cwd = path.to_str().unwrap().into();
            launch.intent.account_id = manual.then(|| "one".into());
            let grant = fixture.service.acquire(launch, now).unwrap();
            fixture.service.finish(abort(&grant), true, now).unwrap();
        }
        for path in [
            outside.clone(),
            root.join("../project-other"),
            root.join("escape"),
        ] {
            let mut launch = request(&new_id());
            launch.intent.cwd = path.to_str().unwrap().into();
            launch.intent.account_id = manual.then(|| "one".into());
            assert_eq!(
                fixture.service.acquire(launch, now).unwrap_err().code,
                if manual {
                    "PROJECT_NOT_ALLOWED"
                } else {
                    "NO_ELIGIBLE_ACCOUNT"
                }
            );
        }
    }
}

#[test]
fn deny_all_blocks_manual_resume_and_child_launches_without_touching_existing_sessions() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut account = account_fixture("one", now);
    account.max_concurrency = 3;
    fixture.save(&account);
    let parent = fixture.service.acquire(request("parent"), now).unwrap();
    let original = fixture.service.acquire(request("previous"), now).unwrap();
    fixture.service.finish(abort(&original), true, now).unwrap();
    project_policy(&fixture, serde_json::json!({"one": []}));
    let mut manual = request("manual");
    manual.intent.account_id = Some("one".into());
    assert_eq!(
        fixture.service.acquire(manual, now).unwrap_err().code,
        "PROJECT_NOT_ALLOWED"
    );
    let mut child = request("child");
    child.intent.account_id = Some("one".into());
    child.intent.parent_session_id = Some(parent.session.id.clone());
    child.parent_capability = Some(parent.capability.clone());
    assert_eq!(
        fixture.service.acquire(child, now).unwrap_err().code,
        "PARENT_SESSION_UNKNOWN"
    );
    let mut resume = request("resume");
    resume.intent.resume_session_id = Some(original.session.id.clone());
    assert!(fixture.service.acquire(resume, now).is_err());
    assert_eq!(
        fixture
            .service
            .acquire(request("parent"), now)
            .unwrap()
            .session
            .state,
        "PREPARED"
    );
    project_policy(&fixture, serde_json::json!({}));
    assert!(fixture
        .service
        .acquire(request("unrestricted"), now)
        .is_ok());
}

#[cfg(target_os = "macos")]
#[test]
fn starting_rechecks_canonical_project_after_directory_is_replaced_with_symlink() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let root = fixture.directory.join("project");
    let child = root.join("child");
    let outside = fixture.directory.join("outside");
    std::fs::create_dir_all(&child).unwrap();
    std::fs::create_dir(&outside).unwrap();
    project_policy(&fixture, serde_json::json!({"one": [root]}));
    let mut launch = request("before-path-change");
    launch.intent.cwd = child.to_str().unwrap().into();
    launch.intent.account_id = Some("one".into());
    let grant = fixture.service.acquire(launch, now).unwrap();
    std::fs::remove_dir(&child).unwrap();
    std::os::unix::fs::symlink(&outside, &child).unwrap();
    assert_eq!(
        fixture
            .service
            .starting(starting_request(&grant, now), now)
            .unwrap_err()
            .code,
        "ADMISSION_CHANGED"
    );
    std::fs::remove_file(&child).unwrap();
    std::fs::create_dir(&child).unwrap();
    let started = fixture
        .service
        .starting(starting_request(&grant, now), now)
        .unwrap();
    assert_eq!(started.state, "STARTING");
    project_policy(&fixture, serde_json::json!({"one": []}));
    assert_eq!(
        fixture
            .service
            .starting(starting_request(&grant, now), now)
            .unwrap()
            .state,
        "STARTING"
    );
}

#[test]
fn diagnostics_export_allowlists_all_strings_and_exports_full_time_filtered_history() {
    let fixture = Fixture::new();
    let now = now_ms();
    let secret = "PRIVATE_SENTINEL_email_token_path_prompt_identity";
    let mut account = account_fixture(secret, now);
    account.omp_credential_pins.push(OmpCredentialPin {
        provider: secret.into(),
        hash: secret.into(),
    });
    fixture.save(&account);
    let grant = fixture.service.acquire(request("diagnostic"), now).unwrap();
    {
        let mut store = fixture.service.lock().unwrap();
        let tx = store.connection.transaction().unwrap();
        let mut record = lease(&tx, &grant.session.id).unwrap();
        record.session.state = "EXITED".into();
        record.session.model = secret.into();
        record.session.cwd = secret.into();
        record.session.native_session_id = Some(secret.into());
        record.session.reason = Some(secret.into());
        record.session.request_id = secret.into();
        record.session.generation = secret.into();
        record.session.spawn_attempt_id = Some(secret.into());
        record.session.process = Some(ProcessIdentity {
            pid: 123,
            started_at: secret.into(),
            boot_id: secret.into(),
        });
        save_lease(&tx, &record).unwrap();
        for index in 0..201 {
            record.session.id = format!("{secret}-{index}");
            record.session.request_id = format!("{secret}-request-{index}");
            record.session.started_at = now + 1;
            insert_lease(&tx, &record, secret).unwrap();
        }
        account.provider = secret.into();
        account.tool = secret.into();
        account.email = Some(secret.into());
        account.organization = Some(secret.into());
        account.plan = Some(secret.into());
        account.profile_path = Some(secret.into());
        account.binary_path = Some(secret.into());
        account.identity_key = Some(secret.into());
        account.auth_status = secret.into();
        account.verification = secret.into();
        account.reason = Some(secret.into());
        account.buckets[0].id = secret.into();
        account.buckets[0].label = secret.into();
        account.buckets[0].model = Some(secret.into());
        account.buckets[0].source = secret.into();
        account.buckets[0].status = secret.into();
        save_account(&tx, &account).unwrap();
        set_metadata(
            &tx,
            "tools",
            &vec![ToolStatus {
                id: secret.into(),
                name: secret.into(),
                provider: secret.into(),
                binary_path: Some(secret.into()),
                version: Some(secret.into()),
                installed: true,
                isolation: secret.into(),
                reason: Some(secret.into()),
            }],
        )
        .unwrap();
        set_metadata(
            &tx,
            "notices",
            &vec![Notice {
                id: secret.into(),
                level: secret.into(),
                title: secret.into(),
                message: secret.into(),
            }],
        )
        .unwrap();
        let mut policy = policy(&tx).unwrap();
        policy
            .project_allowlist
            .insert(secret.into(), vec![secret.into()]);
        policy
            .provider_pins
            .insert(secret.into(), secret.into());
        set_metadata(&tx, "policy", &policy).unwrap();
        tx.commit().unwrap();
    }
    let exported = fixture
        .service
        .dispatch("diagnostics.export", serde_json::json!({}))
        .unwrap();
    let serialized = exported.to_string();
    for forbidden in [
        secret,
        grant.capability.as_str(),
        grant.session.id.as_str(),
        grant.session.generation.as_str(),
        grant.account.profile_path.as_deref().unwrap(),
    ] {
        assert!(!serialized.contains(forbidden));
    }
    assert_eq!(exported["sessions"].as_array().unwrap().len(), 202);
    assert_eq!(
        exported["accounts"][0]["id"],
        exported["sessions"][0]["accountId"]
    );
    assert_eq!(exported["accounts"][0]["provider"], "unknown");
    let selected = fixture
        .service
        .dispatch(
            "diagnostics.export",
            serde_json::json!({"from": now, "to": now}),
        )
        .unwrap();
    assert_eq!(selected["sessions"].as_array().unwrap().len(), 1);
    for params in [
        serde_json::json!({"redact": false}),
        serde_json::json!({"from": -1}),
        serde_json::json!({"from": now + 1, "to": now}),
        serde_json::json!({"to": "not-a-timestamp"}),
    ] {
        assert_eq!(
            fixture
                .service
                .dispatch("diagnostics.export", params)
                .unwrap_err()
                .code,
            "INVALID_PARAMS"
        );
    }
}

fn observed_fixture(pid: u32) -> ObservedSession {
    ObservedSession {
        id: format!("observed:omp:boot:100:000001:{pid}"),
        tool: "omp".into(),
        process: ProcessIdentity {
            pid,
            started_at: "100:000001".into(),
            boot_id: "boot".into(),
        },
        parent_process: None,
        host: "orca".into(),
        cwd: Some("/external/project".into()),
        model: None,
        account_id: None,
        verification: "observed".into(),
        reason: None,
        attributions: Vec::new(),
        native_session_id: None,
    }
}

#[test]
fn external_observations_do_not_reserve_capacity_or_become_resumable_leases() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("available", now));
    let mut observed = observed_fixture(900);
    observed.attributions.push(ObservedAttribution {
        session_id: "external-session".into(),
        role: "main".into(),
        provider: "anthropic".into(),
        model: None,
        account_id: Some("available".into()),
        verification: "session-pin".into(),
        recorded_at: now,
        stop_reason: None,
        source: "session-file".into(),
        route: None,
        reason: None,
    });
    let snapshot = fixture
        .service
        .snapshot_with_observations(aam_adapters::ObservedScan {
            sessions: vec![observed.clone()],
            notices: vec![],
        })
        .unwrap();
    assert_eq!(snapshot.observed_sessions[0].id, observed.id);
    assert!(snapshot.sessions.is_empty());
    assert!(leases(&fixture.service.lock().unwrap().connection)
        .unwrap()
        .is_empty());
    // 외부 관측이 있어도 마지막 관리 슬롯을 정상 예약할 수 있습니다.
    let managed = fixture.service.acquire(request("managed"), now).unwrap();
    assert_eq!(managed.account.id, "available");
    let next = fixture
        .service
        .snapshot_with_observations(aam_adapters::ObservedScan::default())
        .unwrap();
    assert!(next.observed_sessions.is_empty());
    assert_eq!(next.sessions[0].id, managed.session.id);
    let mut resume = request("cannot-resume-observation");
    resume.intent.resume_session_id = Some(observed.id);
    assert_eq!(
        fixture.service.acquire(resume, now).unwrap_err().code,
        "RESUME_UNKNOWN"
    );
}

#[test]
fn observations_deduplicate_managed_birth_identity_not_just_pid() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("available", now));
    let managed = fixture.service.acquire(request("managed"), now).unwrap();
    let observed = observed_fixture(901);
    {
        let store = fixture.service.lock().unwrap();
        let mut record = leases(&store.connection).unwrap().remove(0);
        record.session.process = Some(observed.process.clone());
        save_lease(&store.connection, &record).unwrap();
    }
    let same = fixture
        .service
        .snapshot_with_observations(aam_adapters::ObservedScan {
            sessions: vec![observed.clone()],
            notices: vec![],
        })
        .unwrap();
    assert!(same.observed_sessions.is_empty());
    assert_eq!(same.sessions[0].id, managed.session.id);
    let mut reused = observed;
    reused.process.started_at = "101:000001".into();
    reused.id = "observed:omp:boot:101:000001:901".into();
    let new_birth = fixture
        .service
        .snapshot_with_observations(aam_adapters::ObservedScan {
            sessions: vec![reused.clone()],
            notices: vec![],
        })
        .unwrap();
    assert_eq!(new_birth.observed_sessions[0].id, reused.id);
}

#[test]
fn old_snapshots_decode_without_observations_and_failures_remain_visible() {
    let fixture = Fixture::new();
    let snapshot = fixture
        .service
        .snapshot_with_observations(aam_adapters::ObservedScan {
            sessions: vec![],
            notices: vec![Notice {
                id: "observed-session-discovery".into(),
                level: "warning".into(),
                title: "조회 불완전".into(),
                message: "실행 세션 미확인".into(),
            }],
        })
        .unwrap();
    assert_eq!(snapshot.notices[0].id, "observed-session-discovery");
    let mut old = serde_json::to_value(snapshot).unwrap();
    old.as_object_mut().unwrap().remove("observedSessions");
    assert!(serde_json::from_value::<Snapshot>(old)
        .unwrap()
        .observed_sessions
        .is_empty());
}

/// 한도 소진 뒤 이어가기: 원래 계정을 뺀 계정으로 배정하고 같은 native 대화 ID를 쓰며 원래 세션을 기록한다.
/// 이어 간 뒤의 일반 재개는 마지막 계정으로 가고, 근거(`continued_from`) 없이 계정이 바뀐 대화는 재개하지 않는다.
#[test]
fn continuing_elsewhere_moves_the_conversation_to_another_account_and_resume_follows_it() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    fixture.save(&account_fixture("two", now));
    let mut first = request("first");
    first.intent.account_id = Some("one".into());
    let original = fixture.service.acquire(first, now).unwrap();
    fixture.service.finish(abort(&original), true, now + 1).unwrap();
    let mut next = request("continue");
    next.intent.resume_session_id = Some(original.session.id.clone());
    next.intent.continue_elsewhere = true;
    let moved = fixture.service.acquire(next, now + 2).unwrap();
    assert_eq!(moved.account.id, "two");
    assert_eq!(moved.session.native_session_id, original.session.native_session_id);
    assert_eq!(moved.session.continued_from.as_deref(), Some(original.session.id.as_str()));
    fixture.service.finish(abort(&moved), true, now + 3).unwrap();
    // 지금 대화를 가진 계정(two)을 다시 고르면 이어가기가 아니다. (one으로 되돌아가는 것은 허용된다.)
    let mut same = request("same");
    same.intent.resume_session_id = Some(original.session.id.clone());
    same.intent.continue_elsewhere = true;
    same.intent.account_id = Some("two".into());
    assert_eq!(fixture.service.acquire(same, now + 4).unwrap_err().code, "CONTINUE_SAME_ACCOUNT");
    // 이어 간 대화를 그냥 재개하면 마지막 계정(two)으로 간다.
    let mut resume = request("resume");
    resume.intent.resume_session_id = original.session.native_session_id.clone();
    assert_eq!(fixture.service.acquire(resume, now + 5).unwrap().account.id, "two");
    // 근거 없이 계정이 바뀐 기록이면 재개하지 않는다.
    let mut sessions = fixture.service.snapshot().unwrap().sessions;
    for session in &mut sessions {
        session.continued_from = None;
    }
    let mut unverified = intent();
    unverified.resume_session_id = original.session.native_session_id.clone();
    assert_eq!(
        routes::original(&sessions, &unverified).unwrap_err().code,
        "RESUME_UNVERIFIED"
    );
}

/// Codex는 종료 보고 때 대화 ID를 기록한다. 다른 관리 세션이 이미 쓰는 ID는 받지 않는다.
/// 기록된 대화는 다른 Codex 계정에서 이어 갈 수 있다.
#[test]
fn codex_conversation_id_is_recorded_on_release_and_can_continue_elsewhere() {
    let fixture = Fixture::new();
    let now = now_ms();
    let codex = |id: &str| {
        let mut account = account_fixture(id, now);
        account.tool = "codex".into();
        account.provider = "openai".into();
        account
    };
    fixture.save(&codex("one"));
    fixture.save(&codex("two"));
    let mut launch = request("codex-first");
    launch.intent.tool = "codex".into();
    launch.intent.account_id = Some("one".into());
    let grant = fixture.service.acquire(launch, now).unwrap();
    assert_eq!(grant.session.native_session_id, None);
    let native = "01a0ed42-9fba-73e1-8b22-92bebc543786";
    let mut release = abort(&grant);
    release.native_session_id = Some(native.into());
    let ended = fixture.service.finish(release, true, now + 1).unwrap();
    assert_eq!(ended.native_session_id.as_deref(), Some(native));
    // 같은 ID를 다른 세션이 보고하면 기록하지 않는다.
    let mut again = request("codex-again");
    again.intent.tool = "codex".into();
    again.intent.account_id = Some("two".into());
    let other = fixture.service.acquire(again, now + 2).unwrap();
    let mut duplicate = abort(&other);
    duplicate.native_session_id = Some(native.into());
    assert_eq!(fixture.service.finish(duplicate, true, now + 3).unwrap().native_session_id, None);
    let mut next = request("codex-continue");
    next.intent.tool = "codex".into();
    next.intent.resume_session_id = Some(grant.session.id.clone());
    next.intent.continue_elsewhere = true;
    let moved = fixture.service.acquire(next, now + 4).unwrap();
    assert_eq!(moved.account.id, "two");
    assert_eq!(moved.session.native_session_id.as_deref(), Some(native));
    assert_eq!(moved.session.continued_from.as_deref(), Some(grant.session.id.as_str()));
}
