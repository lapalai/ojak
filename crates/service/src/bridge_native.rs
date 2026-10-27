//! Minimal access-only broker for one registered native CLI profile per gateway.
//! Refresh tokens never leave the official CLI store. Unsupported broker operations
//! return 404 rather than claiming to have persisted data.
use super::*;
use aam_adapters::NativeAccess;
use parking_lot::{Condvar, Mutex as Lock};
use std::{
    collections::{HashSet, VecDeque},
    sync::{LazyLock, OnceLock, Weak},
    time::Instant,
};

/// 한 인증 갱신 job 전체의 상한. 요청 마감과는 분리되어, 대기자가 떠나도 job은 이 시간까지 끝날 수 있다.
const REFRESH_JOB_BUDGET: Duration = Duration::from_secs(20);
/// 갱신이 실패한 직후 대기 중인 요청이 CLI를 다시 띄우지 않도록 같은 실패를 재사용하는 시간.
const FAILURE_COOLDOWN: Duration = Duration::from_secs(3);
/// 대기자가 소켓 reset을 확인하는 간격. 상태 잠금은 이 사이 풀려 있다.
const WAIT_SLICE: Duration = Duration::from_millis(250);
const POLL_SLICE: Duration = Duration::from_millis(500);
const FRESH_MS: i64 = 30_000;
/// 전역 갱신 자리를 기다리는 최대 시간(요청 마감이 더 짧으면 그쪽). 자리가 나지 않으면 `AUTH_REFRESH_BUSY`다.
const PERMIT_WAIT: Duration = Duration::from_secs(10);
/// 같은 소스의 실패 로그 최소 간격. 같은 코드가 이어지는 동안은 아예 다시 남기지 않는다.
const FAILURE_LOG_INTERVAL_MS: i64 = 30_000;

#[cfg(test)]
type Loader = Arc<dyn Fn(&Account, bool, Instant, &mut dyn FnMut(&'static str)) -> Result<NativeAccess, ApiError> + Send + Sync>;
/// bridge.log에 한 줄을 남기는 곳. 상태 잠금을 쥔 채 부르지 않는다.
pub(super) type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// 전역 동시 인증 갱신 상한과 공정한(FIFO) 대기열. 자리가 없으면 정해진 시간까지 도착 순서대로 기다린다.
/// 대기자는 프로필(소스)마다 하나뿐이라(`SourceState::starting`) 대기열은 프로필 수를 넘지 않는다.
/// 잠금은 계수 갱신에만 쓰고 대기·CLI 실행 중에는 잡지 않는다.
pub(super) struct RefreshBudget {
    state: Lock<BudgetState>,
    changed: Condvar,
    limit: usize,
}
#[derive(Default)]
struct BudgetState {
    active: usize,
    queue: VecDeque<u64>,
    next: u64,
}
struct RefreshPermit {
    budget: Arc<RefreshBudget>,
}
impl Drop for RefreshPermit {
    fn drop(&mut self) {
        {
            let mut state = self.budget.state.lock();
            state.active = state.active.saturating_sub(1);
        }
        self.budget.changed.notify_all();
    }
}
/// 대기열 표. 어떤 경로로 대기를 끝내도 표를 지우고 다음 대기자를 깨운다.
struct BudgetTicket<'a> {
    budget: &'a RefreshBudget,
    id: u64,
    queued: bool,
}
impl Drop for BudgetTicket<'_> {
    fn drop(&mut self) {
        if !self.queued {
            return;
        }
        {
            let mut state = self.budget.state.lock();
            state.queue.retain(|id| *id != self.id);
        }
        self.budget.changed.notify_all();
    }
}
impl RefreshBudget {
    pub fn new(limit: usize) -> Self {
        Self { state: Lock::new(BudgetState::default()), changed: Condvar::new(), limit }
    }
    pub fn active(&self) -> usize {
        self.state.lock().active
    }
    pub fn waiting(&self) -> usize {
        self.state.lock().queue.len()
    }
    pub fn limit(&self) -> usize {
        self.limit
    }
    /// 자리를 얻을 때까지 `deadline`까지 기다린다. 시간이 지나면 `AUTH_REFRESH_BUSY`, `cancel`이 참이면 `AUTH_REFRESH_CANCELLED`.
    /// 대기 중 새로 온 요청은 앞선 대기자를 앞지르지 못한다.
    fn acquire_until(self: &Arc<Self>, deadline: Instant, cancel: &dyn Fn() -> bool) -> Result<RefreshPermit, ApiError> {
        if cancel() {
            return Err(cancelled_error());
        }
        let mut state = self.state.lock();
        if Instant::now() >= deadline {
            return Err(busy_error());
        }
        if state.queue.is_empty() && state.active < self.limit {
            state.active += 1;
            return Ok(RefreshPermit { budget: Arc::clone(self) });
        }
        let id = state.next;
        state.next += 1;
        state.queue.push_back(id);
        let mut ticket = BudgetTicket { budget: self, id, queued: true };
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(state);
                return Err(busy_error());
            }
            if state.queue.front() == Some(&id) && state.active < self.limit {
                state.queue.pop_front();
                state.active += 1;
                ticket.queued = false;
                let more = !state.queue.is_empty() && state.active < self.limit;
                drop(state);
                if more {
                    self.changed.notify_all();
                }
                return Ok(RefreshPermit { budget: Arc::clone(self) });
            }
            self.changed.wait_for(&mut state, left.min(WAIT_SLICE));
            drop(state);
            if cancel() {
                return Err(cancelled_error());
            }
            state = self.state.lock();
        }
    }
}

/// 같은 공식 프로필은 Source가 새로 만들어져도 동시에 한 job만 돌린다. 키는 프로필 경로(없으면 계정 id).
fn running_profiles() -> &'static Lock<HashSet<String>> {
    static RUNNING: LazyLock<Lock<HashSet<String>>> = LazyLock::new(|| Lock::new(HashSet::new()));
    &RUNNING
}

/// 공유 갱신 하나. 결과는 상태 잠금 아래에서 한 번만 정해지고 모든 대기자가 같은 값을 본다.
struct Job {
    force: bool,
    deadline: Instant,
    started: Instant,
    /// worker가 실제로 시작한 단계(`aam_adapters::NATIVE_ACCESS_STAGES` 이름 또는 서비스 쪽 `state-check`/`state-recheck`).
    /// 실패한 단계 보고에 쓴다. 잠금은 대입·복사에만 쓴다.
    stage: Lock<&'static str>,
    result: OnceLock<Result<(), ApiError>>,
}
struct Failure {
    error: ApiError,
    until: Instant,
}
struct FailureRecord {
    code: String,
    stage: &'static str,
    duration_ms: u64,
    at: i64,
}
/// 프로필별 안전한 진단. 코드·단계 이름·시간·횟수만 담는다(토큰·이메일·CLI 출력 없음).
#[derive(Default)]
struct Health {
    failures: u64,
    recoveries: u64,
    consecutive: u64,
    busy: u64,
    last_failure: Option<FailureRecord>,
    last_success_at: i64,
    streak_started_at: i64,
    /// 마지막으로 로그에 남긴 실패 코드. 같은 코드가 이어지면 다시 남기지 않고, 복구하면 지운다.
    logged_code: Option<String>,
    last_log_at: i64,
    suppressed: u64,
}

pub(super) struct Source {
    pub account: Account,
    pub provider: &'static str,
    pub identity: String,
    /// 로그·진단의 비식별 계정 칸(identity sha256 앞 12 hex).
    alias: String,
    owner: Weak<Service>,
    budget: Arc<RefreshBudget>,
    log: Option<LogSink>,
    #[cfg(test)]
    loader: Loader,
    job_budget: Duration,
    cooldown: Duration,
    /// 짧은 상태만 보관한다. CLI·DB·소켓·로그 I/O를 하는 동안에는 절대 잡지 않는다.
    state: Lock<SourceState>,
    /// 상태(`generation`)·job 결과가 바뀌면 깨운다. long-poll과 갱신 대기자가 변경·만료·마감까지 잠든다.
    changed: Condvar,
}
#[derive(Default)]
struct SourceState {
    access: Option<NativeAccess>,
    checked_at: i64,
    generation: i64,
    disabled: Option<[u8; 32]>,
    blocks: Vec<Value>,
    /// 결과를 기다리는 공유 job.
    job: Option<Arc<Job>>,
    /// Retain the latest job so permit waiters can observe it even after a fast completion.
    last_job: Option<Arc<Job>>,
    /// worker 스레드가 실제로 끝나기 전까지 true. 대기자가 떠나거나 job이 시간 초과로 정리돼도 새 worker를 겹쳐 띄우지 않는다.
    running: bool,
    /// 한 대기자가 이 프로필의 전역 갱신 자리를 기다리는 중이다. 다른 대기자는 자리를 따로 기다리지 않고 그 결과를 함께 기다린다.
    starting: bool,
    failed: Option<Failure>,
    health: Health,
}
impl SourceState {
    fn rejected(&self) -> bool {
        self.disabled.is_some_and(|disabled| {
            self.access.as_ref().is_some_and(|access| {
                let fingerprint: [u8; 32] = Sha256::digest(access.access.as_bytes()).into();
                disabled == fingerprint
            })
        })
    }
    fn fresh(&self) -> bool {
        let now = now_ms();
        now - self.checked_at < FRESH_MS && self.access.as_ref().is_some_and(|a| a.expires > now + 60_000)
    }
    /// long-poll이 깨어날 때마다 CLI를 다시 띄우지 않도록, 이미 읽은 token이 아직 쓸 수 있는지만 본다.
    fn usable(&self) -> bool {
        let now = now_ms();
        now - self.checked_at < FRESH_MS && self.access.as_ref().is_some_and(|a| a.expires > now)
    }
}
/// worker 스레드가 소유한다. 어떤 경로로 사라져도(정상·panic·스레드 시작 실패) 상태·permit·프로필 등록을 되돌린다.
struct JobGuard {
    source: Arc<Source>,
    job: Arc<Job>,
    key: String,
    outcome: Option<Result<NativeAccess, ApiError>>,
    permit: Option<RefreshPermit>,
}
impl Drop for JobGuard {
    fn drop(&mut self) {
        self.source.settle(&self.job, &self.key, self.outcome.take(), self.permit.take());
    }
}

pub(super) fn identity(account: &Account) -> Option<(&'static str, String)> {
    let provider = match account.tool.as_str() {
        "claude" => "anthropic",
        "codex" => "openai-codex",
        _ => return None,
    };
    if !account.enabled || account.auth_status != "authenticated" {
        return None;
    }
    let key = account.identity_key.as_deref()?;
    let email = account
        .email
        .as_deref()
        .filter(|s| !s.is_empty())?
        .to_lowercase();
    let org = key
        .split('|')
        .find_map(|part| part.strip_prefix("workspace:"))
        .filter(|s| !s.is_empty())?;
    Some((
        provider,
        format!("email:{email}|org:{}", org.to_lowercase()),
    ))
}

impl Source {
    pub fn new(
        account: Account,
        provider: &'static str,
        identity: String,
        owner: Weak<Service>,
        budget: Arc<RefreshBudget>,
    ) -> Self {
        Self {
            account,
            provider,
            alias: account_key(&identity),
            identity,
            owner,
            budget,
            log: None,
            #[cfg(test)]
            loader: Arc::new(|account: &Account, refresh: bool, deadline: Instant, stage: &mut dyn FnMut(&'static str)| {
                aam_adapters::native_access_staged(account, refresh, deadline, stage)
            }),
            job_budget: REFRESH_JOB_BUDGET,
            cooldown: FAILURE_COOLDOWN,
            state: Lock::new(SourceState::default()),
            changed: Condvar::new(),
        }
    }
    /// 실패·복구 한 줄을 남길 곳.
    pub(super) fn with_log(mut self, log: LogSink) -> Self {
        self.log = Some(log);
        self
    }
    #[cfg(test)]
    fn with_loader(mut self, loader: Loader, job_budget: Duration, cooldown: Duration) -> Self {
        self.loader = loader;
        self.job_budget = job_budget;
        self.cooldown = cooldown;
        self
    }
    fn emit(&self, line: Option<String>) {
        if let (Some(line), Some(log)) = (line, &self.log) {
            log(&line);
        }
    }

    /// 이 프로필의 안전한 진단. 상태 잠금은 값을 복사하는 동안만 쥔다.
    pub(super) fn auth_status(&self) -> super::NativeAuthStatus {
        let state = self.state.lock();
        let health = &state.health;
        super::NativeAuthStatus {
            provider: self.provider.to_owned(),
            account_key: self.alias.clone(),
            state: if health.consecutive > 0 { "failing" } else if health.last_success_at > 0 { "ok" } else { "unknown" },
            activity: state.job.as_ref().map(|job| super::NativeActivity {
                stage: *job.stage.lock(),
                elapsed_ms: job.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            }),
            last_failure: health.last_failure.as_ref().map(|failure| super::NativeFailure {
                code: failure.code.clone(),
                stage: failure.stage,
                duration_ms: failure.duration_ms,
                at: failure.at,
                transient: transient_failure(&failure.code),
            }),
            last_success_at: (health.last_success_at > 0).then_some(health.last_success_at),
            failures: health.failures,
            recoveries: health.recoveries,
            consecutive_failures: health.consecutive,
            busy_rejections: health.busy,
        }
    }

    /// 최신 DB binding(계정 id·identity·프로필·바이너리·활성/인증 상태)이 이 Source가 만들어진 때와 같은지 본다.
    /// 상태 잠금 밖에서만 호출한다.
    pub(super) fn validate(&self) -> Result<(), ApiError> {
        let unavailable = || ApiError::new("AUTH_REFRESH_UNAVAILABLE", "계정 상태를 확인하지 못했어요.");
        let service = self.owner.upgrade().ok_or_else(unavailable)?;
        let current = {
            let store = service.lock()?;
            crate::store::account(&store.connection, &self.account.id)
        };
        let current = match current {
            Ok(account) => account,
            Err(error) if error.code == "ACCOUNT_NOT_FOUND" => {
                return Err(ApiError::new("ACCOUNT_DISABLED", "이 계정은 더 이상 등록되어 있지 않아요."))
            }
            Err(_) => return Err(unavailable()),
        };
        if !current.enabled {
            return Err(ApiError::new("ACCOUNT_DISABLED", "이 계정의 배정이 꺼져 있어요."));
        }
        if current.auth_status == "auth-required" {
            return Err(ApiError::new("AUTH_REQUIRED", "사용할 수 있는 공식 CLI 로그인이 없어요."));
        }
        if current.auth_status != "authenticated" {
            return Err(unavailable());
        }
        if current.identity_key != self.account.identity_key
            || identity(&current).as_ref().map(|(provider, identity)| (*provider, identity.as_str()))
                != Some((self.provider, self.identity.as_str()))
        {
            return Err(ApiError::new("PROFILE_IDENTITY_MISMATCH", "공식 CLI의 로그인 계정이 바뀌었어요."));
        }
        if current.profile_path != self.account.profile_path || current.binary_path != self.account.binary_path {
            return Err(ApiError::new("PROFILE_UNVERIFIED", "프로필 연결이 바뀌었어요."));
        }
        Ok(())
    }

    /// 상태 잠금 없이 CLI를 호출하는 worker 본문. 단계는 실제로 시작할 때 `job.stage`에 기록된다.
    fn run(&self, job: &Job) -> Result<NativeAccess, ApiError> {
        let mut stage = |name: &'static str| *job.stage.lock() = name;
        stage("state-check");
        self.validate()?;
        #[cfg(test)]
        let access = (self.loader)(&self.account, job.force, job.deadline, &mut stage)?;
        #[cfg(not(test))]
        let access = aam_adapters::native_access_staged(&self.account, job.force, job.deadline, &mut stage)?;
        if Instant::now() >= job.deadline {
            return Err(timeout_error());
        }
        // 늦게 도착한 결과가 그 사이 바뀐 binding·제외된 계정 위에 게시되지 않게 다시 확인한다.
        stage("state-recheck");
        self.validate()?;
        Ok(access)
    }

    /// 잠금 아래에서 실패를 기록한다. 낡은 token으로 덮지 않고(access 폐기), 짧은 cooldown 동안 같은 실패를 공유한다.
    /// 돌려주는 로그 한 줄은 잠금을 푼 뒤 `emit`으로 남긴다. job당 한 번만 불리므로 같은 실패가 두 번 세어지지 않는다.
    #[must_use]
    fn fail_locked(&self, state: &mut SourceState, job: &Arc<Job>, error: ApiError) -> Option<String> {
        state.access = None;
        state.checked_at = 0;
        state.generation += 1;
        state.failed = cacheable(&error.code).then(|| Failure { error: error.clone(), until: Instant::now() + self.cooldown });
        let event = self.record_failure(state, job, &error);
        let _ = job.result.set(Err(error));
        if state.job.as_ref().is_some_and(|current| Arc::ptr_eq(current, job)) {
            state.job = None;
        }
        event
    }

    fn record_failure(&self, state: &mut SourceState, job: &Job, error: &ApiError) -> Option<String> {
        let (at, stage) = (now_ms(), *job.stage.lock());
        let duration_ms = job.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
        let health = &mut state.health;
        health.failures += 1;
        health.consecutive += 1;
        if health.consecutive == 1 {
            health.streak_started_at = at;
        }
        health.last_failure = Some(FailureRecord { code: error.code.clone(), stage, duration_ms, at });
        // 같은 코드가 이어지면 다시 남기지 않는다. 코드가 바뀌어도 최소 간격 안이면 건너뛴 수만 센다.
        if health.logged_code.as_deref() == Some(error.code.as_str()) {
            return None;
        }
        if at - health.last_log_at < FAILURE_LOG_INTERVAL_MS {
            health.suppressed += 1;
            return None;
        }
        health.logged_code = Some(error.code.clone());
        health.last_log_at = at;
        let suppressed = std::mem::take(&mut health.suppressed);
        Some(format_failure_line(at, self.provider, &self.alias, &error.code, stage, duration_ms, health.consecutive, suppressed))
    }

    /// worker의 마지막 단계. 이미 시간 초과로 정리된 job의 늦은 결과는 버린다.
    /// permit·프로필 점유·`running`은 같은 잠금 안에서 함께 풀려, 결과를 본 대기자가 다음 job을 시작해도 혼잡으로 거절되지 않는다.
    fn settle(&self, job: &Arc<Job>, key: &str, outcome: Option<Result<NativeAccess, ApiError>>, permit: Option<RefreshPermit>) {
        let mut state = self.state.lock();
        drop(permit);
        let mut event = None;
        if job.result.get().is_none() {
            match outcome {
                Some(Ok(access)) if Instant::now() < job.deadline => {
                    if state.access.as_ref().is_none_or(|old| old.access != access.access || old.expires != access.expires) {
                        state.generation += 1;
                    }
                    state.checked_at = now_ms();
                    state.access = Some(access);
                    state.failed = None;
                    event = self.record_success(&mut state);
                    let _ = job.result.set(Ok(()));
                    if state.job.as_ref().is_some_and(|current| Arc::ptr_eq(current, job)) {
                        state.job = None;
                    }
                }
                Some(Ok(_)) => event = self.fail_locked(&mut state, job, timeout_error()),
                Some(Err(error)) => event = self.fail_locked(&mut state, job, error),
                None => {
                    event = self.fail_locked(
                        &mut state,
                        job,
                        ApiError::new("AUTH_REFRESH_UNAVAILABLE", "인증 갱신 작업이 결과 없이 끝났어요."),
                    )
                }
            }
        }
        state.running = false;
        running_profiles().lock().remove(key);
        self.changed.notify_all();
        drop(state);
        self.emit(event);
    }

    fn record_success(&self, state: &mut SourceState) -> Option<String> {
        let at = now_ms();
        let health = &mut state.health;
        health.last_success_at = at;
        if health.consecutive == 0 {
            return None;
        }
        health.recoveries += 1;
        let line = format_recovery_line(at, self.provider, &self.alias, health.consecutive, at - health.streak_started_at);
        health.consecutive = 0;
        health.logged_code = None;
        health.suppressed = 0;
        Some(line)
    }

    /// job을 등록하고 프로필 점유를 얻는다. 이미 도는 worker가 있거나 같은 프로필을 다른 소스가 쓰면 busy다. 잠금 아래에서 호출한다.
    fn begin(self: &Arc<Self>, state: &mut SourceState, force: bool, permit: RefreshPermit) -> Result<JobGuard, ApiError> {
        if state.running {
            return Err(busy_error());
        }
        let key = self.account.profile_path.clone().unwrap_or_else(|| format!("account:{}", self.account.id));
        if !running_profiles().lock().insert(key.clone()) {
            return Err(busy_error());
        }
        let now = Instant::now();
        let job = Arc::new(Job { force, deadline: now + self.job_budget, started: now, stage: Lock::new("state-check"), result: OnceLock::new() });
        state.running = true;
        state.job = Some(Arc::clone(&job));
        state.last_job = Some(Arc::clone(&job));
        Ok(JobGuard { source: Arc::clone(self), job, key, outcome: None, permit: Some(permit) })
    }

    /// 잠금 밖에서 worker를 띄운다. 시작에 실패하면 closure가 버려지며 guard가 상태를 정리하고 UNAVAILABLE을 게시한다.
    fn spawn(self: &Arc<Self>, guard: JobGuard) {
        let worker = Arc::clone(self);
        let _ = std::thread::Builder::new().name("ojak-native-refresh".into()).spawn(move || {
            let mut guard = guard;
            let outcome = worker.run(&guard.job);
            guard.outcome = Some(outcome);
        });
    }

    /// job 결과를 기다린다. 바깥 Err는 이 대기자만의 마감·취소이며 공유 job에는 영향이 없다.
    fn wait_job(
        &self,
        job: &Arc<Job>,
        deadline: Instant,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Result<(), ApiError>, ApiError> {
        loop {
            let mut state = self.state.lock();
            let now = Instant::now();
            if now >= deadline {
                return Err(timeout_error());
            }
            if let Some(result) = job.result.get() {
                return Ok(result.clone());
            }
            if now >= job.deadline {
                // job 자체의 마감: 모든 대기자가 같은 시간 초과를 본다. 아직 도는 worker는 `running`으로 겹침을 막는다.
                // 실패한 단계는 worker가 마지막으로 시작한 단계(멈춰 있는 곳)다.
                let event = self.fail_locked(&mut state, job, timeout_error());
                self.changed.notify_all();
                drop(state);
                self.emit(event);
                continue;
            }
            let wake = deadline.min(job.deadline).min(now + WAIT_SLICE);
            let _ = self.changed.wait_until(&mut state, wake);
            drop(state);
            if cancel() {
                return Err(cancelled_error());
            }
        }
    }

    /// 최신 access를 보장한다. 신선하면 즉시, 아니면 공유 job에 합류하거나 시작한다.
    /// 시작할 때 전역 갱신 자리가 없으면 상태 잠금 밖에서 순서대로 기다린다(프로필마다 대기자는 하나).
    fn ensure(self: &Arc<Self>, force: bool, deadline: Instant, cancel: &dyn Fn() -> bool) -> Result<(), ApiError> {
        let mut joined_start = false;
        let mut preceding_job: Option<Arc<Job>> = None;
        loop {
            if Instant::now() >= deadline {
                return Err(timeout_error());
            }
            if cancel() {
                return Err(cancelled_error());
            }
            let mut state = self.state.lock();
            if !force && state.fresh() {
                return Ok(());
            }
            let joined_job = state.job.clone().or_else(|| {
                if !joined_start { return None; }
                state.last_job.as_ref()
                    .filter(|job| preceding_job.as_ref().is_none_or(|previous| !Arc::ptr_eq(previous, job)))
                    .cloned()
            });
            if let Some(job) = joined_job {
                drop(state);
                let outcome = self.wait_job(&job, deadline, cancel)?;
                if force && !job.force {
                    // 강제 갱신은 진행 중이던 일반 조회와 합칠 수 없다. 끝난 뒤 다시 판단한다.
                    joined_start = false;
                    preceding_job = None;
                    continue;
                }
                return outcome;
            }
            if let Some(failure) = &state.failed {
                if Instant::now() < failure.until {
                    return Err(failure.error.clone());
                }
                state.failed = None;
            }
            if state.starting {
                if !joined_start {
                    preceding_job = state.last_job.clone();
                    joined_start = true;
                }
                // 다른 대기자가 이 프로필의 전역 자리를 기다린다. 자리를 따로 기다리지 않고 그가 시작할 job을 함께 기다린다.
                let _ = self.changed.wait_until(&mut state, deadline.min(Instant::now() + WAIT_SLICE));
                continue;
            }
            state.starting = true;
            drop(state);
            let permit = self.budget.acquire_until(deadline.min(Instant::now() + PERMIT_WAIT), cancel);
            let cancelled = cancel();
            let mut state = self.state.lock();
            state.starting = false;
            self.changed.notify_all();
            if cancelled {
                return Err(cancelled_error());
            }
            let permit = match permit {
                Ok(permit) => permit,
                Err(error) => {
                    if error.code == "AUTH_REFRESH_BUSY" {
                        state.health.busy += 1;
                    }
                    return Err(error);
                }
            };
            if Instant::now() >= deadline {
                return Err(timeout_error());
            }
            // 기다리는 사이 다른 대기자의 job이 시작·완료됐을 수 있다. 상태를 다시 보고 필요할 때만 시작한다.
            if state.job.is_some() || state.failed.is_some() || (!force && state.fresh()) {
                drop(state);
                drop(permit);
                continue;
            }
            let guard = match self.begin(&mut state, force, permit) {
                Ok(guard) => guard,
                Err(error) => {
                    state.health.busy += 1;
                    return Err(error);
                }
            };
            let job = Arc::clone(&guard.job);
            drop(state);
            self.spawn(guard);
            return self.wait_job(&job, deadline, cancel)?;
        }
    }

    pub fn ready(self: &Arc<Self>) -> Result<(), ApiError> {
        self.validate()?;
        self.ensure(false, Instant::now() + self.job_budget + Duration::from_secs(1), &|| false)?;
        self.validate()?;
        if self.state.lock().rejected() {
            return Err(ApiError::new("AUTH_REQUIRED", "공급자가 기존 인증을 거절했어요."));
        }
        Ok(())
    }
    pub(super) fn reset_credential(self: &Arc<Self>) -> Result<Value, ApiError> {
        self.ready()?;
        self.validate()?;
        self.entry(&self.state.lock()).map(|entry| entry["credential"].clone())
            .ok_or_else(|| ApiError::new("AUTH_REQUIRED", "공식 CLI의 유효한 구독 로그인을 확인하지 못했어요."))
    }
    fn entry(&self, state: &SourceState) -> Option<Value> {
        let access = state.access.as_ref()?;
        if state.rejected() {
            return None;
        }
        let mut credential = json!({
            "type": "oauth", "access": access.access, "refresh": "__remote__", "expires": access.expires,
        });
        if let Some(email) = &self.account.email {
            credential["email"] = json!(email);
        }
        if let Some(id) = &access.account_id {
            credential["accountId"] = json!(id);
        }
        if let Some(org) = self
            .identity
            .split('|')
            .find_map(|s| s.strip_prefix("org:"))
        {
            credential["orgId"] = json!(org);
        }
        Some(
            json!({ "id": 1, "provider": self.provider, "identityKey": self.identity, "credential": credential }),
        )
    }

    pub fn handle(self: &Arc<Self>, client: &mut TcpStream, request: &Request, path: &str, deadline: Instant) {
        if let Err(error) = self.validate() {
            return auth_error(client, error);
        }
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        if request.method == "GET" && route == "/v1/healthz" {
            return respond(client, 200, &json!({"ok":true}));
        }
        // omp 18.8.3 explicitly falls back to long polling on a stream 404.
        if route == "/v1/snapshot/stream" {
            return respond(
                client,
                404,
                &json!({"error":"Snapshot streaming is not supported."}),
            );
        }
        if request.method == "GET" && route == "/v1/snapshot" {
            return self.snapshot(client, request, query, deadline);
        }
        // 변경 요청이 끝나 잠금이 풀린 뒤 `generation`이 바뀌었으면 long-poll을 깨운다.
        let before = self.state.lock().generation;
        let _wake = WakeOnChange { source: self, before };
        match (request.method.as_str(), route) {
            ("POST", "/v1/credential/1/refresh") => {
                if let Err(error) = self.ensure(true, deadline, &|| client_gone(client)) {
                    return auth_error(client, error);
                }
                let entry = {
                    let state = self.state.lock();
                    self.entry(&state)
                };
                if let Err(error) = self.validate() {
                    return auth_error(client, error);
                }
                match entry {
                    Some(entry) => respond(client, 200, &json!({"entry":entry})),
                    None => respond(
                        client,
                        401,
                        &json!({"error":"Native credential was rejected; sign in through its official CLI."}),
                    ),
                }
            }
            ("POST", "/v1/credential/1/disable") => {
                // Disable only this rejected access token in the bridge. Do not
                // revoke or delete the user's canonical CLI login.
                {
                    let mut state = self.state.lock();
                    if let Some(access) = &state.access {
                        state.disabled = Some(Sha256::digest(access.access.as_bytes()).into());
                        state.generation += 1;
                    }
                }
                respond(client, 200, &json!({"ok":true}));
            }
            ("POST", "/v1/credential/1/block") => {
                let Ok(value) = serde_json::from_slice::<Value>(&request.body) else {
                    return respond(client, 400, &json!({"error":"Invalid block."}));
                };
                let Some(provider) = value["providerKey"]
                    .as_str()
                    .filter(|v| *v == self.provider)
                else {
                    return respond(client, 400, &json!({"error":"Invalid block provider."}));
                };
                let Some(scope) = value["blockScope"].as_str() else {
                    return respond(client, 400, &json!({"error":"Invalid block scope."}));
                };
                let Some(until) = value["blockedUntilMs"].as_i64() else {
                    return respond(client, 400, &json!({"error":"Invalid block expiry."}));
                };
                {
                    let mut state = self.state.lock();
                    state
                        .blocks
                        .retain(|b| b["providerKey"] != provider || b["blockScope"] != scope);
                    state.blocks.push(json!({"providerKey":provider,"blockScope":scope,"blockedUntilMs":until,"updatedAtMs":now_ms()}));
                    state.generation += 1;
                }
                respond(client, 200, &json!({"ok":true}));
            }
            ("DELETE", "/v1/credential/1/block") => {
                let Ok(value) = serde_json::from_slice::<Value>(&request.body) else {
                    return respond(client, 400, &json!({"error":"Invalid block."}));
                };
                if value["providerKey"].as_str() != Some(self.provider)
                    || !value["blockScope"].is_string()
                {
                    return respond(client, 400, &json!({"error":"Invalid block selector."}));
                }
                {
                    let mut state = self.state.lock();
                    state.blocks.retain(|b| {
                        b["providerKey"] != value["providerKey"]
                            || b["blockScope"] != value["blockScope"]
                    });
                    state.generation += 1;
                }
                respond(client, 200, &json!({"ok":true}));
            }
            ("DELETE", "/v1/credential/1/blocks") => {
                {
                    let mut state = self.state.lock();
                    state.blocks.clear();
                    state.generation += 1;
                }
                respond(client, 200, &json!({"ok":true}));
            }
            _ => respond(
                client,
                404,
                &json!({"error":"This native credential source does not support that broker operation."}),
            ),
        }
    }

    /// `wait`는 요청 마감(`deadline`) 안에서만 유효하다. 갱신 대기도 같은 요청 마감을 쓰고 job 마감과는 별개다.
    fn snapshot(self: &Arc<Self>, client: &mut TcpStream, request: &Request, query: &str, deadline: Instant) {
        let wait = query
            .split('&')
            .find_map(|v| v.strip_prefix("wait="))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            .min(25_000);
        let previous = request
            .header("if-none-match")
            .and_then(|v| v.trim_matches('"').parse::<i64>().ok());
        let poll_deadline = (Instant::now() + Duration::from_millis(wait)).min(deadline);
        let mut first = true;
        loop {
            if first || !self.state.lock().usable() {
                if let Err(error) = self.ensure(false, deadline, &|| client_gone(client)) {
                    return auth_error(client, error);
                }
            }
            first = false;
            let mut state = self.state.lock();
            let now = now_ms();
            let count = state.blocks.len();
            state
                .blocks
                .retain(|b| b["blockedUntilMs"].as_i64().is_some_and(|t| t > now));
            if state.blocks.len() != count {
                state.generation += 1;
                self.changed.notify_all();
            }
            let left = poll_deadline.saturating_duration_since(Instant::now());
            if previous != Some(state.generation) || left.is_zero() {
                let entries: Vec<Value> = self
                    .entry(&state)
                    .into_iter()
                    .map(|mut entry| {
                        entry["rotatesInMs"] =
                            json!(state.access.as_ref().map(|a| (a.expires - now).max(0)));
                        entry["blocks"] = json!(state.blocks);
                        entry
                    })
                    .collect();
                let generation = state.generation;
                let body = json!({"generation":generation,"generatedAt":now,"serverNowMs":now,
                    "refresher":{"enabled":false,"intervalMs":60_000,"skewMs":0,"nextSweepInMs":0},"credentials":entries});
                drop(state);
                if let Err(error) = self.validate() {
                    return auth_error(client, error);
                }
                let text = body.to_string();
                let _ = write!(client,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nETag: \"{}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",generation,text.len(),text);
                finish(client);
                return;
            }
            // 바뀔 때까지 잠근 채 기다리지 않고 잠금을 풀고 잔다. 깨우는 시점: 상태 변경, 가장 이른 차단 만료,
            // 접근 토큰 재확인 시점(30초), 요청 마감, 연결 확인 간격 중 가장 이른 것.
            let next_block = state.blocks.iter().filter_map(|b| b["blockedUntilMs"].as_i64()).min()
                .map(|t| Duration::from_millis((t - now).max(1) as u64));
            let recheck = Duration::from_millis((FRESH_MS - (now - state.checked_at)).clamp(1, FRESH_MS) as u64);
            let sleep = [Some(left), next_block, Some(recheck), Some(POLL_SLICE)].into_iter().flatten().min().unwrap_or(left);
            let _ = self.changed.wait_for(&mut state, sleep);
            drop(state);
            if client_gone(client) {
                return;
            }
        }
    }
}

/// 소켓이 명백히 끊겼는지(reset 등)만 본다. FIN(`peek`가 0)은 정상 write-half-close일 수 있어 취소로 보지 않는다.
/// 상태 잠금을 잡지 않은 채 호출한다.
pub(super) fn client_gone(client: &TcpStream) -> bool {
    if client.set_nonblocking(true).is_err() {
        return false;
    }
    let mut byte = [0u8; 1];
    let gone = match client.peek(&mut byte) {
        Ok(_) => false,
        Err(error) => !matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted | io::ErrorKind::TimedOut
        ),
    };
    let _ = client.set_nonblocking(false);
    gone
}

fn busy_error() -> ApiError {
    ApiError::new("AUTH_REFRESH_BUSY", "다른 인증 갱신이 진행 중이에요.")
}
fn timeout_error() -> ApiError {
    ApiError::new("AUTH_REFRESH_TIMEOUT", "공식 CLI 인증 확인·갱신 작업의 제한 시간이 지났어요.")
}
fn cancelled_error() -> ApiError {
    ApiError::new("AUTH_REFRESH_CANCELLED", "요청이 취소됐어요.")
}
/// 호출자 쪽 사정(혼잡·취소)은 다음 요청이 다시 시도할 수 있어야 하므로 cooldown에 넣지 않는다.
fn cacheable(code: &str) -> bool {
    !matches!(code, "AUTH_REFRESH_BUSY" | "AUTH_REFRESH_CANCELLED")
}

/// 일시적인 CLI·시간·혼잡 실패인지. 명시적 비활성·인증 거절·identity/override 충돌·응답 형식 변경은 false다.
/// 호출자는 일시 실패로 실행 중인 gateway를 내리면 안 된다(요청은 계속 source가 최신 상태로 검증하고, 실패한 동안 낡은 token은 내주지 않는다).
/// 근거(adapters 코드):
/// - `PROBE_TIMEOUT`·`AUTH_REFRESH_TIMEOUT`: 작업 deadline 초과(`process.rs` check_deadline, `access.rs` ensure_deadline).
/// - `PROBE_START_FAILED`: `Command::spawn` 실패. 바이너리 존재·실행 가능은 `executable()`이 먼저 `BINARY_MISSING`/`BINARY_INVALID`로 걸러서 남는 시작 실패는 자원·일시 사유다.
/// - `PROBE_IO`·`PROBE_CLOSED`: 자식 프로세스 입출력 실패·응답 전 종료. 로그인 판정과 무관하다.
/// - `NATIVE_METADATA_UNAVAILABLE`: Codex app-server가 JSON-RPC error를 돌려줌(`process.rs` request).
/// - `NATIVE_AUTH_UNAVAILABLE`: 직전 `auth status`가 로그인을 확인한 뒤 `claude -p /usage`가 `is_error:true`로 응답(`access_claude.rs`). 원인은 응답에 없다.
/// - `NATIVE_KEYCHAIN_LOCKED`: macOS 키체인이 errSecItemNotFound(44) 외로 실패.
/// - `CLI_INSPECTION_FAILED`: 공식 CLI 조회 명령 실행 실패.
pub(super) fn transient_failure(code: &str) -> bool {
    matches!(
        code,
        "AUTH_REFRESH_BUSY"
            | "AUTH_REFRESH_TIMEOUT"
            | "AUTH_REFRESH_CANCELLED"
            | "AUTH_REFRESH_UNAVAILABLE"
            | "PROBE_TIMEOUT"
            | "PROBE_START_FAILED"
            | "PROBE_IO"
            | "PROBE_CLOSED"
            | "CLI_INSPECTION_FAILED"
            | "NATIVE_METADATA_UNAVAILABLE"
            | "NATIVE_AUTH_UNAVAILABLE"
            | "NATIVE_KEYCHAIN_LOCKED"
    )
}

/// `<ms> bridge/native session=- outcome=auth-failure account=<12hex> provider=<p> code=<CODE> stage=<stage> duration_ms=<n> consecutive=<n> transient=<bool> suppressed=<n>`
/// 사용량(`->`) 줄이 아니고 `outcome=`이 있어 데스크톱 집계에 들어가지 않는다. 비밀·이메일·CLI 출력은 담지 않는다.
pub(super) fn format_failure_line(at: i64, provider: &str, alias: &str, code: &str, stage: &str, duration_ms: u64, consecutive: u64, suppressed: u64) -> String {
    format!(
        "{at} bridge/native session=- outcome=auth-failure account={alias} provider={} code={} stage={} duration_ms={duration_ms} consecutive={consecutive} transient={} suppressed={suppressed}",
        sanitize_log_token(provider),
        sanitize_log_token(code),
        sanitize_log_token(stage),
        transient_failure(code),
    )
}
/// `<ms> bridge/native session=- outcome=auth-recovered account=<12hex> provider=<p> failures=<n> down_ms=<n>`
pub(super) fn format_recovery_line(at: i64, provider: &str, alias: &str, failures: u64, down_ms: i64) -> String {
    format!(
        "{at} bridge/native session=- outcome=auth-recovered account={alias} provider={} failures={failures} down_ms={}",
        sanitize_log_token(provider),
        down_ms.max(0),
    )
}

/// 변경 요청이 끝나면(잠금이 풀린 뒤) `generation`이 바뀌었는지 보고 long-poll을 깨운다.
struct WakeOnChange<'a> {
    source: &'a Source,
    before: i64,
}
impl Drop for WakeOnChange<'_> {
    fn drop(&mut self) {
        if self.source.state.lock().generation != self.before {
            self.source.changed.notify_all();
        }
    }
}
fn auth_error(client: &mut TcpStream, error: ApiError) {
    // Never forward native CLI stderr or credential payloads.
    let (status, message) = failure(&error.code);
    respond(client, status, &json!({"code":error.code,"error":message}));
}

pub(super) fn failure(code: &str) -> (u16, &'static str) {
    match code {
        "AUTH_REQUIRED" => (401, "공식 CLI의 로그인이 유효하지 않아요. Ojak → 연결에서 해당 계정에 다시 로그인해 주세요. omp에 별도로 로그인할 필요는 없어요."),
        "AUTH_STATUS_UNCONFIRMED" => (503, "공식 CLI 로그인 상태를 아직 확인하지 못했어요. 로그아웃으로 확인된 것은 아니에요. 잠시 뒤 다시 시도해 주세요. 다시 로그인할 필요는 없어요."),
        "ACCOUNT_DISABLED" => (409, "계정이 배정에서 제외되어 있어요. Ojak → 사용 현황 → 계정 고르기에서 해당 계정을 포함해 주세요. 다시 로그인할 필요는 없어요."),
        "PROFILE_IDENTITY_MISMATCH" => (409, "공식 CLI의 로그인 계정이 바뀌었어요. Ojak에서 계정을 새로고침한 뒤 확인해 주세요."),
        "AUTH_OVERRIDE_CONFLICT" => (409, "API 키·다른 공급자·인증 도우미 설정이 구독 로그인과 충돌해 연결을 멈췄어요. Ojak의 계정 오류 상세에서 해당 설정을 확인해 주세요."),
        "CLI_NOT_FOUND" | "BINARY_MISSING" => (503, "등록된 공식 CLI를 찾지 못했어요. Ojak → 연결에서 원본 CLI 설치 상태를 확인해 주세요."),
        "BINARY_INVALID" | "SHIM_RECURSION" => (409, "연결된 공식 CLI 경로가 실행 파일이 아니거나 Ojak shim이에요. Ojak → 연결에서 원본 CLI 경로를 확인해 주세요."),
        "NATIVE_KEYCHAIN_LOCKED" => (503, "Claude 키체인에 접근하지 못했어요. macOS 키체인 잠금과 접근 권한을 확인해 주세요. 다시 로그인할 필요는 없어요."),
        "NATIVE_STORAGE_UNSUPPORTED" => (409, "이 Codex 프로필의 인증 저장소는 직접 연결할 수 없어요. 저장소 설정은 바꾸지 않았어요. Ojak → 연결에서 이 계정의 저장소 설정을 확인해 주세요."),
        "NATIVE_CREDENTIAL_READ" => (503, "공식 CLI의 인증 파일을 읽지 못했어요. Ojak → 연결에서 원래 프로필 경로와 파일 접근 권한을 확인해 주세요."),
        "NATIVE_CREDENTIAL_FORMAT" => (503, "공식 CLI 인증 저장소의 형식을 해석하지 못했어요. 공식 CLI 버전을 확인해 주세요."),
        "NATIVE_AUTH_UNAVAILABLE" => (503, "Claude 공식 CLI가 인증 갱신 확인에 오류로 응답했어요. 로그인이 거절됐다고 확인된 것은 아니에요. 잠시 뒤 다시 시도해 주세요. 다시 로그인할 필요는 없어요."),
        "NATIVE_REFRESH_UNEXPECTED" => (503, "Claude 공식 CLI의 인증 갱신 응답이 예상한 형식이 아니에요. 공식 CLI 버전을 확인해 주세요."),
        "NATIVE_METADATA_UNAVAILABLE" => (503, "공식 CLI가 계정 정보 조회를 거부했어요. 잠시 뒤 다시 시도해 주세요. 다시 로그인할 필요는 없어요."),
        "PROFILE_UNVERIFIED" | "PROFILE_UNBOUND" | "PROFILE_BINDING_CHANGED" | "PROFILE_INVALID" => (409, "공식 CLI의 프로필 또는 워크스페이스 연결을 확인하지 못했어요. Ojak → 연결에서 원래 프로필 폴더를 확인해 주세요. 폴더를 옮겼다면 원래 경로로 되돌리거나, 이 계정의 연결을 끊은 뒤 새 경로를 연결해 주세요."),
        "PROBE_TIMEOUT" => (503, "공식 CLI 조회가 제한 시간 안에 끝나지 않았어요. 잠시 뒤 다시 시도해 주세요. 다시 로그인할 필요는 없어요."),
        "PROBE_START_FAILED" => (503, "공식 CLI 프로세스를 시작하지 못했어요. 잠시 뒤 다시 시도하고, 계속되면 설치와 실행 권한을 확인해 주세요. 다시 로그인할 필요는 없어요."),
        "PROBE_IO" | "PROBE_CLOSED" | "CLI_INSPECTION_FAILED" => (503, "공식 CLI와의 통신이 중간에 끊겼어요. 잠시 뒤 다시 시도해 주세요. 다시 로그인할 필요는 없어요."),
        "PROBE_OUTPUT_LIMIT" | "PROBE_SCHEMA" => (503, "공식 CLI 응답을 해석하지 못했어요. 공식 CLI 버전을 확인해 주세요."),
        "AUTH_REFRESH_BUSY" => (503, "다른 인증 갱신이 진행 중이에요. 잠시 뒤 다시 시도해 주세요. 다시 로그인할 필요는 없어요."),
        "AUTH_REFRESH_TIMEOUT" => (503, "공식 CLI의 인증 확인이 제한 시간 안에 끝나지 않았어요. 잠시 뒤 다시 시도해 주세요. 다시 로그인할 필요는 없어요."),
        "AUTH_REFRESH_CANCELLED" => (503, "요청이 취소되어 인증 갱신을 기다리지 않았어요."),
        "AUTH_REFRESH_UNAVAILABLE" => (503, "인증 갱신 작업을 완료하지 못했어요. 잠시 뒤 다시 시도해 주세요. 다시 로그인할 필요는 없어요."),
        _ => (503, "기존 공식 CLI 로그인을 읽거나 갱신하지 못했어요. Ojak → 연결에서 해당 계정 상태를 확인해 주세요. 이 오류만으로 omp에 다시 로그인할 필요는 없어요."),
    }
}


#[cfg(test)]
#[path = "bridge_native_tests.rs"]
mod tests;
