//! 브릿지 연결 예산·모델 대기열·본문 메모리 예산.
//!
//! - 전체 연결 상한은 공개 모델 listener와 내부 native broker listener가 나눠 쓴다(64..512, 기본 256).
//! - 연결 허가(`Permit`)는 accept 때 기다림 없이 얻거나 거절한다. 인증 전 연결은 어떤 것도 기다리지 않는다.
//! - 모델 스트림 몫은 **인증을 통과한 뒤에만** 유한한 FIFO 대기열에서 기다린다(`Permit::promote_wait`).
//!   본문은 모델 몫을 얻은 뒤에만 읽는다. 기다리는 동안 본문 바이트는 하나도 잡지 않는다.
//! - 본문 메모리는 전체 가중 합계 예산(`reserve_body`, 선언 길이 × `BODY_COST_FACTOR`)으로 묶는다. 연결 수 × 요청당 상한(64 MiB)이
//!   메모리 상한이 되지 않게 한다. 예산이 모자라면 대기 시간 안에서 기다렸다가 `payload`로 거절한다. 최악 입력의 메모리 보증은 아니다.
//! - 한도는 새 연결을 받을 때만 적용한다. 이미 받은 연결은 예산이 줄어도 끊지 않는다.
//! - Windows commit 여유가 위험 수준으로 내려가면(`runtime_health::ResourceGuard`) **새** 모델 요청만 `BRIDGE_RESOURCE_PRESSURE`(503)로 거절한다.
//!   판단은 모델 몫 대기 전, 대기열의 승격 직전, 본문 예약 전·대기 중에 한다(본문을 읽거나 할당하기 전). 이미 몫·예약을 쥔 스트림, 짧은 제어
//!   (healthz·모델 목록)와 native 경로는 영향이 없다. 표본이 없거나 오래돼 `unknown`이면 막지 않는다.
//! - 모든 허가·대기 표·본문 예약은 RAII라 오류·취소·조기 반환·패닉에서도 반드시 돌아온다. 잠금은 짧은 계수 갱신에만 쓰고 I/O는 하지 않는다.
use parking_lot::{Condvar, Mutex};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use crate::runtime_health::{monotonic_ms, ResourceGuard, ResourceStatus};

pub(super) const MIN_TOTAL: usize = 64;
pub(super) const MAX_TOTAL: usize = 512;
pub(super) const DEFAULT_TOTAL: usize = 256;
/// native 인증 갱신 전역 병렬도. 기존 native 검사 병렬도와 같다.
pub(super) const REFRESH_PARALLELISM: usize = 4;
/// 내부 예약이 아무리 커져도 공개 listener에 남기는 최소 연결 수.
const MIN_PUBLIC: usize = 16;
/// 공개 연결 중 모델 스트림이 쓰지 못하는 짧은 제어(healthz·모델 목록) 최소 몫.
const MIN_CONTROL: usize = 8;
/// 모델 몫을 기다리는 인증된 요청의 최소 대기열 길이.
const MIN_WAITING: usize = 4;
/// native long-poll(profile마다 gateway 하나가 하나씩 건다. 재연결 겹침 여유로 2배) 상한.
const MAX_NATIVE_POLLS: usize = 24;
/// native 짧은 제어 요청(refresh·disable·block·healthz) 예약 상한.
const MAX_NATIVE_SHORT: usize = 16;
/// 모델 몫을 기다리는 기본 최대 시간. 길게 기다려 봐야 스트림은 분 단위로 이어지므로 빨리 구조화된 거절을 돌려준다.
pub(super) const DEFAULT_MODEL_WAIT: Duration = Duration::from_secs(20);
const MAX_MODEL_WAIT_MS: u64 = 120_000;
/// 대기자가 소켓 reset을 확인하는 간격. 잠금은 이 사이 풀려 있다.
const WAIT_SLICE: Duration = Duration::from_millis(100);
/// 선언된 본문 길이에 곱하는 **가중치**. 읽는 중에는 원본 바이트, 파싱 중에는 원본 + `serde_json::Value` + 게이트웨이용 직렬화 본문이
/// 함께 살아 있다. `Value`의 크기는 입력 모양에 따라 달라서(아주 작은 원소가 많은 배열은 바이트당 10배 넘게 커질 수 있다)
/// 이 배수는 최악 경계가 아니라 일반적인 대화 본문(문자열이 대부분)에 맞춘 가중 예약이다. 합계 예산이 연결 수 × 64 MiB로
/// 부풀지 않게 묶는 것이 목적이며, 실제 메모리 상한 보증으로 읽으면 안 된다.
/// 원본과 `Value`가 모두 해제된 뒤에만 게이트웨이용 본문 크기로 줄인다(`BodyReservation::shrink_to`).
pub(super) const BODY_COST_FACTOR: usize = 8;
/// 요청당 상한(64 MiB)의 `BODY_COST_FACTOR`배(512 MiB) 하나는 항상 들어가도록 예산 최소값을 잡는다.
const MIN_BODY_BUDGET_MB: u64 = 512;
const DEFAULT_BODY_BUDGET_MB: u64 = 2048;
const MAX_BODY_BUDGET_MB: u64 = 8192;

/// 환경 변수로 조정하는 운영 한도. 해석하지 못한 값은 기본값, 범위를 벗어난 값은 범위 안으로 맞춘다.
/// - `AAM_BRIDGE_MAX_CONNECTIONS`: 전체 연결 상한 `64..=512`(기본 256)
/// - `AAM_BRIDGE_MODEL_WAIT_MS`: 모델 몫 대기 최대 시간 `0..=120000`(기본 20000, 0이면 기다리지 않고 바로 거절)
/// - `AAM_BRIDGE_BODY_BUDGET_MB`: 처리 중인 모델 요청 본문의 가중 합계 예산(선언 길이 × `BODY_COST_FACTOR`) `512..=8192`(기본 2048)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Config {
    pub total: usize,
    pub model_wait: Duration,
    pub body_budget: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            total: DEFAULT_TOTAL,
            model_wait: DEFAULT_MODEL_WAIT,
            body_budget: DEFAULT_BODY_BUDGET_MB as usize * 1024 * 1024,
        }
    }
}

impl Config {
    pub(super) fn from_env() -> Self {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Self {
        let number = |name: &str, default: u64, min: u64, max: u64| {
            get(name).and_then(|value| value.trim().parse::<u64>().ok()).map_or(default, |value| value.clamp(min, max))
        };
        Self {
            total: number("AAM_BRIDGE_MAX_CONNECTIONS", DEFAULT_TOTAL as u64, MIN_TOTAL as u64, MAX_TOTAL as u64) as usize,
            model_wait: Duration::from_millis(number("AAM_BRIDGE_MODEL_WAIT_MS", DEFAULT_MODEL_WAIT.as_millis() as u64, 0, MAX_MODEL_WAIT_MS)),
            body_budget: number("AAM_BRIDGE_BODY_BUDGET_MB", DEFAULT_BODY_BUDGET_MB, MIN_BODY_BUDGET_MB, MAX_BODY_BUDGET_MB) as usize * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scope {
    Public,
    Native,
}
impl Scope {
    pub(super) fn name(self) -> &'static str {
        match self {
            Scope::Public => "public",
            Scope::Native => "native",
        }
    }
    pub(super) fn busy_code(self) -> &'static str {
        match self {
            Scope::Public => "BRIDGE_BUSY",
            Scope::Native => "AUTH_BROKER_BUSY",
        }
    }
    pub(super) fn busy_message(self) -> &'static str {
        match self {
            Scope::Public => "Ojak 연결이 너무 많아요. 잠시 뒤 다시 시도해 주세요.",
            Scope::Native => "Ojak 인증 연결이 너무 많아요. 잠시 뒤 다시 시도해 주세요.",
        }
    }
}

/// native 프로필 수와 전체 상한에서 도출한 연결 한도.
/// - `polls = min(2 × profiles, 24)`, `short = min(profiles + 4, 16)`
/// - 공개 최소 16개를 지키도록 내부 합계를 `total - 16`으로 자른다(short 우선, 남는 만큼 polls).
/// - `native = polls + short`, `public = total - native`
/// - 공개 연결 안에서 `control = max(8, public/16)`, `waiting = max(4, public/8)`, `models = public - control - waiting`
///   (공개 연결 = 스트리밍 중인 모델 + 대기 중인 모델 요청 + 짧은 제어)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Limits {
    pub total: usize,
    pub public: usize,
    pub models: usize,
    pub waiting: usize,
    pub control: usize,
    pub native: usize,
    pub polls: usize,
}
impl Limits {
    pub(super) fn derive(profiles: usize, total: usize) -> Self {
        let total = total.clamp(MIN_TOTAL, MAX_TOTAL);
        let room = total - MIN_PUBLIC;
        let short = profiles.saturating_add(REFRESH_PARALLELISM).min(MAX_NATIVE_SHORT).min(room);
        let polls = profiles.saturating_mul(2).min(MAX_NATIVE_POLLS).min(room - short);
        let native = polls + short;
        let public = total - native;
        let control = (public / 16).max(MIN_CONTROL);
        let waiting = (public / 8).max(MIN_WAITING);
        Self { total, public, models: public.saturating_sub(control + waiting), waiting, control, native, polls }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct Counts {
    pub public: usize,
    pub models: usize,
    /// 모델 몫을 기다리는 인증된 요청 수.
    pub waiting: usize,
    pub native: usize,
    pub polls: usize,
}

/// 예산을 넘어 거절된 요청의 비밀 없는 설명.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Overload {
    pub scope: Scope,
    /// `connection`(전체), `model`(대기 없이 거절), `queue`(대기열 가득), `queue_timeout`(기다렸지만 자리가 나지 않음),
    /// `payload`(본문 메모리 예산), `poll`, `resource`(시스템 메모리 압박) 중 어느 몫이 찼는지.
    /// `resource`는 `active`가 지금 commit 여유, `limit`가 압박 진입 기준이며 단위가 MiB다.
    pub class: &'static str,
    pub active: usize,
    pub limit: usize,
}
impl Overload {
    pub(super) fn code(&self) -> &'static str {
        match self.class {
            "queue_timeout" => "BRIDGE_QUEUE_TIMEOUT",
            "resource" => "BRIDGE_RESOURCE_PRESSURE",
            _ => self.scope.busy_code(),
        }
    }
    pub(super) fn message(&self) -> &'static str {
        match self.class {
            "queue_timeout" => "Ojak 모델 연결 자리가 정해진 시간 안에 나지 않았어요. 잠시 뒤 다시 시도해 주세요.",
            "resource" => "Windows 메모리 여유가 매우 부족해서 Ojak이 새 모델 요청을 잠시 받지 않아요. 이미 진행 중인 요청은 계속 처리돼요. 잠시 뒤 다시 시도해 주세요.",
            "payload" => "Ojak이 처리 중인 요청 본문 메모리가 가득 찼어요. 잠시 뒤 다시 시도해 주세요.",
            _ => self.scope.busy_message(),
        }
    }
}

/// 모델 몫 대기가 끝난 이유가 거절이 아닌 경우.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Denied {
    Overload(Overload),
    /// 기다리는 동안 클라이언트가 떠났다. 응답할 상대가 없다.
    Cancelled,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Snapshot {
    pub limits: Limits,
    pub counts: Counts,
    pub rejected_public: u64,
    pub rejected_native: u64,
    /// 기다린 끝에 모델 몫을 얻은 누적 수와 기다리다 시간이 지난 누적 수.
    pub waited: u64,
    pub wait_timeouts: u64,
    pub body_bytes: usize,
    pub body_limit: usize,
    pub model_wait: Duration,
}

#[derive(Default)]
struct Inner {
    counts: Counts,
    /// 모델 몫을 기다리는 표. 앞에서부터 차례로만 받는다.
    queue: VecDeque<u64>,
    next_ticket: u64,
    body_bytes: usize,
}

pub(super) struct Admission {
    config: Config,
    inner: Mutex<Inner>,
    /// 모델 몫이 돌아오거나 대기열 앞이 바뀌면 깨운다.
    changed: Condvar,
    profiles: AtomicUsize,
    rejected_public: AtomicU64,
    rejected_native: AtomicU64,
    waited: AtomicU64,
    wait_timeouts: AtomicU64,
    /// 새 모델 요청 admission이 읽는 시스템 메모리 상태. 읽기만 하며 요청 경로에서 표본을 만들지 않는다.
    guard: Arc<ResourceGuard>,
}

impl Admission {
    #[cfg(test)]
    pub(super) fn new() -> Arc<Self> {
        Self::with_config(Config::default())
    }

    #[cfg(test)]
    pub(super) fn with_config(config: Config) -> Arc<Self> {
        Self::with_guard(config, ResourceGuard::new())
    }

    pub(super) fn with_guard(config: Config, guard: Arc<ResourceGuard>) -> Arc<Self> {
        let config = Config { total: config.total.clamp(MIN_TOTAL, MAX_TOTAL), ..config };
        Arc::new(Self {
            config,
            inner: Mutex::new(Inner::default()),
            changed: Condvar::new(),
            profiles: AtomicUsize::new(0),
            rejected_public: AtomicU64::new(0),
            rejected_native: AtomicU64::new(0),
            waited: AtomicU64::new(0),
            wait_timeouts: AtomicU64::new(0),
            guard,
        })
    }

    /// 이후 신규 admission의 예약량만 바꾼다. 이미 받은 연결은 건드리지 않는다.
    pub(super) fn set_native_profiles(&self, profiles: usize) {
        self.profiles.store(profiles, Ordering::SeqCst);
    }

    pub(super) fn limits(&self) -> Limits {
        Limits::derive(self.profiles.load(Ordering::SeqCst), self.config.total)
    }

    #[cfg(test)]
    pub(super) fn guard(&self) -> &Arc<ResourceGuard> {
        &self.guard
    }

    pub(super) fn resource_status(&self) -> ResourceStatus {
        self.guard.status(monotonic_ms())
    }

    /// 새 모델 작업을 받아도 되는지. 신선한 표본이 압박일 때만 거절한다. 원자값만 읽으므로 admission 잠금 밖에서 부른다.
    fn pressure_check(&self) -> Result<(), Overload> {
        const MIB: u64 = 1024 * 1024;
        self.guard
            .check(monotonic_ms())
            .map_err(|pressure| self.reject(Scope::Public, "resource", (pressure.available / MIB) as usize, (pressure.enter_below / MIB) as usize))
    }

    pub(super) fn snapshot(&self) -> Snapshot {
        let (counts, body_bytes) = {
            let inner = self.inner.lock();
            (Counts { waiting: inner.queue.len(), ..inner.counts }, inner.body_bytes)
        };
        Snapshot {
            limits: self.limits(),
            counts,
            rejected_public: self.rejected_public.load(Ordering::SeqCst),
            rejected_native: self.rejected_native.load(Ordering::SeqCst),
            waited: self.waited.load(Ordering::SeqCst),
            wait_timeouts: self.wait_timeouts.load(Ordering::SeqCst),
            body_bytes,
            body_limit: self.config.body_budget,
            model_wait: self.config.model_wait,
        }
    }

    fn reject(&self, scope: Scope, class: &'static str, active: usize, limit: usize) -> Overload {
        match scope {
            Scope::Public => &self.rejected_public,
            Scope::Native => &self.rejected_native,
        }
        .fetch_add(1, Ordering::Relaxed);
        Overload { scope, class, active, limit }
    }

    pub(super) fn admit(self: &Arc<Self>, scope: Scope) -> Result<Permit, Overload> {
        let limits = self.limits();
        let mut inner = self.inner.lock();
        let counts = &mut inner.counts;
        let total = counts.public + counts.native;
        if total >= limits.total {
            drop(inner);
            return Err(self.reject(scope, "connection", total, limits.total));
        }
        let (active, limit) = match scope {
            Scope::Public => (&mut counts.public, limits.public),
            Scope::Native => (&mut counts.native, limits.native),
        };
        if *active >= limit {
            let active = *active;
            drop(inner);
            return Err(self.reject(scope, "connection", active, limit));
        }
        *active += 1;
        drop(inner);
        Ok(Permit { admission: Arc::clone(self), scope, special: false })
    }

    /// 인증이 끝난 모델 요청이 본문을 읽기 전에 잡는 가중 메모리 예약(선언 길이 × `BODY_COST_FACTOR`).
    /// 합계 예산이 모자라면 `config.model_wait`까지 예약이 돌아오기를 기다린다(예약을 쥔 스트림이 끝나면 돌아온다).
    /// 요청 하나의 가중치가 예산 전체를 넘으면 기다려도 소용없으므로 바로 거절한다. 기다리는 동안 본문은 읽지 않는다.
    /// `cancel`은 잠금 밖에서 부르며 참이면 접는다(`Denied::Cancelled`).
    pub(super) fn reserve_body(self: &Arc<Self>, length: usize, cancel: &dyn Fn() -> bool) -> Result<BodyReservation, Denied> {
        let cost = length.saturating_mul(BODY_COST_FACTOR);
        // 본문 예약 전에 시스템 메모리 압박을 본다. 압박이면 아무것도 예약하지 않는다.
        self.pressure_check().map_err(Denied::Overload)?;
        let deadline = Instant::now() + self.config.model_wait;
        let mut inner = self.inner.lock();
        loop {
            if inner.body_bytes.saturating_add(cost) <= self.config.body_budget {
                inner.body_bytes += cost;
                drop(inner);
                return Ok(BodyReservation { admission: Arc::clone(self), bytes: cost });
            }
            let used = inner.body_bytes;
            let left = deadline.saturating_duration_since(Instant::now());
            if cost > self.config.body_budget || left.is_zero() {
                drop(inner);
                return Err(Denied::Overload(self.reject(Scope::Public, "payload", used, self.config.body_budget)));
            }
            self.changed.wait_for(&mut inner, left.min(WAIT_SLICE));
            drop(inner);
            if cancel() {
                return Err(Denied::Cancelled);
            }
            // 기다리는 동안 압박이 시작됐으면 본문을 읽기 전에 접는다.
            self.pressure_check().map_err(Denied::Overload)?;
            inner = self.inner.lock();
        }
    }
}

/// 연결 하나의 허가. `Drop`에서 반드시 돌려준다.
pub(super) struct Permit {
    admission: Arc<Admission>,
    scope: Scope,
    /// 공개: 모델 스트림 몫을 쥐었는지. native: long-poll 몫을 쥐었는지.
    special: bool,
}

impl std::fmt::Debug for Permit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Permit").field("scope", &self.scope).field("long", &self.special).finish()
    }
}

/// 대기열 표. 어떤 경로로 대기를 끝내도(거절·취소·패닉) 표를 지우고 다음 대기자를 깨운다.
struct Ticket<'a> {
    admission: &'a Admission,
    id: u64,
    queued: bool,
}
impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        if !self.queued {
            return;
        }
        {
            let mut inner = self.admission.inner.lock();
            inner.queue.retain(|id| *id != self.id);
        }
        self.admission.changed.notify_all();
    }
}

impl Permit {
    /// 요청 종류가 밝혀진 뒤 장기 연결 몫을 더 쥔다. native long-poll용이며 기다리지 않는다.
    /// 이미 쥐었으면 아무 일도 하지 않는다. 연결 허가는 그대로 유지하므로 실패해도 짧은 요청 몫은 줄지 않는다.
    pub(super) fn promote(&mut self) -> Result<(), Overload> {
        if self.special {
            return Ok(());
        }
        if self.scope == Scope::Public {
            self.admission.pressure_check()?;
        }
        let limits = self.admission.limits();
        let mut inner = self.admission.inner.lock();
        let (active, limit, class) = match self.scope {
            Scope::Public => (&mut inner.counts.models, limits.models, "model"),
            Scope::Native => (&mut inner.counts.polls, limits.polls, "poll"),
        };
        if *active >= limit {
            let active = *active;
            drop(inner);
            return Err(self.admission.reject(self.scope, class, active, limit));
        }
        *active += 1;
        self.special = true;
        Ok(())
    }

    /// 인증된 모델 요청이 스트림 몫을 얻는다. 자리가 없으면 유한한 FIFO 대기열에서 기다린다.
    /// 대기열이 가득 차면(`queue`) 바로, 시간이 지나면(`queue_timeout`) 구조화된 거절을 돌려준다.
    /// `cancel`은 잠금 밖에서 호출되며 참이면 대기를 접고 `Denied::Cancelled`를 돌려준다.
    /// 성공하면 기다렸는지(true)를 돌려준다.
    pub(super) fn promote_wait(&mut self, cancel: &dyn Fn() -> bool) -> Result<bool, Denied> {
        if self.special {
            return Ok(false);
        }
        // 몫을 얻기 전에 압박을 본다. 압박이면 대기열에도 들어가지 않는다. 이미 쥔 몫(`special`)은 위에서 그대로 돌려줬다.
        self.admission.pressure_check().map_err(Denied::Overload)?;
        debug_assert_eq!(self.scope, Scope::Public);
        let admission = Arc::clone(&self.admission);
        let limits = admission.limits();
        let wait = admission.config.model_wait;
        let mut inner = admission.inner.lock();
        // 앞서 기다리는 요청이 있으면 끼어들지 않는다.
        if inner.queue.is_empty() && inner.counts.models < limits.models {
            inner.counts.models += 1;
            self.special = true;
            return Ok(false);
        }
        if wait.is_zero() {
            let active = inner.counts.models;
            drop(inner);
            return Err(Denied::Overload(admission.reject(Scope::Public, "model", active, limits.models)));
        }
        if inner.queue.len() >= limits.waiting {
            let waiting = inner.queue.len();
            drop(inner);
            return Err(Denied::Overload(admission.reject(Scope::Public, "queue", waiting, limits.waiting)));
        }
        let id = inner.next_ticket;
        inner.next_ticket += 1;
        inner.queue.push_back(id);
        let mut ticket = Ticket { admission: &admission, id, queued: true };
        let deadline = Instant::now() + wait;
        loop {
            let limits = admission.limits();
            if inner.queue.front() == Some(&id) && inner.counts.models < limits.models {
                inner.queue.pop_front();
                inner.counts.models += 1;
                self.special = true;
                ticket.queued = false;
                let more = !inner.queue.is_empty() && inner.counts.models < limits.models;
                drop(inner);
                admission.waited.fetch_add(1, Ordering::Relaxed);
                if more {
                    admission.changed.notify_all();
                }
                return Ok(true);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                let waiting = inner.queue.len();
                drop(inner);
                admission.wait_timeouts.fetch_add(1, Ordering::Relaxed);
                return Err(Denied::Overload(admission.reject(Scope::Public, "queue_timeout", waiting, limits.waiting)));
            }
            admission.changed.wait_for(&mut inner, left.min(WAIT_SLICE));
            drop(inner);
            if cancel() {
                return Err(Denied::Cancelled);
            }
            // 대기열 승격 직전에 압박을 다시 본다. 압박이면 표(`Ticket`)가 대기열에서 빠지고 구조화된 거절을 받는다.
            admission.pressure_check().map_err(Denied::Overload)?;
            inner = admission.inner.lock();
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let freed_model = {
            let mut inner = self.admission.inner.lock();
            let counts = &mut inner.counts;
            let (active, special) = match self.scope {
                Scope::Public => (&mut counts.public, &mut counts.models),
                Scope::Native => (&mut counts.native, &mut counts.polls),
            };
            *active = active.saturating_sub(1);
            if self.special {
                *special = special.saturating_sub(1);
            }
            self.special && self.scope == Scope::Public
        };
        if freed_model {
            self.admission.changed.notify_all();
        }
    }
}

/// 처리 중인 모델 요청 본문의 가중 메모리 예약. `Drop`에서 돌려준다.
pub(super) struct BodyReservation {
    admission: Arc<Admission>,
    bytes: usize,
}
impl BodyReservation {
    /// 파싱이 끝나 실제로 남은 본문 크기만 쥔다.
    pub(super) fn shrink_to(&mut self, retained: usize) {
        let next = retained.min(self.bytes);
        let freed = self.bytes - next;
        if freed > 0 {
            {
                let mut inner = self.admission.inner.lock();
                inner.body_bytes = inner.body_bytes.saturating_sub(freed);
            }
            self.bytes = next;
            self.admission.changed.notify_all();
        }
    }
}
impl Drop for BodyReservation {
    fn drop(&mut self) {
        {
            let mut inner = self.admission.inner.lock();
            inner.body_bytes = inner.body_bytes.saturating_sub(self.bytes);
        }
        self.admission.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(total: usize, wait_ms: u64) -> Config {
        Config { total, model_wait: Duration::from_millis(wait_ms), ..Config::default() }
    }

    #[test]
    fn limits_follow_profiles_and_refresh_parallelism_with_caps_for_every_total() {
        assert_eq!(Limits::derive(0, 64), Limits { total: 64, public: 60, models: 45, waiting: 7, control: 8, native: 4, polls: 0 });
        assert_eq!(Limits::derive(1, 256), Limits { total: 256, public: 249, models: 203, waiting: 31, control: 15, native: 7, polls: 2 });
        assert_eq!(Limits::derive(100, 64).public, 24);
        for total in [0, 64, 100, 256, 512, usize::MAX] {
            for profiles in (0..2000).chain([usize::MAX]) {
                let limits = Limits::derive(profiles, total);
                assert!((MIN_TOTAL..=MAX_TOTAL).contains(&limits.total));
                assert_eq!(limits.public + limits.native, limits.total);
                assert!(limits.public >= MIN_PUBLIC, "{profiles}");
                assert!(limits.models + limits.waiting + limits.control <= limits.public);
                assert!(limits.models >= 1 && limits.waiting >= MIN_WAITING && limits.control >= MIN_CONTROL);
                assert!(limits.native - limits.polls >= REFRESH_PARALLELISM, "short control keeps the refresh parallelism: {profiles}");
            }
        }
    }

    #[test]
    fn the_default_total_holds_128_streams_with_a_queue_and_control_left_over() {
        let limits = Limits::derive(4, DEFAULT_TOTAL);
        assert!(limits.models >= 128 + limits.waiting, "{limits:?}");
        assert!(limits.control >= 8);
        // 프로필이 아무리 많아도 native 예약은 상한이 있어 128개 스트림은 항상 들어간다.
        for profiles in 0..=1000 {
            assert!(Limits::derive(profiles, DEFAULT_TOTAL).models >= 128, "{profiles}");
        }
    }

    #[test]
    fn config_clamps_out_of_range_values_and_ignores_unparsable_ones() {
        let lookup = |pairs: &'static [(&'static str, &'static str)]| {
            Config::from_lookup(move |name| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| (*value).to_owned()))
        };
        assert_eq!(lookup(&[]), Config::default());
        let low = lookup(&[("AAM_BRIDGE_MAX_CONNECTIONS", "1"), ("AAM_BRIDGE_MODEL_WAIT_MS", "999999999"), ("AAM_BRIDGE_BODY_BUDGET_MB", "1")]);
        assert_eq!((low.total, low.model_wait, low.body_budget), (64, Duration::from_millis(120_000), 512 << 20));
        let high = lookup(&[("AAM_BRIDGE_MAX_CONNECTIONS", "100000"), ("AAM_BRIDGE_BODY_BUDGET_MB", "999999")]);
        assert_eq!((high.total, high.body_budget), (512, 8192 << 20));
        assert_eq!(lookup(&[("AAM_BRIDGE_MAX_CONNECTIONS", "many"), ("AAM_BRIDGE_MODEL_WAIT_MS", "-1")]), Config::default());
        assert!(lookup(&[("AAM_BRIDGE_MODEL_WAIT_MS", "0")]).model_wait.is_zero());
    }

    #[test]
    fn public_models_leave_control_slots_and_native_polls_leave_short_slots() {
        let admission = Admission::with_config(config(64, 0));
        admission.set_native_profiles(1);
        let limits = admission.limits();
        let mut held = Vec::new();
        for _ in 0..limits.models {
            let mut permit = admission.admit(Scope::Public).unwrap();
            permit.promote().unwrap();
            held.push(permit);
        }
        let mut extra = admission.admit(Scope::Public).unwrap();
        let over = extra.promote().unwrap_err();
        assert_eq!((over.scope, over.class, over.limit), (Scope::Public, "model", limits.models));
        // 모델 몫이 가득 차도 짧은 제어는 남은 공개 몫만큼 계속 들어온다.
        let mut controls = vec![extra];
        while controls.len() < limits.public - limits.models {
            controls.push(admission.admit(Scope::Public).unwrap());
        }
        assert_eq!(admission.snapshot().counts.public, limits.public);
        assert!(admission.admit(Scope::Public).is_err());

        let mut polls = Vec::new();
        for _ in 0..limits.polls {
            let mut permit = admission.admit(Scope::Native).unwrap();
            permit.promote().unwrap();
            polls.push(permit);
        }
        let mut blocked = admission.admit(Scope::Native).unwrap();
        assert_eq!(blocked.promote().unwrap_err().class, "poll");
        let mut shorts = vec![blocked];
        while shorts.len() < limits.native - limits.polls {
            shorts.push(admission.admit(Scope::Native).unwrap());
        }
        assert_eq!(admission.admit(Scope::Native).unwrap_err().class, "connection");
        let snapshot = admission.snapshot();
        assert!(snapshot.rejected_public >= 2 && snapshot.rejected_native >= 2);
        drop((held, controls, polls, shorts));
        assert_eq!(admission.snapshot().counts, Counts::default());
    }

    #[test]
    fn shrinking_budget_only_gates_new_admission() {
        let admission = Admission::with_config(config(64, 0));
        let held: Vec<Permit> = (0..admission.limits().public).map(|_| admission.admit(Scope::Public).unwrap()).collect();
        assert_eq!(held.len(), 60);
        admission.set_native_profiles(100);
        // 이미 받은 60개 연결은 그대로 살아 있고, 새 연결만 줄어든 한도로 거절된다.
        assert_eq!(admission.snapshot().counts.public, 60);
        assert!(admission.admit(Scope::Public).is_err());
        // 하드 합계는 예산이 커진 native 쪽에도 지켜진다.
        let native: Vec<Permit> = std::iter::from_fn(|| admission.admit(Scope::Native).ok()).collect();
        assert_eq!(native.len(), 4);
        drop(held);
        let more: Vec<Permit> = std::iter::from_fn(|| admission.admit(Scope::Native).ok()).collect();
        assert_eq!(native.len() + more.len(), 40);
    }

    #[test]
    fn permit_is_returned_on_unwind_and_early_return() {
        let admission = Admission::new();
        let worker = Arc::clone(&admission);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let mut permit = worker.admit(Scope::Public).unwrap();
            permit.promote().unwrap();
            let _held = permit;
            panic!("handler failed");
        }));
        assert!(result.is_err());
        assert_eq!(admission.snapshot().counts, Counts::default());
        fn early(admission: &Arc<Admission>) -> Result<(), ()> {
            let _permit = admission.admit(Scope::Native).map_err(|_| ())?;
            Err(())
        }
        assert!(early(&admission).is_err());
        assert_eq!(admission.snapshot().counts, Counts::default());
    }

    /// 모델 몫을 전부 채워 두고 다음 요청들이 허가를 받아 오는 장면을 만든다.
    fn fill_models(admission: &Arc<Admission>) -> Vec<Permit> {
        (0..admission.limits().models)
            .map(|_| {
                let mut permit = admission.admit(Scope::Public).unwrap();
                permit.promote().unwrap();
                permit
            })
            .collect()
    }

    fn eventually(what: &str, condition: impl Fn() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn authenticated_waiters_take_freed_model_slots_in_arrival_order() {
        let admission = Admission::with_config(config(64, 5_000));
        let mut held = fill_models(&admission);
        let order = Arc::new(Mutex::new(Vec::new()));
        let mut workers = Vec::new();
        for index in 0..4 {
            let (shared, order) = (Arc::clone(&admission), Arc::clone(&order));
            workers.push(std::thread::spawn(move || {
                let mut permit = shared.admit(Scope::Public).unwrap();
                assert_eq!(permit.promote_wait(&|| false), Ok(true));
                order.lock().push(index);
                // 몫을 놓으면 다음 대기자가 이어서 받는다.
                std::thread::sleep(Duration::from_millis(20));
                drop(permit);
            }));
            // 대기열에 들어간 순서를 고정한다.
            eventually("waiter queued", || admission.snapshot().counts.waiting == index + 1);
        }
        assert_eq!(admission.snapshot().counts.waiting, 4);
        held.pop();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(*order.lock(), vec![0, 1, 2, 3]);
        let snapshot = admission.snapshot();
        assert_eq!((snapshot.counts.waiting, snapshot.waited, snapshot.wait_timeouts), (0, 4, 0));
        drop(held);
        assert_eq!(admission.snapshot().counts, Counts::default());
    }

    #[test]
    fn a_full_queue_is_refused_at_once_and_a_wait_that_never_clears_times_out() {
        let admission = Admission::with_config(config(64, 1_500));
        let held = fill_models(&admission);
        let limits = admission.limits();
        let waiters: Vec<_> = (0..limits.waiting)
            .map(|_| {
                let admission = Arc::clone(&admission);
                std::thread::spawn(move || {
                    let mut permit = admission.admit(Scope::Public).unwrap();
                    permit.promote_wait(&|| false)
                })
            })
            .collect();
        eventually("queue full", || admission.snapshot().counts.waiting == limits.waiting);
        let started = Instant::now();
        let mut overflow = admission.admit(Scope::Public).unwrap();
        match overflow.promote_wait(&|| false) {
            Err(Denied::Overload(over)) => {
                assert_eq!((over.class, over.limit, over.code()), ("queue", limits.waiting, "BRIDGE_BUSY"));
            }
            other => panic!("{other:?}"),
        }
        assert!(started.elapsed() < Duration::from_millis(250), "a full queue must not wait");
        for waiter in waiters {
            match waiter.join().unwrap() {
                Err(Denied::Overload(over)) => assert_eq!((over.class, over.code()), ("queue_timeout", "BRIDGE_QUEUE_TIMEOUT")),
                other => panic!("{other:?}"),
            }
        }
        let snapshot = admission.snapshot();
        assert_eq!((snapshot.counts.waiting, snapshot.wait_timeouts), (0, limits.waiting as u64));
        // 대기열이 비면 모델 몫이 돌아오는 대로 다시 받는다(과부하 뒤 회복).
        drop(held);
        let mut again = admission.admit(Scope::Public).unwrap();
        assert_eq!(again.promote_wait(&|| false), Ok(false));
    }

    #[test]
    fn a_cancelled_waiter_leaves_the_queue_and_the_next_one_proceeds() {
        let admission = Admission::with_config(config(64, 5_000));
        let mut held = fill_models(&admission);
        let gone = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let first = {
            let (admission, gone) = (Arc::clone(&admission), Arc::clone(&gone));
            std::thread::spawn(move || {
                let mut permit = admission.admit(Scope::Public).unwrap();
                permit.promote_wait(&|| gone.load(Ordering::SeqCst))
            })
        };
        eventually("first queued", || admission.snapshot().counts.waiting == 1);
        let second = {
            let admission = Arc::clone(&admission);
            std::thread::spawn(move || {
                let mut permit = admission.admit(Scope::Public).unwrap();
                let result = permit.promote_wait(&|| false);
                (result, permit)
            })
        };
        eventually("second queued", || admission.snapshot().counts.waiting == 2);
        gone.store(true, Ordering::SeqCst);
        assert_eq!(first.join().unwrap(), Err(Denied::Cancelled));
        assert_eq!(admission.snapshot().counts.waiting, 1);
        held.pop();
        let (result, permit) = second.join().unwrap();
        assert_eq!(result, Ok(true));
        drop((permit, held));
        assert_eq!(admission.snapshot().counts, Counts::default());
    }

    #[test]
    fn a_zero_wait_config_refuses_immediately_without_queueing() {
        let admission = Admission::with_config(config(64, 0));
        let held = fill_models(&admission);
        let mut permit = admission.admit(Scope::Public).unwrap();
        match permit.promote_wait(&|| false) {
            Err(Denied::Overload(over)) => assert_eq!(over.class, "model"),
            other => panic!("{other:?}"),
        }
        assert_eq!(admission.snapshot().counts.waiting, 0);
        drop((held, permit));
    }

    #[test]
    fn body_budget_is_weighted_shared_waits_for_a_return_and_shrinks_only_when_told() {
        let never = || false;
        let admission = Admission::with_config(Config { body_budget: 80 << 20, model_wait: Duration::from_millis(300), ..Config::default() });
        // 선언 길이 5 MiB는 가중치 8배인 40 MiB를 예약한다. 둘이면 예산이 찬다.
        let one = admission.reserve_body(5 << 20, &never).unwrap();
        let two = admission.reserve_body(5 << 20, &never).unwrap();
        assert_eq!(admission.snapshot().body_bytes, 80 << 20);
        let started = Instant::now();
        let refused = match admission.reserve_body(1, &never) {
            Err(Denied::Overload(over)) => over,
            other => panic!("{:?}", other.map(|_| ())),
        };
        assert!(started.elapsed() >= Duration::from_millis(250), "a full budget waits before refusing");
        assert_eq!((refused.class, refused.scope, refused.code()), ("payload", Scope::Public, "BRIDGE_BUSY"));
        assert_eq!(admission.snapshot().rejected_public, 1);
        // 예약이 돌아오면 기다리던 요청이 받는다.
        let waiter = {
            let admission = Arc::clone(&admission);
            std::thread::spawn(move || admission.reserve_body(5 << 20, &|| false))
        };
        std::thread::sleep(Duration::from_millis(50));
        drop(one);
        let three = waiter.join().unwrap().unwrap();
        assert_eq!(three.bytes, 40 << 20);
        // 요청 하나가 예산 전체보다 크면 기다리지 않고 거절한다.
        let started = Instant::now();
        assert!(matches!(admission.reserve_body(11 << 20, &never), Err(Denied::Overload(_))));
        assert!(started.elapsed() < Duration::from_millis(200));
        // 줄이는 것은 호출자가 원본·파싱 결과를 놓은 뒤에만 한다. 줄여도 커지지는 않는다.
        let mut two = two;
        two.shrink_to(1 << 20);
        assert_eq!(admission.snapshot().body_bytes, (1 << 20) + (40 << 20));
        two.shrink_to(usize::MAX);
        assert_eq!(admission.snapshot().body_bytes, (1 << 20) + (40 << 20));
        drop(two);
        assert_eq!(admission.snapshot().body_bytes, 40 << 20);
        drop(three);
        assert_eq!(admission.snapshot().body_bytes, 0);
    }

    #[test]
    fn a_waiting_body_reservation_is_cancelled_when_the_client_leaves() {
        let admission = Admission::with_config(Config { body_budget: 512 << 20, model_wait: Duration::from_secs(5), ..Config::default() });
        let held = admission.reserve_body(64 << 20, &|| false).unwrap();
        let started = Instant::now();
        assert!(matches!(admission.reserve_body(1, &|| true), Err(Denied::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(held);
        assert_eq!(admission.snapshot().body_bytes, 0);
    }

    mod pressure {
        use super::*;
        use crate::runtime_health::Sample;

        const MIB: u64 = 1024 * 1024;
        const GIB: u64 = 1024 * MIB;

        fn sample(available: u64) -> Sample {
            Sample { commit_limit: 16 * GIB, commit_available: available, process_private: None, process_working_set: None }
        }
        fn observe(guard: &ResourceGuard, available: u64) {
            guard.observe(sample(available), monotonic_ms());
        }
        /// 해제 기준(진입의 2배) 이상을 연속으로 관측해 정상으로 돌린다.
        fn recover(guard: &ResourceGuard) {
            for _ in 0..3 {
                observe(guard, 4 * GIB);
            }
        }
        fn setup(wait_ms: u64) -> (Arc<Admission>, Arc<ResourceGuard>) {
            let guard = ResourceGuard::with_support(true);
            (Admission::with_guard(config(64, wait_ms), Arc::clone(&guard)), guard)
        }
        fn resource(denied: Result<bool, Denied>) -> Overload {
            match denied {
                Err(Denied::Overload(over)) => over,
                other => panic!("{other:?}"),
            }
        }

        #[test]
        fn pressure_refuses_new_model_work_keeps_held_work_and_recovers_with_hysteresis() {
            let (admission, guard) = setup(0);
            let never = || false;
            // 압박 전에 이미 모델 몫과 본문 예약을 쥔 스트림.
            let mut active = admission.admit(Scope::Public).unwrap();
            assert_eq!(active.promote_wait(&never), Ok(false));
            let reservation = admission.reserve_body(1 << 20, &never).unwrap();
            // 표본이 없는 동안(unknown)에는 막지 않는다.
            assert_eq!(admission.resource_status().state, "unknown");
            let mut before = admission.admit(Scope::Public).unwrap();
            assert_eq!(before.promote_wait(&never), Ok(false));

            observe(&guard, 100 * MIB);
            let mut fresh = admission.admit(Scope::Public).unwrap();
            let refused = resource(fresh.promote_wait(&never));
            assert_eq!((refused.scope, refused.class, refused.code()), (Scope::Public, "resource", "BRIDGE_RESOURCE_PRESSURE"));
            assert_eq!((refused.active, refused.limit), (100, 512), "MiB");
            assert!(refused.message().contains("진행 중인 요청은 계속"));
            assert_ne!(refused.code(), "BRIDGE_BUSY");
            assert_ne!(refused.code(), "BRIDGE_QUEUE_TIMEOUT");
            assert!(matches!(admission.reserve_body(1, &never), Err(Denied::Overload(over)) if over.class == "resource"));
            // 이미 쥔 몫·본문 예약·연결은 그대로다. 새 연결(짧은 제어·healthz)과 native도 받는다.
            let counts = admission.snapshot().counts;
            assert_eq!((counts.models, counts.public), (2, 3));
            assert_eq!(admission.snapshot().body_bytes, (1 << 20) * BODY_COST_FACTOR);
            let control = admission.admit(Scope::Public).unwrap();
            admission.set_native_profiles(1);
            let mut poll = admission.admit(Scope::Native).unwrap();
            assert!(poll.promote().is_ok());
            // 이미 몫을 쥔 permit이 다시 승격을 부르는 것은 그대로 성공한다.
            assert_eq!(active.promote_wait(&never), Ok(false));
            let status = admission.resource_status();
            assert_eq!((status.state, status.model_rejections), ("pressure", 2));
            assert_eq!(admission.snapshot().rejected_public, 2);

            // 진입과 해제 사이에서는 압박이 유지되고, 해제 기준 이상이 연속 세 번이어야 정상이 된다.
            observe(&guard, 800 * MIB);
            assert!(matches!(fresh.promote_wait(&never), Err(Denied::Overload(_))));
            observe(&guard, 4 * GIB);
            observe(&guard, 4 * GIB);
            assert!(matches!(fresh.promote_wait(&never), Err(Denied::Overload(_))), "two healthy samples are not enough");
            observe(&guard, 4 * GIB);
            assert_eq!(fresh.promote_wait(&never), Ok(false));
            let reservation_two = admission.reserve_body(1 << 20, &never).unwrap();
            assert_eq!(admission.resource_status().state, "normal");
            assert_eq!(admission.resource_status().transitions, 1, "the first sample is not a transition");
            // 회복 뒤에도 처음부터 쥐고 있던 것은 그대로였다.
            assert_eq!(admission.snapshot().counts.models, 3);
            drop((active, before, fresh, control, poll, reservation, reservation_two));
            let snapshot = admission.snapshot();
            assert_eq!((snapshot.counts, snapshot.body_bytes), (Counts::default(), 0));
        }

        #[test]
        fn a_queued_waiter_is_refused_when_pressure_starts_and_held_permits_stay() {
            let (admission, guard) = setup(5_000);
            let mut held = fill_models(&admission);
            let waiter = {
                let admission = Arc::clone(&admission);
                std::thread::spawn(move || {
                    let mut permit = admission.admit(Scope::Public).unwrap();
                    let started = Instant::now();
                    (permit.promote_wait(&|| false), started.elapsed())
                })
            };
            eventually("waiter queued", || admission.snapshot().counts.waiting == 1);
            let models = held.len();
            observe(&guard, 100 * MIB);
            let (result, waited) = waiter.join().unwrap();
            assert_eq!(resource(result).class, "resource");
            assert!(waited < Duration::from_secs(2), "{waited:?}");
            let snapshot = admission.snapshot();
            assert_eq!((snapshot.counts.waiting, snapshot.counts.models, snapshot.wait_timeouts), (0, models, 0));
            // 압박이 풀리고 몫이 돌아오면 다시 받는다.
            recover(&guard);
            held.pop();
            let mut again = admission.admit(Scope::Public).unwrap();
            assert_eq!(again.promote_wait(&|| false), Ok(false));
            drop((held, again));
            assert_eq!(admission.snapshot().counts, Counts::default());
        }

        #[test]
        fn a_waiting_body_reservation_is_refused_before_any_body_is_read_when_pressure_starts() {
            let guard = ResourceGuard::with_support(true);
            let admission = Admission::with_guard(Config { body_budget: 512 << 20, model_wait: Duration::from_secs(5), ..Config::default() }, Arc::clone(&guard));
            let held = admission.reserve_body(64 << 20, &|| false).unwrap();
            let waiter = {
                let admission = Arc::clone(&admission);
                std::thread::spawn(move || admission.reserve_body(1, &|| false).map(|_| ()))
            };
            std::thread::sleep(Duration::from_millis(150));
            observe(&guard, 100 * MIB);
            match waiter.join().unwrap() {
                Err(Denied::Overload(over)) => assert_eq!((over.class, over.code()), ("resource", "BRIDGE_RESOURCE_PRESSURE")),
                other => panic!("{other:?}"),
            }
            assert_eq!(admission.snapshot().body_bytes, 512 << 20, "the running request keeps its reservation");
            drop(held);
            assert_eq!(admission.snapshot().body_bytes, 0);
        }
    }
}
