//! 서비스 런타임 진단과 메모리 압박 보호.
//!
//! - 진단 로그 `logs/service-health.log`는 소유자 전용 파일이며 크기를 제한한다(`LOG_MAX_BYTES`, 회전 한 번이라 최대 두 배).
//!   줄에는 서비스 버전·PID·**내부에서 정한 이벤트 식별자**·숫자만 들어간다. 오류 원문·패닉 메시지(payload)·경로·토큰·이메일·프롬프트는
//!   어떤 경로로도 쓰지 않는다. 패닉은 소스 파일 이름(컴파일 시점 상수)과 줄 번호만 남긴다.
//! - Windows에서만 동작한다. 다른 OS에서는 `start`·`record`가 아무것도 하지 않고 압박 보호는 항상 `unknown`(= 새 요청을 막지 않음)이다.
//! - 메모리 표본은 전용 스레드가 `SAMPLE_INTERVAL`마다 한 번 읽는다(시스템 commit 여유, 이 프로세스의 private bytes·working set).
//!   요청 경로는 표본을 읽지 않고 원자값 몇 개만 본다. 할당도 syscall도 없다.
//! - 압박 판정은 히스테리시스를 둔다. commit 여유가 `enter_below` 아래로 내려가면 한 번에 `pressure`, `exit_at`(= 진입 기준의 2배) 이상이
//!   `EXIT_SAMPLES`번 연속 관측돼야 `normal`로 돌아온다. 표본이 없거나(`never`) 오래됐으면(`stale`) 상태는 `unknown`이고
//!   `unknown`은 건강하다고도 압박이라고도 주장하지 않는다 — 새 요청은 막지 않고 상태 보고에서 그대로 `unknown`으로 드러난다.
//! - 이 모듈은 어떤 요청도 끊지 않는다. 판정은 새 모델 요청의 admission(`bridge_admission.rs`)만 소비한다.
use aam_protocol::{ApiError, Paths};
use parking_lot::Mutex;
use serde::Serialize;
use std::{
    fmt::Write as _,
    fs,
    io::{self, Write as _},
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, LazyLock,
    },
    time::{Duration, Instant},
};

const LOG_FILE: &str = "logs/service-health.log";
/// 실행 중에만 존재하는 표식. 새로 시작할 때 남아 있으면 이전 실행이 정상 종료 기록을 남기지 못한 것이다.
const MARKER_FILE: &str = "logs/service-run.marker";
/// 로그 한 파일의 상한. 넘으면 `.log.1`로 한 번 회전하므로 디스크 사용은 최대 두 배다.
const LOG_MAX_BYTES: u64 = 256 * 1024;
const MAX_EVENT_LEN: usize = 48;
/// 패닉 폭주가 로그를 채우지 않게 기록하는 패닉 수의 상한.
const MAX_PANIC_LINES: u64 = 32;

/// 표본 간격. 낮은 빈도로 유지한다(호출 두 번, 마이크로초 단위).
const SAMPLE_INTERVAL: Duration = Duration::from_secs(10);
/// 마지막 표본이 이보다 오래되면 `unknown`이다(표본 스레드 정지·연속 실패).
const STALE_AFTER_MS: u64 = 45_000;
/// 평상시 메모리 줄 간격과 압박 중 간격.
const MEMORY_LOG_NORMAL_MS: u64 = 300_000;
const MEMORY_LOG_PRESSURE_MS: u64 = 30_000;
/// commit 여유가 이 값(또는 한도의 `ENTER_PERCENT`%) 아래면 압박 진입. 둘 중 큰 값.
const ENTER_FLOOR_BYTES: u64 = 512 * 1024 * 1024;
const ENTER_PERCENT: u64 = 2;
/// 압박 해제 기준은 진입 기준의 이 배수.
const EXIT_FACTOR: u64 = 2;
/// 해제 기준 이상이 연속으로 이만큼 관측돼야 정상으로 돌아온다.
const EXIT_SAMPLES: u8 = 3;
/// 표본이 없는 칸.
const ABSENT: u64 = u64::MAX;

// ───────────────────────────── 진단 로그 ─────────────────────────────

/// 내부 이벤트 이름의 줄 구분자·길이를 제한한다. 비밀 값의 마스킹은 아니므로 사용자 입력을 넘기면 안 된다.
fn identifier(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .take(MAX_EVENT_LEN)
        .map(|c| match c.to_ascii_lowercase() {
            c @ ('a'..='z' | '0'..='9' | '_' | '.' | '-') => c,
            _ => '_',
        })
        .collect();
    if out.is_empty() {
        out.push_str("unknown");
    }
    out
}

/// `<ms> service/health v=<버전> pid=<pid> event=<식별자> <이름>=<숫자|->...`. 필드 이름은 이 파일 안의 상수만 쓴다.
fn format_line(at_ms: i64, event: &str, fields: &[(&str, Option<u64>)]) -> String {
    let mut line = format!("{at_ms} service/health v={} pid={} event={}", env!("CARGO_PKG_VERSION"), std::process::id(), identifier(event));
    for (name, value) in fields {
        let _ = match value {
            Some(value) => write!(line, " {name}={value}"),
            None => write!(line, " {name}=-"),
        };
    }
    line
}

static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[cfg(windows)]
fn open_private_append(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    // Append-only data access plus rights required to verify and restrict the opened object.
    let file = fs::OpenOptions::new().create(true).append(true).read(true)
        .access_mode(0x8000_0000 | 0x0004 | 0x0002_0000 | 0x0004_0000 | 0x0008_0000)
        .custom_flags(0x0020_0000).open(path)?;
    if !aam_protocol::winutil::single_link_regular_file(&file)?
        || !aam_protocol::secure::file_owned_by_me(&file)?
    {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "unsafe log file"));
    }
    aam_protocol::winutil::lock_down_handle(&file)?;
    Ok(file)
}

#[cfg(not(windows))]
fn open_private_append(path: &Path) -> io::Result<fs::File> {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink() || !meta.is_file()) {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "unsafe log path"));
    }
    // 새로 만들 때만 소유자 전용으로 고정한다. 쓸 때마다 권한 syscall을 하지 않는다.
    match aam_protocol::secure::private_options(fs::OpenOptions::new().create_new(true).append(true)).open(path) {
        Ok(file) => {
            aam_protocol::secure::restrict_file(path)?;
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => aam_protocol::secure::private_options(fs::OpenOptions::new().append(true)).open(path),
        Err(error) => Err(error),
    }
}

/// 한 줄을 덧붙인다. 상한을 넘으면 먼저 회전한다. 쓰기는 직렬화해 동시 호출의 줄이 섞이지 않게 한다.
fn append_line(path: &Path, line: &str, max_bytes: u64) -> io::Result<()> {
    let _serialized = WRITE_LOCK.lock();
    if let Some(parent) = path.parent() {
        aam_protocol::secure::restrict_dir(parent)?;
    }
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink() && meta.len() >= max_bytes) {
        fs::rename(path, path.with_extension("log.1"))?;
    }
    let mut file = open_private_append(path)?;
    // 줄과 줄바꿈을 한 번에 쓴다.
    let mut bytes = Vec::with_capacity(line.len() + 1);
    bytes.extend_from_slice(line.as_bytes());
    bytes.push(b'\n');
    file.write_all(&bytes)
}

fn write_event(paths: &Paths, event: &str, fields: &[(&str, Option<u64>)]) -> io::Result<()> {
    append_line(&paths.home.join(LOG_FILE), &format_line(aam_protocol::now_ms(), event, fields), LOG_MAX_BYTES)
}

/// 믿을 수 있는 내부 이벤트 식별자와 숫자 하나를 기록한다. 실패해도 호출자에게 영향이 없다(best effort).
/// 오류 원문·사용자 입력·경로를 `event`에 넣지 않는다.
/// Windows 밖에서는 아무것도 하지 않는다.
pub fn record(paths: &Paths, event: &str, value: Option<u64>) {
    if cfg!(windows) {
        let _ = write_event(paths, event, &[("value", value)]);
    }
}

/// 정상 종료 표시. 실행 표식을 지우고 `stop_clean`을 남긴다.
pub fn finish_clean(paths: &Paths) {
    if cfg!(windows) {
        let _ = write_event(paths, "stop_clean", &[]);
        let _ = fs::remove_file(paths.home.join(MARKER_FILE));
    }
}

/// 오류로 종료 표시. 오류 **코드**(예: `SERVICE_IO_ERROR`)만 식별자로 남기고 메시지는 쓰지 않는다. 실행 표식을 지운다.
pub fn finish_error(paths: &Paths, error: &ApiError) {
    if cfg!(windows) {
        let _ = write_event(paths, &format!("stop_error.{}", error.code), &[]);
        let _ = fs::remove_file(paths.home.join(MARKER_FILE));
    }
}

/// 시작 표시. 남아 있는 실행 표식이 있으면 이전 실행이 닫히지 않았다는 기록을 먼저 남긴다(강제 종료·크래시·전원 차단 등 원인 불문).
fn begin(paths: &Paths) -> io::Result<()> {
    let marker = paths.home.join(MARKER_FILE);
    if let Some(parent) = marker.parent() {
        aam_protocol::secure::restrict_dir(parent)?;
    }
    #[cfg(windows)]
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = aam_protocol::secure::open_owned_file(&marker)?;
        if file.metadata()?.len() > 0 {
            let mut text = String::new();
            (&mut file).take(64).read_to_string(&mut text)?;
            write_event(paths, "previous_run_open", &[("value", text.trim().parse().ok())])?;
        }
        file.seek(SeekFrom::Start(0))?;
        file.set_len(0)?;
        write!(file, "{}", std::process::id())?;
    }
    #[cfg(not(windows))]
    {
        if marker.exists() {
            write_event(paths, "previous_run_open", &[("value", fs::read_to_string(&marker).ok().and_then(|text| text.trim().parse().ok()))])?;
        }
        let mut file = aam_protocol::secure::private_options(fs::OpenOptions::new().create(true).write(true).truncate(true)).open(&marker)?;
        file.write_all(std::process::id().to_string().as_bytes())?;
    }
    write_event(paths, "start", &[])
}

/// 이 프로세스의 패닉을 소스 파일 이름과 줄 번호만으로 기록한다. payload(메시지)는 읽지 않는다.
fn install_panic_hook(paths: &Paths) {
    static COUNT: AtomicU64 = AtomicU64::new(0);
    let paths = paths.clone();
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if COUNT.fetch_add(1, Ordering::Relaxed) < MAX_PANIC_LINES {
            let (file, line) = info
                .location()
                .map(|location| (location.file().rsplit(['/', '\\']).next().unwrap_or("unknown"), Some(location.line() as u64)))
                .unwrap_or(("unknown", None));
            let _ = write_event(&paths, &format!("panic.{file}"), &[("value", line)]);
        }
        previous(info);
    }));
}

// ───────────────────────────── 메모리 표본과 압박 판정 ─────────────────────────────

/// 프로세스 시작 이후의 단조 시간(ms). 표본 신선도 판단에만 쓴다.
pub(crate) fn monotonic_ms() -> u64 {
    static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);
    EPOCH.elapsed().as_millis() as u64
}

/// 한 번의 측정값(바이트). 프로세스 값은 읽지 못하면 `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sample {
    /// 시스템 commit 한도와 그 여유(한도 − 현재 commit).
    pub commit_limit: u64,
    pub commit_available: u64,
    pub process_private: Option<u64>,
    pub process_working_set: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Unknown,
    Normal,
    Pressure,
}
impl Level {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Level::Normal,
            2 => Level::Pressure,
            _ => Level::Unknown,
        }
    }
    fn as_u8(self) -> u8 {
        match self {
            Level::Unknown => 0,
            Level::Normal => 1,
            Level::Pressure => 2,
        }
    }
}

/// 상태가 바뀐 표본.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Change {
    pub from: Level,
    pub to: Level,
}

/// 압박 진입 기준(바이트): 한도의 `ENTER_PERCENT`%와 `ENTER_FLOOR_BYTES` 중 큰 값.
pub(crate) fn enter_below(commit_limit: u64) -> u64 {
    ENTER_FLOOR_BYTES.max(commit_limit / 100 * ENTER_PERCENT)
}

/// 압박으로 거절한 요청의 사실. 값은 바이트다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pressure {
    pub available: u64,
    pub enter_below: u64,
}

struct Machine {
    level: Level,
    /// 압박 중 해제 기준 이상으로 연속 관측된 횟수.
    healthy: u8,
}

/// 새 모델 요청 admission이 소비하는 자원 상태. 표본 스레드만 쓰고 요청 경로는 원자값만 읽는다.
pub(crate) struct ResourceGuard {
    supported: bool,
    machine: Mutex<Machine>,
    level: AtomicU8,
    /// 마지막 표본 시각 + 1(0은 아직 없음).
    sampled_at: AtomicU64,
    available: AtomicU64,
    limit: AtomicU64,
    enter: AtomicU64,
    private: AtomicU64,
    working_set: AtomicU64,
    failures: AtomicU64,
    transitions: AtomicU64,
    rejections: AtomicU64,
}

impl ResourceGuard {
    pub(crate) fn new() -> Arc<Self> {
        Self::with_support(cfg!(windows))
    }

    /// `supported`는 이 플랫폼이 표본을 읽을 수 있는지다. 상태 보고의 `unsupported`를 가를 뿐 판정은 바꾸지 않는다.
    pub(crate) fn with_support(supported: bool) -> Arc<Self> {
        Arc::new(Self {
            supported,
            machine: Mutex::new(Machine { level: Level::Unknown, healthy: 0 }),
            level: AtomicU8::new(Level::Unknown.as_u8()),
            sampled_at: AtomicU64::new(0),
            available: AtomicU64::new(ABSENT),
            limit: AtomicU64::new(ABSENT),
            enter: AtomicU64::new(ABSENT),
            private: AtomicU64::new(ABSENT),
            working_set: AtomicU64::new(ABSENT),
            failures: AtomicU64::new(0),
            transitions: AtomicU64::new(0),
            rejections: AtomicU64::new(0),
        })
    }

    /// 표본 하나를 반영한다. 상태가 바뀌었으면 바뀐 내용을 돌려준다.
    pub(crate) fn observe(&self, sample: Sample, now: u64) -> Option<Change> {
        let enter = enter_below(sample.commit_limit);
        let exit = enter.saturating_mul(EXIT_FACTOR);
        let mut machine = self.machine.lock();
        if !self.fresh(now) {
            machine.healthy = 0;
        }
        let from = machine.level;
        let to = match from {
            Level::Unknown | Level::Normal => {
                machine.healthy = 0;
                if sample.commit_available < enter { Level::Pressure } else { Level::Normal }
            }
            Level::Pressure => {
                if sample.commit_available >= exit {
                    machine.healthy = machine.healthy.saturating_add(1);
                } else {
                    machine.healthy = 0;
                }
                if machine.healthy >= EXIT_SAMPLES {
                    machine.healthy = 0;
                    Level::Normal
                } else {
                    Level::Pressure
                }
            }
        };
        machine.level = to;
        self.available.store(sample.commit_available, Ordering::Relaxed);
        self.limit.store(sample.commit_limit, Ordering::Relaxed);
        self.enter.store(enter, Ordering::Relaxed);
        self.private.store(sample.process_private.unwrap_or(ABSENT), Ordering::Relaxed);
        self.working_set.store(sample.process_working_set.unwrap_or(ABSENT), Ordering::Relaxed);
        self.level.store(to.as_u8(), Ordering::Release);
        self.sampled_at.store(now.saturating_add(1), Ordering::Release);
        drop(machine);
        if from == to {
            return None;
        }
        if from != Level::Unknown {
            self.transitions.fetch_add(1, Ordering::Relaxed);
        }
        Some(Change { from, to })
    }

    /// 표본을 읽지 못했다. 상태는 바꾸지 않는다. 실패가 길어지면 표본이 오래돼 `unknown`이 된다.
    pub(crate) fn observe_failure(&self) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        self.machine.lock().healthy = 0;
    }

    fn fresh(&self, now: u64) -> bool {
        let sampled = self.sampled_at.load(Ordering::Acquire);
        sampled != 0 && now.saturating_sub(sampled - 1) <= STALE_AFTER_MS
    }

    /// 새 모델 요청을 받아도 되는지. 신선한 표본이 압박을 가리킬 때만 거절한다. `unknown`은 거절하지 않는다.
    /// 원자값 읽기뿐이며 할당·잠금·syscall이 없다(거절 횟수만 센다).
    pub(crate) fn check(&self, now: u64) -> Result<(), Pressure> {
        if Level::from_u8(self.level.load(Ordering::Acquire)) != Level::Pressure || !self.fresh(now) {
            return Ok(());
        }
        self.rejections.fetch_add(1, Ordering::Relaxed);
        Err(Pressure { available: self.available.load(Ordering::Relaxed), enter_below: self.enter.load(Ordering::Relaxed) })
    }

    pub(crate) fn level(&self, now: u64) -> Level {
        if self.fresh(now) { Level::from_u8(self.level.load(Ordering::Acquire)) } else { Level::Unknown }
    }

    pub(crate) fn status(&self, now: u64) -> ResourceStatus {
        let sampled = self.sampled_at.load(Ordering::Acquire);
        let fresh = self.fresh(now);
        let (sample, age) = match (sampled, fresh) {
            (0, _) if !self.supported => ("unsupported", None),
            (0, _) => ("never", None),
            (_, true) => ("fresh", Some(now.saturating_sub(sampled - 1))),
            (_, false) => ("stale", Some(now.saturating_sub(sampled - 1))),
        };
        // 신선하지 않은 표본의 숫자는 현재 값처럼 보이지 않게 비운다.
        let number = |cell: &AtomicU64| Some(cell.load(Ordering::Relaxed)).filter(|value| fresh && *value != ABSENT);
        let state = match self.level(now) {
            Level::Unknown => "unknown",
            Level::Normal => "normal",
            Level::Pressure => "pressure",
        };
        let enter = number(&self.enter);
        ResourceStatus {
            platform_supported: self.supported,
            state,
            sample,
            sample_age_ms: age,
            sample_failures: self.failures.load(Ordering::Relaxed),
            commit_available_bytes: number(&self.available),
            commit_limit_bytes: number(&self.limit),
            enter_below_bytes: enter,
            exit_at_bytes: enter.map(|value| value.saturating_mul(EXIT_FACTOR)),
            process_private_bytes: number(&self.private),
            process_working_set_bytes: number(&self.working_set),
            transitions: self.transitions.load(Ordering::Relaxed),
            model_rejections: self.rejections.load(Ordering::Relaxed),
        }
    }
}

/// `bridge.status`의 `resources`. 숫자는 신선한 표본일 때만 채운다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceStatus {
    /// 이 OS가 표본을 읽을 수 있는지(Windows만 참).
    pub platform_supported: bool,
    /// `unknown`(표본이 없거나 오래됨·미지원: 새 요청을 막지 않는다) / `normal` / `pressure`(새 모델 요청만 거절 중).
    pub state: &'static str,
    /// `unsupported` / `never`(아직 표본 없음) / `fresh` / `stale`(마지막 표본이 오래됨).
    pub sample: &'static str,
    pub sample_age_ms: Option<u64>,
    /// 표본을 읽지 못한 누적 횟수.
    pub sample_failures: u64,
    /// 시스템 commit 여유·한도와 이 서비스 프로세스의 private bytes·working set(바이트).
    pub commit_available_bytes: Option<u64>,
    pub commit_limit_bytes: Option<u64>,
    /// 압박 진입 기준(여유가 이 값 아래)과 해제 기준(이 값 이상이 연속 관측).
    pub enter_below_bytes: Option<u64>,
    pub exit_at_bytes: Option<u64>,
    pub process_private_bytes: Option<u64>,
    pub process_working_set_bytes: Option<u64>,
    /// 정상↔압박 전환 누적 수와 압박 때문에 거절한 새 모델 요청 누적 수.
    pub transitions: u64,
    pub model_rejections: u64,
}

/// 운영에서 모든 admission이 공유하는 상태.
pub(crate) fn shared_guard() -> Arc<ResourceGuard> {
    static GUARD: LazyLock<Arc<ResourceGuard>> = LazyLock::new(ResourceGuard::new);
    Arc::clone(&GUARD)
}

#[cfg(windows)]
fn sample_native() -> Option<Sample> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::System::{
        ProcessStatus::{GetPerformanceInfo, GetProcessMemoryInfo, PERFORMANCE_INFORMATION, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX},
        Threading::GetCurrentProcess,
    };
    // SAFETY: 두 구조체는 정수만 가진 `repr(C)`이라 0 초기화가 유효하고, `cb`에 실제 크기를 넣어 넘긴다.
    unsafe {
        let mut info: PERFORMANCE_INFORMATION = zeroed();
        let info_size = size_of::<PERFORMANCE_INFORMATION>() as u32;
        info.cb = info_size;
        if GetPerformanceInfo(&mut info, info_size) == 0 {
            return None;
        }
        let page = info.PageSize as u64;
        let commit_limit = (info.CommitLimit as u64).saturating_mul(page);
        let commit_total = (info.CommitTotal as u64).saturating_mul(page);
        if commit_limit == 0 || page == 0 {
            return None;
        }
        let mut counters: PROCESS_MEMORY_COUNTERS_EX = zeroed();
        let counters_size = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        counters.cb = counters_size;
        let process = GetProcessMemoryInfo(GetCurrentProcess(), (&mut counters as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(), counters_size) != 0;
        Some(Sample {
            commit_limit,
            commit_available: commit_limit.saturating_sub(commit_total),
            process_private: process.then_some(counters.PrivateUsage as u64),
            process_working_set: process.then_some(counters.WorkingSetSize as u64),
        })
    }
}

#[cfg(not(windows))]
fn sample_native() -> Option<Sample> {
    None
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Unknown => "unknown",
        Level::Normal => "normal",
        Level::Pressure => "pressure",
    }
}

/// 표본 스레드 본체. 한 번 읽고 반영하며, 평소에는 5분마다(압박 중에는 30초마다) 한 줄, 상태 전환 때는 그 즉시 한 줄을 남긴다.
fn sample_loop(paths: Paths, guard: Arc<ResourceGuard>) {
    let mut next_log = 0u64;
    let mut failing = false;
    loop {
        let now = monotonic_ms();
        match sample_native() {
            Some(sample) => {
                failing = false;
                let change = guard.observe(sample, now);
                let fields = [
                    ("commit_available", Some(sample.commit_available)),
                    ("commit_limit", Some(sample.commit_limit)),
                    ("enter_below", Some(enter_below(sample.commit_limit))),
                    ("private", sample.process_private),
                    ("working_set", sample.process_working_set),
                ];
                if let Some(change) = change {
                    let event = match (change.from, change.to) {
                        (_, Level::Pressure) => "pressure_enter",
                        (Level::Pressure, _) => "pressure_exit",
                        _ => "memory_ready",
                    };
                    let _ = write_event(&paths, event, &fields);
                    next_log = now + if change.to == Level::Pressure { MEMORY_LOG_PRESSURE_MS } else { MEMORY_LOG_NORMAL_MS };
                } else if now >= next_log {
                    let _ = write_event(&paths, &format!("memory.{}", level_name(guard.level(now))), &fields);
                    next_log = now + if guard.level(now) == Level::Pressure { MEMORY_LOG_PRESSURE_MS } else { MEMORY_LOG_NORMAL_MS };
                }
            }
            None => {
                guard.observe_failure();
                // 연속 실패는 처음 한 번만 기록한다.
                if !failing {
                    failing = true;
                    let _ = write_event(&paths, "sample_failed", &[]);
                }
            }
        }
        std::thread::sleep(SAMPLE_INTERVAL);
    }
}

/// 진단을 시작한다. `service_lifetime_lock`을 얻은 직후 한 번 부른다(다시 불러도 아무 일도 하지 않는다).
/// 시작·이전 실행 미종료 표식을 남기고, 패닉 표식 hook을 걸고, 메모리 표본 스레드를 띄운다.
/// 로그 경로가 안전하지 않으면(심볼릭 링크 등) 오류를 돌려주지만 진단은 부가 기능이라 서비스를 멈출 이유는 아니다.
/// Windows 밖에서는 아무것도 하지 않는다.
pub fn start(paths: &Paths) -> Result<(), ApiError> {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if !cfg!(windows) || STARTED.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    let log_result = begin(paths).map_err(|_| ApiError::new("UNSAFE_SERVICE_PATH", "서비스 진단 로그 경로를 준비할 수 없습니다. 소유권과 권한을 확인하세요."));
    if log_result.is_ok() {
        install_panic_hook(paths);
    }
    let (paths, guard) = (paths.clone(), shared_guard());
    std::thread::Builder::new().name("ojak-health".into()).spawn(move || sample_loop(paths, guard))
        .map_err(|_| ApiError::new("RESOURCE_MONITOR_START_FAILED", "서비스 메모리 관측을 시작하지 못했습니다."))?;
    log_result
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    fn sample(available: u64) -> Sample {
        Sample { commit_limit: 16 * GIB, commit_available: available, process_private: Some(200 * MIB), process_working_set: Some(150 * MIB) }
    }

    struct TempPaths(Paths);
    impl std::ops::Deref for TempPaths {
        type Target = Paths;
        fn deref(&self) -> &Paths { &self.0 }
    }
    impl Drop for TempPaths {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.home); }
    }
    fn temp_paths() -> TempPaths {
        let home = std::env::temp_dir().join(format!("ojak-health-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&home).unwrap();
        TempPaths(Paths { socket: home.join("service.sock"), database: home.join("state.db"), profiles: home.join("profiles"), home })
    }

    #[test]
    fn thresholds_use_the_larger_of_the_floor_and_the_percentage() {
        assert_eq!(enter_below(16 * GIB), 512 * MIB);
        assert_eq!(enter_below(100 * GIB), 2 * GIB);
        assert_eq!(enter_below(0), 512 * MIB);
    }

    #[test]
    fn pressure_needs_sustained_recovery_and_a_dip_resets_the_streak() {
        let guard = ResourceGuard::with_support(true);
        assert_eq!(guard.observe(sample(8 * GIB), 0), Some(Change { from: Level::Unknown, to: Level::Normal }));
        assert!(guard.check(0).is_ok());
        // 진입 기준(512 MiB) 바로 위는 압박이 아니다.
        assert_eq!(guard.observe(sample(512 * MIB), 10), None);
        assert_eq!(guard.observe(sample(512 * MIB - 1), 20), Some(Change { from: Level::Normal, to: Level::Pressure }));
        let refused = guard.check(21).unwrap_err();
        assert_eq!((refused.available, refused.enter_below), (512 * MIB - 1, 512 * MIB));
        // 진입과 해제 사이(512 MiB..1 GiB)는 압박을 유지한다. 해제 기준 이상이 세 번 연속이어야 정상이다.
        assert_eq!(guard.observe(sample(900 * MIB), 30), None);
        assert!(guard.check(31).is_err());
        assert_eq!(guard.observe(sample(GIB), 40), None);
        assert_eq!(guard.observe(sample(2 * GIB), 50), None);
        assert!(guard.check(51).is_err(), "two healthy samples are not enough");
        // 사이에 다시 내려가면 연속 횟수가 처음부터다.
        assert_eq!(guard.observe(sample(700 * MIB), 60), None);
        assert_eq!(guard.observe(sample(2 * GIB), 70), None);
        assert_eq!(guard.observe(sample(2 * GIB), 80), None);
        assert!(guard.check(81).is_err());
        assert_eq!(guard.observe(sample(2 * GIB), 90), Some(Change { from: Level::Pressure, to: Level::Normal }));
        assert!(guard.check(91).is_ok());
        let status = guard.status(91);
        assert_eq!((status.state, status.transitions, status.model_rejections), ("normal", 2, 4));
    }

    #[test]
    fn the_first_sample_can_start_in_pressure_and_is_not_counted_as_a_transition() {
        let guard = ResourceGuard::with_support(true);
        assert_eq!(guard.observe(sample(100 * MIB), 5), Some(Change { from: Level::Unknown, to: Level::Pressure }));
        assert!(guard.check(6).is_err());
        assert_eq!(guard.status(6).transitions, 0);
    }

    #[test]
    fn unknown_never_pretends_to_be_healthy_or_under_pressure() {
        let guard = ResourceGuard::with_support(true);
        let never = guard.status(1_000);
        assert_eq!((never.state, never.sample, never.sample_age_ms, never.commit_available_bytes), ("unknown", "never", None, None));
        assert!(guard.check(1_000).is_ok(), "no sample must not block new requests");
        let unsupported = ResourceGuard::with_support(false).status(0);
        assert_eq!((unsupported.state, unsupported.sample, unsupported.platform_supported), ("unknown", "unsupported", false));

        guard.observe(sample(100 * MIB), 1_000);
        assert_eq!(guard.status(1_001).state, "pressure");
        // 표본 스레드가 멈추면 압박이라고 우기지 않고 unknown으로 돌아선다. 숫자도 현재 값처럼 보이지 않게 비운다.
        let stale_at = 1_000 + STALE_AFTER_MS + 1;
        assert!(guard.check(stale_at).is_ok());
        let stale = guard.status(stale_at);
        assert_eq!((stale.state, stale.sample, stale.sample_age_ms), ("unknown", "stale", Some(STALE_AFTER_MS + 1)));
        assert_eq!((stale.commit_available_bytes, stale.commit_limit_bytes, stale.process_private_bytes), (None, None, None));
        // 새 표본이 오면 내부 판정(히스테리시스)이 이어진다.
        guard.observe(sample(2 * GIB), stale_at + 1);
        assert_eq!(guard.status(stale_at + 2).state, "pressure");
    }

    #[test]
    fn a_failed_read_counts_but_never_flips_the_state() {
        let guard = ResourceGuard::with_support(true);
        guard.observe(sample(8 * GIB), 0);
        guard.observe_failure();
        guard.observe_failure();
        let status = guard.status(1);
        assert_eq!((status.state, status.sample, status.sample_failures), ("normal", "fresh", 2));
    }

    #[test]
    fn failed_or_stale_samples_break_the_recovery_streak() {
        for gap in [false, true] {
            let guard = ResourceGuard::with_support(true);
            guard.observe(sample(100 * MIB), 0);
            guard.observe(sample(8 * GIB), 10);
            guard.observe(sample(8 * GIB), 20);
            let next = if gap { STALE_AFTER_MS + 21 } else { guard.observe_failure(); 30 };
            guard.observe(sample(8 * GIB), next);
            assert_eq!(guard.status(next).state, "pressure");
            guard.observe(sample(8 * GIB), next + 10);
            assert_eq!(guard.status(next + 10).state, "pressure");
            guard.observe(sample(8 * GIB), next + 20);
            assert_eq!(guard.status(next + 20).state, "normal");
        }
    }

    #[test]
    fn status_serializes_camel_case_with_nulls_for_missing_values() {
        let guard = ResourceGuard::with_support(true);
        let value = serde_json::to_value(guard.status(0)).unwrap();
        assert_eq!(value["state"], "unknown");
        assert_eq!(value["sample"], "never");
        assert!(value["commitAvailableBytes"].is_null() && value["sampleAgeMs"].is_null());
        assert_eq!(value["modelRejections"], 0);
        guard.observe(Sample { process_private: None, process_working_set: None, ..sample(8 * GIB) }, 0);
        let value = serde_json::to_value(guard.status(1)).unwrap();
        assert_eq!((value["state"].as_str(), value["commitAvailableBytes"].as_u64(), value["enterBelowBytes"].as_u64()), (Some("normal"), Some(8 * GIB), Some(512 * MIB)));
        assert_eq!(value["exitAtBytes"], GIB);
        assert!(value["processPrivateBytes"].is_null());
    }

    #[test]
    fn identifiers_are_reduced_to_a_safe_alphabet_and_length() {
        assert_eq!(identifier("Stop_Error.SERVICE_IO_ERROR"), "stop_error.service_io_error");
        let hostile = identifier("a b\nc=d \\\\x:\" 이메일@example.com");
        assert!(hostile.len() <= MAX_EVENT_LEN && hostile.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.-".contains(c)), "{hostile}");
        assert_eq!(identifier("").as_str(), "unknown");
        let line = format_line(1, "x\ny", &[("value", Some(7)), ("private", None)]);
        assert!(line.ends_with("event=x_y value=7 private=-") && !line.contains('\n'), "{line}");
    }

    #[test]
    fn the_log_is_private_bounded_and_rotates_once() {
        let paths = temp_paths();
        let path = paths.home.join(LOG_FILE);
        for index in 0..200 {
            append_line(&path, &format_line(index, "memory.normal", &[("private", Some(index as u64))]), 4096).unwrap();
        }
        let size = fs::metadata(&path).unwrap().len();
        assert!(size <= 4096 + 200, "{size}");
        assert!(path.with_extension("log.1").exists());
        assert!(fs::read_to_string(&path).unwrap().lines().all(|line| line.contains("service/health v=")));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        #[cfg(unix)]
        {
            let link = paths.home.join("logs/link.log");
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(append_line(&link, "x", 4096).is_err());
        }
    }

    #[test]
    fn a_marker_left_behind_reports_an_unclean_previous_run_and_a_clean_stop_removes_it() {
        let paths = temp_paths();
        begin(&paths).unwrap();
        let marker = paths.home.join(MARKER_FILE);
        assert_eq!(fs::read_to_string(&marker).unwrap(), std::process::id().to_string());
        let log = || fs::read_to_string(paths.home.join(LOG_FILE)).unwrap();
        assert!(log().contains("event=start") && !log().contains("previous_run_open"));
        // 정상 종료 기록 없이 다시 시작하면 이전 실행이 닫히지 않았다는 표식이 먼저 남는다.
        begin(&paths).unwrap();
        let text = log();
        let open = text.find("event=previous_run_open").expect("unclean marker");
        assert!(text.contains(&format!("event=previous_run_open value={}", std::process::id())) && text[open..].contains("event=start"));
        // 정상 종료는 표식을 지운다(Windows에서만 동작하는 finish_*와 같은 내용을 직접 확인한다).
        write_event(&paths, "stop_clean", &[]).unwrap();
        fs::remove_file(&marker).unwrap();
        begin(&paths).unwrap();
        assert_eq!(log().matches("previous_run_open").count(), 1);
    }

    #[cfg(windows)]
    #[test]
    fn diagnostic_files_never_modify_hard_link_targets() {
        for relative in [LOG_FILE, MARKER_FILE] {
            let paths = temp_paths();
            let target = paths.home.join("preserved.txt");
            fs::write(&target, "preserve this data").unwrap();
            let link = paths.home.join(relative);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            fs::hard_link(&target, &link).unwrap();
            let result = if relative == LOG_FILE { append_line(&link, "new event", LOG_MAX_BYTES) } else { begin(&paths) };
            assert!(result.is_err());
            assert_eq!(fs::read_to_string(&target).unwrap(), "preserve this data");
        }
    }

}
