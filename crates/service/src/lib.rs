mod bridge;
mod diagnostics;
mod managed_sessions;
mod process;
mod quota_summary;
mod routes;
pub mod scheduler;
mod server;
mod store;

use aam_protocol::*;
use process::Liveness;
use rusqlite::TransactionBehavior;
use serde::{Deserialize, Serialize};
use serde_json::Value;
pub use server::run;
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard, OnceLock,
    },
    time::{Duration, Instant},
};
use store::*;

pub struct Service {
    paths: Paths,
    store: Mutex<Store>,
    refreshing: AtomicBool,
    started_at: i64,
    prepared_ms: i64,
    suspect_ms: i64,
    /// 백그라운드 시작 뒤에만 생긴다. 테스트용 서비스는 브릿지를 띄우지 않는다.
    bridge: OnceLock<Arc<bridge::Bridge>>,
    /// Windows: 세션별 Job 핸들. 이름 있는 Job은 마지막 핸들이 닫히면 이름이 사라지므로 실행기가 살아 있는
    /// lease.started 때 열어 둔다. 서비스가 재시작되어 잃으면 종료 근거가 없으므로 슬롯을 유지한다.
    #[cfg(windows)]
    jobs: Mutex<std::collections::HashMap<String, aam_protocol::ProcessJob>>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Acquire {
    request_id: String,
    client_instance_id: String,
    intent: LaunchIntent,
    #[serde(default)]
    parent_capability: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Starting {
    session_id: String,
    capability: String,
    generation: String,
    spawn_attempt_id: String,
    supervisor: ProcessIdentity,
    evidence: IdentityEvidence,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Started {
    session_id: String,
    capability: String,
    spawn_attempt_id: String,
    process: ProcessIdentity,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LeaseAction {
    session_id: String,
    capability: String,
    exit_code: Option<i32>,
    reason: Option<String>,
    session_persisted: Option<bool>,
    background_processes: Option<Vec<ProcessIdentity>>,
    /// Codex는 시작 때 대화 ID를 정할 수 없어 실행기가 종료 뒤 찾아 보고한다(`adapters::codex_session_for`).
    #[serde(default)]
    native_session_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PolicyUpdate {
    expected_revision: u64,
    automatic: Option<bool>,
    allocation_mode: Option<AllocationMode>,
    account_priority: Option<Vec<String>>,
    provider_pins: Option<BTreeMap<String, String>>,
    safety_reserve_percent: Option<f64>,
    auto_takeover: Option<bool>,
    project_allowlist: Option<BTreeMap<String, Vec<String>>>,
    project_routes: Option<Vec<ProjectRoute>>,
    expiring_boost: Option<bool>,
    expiring_window_hours: Option<u32>,
    expiring_min_percent: Option<f64>,
    use_credits_after_limit: Option<bool>,
    use_extra_usage_after_limit: Option<bool>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TakeoverRequest {
    tool: String,
    native_session_id: String,
}
impl TakeoverRequest {
    /// 도구와 대화 식별자의 형태만 확인합니다. 소유 계정은 실제 파일 근거로 따로 확인합니다.
    fn checked(&self) -> Result<String, ApiError> {
        if self.tool != "claude" {
            return Err(ApiError::new(
                "TAKEOVER_UNSUPPORTED",
                "확인된 대화 넘기기는 claude만 지원해요.",
            ));
        }
        let native = self.native_session_id.trim();
        if native.is_empty() || native.len() > 4096 || native.chars().any(char::is_control) {
            return Err(ApiError::new(
                "INVALID_PARAMS",
                "넘길 대화 ID가 올바르지 않아요.",
            ));
        }
        Ok(uuid::Uuid::parse_str(native)
            .map_err(|_| {
                ApiError::new(
                    "TAKEOVER_UNSUPPORTED",
                    "Claude 대화는 완전한 UUID로만 넘길 수 있어요.",
                )
            })?
            .to_string())
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Register {
    tool: String,
    label: String,
    profile_path: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountUpdate {
    id: String,
    enabled: Option<bool>,
    max_concurrency: Option<u32>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Explain {
    intent: LaunchIntent,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ValidateChild {
    session_id: String,
    capability: String,
    account_id: String,
    process: ProcessIdentity,
    #[serde(default)]
    root_spawn: bool,
}

fn parse<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ApiError> {
    serde_json::from_value(value).map_err(|_| {
        ApiError::new(
            "INVALID_PARAMS",
            "요청 항목이나 형식이 맞지 않아요.",
        )
    })
}
fn value<T: Serialize>(result: T) -> Result<Value, ApiError> {
    serde_json::to_value(result)
        .map_err(|_| ApiError::new("STATE_INVALID", "응답을 만들지 못했어요."))
}
fn config_ms(name: &str, default: i64, min: i64, max: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(default)
        .clamp(min, max)
}
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_:./".contains(&byte))
}
fn validate_intent(intent: &LaunchIntent) -> Result<(), ApiError> {
    if !["claude", "codex"].contains(&intent.tool.as_str())
        || !safe_id(&intent.model)
        || intent.cwd.len() > 4096
        || !std::path::Path::new(&intent.cwd).is_absolute()
        || intent.cwd.contains('\0')
    {
        return Err(ApiError::new(
            "INVALID_INTENT",
            "지원 도구, 정확한 모델 ID, 절대 프로젝트 경로가 필요해요.",
        ));
    }
    Ok(())
}
/// 삭제한 계정을 가리키는 정책 항목(기본 계정·우선순위·프로젝트 허용)을 정리합니다.
/// 고정 규칙은 fail-closed로 남기고, 하나라도 바뀌면 true를 돌려 revision 증가를 맡깁니다.
fn forget_account(policy: &mut Policy, id: &str) -> bool {
    let preferred = policy.provider_pins.len();
    policy.provider_pins.retain(|_, value| value != id);
    let priority = policy.account_priority.len();
    policy.account_priority.retain(|value| value != id);
    let allowlist = policy.project_allowlist.remove(id).is_some();
    allowlist
        || preferred != policy.provider_pins.len()
        || priority != policy.account_priority.len()
}
fn authorize(record: &LeaseRecord, capability: &str) -> Result<(), ApiError> {
    let matches = record
        .capability
        .as_bytes()
        .iter()
        .zip(capability.as_bytes())
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
        && record.capability.len() == capability.len();
    if !matches {
        return Err(ApiError::new(
            "LEASE_FORBIDDEN",
            "이 대화를 제어할 권한이 없어요.",
        ));
    }
    Ok(())
}
fn terminal(session: &Session) -> bool {
    !scheduler::holds_capacity(&session.state)
}
fn expire(connection: &rusqlite::Connection, now: i64) -> Result<(), ApiError> {
    for mut record in leases(connection)? {
        if record.session.state == "PREPARED" && now >= record.expires_at {
            record.session.state = "ABORTED".into();
            record.session.reason =
                Some("시작 전 예약이 끝났어요. 다시 요청해 주세요.".into());
            record.session.updated_at = now;
            save_lease(connection, &record)?;
        }
    }
    Ok(())
}

impl Service {
    pub fn open(paths: Paths) -> Result<Arc<Self>, ApiError> {
        let store = Store::open(&paths.database)?;
        Ok(Arc::new(Self {
            paths,
            store: Mutex::new(store),
            refreshing: AtomicBool::new(false),
            started_at: now_ms(),
            prepared_ms: config_ms("AAM_PREPARED_TIMEOUT_MS", 30_000, 1_000, 300_000),
            suspect_ms: config_ms("AAM_SUSPECT_THRESHOLD_MS", 45_000, 10_000, 600_000),
            bridge: OnceLock::new(),
            #[cfg(windows)]
            jobs: Mutex::new(std::collections::HashMap::new()),
        }))
    }
    fn lock(&self) -> Result<MutexGuard<'_, Store>, ApiError> {
        self.store.lock().map_err(|_| {
            ApiError::new(
                "SERVICE_UNHEALTHY",
                "서비스 상태 잠금에 문제가 생겼어요. 서비스를 다시 시작해 주세요.",
            )
        })
    }
    pub fn snapshot(&self) -> Result<Snapshot, ApiError> {
        // 계정 카탈로그만 짧게 읽고 파일·OS 관측은 DB 잠금 밖에서 수행합니다.
        let catalog = {
            let store = self.lock()?;
            accounts(&store.connection)?
        };
        self.snapshot_with_observations(aam_adapters::discover_sessions(&self.paths, &catalog))
    }
    fn snapshot_with_observations(
        &self,
        mut observed: aam_adapters::ObservedScan,
    ) -> Result<Snapshot, ApiError> {
        // Bridge는 서비스 DB도 사용한다. 두 잠금을 동시에 잡지 않는다.
        let bridge = self.bridge.get().map(|bridge| bridge.status());
        let store = self.lock()?;
        let policy = policy(&store.connection)?;
        let now = now_ms();
        let mut accounts = accounts(&store.connection)?;
        for account in &mut accounts {
            for bucket in &mut account.buckets {
                if bucket.status == "known"
                    && (bucket.observed_at <= 0
                        || now.saturating_sub(bucket.observed_at)
                            > policy
                                .stale_after_seconds
                                .saturating_mul(1000)
                                .min(i64::MAX as u64) as i64
                        || bucket.resets_at.is_some_and(|reset| reset <= now))
                {
                    bucket.status = "stale".into();
                }
            }
        }
        let mut sessions: Vec<Session> = leases(&store.connection)?
            .into_iter()
            .map(|record| record.session)
            .collect();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.updated_at));
        // 활성 세션은 모두 보여주고 종료 이력만 최근 200개로 제한합니다.
        let mut history = 0;
        sessions.retain(|session| {
            if scheduler::holds_capacity(&session.state) {
                true
            } else {
                history += 1;
                history <= 200
            }
        });
        let mut notices = notices(&store.connection)?;
        // 동일 PID만으로 합치지 않으며 관측값은 저장하거나 예약 용량에 넣지 않습니다.
        observed.sessions.retain(|observed| {
            !sessions.iter().any(|managed| {
                managed.process.as_ref() == Some(&observed.process)
                    || managed.supervisor.as_ref() == Some(&observed.process)
                    || managed
                        .background_processes
                        .as_ref()
                        .is_some_and(|background| background.contains(&observed.process))
            })
        });
        notices.extend(observed.notices);
        if metadata::<Option<String>>(&store.connection, "uninstallPermit")?
            .flatten()
            .is_some()
        {
            notices.push(Notice { id: "admission-disabled".into(), level: "warning".into(), title: "서비스 해제 준비 중".into(), message: "새 배정을 잠시 멈췄어요. 해제를 취소하거나 서비스를 다시 설치하면 다시 열려요.".into() });
        }
        // 이미 관리 세션이 생긴 인계 기록은 화면에서 제외합니다.
        let adopted: Vec<Takeover> = takeovers(&store.connection)?
            .into_iter()
            .filter(|record| {
                !sessions.iter().any(|session| {
                    session.tool == record.tool
                        && session.native_session_id.as_deref()
                            == Some(record.native_session_id.as_str())
                })
            })
            .collect();
        let quota_summaries = quota_summary::summaries(&accounts, &policy, bridge.as_ref().map_or(&[], |status| status.blocks.as_slice()), now);
        Ok(Snapshot {
            version: PROTOCOL_VERSION,
            generated_at: now,
            service_started_at: self.started_at,
            accounts,
            tools: tools(&store.connection)?,
            sessions,
            observed_sessions: observed.sessions,
            policy,
            notices,
            refreshing: self.refreshing.load(Ordering::Acquire),
            last_refresh_at: metadata(&store.connection, "lastRefreshAt")?,
            takeovers: adopted,
            quota_summaries,
            service_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        })
    }
    pub fn dispatch(self: &Arc<Self>, method: &str, params: Value) -> Result<Value, ApiError> {
        match method {
            "status.read" => value(self.snapshot()?),
            "quota.refresh" => {
                self.start_refresh();
                value(self.snapshot()?)
            }
            "service.prepareUninstall" => self.prepare_uninstall(),
            "service.cancelUninstall" => self.cancel_uninstall(params),
            "service.resumeAdmission" => {
                let store = self.lock()?;
                set_metadata(
                    &store.connection,
                    "uninstallPermit",
                    &Option::<String>::None,
                )?;
                Ok(serde_json::json!({"resumed": true}))
            }
            "route.resolve" | "route.explain" => {
                let mut request: Explain = parse(params)?;
                validate_intent(&request.intent)?;
                let configured = self.configured_for(&request.intent)?;
                let store = self.lock()?;
                let sessions: Vec<_> = leases(&store.connection)?
                    .into_iter()
                    .map(|record| record.session)
                    .collect();
                let accounts = accounts(&store.connection)?;
                let stored = policy(&store.connection)?;
                self.normalize_resume(
                    &mut request.intent,
                    &sessions,
                    &accounts,
                    &takeovers(&store.connection)?,
                    stored.auto_takeover,
                )?;
                if method == "route.resolve" {
                    return value(routes::resolve(
                        &accounts,
                        &sessions,
                        &policy(&store.connection)?,
                        &request.intent,
                    )?);
                }
                value(scheduler::decide_with(
                    &accounts,
                    &sessions,
                    &policy(&store.connection)?,
                    &request.intent,
                    now_ms(),
                    &configured,
                )?)
            }
            "takeover.adopt" => value(self.adopt_takeover(parse(params)?)?),
            "takeover.release" => self.release_takeover(parse(params)?),
            "lease.validate-child" => value(self.validate_child(parse(params)?)?),
            "policy.update" => value(self.update_policy(parse(params)?)?),
            "bridge.status" => value(bridge::status_or_inactive(self.bridge.get())?),
            "diagnostics.export" => diagnostics::export(self, parse(params)?),
            "account.register" => value(self.register(parse(params)?)?),
            "account.update" => value(self.update_account(parse(params)?)?),
            "lease.acquire" => {
                let request: Acquire = parse(params)?;
                validate_intent(&request.intent)?;
                let needs_refresh = {
                    let store = self.lock()?;
                    let policy = policy(&store.connection)?;
                    let now = now_ms();
                    by_request(&store.connection, &request.request_id)?.is_none()
                        && (metadata::<i64>(&store.connection, "lastRefreshAt")?.is_none()
                            || accounts(&store.connection)?
                                .iter()
                                .filter(|account| {
                                    account.tool == request.intent.tool && account.can_launch
                                })
                                .any(|account| {
                                    account.buckets.iter().any(|bucket| {
                                        bucket.status == "stale"
                                            || now.saturating_sub(bucket.observed_at)
                                                > policy
                                                    .stale_after_seconds
                                                    .saturating_mul(1000)
                                                    .min(i64::MAX as u64)
                                                    as i64
                                            || bucket.resets_at.is_some_and(|reset| reset <= now)
                                    })
                                }))
                };
                if needs_refresh {
                    self.start_refresh();
                    let deadline = Instant::now() + Duration::from_secs(20);
                    while self.refreshing.load(Ordering::Acquire) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                value(self.acquire(request, now_ms())?)
            }
            "lease.starting" => value(self.starting(parse(params)?, now_ms())?),
            "lease.started" => value(self.started(parse(params)?, now_ms())?),
            "lease.heartbeat" => value(self.heartbeat(parse(params)?, now_ms())?),
            "lease.release" => value(self.finish(parse(params)?, false, now_ms())?),
            "lease.abort" => value(self.finish(parse(params)?, true, now_ms())?),
            _ => Err(ApiError::new(
                "METHOD_NOT_FOUND",
                "지원하지 않는 요청이에요.",
            )),
        }
    }
    fn prepare_uninstall(&self) -> Result<Value, ApiError> {
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        expire(&tx, now_ms())?;
        if leases(&tx)?
            .iter()
            .any(|record| scheduler::holds_capacity(&record.session.state))
        {
            return Err(ApiError::new(
                "SESSION_BUSY",
                "실행 중이거나 시작 결과가 불확실한 대화가 남아 있어 서비스를 해제할 수 없어요.",
            ));
        }
        let permit = metadata::<Option<String>>(&tx, "uninstallPermit")?
            .flatten()
            .unwrap_or_else(new_id);
        set_metadata(&tx, "uninstallPermit", &Some(&permit))?;
        tx.commit().map_err(db_error)?;
        Ok(serde_json::json!({"safe": true, "permit": permit}))
    }
    fn cancel_uninstall(&self, params: Value) -> Result<Value, ApiError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Cancel {
            permit: String,
        }
        let request: Cancel = parse(params)?;
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        if metadata::<Option<String>>(&tx, "uninstallPermit")?
            .flatten()
            .as_deref()
            != Some(request.permit.as_str())
        {
            return Err(ApiError::new(
                "UNINSTALL_PERMIT_MISMATCH",
                "서비스 해제 확인이 맞지 않아요.",
            ));
        }
        set_metadata(&tx, "uninstallPermit", &Option::<String>::None)?;
        tx.commit().map_err(db_error)?;
        Ok(serde_json::json!({"cancelled": true}))
    }
    fn update_policy(&self, request: PolicyUpdate) -> Result<Policy, ApiError> {
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let mut current = policy(&tx)?;
        if current.revision != request.expected_revision {
            return Err(ApiError::new(
                "POLICY_CONFLICT",
                "다른 창에서 설정을 바꿨어요. 최신 상태를 읽고 다시 시도해 주세요.",
            ));
        }
        if let Some(priority) = request.account_priority {
            let mut seen = std::collections::HashSet::with_capacity(priority.len());
            for id in &priority {
                if !seen.insert(id) {
                    return Err(ApiError::new(
                        "INVALID_PARAMS",
                        "계정 순서에 같은 계정을 두 번 넣을 수 없어요.",
                    ));
                }
                account(&tx, id)?;
            }
            current.account_priority = priority;
        }
        if let Some(mode) = request.allocation_mode {
            current.allocation_mode = mode;
        }
        if let Some(reserve) = request.safety_reserve_percent {
            if !reserve.is_finite() || !(0.0..100.0).contains(&reserve) {
                return Err(ApiError::new(
                    "INVALID_PARAMS",
                    "안전 잔여량은 0 이상 100 미만이어야 해요.",
                ));
            }
            current.safety_reserve_percent = reserve;
        }
        if let Some(use_credits) = request.use_credits_after_limit {
            current.use_credits_after_limit = use_credits;
        }
        if let Some(use_extra) = request.use_extra_usage_after_limit {
            current.use_extra_usage_after_limit = use_extra;
        }
        if let Some(boost) = request.expiring_boost {
            current.expiring_boost = boost;
        }
        if let Some(hours) = request.expiring_window_hours {
            if !(1..=168).contains(&hours) {
                return Err(ApiError::new("INVALID_PARAMS", "곧 리셋 기준은 1~168시간이어야 해요."));
            }
            current.expiring_window_hours = hours;
        }
        if let Some(percent) = request.expiring_min_percent {
            if !percent.is_finite() || !(1.0..=100.0).contains(&percent) {
                return Err(ApiError::new("INVALID_PARAMS", "알림 기준 잔여량은 1~100%여야 해요."));
            }
            current.expiring_min_percent = percent;
        }
        if let Some(pins) = request.provider_pins {
            for (provider, id) in &pins {
                let account = account(&tx, id)?;
                if aam_protocol::pin_provider(&account.provider) != provider
                    || aam_protocol::pin_provider(provider) != provider
                {
                    return Err(ApiError::new(
                        "INVALID_PARAMS",
                        "직접 고른 계정과 공급자가 맞지 않아요.",
                    ));
                }
            }
            current.provider_pins = pins;
        }
        if let Some(allowlist) = request.project_allowlist {
            if allowlist.len() > 1024 {
                return Err(ApiError::new(
                    "INVALID_PARAMS",
                    "프로젝트 제한은 계정 1,024개까지 설정할 수 있어요.",
                ));
            }
            let mut canonical = BTreeMap::new();
            for (id, roots) in allowlist {
                account(&tx, &id)?;
                if roots.len() > 64 {
                    return Err(ApiError::new(
                        "INVALID_PARAMS",
                        "계정별 프로젝트 허용 폴더는 최대 64개입니다.",
                    ));
                }
                let mut directories = Vec::with_capacity(roots.len());
                for root in roots {
                    let resolved = scheduler::canonical_directory(&root)?;
                    let directory = resolved.to_str().ok_or_else(|| {
                        ApiError::new(
                            "INVALID_PROJECT",
                            "프로젝트 경로를 문자로 표현할 수 없습니다.",
                        )
                    })?;
                    if directory.len() > 4096 {
                        return Err(ApiError::new(
                            "INVALID_PROJECT",
                            "프로젝트 경로가 너무 깁니다.",
                        ));
                    }
                    directories.push(directory.to_owned());
                }
                directories.sort();
                directories.dedup();
                canonical.insert(id, directories);
            }
            current.project_allowlist = canonical;
        }
        if let Some(project_routes) = request.project_routes {
            current.project_routes = routes::validate(project_routes, &accounts(&tx)?)?;
        }
        if let Some(automatic) = request.automatic {
            current.automatic = automatic;
        }
        if let Some(auto) = request.auto_takeover {
            current.auto_takeover = auto;
        }
        current.revision = current.revision.checked_add(1).ok_or_else(|| {
            ApiError::new("POLICY_CONFLICT", "설정 버전 범위를 넘었어요.")
        })?;
        set_metadata(&tx, "policy", &current)?;
        tx.commit().map_err(db_error)?;
        Ok(current)
    }
    /// 외부 대화를 확인된 소유 계정으로 인계 등록합니다. 계정을 추정하거나 대화를 옮기지 않습니다.
    fn adopt_takeover(&self, request: TakeoverRequest) -> Result<Takeover, ApiError> {
        let native = request.checked()?;
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let sessions: Vec<_> = leases(&tx)?
            .into_iter()
            .map(|record| record.session)
            .collect();
        let managed = |tool: &str, id: &str| {
            sessions.iter().any(|session| {
                session.tool == tool && session.native_session_id.as_deref() == Some(id)
            })
        };
        if managed(&request.tool, &native) {
            return Err(ApiError::new(
                "TAKEOVER_UNSUPPORTED",
                "이미 관리 중인 대화입니다. 인계가 필요하지 않습니다.",
            ));
        }
        let accounts = accounts(&tx)?;
        let owner = aam_adapters::session_owner(&accounts, &request.tool, &native)?;
        if managed(&request.tool, &owner.native_session_id) {
            return Err(ApiError::new(
                "TAKEOVER_UNSUPPORTED",
                "이미 관리 중인 대화입니다. 인계가 필요하지 않습니다.",
            ));
        }
        let record = Takeover {
            tool: request.tool,
            native_session_id: owner.native_session_id,
            account_id: owner.account_id,
            cwd: owner.cwd,
            adopted_at: now_ms(),
            source: "manual".into(),
            evidence: owner.evidence,
        };
        // 이미 관리 세션이 생긴 인계 기록은 함께 정리합니다.
        let mut stored: Vec<Takeover> = takeovers(&tx)?
            .into_iter()
            .filter(|other| {
                !(other.tool == record.tool && other.native_session_id == record.native_session_id)
                    && !managed(&other.tool, &other.native_session_id)
            })
            .collect();
        if stored.len() >= 512 {
            stored.sort_by_key(|other| other.adopted_at);
            stored.remove(0);
        }
        stored.push(record.clone());
        set_takeovers(&tx, &stored)?;
        tx.commit().map_err(db_error)?;
        Ok(record)
    }
    fn release_takeover(&self, request: TakeoverRequest) -> Result<Value, ApiError> {
        let native = request.checked()?;
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let stored = takeovers(&tx)?;
        let remaining: Vec<Takeover> = stored
            .iter()
            .filter(|other| {
                !(other.tool == request.tool && other.native_session_id == native)
            })
            .cloned()
            .collect();
        if remaining.len() == stored.len() {
            return Err(ApiError::new(
                "TAKEOVER_NOT_FOUND",
                "해제할 인계 등록을 찾을 수 없습니다.",
            ));
        }
        set_takeovers(&tx, &remaining)?;
        tx.commit().map_err(db_error)?;
        Ok(serde_json::json!({"released": true}))
    }
    fn update_account(&self, request: AccountUpdate) -> Result<Account, ApiError> {
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let mut current = account(&tx, &request.id)?;
        if let Some(enabled) = request.enabled {
            current.enabled = enabled;
        }
        if let Some(limit) = request.max_concurrency {
            if limit == 0 {
                return Err(ApiError::new(
                    "INVALID_PARAMS",
                    "동시 실행 상한은 1 이상이어야 합니다.",
                ));
            }
            if limit > 32 {
                return Err(ApiError::new(
                    "INVALID_PARAMS",
                    "관리 서비스의 계정별 동시 실행 상한은 32입니다.",
                ));
            }
            if limit > 1 && !aam_adapters::supports_shared_profile_concurrency(&current) {
                return Err(ApiError::new("ADAPTER_UNVERIFIED", "이 도구는 같은 로그인으로 여러 대화를 동시에 여는 방식이 확인되지 않았어요."));
            }
            current.max_concurrency = limit;
        }
        save_account(&tx, &current)?;
        tx.commit().map_err(db_error)?;
        Ok(current)
    }
    fn register(&self, request: Register) -> Result<Account, ApiError> {
        if request.label.is_empty()
            || request.label.chars().count() > 120
            || request.label.chars().any(char::is_control)
        {
            return Err(ApiError::new(
                "INVALID_PARAMS",
                "계정 이름은 제어 문자 없이 1~120자로 입력하세요.",
            ));
        }
        // 공식 CLI 검사에는 DB 잠금을 유지하지 않습니다.
        let mut registered = aam_adapters::register(
            &self.paths,
            &request.tool,
            &request.label,
            &request.profile_path,
        )?;
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        // 같은 프로필의 기존 binding과, 다른 프로필에 있는 같은 native 계정(재로그인·갱신)을
        // 함께 모읍니다. 후자는 새 binding으로 흡수해 계정이 하나만 남게 합니다.
        let related: Vec<_> = accounts(&tx)?
            .into_iter()
            .filter(|item| {
                item.tool == registered.tool
                    && (item.profile_path == registered.profile_path
                        || supersedes(&registered, item))
            })
            .collect();
        if leases(&tx)?.iter().any(|record| {
            related
                .iter()
                .any(|previous| previous.id == record.session.account_id)
                && scheduler::holds_capacity(&record.session.state)
        }) {
            return Err(ApiError::new(
                "SESSION_BUSY",
                "이 프로필을 사용하는 세션이 남아 있어 계정 등록을 변경할 수 없습니다.",
            ));
        }
        for mut previous in related {
            let replaced = supersedes(&registered, &previous);
            if previous.id == registered.id
                || (replaced && previous.profile_path == registered.profile_path)
            {
                registered.enabled = previous.enabled;
                registered.max_concurrency = previous.max_concurrency;
            }
            if replaced {
                tx.execute("DELETE FROM accounts WHERE id=?1", [&previous.id])
                    .map_err(db_error)?;
                let mut current_policy = policy(&tx)?;
                let mut changed = false;
                if current_policy.account_priority.contains(&previous.id) {
                    for id in &mut current_policy.account_priority {
                        if id == &previous.id {
                            *id = registered.id.clone();
                        }
                    }
                    // 기존·새 binding이 모두 있으면 먼저 지정된 순서를 보존합니다.
                    let mut registered_seen = false;
                    current_policy.account_priority.retain(|id| {
                        if id != &registered.id {
                            return true;
                        }
                        let first = !registered_seen;
                        registered_seen = true;
                        first
                    });
                    changed = true;
                }
                for id in current_policy.provider_pins.values_mut() {
                    if id == &previous.id {
                        *id = registered.id.clone();
                        changed = true;
                    }
                }
                if let Some(roots) = current_policy.project_allowlist.remove(&previous.id) {
                    current_policy
                        .project_allowlist
                        .entry(registered.id.clone())
                        .or_insert(roots);
                    changed = true;
                }
                for route in &mut current_policy.project_routes {
                    if route.account_id.as_ref() == Some(&previous.id) {
                        route.account_id = Some(registered.id.clone());
                        changed = true;
                    }
                }
                if changed {
                    current_policy.revision =
                        current_policy.revision.checked_add(1).ok_or_else(|| {
                            ApiError::new("POLICY_CONFLICT", "설정 버전 범위를 넘었어요.")
                        })?;
                    set_metadata(&tx, "policy", &current_policy)?;
                }
            } else if previous.id != registered.id {
                previous.can_launch = false;
                previous.auth_status = "unverified".into();
                previous.verification = "observed".into();
                previous.reason = Some("이 프로필에 다른 identity가 등록되었습니다. 과거 binding은 이력으로 보존합니다.".into());
                for bucket in &mut previous.buckets {
                    bucket.status = "stale".into();
                }
                save_account(&tx, &previous)?;
            }
        }
        save_account(&tx, &registered)?;
        tx.commit().map_err(db_error)?;
        Ok(registered)
    }
    fn acquire(&self, mut request: Acquire, now: i64) -> Result<LeaseGrant, ApiError> {
        validate_intent(&request.intent)?;
        if !safe_id(&request.request_id) || !safe_id(&request.client_instance_id) {
            return Err(ApiError::new(
                "INVALID_PARAMS",
                "유효한 요청 ID와 실행기 ID가 필요합니다.",
            ));
        }
        let payload_hash = fingerprint(&request)?;
        let configured = self.configured_for(&request.intent)?;
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        expire(&tx, now)?;
        if let Some(record) = by_request(&tx, &request.request_id)? {
            if record.payload_hash != payload_hash {
                return Err(ApiError::new(
                    "IDEMPOTENCY_CONFLICT",
                    "같은 요청 ID에 다른 실행 내용을 사용할 수 없습니다.",
                ));
            }
            tx.commit().map_err(db_error)?;
            return Ok(LeaseGrant {
                session: record.session,
                capability: record.capability,
                account: record.account,
                pin_unavailable: false,
                credits_fallback: false,
                extra_usage_fallback: false,
            });
        }
        admission_allowed(&tx)?;
        let accounts = accounts(&tx)?;
        let sessions: Vec<_> = leases(&tx)?
            .into_iter()
            .map(|record| record.session)
            .collect();
        let takeover_records = takeovers(&tx)?;
        let auto_takeover = policy(&tx)?.auto_takeover;
        self.normalize_resume(
            &mut request.intent,
            &sessions,
            &accounts,
            &takeover_records,
            auto_takeover,
        )?;
        if let Some(id) = &request.intent.parent_session_id {
            let parent = lease(&tx, id)?;
            if !accounts.iter().any(|account| {
                account.id == parent.account.id
                    && account.identity_key == parent.account.identity_key
                    && account.omp_credential_pins == parent.account.omp_credential_pins
            }) {
                return Err(ApiError::new(
                    "PARENT_IDENTITY_UNVERIFIED",
                    "부모 실행 이후 계정 identity 연결이 바뀌었습니다.",
                ));
            }
            authorize(&parent, request.parent_capability.as_deref().unwrap_or(""))?;
            if parent.session.state != "ACTIVE" || parent.session.process.is_none() {
                return Err(ApiError::new(
                    "PARENT_SESSION_UNKNOWN",
                    "실행 중인 부모 관리 프로세스를 확인할 수 없습니다.",
                ));
            }
        }
        if let Some(original) = routes::original(&sessions, &request.intent)? {
            let original = lease(&tx, &original.id)?;
            if !accounts.iter().any(|account| {
                account.id == original.account.id
                    && account.identity_key == original.account.identity_key
                    && account.omp_credential_pins == original.account.omp_credential_pins
            }) {
                return Err(ApiError::new(
                    "RESUME_UNVERIFIED",
                    "원래 실행 이후 계정 identity 연결이 바뀌었습니다.",
                ));
            }
        }
        let policy = policy(&tx)?;
        let resolution = routes::resolve(&accounts, &sessions, &policy, &request.intent)?;
        let effective = routes::effective(&request.intent, &resolution);
        let decision = scheduler::decide_with(&accounts, &sessions, &policy, &request.intent, now, &configured)?;
        let pin_unavailable = decision.pin_unavailable.is_some();
        let credits_fallback = scheduler::chose_credits(&decision);
        let extra_usage_fallback = scheduler::chose_extra_usage(&decision);
        let selected = decision
            .selected_account_id
            .as_ref()
            .and_then(|id| accounts.iter().find(|account| &account.id == id));
        let Some(account) = selected else {
            let explicit = resolution.account_id.as_ref();
            let code = explicit
                .and_then(|id| {
                    decision
                        .candidates
                        .iter()
                        .find(|candidate| &candidate.account_id == id)
                })
                .and_then(|candidate| candidate.reasons.first())
                .and_then(|reason| reason.split_once(':'))
                .map(|(code, _)| code)
                .unwrap_or("NO_ELIGIBLE_ACCOUNT");
            return Err(ApiError::new(code, format!("배정할 수 있는 계정이 없어요. aam explain --tool {}로 제외 이유를 확인해 주세요.", request.intent.tool)));
        };
        let manual = resolution.account_id.is_some();
        let id = new_id();
        let resumed = routes::original(&sessions, &request.intent)?;
        let continued_from = resumed
            .filter(|_| request.intent.continue_elsewhere)
            .map(|original| original.id.clone());
        let native_session_id =
            if let Some(original) = resumed {
                original.native_session_id.clone()
            } else if request.intent.adopted {
                // 인계 대화는 새 식별자를 만들지 않고 확인된 외부 대화를 그대로 이어받습니다.
                request.intent.resume_session_id.clone()
            } else if request.intent.tool == "claude" {
                let native = uuid::Uuid::parse_str(
                    request.intent.native_session_id.as_deref().unwrap_or(&id),
                )
                .map_err(|_| {
                    ApiError::new(
                        "NATIVE_SESSION_CONFLICT",
                        "새 native 세션 ID는 유효한 UUID여야 합니다.",
                    )
                })?;
                if sessions
                    .iter()
                    .flat_map(|session| {
                        session.native_session_id.as_deref().into_iter().chain(
                            (session.tool == request.intent.tool).then_some(session.id.as_str()),
                        )
                    })
                    .filter_map(|id| uuid::Uuid::parse_str(id).ok())
                    .any(|id| id == native)
                {
                    return Err(ApiError::new(
                        "NATIVE_SESSION_CONFLICT",
                        "새 native 세션 ID는 사용되지 않은 UUID여야 합니다.",
                    ));
                }
                Some(native.to_string())
            } else {
                None
            };
        let session = Session {
            id,
            request_id: request.request_id,
            account_id: account.id.clone(),
            tool: request.intent.tool,
            model: effective.model,
            cwd: scheduler::canonical_directory(&request.intent.cwd)?
                .to_string_lossy()
                .into_owned(),
            state: "PREPARED".into(),
            verification: "configured".into(),
            started_at: now,
            updated_at: now,
            process: None,
            supervisor: None,
            spawn_attempt_id: None,
            generation: new_id(),
            exit_code: None,
            reason: Some("시작 전 identity 확인과 내구성 시작 승인을 기다립니다.".into()),
            native_session_id,
            background_processes: None,
            parent_session_id: request.intent.parent_session_id,
            continued_from,
        };
        let record = LeaseRecord {
            session,
            capability: format!("{}{}", new_id(), new_id()),
            payload_hash,
            policy_revision: policy.revision,
            binding_hash: binding_fingerprint(account)?,
            account: account.clone(),
            expires_at: now.saturating_add(self.prepared_ms),
            heartbeat_at: now,
            manual,
        };
        insert_lease(&tx, &record, &request.client_instance_id)?;
        tx.commit().map_err(db_error)?;
        Ok(LeaseGrant {
            session: record.session,
            capability: record.capability,
            account: record.account,
            pin_unavailable,
            credits_fallback,
            extra_usage_fallback,
        })
    }
    /// 계정별 기본 모델 설정을 서비스 잠금 밖에서 읽는다. 계정 목록만 짧게 잠가 가져온다.
    fn configured_for(&self, intent: &LaunchIntent) -> Result<scheduler::Configured, ApiError> {
        let catalog = {
            let store = self.lock()?;
            accounts(&store.connection)?
        };
        Ok(scheduler::configured_models(&catalog, intent))
    }
    fn starting(&self, request: Starting, now: i64) -> Result<Session, ApiError> {
        // 입장 재검사에 쓸 기본 모델 설정을 잠금 밖에서 먼저 읽는다.
        let configured = {
            let pending = {
                let store = self.lock()?;
                lease(&store.connection, &request.session_id)?.session
            };
            self.configured_for(&LaunchIntent {
                tool: pending.tool,
                model: pending.model,
                cwd: pending.cwd,
                ..Default::default()
            })?
        };
        if !safe_id(&request.spawn_attempt_id) {
            return Err(ApiError::new(
                "INVALID_PARAMS",
                "유효한 실행 시도 ID가 필요합니다.",
            ));
        }
        if process::inspect(&request.supervisor) != Liveness::Alive {
            return Err(ApiError::new(
                "PROCESS_IDENTITY_UNVERIFIED",
                "실행기 프로세스의 시작 identity를 확인할 수 없습니다.",
            ));
        }
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let now = now.max(now_ms());
        admission_allowed(&tx)?;
        let mut record = lease(&tx, &request.session_id)?;
        authorize(&record, &request.capability)?;
        if record.session.generation != request.generation {
            return Err(ApiError::new(
                "LEASE_FENCED",
                "이전 예약 세대로 실행할 수 없습니다.",
            ));
        }
        if record.session.state != "PREPARED" {
            if matches!(
                record.session.state.as_str(),
                "STARTING" | "ACTIVE" | "SUSPECT" | "ORPHANED"
            ) && record.session.spawn_attempt_id.as_deref()
                == Some(request.spawn_attempt_id.as_str())
                && record.session.supervisor.as_ref() == Some(&request.supervisor)
            {
                return Ok(record.session);
            }
            return Err(ApiError::new(
                "LEASE_NOT_PREPARED",
                "이 예약은 실행할 수 없습니다. 새 요청으로 다시 배정하세요.",
            ));
        }
        if now >= record.expires_at {
            record.session.state = "ABORTED".into();
            record.session.updated_at = now;
            record.session.reason = Some("시작 전 예약이 만료되었습니다.".into());
            save_lease(&tx, &record)?;
            tx.commit().map_err(db_error)?;
            return Err(ApiError::new(
                "LEASE_EXPIRED",
                "예약 유효 시간이 지났습니다. 새 요청으로 다시 배정하세요.",
            ));
        }
        let current = account(&tx, &record.session.account_id)?;
        if policy(&tx)?.revision != record.policy_revision
            || binding_fingerprint(&current)? != record.binding_hash
        {
            return Err(ApiError::new(
                "POLICY_CONFLICT",
                "예약 이후 정책 또는 계정 설정이 바뀌었습니다. 새 요청으로 다시 배정하세요.",
            ));
        }
        if let Some(id) = &record.session.parent_session_id {
            let parent = lease(&tx, id)?;
            self.check_child(&parent, &request.supervisor)?;
        }
        let intent = LaunchIntent {
            tool: record.session.tool.clone(),
            model: record.session.model.clone(),
            cwd: record.session.cwd.clone(),
            account_id: record.manual.then(|| current.id.clone()),
            parent_session_id: record.session.parent_session_id.clone(),
            resume_session_id: None,
            native_session_id: None,
            adopted: false,
            continue_elsewhere: false,
        };
        let other_sessions: Vec<_> = leases(&tx)?
            .into_iter()
            .filter(|other| other.session.id != record.session.id)
            .map(|other| other.session)
            .collect();
        // 취득 시 확정한 경로·재개 identity는 유지하고 최신 안전 조건만 다시 검사합니다.
        let mut admission_policy = policy(&tx)?;
        admission_policy.project_routes.clear();
        admission_policy.provider_pins.clear();
        admission_policy.automatic = true;
        let checked = scheduler::decide_with(
            &accounts(&tx)?,
            &other_sessions,
            &admission_policy,
            &intent,
            now,
            &configured,
        )?;
        if !checked
            .candidates
            .iter()
            .any(|candidate| candidate.account_id == current.id && candidate.eligible)
        {
            return Err(ApiError::new("ADMISSION_CHANGED", "예약한 뒤 로그인·한도·동시 사용 조건이 바뀌어 시작을 멈췄어요. 새 배정 설명을 확인해 주세요."));
        }
        if request.evidence.tier != "preflight-verified"
            || current.identity_key.as_deref() != Some(request.evidence.identity_key.as_str())
            || request.evidence.observed_at < record.session.started_at
            || request.evidence.observed_at > now.saturating_add(5_000)
            || now.saturating_sub(request.evidence.observed_at) > self.prepared_ms
        {
            return Err(ApiError::new(
                "IDENTITY_MISMATCH",
                "최종 실행 환경에서 등록 계정을 확인한 최신 근거가 필요합니다.",
            ));
        }
        record.session.state = "STARTING".into();
        record.session.supervisor = Some(request.supervisor);
        record.session.spawn_attempt_id = Some(request.spawn_attempt_id);
        record.session.verification = request.evidence.tier;
        record.session.updated_at = now;
        record.heartbeat_at = now;
        record.session.reason = Some(
            "실행 시도가 승인되었습니다. 프로세스 보고 전에는 슬롯을 반환하지 않습니다.".into(),
        );
        save_lease(&tx, &record)?;
        tx.commit().map_err(db_error)?;
        Ok(record.session)
    }
    /// Windows: 실행기가 만든 Job을 열어 둔다. 실패하면 아무것도 쥐지 않아 종료 근거가 없고 슬롯을 유지한다.
    #[cfg(windows)]
    fn hold_job(&self, session_id: &str, process: &ProcessIdentity) {
        if let Ok(job) = aam_protocol::ProcessJob::open(&aam_protocol::job_name(process.pid, &process.started_at)) {
            if let Ok(mut jobs) = self.jobs.lock() {
                jobs.entry(session_id.to_owned()).or_insert(job);
            }
        }
    }

    /// Windows: 쥔 Job 안에 살아 있는 프로세스가 하나도 없으면 참. 핸들이 없거나 조회에 실패하면 거짓이다.
    #[cfg(windows)]
    fn job_finished(&self, session_id: &str) -> bool {
        self.jobs.lock().ok().is_some_and(|jobs| {
            jobs.get(session_id)
                .is_some_and(|job| job.active_processes().is_ok_and(|active| active == 0))
        })
    }
    #[cfg(not(windows))]
    fn job_finished(&self, _session_id: &str) -> bool {
        false
    }

    fn started(&self, request: Started, now: i64) -> Result<Session, ApiError> {
        let previous = {
            let store = self.lock()?;
            lease(&store.connection, &request.session_id)?
        };
        authorize(&previous, &request.capability)?;
        #[cfg(windows)]
        self.hold_job(&request.session_id, &request.process);
        let child_verified = previous
            .session
            .supervisor
            .as_ref()
            .is_some_and(|supervisor| process::is_child(&request.process, supervisor));
        let liveness = process::inspect(&request.process);
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let mut record = lease(&tx, &request.session_id)?;
        authorize(&record, &request.capability)?;
        if record.session.spawn_attempt_id.as_deref() != Some(request.spawn_attempt_id.as_str())
            || terminal(&record.session)
        {
            return Err(ApiError::new(
                "LEASE_FENCED",
                "승인된 실행 시도와 일치하지 않습니다.",
            ));
        }
        if let Some(process) = &record.session.process {
            if process != &request.process {
                return Err(ApiError::new(
                    "PROCESS_IDENTITY_MISMATCH",
                    "이미 등록한 실행 프로세스와 다릅니다.",
                ));
            }
            return Ok(record.session);
        }
        if !matches!(
            record.session.state.as_str(),
            "STARTING" | "SUSPECT" | "ORPHANED"
        ) || !child_verified
            || liveness != Liveness::Alive
        {
            record.session.state = "SUSPECT".into();
            record.session.updated_at = now;
            // 매우 짧은 child는 실행기가 birth identity를 읽은 뒤 보고 전에 종료할 수 있습니다.
            if liveness == Liveness::Dead
                && request.process.pid > 0
                && !request.process.started_at.is_empty()
                && record
                    .session
                    .supervisor
                    .as_ref()
                    .is_some_and(|supervisor| {
                        supervisor.pid != request.process.pid
                            && supervisor.boot_id == request.process.boot_id
                    })
            {
                record.session.process = Some(request.process);
            }
            record.session.reason =
                Some("실행 결과 또는 프로세스 identity를 확인할 수 없어 슬롯을 유지합니다.".into());
            save_lease(&tx, &record)?;
            tx.commit().map_err(db_error)?;
            return Err(ApiError::new(
                "PROCESS_IDENTITY_UNVERIFIED",
                "실행기 자식의 시작 identity를 확인하지 못했습니다. 실행을 반복하지 마세요.",
            ));
        }
        record.session.process = Some(request.process);
        record.session.state = "ACTIVE".into();
        record.session.updated_at = now;
        record.heartbeat_at = now;
        record.session.reason = Some(
            "시작 전 계정을 확인했습니다. 실행 중 upstream identity는 별도 검증 대상입니다.".into(),
        );
        save_lease(&tx, &record)?;
        tx.commit().map_err(db_error)?;
        Ok(record.session)
    }
    fn heartbeat(&self, request: LeaseAction, now: i64) -> Result<Session, ApiError> {
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let mut record = lease(&tx, &request.session_id)?;
        authorize(&record, &request.capability)?;
        if !terminal(&record.session) {
            record.heartbeat_at = now;
            record.session.updated_at = now;
            save_lease(&tx, &record)?;
        }
        tx.commit().map_err(db_error)?;
        Ok(record.session)
    }
    fn finish(&self, request: LeaseAction, abort: bool, now: i64) -> Result<Session, ApiError> {
        let previous = {
            let store = self.lock()?;
            lease(&store.connection, &request.session_id)?
        };
        authorize(&previous, &request.capability)?;
        let liveness = previous.session.process.as_ref().map(process::inspect);
        let native_completion = !abort
            && matches!(previous.session.tool.as_str(), "claude" | "codex")
            && matches!(
                request.reason.as_deref(),
                Some("native-exit-foreground-confirmed" | "native-exit-background-unverified")
            );
        let foreground_confirmed = native_completion
            && request.reason.as_deref() == Some("native-exit-foreground-confirmed")
            && request
                .background_processes
                .as_ref()
                .is_some_and(|background| {
                    // Windows는 프로세스 그룹이 없으므로 서비스가 쥔 Job의 활성 프로세스 0으로 확인한다.
                    background.is_empty()
                        && (self.job_finished(&request.session_id)
                            || previous.session.process.as_ref().is_some_and(|native| {
                                process::background_exit_confirmed(native, background)
                            }))
                });
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let mut record = lease(&tx, &request.session_id)?;
        authorize(&record, &request.capability)?;
        if terminal(&record.session) {
            return Ok(record.session);
        }
        if record.session.process != previous.session.process
            || record.session.background_processes != previous.session.background_processes
            || record.session.updated_at != previous.session.updated_at
            || record.session.state != previous.session.state
        {
            return Err(ApiError::new(
                "SESSION_CONFLICT",
                "실행 상태가 바뀌었습니다. 최신 세션을 확인하세요.",
            ));
        }
        let background_unknown =
            request.reason.as_deref() == Some("native-exit-background-unverified");
        if native_completion {
            record.session.background_processes = request.background_processes;
        }
        if record.session.state == "PREPARED" {
            record.session.state = "ABORTED".into();
            record.session.reason = Some("시작 전에 예약을 취소했습니다.".into());
        } else if abort
            && matches!(record.session.state.as_str(), "STARTING" | "SUSPECT")
            && record.session.process.is_none()
            && request.reason.as_deref() == Some("spawn-failed")
        {
            record.session.state = "FAILED".into();
            record.session.reason = Some("실행기가 OS 프로세스 생성 실패를 확인했습니다.".into());
        } else if foreground_confirmed {
            record.session.state = "EXITED".into();
            record.session.reason = Some("등록된 실행 프로세스의 종료를 확인했습니다.".into());
        } else {
            record.session.state = if background_unknown || liveness == Some(Liveness::Alive) {
                "ORPHANED"
            } else {
                "SUSPECT"
            }
            .into();
            record.session.reason = Some(if background_unknown { "전경 프로세스는 종료했지만 native 백그라운드 작업 소멸을 확인하지 못해 슬롯을 유지합니다." } else { "실행 결과가 불확실하거나 프로세스가 살아 있어 슬롯을 유지합니다." }.into());
        }
        if request.session_persisted == Some(false) {
            record.session.native_session_id = None;
        } else if record.session.tool == "codex" && record.session.native_session_id.is_none() {
            // 다른 관리 세션이 이미 쓰는 대화 ID는 기록하지 않는다(이어 간 대화는 시작 때 이미 ID가 있다).
            let reported = request
                .native_session_id
                .as_deref()
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                .map(|id| id.to_string());
            if let Some(id) = reported {
                let taken = leases(&tx)?.iter().any(|other| {
                    other.session.id != record.session.id
                        && other.session.tool == "codex"
                        && other.session.native_session_id.as_deref() == Some(id.as_str())
                });
                if !taken {
                    record.session.native_session_id = Some(id);
                }
            }
        }
        record.session.updated_at = now;
        record.session.exit_code = request.exit_code;
        save_lease(&tx, &record)?;
        tx.commit().map_err(db_error)?;
        Ok(record.session)
    }
    pub fn start_refresh(self: &Arc<Self>) {
        if self
            .refreshing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let service = Arc::clone(self);
        std::thread::spawn(move || {
            let result = service.refresh_inner();
            if result.is_err() {
                if let Ok(store) = service.lock() {
                    let _ = set_metadata(&store.connection, "notices", &vec![Notice { id: "refresh-failed".into(), level: "warning".into(), title: "사용량 조회 실패".into(), message: "마지막 실제 관측을 유지합니다. 조회 실패를 새 관측으로 표시하지 않습니다.".into() }]);
                }
            }
            service.refreshing.store(false, Ordering::Release);
        });
    }
    fn refresh_inner(&self) -> Result<(), ApiError> {
        let before = {
            let store = self.lock()?;
            accounts(&store.connection)?
        };
        let scan = aam_adapters::scan(&self.paths, &before)?;
        self.apply_scan(before, scan)
    }

    fn apply_scan(
        &self,
        before: Vec<Account>,
        scan: aam_adapters::ScanResult,
    ) -> Result<(), ApiError> {
        let found_ids: std::collections::HashSet<String> = scan
            .accounts
            .iter()
            .map(|account| account.id.clone())
            .collect();
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        let current = accounts(&tx)?;
        for mut incoming in scan.accounts {
            let previous = current.iter().find(|account| account.id == incoming.id);
            if let Some(previous) = previous {
                incoming.enabled = previous.enabled;
                incoming.max_concurrency = previous.max_concurrency;
                // 자동 생성 라벨에는 확인된 이메일 표시를 덧붙이고, 사용자가 바꾼 이름은 유지합니다.
                if !incoming.label.starts_with(&previous.label) {
                    incoming.label = previous.label.clone();
                }
                if previous.identity_key.is_some() && previous.identity_key != incoming.identity_key
                {
                    incoming.identity_key = previous.identity_key.clone();
                    incoming.can_launch = false;
                    incoming.verification = "observed".into();
                    incoming.reason = Some("등록된 프로필의 identity 변경을 감지했습니다. 활성 세션을 유지하고 새 배정을 차단합니다.".into());
                }
                // 진행 중 사용자 등록/검증이 바뀌었다면 오래된 스캔으로 덮지 않습니다.
                if before
                    .iter()
                    .find(|account| account.id == previous.id)
                    .is_some_and(|original| {
                        original.identity_key != previous.identity_key
                            || original.profile_path != previous.profile_path
                            || original.auth_status != previous.auth_status
                            || original.can_launch != previous.can_launch
                            || original.verification != previous.verification
                            || original.last_checked_at != previous.last_checked_at
                    })
                {
                    continue;
                }
            }
            save_account(&tx, &incoming)?;
        }
        // 스캔에서 누락된 기존 identity는 삭제하지 않고 비활성 관측으로 남깁니다.
        let mut current_policy = policy(&tx)?;
        let mut policy_changed = false;
        for mut previous in current {
            // native binding에 명시적으로 통합된 읽기 전용 관측과, 실행 어댑터가 제거된 도구(grok·agy)의
            // 행만 제거하고 세션 이력은 보존합니다.
            let obsolete = matches!(previous.tool.as_str(), "grok" | "agy")
                || (previous.tool == "omp" && scan.merged_observation_ids.contains(&previous.id));
            if obsolete {
                let removed = tx.execute(
                    "DELETE FROM accounts WHERE id=?1 AND NOT EXISTS (SELECT 1 FROM leases WHERE account_id=?1)",
                    [&previous.id],
                ).map_err(db_error)?;
                if removed > 0 {
                    policy_changed |= forget_account(&mut current_policy, &previous.id);
                    continue;
                }
            }
            if !found_ids.contains(&previous.id) && before.iter().any(|old| old.id == previous.id) {
                previous.can_launch = false;
                previous.auth_status = "unverified".into();
                previous.verification = "observed".into();
                previous.reason = Some("마지막 조회에서 이 identity가 관측되지 않았습니다. 이전 사용량은 오래된 자료로만 보존합니다.".into());
                for bucket in &mut previous.buckets {
                    bucket.status = "stale".into();
                }
                save_account(&tx, &previous)?;
            }
        }
        // 실행 도구가 아닌 규칙(grok·agy·omp)은 검증을 통과하지 못하므로 함께 정리합니다.
        let routes = current_policy.project_routes.len();
        current_policy
            .project_routes
            .retain(|route| matches!(route.tool.as_str(), "claude" | "codex"));
        policy_changed |= routes != current_policy.project_routes.len();
        if policy_changed {
            current_policy.revision = current_policy.revision.checked_add(1).ok_or_else(|| {
                ApiError::new("POLICY_CONFLICT", "설정 버전 범위를 넘었어요.")
            })?;
            set_metadata(&tx, "policy", &current_policy)?;
        }
        set_metadata(&tx, "tools", &scan.tools)?;
        set_metadata(&tx, "notices", &scan.notices)?;
        set_metadata(&tx, "lastRefreshAt", &now_ms())?;
        tx.commit().map_err(db_error)?;
        Ok(())
    }
    pub fn reconcile(&self, restarting: bool) -> Result<(), ApiError> {
        let records = {
            let store = self.lock()?;
            leases(&store.connection)?
        };
        // Windows: 더 이상 용량을 쥐지 않는 세션의 Job 핸들은 놓는다.
        #[cfg(windows)]
        if let Ok(mut jobs) = self.jobs.lock() {
            let live: std::collections::HashSet<&str> = records
                .iter()
                .filter(|record| scheduler::holds_capacity(&record.session.state))
                .map(|record| record.session.id.as_str())
                .collect();
            jobs.retain(|id, _| live.contains(id.as_str()));
        }
        let observations: Vec<_> = records
            .into_iter()
            .filter(|record| scheduler::holds_capacity(&record.session.state))
            .map(|record| {
                let child = record.session.process.as_ref().map(process::inspect);
                let supervisor = record.session.supervisor.as_ref().map(process::inspect);
                let background_finished = record
                    .session
                    .background_processes
                    .as_ref()
                    .zip(record.session.process.as_ref())
                    .is_some_and(|(background, native)| {
                        process::background_exit_confirmed(native, background)
                    });
                let job_finished = self.job_finished(&record.session.id);
                (record, child, supervisor, background_finished, job_finished)
            })
            .collect();
        let now = now_ms();
        let mut store = self.lock()?;
        let tx = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(db_error)?;
        expire(&tx, now)?;
        for (previous, child, supervisor, background_finished, job_finished) in observations {
            let mut record = lease(&tx, &previous.session.id)?;
            if terminal(&record.session)
                || record.session.updated_at != previous.session.updated_at
                || record.session.process != previous.session.process
                || record.session.background_processes != previous.session.background_processes
                || record.session.state != previous.session.state
            {
                continue;
            }
            let next = if record.session.state == "PREPARED" {
                if restarting {
                    Some((
                        "ABORTED",
                        "서비스 재시작 전 미실행 예약을 취소했습니다. 새 요청으로 시작하세요.",
                    ))
                } else {
                    None
                }
            } else if job_finished {
                // Windows: 실행기가 native를 넣은 Job은 자손이 벗어날 수 없다. 활성 프로세스 0이 전체 종료의 근거다.
                Some(("EXITED", "실행 Job 안의 native 프로세스와 모든 자손의 종료를 확인했습니다."))
            } else if record.session.background_processes.is_none()
                && process::all_from_previous_boot(
                    &record
                        .session
                        .process
                        .iter()
                        .chain(record.session.supervisor.iter())
                        .collect::<Vec<_>>(),
                )
            {
                // 자손 기록이 없는 세션은 같은 부팅 안에서는 정리 근거가 없어 유지하고, 재부팅만 종료 근거로 쓴다.
                Some(("EXITED", "이전 부팅에서 시작된 세션입니다. 재부팅으로 실행기와 모든 자손이 종료되었으므로 슬롯을 반환합니다."))
            } else if record.session.background_processes.is_some() {
                if background_finished {
                    Some(("EXITED", "등록된 native 프로세스와 백그라운드 자손 및 원래 프로세스 그룹의 종료를 확인했습니다."))
                } else {
                    Some(("ORPHANED", "백그라운드 프로세스 또는 원래 프로세스 그룹의 종료가 확인되지 않아 슬롯을 유지합니다."))
                }
            } else if record.session.state == "ORPHANED"
                && record
                    .session
                    .reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("백그라운드"))
            {
                None
            } else if child == Some(Liveness::Dead) && supervisor == Some(Liveness::Alive) {
                Some(("SUSPECT", "전경 프로세스 종료를 확인했습니다. 실행기의 자손 프로세스 정리 근거를 기다립니다."))
            } else if child == Some(Liveness::Dead) {
                Some(("ORPHANED", "전경 프로세스는 종료했지만 실행기와 백그라운드 작업 정리 근거가 없어 슬롯을 유지합니다."))
            } else if child == Some(Liveness::Alive) && supervisor == Some(Liveness::Dead) {
                Some((
                    "ORPHANED",
                    "실행기가 종료되었지만 자식 프로세스는 살아 있습니다. 슬롯을 유지합니다.",
                ))
            } else if child == Some(Liveness::Alive)
                && supervisor == Some(Liveness::Alive)
                && now.saturating_sub(record.heartbeat_at) <= self.suspect_ms
            {
                Some(("ACTIVE", "프로세스 identity와 실행기 생존을 확인했습니다."))
            } else if restarting || now.saturating_sub(record.heartbeat_at) > self.suspect_ms {
                Some(("SUSPECT", "시작 결과 또는 생존 확인이 불확실합니다. 시간 초과만으로 슬롯을 반환하지 않습니다."))
            } else {
                None
            };
            if let Some((state, reason)) = next {
                if record.session.state != state {
                    record.session.state = state.into();
                    record.session.reason = Some(reason.into());
                    record.session.updated_at = now;
                    save_lease(&tx, &record)?;
                }
            }
        }
        tx.commit().map_err(db_error)?;
        Ok(())
    }
    pub fn start_background(self: &Arc<Self>) {
        self.start_refresh();
        let _ = self.bridge.set(bridge::Bridge::start(self));
        let service = Arc::clone(self);
        std::thread::spawn(move || {
            // 벽시계로 잰다. macOS의 Instant는 잠자기 동안 멈춰서, 노트북을 깨운 뒤에도
            // 남은 간격만큼 조회가 밀리고 그동안 사용량이 오래된 값으로 남았다.
            let mut refresh_at = aam_protocol::now_ms() + refresh_interval_ms();
            loop {
                std::thread::sleep(Duration::from_secs(2));
                let _ = service.reconcile(false);
                let now = aam_protocol::now_ms();
                if now >= refresh_at {
                    service.start_refresh();
                    refresh_at = now + refresh_interval_ms();
                }
            }
        });
    }
}

/// 새로 등록하는 binding이 기존 계정 행을 대체하는지 판단합니다.
/// - 같은 프로필의 미로그인 binding
/// - 다른 프로필에 등록된 같은 native identity (만료 후 재로그인, 프로필 이전)
/// 같은 프로필에 다른 identity가 있던 경우는 대체가 아니라 이력 보존 대상입니다.
fn supersedes(registered: &Account, previous: &Account) -> bool {
    if previous.id == registered.id || previous.tool != registered.tool {
        return false;
    }
    if previous.profile_path == registered.profile_path {
        return previous.identity_key.is_none();
    }
    registered
        .identity_key
        .as_deref()
        .is_some_and(|key| !key.is_empty() && previous.identity_key.as_deref() == Some(key))
}
fn refresh_interval_ms() -> i64 {
    let random = uuid::Uuid::new_v4();
    let jitter = u16::from_be_bytes([random.as_bytes()[0], random.as_bytes()[1]]) as i64 % 151;
    (225 + jitter) * 1000
}
fn admission_allowed(connection: &rusqlite::Connection) -> Result<(), ApiError> {
    if metadata::<Option<String>>(connection, "uninstallPermit")?
        .flatten()
        .is_some()
    {
        return Err(ApiError::new(
            "ADMISSION_DISABLED",
            "서비스 해제가 준비되어 신규 배정을 중지했습니다. 다시 설치하거나 해제를 취소하세요.",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
