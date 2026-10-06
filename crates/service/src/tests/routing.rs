use super::*;

fn route(path: &std::path::Path, scope: &str, mode: &str, account: Option<&str>) -> Value {
    serde_json::json!({"path": path, "scope": scope, "tool": "claude", "mode": mode, "accountId": account, "model": null})
}
fn save_routes(fixture: &Fixture, routes: Value) -> Result<Value, ApiError> {
    let revision = fixture.service.snapshot().unwrap().policy.revision;
    fixture.service.dispatch(
        "policy.update",
        serde_json::json!({"expectedRevision": revision, "projectRoutes": routes}),
    )
}
fn resolve(fixture: &Fixture, intent: &LaunchIntent) -> RouteResolution {
    serde_json::from_value(
        fixture
            .service
            .dispatch("route.resolve", serde_json::json!({"intent": intent}))
            .unwrap(),
    )
    .unwrap()
}
fn git(directory: &std::path::Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(directory)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn deepest_directory_route_wins_without_prefix_escape_and_is_enforced_at_acquire() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    fixture.save(&account_fixture("two", now));
    let root = fixture.directory.join("project");
    let deep = root.join("deep");
    let outside = fixture.directory.join("project-other");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::create_dir(&outside).unwrap();
    save_routes(
        &fixture,
        serde_json::json!([
            route(&root, "directory", "pinned", Some("one")),
            route(&deep, "directory", "pinned", Some("two"))
        ]),
    )
    .unwrap();
    let mut launch = request("deep");
    launch.intent.cwd = deep.to_string_lossy().into_owned();
    assert_eq!(
        resolve(&fixture, &launch.intent).account_id.as_deref(),
        Some("two")
    );
    let explained = fixture
        .service
        .dispatch(
            "route.explain",
            serde_json::json!({"intent": launch.intent}),
        )
        .unwrap();
    assert_eq!(explained["selectedAccountId"], "two");
    let grant = fixture.service.acquire(launch, now).unwrap();
    assert_eq!(grant.account.id, "two");
    let mut launch = intent();
    launch.cwd = outside.to_string_lossy().into_owned();
    assert_eq!(resolve(&fixture, &launch).source, "global");
    launch.cwd = root.to_string_lossy().into_owned();
    launch.account_id = Some("two".into());
    assert_eq!(
        fixture
            .service
            .dispatch("route.resolve", serde_json::json!({"intent": launch}))
            .unwrap_err()
            .code,
        "ROUTE_CONFLICT"
    );
    let reopened = Service::open(fixture.service.paths.clone()).unwrap();
    assert_eq!(reopened.snapshot().unwrap().policy.project_routes.len(), 2);
}

#[test]
fn deleted_unrelated_directory_rule_does_not_block_a_pinned_launch() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    fixture.save(&account_fixture("two", now));
    let stale = fixture.directory.join("deleted");
    let project = fixture.directory.join("project");
    std::fs::create_dir(&stale).unwrap();
    std::fs::create_dir(&project).unwrap();
    save_routes(
        &fixture,
        serde_json::json!([
            route(&stale, "directory", "pinned", Some("one")),
            route(&project, "directory", "pinned", Some("two"))
        ]),
    )
    .unwrap();
    std::fs::remove_dir(&stale).unwrap();
    let mut launch = request("unrelated-stale-route");
    launch.intent.cwd = project.to_string_lossy().into_owned();
    assert_eq!(
        fixture.service.acquire(launch, now).unwrap().account.id,
        "two"
    );
}

#[test]
fn unverifiable_repository_rule_names_the_broken_route_and_never_falls_back() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    fixture.save(&account_fixture("two", now));
    let root = fixture.directory.join("repo");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    save_routes(
        &fixture,
        serde_json::json!([route(&root, "repository", "pinned", Some("one"))]),
    )
    .unwrap();
    let stored = fixture.service.snapshot().unwrap().policy.project_routes[0]
        .path
        .clone();
    std::fs::remove_dir_all(root.join(".git")).unwrap();
    let mut launch = request("broken-repository");
    launch.intent.cwd = fixture.directory.to_string_lossy().into_owned();
    launch.intent.account_id = Some("two".into());
    let failure = fixture.service.acquire(launch, now).unwrap_err();
    assert_eq!(failure.code, "REPOSITORY_UNVERIFIED");
    assert!(failure.message.contains(&stored));
    std::fs::remove_dir(&root).unwrap();
    let mut launch = request("missing-repository");
    launch.intent.cwd = fixture.directory.to_string_lossy().into_owned();
    launch.intent.account_id = Some("two".into());
    let failure = fixture.service.acquire(launch, now + 1).unwrap_err();
    assert_eq!(failure.code, "REPOSITORY_UNVERIFIED");
    assert!(failure.message.contains(&stored));
}

#[test]
fn repository_routes_follow_linked_worktrees_and_reject_duplicate_aliases() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let root = fixture.directory.join("repo");
    let linked = fixture.directory.join("linked");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q"]);
    git(
        &root,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    );
    git(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ],
    );
    save_routes(
        &fixture,
        serde_json::json!([route(&root, "repository", "pinned", Some("one"))]),
    )
    .unwrap();
    let mut launch = intent();
    launch.cwd = linked.to_string_lossy().into_owned();
    assert_eq!(resolve(&fixture, &launch).source, "repository");
    assert_eq!(
        resolve(&fixture, &launch).account_id.as_deref(),
        Some("one")
    );
    assert_eq!(
        save_routes(
            &fixture,
            serde_json::json!([
                route(&root, "repository", "pinned", Some("one")),
                route(&linked, "repository", "unmanaged", None)
            ])
        )
        .unwrap_err()
        .code,
        "DUPLICATE_ROUTE"
    );
    save_routes(
        &fixture,
        serde_json::json!([
            route(&root, "repository", "pinned", Some("one")),
            route(&linked, "directory", "unmanaged", None)
        ]),
    )
    .unwrap();
    assert_eq!(resolve(&fixture, &launch).mode, RouteMode::Unmanaged);
    let mut acquire = request("unmanaged");
    acquire.intent = launch;
    assert_eq!(
        fixture.service.acquire(acquire, now).unwrap_err().code,
        "UNMANAGED_ROUTE"
    );
}

#[test]
fn disabled_project_pin_never_falls_back_and_invalid_updates_are_atomic() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut one = account_fixture("one", now);
    one.enabled = false;
    fixture.save(&one);
    fixture.save(&account_fixture("two", now));
    save_routes(
        &fixture,
        serde_json::json!([route(
            &fixture.directory,
            "directory",
            "pinned",
            Some("one")
        )]),
    )
    .unwrap();
    let mut launch = request("disabled");
    launch.intent.cwd = fixture.directory.to_string_lossy().into_owned();
    assert_eq!(
        fixture.service.acquire(launch, now).unwrap_err().code,
        "ACCOUNT_DISABLED"
    );
    let revision = fixture.service.snapshot().unwrap().policy.revision;
    let mut invalid = route(&fixture.directory, "directory", "pinned", Some("one"));
    invalid["tool"] = "omp".into();
    assert_eq!(
        save_routes(&fixture, serde_json::json!([invalid]))
            .unwrap_err()
            .code,
        "INVALID_ROUTE"
    );
    assert_eq!(
        fixture.service.snapshot().unwrap().policy.revision,
        revision
    );
    assert_eq!(
        fixture
            .service
            .dispatch(
                "policy.update",
                serde_json::json!({"expectedRevision": revision - 1, "projectRoutes": []})
            )
            .unwrap_err()
            .code,
        "POLICY_CONFLICT"
    );
}

#[test]
fn changed_project_defaults_cannot_move_a_resume_and_host_uuid_cannot_collide() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    fixture.save(&account_fixture("two", now));
    let native = new_id();
    let mut launch = request("original-host");
    launch.intent.account_id = Some("one".into());
    launch.intent.native_session_id = Some(native.clone());
    launch.intent.cwd = fixture.directory.to_string_lossy().into_owned();
    let original = fixture.service.acquire(launch, now).unwrap();
    assert_eq!(original.session.native_session_id.as_ref(), Some(&native));
    fixture.service.finish(abort(&original), true, now).unwrap();
    save_routes(
        &fixture,
        serde_json::json!([route(
            &fixture.directory,
            "directory",
            "pinned",
            Some("two")
        )]),
    )
    .unwrap();
    let mut resume = request("resume-host");
    resume.intent.cwd = fixture.directory.to_string_lossy().into_owned();
    resume.intent.resume_session_id = Some(native.to_uppercase());
    let resumed = fixture.service.acquire(resume, now + 1).unwrap();
    assert_eq!(resumed.account.id, "one");
    fixture
        .service
        .finish(abort(&resumed), true, now + 2)
        .unwrap();
    let mut collision = request("duplicate-host");
    collision.intent.native_session_id = Some(native);
    collision.intent.account_id = Some("two".into());
    assert_eq!(
        fixture
            .service
            .acquire(collision, now + 3)
            .unwrap_err()
            .code,
        "NATIVE_SESSION_CONFLICT"
    );
    let mut collision = request("duplicate-aam-id");
    collision.intent.native_session_id = Some(original.session.id.clone());
    collision.intent.account_id = Some("two".into());
    assert_eq!(
        fixture
            .service
            .acquire(collision, now + 4)
            .unwrap_err()
            .code,
        "NATIVE_SESSION_CONFLICT"
    );
    let mut resume = request("resume-again");
    resume.intent.resume_session_id = resumed.session.native_session_id.clone();
    let resumed_again = fixture.service.acquire(resume, now + 5).unwrap();
    assert_eq!(resumed_again.account.id, "one");
    assert_eq!(
        resumed_again.session.native_session_id,
        original.session.native_session_id
    );
}

#[test]
fn legacy_aam_and_native_namespace_collision_cannot_select_the_newer_conversation() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    fixture.save(&account_fixture("two", now));
    let mut first = request("first-conversation");
    first.intent.account_id = Some("one".into());
    first.intent.native_session_id = Some(new_id());
    let first = fixture.service.acquire(first, now).unwrap();
    fixture
        .service
        .finish(abort(&first), true, now + 1)
        .unwrap();
    let mut second = request("second-conversation");
    second.intent.account_id = Some("two".into());
    let second = fixture.service.acquire(second, now + 2).unwrap();
    fixture
        .service
        .finish(abort(&second), true, now + 3)
        .unwrap();
    {
        let store = fixture.service.lock().unwrap();
        let mut record = lease(&store.connection, &second.session.id).unwrap();
        record.session.native_session_id = Some(first.session.id.clone());
        save_lease(&store.connection, &record).unwrap();
    }
    let mut resume = request("ambiguous-resume");
    resume.intent.resume_session_id = Some(first.session.id);
    assert_eq!(
        fixture
            .service
            .dispatch(
                "route.resolve",
                serde_json::json!({"intent": resume.intent})
            )
            .unwrap_err()
            .code,
        "RESUME_UNVERIFIED"
    );
    assert_eq!(
        fixture.service.acquire(resume, now + 4).unwrap_err().code,
        "RESUME_UNVERIFIED"
    );
}

#[test]
fn legacy_native_conversation_with_different_account_bindings_is_not_resumable() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    fixture.save(&account_fixture("two", now));
    let mut first = request("first-binding");
    first.intent.account_id = Some("one".into());
    let first = fixture.service.acquire(first, now).unwrap();
    fixture
        .service
        .finish(abort(&first), true, now + 1)
        .unwrap();
    let mut second = request("second-binding");
    second.intent.account_id = Some("two".into());
    let second = fixture.service.acquire(second, now + 2).unwrap();
    fixture
        .service
        .finish(abort(&second), true, now + 3)
        .unwrap();
    {
        let store = fixture.service.lock().unwrap();
        let mut record = lease(&store.connection, &second.session.id).unwrap();
        record.session.native_session_id = first.session.native_session_id.clone();
        save_lease(&store.connection, &record).unwrap();
    }
    let mut resume = request("ambiguous-binding");
    resume.intent.resume_session_id = Some(second.session.id);
    assert_eq!(
        fixture.service.acquire(resume, now + 4).unwrap_err().code,
        "RESUME_UNVERIFIED"
    );
}

#[test]
fn children_require_parent_capability_and_keep_identity_with_independent_capacity() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut one = account_fixture("one", now);
    one.max_concurrency = 2;
    fixture.save(&one);
    fixture.save(&account_fixture("two", now));
    let mut launch = request("parent");
    launch.intent.account_id = Some("one".into());
    let parent = fixture.service.acquire(launch, now).unwrap();
    {
        let store = fixture.service.lock().unwrap();
        let mut record = lease(&store.connection, &parent.session.id).unwrap();
        record.session.state = "ACTIVE".into();
        record.session.process = Some(ProcessIdentity {
            pid: 999_999,
            started_at: "fixture".into(),
            boot_id: "fixture".into(),
        });
        save_lease(&store.connection, &record).unwrap();
    }
    fixture
        .service
        .dispatch(
            "policy.update",
            serde_json::json!({
                "expectedRevision": 1,
                "allocationMode": "priority",
                "accountPriority": ["two", "one"]
            }),
        )
        .unwrap();
    let mut child = request("unauthorized-child");
    child.intent.parent_session_id = Some(parent.session.id.clone());
    assert_eq!(
        fixture.service.acquire(child, now).unwrap_err().code,
        "LEASE_FORBIDDEN"
    );
    let mut child = request("child");
    child.parent_capability = Some(parent.capability.clone());
    child.intent.parent_session_id = Some(parent.session.id.clone());
    let granted = fixture.service.acquire(child, now).unwrap();
    assert_eq!(granted.account.id, "one");
    assert_eq!(
        granted.session.parent_session_id.as_ref(),
        Some(&parent.session.id)
    );
    let mut child = request("extra-child");
    child.parent_capability = Some(parent.capability.clone());
    child.intent.parent_session_id = Some(parent.session.id.clone());
    assert_eq!(
        fixture.service.acquire(child, now).unwrap_err().code,
        "CAPACITY_RESERVED"
    );
    let mut conflict = intent();
    conflict.parent_session_id = Some(parent.session.id);
    conflict.account_id = Some("two".into());
    assert_eq!(
        fixture
            .service
            .dispatch("route.resolve", serde_json::json!({"intent": conflict}))
            .unwrap_err()
            .code,
        "SWITCH_UNSUPPORTED"
    );
}

#[test]
fn cross_tool_parent_requires_one_verified_identity_binding() {
    let fixture = Fixture::new();
    let now = now_ms();
    let native = account_fixture("native", now);
    fixture.save(&native);
    let parent = fixture.service.acquire(request("parent"), now).unwrap();
    let mut session = parent.session;
    session.state = "ACTIVE".into();
    let mut codex = account_fixture("codex", now);
    codex.tool = "codex".into();
    codex.identity_key = native.identity_key.clone();
    let mut launch = intent();
    launch.tool = "codex".into();
    launch.parent_session_id = Some(session.id.clone());
    let policy = Policy::default();
    assert_eq!(
        routes::resolve(
            &[native.clone(), codex.clone()],
            &[session.clone()],
            &policy,
            &launch
        )
        .unwrap()
        .account_id
        .as_deref(),
        Some("codex")
    );
    let mut ambiguous = codex.clone();
    ambiguous.id = "ambiguous".into();
    assert_eq!(
        routes::resolve(
            &[native.clone(), codex.clone(), ambiguous],
            &[session.clone()],
            &policy,
            &launch
        )
        .unwrap_err()
        .code,
        "PARENT_IDENTITY_UNVERIFIED"
    );
    codex.verification = "unverified".into();
    assert_eq!(
        routes::resolve(&[native, codex], &[session], &policy, &launch)
            .unwrap_err()
            .code,
        "PARENT_IDENTITY_UNVERIFIED"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn child_wrapper_cannot_claim_an_unrelated_live_process() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let grant = fixture.service.acquire(request("parent"), now).unwrap();
    let process = process_identity(std::process::id()).unwrap();
    {
        let store = fixture.service.lock().unwrap();
        let mut record = lease(&store.connection, &grant.session.id).unwrap();
        record.session.state = "ACTIVE".into();
        record.session.process = Some(process.clone());
        record.session.supervisor = Some(process.clone());
        save_lease(&store.connection, &record).unwrap();
    }
    for root in [false, true] {
        let result = fixture.service.dispatch(
            "lease.validate-child",
            serde_json::json!({
                "sessionId": grant.session.id, "capability": grant.capability,
                "accountId": grant.account.id, "process": process, "rootSpawn": root
            }),
        );
        assert_eq!(result.unwrap_err().code, "PARENT_PROCESS_UNVERIFIED");
    }
}

#[test]
fn legacy_policy_and_sessions_keep_compatible_defaults() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let grant = fixture.service.acquire(request("legacy"), now).unwrap();
    let mut session = serde_json::to_value(grant.session).unwrap();
    session.as_object_mut().unwrap().remove("parentSessionId");
    assert!(serde_json::from_value::<Session>(session)
        .unwrap()
        .parent_session_id
        .is_none());
    let mut policy = serde_json::to_value(Policy::default()).unwrap();
    policy.as_object_mut().unwrap().remove("projectRoutes");
    policy.as_object_mut().unwrap().remove("allocationMode");
    policy.as_object_mut().unwrap().remove("accountPriority");
    let legacy: Policy = serde_json::from_value(policy).unwrap();
    assert!(legacy.project_routes.is_empty());
    assert_eq!(legacy.allocation_mode, AllocationMode::Smart);
    assert!(legacy.account_priority.is_empty());
    assert_eq!(
        scheduler::decide(
            &fixture.service.snapshot().unwrap().accounts,
            &[],
            &legacy,
            &intent(),
            now,
        )
        .unwrap()
        .selected_account_id
        .as_deref(),
        Some("one")
    );
}
