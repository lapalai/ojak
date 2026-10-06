use super::*;

/// 실제 Claude 프로필 구조(projects/<프로젝트>/<UUID>.jsonl)로 소유 근거를 만듭니다.
fn claude_profile(fixture: &Fixture, id: &str, now: i64, conversation: &str, cwd: &str) -> Account {
    let mut account = account_fixture(id, now);
    let profile = fixture.directory.join(format!("profile-{id}"));
    let project = profile.join("projects").join("encoded-project");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join(format!("{conversation}.jsonl")),
        format!("{}\n", serde_json::json!({"type":"user","cwd":cwd})),
    )
    .unwrap();
    account.profile_path = Some(profile.to_string_lossy().into_owned());
    fixture.save(&account);
    account
}

fn adopt(fixture: &Fixture, tool: &str, native: &str) -> Result<Takeover, ApiError> {
    fixture
        .service
        .dispatch(
            "takeover.adopt",
            serde_json::json!({"tool": tool, "nativeSessionId": native}),
        )
        .map(|value| serde_json::from_value(value).unwrap())
}

fn resume(id: &str, native: &str, cwd: &str) -> Acquire {
    let mut launch = request(id);
    launch.intent.cwd = cwd.into();
    launch.intent.resume_session_id = Some(native.into());
    launch
}

#[test]
fn external_conversation_is_adopted_to_its_own_account_and_keeps_the_conversation_file() {
    let fixture = Fixture::new();
    let now = now_ms();
    let project = fixture.directory.join("work");
    std::fs::create_dir(&project).unwrap();
    let cwd = project.canonicalize().unwrap().to_string_lossy().into_owned();
    let conversation = new_id();
    let owner = claude_profile(&fixture, "owner", now, &conversation, &cwd);
    // 같은 도구의 다른 계정이 있어도 대화 파일이 있는 계정만 선택해야 합니다.
    fixture.save(&account_fixture("other", now));
    let record = adopt(&fixture, "claude", &conversation).unwrap();
    assert_eq!(record.account_id, owner.id);
    assert_eq!(record.source, "manual");
    assert_eq!(record.cwd.as_deref(), Some(cwd.as_str()));
    assert!(record.evidence.contains("owner"));
    let resolution: RouteResolution = serde_json::from_value(
        fixture
            .service
            .dispatch(
                "route.resolve",
                serde_json::json!({"intent": resume("probe", &conversation, &cwd).intent}),
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(resolution.source, "takeover");
    assert_eq!(resolution.account_id.as_deref(), Some(owner.id.as_str()));
    let grant = fixture
        .service
        .acquire(resume("adopted", &conversation, &cwd), now)
        .unwrap();
    assert_eq!(grant.account.id, owner.id);
    // 인계 실행은 새 대화를 만들지 않고 확인된 외부 대화를 그대로 이어받습니다.
    assert_eq!(
        grant.session.native_session_id.as_deref(),
        Some(conversation.as_str())
    );
    // 관리 세션이 생기면 인계 등록은 목록에서 정리됩니다.
    assert!(fixture.service.snapshot().unwrap().takeovers.is_empty());
}

#[test]
fn automatic_takeover_can_be_turned_off_and_manual_records_still_apply() {
    let fixture = Fixture::new();
    let now = now_ms();
    let project = fixture.directory.join("work");
    std::fs::create_dir(&project).unwrap();
    let cwd = project.canonicalize().unwrap().to_string_lossy().into_owned();
    let conversation = new_id();
    let owner = claude_profile(&fixture, "owner", now, &conversation, &cwd);
    // 기본값은 자동 인계이므로 등록 없이도 소유 계정으로 재개합니다.
    let grant = fixture
        .service
        .acquire(resume("auto", &conversation, &cwd), now)
        .unwrap();
    assert_eq!(grant.account.id, owner.id);
    fixture
        .service
        .finish(abort(&grant), true, now + 1)
        .unwrap();
    let revision = fixture.service.snapshot().unwrap().policy.revision;
    fixture
        .service
        .dispatch(
            "policy.update",
            serde_json::json!({"expectedRevision": revision, "autoTakeover": false}),
        )
        .unwrap();
    let second = new_id();
    claude_profile(&fixture, "owner-two", now, &second, &cwd);
    assert_eq!(
        fixture
            .service
            .acquire(resume("blocked", &second, &cwd), now + 2)
            .unwrap_err()
            .code,
        "RESUME_UNKNOWN"
    );
    adopt(&fixture, "claude", &second).unwrap();
    assert_eq!(
        fixture
            .service
            .acquire(resume("manual", &second, &cwd), now + 3)
            .unwrap()
            .account
            .id,
        "owner-two"
    );
}

#[test]
fn takeover_needs_real_evidence_and_never_moves_a_conversation_to_another_account() {
    let fixture = Fixture::new();
    let now = now_ms();
    let project = fixture.directory.join("work");
    std::fs::create_dir(&project).unwrap();
    let cwd = project.canonicalize().unwrap().to_string_lossy().into_owned();
    let conversation = new_id();
    claude_profile(&fixture, "owner", now, &conversation, &cwd);
    fixture.save(&account_fixture("other", now));
    // 프로필에 대화 파일이 없는 식별자는 계정을 추정하지 않습니다.
    assert_eq!(
        adopt(&fixture, "claude", &new_id()).unwrap_err().code,
        "TAKEOVER_OWNER_UNKNOWN"
    );
    assert_eq!(
        adopt(&fixture, "claude", "not-a-uuid").unwrap_err().code,
        "TAKEOVER_UNSUPPORTED"
    );
    assert_eq!(
        adopt(&fixture, "codex", &conversation).unwrap_err().code,
        "TAKEOVER_UNSUPPORTED"
    );
    // 소유 계정이 아닌 계정을 지정한 재개는 대체하지 않고 중단합니다.
    let mut cross = resume("cross", &conversation, &cwd);
    cross.intent.account_id = Some("other".into());
    assert_eq!(
        fixture.service.acquire(cross, now).unwrap_err().code,
        "SWITCH_UNSUPPORTED"
    );
    adopt(&fixture, "claude", &conversation).unwrap();
    assert_eq!(fixture.service.snapshot().unwrap().takeovers.len(), 1);
    fixture
        .service
        .dispatch(
            "takeover.release",
            serde_json::json!({"tool": "claude", "nativeSessionId": conversation}),
        )
        .unwrap();
    assert!(fixture.service.snapshot().unwrap().takeovers.is_empty());
    assert_eq!(
        fixture
            .service
            .dispatch(
                "takeover.release",
                serde_json::json!({"tool": "claude", "nativeSessionId": conversation}),
            )
            .unwrap_err()
            .code,
        "TAKEOVER_NOT_FOUND"
    );
}

#[test]
fn adopted_conversation_keeps_its_account_even_when_a_project_rule_pins_another() {
    let fixture = Fixture::new();
    let now = now_ms();
    let project = fixture.directory.join("work");
    std::fs::create_dir(&project).unwrap();
    let cwd = project.canonicalize().unwrap().to_string_lossy().into_owned();
    let conversation = new_id();
    claude_profile(&fixture, "owner", now, &conversation, &cwd);
    fixture.save(&account_fixture("pinned-elsewhere", now));
    let revision = fixture.service.snapshot().unwrap().policy.revision;
    fixture
        .service
        .dispatch(
            "policy.update",
            serde_json::json!({"expectedRevision": revision, "projectRoutes": [
                {"path": project, "scope": "directory", "tool": "claude", "mode": "pinned", "accountId": "pinned-elsewhere", "model": null}
            ]}),
        )
        .unwrap();
    // 새 세션은 프로젝트 규칙을 따르고, 기존 대화는 자기 계정을 유지합니다.
    let mut fresh = request("fresh");
    fresh.intent.cwd = cwd.clone();
    assert_eq!(
        fixture.service.acquire(fresh, now).unwrap().account.id,
        "pinned-elsewhere"
    );
    assert_eq!(
        fixture
            .service
            .acquire(resume("adopted", &conversation, &cwd), now + 1)
            .unwrap()
            .account
            .id,
        "owner"
    );
}
