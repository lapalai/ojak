pub mod arguments;
mod descendants;
pub mod hosts;
pub mod install;
pub mod setup;
pub mod supervisor;

use aam_protocol::{
    call, new_id, now_ms, process_identity, read_frame, write_frame, Account, ApiError, LaunchIntent,
    LeaseGrant, Paths, ProcessIdentity, RouteMode, RouteResolution, RpcRequest, RpcResponse,
    Session, Snapshot, NATIVE_DEFAULT_MODEL, PROTOCOL_VERSION,
};
use serde_json::{json, Value};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    sync::mpsc,
    thread,
    time::Duration,
};

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|_| {
        ApiError::new(
            "PROTOCOL_MISMATCH",
            "관리 서비스 응답을 읽지 못했어요.",
        )
    })
}

pub fn read_snapshot(paths: &Paths) -> Result<Snapshot, ApiError> {
    decode(call(paths, "status.read", json!({}))?)
}

// heartbeat와 종료 보고가 native 종료·신호 전달을 장시간 붙잡지 않도록 제한합니다.
fn lifecycle_call(paths: &Paths, method: &str, params: Value) -> Result<Value, ApiError> {
    bounded_call(paths, method, params, Duration::from_secs(3))
}

fn bounded_call(
    paths: &Paths,
    method: &str,
    params: Value,
    timeout: Duration,
) -> Result<Value, ApiError> {
    let fail = |_| {
        ApiError::new(
            "DAEMON_UNAVAILABLE",
            "서비스에 상태를 알리지 못했어요. 돌아가고 있는 작업은 그대로 둬요.",
        )
    };
    let mut stream = aam_protocol::connect(&paths.socket).map_err(fail)?;
    if !aam_protocol::peer_is_self(&stream) {
        return Err(ApiError::new("DAEMON_UNTRUSTED", "서비스 연결의 소유자가 지금 사용자가 아니에요. 연결하지 않았어요."));
    }
    stream.set_read_timeout(Some(timeout)).map_err(fail)?;
    stream.set_write_timeout(Some(timeout)).map_err(fail)?;
    let request = RpcRequest::new(method, params);
    write_frame(&mut stream, &request).map_err(fail)?;
    let response: RpcResponse = read_frame(&mut stream).map_err(fail)?;
    if response.id != request.id || response.version != PROTOCOL_VERSION {
        return Err(ApiError::new(
            "PROTOCOL_MISMATCH",
            "상태 보고 응답이 요청과 달라요.",
        ));
    }
    if let Some(error) = response.error {
        return Err(error);
    }
    response
        .result
        .ok_or_else(|| ApiError::new("PROTOCOL_MISMATCH", "상태 보고 결과가 없어요."))
}

fn abort(paths: &Paths, grant: &LeaseGrant, reason: &str) {
    if lifecycle_call(
        paths,
        "lease.abort",
        json!({"sessionId":grant.session.id,"capability":grant.capability,"reason":reason}),
    )
    .is_err()
    {
        eprintln!(
            "aam: 쓰지 않은 예약을 정리했다고 알리지 못했어요. 서비스가 돌아오면 상태를 확인해 주세요."
        );
    }
}

fn intent_checked(mut intent: LaunchIntent) -> Result<LaunchIntent, ApiError> {
    if !["claude", "codex"].contains(&intent.tool.as_str()) {
        return Err(ApiError::new(
            "UNSUPPORTED_TOOL",
            "쓸 수 있는 도구는 claude, codex예요.",
        ));
    }
    if intent.model.trim().is_empty()
        || intent.model.starts_with('-')
        || intent.model.chars().any(char::is_control)
    {
        return Err(ApiError::new(
            "MODEL_REQUIRED",
            "모델 ID를 정확히 적어 주세요. 빈 값이나 옵션은 안 돼요.",
        ));
    }
    let cwd = std::fs::canonicalize(&intent.cwd)
        .map_err(|_| ApiError::new("INVALID_CWD", "작업 폴더가 없거나 열 수 없어요."))?;
    if !cwd.is_dir() {
        return Err(ApiError::new("INVALID_CWD", "작업 경로가 폴더가 아니에요."));
    }
    intent.cwd = cwd
        .to_str()
        .ok_or_else(|| ApiError::new("INVALID_CWD", "작업 폴더 경로를 읽지 못했어요."))?
        .to_owned();
    if let Some(parent) = std::env::var_os("AAM_PARENT_SESSION_ID") {
        let parent = parent
            .to_str()
            .filter(|value| !value.is_empty() && value.len() <= 128)
            .ok_or_else(|| {
                ApiError::new(
                    "PARENT_CONTEXT_INVALID",
                    "물려받은 부모 대화를 확인하지 못했어요.",
                )
            })?;
        if intent
            .parent_session_id
            .as_deref()
            .is_some_and(|requested| requested != parent)
        {
            return Err(ApiError::new(
                "PARENT_CONTEXT_CONFLICT",
                "물려받은 부모 대화를 다른 대화로 바꿀 수 없어요.",
            ));
        }
        intent.parent_session_id = Some(parent.to_owned());
    }
    Ok(intent)
}

fn resolve_resume(
    paths: &Paths,
    intent: &mut LaunchIntent,
    selection: Option<arguments::Resume>,
) -> Result<(), ApiError> {
    if selection.is_some() && intent.resume_session_id.is_some() {
        return Err(ApiError::new(
            "SESSION_CONFLICT",
            "다시 열 대상을 두 번 지정할 수 없어요.",
        ));
    }
    let selection = selection.or_else(|| {
        intent
            .resume_session_id
            .clone()
            .map(arguments::Resume::Session)
    });
    let Some(selection) = selection else {
        return Ok(());
    };
    // 명시 ID/path는 서비스가 관리 매핑만 조회합니다. OMP 실제 header UUID도 여기서 해석됩니다.
    if let arguments::Resume::Session(id) = selection {
        intent.resume_session_id = Some(id);
        return Ok(());
    }
    let snapshot = read_snapshot(paths)?;
    let original = select_continue(&snapshot.sessions, intent)?;
    intent.resume_session_id = Some(original.id.clone());
    Ok(())
}

fn select_continue<'a>(
    sessions: &'a [Session],
    intent: &LaunchIntent,
) -> Result<&'a Session, ApiError> {
    let eligible = |session: &&Session| session.tool == intent.tool;
    let mut latest: Option<&Session> = None;
    let mut ambiguous = false;
    for session in sessions.iter().filter(eligible).filter(|session| {
        Path::new(&session.cwd).canonicalize().ok().as_deref() == Some(Path::new(&intent.cwd))
    }) {
        match latest.map(|old| {
            (session.started_at, session.updated_at).cmp(&(old.started_at, old.updated_at))
        }) {
            None | Some(std::cmp::Ordering::Greater) => {
                latest = Some(session);
                ambiguous = false;
            }
            Some(std::cmp::Ordering::Equal) => ambiguous = true,
            Some(std::cmp::Ordering::Less) => {}
        }
    }
    let original = latest.ok_or_else(|| {
        ApiError::new(
            "SESSION_NOT_FOUND",
            "다시 열 대화를 찾지 못했어요. 계정이나 바깥 대화 파일은 추측하지 않아요.",
        )
    })?;
    if ambiguous {
        return Err(ApiError::new(
            "CONTINUE_AMBIGUOUS",
            "최근 대화를 하나로 고르지 못했어요. 대화 ID를 적어 다시 열어 주세요.",
        ));
    }
    if !matches!(original.tool.as_str(), "claude" | "codex") || original.native_session_id.is_none() {
        return Err(ApiError::new(
            "RESUME_UNSUPPORTED",
            "확인된 공식 대화 연결이 없는 세션이에요.",
        ));
    }
    if intent
        .account_id
        .as_ref()
        .is_some_and(|id| id != &original.account_id)
    {
        return Err(ApiError::new(
            "CROSS_ACCOUNT_RESUME_BLOCKED",
            "원래 대화와 다른 계정으로는 다시 열 수 없어요.",
        ));
    }
    Ok(original)
}

pub fn run_cli(paths: &Paths, tool: &str, args: Vec<OsString>) -> Result<ExitStatus, ApiError> {
    // 비관리 경로가 확인되기 전에는 native 모델·새 세션 옵션을 관리 규칙으로 해석하지 않습니다.
    let intent = LaunchIntent {
        tool: tool.into(),
        model: NATIVE_DEFAULT_MODEL.into(),
        cwd: current_cwd()?,
        account_id: None,
        parent_session_id: None,
        resume_session_id: None,
        native_session_id: None,
        adopted: false,
        continue_elsewhere: false,
    };
    run_inner(paths, intent, args.clone(), Some(args))
}

/// Codex 실행 중 이 실행의 프로세스 트리(루트와 관측한 자손)가 쓰기 모드로 연 `.jsonl` 파일을 모은다.
/// 끝난 뒤 이 중에서만 대화 ID를 찾는다(파일을 연 프로세스가 곧 소유 근거). 관측할 수 없는 OS에서는 비어 있다.
struct WriterWatch {
    stop: mpsc::Sender<()>,
    thread: thread::JoinHandle<std::collections::BTreeSet<PathBuf>>,
}
impl WriterWatch {
    /// `root`는 spawn 때 확인한 birth identity다. 매 샘플에서 birth가 그대로인 프로세스만 조회하고,
    /// 조회 뒤에도 같은 프로세스일 때만 결과를 받는다. 루트 birth가 달라지거나 사라지면 루트 종료로 보고
    /// 그 PID를 더는 부모·writer로 조회하지 않는다(재사용 PID의 무관한 트리를 채택하지 않음).
    fn start(root: ProcessIdentity) -> Self {
        let (send, receive) = mpsc::channel();
        let thread = thread::spawn(move || {
            let same = |identity: &ProcessIdentity| {
                aam_protocol::process_identity(identity.pid).as_ref().ok() == Some(identity)
            };
            let mut seen = std::collections::BTreeSet::new();
            let mut tree = descendants::Descendants::new(root.pid);
            let mut root_alive = true;
            loop {
                if root_alive && !same(&root) {
                    root_alive = false;
                    tree.root_lost();
                }
                tree.observe();
                let live = root_alive.then(|| root.clone()).into_iter().chain(tree.identities());
                for identity in live {
                    if !same(&identity) {
                        continue;
                    }
                    let files = aam_adapters::open_jsonl_writers(identity.pid);
                    if same(&identity) {
                        seen.extend(files);
                    }
                }
                match receive.recv_timeout(Duration::from_millis(200)) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
            seen
        });
        Self { stop: send, thread }
    }
    fn finish(self) -> Vec<PathBuf> {
        let _ = self.stop.send(());
        self.thread.join().map(|seen| seen.into_iter().collect()).unwrap_or_default()
    }
}

struct Heartbeat {
    stop: mpsc::Sender<()>,
    thread: thread::JoinHandle<()>,
}
impl Heartbeat {
    fn start(
        paths: Paths,
        grant: LeaseGrant,
        attempt: String,
        identity: Option<ProcessIdentity>,
    ) -> Self {
        let (send, receive) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut started = false;
            let mut warned = false;
            loop {
                let result = if !started {
                    if let Some(identity) = &identity {
                        lifecycle_call(&paths, "lease.started", json!({"sessionId":grant.session.id,"capability":grant.capability,"spawnAttemptId":attempt,"process":identity})).inspect(|_| { started = true; })
                    } else {
                        lifecycle_call(
                            &paths,
                            "lease.heartbeat",
                            json!({"sessionId":grant.session.id,"capability":grant.capability}),
                        )
                    }
                } else {
                    lifecycle_call(
                        &paths,
                        "lease.heartbeat",
                        json!({"sessionId":grant.session.id,"capability":grant.capability}),
                    )
                };
                if result.is_err() && !warned {
                    eprintln!("aam: 서비스 연결이 끊겼어요. 돌아가고 있는 작업은 멈추지 않고, 계정 사용은 그대로 둬요.");
                    warned = true;
                }
                match receive.recv_timeout(Duration::from_secs(10)) {
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
        });
        Self { stop: send, thread }
    }
    fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.thread.join();
    }
}

pub fn run(
    paths: &Paths,
    intent: LaunchIntent,
    native_args: Vec<OsString>,
) -> Result<ExitStatus, ApiError> {
    run_inner(paths, intent, native_args, None)
}

enum LaunchRoute {
    Managed(LaunchIntent, Vec<OsString>),
    Unmanaged(LaunchIntent, Vec<OsString>),
}

fn resolve_route(paths: &Paths, intent: &LaunchIntent) -> Result<RouteResolution, ApiError> {
    decode(call(paths, "route.resolve", json!({"intent":intent}))?)
}

fn unmanaged_launch(
    intent: LaunchIntent,
    route: &RouteResolution,
    original_args: Option<Vec<OsString>>,
) -> Result<LaunchRoute, ApiError> {
    if intent.parent_session_id.is_some()
        || intent.resume_session_id.is_some()
        || intent.account_id.is_some()
        || !matches!(route.source.as_str(), "directory" | "repository")
        || route.account_id.is_some()
        || route.model.is_some()
    {
        return Err(ApiError::new(
            "PROTOCOL_MISMATCH",
            "명시적인 비관리 경로 승인이 없거나 관리 세션 문맥과 충돌합니다.",
        ));
    }
    let args = original_args.ok_or_else(|| {
        ApiError::new(
            "UNMANAGED_SHIM_REQUIRED",
            "명시적 unmanaged 경로에서는 원본 인수를 보존하는 도구 명령을 사용하세요.",
        )
    })?;
    Ok(LaunchRoute::Unmanaged(intent, args))
}

fn prepare_launch(
    paths: &Paths,
    mut intent: LaunchIntent,
    native_args: Vec<OsString>,
    original_args: Option<Vec<OsString>>,
) -> Result<LaunchRoute, ApiError> {
    let (selection, native_args) = arguments::shim_resume(&intent.tool, &native_args)?;
    let continuing = matches!(selection, Some(arguments::Resume::Continue));
    let resolution =
        resolve_resume(paths, &mut intent, selection).and_then(|()| resolve_route(paths, &intent));
    let route = match resolution {
        Ok(route) => route,
        Err(error)
            if original_args.is_some()
                && intent.parent_session_id.is_none()
                && ((error.code == "RESUME_UNKNOWN"
                    && !continuing
                    && intent.resume_session_id.is_some())
                    || (continuing
                        && intent.resume_session_id.is_none()
                        && error.code == "SESSION_NOT_FOUND")) =>
        {
            // 미등록 native 대화만 별도의 경로 승인을 받을 수 있습니다.
            // BUSY·매핑·identity·RPC 오류는 이 분기로 들어오지 않습니다.
            if let Some(id) = &intent.resume_session_id {
                let exact = uuid::Uuid::parse_str(id).is_ok();
                if !exact {
                    return Err(ApiError::new(
                        "SESSION_REQUIRED",
                        "대화 이름·부분 ID 선택기는 관리 대화와 구별할 수 없습니다. 전체 UUID를 지정하세요.",
                    ));
                }
            }
            let mut base = intent.clone();
            base.resume_session_id = None;
            let route = resolve_route(paths, &base)?;
            if route.mode == RouteMode::Unmanaged {
                return unmanaged_launch(base, &route, original_args);
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    if route.mode == RouteMode::Unmanaged {
        return unmanaged_launch(intent, &route, original_args);
    }
    let native_args = if original_args.is_some() {
        let (model, args) = arguments::shim_model(&intent.tool, &native_args)?;
        intent.model = model;
        args
    } else {
        native_args
    };
    let (native_id, native_args) = arguments::shim_session_id(&intent.tool, &native_args)?;
    if let Some(native_id) = native_id {
        if intent.native_session_id.replace(native_id).is_some() {
            return Err(ApiError::new(
                "SESSION_CONFLICT",
                "새 native 세션 ID를 중복 지정했습니다.",
            ));
        }
    }
    arguments::validate_native_args(&intent.tool, &native_args)?;
    intent.account_id = route.account_id;
    if intent.model == NATIVE_DEFAULT_MODEL {
        if let Some(model) = route.model {
            intent.model = model;
        }
    }
    Ok(LaunchRoute::Managed(intent, native_args))
}

/// 관리 배정을 적용할 수 없는 상태입니다. 사용자의 실행 자체를 막지 않고 원본 CLI로 통과시킵니다.
/// 사용자가 명시한 제한(프로젝트 허용 목록·경로 규칙·계정 지정 충돌)은 여기에 넣지 않습니다.
fn management_unavailable(code: &str) -> bool {
    matches!(
        code,
        "NO_ELIGIBLE_ACCOUNT"
            | "QUOTA_EXHAUSTED"
            | "QUOTA_STALE"
            | "QUOTA_UNKNOWN"
            | "RESET_UNCONFIRMED"
            | "CAPACITY_RESERVED"
            | "AUTH_REQUIRED"
            | "ADAPTER_UNVERIFIED"
            | "ACCOUNT_DISABLED"
            | "AUTOMATIC_PAUSED"
            | "DAEMON_UNAVAILABLE"
            | "SERVICE_UNHEALTHY"
    )
}

/// 관리 실행을 준비하지 못한 이유를 알린 뒤 원본 CLI를 그대로 실행합니다. 계정은 공식 CLI가 결정합니다.
fn passthrough(
    paths: &Paths,
    tool: &str,
    cwd: &str,
    args: Vec<OsString>,
    error: &ApiError,
) -> ApiError {
    eprintln!(
        "aam: 관리 배정을 적용하지 못해 원본 {tool}을(를) 그대로 실행합니다. 이 실행은 관리 세션으로 기록되지 않습니다. ({}) {}",
        error.code, error.message
    );
    match install::registered_native(paths, tool) {
        Ok(binary) => exec_native(Command::new(binary).args(args).current_dir(cwd)),
        Err(error) => error,
    }
}

fn run_inner(
    paths: &Paths,
    intent: LaunchIntent,
    native_args: Vec<OsString>,
    original_args: Option<Vec<OsString>>,
) -> Result<ExitStatus, ApiError> {
    let session_persisted = !native_args
        .iter()
        .take_while(|arg| arg.as_os_str() != "--")
        .any(|arg| arg == "--no-session-persistence");
    if !cfg!(any(target_os = "macos", windows)) {
        return Err(ApiError::new(
            "UNSUPPORTED_PLATFORM",
            "검증된 native 관리 실행은 현재 macOS와 Windows만 지원합니다.",
        ));
    }
    // 물려받은 부모 세션이 이 프로세스의 조상이 아니면(부모 관리 세션 안에서 띄운 터미널 앱·tmux 등이
    // 환경 변수를 퍼뜨린 경우) 부모를 버리고 새로 배정한다. native 실행 전 단계에서만 한 번 다시 시도한다.
    let unparented = std::env::var_os("AAM_PARENT_SESSION_ID")
        .is_some()
        .then(|| (intent.clone(), native_args.clone(), original_args.clone()));
    let retry_unparented = |reason: &ApiError| -> Option<Result<ExitStatus, ApiError>> {
        let (intent, native_args, original_args) = unparented.clone()?;
        if !matches!(reason.code.as_str(), "PARENT_SESSION_UNKNOWN" | "PARENT_PROCESS_UNVERIFIED") {
            return None;
        }
        eprintln!("aam: 이 터미널이 물려받은 부모 대화 정보가 지금 실행과 맞지 않아 무시하고 새로 골라요. ({})", reason.code);
        std::env::remove_var("AAM_PARENT_SESSION_ID");
        std::env::remove_var("AAM_PARENT_CAPABILITY");
        Some(run_inner(paths, intent, native_args, original_args))
    };
    let intent = intent_checked(intent)?;
    // 관리 실행이 불가능해도 사용자의 실행을 막지 않기 위해 원본 인수를 남겨 둡니다.
    let fallback = original_args
        .clone()
        .map(|args| (intent.tool.clone(), intent.cwd.clone(), args));
    let prepared = prepare_launch(paths, intent, native_args, original_args);
    let (intent, native_args) = match (prepared, &fallback) {
        (Ok(LaunchRoute::Managed(intent, args)), _) => (intent, args),
        (Ok(LaunchRoute::Unmanaged(intent, args)), _) => {
            let binary = install::registered_native(paths, &intent.tool)?;
            return Err(exec_native(
                Command::new(binary)
                    .args(args)
                    .current_dir(&intent.cwd),
            ));
        }
        (Err(error), Some((tool, cwd, args))) if management_unavailable(&error.code) =>
        {
            let error = fallback_notice(paths, tool, error);
            return Err(passthrough(paths, tool, cwd, args.clone(), &error));
        }
        (Err(error), _) => return Err(error),
    };
    let acquired = call(
        paths,
        "lease.acquire",
        json!({"requestId":new_id(),"clientInstanceId":new_id(),"intent":intent,"parentCapability":std::env::var("AAM_PARENT_CAPABILITY").ok()}),
    );
    let grant: LeaseGrant = match (acquired, &fallback) {
        (Ok(value) , _) => decode(value)?,
        (Err(error), Some((tool, cwd, args))) if management_unavailable(&error.code) => {
            let error = fallback_notice(paths, tool, error);
            return Err(passthrough(paths, tool, cwd, args.clone(), &error));
        }
        (Err(error), _) => return retry_unparented(&error).unwrap_or(Err(error)),
    };
    if grant.session.state == "PREPARED"
        && grant.account.tool == intent.tool
        && !intent.account_id.as_ref().is_some_and(|id| id != &grant.account.id)
    {
        announce_chosen_account(paths, &intent, &grant);
    }
    if grant.session.state != "PREPARED"
        || grant.account.tool != intent.tool
        || intent
            .account_id
            .as_ref()
            .is_some_and(|id| id != &grant.account.id)
    {
        return Err(ApiError::new(
            "INVALID_LEASE",
            "반환된 예약은 이 실행 요청을 시작할 수 있는 상태가 아닙니다.",
        ));
    }
    let prepared = (|| {
        // 서비스 응답의 binary_path를 그대로 실행하지 않는다. 이 사용자가 `aam integration install`로
        // 기록한 원본 CLI(integration.json)와 같은 실행 파일일 때만 계획을 세우고 identity 확인을 한다.
        let registered = install::registered_native(paths, &intent.tool)?;
        let bound = grant.account.binary_path.as_deref().ok_or_else(|| {
            ApiError::new("BINARY_UNVERIFIED", "이 계정의 확인된 공식 CLI가 없어요.")
        })?;
        if install::native_program(Path::new(bound))? != registered {
            return Err(ApiError::new(
                "IDENTITY_MISMATCH",
                "서비스가 지정한 실행 파일이 이 사용자가 등록한 원본 CLI와 다릅니다. 실행하지 않았습니다.",
            ));
        }
        if intent.continue_elsewhere {
            // 서비스가 확인한 원래 세션(continued_from)의 계정에서 대화 기록만 새 계정 프로필로 옮긴다.
            let snapshot = read_snapshot(paths)?;
            let unknown = || ApiError::new("CONTINUE_UNKNOWN", "이어 갈 원래 대화를 확인하지 못했어요.");
            let original = grant.session.continued_from.as_ref()
                .and_then(|id| snapshot.sessions.iter().find(|session| &session.id == id))
                .ok_or_else(unknown)?;
            let source = snapshot.accounts.iter().find(|account| account.id == original.account_id).ok_or_else(unknown)?;
            let native = grant.session.native_session_id.as_deref().ok_or_else(unknown)?;
            aam_adapters::copy_conversation(source, &grant.account, native)?;
        }
        // 실패해도 실행은 막지 않는다(확장 없이 실행될 뿐 인증 경계는 그대로). 안전 검사는 합친 설정으로 다시 한다.
        if let Err(error) = aam_adapters::share_extensions(&grant.account) {
            eprintln!("aam: {} ({})", error.message, error.code);
        }
        let mut native_intent = intent.clone();
        native_intent.cwd = grant.session.cwd.clone();
        native_intent.resume_session_id = if intent.resume_session_id.is_some() {
            grant.session.native_session_id.clone()
        } else {
            None
        };
        let mut plan = aam_adapters::build_launch_plan(&grant.account, &native_intent)?;
        if intent.tool == "claude" && intent.resume_session_id.is_none() {
            let native_id = grant.session.native_session_id.as_ref().ok_or_else(|| {
                ApiError::new(
                    "SESSION_MAPPING_MISSING",
                    "native 세션 식별자가 준비되지 않았습니다.",
                )
            })?;
            plan.args.extend(["--session-id".into(), native_id.clone()]);
        }
        // 공식 self-exec 계약(같은 PID로 교체)은 Unix exec에서만 지킬 수 있다. Windows는 wrapper를 두지 않고,
        // Claude의 재실행은 같은 Job과 같은 프로필 환경을 물려받는다.
        if intent.tool == "claude" && cfg!(unix) {
            let wrapper = claude_wrapper_value()?;
            if std::env::var_os("CLAUDE_CODE_PROCESS_WRAPPER")
                .is_some_and(|value| value != wrapper.as_str())
            {
                return Err(ApiError::new(
                    "HOST_WRAPPER_CONFLICT",
                    "호스트가 지정한 Claude process wrapper를 자동으로 덮어쓰지 않습니다.",
                ));
            }
            plan.env
                .insert("CLAUDE_CODE_PROCESS_WRAPPER".into(), wrapper);
            plan.env.insert(
                "AAM_CLAUDE_ROOT_SESSION_ID".into(),
                grant.session.id.clone(),
            );
            plan.env.insert(
                "AAM_CLAUDE_ROOT_CAPABILITY".into(),
                grant.capability.clone(),
            );
            plan.env.insert(
                "AAM_CLAUDE_ROOT_ACCOUNT_ID".into(),
                grant.account.id.clone(),
            );
        }
        let program = install::native_program(Path::new(&plan.program))?;
        if program != registered || plan.account.id != grant.account.id {
            return Err(ApiError::new(
                "IDENTITY_MISMATCH",
                "실행 계획이 선택한 계정 또는 등록된 원본 CLI와 일치하지 않습니다.",
            ));
        }
        if intent.tool == "claude" {
            // 실행 중 공식 업데이트로 진입 링크가 바뀌어도 이 세션의 자기 재실행은 시작한 실행 파일로 확인합니다.
            plan.env.insert(
                "AAM_CLAUDE_ROOT_PROGRAM".into(),
                program.to_string_lossy().into_owned(),
            );
        }
        let supervisor = process_identity(std::process::id())?;
        // 최종 native identity를 확인하기 전에는 prompt를 IPC나 native 프로세스에 전달하지 않습니다.
        let evidence = aam_adapters::verify(&grant.account)?;
        if evidence.tier != "preflight-verified"
            || grant.account.identity_key.as_ref() != Some(&evidence.identity_key)
        {
            return Err(ApiError::new(
                "IDENTITY_MISMATCH",
                "실행 직전 확인한 계정이 선택한 계정과 일치하지 않습니다.",
            ));
        }
        Ok((plan, program, supervisor, evidence))
    })();
    let (plan, program, supervisor, evidence) = match prepared {
        Ok(value) => value,
        Err(error) => {
            abort(paths, &grant, "preflight-failed");
            return Err(error);
        }
    };
    let attempt = new_id();
    // 응답 유실 시 절대로 spawn을 재시도하거나 불확실 예약을 환급하지 않습니다.
    let starting = call(
        paths,
        "lease.starting",
        json!({"sessionId":grant.session.id,"capability":grant.capability,"generation":grant.session.generation,"spawnAttemptId":attempt,"supervisor":supervisor,"evidence":evidence}),
    );
    // 서비스가 부모 검증 실패를 명시적으로 돌려준 경우만 예약을 풀고 다시 배정한다(응답 유실은 해당 없음).
    if let Err(error) = &starting {
        if error.code == "PARENT_PROCESS_UNVERIFIED" && unparented.is_some() {
            abort(paths, &grant, "parent-context-stale");
            if let Some(result) = retry_unparented(error) {
                return result;
            }
        }
    }
    let starting: Session = decode(starting?)?;
    if starting.state != "STARTING"
        || starting.id != grant.session.id
        || starting.spawn_attempt_id.as_ref() != Some(&attempt)
    {
        return Err(ApiError::new(
            "INVALID_SPAWN_ACK",
            "서비스가 이 시작 시도를 승인하지 않았습니다. native 실행은 하지 않았습니다.",
        ));
    }
    // cmux가 기록하는 PID는 supervisor가 아니라 exec 후에도 유지되는 실제 native PID입니다.
    let mut command = if intent.tool == "claude" && std::env::var_os("CMUX_CLAUDE_PID").is_some() {
        let launcher = std::env::current_exe().map_err(exec_error)?;
        let mut command = Command::new(launcher);
        command.arg("--claude-native-exec").arg(program);
        command
    } else {
        Command::new(program)
    };
    if intent.tool == "claude" && !plan.env.contains_key("CLAUDE_CONFIG_DIR") {
        command.env_remove("CLAUDE_CONFIG_DIR");
    }
    // Codex 비대화형 재개는 `codex exec resume <id> ...` 순서여야 한다. 계획의 `resume <id>`를 exec 뒤로 옮긴다.
    let (plan_args, native_args) = if intent.tool == "codex"
        && intent.resume_session_id.is_some()
        && native_args.first().is_some_and(|arg| arg == "exec")
    {
        (std::iter::once(OsString::from("exec")).chain(plan.args.iter().map(OsString::from)).collect::<Vec<_>>(), native_args[1..].to_vec())
    } else {
        (plan.args.iter().map(OsString::from).collect(), native_args)
    };
    command
        .args(plan_args)
        .args(native_args)
        .envs(plan.env)
        .env("AAM_PARENT_SESSION_ID", &grant.session.id)
        .env("AAM_PARENT_CAPABILITY", &grant.capability)
        .current_dir(&grant.session.cwd);
    let mut heartbeat = None;
    let mut watch = None;
    let watch_codex = intent.tool == "codex" && grant.session.native_session_id.is_none();
    let result = supervisor::supervise(command, |identity| {
        if let (true, Some(identity)) = (watch_codex, identity) {
            watch = Some(WriterWatch::start(identity.clone()));
        }
        if identity.is_none() {
            eprintln!("aam: 시작 시각을 확인하지 못했어요. 작업은 그대로 두고, 계정 사용 상태는 불확실로 남겨요.");
        }
        heartbeat = Some(Heartbeat::start(
            paths.clone(),
            grant.clone(),
            attempt.clone(),
            identity.cloned(),
        ));
    });
    if let Some(heartbeat) = heartbeat {
        heartbeat.stop();
    }
    match result {
        Ok(outcome) => {
            let status = outcome.status;
            // 관측 가능한 로컬 자식만 확인합니다. 숨은 원격 daemon 소멸은 보장하지 않습니다.
            let reason = if outcome
                .background_processes
                .as_ref()
                .is_some_and(Vec::is_empty)
            {
                "native-exit-foreground-confirmed"
            } else {
                "native-exit-background-unverified"
            };
            #[cfg(unix)]
            let code = status.code().or_else(|| {
                use std::os::unix::process::ExitStatusExt;
                status.signal().map(|s| 128 + s)
            });
            #[cfg(windows)]
            let code = status.code();
            // Codex는 시작 때 대화 ID를 정할 수 없어 끝난 뒤 이 계정 프로필에서 찾는다(정확히 하나일 때만).
            let written = watch.take().map(WriterWatch::finish).unwrap_or_default();
            let discovered = (grant.account.tool == "codex" && grant.session.native_session_id.is_none())
                .then(|| aam_adapters::codex_session_from(&grant.account, &grant.session.cwd, &written))
                .flatten();
            if lifecycle_call(paths, "lease.release", json!({"sessionId":grant.session.id,"capability":grant.capability,"exitCode":code,"reason":reason,"sessionPersisted":session_persisted,"backgroundProcesses":outcome.background_processes,"nativeSessionId":discovered})).is_err() {
                eprintln!("aam: 종료를 저장하지 못했어요. 서비스가 돌아오면 조심스럽게 정리해요.");
            }
            let native = grant.session.native_session_id.clone().or(discovered);
            if let Some(next) = offer_continue(paths, &grant, native.as_deref()) {
                return next;
            }
            Ok(status)
        }
        Err(failure) => {
            if let Some(watch) = watch.take() {
                watch.finish();
            }
            if !failure.spawned {
                abort(paths, &grant, "spawn-failed");
            }
            Err(failure.error)
        }
    }
}

/// 대화형 Claude 실행이 끝났을 때, 쓰던 계정이 한도를 다 써서(또는 안전 여유량 아래라) 새 작업을 받을 수 없고
/// 다른 계정이 있으면 같은 대화를 그 계정에서 이어 열지 묻는다. Enter(기본 예)면 이어 열고 그 결과를 돌려준다.
/// 사용량 판단은 서비스의 현재 관측을 쓴다. 관측이 늦어 묻지 않았으면 `aam continue`로 직접 이어 갈 수 있다.
fn offer_continue(paths: &Paths, grant: &LeaseGrant, native: Option<&str>) -> Option<Result<ExitStatus, ApiError>> {
    use std::io::{BufRead, IsTerminal, Write};
    if !matches!(grant.account.tool.as_str(), "claude" | "codex") || native.is_none() {
        return None;
    }
    let base = LaunchIntent {
        tool: grant.account.tool.clone(),
        model: grant.session.model.clone(),
        cwd: grant.session.cwd.clone(),
        account_id: Some(grant.account.id.clone()),
        parent_session_id: None,
        resume_session_id: None,
        native_session_id: None,
        adopted: false,
        continue_elsewhere: false,
    };
    let same: aam_protocol::Decision = decode(call(paths, "route.explain", json!({"intent":base})).ok()?).ok()?;
    let spent = same.candidates.iter().find(|candidate| candidate.account_id == grant.account.id).is_some_and(|candidate| {
        !candidate.eligible
            && candidate.reasons.iter().any(|reason| reason.starts_with("QUOTA_EXHAUSTED"))
    });
    if !spent {
        return None;
    }
    let interactive = std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal()
        && std::env::var_os("AAM_PARENT_SESSION_ID").is_none();
    let next = LaunchIntent { account_id: None, resume_session_id: Some(grant.session.id.clone()), continue_elsewhere: true, ..base };
    let other: Option<aam_protocol::Decision> = call(paths, "route.explain", json!({"intent":next})).ok().and_then(|value| decode(value).ok());
    let target = other.as_ref().and_then(|decision| decision.selected_account_id.clone());
    let account = target.as_ref().and_then(|id| read_snapshot(paths).ok().and_then(|snapshot| {
        snapshot.accounts.iter().find(|account| &account.id == id).map(|account| (account.label.clone(), account.email.clone()))
    }));
    if !interactive || account.is_none() {
        eprintln!("aam: 이 계정은 한도를 다 썼어요. 다른 계정에서 이으려면 aam continue를 실행해 주세요.");
        return None;
    }
    let (label, email) = account?;
    let name = display_account_label(&label, email.as_deref(), privacy_masked(paths));
    eprint!("\naam: 이 계정은 한도를 다 써서 새 작업을 받을 수 없습니다. 같은 대화를 {name} 계정에서 이어 열까요? [Y/n] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).ok()?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "" | "y" | "yes" | "ㅛ") {
        return None;
    }
    Some(run_inner(paths, next, Vec::new(), None))
}

/// 한도 소진 뒤 같은 대화를 다른 계정에서 이어 연다(`aam continue`). `session`이 없으면 현재 폴더의 마지막 관리 대화
/// (`tool`이 없으면 Claude·Codex 중 더 최근 것).
/// 최신 사용량으로 고르도록 조회를 한 번 요청하고 끝날 때까지 잠시(최대 30초) 기다린다.
pub fn continue_elsewhere(paths: &Paths, tool: Option<String>, session: Option<String>, account: Option<String>, native_args: Vec<OsString>) -> Result<ExitStatus, ApiError> {
    let mut intent = LaunchIntent {
        tool: tool.clone().unwrap_or_else(|| "claude".into()),
        model: NATIVE_DEFAULT_MODEL.into(),
        cwd: current_cwd()?,
        account_id: None,
        parent_session_id: None,
        resume_session_id: None,
        native_session_id: None,
        adopted: false,
        continue_elsewhere: false,
    };
    let _ = call(paths, "quota.refresh", json!({}));
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let snapshot = loop {
        let snapshot = read_snapshot(paths)?;
        if !snapshot.refreshing || std::time::Instant::now() >= deadline {
            break snapshot;
        }
        thread::sleep(Duration::from_millis(500));
    };
    let original = match session {
        Some(id) => id,
        None => {
            let tools: Vec<String> = tool.map_or_else(|| vec!["claude".into(), "codex".into()], |tool| vec![tool]);
            let mut found: Vec<&Session> = Vec::new();
            let mut last_error = None;
            for tool in &tools {
                let probe = LaunchIntent { tool: tool.clone(), ..intent.clone() };
                match select_continue(&snapshot.sessions, &probe) {
                    Ok(session) => found.push(session),
                    Err(error) => last_error = Some(error),
                }
            }
            let latest = found.into_iter().max_by_key(|session| session.started_at).ok_or_else(|| {
                last_error.unwrap_or_else(|| ApiError::new("SESSION_NOT_FOUND", "이어 갈 대화를 찾지 못했어요."))
            })?;
            intent.tool = latest.tool.clone();
            latest.id.clone()
        }
    };
    run_inner(paths, LaunchIntent { account_id: account, resume_session_id: Some(original), continue_elsewhere: true, ..intent }, native_args, None)
}

/// 원본 CLI로 넘긴다. Unix는 같은 PID로 교체하고, Windows는 교체가 없으므로 같은 콘솔에서 실행해 끝날 때까지
/// 기다린 뒤 그 종료 코드로 끝낸다(관리 세션이 아닌 원본 실행이라 lease를 만들지 않는다).
fn exec_native(command: &mut Command) -> ApiError {
    #[cfg(unix)]
    {
        exec_error(command.exec())
    }
    #[cfg(windows)]
    {
        match command.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(error) => exec_error(error),
        }
    }
}

fn exec_error(_: std::io::Error) -> ApiError {
    ApiError::new(
        "NATIVE_EXEC_FAILED",
        "검증된 native 실행 파일로 프로세스를 교체하지 못했습니다.",
    )
}

fn claude_wrapper_value() -> Result<String, ApiError> {
    let executable = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_err(|_| {
            ApiError::new(
                "BINARY_UNVERIFIED",
                "현재 launcher의 절대 경로를 확인하지 못했습니다.",
            )
        })?;
    Ok(json!([executable, "--claude-process-wrapper"]).to_string())
}

/// 공식 self-exec 계약: argv/env를 재구성하지 않고 같은 PID에서 실행하며 추가 lease를 얻지 않습니다.
pub fn claude_process_wrapper(paths: &Paths, args: &[OsString]) -> Result<(), ApiError> {
    claude_exec(paths, args, false)
}

pub fn claude_native_exec(paths: &Paths, args: &[OsString]) -> Result<(), ApiError> {
    claude_exec(paths, args, true)
}

/// 자식 실행 파일은 계정의 현재 진입 경로 대상이거나 이 세션이 시작한 실행 파일이어야 합니다.
fn child_binary_allowed(binary: &Path, bound: Option<&Path>, root: Option<&Path>) -> bool {
    bound == Some(binary) || root == Some(binary)
}

fn claude_exec(paths: &Paths, args: &[OsString], root_spawn: bool) -> Result<(), ApiError> {
    let started = std::time::Instant::now();
    let required = |key| {
        std::env::var(key)
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                ApiError::new(
                    "CHILD_CONTEXT_MISSING",
                    "원래 관리 Claude 실행의 승인 정보가 없습니다.",
                )
            })
    };
    let session_id = required("AAM_CLAUDE_ROOT_SESSION_ID")?;
    let capability = required("AAM_CLAUDE_ROOT_CAPABILITY")?;
    let account_id = required("AAM_CLAUDE_ROOT_ACCOUNT_ID")?;
    if required("CLAUDE_CODE_PROCESS_WRAPPER")? != claude_wrapper_value()? {
        return Err(ApiError::new(
            "CHILD_WRAPPER_MISMATCH",
            "상속된 Claude 실행 wrapper가 현재 launcher와 다릅니다.",
        ));
    }
    let requested = args.first().ok_or_else(|| {
        ApiError::new(
            "CHILD_BINARY_MISSING",
            "공식 Claude self-exec 명령이 없습니다.",
        )
    })?;
    let binary = install::native_program(Path::new(requested))?;
    #[derive(serde::Deserialize)]
    struct ChildGrant {
        session: Session,
        account: Account,
    }
    let grant: ChildGrant = decode(bounded_call(
        paths,
        "lease.validate-child",
        json!({"sessionId":session_id,"capability":capability,"accountId":account_id,"process":process_identity(std::process::id())?,"rootSpawn":root_spawn}),
        Duration::from_millis(1200),
    )?)?;
    // 서비스 응답이 아니라 이 사용자가 등록한 원본 CLI(integration.json)에 고정한다. 세션 도중 공식
    // 업데이트로 진입 링크가 새 버전을 가리켜도, 같은 설치 폴더 안의 시작 당시 실행 파일(자기 재실행)은 허용한다.
    let registered = install::registered_native(paths, "claude")?;
    let root = std::env::var_os("AAM_CLAUDE_ROOT_PROGRAM")
        .and_then(|path| install::native_program(Path::new(&path)).ok())
        .filter(|path| path.parent() == registered.parent());
    if grant.session.id != session_id
        || grant.session.account_id != account_id
        || grant.account.id != account_id
        || grant.account.tool != "claude"
        || !child_binary_allowed(&binary, Some(&registered), root.as_deref())
    {
        return Err(ApiError::new(
            "CHILD_IDENTITY_MISMATCH",
            "자식 실행이 원래 Claude 계정·바이너리와 다릅니다.",
        ));
    }
    let profile =
        grant.account.profile_path.as_deref().ok_or_else(|| {
            ApiError::new("PROFILE_UNBOUND", "원래 Claude 프로필이 연결돼 있지 않아요.")
        })?;
    let inherited = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| aam_protocol::user_home().map(|home| home.join(".claude")));
    if inherited
        .and_then(|path| path.canonicalize().ok())
        .as_deref()
        != Some(Path::new(profile))
    {
        return Err(ApiError::new(
            "CHILD_PROFILE_MISMATCH",
            "자식 실행의 Claude 프로필이 원래 계정과 다릅니다.",
        ));
    }
    if started.elapsed() >= Duration::from_millis(2500) {
        return Err(ApiError::new(
            "CHILD_VALIDATION_TIMEOUT",
            "공식 self-exec 시간 안에 원래 예약을 확인하지 못했습니다.",
        ));
    }
    // 공식 내부 인증 환경변수도 그대로 유지합니다. 갱신·로그인 자체를 이 wrapper가 통제한다고 주장하지 않습니다.
    #[cfg(windows)]
    {
        let _ = (binary, requested, root_spawn);
        return Err(ApiError::new(
            "PLATFORM_UNSUPPORTED",
            "Windows에서는 Claude self-exec wrapper를 지원하지 않습니다.",
        ));
    }
    #[cfg(unix)]
    {
        let mut command = Command::new(binary);
        command.arg0(requested).args(&args[1..]);
        if root_spawn {
            command.env("CMUX_CLAUDE_PID", std::process::id().to_string());
        }
        Err(exec_error(command.exec()))
    }
}

pub fn account_login(
    paths: &Paths,
    tool: &str,
    label: &str,
    settings_digest: Option<&str>,
) -> Result<ExitStatus, ApiError> {
    if !cfg!(any(target_os = "macos", windows)) {
        return Err(ApiError::new(
            "UNSUPPORTED_PLATFORM",
            "native 계정 등록은 현재 macOS와 Windows만 지원합니다.",
        ));
    }
    // 계정 생성 전에 서비스 접근부터 확인하며 기존 로그인 파일을 복제하지 않습니다.
    read_snapshot(paths)?;
    let settings = settings_digest
        .map(|digest| aam_adapters::prepare_settings_import(tool, digest))
        .transpose()?;
    let plan = aam_adapters::enrollment_plan(paths, tool, label)?;
    if let Some(settings) = settings {
        aam_adapters::apply_settings_import(Path::new(&plan.profile_path), settings)?;
    } else {
        eprintln!("aam: 새 프로필의 기본 설정으로 시작해요. 기존 권한, MCP, skills, plugins, 로그인 파일은 복사하지 않아요.");
    }
    let program = install::native_program(Path::new(&plan.program))?;
    let mut command = Command::new(program);
    command.args(&plan.args).envs(&plan.env);
    let status = supervisor::supervise(command, |_| {})
        .map_err(|failure| failure.error)?
        .status;
    if status.success() {
        let account: Account = decode(call(
            paths,
            "account.register",
            json!({"tool":plan.tool,"label":plan.label,"profilePath":plan.profile_path}),
        )?)?;
        eprintln!(
            "aam: native 로그인 후 계정 등록을 완료했습니다. 상태: {} / {}",
            account.auth_status, account.verification
        );
        announce_integration(paths, &plan.tool);
    }
    Ok(status)
}

/// 이미 등록된 프로필에서 공식 로그인을 다시 연다. 새 프로필을 만들거나 자격 증명을 복사하지 않는다.
pub fn account_relogin(paths: &Paths, tool: &str, account_id: &str) -> Result<ExitStatus, ApiError> {
    if !cfg!(any(target_os = "macos", windows)) {
        return Err(ApiError::new(
            "UNSUPPORTED_PLATFORM",
            "native 계정 등록은 현재 macOS와 Windows만 지원합니다.",
        ));
    }
    if account_id.is_empty()
        || account_id.len() > 128
        || !account_id.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return Err(ApiError::new("INVALID_PARAMS", "다시 로그인할 계정을 확인하지 못했어요."));
    }
    let snapshot = read_snapshot(paths)?;
    let account = snapshot.accounts.iter().find(|account| account.id == account_id && account.tool == tool)
        .ok_or_else(|| ApiError::new("ACCOUNT_NOT_FOUND", "다시 로그인할 계정을 찾지 못했어요."))?;
    let profile = account.profile_path.clone().ok_or_else(|| {
        ApiError::new("PROFILE_UNBOUND", "이 계정에 연결된 프로필이 없어 다시 로그인할 수 없어요.")
    })?;
    let label = {
        let trimmed = account.label.trim();
        if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
            "계정".to_owned()
        } else if trimmed.chars().count() > 120 {
            trimmed.chars().take(80).collect()
        } else {
            trimmed.to_owned()
        }
    };
    let plan = aam_adapters::relogin_plan(paths, tool, &label, Path::new(&profile))?;
    eprintln!("aam: 기존 프로필에서 공식 로그인을 다시 열어요. 설정과 로그인 정보는 복사하지 않아요.");
    let program = install::native_program(Path::new(&plan.program))?;
    let mut command = Command::new(program);
    command.args(&plan.args).envs(&plan.env);
    let status = supervisor::supervise(command, |_| {}).map_err(|failure| failure.error)?.status;
    if status.success() {
        let account: Account = decode(call(
            paths,
            "account.register",
            json!({"tool":plan.tool,"label":plan.label,"profilePath":plan.profile_path}),
        )?)?;
        eprintln!(
            "aam: native 로그인 후 계정 등록을 완료했습니다. 상태: {} / {}",
            account.auth_status, account.verification
        );
        announce_integration(paths, &plan.tool);
    }
    Ok(status)
}

/// 사용자가 이미 연결을 설치했다면(integration.json 있음) 새 계정으로 실행 가능해진 도구의 shim을 더한다.
/// 설치 스크립트는 계정이 없을 때 연결을 설치하므로, 이게 없으면 첫 로그인 뒤에도 `claude`가 원본으로 실행된다.
/// 연결을 설치한 적이 없으면 아무것도 바꾸지 않는다.
pub fn extend_integration(paths: &Paths, tool: &str) -> Result<bool, ApiError> {
    if !paths.home.join("integration.json").is_file() || install::shim_installed(paths, tool)? {
        return Ok(false);
    }
    // 서비스가 막 시작해 도구 탐지가 끝나지 않았으면 설치 판단이 틀린다. 조회가 끝날 때까지 잠깐 기다린다.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while read_snapshot(paths).is_ok_and(|snapshot| snapshot.refreshing) && std::time::Instant::now() < deadline {
        thread::sleep(Duration::from_millis(300));
    }
    install::integration_install(paths)?;
    install::shim_installed(paths, tool)
}

fn announce_integration(paths: &Paths, tool: &str) {
    match extend_integration(paths, tool) {
        Ok(true) => eprintln!("aam: 새 터미널의 `{tool}` 명령이 이제 Ojak으로 연결돼요."),
        Ok(false) => {}
        Err(error) => eprintln!("aam: `{tool}` 연결을 추가하지 못했어요. 앱의 연결 화면에서 다시 설치해 주세요. ({})", error.code),
    }
}

pub fn current_cwd() -> Result<String, ApiError> {
    std::env::current_dir()
        .ok()
        .and_then(|p: PathBuf| p.into_os_string().into_string().ok())
        .ok_or_else(|| ApiError::new("INVALID_CWD", "현재 작업 폴더를 확인할 수 없습니다."))
}

/// 터미널에 계정 이름을 알릴 때, 기본 배정이 아니면 이유를 한 줄에 붙인다.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LaunchNotice {
    Default,
    Pinned,
    QuotaSwitch,
    PinFallback,
}

/// 앱이 보여주는 계정 이름. 가림이 켜져 있으면 이메일을 넣지 않는다.
fn display_account_label(label: &str, email: Option<&str>, masked: bool) -> String {
    let mut name = label.trim().to_owned();
    if !masked {
        if let Some(email) = email.map(str::trim).filter(|email| !email.is_empty()) {
            if !name.contains(email) {
                name = if name.is_empty() { email.to_owned() } else { format!("{name} · {email}") };
            }
        }
        return if name.is_empty() { "계정".into() } else { name };
    }
    if let Some(email) = email.map(str::trim).filter(|email| !email.is_empty()) {
        name = name.replace(email, "이메일 숨김");
    }
    let name = mask_email_tokens(&name);
    let name = name.trim().trim_matches(['·', ' ']).trim();
    if name.is_empty() { "계정".into() } else { name.to_owned() }
}

fn mask_email_tokens(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    while index < chars.len() {
        if let Some(end) = email_span(&chars, index) {
            out.push_str("이메일 숨김");
            index = end;
        } else {
            out.push(chars[index]);
            index += 1;
        }
    }
    out
}

fn email_span(chars: &[char], start: usize) -> Option<usize> {
    let at = chars[start..].iter().position(|ch| *ch == '@')?;
    if at == 0 || chars[start..start + at].iter().any(|ch| !is_email_local(*ch)) {
        return None;
    }
    let domain = start + at + 1;
    if domain >= chars.len() {
        return None;
    }
    let mut end = domain;
    while end < chars.len() && is_email_domain(chars[end]) {
        end += 1;
    }
    let last_dot = (domain..end).rev().find(|index| chars[*index] == '.')?;
    if last_dot == domain || last_dot + 1 == end {
        return None;
    }
    if chars[last_dot + 1..end].iter().any(|ch| !ch.is_ascii_alphanumeric() && *ch != '_' && *ch != '-') {
        return None;
    }
    Some(end)
}

fn is_email_local(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '+' | '-')
}

fn is_email_domain(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-')
}

fn launch_announcement(label: &str, email: Option<&str>, masked: bool, notice: LaunchNotice) -> String {
    let name = display_account_label(label, email, masked);
    let why = match notice {
        LaunchNotice::Default => "",
        LaunchNotice::Pinned => " 고정된 계정이에요.",
        LaunchNotice::QuotaSwitch => " 다른 계정 한도 소진으로 전환했어요.",
        LaunchNotice::PinFallback => " 직접 고른 계정을 쓸 수 없어 바꿨어요.",
    };
    format!("aam: {name} 계정으로 실행해요.{why}")
}

fn user_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal() && std::io::stderr().is_terminal()
}

fn privacy_masked(paths: &Paths) -> bool {
    let Ok(bytes) = std::fs::read(paths.home.join("ui-settings.json")) else {
        return true;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return true;
    };
    value.get("privacy").and_then(serde_json::Value::as_str) != Some("visible")
}

fn announce_chosen_account(paths: &Paths, intent: &LaunchIntent, grant: &LeaseGrant) {
    if !user_terminal() {
        return;
    }
    let notice = if intent.continue_elsewhere {
        LaunchNotice::QuotaSwitch
    } else if grant.pin_unavailable {
        LaunchNotice::PinFallback
    } else if intent.account_id.is_some() {
        LaunchNotice::Pinned
    } else if read_snapshot(paths).ok().is_some_and(|snapshot| switched_for_quota(&snapshot, &grant.account)) {
        LaunchNotice::QuotaSwitch
    } else {
        LaunchNotice::Default
    };
    eprintln!(
        "{}",
        launch_announcement(&grant.account.label, grant.account.email.as_deref(), privacy_masked(paths), notice)
    );
}

fn switched_for_quota(snapshot: &Snapshot, selected: &Account) -> bool {
    let same: Vec<_> = snapshot.accounts.iter().filter(|account| account.tool == selected.tool && account.enabled).collect();
    let rank = |id: &str| snapshot.policy.account_priority.iter().position(|item| item == id).unwrap_or(usize::MAX);
    let Some(first) = same.iter().min_by_key(|account| rank(&account.id)) else {
        return false;
    };
    first.id != selected.id && quota_exhausted(first)
}

fn quota_exhausted(account: &Account) -> bool {
    account.buckets.iter().any(|bucket| bucket.status == "exhausted" || bucket.used_percent.unwrap_or(0.0) >= 100.0)
        && !account.buckets.iter().any(|bucket| bucket.status == "known" && bucket.used_percent.unwrap_or(100.0) < 100.0)
}

fn all_quota_exhausted(snapshot: &Snapshot, tool: &str) -> bool {
    let accounts: Vec<_> = snapshot.accounts.iter().filter(|account| account.tool == tool && account.enabled).collect();
    !accounts.is_empty() && accounts.iter().all(|account| quota_exhausted(account))
}

fn earliest_reset_phrase(snapshot: &Snapshot, tool: &str) -> Option<String> {
    let now = now_ms();
    let reset = snapshot.accounts.iter().filter(|account| account.tool == tool).flat_map(|account| account.buckets.iter()).filter_map(|bucket| bucket.resets_at).filter(|reset| *reset > now).min()?;
    let minutes = (reset - now) / 60_000;
    Some(if minutes < 60 {
        format!("{minutes}분 뒤")
    } else if minutes < 60 * 48 {
        format!("{}시간 뒤", minutes / 60)
    } else {
        format!("{}일 뒤", minutes / (60 * 24))
    })
}

fn default_profile_label(snapshot: &Snapshot, tool: &str, masked: bool) -> String {
    let folder = match tool {
        "claude" => ".claude",
        "codex" => ".codex",
        _ => return "공식 CLI 기본 프로필".into(),
    };
    let Some(home) = aam_protocol::user_home() else {
        return "공식 CLI 기본 프로필".into();
    };
    let profile = home.join(folder);
    let canonical = profile.canonicalize().ok();
    let Some(account) = snapshot.accounts.iter().find(|account| {
        account.tool == tool && account.profile_path.as_deref().is_some_and(|path| {
            canonical.as_ref().is_some_and(|resolved| Path::new(path) == resolved.as_path()) || Path::new(path) == profile
        })
    }) else {
        return "공식 CLI 기본 프로필".into();
    };
    display_account_label(&account.label, account.email.as_deref(), masked)
}

fn exhausted_fallback_line(tool: &str, earliest_reset: Option<&str>, original_account: &str) -> String {
    let until = match earliest_reset {
        Some(reset) => format!("가장 빠른 리셋은 {reset}예요."),
        None => "리셋 시각은 아직 몰라요.".into(),
    };
    format!("Ojak 계정이 모두 한도를 다 썼어요. {until} 원래 {tool}은 {original_account} 계정을 써요. 자세한 내용은 aam explain --tool {tool}에서 확인해 주세요.")
}

fn fallback_notice(paths: &Paths, tool: &str, mut error: ApiError) -> ApiError {
    let rewrite = error.message.contains("배정 설명")
        || matches!(error.code.as_str(), "NO_ELIGIBLE_ACCOUNT" | "QUOTA_EXHAUSTED");
    if !rewrite {
        return error;
    }
    let snapshot = read_snapshot(paths).ok();
    let masked = privacy_masked(paths);
    let original = snapshot.as_ref().map(|snapshot| default_profile_label(snapshot, tool, masked)).unwrap_or_else(|| "공식 CLI 기본 프로필".into());
    let quota = error.code == "QUOTA_EXHAUSTED" || snapshot.as_ref().is_some_and(|snapshot| all_quota_exhausted(snapshot, tool));
    error.message = if quota {
        let reset = snapshot.as_ref().and_then(|snapshot| earliest_reset_phrase(snapshot, tool));
        exhausted_fallback_line(tool, reset.as_deref(), &original)
    } else {
        format!("쓸 수 있는 Ojak 계정이 없어요. 원래 {tool}은 {original} 계정을 써요. 자세한 내용은 aam explain --tool {tool}에서 확인해 주세요.")
    };
    error
}

#[cfg(test)]
mod bridge_tests {
    use super::*;

    #[test]
    fn announcement_uses_the_label_and_hides_email_when_masked() {
        let masked = launch_announcement(
            "작업용 · person@example.com",
            Some("person@example.com"),
            true,
            LaunchNotice::QuotaSwitch,
        );
        assert!(masked.contains("작업용"), "{masked}");
        assert!(masked.contains("다른 계정 한도 소진으로 전환"), "{masked}");
        assert!(!masked.contains("person@example.com"), "{masked}");
        assert!(!masked.contains('@'), "{masked}");
        assert!(!masked.contains('\n'), "{masked}");
        let pinned = launch_announcement("작업용", Some("person@example.com"), false, LaunchNotice::Pinned);
        assert!(pinned.contains("person@example.com"), "{pinned}");
        assert!(pinned.contains("고정"), "{pinned}");
        let plain = launch_announcement("작업용", None, true, LaunchNotice::Default);
        assert!(plain.contains("작업용"), "{plain}");
        assert!(!plain.contains("전환"), "{plain}");
        assert!(!plain.contains("고정"), "{plain}");
        let fallback = exhausted_fallback_line("claude", Some("3시간 뒤"), "공식 CLI 기본 프로필");
        assert!(fallback.contains("aam explain --tool claude"), "{fallback}");
        assert!(fallback.contains("3시간 뒤"), "{fallback}");
        assert!(!fallback.contains("배정 설명"), "{fallback}");
    }


    fn session(id: &str, cwd: &str, account: &str, started: i64, state: &str) -> Session {
        serde_json::from_value(json!({
            "id":id, "requestId":id, "accountId":account, "tool":"claude", "model":"sonnet",
            "cwd":cwd, "state":state, "verification":"preflight-verified",
            "startedAt":started, "updatedAt":started, "process":null, "supervisor":null,
            "spawnAttemptId":null, "generation":"generation", "exitCode":0, "reason":null,
            "nativeSessionId":id
        }))
        .unwrap()
    }

    #[test]
    fn child_self_exec_survives_update_during_session_but_rejects_other_binaries() {
        let (old, new, other) = (Path::new("/v/1.0.0"), Path::new("/v/1.0.1"), Path::new("/x/claude"));
        // 세션 도중 공식 업데이트로 계정 진입 경로가 새 버전을 가리키게 된 상황입니다.
        assert!(child_binary_allowed(old, Some(new), Some(old)));
        assert!(child_binary_allowed(new, Some(new), Some(old)));
        assert!(!child_binary_allowed(other, Some(new), Some(old)));
        assert!(!child_binary_allowed(old, Some(new), None));
        assert!(!child_binary_allowed(old, None, None));
    }

    #[test]
    fn continue_uses_canonical_cwd_and_never_changes_the_original_account() {
        let cwd = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let sessions = vec![
            session("old", &cwd, "a", 1, "EXITED"),
            session("latest", &format!("{cwd}/."), "b", 2, "EXITED"),
            session("running", &cwd, "b", 3, "ACTIVE"),
            session("elsewhere", "/not-a-real-aam-project", "a", 4, "EXITED"),
        ];
        let mut intent = LaunchIntent {
            tool: "claude".into(),
            cwd,
            ..LaunchIntent::default()
        };
        assert_eq!(select_continue(&sessions, &intent).unwrap().id, "running");
        intent.account_id = Some("a".into());
        assert_eq!(
            select_continue(&sessions, &intent).unwrap_err().code,
            "CROSS_ACCOUNT_RESUME_BLOCKED"
        );
        assert_eq!(
            select_continue(&[], &intent).unwrap_err().code,
            "SESSION_NOT_FOUND"
        );
        let tied = vec![
            session("a", &intent.cwd, "a", 5, "EXITED"),
            session("b", &intent.cwd, "b", 5, "EXITED"),
        ];
        assert_eq!(
            select_continue(&tied, &intent).unwrap_err().code,
            "CONTINUE_AMBIGUOUS"
        );
    }

    fn intent(tool: &str) -> LaunchIntent {
        LaunchIntent {
            tool: tool.into(),
            model: NATIVE_DEFAULT_MODEL.into(),
            cwd: std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .to_str()
                .unwrap()
                .into(),
            ..LaunchIntent::default()
        }
    }

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn route(mode: &str, source: &str, model: Option<&str>) -> Result<Value, ApiError> {
        Ok(json!({
            "mode":mode, "source":source, "model":model, "policyRevision":1,
            "accountId": if mode == "pinned" { Some("original-account") } else { None }
        }))
    }

    fn snapshot(sessions: Vec<Session>) -> Result<Value, ApiError> {
        Ok(json!({
            "version":PROTOCOL_VERSION, "generatedAt":0, "serviceStartedAt":0,
            "accounts":[], "tools":[], "sessions":sessions, "policy":{
                "revision":1, "automatic":true, "providerPins":{},
                "safetyReservePercent":0, "staleAfterSeconds":60
            }, "notices":[], "refreshing":false, "lastRefreshAt":null
        }))
    }

    // 실제 IPC 응답을 경계로 실행 결정을 검증합니다. native 추론이나 사용자 설정은 사용하지 않습니다.
    fn prepared(
        intent: LaunchIntent,
        native: Vec<OsString>,
        shim: bool,
        responses: Vec<Result<Value, ApiError>>,
    ) -> Result<LaunchRoute, ApiError> {
        // Unix socket 경로 길이 제한 때문에 macOS는 짧은 /tmp를 쓴다.
        #[cfg(unix)]
        let home = PathBuf::from("/tmp").join(format!("aam-launch-{}", new_id()));
        #[cfg(windows)]
        let home = std::env::temp_dir().join(format!("aam-launch-{}", new_id()));
        let paths = Paths {
            socket: home.join("rpc.sock"),
            database: home.join("unused.sqlite3"),
            profiles: home.join("profiles"),
            home,
        };
        paths.prepare().unwrap();
        let listener = aam_protocol::LocalListener::bind(&paths.socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let mut listener = Some(listener);
            let count = responses.len();
            for (index, response) in responses.into_iter().enumerate() {
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.as_ref().unwrap().accept() {
                        Ok(stream) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "예상한 IPC 요청이 없습니다."
                            );
                            thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                if index + 1 == count {
                    listener.take();
                }
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let request: RpcRequest = read_frame(&mut stream).unwrap();
                let response = match response {
                    Ok(value) => RpcResponse::ok(request.id, value),
                    Err(error) => RpcResponse::err(request.id, error),
                };
                write_frame(&mut stream, &response).unwrap();
            }
        });
        let original = shim.then(|| native.clone());
        let result = prepare_launch(&paths, intent, native, original);
        let joined = server.join();
        std::fs::remove_dir_all(&paths.home).unwrap();
        joined.unwrap();
        result
    }

    fn error_code(result: Result<LaunchRoute, ApiError>) -> String {
        match result {
            Err(error) => error.code,
            Ok(_) => panic!("실행 계획 대신 오류가 필요합니다."),
        }
    }

    #[test]
    fn explicit_model_wins_over_project_and_original_defaults() {
        for source in ["directory", "resume"] {
            let mut launch = intent("claude");
            if source == "resume" {
                launch.resume_session_id = Some(new_id());
            }
            let LaunchRoute::Managed(explicit, _) = prepared(
                launch.clone(),
                args(&["--model=opus", "--print", "hello"]),
                true,
                vec![route("pinned", source, Some("sonnet"))],
            )
            .unwrap() else {
                panic!("관리 실행이어야 합니다.")
            };
            assert_eq!(explicit.model, "opus");
            let LaunchRoute::Managed(default, _) = prepared(
                launch.clone(),
                vec![],
                true,
                vec![route("pinned", source, Some("sonnet"))],
            )
            .unwrap() else {
                panic!("관리 실행이어야 합니다.")
            };
            assert_eq!(default.model, "sonnet");
            launch.model = "haiku".into();
            let LaunchRoute::Managed(direct, _) = prepared(
                launch,
                vec![],
                false,
                vec![route("pinned", source, Some("sonnet"))],
            )
            .unwrap() else {
                panic!("관리 실행이어야 합니다.")
            };
            assert_eq!(direct.model, "haiku");
        }
    }

    #[test]
    fn unknown_native_sources_need_separate_explicit_unmanaged_authorization() {
        let id = new_id();
        let original = args(&["--resume", &id, "--native-future-option=value"]);
        let LaunchRoute::Unmanaged(_, _) = prepared(
            intent("claude"),
            original,
            true,
            vec![
                Err(ApiError::new("RESUME_UNKNOWN", "미등록 대화")),
                route("unmanaged", "repository", None),
            ],
        )
        .unwrap() else {
            panic!("비관리 실행이어야 합니다.")
        };
        assert_eq!(
            error_code(prepared(
                intent("claude"),
                args(&["--resume", &id]),
                true,
                vec![
                    Err(ApiError::new("RESUME_UNKNOWN", "미등록 대화")),
                    route("automatic", "global", None)
                ],
            )),
            "RESUME_UNKNOWN"
        );
        assert_eq!(
            error_code(prepared(
                intent("claude"),
                args(&["--resume", &id]),
                true,
                vec![
                    Err(ApiError::new("RESUME_UNKNOWN", "미등록 대화")),
                    Err(ApiError::new("DAEMON_UNAVAILABLE", "연결 실패"))
                ],
            )),
            "DAEMON_UNAVAILABLE"
        );
    }

    #[test]
    fn only_management_unavailable_failures_may_run_the_original_cli() {
        for code in [
            "NO_ELIGIBLE_ACCOUNT",
            "QUOTA_EXHAUSTED",
            "CAPACITY_RESERVED",
            "AUTH_REQUIRED",
            "AUTOMATIC_PAUSED",
            "DAEMON_UNAVAILABLE",
        ] {
            assert!(management_unavailable(code), "{code}");
        }
        // 사용자가 직접 설정한 제한과 대화·계정 보호 규칙은 우회하지 않습니다.
        for code in [
            "PROJECT_NOT_ALLOWED",
            "ROUTE_CONFLICT",
            "ROUTE_CHANGED",
            "SWITCH_UNSUPPORTED",
            "CROSS_ACCOUNT_RESUME_BLOCKED",
            "RESUME_UNKNOWN",
            "RESUME_UNVERIFIED",
            "SESSION_BUSY",
            "IDENTITY_MISMATCH",
            "PARENT_IDENTITY_UNVERIFIED",
            "UNMANAGED_ROUTE",
        ] {
            assert!(!management_unavailable(code), "{code}");
        }
    }

    #[test]
    fn mapped_resume_and_parent_stay_managed() {
        let id = new_id();
        let LaunchRoute::Managed(resumed, _) = prepared(
            intent("claude"),
            args(&["--resume", &id]),
            true,
            vec![route("pinned", "resume", Some("sonnet"))],
        )
        .unwrap() else {
            panic!("원래 관리 계정이어야 합니다.")
        };
        assert_eq!(resumed.account_id.as_deref(), Some("original-account"));
        assert_eq!(resumed.resume_session_id.as_deref(), Some(id.as_str()));
        let mut child = intent("claude");
        child.parent_session_id = Some("parent".into());
        let LaunchRoute::Managed(inherited, _) = prepared(
            child.clone(),
            vec![],
            true,
            vec![route("pinned", "parent", None)],
        )
        .unwrap() else {
            panic!("부모 관리 계정이어야 합니다.")
        };
        assert_eq!(inherited.account_id.as_deref(), Some("original-account"));
        assert_eq!(
            error_code(prepared(
                child.clone(),
                args(&["--resume", &id]),
                true,
                vec![Err(ApiError::new("RESUME_UNKNOWN", "미등록 대화"))],
            )),
            "RESUME_UNKNOWN"
        );
        assert_eq!(
            error_code(prepared(
                child,
                vec![],
                true,
                vec![route("unmanaged", "directory", None)],
            )),
            "PROTOCOL_MISMATCH"
        );
    }

    #[test]
    fn known_resume_failures_and_unavailable_services_never_authorize_raw() {
        for code in [
            "SESSION_BUSY",
            "RESUME_UNVERIFIED",
            "SESSION_MAPPING_INVALID",
            "IDENTITY_MISMATCH",
            "PARENT_SESSION_UNKNOWN",
            "DAEMON_UNAVAILABLE",
            "PROTOCOL_MISMATCH",
        ] {
            assert_eq!(
                error_code(prepared(
                    intent("omp"),
                    args(&["--resume", &new_id()]),
                    true,
                    vec![Err(ApiError::new(code, "관리 오류"))],
                )),
                code
            );
        }
        assert_eq!(
            error_code(prepared(
                intent("claude"),
                vec![],
                true,
                vec![Err(ApiError::new("DAEMON_UNAVAILABLE", "연결 실패"))],
            )),
            "DAEMON_UNAVAILABLE"
        );
        assert_eq!(
            error_code(prepared(
                intent("claude"),
                vec![],
                true,
                vec![route("unmanaged", "global", None)],
            )),
            "PROTOCOL_MISMATCH"
        );
    }

    #[test]
    fn continue_without_mapping_can_be_unmanaged_but_known_targets_cannot() {
        let launch = intent("claude");
        let LaunchRoute::Unmanaged(_, _) = prepared(
            launch.clone(),
            args(&["--continue"]),
            true,
            vec![snapshot(vec![]), route("unmanaged", "directory", None)],
        )
        .unwrap() else {
            panic!("명시적 비관리 경로이어야 합니다.")
        };
        let original = session(&new_id(), &launch.cwd, "original-account", 1, "EXITED");
        let LaunchRoute::Managed(resumed, _) = prepared(
            launch.clone(),
            args(&["--continue"]),
            true,
            vec![
                snapshot(vec![original.clone()]),
                route("pinned", "resume", Some("sonnet")),
            ],
        )
        .unwrap() else {
            panic!("관리 재개이어야 합니다.")
        };
        assert_eq!(
            resumed.resume_session_id.as_deref(),
            Some(original.id.as_str())
        );
        assert_eq!(resumed.account_id.as_deref(), Some("original-account"));
        let mut active = original.clone();
        active.state = "ACTIVE".into();
        assert_eq!(
            error_code(prepared(
                launch.clone(),
                args(&["--continue"]),
                true,
                vec![
                    snapshot(vec![active]),
                    Err(ApiError::new("SESSION_BUSY", "실행 중"))
                ],
            )),
            "SESSION_BUSY"
        );
        let mut unmapped = original.clone();
        unmapped.native_session_id = None;
        assert_eq!(
            error_code(prepared(
                launch.clone(),
                args(&["--continue"]),
                true,
                vec![snapshot(vec![unmapped])],
            )),
            "RESUME_UNSUPPORTED"
        );
        assert_eq!(
            error_code(prepared(
                launch,
                args(&["--continue"]),
                true,
                vec![
                    snapshot(vec![original]),
                    Err(ApiError::new("RESUME_UNKNOWN", "매핑 변경"))
                ],
            )),
            "RESUME_UNKNOWN"
        );
    }

    #[test]
    fn ambiguous_pickers_and_duplicate_sources_are_never_raw() {
        assert_eq!(
            error_code(prepared(
                intent("claude"),
                args(&["--resume"]),
                true,
                vec![],
            )),
            "SESSION_REQUIRED"
        );
        assert_eq!(
            error_code(prepared(
                intent("claude"),
                args(&["--resume", &new_id(), "--continue"]),
                true,
                vec![],
            )),
            "AUTH_OVERRIDE_CONFLICT"
        );
        assert_eq!(
            error_code(prepared(
                intent("claude"),
                args(&["--resume", "conversation-name"]),
                true,
                vec![Err(ApiError::new("RESUME_UNKNOWN", "알 수 없는 이름"))],
            )),
            "SESSION_REQUIRED"
        );
    }
}

#[cfg(all(test, target_os = "macos"))]
mod writer_watch_tests {
    use super::*;

    /// writer 조회는 spawn 때 확인한 birth identity와 같은 프로세스에만 한다.
    /// 같은 PID라도 birth가 다르면(재사용 PID) 그 프로세스가 연 rollout을 모으지 않는다.
    #[test]
    fn writer_watch_reads_only_the_identified_root() {
        let dir = std::env::temp_dir().join(format!("aam-writer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("rollout-x.jsonl");
        // 실제 supervisor처럼 독립 process group(PGID = 루트 PID)으로 띄운다.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "exec 3>>\"$0\"; exec sleep 5"])
            .arg(&file)
            .process_group(0)
            .spawn()
            .unwrap();
        thread::sleep(Duration::from_millis(300));
        let identity = aam_protocol::process_identity(child.id()).unwrap();
        let mut other = identity.clone();
        other.started_at = "0".into();
        let stale = WriterWatch::start(other);
        let real = WriterWatch::start(identity);
        thread::sleep(Duration::from_millis(500));
        let stale = stale.finish();
        let real = real.finish();
        let _ = child.kill();
        let _ = child.wait();
        let canonical = file.canonicalize().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(stale.is_empty(), "{stale:?}");
        assert!(real.contains(&canonical), "{real:?}");
    }
}
