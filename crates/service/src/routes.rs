use crate::scheduler::{canonical_directory, holds_capacity};
use aam_protocol::{
    Account, ApiError, LaunchIntent, Policy, ProjectRoute, RouteMode, RouteResolution, RouteScope,
    Session, NATIVE_DEFAULT_MODEL,
};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::Command,
};

pub(crate) fn repository(directory: &Path) -> Result<Option<PathBuf>, ApiError> {
    let mut command = Command::new("git");
    command.arg("-C").arg(directory).args([
        "rev-parse",
        "--path-format=absolute",
        "--git-common-dir",
    ]);
    // 호스트에서 상속한 Git 환경이 다른 저장소로 경로 해석을 바꾸면 안 됩니다.
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    // 창 없는 서비스가 콘솔 프로그램을 띄우면 Windows가 새 콘솔 창을 연다. 출력은 모두 돌려받는다.
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, 0x0800_0000);
    let result = command.output().map_err(|_| {
        ApiError::new(
            "REPOSITORY_UNVERIFIED",
            "Git 저장소 식별자를 확인할 수 없습니다.",
        )
    })?;
    if !result.status.success() {
        return Ok(None);
    }
    let text = std::str::from_utf8(&result.stdout)
        .map_err(|_| ApiError::new("INVALID_PROJECT", "Git 저장소 경로를 읽을 수 없습니다."))?
        .trim_end_matches(['\r', '\n']);
    canonical_directory(text).map(Some)
}

pub(crate) fn validate(
    mut routes: Vec<ProjectRoute>,
    accounts: &[Account],
) -> Result<Vec<ProjectRoute>, ApiError> {
    if routes.len() > 1024 {
        return Err(ApiError::new(
            "INVALID_ROUTE",
            "프로젝트 경로 규칙은 최대 1,024개입니다.",
        ));
    }
    let mut seen = BTreeSet::new();
    for route in &mut routes {
        if !["claude", "codex"].contains(&route.tool.as_str())
            || route
                .model
                .as_ref()
                .is_some_and(|model| !crate::safe_id(model))
            || (route.mode == RouteMode::Pinned) != route.account_id.is_some()
            || (route.mode == RouteMode::Unmanaged && route.model.is_some())
        {
            return Err(ApiError::new(
                "INVALID_ROUTE",
                "도구·모델·배정 모드와 계정 조합이 올바르지 않습니다.",
            ));
        }
        if let Some(id) = &route.account_id {
            if !accounts
                .iter()
                .any(|account| &account.id == id && account.tool == route.tool)
            {
                return Err(ApiError::new(
                    "INVALID_ROUTE",
                    "경로 규칙의 계정과 도구가 일치하지 않습니다.",
                ));
            }
        }
        let path = canonical_directory(&route.path)?;
        let identity = match route.scope {
            RouteScope::Directory => path.clone(),
            RouteScope::Repository => repository(&path)?.ok_or_else(|| {
                ApiError::new(
                    "INVALID_ROUTE",
                    "저장소 규칙에는 Git 작업 폴더가 필요합니다.",
                )
            })?,
        };
        if !seen.insert((
            route.tool.clone(),
            route.scope == RouteScope::Repository,
            identity,
        )) {
            return Err(ApiError::new(
                "DUPLICATE_ROUTE",
                "같은 도구와 경로 범위에 중복 또는 충돌하는 규칙이 있습니다.",
            ));
        }
        route.path = path
            .to_str()
            .ok_or_else(|| {
                ApiError::new(
                    "INVALID_PROJECT",
                    "프로젝트 경로를 문자로 표현할 수 없습니다.",
                )
            })?
            .to_owned();
    }
    Ok(routes)
}

pub(crate) fn same_identity(left: &Account, right: &Account) -> bool {
    left.id == right.id
        || (left.provider == right.provider
            && left.verification == "preflight-verified"
            && right.verification == "preflight-verified"
            && (left
                .identity_key
                .as_ref()
                .is_some_and(|key| !key.is_empty() && right.identity_key.as_ref() == Some(key))
                || left
                    .omp_credential_pins
                    .iter()
                    .any(|pin| !pin.hash.is_empty() && right.omp_credential_pins.contains(pin))))
}

pub(crate) fn original<'a>(
    sessions: &'a [Session],
    intent: &LaunchIntent,
) -> Result<Option<&'a Session>, ApiError> {
    let Some(id) = &intent.resume_session_id else {
        return Ok(None);
    };
    if intent.adopted {
        // 인계 대상은 관리 세션 기록이 아니라 확인된 외부 대화입니다.
        return Ok(None);
    }
    let mut matches = sessions
        .iter()
        .filter(|session| session.tool == intent.tool)
        .filter(|session| &session.id == id || session.native_session_id.as_ref() == Some(id));
    let first = matches.next().ok_or_else(|| {
        ApiError::new(
            "RESUME_UNKNOWN",
            "원래 관리 세션을 찾을 수 없습니다. 임의의 대화 기록은 가져오지 않습니다.",
        )
    })?;
    if matches.any(|candidate| candidate.native_session_id != first.native_session_id) {
        return Err(ApiError::new(
            "RESUME_UNVERIFIED",
            "세션 ID가 서로 다른 관리 대화에 연결되어 재개할 수 없습니다.",
        ));
    }
    if !matches!(first.tool.as_str(), "claude" | "codex") || first.native_session_id.is_none() {
        return Err(ApiError::new(
            "RESUME_UNVERIFIED",
            "원래 native 세션과 계정 연결을 검증하지 못했습니다.",
        ));
    }
    // 같은 native 대화의 관리 세션을 시간순으로 본다. 계정이 바뀐 곳은 반드시 바로 앞 세션에서 이어 간
    // 기록(`continued_from`)이어야 한다. 근거 없이 여러 계정에 걸친 대화는 재개하지 않는다.
    let mut chain: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.tool == first.tool && session.native_session_id == first.native_session_id)
        .collect();
    chain.sort_by_key(|session| (session.started_at, session.updated_at));
    for pair in chain.windows(2) {
        if pair[1].account_id != pair[0].account_id && pair[1].continued_from.as_deref() != Some(pair[0].id.as_str()) {
            return Err(ApiError::new(
                "RESUME_UNVERIFIED",
                "원래 native 대화가 여러 계정에 연결되어 재개할 수 없습니다.",
            ));
        }
    }
    let original = *chain.last().expect("first is in the chain");
    if holds_capacity(&original.state)
        || sessions.iter().any(|session| {
            session.tool == original.tool
                && session.native_session_id == original.native_session_id
                && holds_capacity(&session.state)
        })
    {
        return Err(ApiError::new(
            "SESSION_BUSY",
            "원래 세션이 실행 중이거나 종료 확인 중입니다.",
        ));
    }
    Ok(Some(original))
}

pub(crate) fn resolve(
    accounts: &[Account],
    sessions: &[Session],
    policy: &Policy,
    intent: &LaunchIntent,
) -> Result<RouteResolution, ApiError> {
    let cwd = canonical_directory(&intent.cwd)?;
    let resumed = original(sessions, intent)?;
    let parent = intent
        .parent_session_id
        .as_ref()
        .map(|id| {
            sessions
                .iter()
                .find(|session| &session.id == id && session.state == "ACTIVE")
                .ok_or_else(|| {
                    ApiError::new(
                        "PARENT_SESSION_UNKNOWN",
                        "실행 중인 부모 관리 세션을 확인할 수 없습니다.",
                    )
                })
        })
        .transpose()?;
    let inherited = parent
        .map(|parent| {
            let source = accounts
                .iter()
                .find(|account| account.id == parent.account_id)
                .ok_or_else(|| {
                    ApiError::new(
                        "PARENT_IDENTITY_UNVERIFIED",
                        "부모 계정의 identity를 확인할 수 없습니다.",
                    )
                })?;
            let bindings: Vec<_> = accounts
                .iter()
                .filter(|account| account.tool == intent.tool && same_identity(source, account))
                .collect();
            if source.tool == intent.tool {
                return Ok(source.id.clone());
            }
            match bindings.as_slice() {
                [binding] => Ok(binding.id.clone()),
                _ => Err(ApiError::new(
                    "PARENT_IDENTITY_UNVERIFIED",
                    "다른 도구의 동일 identity 연결이 없거나 모호합니다.",
                )),
            }
        })
        .transpose()?;
    if intent.adopted {
        let id = intent.account_id.clone().ok_or_else(|| {
            ApiError::new(
                "TAKEOVER_OWNER_UNKNOWN",
                "인계 대상의 소유 계정을 확인하지 못했습니다.",
            )
        })?;
        if inherited.as_deref().is_some_and(|parent| parent != id) {
            return Err(ApiError::new(
                "SWITCH_UNSUPPORTED",
                "부모 세션의 계정과 인계 대화의 소유 계정이 달라 자식 실행으로 재개할 수 없습니다.",
            ));
        }
        // 기존 대화는 프로젝트 규칙으로 계정을 바꾸지 않습니다. 규칙은 새 세션에만 적용합니다.
        return Ok(RouteResolution {
            mode: RouteMode::Pinned,
            account_id: Some(id),
            model: None,
            source: "takeover".into(),
            policy_revision: policy.revision,
            excluded_account_id: None,
        });
    }
    if intent.continue_elsewhere {
        let original = resumed.ok_or_else(|| {
            ApiError::new("CONTINUE_UNKNOWN", "이어 갈 원래 관리 세션을 지정해 주세요.")
        })?;
        if inherited.is_some() {
            return Err(ApiError::new(
                "SWITCH_UNSUPPORTED",
                "자식 실행은 다른 계정에서 이어 갈 수 없습니다.",
            ));
        }
        let source = accounts.iter().find(|account| account.id == original.account_id);
        if let (Some(source), Some(explicit)) = (source, intent.account_id.as_deref()) {
            if accounts
                .iter()
                .find(|account| account.id == explicit)
                .is_none_or(|target| same_identity(source, target))
            {
                return Err(ApiError::new(
                    "CONTINUE_SAME_ACCOUNT",
                    "이어 갈 계정은 원래 계정과 달라야 합니다.",
                ));
            }
        }
        // 원래 계정만 빼고 평소 규칙대로 고른다(모델은 원래 대화 모델). 프로젝트 규칙은 새 세션용이라 적용하지 않는다.
        return Ok(RouteResolution {
            mode: if intent.account_id.is_some() { RouteMode::Pinned } else { RouteMode::Automatic },
            account_id: intent.account_id.clone(),
            model: Some(original.model.clone()),
            source: "continue".into(),
            policy_revision: policy.revision,
            excluded_account_id: Some(original.account_id.clone()),
        });
    }
    let fixed = resumed
        .map(|session| session.account_id.as_str())
        .or(inherited.as_deref());
    if let Some(id) = fixed {
        if intent
            .account_id
            .as_deref()
            .is_some_and(|explicit| explicit != id)
            || inherited
                .as_deref()
                .is_some_and(|parent_id| parent_id != id)
        {
            return Err(ApiError::new(
                "SWITCH_UNSUPPORTED",
                "재개와 자식 실행은 원래 계정 identity를 변경할 수 없습니다.",
            ));
        }
        return Ok(RouteResolution {
            mode: RouteMode::Pinned,
            account_id: Some(id.to_owned()),
            model: resumed.map(|session| session.model.clone()),
            source: if resumed.is_some() {
                "resume"
            } else {
                "parent"
            }
            .into(),
            policy_revision: policy.revision,
            excluded_account_id: None,
        });
    }
    let mut matched = None;
    let mut depth = 0;
    for route in policy
        .project_routes
        .iter()
        .filter(|route| route.tool == intent.tool && route.scope == RouteScope::Directory)
    {
        let stored = Path::new(&route.path);
        if !cwd.starts_with(stored) {
            continue;
        }
        let path = canonical_directory(&route.path).map_err(|_| {
            ApiError::new(
                "ROUTE_CHANGED",
                format!("경로 규칙 '{}'의 위치를 확인할 수 없습니다. 규칙 경로를 복구하거나 수정하세요.", route.path),
            )
        })?;
        if path != stored {
            return Err(ApiError::new(
                "ROUTE_CHANGED",
                format!(
                    "경로 규칙 '{}'의 실제 위치가 바뀌었습니다. 규칙 경로를 복구하거나 수정하세요.",
                    route.path
                ),
            ));
        }
        let current_depth = path.components().count();
        if current_depth > depth {
            matched = Some(route);
            depth = current_depth;
        }
    }
    if matched.is_none()
        && policy
            .project_routes
            .iter()
            .any(|route| route.tool == intent.tool && route.scope == RouteScope::Repository)
    {
        let current = repository(&cwd)?;
        for route in policy
            .project_routes
            .iter()
            .filter(|route| route.tool == intent.tool && route.scope == RouteScope::Repository)
        {
            let path = canonical_directory(&route.path).map_err(|_| {
                ApiError::new(
                    "REPOSITORY_UNVERIFIED",
                    format!("저장소 규칙 '{}'의 경로를 확인할 수 없습니다. 저장소 경로를 복구하거나 규칙을 수정하세요.", route.path),
                )
            })?;
            if path != Path::new(&route.path) {
                return Err(ApiError::new(
                    "ROUTE_CHANGED",
                    format!("저장소 규칙 '{}'의 실제 위치가 바뀌었습니다. 저장소 경로를 복구하거나 규칙을 수정하세요.", route.path),
                ));
            }
            let root = repository(&path)
                .map_err(|_| {
                    ApiError::new(
                        "REPOSITORY_UNVERIFIED",
                        format!("저장소 규칙 '{}'의 Git 식별자를 확인할 수 없습니다. 저장소를 복구하거나 규칙을 수정하세요.", route.path),
                    )
                })?
                .ok_or_else(|| {
                    ApiError::new(
                        "REPOSITORY_UNVERIFIED",
                        format!("저장소 규칙 '{}'에서 Git 저장소를 찾을 수 없습니다. 저장소를 복구하거나 규칙을 수정하세요.", route.path),
                    )
                })?;
            if current.as_ref() == Some(&root) {
                matched = Some(route);
                break;
            }
        }
    }
    if let Some(route) = matched {
        if route.mode == RouteMode::Pinned
            && intent
                .account_id
                .as_ref()
                .is_some_and(|id| Some(id) != route.account_id.as_ref())
        {
            return Err(ApiError::new(
                "ROUTE_CONFLICT",
                "명시한 계정이 프로젝트 고정 계정과 충돌합니다.",
            ));
        }
        if route.mode == RouteMode::Unmanaged && intent.account_id.is_some() {
            return Err(ApiError::new(
                "ROUTE_CONFLICT",
                "관리 계정 지정과 비관리 경로 규칙을 함께 사용할 수 없습니다.",
            ));
        }
        return Ok(RouteResolution {
            mode: if intent.account_id.is_some() {
                RouteMode::Pinned
            } else {
                route.mode.clone()
            },
            account_id: intent
                .account_id
                .clone()
                .or_else(|| route.account_id.clone()),
            model: route.model.clone(),
            source: if route.scope == RouteScope::Directory {
                "directory"
            } else {
                "repository"
            }
            .into(),
            policy_revision: policy.revision,
            excluded_account_id: None,
        });
    }
    // 공급자 수동 배정은 여기서 고정하지 않는다. 쓸 수 없으면 넘어가야 하므로 배정 단계(scheduler)가 우선순위로 적용한다.
    let account_id = intent.account_id.clone();
    Ok(RouteResolution {
        mode: if account_id.is_some() {
            RouteMode::Pinned
        } else {
            RouteMode::Automatic
        },
        account_id,
        model: None,
        source: if intent.account_id.is_some() {
            "explicit"
        } else {
            "global"
        }
        .into(),
        policy_revision: policy.revision,
        excluded_account_id: None,
    })
}

pub(crate) fn effective(intent: &LaunchIntent, resolution: &RouteResolution) -> LaunchIntent {
    let mut effective = intent.clone();
    effective.account_id.clone_from(&resolution.account_id);
    if intent.model == NATIVE_DEFAULT_MODEL {
        if let Some(model) = &resolution.model {
            effective.model.clone_from(model);
        }
    }
    effective
}
