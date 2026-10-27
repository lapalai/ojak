use super::*;
use serde_json::json;

fn remove(fixture: &Fixture, id: &str) -> Result<serde_json::Value, ApiError> {
    fixture.service.dispatch("account.remove", json!({ "id": id }))
}
fn stored_policy(fixture: &Fixture) -> Policy {
    policy(&fixture.service.lock().unwrap().connection).unwrap()
}
fn dismissed(fixture: &Fixture) -> Vec<DismissedProfile> {
    dismissed_profiles(&fixture.service.lock().unwrap().connection).unwrap()
}

#[test]
fn remove_rejects_unknown_ids_unknown_fields_and_omp_rows() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut observed = account_fixture("observed", now);
    observed.tool = "omp".into();
    observed.can_launch = false;
    fixture.save(&observed);
    assert_eq!(remove(&fixture, "missing").unwrap_err().code, "ACCOUNT_NOT_FOUND");
    assert_eq!(remove(&fixture, "observed").unwrap_err().code, "INVALID_PARAMS");
    assert_eq!(
        fixture.service.dispatch("account.remove", json!({ "id": "observed", "force": true })).unwrap_err().code,
        "INVALID_PARAMS"
    );
    // 거절된 요청은 행도 정책 revision도 바꾸지 않는다.
    assert!(accounts(&fixture.service.lock().unwrap().connection).unwrap().iter().any(|account| account.id == "observed"));
    assert_eq!(stored_policy(&fixture).revision, 1);
    assert!(dismissed(&fixture).is_empty());
}

#[test]
fn remove_is_refused_while_a_lease_holds_capacity_and_keeps_history_afterwards() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    let grant = fixture.service.acquire(request("remove-busy"), now).unwrap();
    assert_eq!(grant.account.id, "one");
    assert_eq!(remove(&fixture, "one").unwrap_err().code, "SESSION_BUSY");
    assert!(accounts(&fixture.service.lock().unwrap().connection).unwrap().iter().any(|account| account.id == "one"));
    assert!(dismissed(&fixture).is_empty());
    assert_eq!(stored_policy(&fixture).revision, 1);

    fixture.service.finish(abort(&grant), true, now + 1).unwrap();
    let result = remove(&fixture, "one").unwrap();
    assert_eq!(result, json!({ "id": "one", "removedRoutes": 0 }));

    let store = fixture.service.lock().unwrap();
    assert!(accounts(&store.connection).unwrap().is_empty());
    // 세션 이력은 남고, 계정이 없어도 상태 조회와 이력 읽기가 깨지지 않는다.
    let history = leases(&store.connection).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].session.account_id, "one");
    drop(store);
    let snapshot = fixture.service.snapshot().unwrap();
    assert!(snapshot.accounts.is_empty());
    assert_eq!(snapshot.sessions.len(), 1);
}

#[test]
fn remove_clears_policy_references_pinned_routes_takeovers_and_reset_quote() {
    let fixture = Fixture::new();
    let now = now_ms();
    fixture.save(&account_fixture("one", now));
    fixture.save(&account_fixture("two", now));
    let second = fixture.directory.join("second");
    let third = fixture.directory.join("third");
    std::fs::create_dir_all(&second).unwrap();
    std::fs::create_dir_all(&third).unwrap();
    let saved: Policy = serde_json::from_value(fixture.service.dispatch("policy.update", json!({
        "expectedRevision": 1,
        "allocationMode": "priority",
        "accountPriority": ["one", "two"],
        "providerPins": {"anthropic": "one"},
        "projectAllowlist": {"one": [], "two": []},
        "projectRoutes": [
            {"path": fixture.directory, "scope": "directory", "tool": "claude", "mode": "pinned", "accountId": "one"},
            {"path": second, "scope": "directory", "tool": "claude", "mode": "pinned", "accountId": "two"},
            {"path": third, "scope": "directory", "tool": "claude", "mode": "automatic", "accountId": null}
        ]
    })).unwrap()).unwrap();
    let takeover = |account: &str, native: &str| Takeover {
        tool: "claude".into(),
        native_session_id: native.into(),
        account_id: account.into(),
        cwd: None,
        adopted_at: now,
        source: "manual".into(),
        evidence: "fixture".into(),
    };
    {
        let store = fixture.service.lock().unwrap();
        set_takeovers(&store.connection, &[takeover("one", "native-one"), takeover("two", "native-two")]).unwrap();
        set_metadata(&store.connection, "resetQuote:one", &json!({"cached": true})).unwrap();
        set_metadata(&store.connection, "resetQuote:two", &json!({"cached": true})).unwrap();
    }

    let result = remove(&fixture, "one").unwrap();
    assert_eq!(result, json!({ "id": "one", "removedRoutes": 1 }));

    let after = stored_policy(&fixture);
    assert_eq!(after.revision, saved.revision + 1);
    assert_eq!(after.account_priority, vec!["two"]);
    assert!(after.provider_pins.is_empty());
    assert_eq!(after.project_allowlist.keys().collect::<Vec<_>>(), vec!["two"]);
    // 지운 계정에 고정된 규칙만 사라지고, 다른 계정 고정·자동 규칙은 그대로다.
    assert_eq!(after.project_routes.len(), 2);
    assert!(after.project_routes.iter().all(|route| route.account_id.as_deref() != Some("one")));
    assert!(after.project_routes.iter().any(|route| route.account_id.as_deref() == Some("two")));
    let store = fixture.service.lock().unwrap();
    assert_eq!(takeovers(&store.connection).unwrap().iter().map(|record| record.account_id.as_str()).collect::<Vec<_>>(), vec!["two"]);
    assert!(metadata::<serde_json::Value>(&store.connection, "resetQuote:one").unwrap().is_none());
    assert!(metadata::<serde_json::Value>(&store.connection, "resetQuote:two").unwrap().is_some());
}

#[test]
fn removing_an_account_nothing_refers_to_does_not_bump_the_policy_revision() {
    let fixture = Fixture::new();
    fixture.save(&account_fixture("lonely", now_ms()));
    remove(&fixture, "lonely").unwrap();
    assert_eq!(stored_policy(&fixture).revision, 1);
}

/// 빈 카탈로그로 시작한 느린 조회 도중 사용자가 그 프로필을 등록했다가 해제하면,
/// 늦게 도착한 진행 결과(publish)와 최종 결과(apply)가 해제된 계정을 되살리지 않는다.
#[test]
fn a_late_scan_result_cannot_resurrect_a_profile_removed_during_the_scan() {
    let fixture = Fixture::new();
    let now = now_ms();
    let mut found = account_fixture("found", now);
    found.profile_path = Some("/profiles/claude-late".into());
    // 조회 시작 시점의 기준(before)은 비어 있다. 그 사이 등록 → 해제가 끝난다.
    fixture.save(&found);
    remove(&fixture, "found").unwrap();
    assert_eq!(dismissed(&fixture), vec![DismissedProfile { tool: "claude".into(), path: "/profiles/claude-late".into() }]);

    assert!(fixture.service.publish_account(None, &found).unwrap().is_none());
    fixture.service.apply_scan(Vec::new(), aam_adapters::ScanResult {
        accounts: vec![found.clone()], tools: vec![], notices: vec![], merged_observation_ids: vec![],
    }).unwrap();
    assert!(!accounts(&fixture.service.lock().unwrap().connection).unwrap().iter().any(|account| account.id == "found"));

    // 해제되지 않은 다른 프로필의 늦은 신규 결과는 그대로 저장된다.
    let mut other = account_fixture("other", now);
    other.profile_path = Some("/profiles/claude-other".into());
    assert!(fixture.service.publish_account(None, &other).unwrap().is_some());
}

#[test]
fn legacy_lease_foreign_key_is_dropped_so_accounts_with_history_can_be_removed() {
    let directory = std::env::temp_dir().join(format!("aam-service-legacy-{}", new_id()));
    std::fs::create_dir_all(&directory).unwrap();
    let database = directory.join("state.sqlite3");
    {
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection.execute_batch(
            "CREATE TABLE accounts (id TEXT PRIMARY KEY, body TEXT NOT NULL);
             CREATE TABLE metadata (key TEXT PRIMARY KEY, body TEXT NOT NULL);
             CREATE TABLE leases (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL REFERENCES accounts(id),
                request_id TEXT NOT NULL UNIQUE,
                client_id TEXT NOT NULL,
                payload_hash TEXT NOT NULL,
                capability TEXT NOT NULL,
                policy_revision INTEGER NOT NULL,
                binding_hash TEXT NOT NULL,
                account_body TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                heartbeat_at INTEGER NOT NULL,
                manual INTEGER NOT NULL,
                body TEXT NOT NULL
             );
             PRAGMA user_version=1;
             INSERT INTO accounts(id,body) VALUES ('old','{}');
             INSERT INTO leases VALUES ('lease','old','request','client','hash','cap',1,'binding','{}',0,0,0,'{\"state\":\"EXITED\"}');",
        ).unwrap();
    }
    let store = Store::open(&database).unwrap();
    store.connection.execute("DELETE FROM accounts WHERE id='old'", []).unwrap();
    let kept: i64 = store.connection.query_row("SELECT COUNT(*) FROM leases", [], |row| row.get(0)).unwrap();
    assert_eq!(kept, 1);
    drop(store);
    // 이미 새 형식이면 다시 만들지 않고 그대로 연다.
    Store::open(&database).unwrap();
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn register_rejects_relative_paths_and_expands_only_a_leading_tilde() {
    let fixture = Fixture::new();
    let register = |path: &str| {
        fixture.service.dispatch("account.register", json!({"tool": "claude", "label": "계정", "profilePath": path})).unwrap_err().code
    };
    assert_eq!(register("foo/bar"), "PROFILE_INVALID");
    assert_eq!(register("~someone/profile"), "PROFILE_INVALID");
    assert_eq!(register("./profile"), "PROFILE_INVALID");
    // `~/`는 사용자 홈으로 풀린다. 없는 폴더라서 "절대 경로 필요(PROFILE_INVALID)"가 아니라 "폴더 없음(PROFILE_MISSING)"이 된다.
    assert_eq!(register(&format!("~/aam-test-missing-{}", new_id())), "PROFILE_MISSING");
}

#[cfg(unix)]
mod with_fake_cli {
    use super::*;

    /// 폴더 이름으로 조직이 정해지는 가짜 공식 Claude. 프로필마다 다른 identity가 나온다.
    fn install_fake_claude(fixture: &Fixture) {
        use std::os::unix::fs::PermissionsExt;
        let binary = fixture.directory.join("claude");
        let staged = fixture.directory.join("claude.staged");
        std::fs::write(
            &staged,
            "#!/bin/sh\ncase \"$1\" in\n  auth) printf '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"apiProvider\":\"firstParty\",\"email\":\"person@invalid.test\",\"orgId\":\"org-%s\"}\\n' \"$(basename \"$CLAUDE_CONFIG_DIR\")\" ;;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&staged, &binary).unwrap();
        std::fs::write(
            fixture.directory.join("integration.json"),
            json!({"version": INTEGRATION_VERSION, "nativeBinaries": {"claude": binary}}).to_string(),
        )
        .unwrap();
    }
    /// 가짜 CLI를 막 만든 직후의 일시적 시작 실패(ETXTBSY)만 다시 시도한다.
    fn register(fixture: &Fixture, path: &std::path::Path) -> Result<serde_json::Value, ApiError> {
        let mut result = Err(ApiError::new("UNSET", ""));
        for _ in 0..40 {
            result = fixture.service.dispatch(
                "account.register",
                json!({"tool": "claude", "label": "계정", "profilePath": path}),
            );
            if !matches!(&result, Err(error) if error.code == "PROBE_START_FAILED") {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        result
    }
    fn profile(root: &std::path::Path, name: &str) -> std::path::PathBuf {
        let path = root.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path.canonicalize().unwrap()
    }
    fn candidates(fixture: &Fixture, home: &std::path::Path, dismissed: &[DismissedProfile]) -> Vec<std::path::PathBuf> {
        let mut found: Vec<_> = aam_adapters::discovery_candidates(home, &fixture.service.paths, &[], dismissed, true)
            .into_iter()
            .map(|(_, path)| path)
            .collect();
        found.sort();
        found
    }

    #[test]
    fn removed_default_and_managed_profiles_are_not_rediscovered_until_registered_again() {
        let fixture = Fixture::new();
        install_fake_claude(&fixture);
        let home = profile(&fixture.directory, "home");
        let default = profile(&home, ".claude");
        let managed = profile(&fixture.service.paths.profiles, &format!("claude-{}", uuid::Uuid::new_v4()));

        let default_account: Account = serde_json::from_value(register(&fixture, &default).unwrap()).unwrap();
        let managed_account: Account = serde_json::from_value(register(&fixture, &managed).unwrap()).unwrap();
        assert!(dismissed(&fixture).is_empty());

        remove(&fixture, &default_account.id).unwrap();
        remove(&fixture, &managed_account.id).unwrap();
        let both = vec![
            DismissedProfile { tool: "claude".into(), path: default.to_str().unwrap().into() },
            DismissedProfile { tool: "claude".into(), path: managed.to_str().unwrap().into() },
        ];
        assert_eq!(dismissed(&fixture), both);
        // 서비스가 스캔에 넘기는 목록 그대로를 쓴다. 두 후보 모두 빠진다.
        assert!(candidates(&fixture, &home, &dismissed(&fixture)).is_empty());
        // 끊기 전(=기록 없음)에는 두 폴더가 모두 후보라서, 위 결과가 기록 때문임을 확인한다.
        let mut expected = vec![default.clone(), managed.clone()];
        expected.sort();
        assert_eq!(candidates(&fixture, &home, &[]), expected);

        // 명시적 재등록은 그 폴더의 끊김 기록만 푼다. 다른 폴더의 기록은 그대로다.
        let again: Account = serde_json::from_value(register(&fixture, &managed).unwrap()).unwrap();
        assert_eq!(dismissed(&fixture), vec![both[0].clone()]);
        assert_eq!(candidates(&fixture, &home, &dismissed(&fixture)), vec![managed.clone()]);
        // 다시 끊으면 기록이 중복 없이 돌아온다.
        remove(&fixture, &again.id).unwrap();
        remove(&fixture, &again.id).unwrap_err();
        assert_eq!(dismissed(&fixture).len(), 2);
        assert!(candidates(&fixture, &home, &dismissed(&fixture)).is_empty());
    }

    #[test]
    fn register_is_refused_while_uninstall_is_prepared_and_changes_nothing() {
        let fixture = Fixture::new();
        install_fake_claude(&fixture);
        let path = profile(&fixture.directory, "blocked");
        fixture.service.dispatch("service.prepareUninstall", json!({})).unwrap();
        assert_eq!(register(&fixture, &path).unwrap_err().code, "ADMISSION_DISABLED");
        assert!(accounts(&fixture.service.lock().unwrap().connection).unwrap().is_empty());
        fixture.service.dispatch("service.resumeAdmission", json!({})).unwrap();
        assert!(register(&fixture, &path).is_ok());
    }

    #[test]
    fn a_moved_account_can_be_connected_at_the_new_path_only_after_the_old_connection_is_removed() {
        let fixture = Fixture::new();
        install_fake_claude(&fixture);
        // 같은 계정(identity)이 되도록 폴더 이름을 같게 하고 부모만 다르게 둔다.
        let old = profile(&profile(&fixture.directory, "old-place"), "same-name");
        let new = profile(&profile(&fixture.directory, "new-place"), "same-name");
        let connected: Account = serde_json::from_value(register(&fixture, &old).unwrap()).unwrap();
        let error = register(&fixture, &new).unwrap_err();
        assert_eq!(error.code, "ACCOUNT_ALREADY_CONNECTED");
        assert!(error.message.contains("먼저 끊은"), "{}", error.message);
        remove(&fixture, &connected.id).unwrap();
        let moved: Account = serde_json::from_value(register(&fixture, &new).unwrap()).unwrap();
        assert_eq!(moved.profile_path.as_deref(), new.to_str());
        assert_ne!(moved.id, connected.id);
    }
}
