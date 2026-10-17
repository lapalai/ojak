//! Windows 사용자별 독립 감독 프로세스.
//!
//! `aam-service.exe`를 인자 없이(또는 `--supervise`로) 실행하면 이 프로세스가 되고, 같은 실행 파일을 `--worker`로 띄워
//! 실제 서비스(RPC·SQLite·lease 소유자)를 자식으로 감독한다. `--foreground`는 감독 없이 서비스를 직접 돌린다(개발용).
//! 작업자가 충돌해도 사용자가 데스크톱 앱을 열 때까지 서비스가 멈춰 있지 않게 하는 것이 목적이다.
//!
//! 파일(모두 `AAM_HOME` 안, 현재 사용자 전용 ACL):
//! - `service-supervisor.lock` — 감독 프로세스가 수명 동안 쥐는 단일 인스턴스 잠금. OS가 프로세스 종료 때 회수하므로
//!   남은 PID 기록을 믿지 않고도 "감독 프로세스가 살아 있는가"를 정확히 답한다.
//! - `service-supervisor.gate` — 작업자 생성 관문. 감독 프로세스는 이 잠금을 쥔 동안에만 작업자를 만들고(곧바로 푼다),
//!   중지·재시작·업데이트 작업(`engage`)은 이 잠금을 작업이 끝날 때까지 쥔다. 관문은 **일시 정지**일 뿐이다. 잡혀 있는 동안
//!   감독 프로세스는 새 작업자를 만들지 않지만 이미 도는 작업자(자식 또는 넘겨받은 것)는 계속 지켜보고, 관문이 (확정 없이) 풀리면
//!   다시 작업자를 만든다. 작업이 거절되거나 비정상 종료해도 OS가 잠금을 풀어 감독이 이어진다.
//! - `service-supervisor.retire` — 감독 퇴역 확정. 작업이 작업자 종료를 확인한 뒤 `Fence::commit_retirement`가 쥔다.
//!   감독 프로세스는 이 잠금이 잡힌 것을 보면 작업자를 되살리지 않고 스스로 끝난다.
//! - `service-supervisor.child` — 감독 프로세스가 만든 자식 작업자가 살아 있는 동안 감독 프로세스가 쥔다. 관문을 쥔 채로만
//!   얻으므로 중지 작업은 "시작 중인 자식 없음"을 정확히 안다.
//! - `service-supervisor.process` — 감독 프로세스가 단일 인스턴스 잠금을 얻은 직후 적는 자기 PID와 생성 시각. 중지 작업은
//!   이 기록으로 프로세스 핸들을 열어 종료 신호를 확인한다(잠금이 풀린 것만으로는 실행 파일이 풀렸다고 보지 않는다).
//!
//! 안전 규칙:
//! - 감독 프로세스는 작업자를 시간 초과만으로 죽이지 않는다. 응답이 없어도 lease가 남아 있을 수 있어 준비 확인은 증거 기록뿐이다.
//! - 작업자의 종료 코드·가동 시간·재시작 예정만 `runtime_health::record`로 남긴다(식별자와 숫자뿐, 오류 원문·경로 없음).
//! - lease 안전 검사(`service.prepareUninstall` permit)와 종료는 여전히 중지하는 쪽(launcher)이 작업자 프로세스 객체에 대해 한다.
//!   감독 프로세스는 그 승인 이전에는 작업자를 건드리지 않는다.
use crate::runtime_health;
use aam_protocol::{secure, ApiError, Paths};
use fs2::FileExt;
use std::{
    ffi::OsString,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_INVALID_PARAMETER, FILETIME, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, OpenProcess, WaitForSingleObject,
        PROCESS_QUERY_LIMITED_INFORMATION,
    },
};

/// 감독 프로세스가 띄우는 작업자 모드.
pub const WORKER_ARG: &str = "--worker";
/// 감독 모드를 명시한다. Windows에서 인자가 없을 때와 같다.
pub const SUPERVISE_ARG: &str = "--supervise";
/// 이미 다른 작업자(또는 감독 프로세스)가 있어 시작하지 않았다는 종료 코드. 충돌이 아니므로 재시작 예산을 쓰지 않는다.
pub const PEER_EXISTS_EXIT: u32 = 73;
/// 종료 코드를 읽지 못했을 때의 값. 충돌로 취급한다.
const UNKNOWN_EXIT: u32 = u32::MAX;
const SUPERVISOR_LOCK: &str = "service-supervisor.lock";
const GATE_FILE: &str = "service-supervisor.gate";
const RETIRE_FILE: &str = "service-supervisor.retire";
const CHILD_LOCK: &str = "service-supervisor.child";
const PROCESS_FILE: &str = "service-supervisor.process";
/// `WaitForSingleObject`에 필요한 접근 권한(`SYNCHRONIZE`).
const SYNCHRONIZE: u32 = 0x0010_0000;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// 다른 중지 작업이 관문을 쥐고 있을 때 기다리는 최대 시간.
const GATE_WAIT: Duration = Duration::from_secs(10);
/// 퇴역을 확정한 작업이 도중에 끝나도 감독 프로세스가 끝난 것을 확인할 때까지 기다리는 최대 시간.
const DROP_EXIT_WAIT: Duration = Duration::from_secs(15);

fn unsafe_path(_: io::Error) -> ApiError {
    ApiError::new(
        "UNSAFE_SERVICE_PATH",
        "서비스 감독 파일의 종류 또는 소유권이 올바르지 않습니다.",
    )
}

/// 소유자 전용 ACL, 단일 링크 일반 파일, 현재 사용자 소유를 확인한 잠금 파일.
fn lock_file(paths: &Paths, name: &str) -> Result<File, ApiError> {
    secure::open_owned_file(&paths.home.join(name)).map_err(unsafe_path)
}

/// 다른 핸들이 잠금을 쥐고 있어 즉시 얻지 못한 경우.
fn contended(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

fn singleton(paths: &Paths) -> Result<File, ApiError> {
    let lock = lock_file(paths, SUPERVISOR_LOCK)?;
    // `running`과 `Fence`가 잠금을 아주 잠깐 쥐고 확인하는 순간과 겹쳐도 실제 중복으로 오해하지 않도록 짧게 몇 번 다시 시도한다.
    // 진짜 감독 프로세스는 수명 동안 쥐고 있으므로 이 시도에도 실패한다.
    for attempt in 0..5 {
        if lock.try_lock_exclusive().is_ok() {
            return Ok(lock);
        }
        if attempt < 4 {
            std::thread::sleep(Duration::from_millis(30));
        }
    }
    Err(ApiError::new(
        "SUPERVISOR_ALREADY_RUNNING",
        "이 사용자의 서비스 감독 프로세스가 이미 실행 중입니다.",
    ))
}

/// 이 사용자의 감독 프로세스가 살아 있는지. 잠금이 잡혀 있으면 true다.
pub fn running(paths: &Paths) -> bool {
    if !paths.home.join(SUPERVISOR_LOCK).exists() {
        return false;
    }
    let Ok(lock) = lock_file(paths, SUPERVISOR_LOCK) else {
        return false;
    };
    match lock.try_lock_exclusive() {
        Ok(()) => {
            let _ = lock.unlock();
            false
        }
        Err(_) => true,
    }
}

// ───────────────────────────── 프로세스 핸들 증거 ─────────────────────────────

fn creation_time(handle: HANDLE) -> io::Result<u64> {
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
}

/// 종료 신호를 기다릴 수 있게 연 프로세스 핸들.
#[derive(Debug)]
struct ProcessHandle(OwnedHandle);

impl ProcessHandle {
    /// 그런 PID의 프로세스가 없으면 `Ok(None)`. 그 밖의 실패는 확인하지 못한 것이다.
    fn open(pid: u32) -> io::Result<Option<Self>> {
        let handle = unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            let error = io::Error::last_os_error();
            return if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                Ok(None)
            } else {
                Err(error)
            };
        }
        Ok(Some(Self(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })))
    }

    fn created(&self) -> io::Result<u64> {
        creation_time(self.0.as_raw_handle() as HANDLE)
    }

    /// 프로세스 객체가 종료로 신호됐는가. 실행 파일 매핑까지 정리된 뒤에야 신호된다.
    fn signaled(&self) -> io::Result<bool> {
        match unsafe { WaitForSingleObject(self.0.as_raw_handle() as HANDLE, 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(io::Error::last_os_error()),
        }
    }
}

fn write_process_record(paths: &Paths, pid: u32, created: u64) -> Result<(), ApiError> {
    let mut file = lock_file(paths, PROCESS_FILE)?;
    file.set_len(0)
        .and_then(|()| file.seek(SeekFrom::Start(0)))
        .and_then(|_| file.write_all(format!("{pid} {created}\n").as_bytes()))
        .and_then(|()| file.sync_all())
        .map_err(unsafe_path)
}

/// 이 프로세스를 감독 프로세스로 기록한다. 단일 인스턴스 잠금을 쥔 채로만 부른다.
fn record_current_process(paths: &Paths) -> Result<(), ApiError> {
    let created = creation_time(unsafe { GetCurrentProcess() }).map_err(|_| {
        ApiError::new(
            "SERVICE_IO_ERROR",
            "서비스 감독 프로세스의 시작 시각을 확인할 수 없습니다.",
        )
    })?;
    write_process_record(paths, unsafe { GetCurrentProcessId() }, created)
}

/// 기록이 없으면 `Ok(None)`(감독 프로세스가 기록한 적이 없다). 읽을 수 없거나 형식이 틀리면 확인하지 못한 것이다.
fn read_process_record(paths: &Paths) -> Result<Option<(u32, u64)>, ApiError> {
    let mut file = lock_file(paths, PROCESS_FILE)?;
    let mut text = String::new();
    file.read_to_string(&mut text).map_err(unsafe_path)?;
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    let mut parts = text.split_whitespace();
    match (
        parts.next().and_then(|part| part.parse().ok()),
        parts.next().and_then(|part| part.parse().ok()),
        parts.next(),
    ) {
        (Some(pid), Some(created), None) => Ok(Some((pid, created))),
        _ => Err(unsafe_path(io::Error::other("invalid supervisor process record"))),
    }
}

/// 단일 인스턴스 잠금을 쥔 상태에서만 부른다(그래야 기록이 마지막 감독 프로세스의 것이다).
/// 기록된 프로세스가 없거나 PID가 다른 프로세스에 재사용됐으면 `Proven`, 같은 프로세스면 종료 신호를 기다릴 핸들,
/// 확인하지 못하면 None이다.
fn resolve_termination(paths: &Paths) -> Option<Termination> {
    let Some((pid, created)) = read_process_record(paths).ok()? else {
        return Some(Termination::Proven);
    };
    match ProcessHandle::open(pid).ok()? {
        None => Some(Termination::Proven),
        Some(handle) => match handle.created().ok()? {
            actual if actual == created => Some(Termination::Pending(handle)),
            _ => Some(Termination::Proven),
        },
    }
}

/// 감독 프로세스가 끝났다는 증거의 진행 상태.
#[derive(Debug)]
enum Termination {
    Unresolved,
    Pending(ProcessHandle),
    Proven,
}

/// 중지·재시작·제거·업데이트가 서비스를 내리는 동안 감독 프로세스가 작업자를 새로 만들지 못하게 하는 관문.
/// 이 값이 살아 있는 동안 감독 프로세스는 새 작업자를 만들지 않지만(일시 정지) 작업자를 건드리거나 스스로 끝나지도 않는다.
/// 서비스가 안전하게 내려간 것을 확인한 작업만 `commit_retirement`로 감독 프로세스를 퇴역시킨다.
/// 확정 없이 떨어뜨리면(프로세스가 죽어도 마찬가지) 관문이 열리고 감독 프로세스가 이어서 일한다.
#[derive(Debug)]
pub struct Fence {
    _gate: File,
    paths: Paths,
    supervisor: File,
    retire: File,
    /// 이 값이 감독 프로세스 단일 인스턴스 잠금을 쥐고 있다(감독 프로세스가 활동하지 않는다는 뜻).
    supervisor_locked: bool,
    termination: Termination,
    committed: bool,
}

/// 관문을 얻는다. 감독 프로세스가 없어도 얻는다(그 경우 `supervisor_alive`가 false다).
/// 다른 작업이 관문을 쥐고 있으면 잠시 기다린 뒤 `SERVICE_OPERATION_BUSY`다.
pub fn engage(paths: &Paths) -> Result<Fence, ApiError> {
    engage_within(paths, GATE_WAIT)
}

fn engage_within(paths: &Paths, wait: Duration) -> Result<Fence, ApiError> {
    secure::restrict_dir(&paths.home).map_err(unsafe_path)?;
    let gate = lock_file(paths, GATE_FILE)?;
    let deadline = Instant::now() + wait;
    loop {
        match gate.try_lock_exclusive() {
            Ok(()) => break,
            Err(error) if contended(&error) => {
                if Instant::now() >= deadline {
                    return Err(ApiError::new(
                        "SERVICE_OPERATION_BUSY",
                        "다른 서비스 중지·재시작 작업이 진행 중입니다. 끝난 뒤 다시 시도해 주세요.",
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(unsafe_path(error)),
        }
    }
    let supervisor = lock_file(paths, SUPERVISOR_LOCK)?;
    let retire = lock_file(paths, RETIRE_FILE)?;
    Ok(Fence {
        _gate: gate,
        paths: paths.clone(),
        supervisor,
        retire,
        supervisor_locked: false,
        termination: Termination::Unresolved,
        committed: false,
    })
}

impl Fence {
    /// 감독 프로세스가 아직 끝나지 않았으면 true. 단일 인스턴스 잠금을 얻고(작업 도중 새 감독 프로세스가 시작하지 못하게 이 값이
    /// 끝날 때까지 쥔다) 기록된 감독 프로세스의 핸들이 종료로 신호된 것까지 확인해야 false다.
    /// 일시 정지 중이거나 재시작을 기다리는 감독 프로세스도 true다. 확인하지 못하면 살아 있다고 본다.
    pub fn supervisor_alive(&mut self) -> bool {
        if matches!(self.termination, Termination::Proven) {
            return false;
        }
        if !self.supervisor_locked {
            if self.supervisor.try_lock_exclusive().is_err() {
                return true;
            }
            self.supervisor_locked = true;
        }
        if matches!(self.termination, Termination::Unresolved) {
            match resolve_termination(&self.paths) {
                Some(termination) => self.termination = termination,
                None => return true,
            }
        }
        if let Termination::Pending(process) = &self.termination {
            if !matches!(process.signaled(), Ok(true)) {
                return true;
            }
        }
        self.termination = Termination::Proven;
        false
    }

    /// 서비스(작업자)가 이미 내려갔음을 확인한 뒤에만 부른다. 감독 프로세스가 작업자를 되살리지 않고 끝나게 한다.
    /// 이 값이 끝나기 전에는 되돌릴 수 없다(확정한 값을 떨어뜨리면 감독 프로세스가 끝난 것을 기다린 뒤에 잠금을 푼다).
    /// 감독 프로세스가 없어도 부를 수 있다. 확정하지 못하면 `wait_supervisor_exit`가 알린다.
    pub fn commit_retirement(&mut self) {
        if self.committed {
            return;
        }
        // 감독 프로세스가 이 잠금을 아주 잠깐 쥐고 확인하는 순간과 겹쳐도 다시 시도한다.
        for attempt in 0..10 {
            if self.retire.try_lock_exclusive().is_ok() {
                self.committed = true;
                return;
            }
            if attempt < 9 {
                std::thread::sleep(Duration::from_millis(30));
            }
        }
    }

    /// 감독 프로세스가 만든 자식 작업자(시작 중 포함)나 감독 없이 도는 작업자(수명 잠금)가 있을 수 있으면 true. 확인하지 못해도 true.
    /// 관문을 쥔 동안에만 의미가 있다(관문이 새 자식 생성을 막는다).
    fn worker_live(&self) -> bool {
        match lock_file(&self.paths, CHILD_LOCK) {
            Ok(child) => match child.try_lock_exclusive() {
                Ok(()) => {
                    let _ = child.unlock();
                }
                Err(_) => return true,
            },
            Err(_) => return true,
        }
        match crate::server::service_lifetime_lock(&self.paths) {
            Ok(lifetime) => {
                drop(lifetime);
                false
            }
            Err(_) => true,
        }
    }

    /// 감독 프로세스가 끝나지 않았고(일시 정지·재시작 대기) 감독이 만든 자식 작업자도, 넘겨받은 작업자도 없을 때만 true.
    /// 확인하지 못하면 false다.
    pub fn supervisor_idle(&mut self) -> bool {
        self.supervisor_alive() && !self.worker_live()
    }

    /// 감독 프로세스 **프로세스**가 끝난 것을 확인할 때까지 기다린다. 확인하지 못하면 `SUPERVISOR_STOP_FAILED`다.
    /// 감독 프로세스가 같은 실행 파일이므로, 이 확인 없이는 실행 파일을 교체하거나 등록을 지우면 안 된다.
    /// 살아 있는 감독 프로세스가 있는데 `commit_retirement`가 확정되지 않았으면 기다려도 끝나지 않으므로
    /// `SUPERVISOR_RETIREMENT_UNCOMMITTED`로 바로 거절한다.
    pub fn wait_supervisor_exit(&mut self, timeout: Duration) -> Result<(), ApiError> {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.supervisor_alive() {
                return Ok(());
            }
            if !self.committed && !self.supervisor_locked {
                return Err(ApiError::new(
                    "SUPERVISOR_RETIREMENT_UNCOMMITTED",
                    "서비스 감독 프로세스를 퇴역시키지 않아 끝나기를 기다리지 않았습니다. 실행 파일과 설치 기록은 바꾸지 않았습니다.",
                ));
            }
            if Instant::now() >= deadline {
                return Err(ApiError::new(
                    "SUPERVISOR_STOP_FAILED",
                    "서비스 감독 프로세스가 끝난 것을 확인하지 못했습니다. 실행 파일과 설치 기록은 바꾸지 않았습니다.",
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Fence {
    fn drop(&mut self) {
        // 퇴역을 확정했다면 감독 프로세스가 그것을 보고 끝날 때까지 확정을 풀지 않는다.
        if self.committed {
            let _ = self.wait_supervisor_exit(DROP_EXIT_WAIT);
        }
    }
}

// ───────────────────────────── 감독 정책과 상태 기계 ─────────────────────────────

/// 재시작 정책. 연속 실패는 지수로 늘어나는 간격을 두고 예산이 다하면 멈춘다. 안정적으로 오래 돈 작업자 뒤에는 예산이 초기화된다.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// 종료·관문 확인 주기.
    pub tick: Duration,
    /// 준비 확인 간격과, 준비 시한 뒤의 느린 간격.
    pub probe: Duration,
    pub slow_probe: Duration,
    /// 이 시간 안에 준비되지 않으면 증거만 남긴다(죽이지 않는다).
    pub ready_timeout: Duration,
    /// 다른 작업자가 수명 잠금을 쥐고 있을 때 다시 확인하는 간격.
    pub peer_poll: Duration,
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// 연속 이만큼 실패하면 감독을 끝낸다.
    pub max_consecutive_failures: u32,
    /// 이 시간 이상 돈 작업자가 끝나면 이전 실패를 지운다.
    pub stable_after: Duration,
}

impl Policy {
    pub const fn production() -> Self {
        Self {
            tick: Duration::from_millis(250),
            probe: Duration::from_secs(1),
            slow_probe: Duration::from_secs(15),
            ready_timeout: Duration::from_secs(60),
            peer_poll: Duration::from_secs(2),
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(60),
            max_consecutive_failures: 8,
            stable_after: Duration::from_secs(120),
        }
    }

    /// `failures`번째 연속 실패 뒤 기다릴 시간: base, 2·base, 4·base… 최대 `max_delay`.
    fn delay(&self, failures: u32) -> Duration {
        let shift = failures.saturating_sub(1).min(16);
        self.base_delay.saturating_mul(1u32 << shift).min(self.max_delay)
    }
}

/// 감독 중인 작업자 하나.
pub trait Worker {
    fn pid(&self) -> u32;
    /// 끝났으면 종료 코드, 아직 돌면 None.
    fn exit_code(&mut self) -> Option<u32>;
    /// 서비스가 응답하는 증거를 한 번 확인한다. 오래 막히면 안 된다.
    fn poll_ready(&mut self) -> bool;
}

pub enum Spawn<W> {
    /// 중지 작업이 관문을 쥐고 있다.
    Fenced,
    Started(W),
}

/// 감독 상태 기계가 쓰는 환경. 실제 구현은 `SystemHost`, 테스트는 가짜 시계와 가짜 작업자를 쓴다.
pub trait Host {
    type Worker: Worker;
    /// 관문을 쥘 수 있을 때만 작업자를 만든다(만드는 동안 관문을 쥔다). 관문이 잡혀 있으면 `Spawn::Fenced`.
    fn spawn_unless_fenced(&mut self) -> io::Result<Spawn<Self::Worker>>;
    /// 중지 작업이 관문을 쥐고 있는지(일시 정지 요청). 작업자를 새로 만들지 못하게 할 뿐 감독을 끝내지 않는다.
    fn fenced(&mut self) -> bool;
    /// 중지 작업이 서비스가 내려간 것을 확인하고 감독 퇴역을 확정했는지. 이때만 감독이 끝난다.
    fn retired(&mut self) -> bool;
    /// 감독하지 않는 다른 작업자가 수명 잠금을 쥐고 있는지.
    fn peer_worker_alive(&mut self) -> bool;
    fn now(&self) -> Instant;
    fn sleep(&mut self, duration: Duration);
    fn record(&mut self, event: &str, value: Option<u64>);
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 중지 작업이 서비스가 내려간 것을 확인하고 퇴역을 확정했다. 작업자를 되살리지 않고 끝낸다.
    Retired,
    /// 연속 실패가 예산을 넘었다.
    BudgetExhausted { failures: u32 },
}

enum Wait {
    Elapsed,
    Retired,
}

enum Pause {
    Retired,
    Resumed,
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn retired_outcome<H: Host>(host: &mut H) -> Outcome {
    host.record("supervisor_retired", None);
    Outcome::Retired
}

fn exhausted_outcome<H: Host>(host: &mut H, failures: u32) -> Outcome {
    host.record("supervisor_budget_exhausted", Some(u64::from(failures)));
    Outcome::BudgetExhausted { failures }
}

/// `duration` 동안 기다리되 매 주기마다 퇴역 확정을 본다. 일시 정지(관문)는 기다림을 줄이지 않는다.
fn wait<H: Host>(host: &mut H, policy: &Policy, duration: Duration) -> Wait {
    let deadline = host.now() + duration;
    loop {
        if host.retired() {
            return Wait::Retired;
        }
        let now = host.now();
        if now >= deadline {
            return Wait::Elapsed;
        }
        host.sleep((deadline - now).min(policy.tick));
    }
}

fn back_off<H: Host>(host: &mut H, policy: &Policy, failures: u32) -> Wait {
    let delay = policy.delay(failures);
    host.record("supervisor_restart_scheduled", Some(millis(delay)));
    wait(host, policy, delay)
}

/// 관문이 잡혀 작업자를 만들 수 없는 동안 아무것도 하지 않고 기다린다. 퇴역이 확정되면 끝내고, 관문이 확정 없이 풀리면 다시 일한다.
fn pause<H: Host>(host: &mut H, policy: &Policy) -> Pause {
    host.record("supervisor_paused", None);
    loop {
        if host.retired() {
            return Pause::Retired;
        }
        if !host.fenced() {
            host.record("supervisor_resumed", None);
            return Pause::Resumed;
        }
        host.sleep(policy.tick);
    }
}

/// 작업자가 끝날 때까지 지켜보고 종료 코드를 돌려준다. 준비 증거만 기록하고 작업자는 건드리지 않는다.
/// 관문이 잡혀도 멈추지 않는다. 작업자가 정말 끝난 뒤에만 돌아온다.
fn watch<H: Host>(host: &mut H, worker: &mut H::Worker, started: Instant, policy: &Policy) -> u32 {
    let mut ready = false;
    let mut timeout_logged = false;
    let mut next_probe = started;
    loop {
        if let Some(code) = worker.exit_code() {
            return code;
        }
        let now = host.now();
        if !ready && now >= next_probe {
            let waited = now.saturating_duration_since(started);
            if worker.poll_ready() {
                ready = true;
                host.record("supervisor_worker_ready", Some(millis(waited)));
            } else {
                let late = waited >= policy.ready_timeout;
                if late && !timeout_logged {
                    timeout_logged = true;
                    host.record("supervisor_worker_ready_timeout", Some(millis(waited)));
                }
                next_probe = now + if late { policy.slow_probe } else { policy.probe };
            }
        }
        host.sleep(policy.tick);
    }
}

/// 작업자를 만들고 지켜보며, 비정상 종료마다 지수 간격으로 다시 만든다. 관문은 새 작업자 생성만 멈추고(일시 정지),
/// 퇴역이 확정되면 되살리지 않고 끝낸다. 관문이 확정 없이 풀리면 다시 작업자를 만든다.
/// 어떤 경우에도 대기 없이 반복하지 않는다(생성 실패·즉시 종료도 최소 `base_delay`를 기다린다).
pub fn supervise<H: Host>(host: &mut H, policy: &Policy) -> Outcome {
    host.record("supervisor_start", None);
    let mut failures = 0u32;
    let mut peer_logged = false;
    loop {
        if host.retired() {
            return retired_outcome(host);
        }
        if host.peer_worker_alive() {
            // 이미 도는 작업자(감독 없이 시작했거나 이전 감독 프로세스가 남긴 것)를 대신 지켜본다. 충돌이 아니므로 예산은 그대로다.
            // 관문이 잡혀 있어도 이 작업자가 실제로 끝날 때까지 계속 지켜본다(중지가 거절되면 이어서 감독해야 한다).
            if !peer_logged {
                peer_logged = true;
                host.record("supervisor_peer_worker", None);
            }
            if let Wait::Retired = wait(host, policy, policy.peer_poll) {
                return retired_outcome(host);
            }
            continue;
        }
        peer_logged = false;
        let mut worker = match host.spawn_unless_fenced() {
            Ok(Spawn::Fenced) => match pause(host, policy) {
                Pause::Retired => return retired_outcome(host),
                Pause::Resumed => continue,
            },
            Ok(Spawn::Started(worker)) => worker,
            Err(_) => {
                failures += 1;
                host.record("supervisor_spawn_failed", Some(u64::from(failures)));
                if failures >= policy.max_consecutive_failures {
                    return exhausted_outcome(host, failures);
                }
                match back_off(host, policy, failures) {
                    Wait::Retired => return retired_outcome(host),
                    Wait::Elapsed => continue,
                }
            }
        };
        let started = host.now();
        host.record("supervisor_worker_spawned", Some(u64::from(worker.pid())));
        let code = watch(host, &mut worker, started, policy);
        // 관문이 잡힌 채 끝난 작업자는 중지 작업이 끝냈을 가능성이 크다. 그 종료는 충돌 예산에 넣지 않는다.
        let fenced_exit = host.fenced();
        drop(worker);
        let uptime = host.now().saturating_duration_since(started);
        host.record("supervisor_worker_exit", Some(u64::from(code)));
        host.record("supervisor_worker_uptime_ms", Some(millis(uptime)));
        if host.retired() {
            return retired_outcome(host);
        }
        if code == PEER_EXISTS_EXIT {
            if let Wait::Retired = wait(host, policy, policy.peer_poll) {
                return retired_outcome(host);
            }
            continue;
        }
        if fenced_exit {
            // 곧바로 다시 돌아 관문이 아직 잡혀 있으면 일시 정지하고, 퇴역이 확정되지 않은 채 풀리면 이어서 작업자를 만든다.
            continue;
        }
        if uptime >= policy.stable_after {
            failures = 0;
        }
        failures += 1;
        if failures >= policy.max_consecutive_failures {
            return exhausted_outcome(host, failures);
        }
        if let Wait::Retired = back_off(host, policy, failures) {
            return retired_outcome(host);
        }
    }
}

// ───────────────────────────── 실제 시스템 ─────────────────────────────

/// 작업자를 띄우는 명령.
struct Launch {
    program: PathBuf,
    args: Vec<OsString>,
}

impl Launch {
    /// 이 실행 파일을 `--worker`로 다시 띄운다. 감독 프로세스가 실행 파일을 쥐고 있으므로 교체 중에 바뀌지 않는다.
    fn current() -> Result<Self, ApiError> {
        let program = std::env::current_exe().map_err(|_| {
            ApiError::new(
                "SERVICE_IO_ERROR",
                "서비스 실행 파일 경로를 확인할 수 없습니다.",
            )
        })?;
        Ok(Self {
            program,
            args: vec![OsString::from(WORKER_ARG)],
        })
    }
}

fn spawn_child(launch: &Launch, home: &Path) -> io::Result<Child> {
    use std::os::windows::process::CommandExt;
    Command::new(&launch.program)
        .args(&launch.args)
        // 감독 프로세스와 작업자가 같은 상태 폴더(과 그 이름의 pipe)를 쓰도록 명시한다.
        .env("AAM_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
}

struct SystemWorker {
    child: Child,
    paths: Paths,
    probe: Option<mpsc::Receiver<bool>>,
    /// 자식이 살아 있는 동안 쥐는 잠금. 이 값이 떨어질 때(자식이 끝난 것을 확인한 뒤) 풀려 중지 작업이 "자식 없음"을 안다.
    _live: File,
}

impl Worker for SystemWorker {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn exit_code(&mut self) -> Option<u32> {
        match self.child.try_wait() {
            Ok(None) => None,
            // 0xC0000409 같은 NTSTATUS도 그대로 부호 없는 값으로 남긴다.
            Ok(Some(status)) => Some(status.code().map_or(UNKNOWN_EXIT, |code| code as u32)),
            // A failed query is not evidence that the process exited.
            Err(_) => None,
        }
    }

    /// 이 자식 프로세스의 pipe에 `status.read`를 보내 응답을 확인한다. 응답 없는 작업자가 감독 루프를 막지 않도록
    /// 확인은 별도 스레드에서 하고 한 번에 하나만 진행한다.
    fn poll_ready(&mut self) -> bool {
        if let Some(receiver) = &self.probe {
            match receiver.try_recv() {
                Ok(ready) => {
                    self.probe = None;
                    return ready;
                }
                Err(mpsc::TryRecvError::Empty) => return false,
                Err(mpsc::TryRecvError::Disconnected) => self.probe = None,
            }
        }
        let (sender, receiver) = mpsc::channel();
        let paths = self.paths.clone();
        let pid = self.child.id();
        let spawned = std::thread::Builder::new()
            .name("ojak-supervisor-probe".into())
            .spawn(move || {
                // 서버 프로세스가 이 자식(pid)일 때만 응답으로 인정한다. 다른 프로세스가 같은 pipe를 쥐었으면 실패한다.
                let ready = aam_protocol::call_for_process(
                    &paths,
                    "status.read",
                    serde_json::json!({"observe": false}),
                    pid,
                )
                .is_ok();
                let _ = sender.send(ready);
            });
        if spawned.is_ok() {
            self.probe = Some(receiver);
        }
        false
    }
}

struct SystemHost {
    paths: Paths,
    launch: Launch,
    gate: File,
    retire: File,
}

impl SystemHost {
    fn open(paths: &Paths, launch: Launch) -> Result<Self, ApiError> {
        Ok(Self {
            paths: paths.clone(),
            launch,
            gate: lock_file(paths, GATE_FILE)?,
            retire: lock_file(paths, RETIRE_FILE)?,
        })
    }
}

impl Host for SystemHost {
    type Worker = SystemWorker;

    fn spawn_unless_fenced(&mut self) -> io::Result<Spawn<SystemWorker>> {
        match self.gate.try_lock_exclusive() {
            Ok(()) => {}
            Err(error) if contended(&error) => return Ok(Spawn::Fenced),
            Err(error) => return Err(error),
        }
        // 관문을 쥔 채로 자식 잠금을 얻고 만든다. 중지 작업이 관문을 얻은 뒤에는 새 작업자가 생기지 않고,
        // 그 전에 시작한 자식은 끝날 때까지 자식 잠금이 잡혀 있다.
        let started = (|| {
            let live = lock_file(&self.paths, CHILD_LOCK).map_err(|_| io::Error::other("supervisor child lock"))?;
            live.try_lock_exclusive()?;
            let child = spawn_child(&self.launch, &self.paths.home)?;
            Ok::<_, io::Error>((child, live))
        })();
        let _ = self.gate.unlock();
        started.map(|(child, live)| {
            Spawn::Started(SystemWorker {
                child,
                paths: self.paths.clone(),
                probe: None,
                _live: live,
            })
        })
    }

    fn fenced(&mut self) -> bool {
        match self.gate.try_lock_exclusive() {
            Ok(()) => {
                let _ = self.gate.unlock();
                false
            }
            // 경합이면 중지 작업 중이다. 그 밖의 오류도 되살리는 쪽보다 멈추는 쪽이 안전하다.
            Err(_) => true,
        }
    }

    fn retired(&mut self) -> bool {
        match self.retire.try_lock_exclusive() {
            Ok(()) => {
                let _ = self.retire.unlock();
                false
            }
            // 경합이면 중지 작업이 서비스가 내려간 것을 확인하고 확정한 것이다. 확인하지 못하면 감독을 이어간다(lease는 불확실하면 유지한다).
            Err(error) => contended(&error),
        }
    }

    fn peer_worker_alive(&mut self) -> bool {
        matches!(
            crate::server::service_lifetime_lock(&self.paths),
            Err(error) if error.code == "SERVICE_ALREADY_RUNNING"
        )
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn record(&mut self, event: &str, value: Option<u64>) {
        runtime_health::record(&self.paths, event, value);
    }
}

/// 감독 프로세스 본체. 단일 인스턴스 잠금을 얻고 자기 PID·생성 시각을 기록한 뒤, 중지 작업이 퇴역을 확정하면(`Outcome::Retired`) 정상으로,
/// 예산이 다하면 오류로 끝난다. 일시 정지(관문)만으로는 끝나지 않는다.
/// 서비스 진단 표식(`runtime_health::start`·`finish_*`)은 작업자의 것이라 여기서 건드리지 않는다.
pub fn run(paths: Paths) -> Result<(), ApiError> {
    secure::restrict_dir(&paths.home).map_err(unsafe_path)?;
    let _singleton = singleton(&paths)?;
    // 기록하지 못하면 중지 작업이 이 프로세스의 종료를 증명할 수 없으므로 감독하지 않는다.
    record_current_process(&paths)?;
    let launch = Launch::current()?;
    let mut host = SystemHost::open(&paths, launch)?;
    match supervise(&mut host, &Policy::production()) {
        Outcome::Retired => Ok(()),
        Outcome::BudgetExhausted { .. } => Err(ApiError::new(
            "SUPERVISOR_BUDGET_EXHAUSTED",
            "서비스가 짧은 시간에 반복해서 종료되어 감독을 멈췄습니다. 로그인 때 또는 서비스를 다시 시작할 때 감독이 다시 시작됩니다.",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, collections::VecDeque, rc::Rc};

    fn policy() -> Policy {
        Policy {
            tick: Duration::from_millis(100),
            probe: Duration::from_secs(1),
            slow_probe: Duration::from_secs(5),
            ready_timeout: Duration::from_secs(4),
            peer_poll: Duration::from_secs(2),
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(8),
            max_consecutive_failures: 5,
            stable_after: Duration::from_secs(120),
        }
    }

    const CRASH: u32 = 0xC000_0409;

    /// 가짜 작업자의 수명. `lives`가 None이면 중지 작업(관문)이 끝낼 때까지 산다.
    #[derive(Clone)]
    struct Script {
        lives: Option<Duration>,
        code: u32,
        ready_after: Option<Duration>,
    }

    fn crashes_after(seconds: u64, code: u32) -> Script {
        Script {
            lives: Some(Duration::from_secs(seconds)),
            code,
            ready_after: None,
        }
    }

    fn until_stopped() -> Script {
        Script {
            lives: None,
            code: 0,
            ready_after: None,
        }
    }

    struct Clock {
        base: Instant,
        elapsed: Cell<Duration>,
    }

    struct FakeWorker {
        pid: u32,
        clock: Rc<Clock>,
        /// 중지 작업이 이 시각에 작업자를 끝낸다(`lives`가 None인 작업자만).
        stop_at: Option<Duration>,
        spawned_at: Duration,
        script: Script,
    }

    impl Worker for FakeWorker {
        fn pid(&self) -> u32 {
            self.pid
        }

        fn exit_code(&mut self) -> Option<u32> {
            let now = self.clock.elapsed.get();
            match self.script.lives {
                Some(lives) if now >= self.spawned_at + lives => Some(self.script.code),
                Some(_) => None,
                None if self.stop_at.is_some_and(|at| now >= at) => Some(self.script.code),
                None => None,
            }
        }

        fn poll_ready(&mut self) -> bool {
            self.script
                .ready_after
                .is_some_and(|after| self.clock.elapsed.get() >= self.spawned_at + after)
        }
    }

    struct FakeHost {
        clock: Rc<Clock>,
        /// 관문이 잡힌 구간(시작, 끝). 끝이 None이면 계속 잡혀 있다.
        windows: Vec<(Duration, Option<Duration>)>,
        /// 퇴역이 확정되는 시각.
        retire_at: Option<Duration>,
        /// 중지 작업이 `until_stopped` 작업자를 끝내는 시각.
        stop_at: Option<Duration>,
        /// 이 시각 전에는 다른 작업자가 수명 잠금을 쥐고 있다.
        peer_until: Duration,
        spawns_left: VecDeque<Result<Script, ()>>,
        spawns: Vec<Duration>,
        events: Vec<(String, Option<u64>)>,
        next_pid: u32,
    }

    impl FakeHost {
        fn new(spawns: Vec<Result<Script, ()>>) -> Self {
            Self {
                clock: Rc::new(Clock {
                    base: Instant::now(),
                    elapsed: Cell::new(Duration::ZERO),
                }),
                windows: Vec::new(),
                retire_at: None,
                stop_at: None,
                peer_until: Duration::ZERO,
                spawns_left: spawns.into(),
                spawns: Vec::new(),
                events: Vec::new(),
                next_pid: 1000,
            }
        }

        /// 중지 작업이 이 시각에 작업자를 끝내고 관문을 건 채 퇴역까지 확정한다(성공한 중지).
        fn committed_stop_at(self, seconds: u64) -> Self {
            self.committed_stop_after_millis(seconds * 1000)
        }

        fn committed_stop_after_millis(mut self, millis: u64) -> Self {
            let at = Duration::from_millis(millis);
            self.windows.push((at, None));
            self.retire_at = Some(at);
            self.stop_at = Some(at);
            self
        }

        /// 퇴역을 확정하지 않는 임시 관문(거절된 중지). 작업자는 끝내지 않는다.
        fn refused_stop(mut self, from_millis: u64, to_millis: u64) -> Self {
            self.windows
                .push((Duration::from_millis(from_millis), Some(Duration::from_millis(to_millis))));
            self
        }

        fn values(&self, event: &str) -> Vec<Option<u64>> {
            self.events
                .iter()
                .filter(|(name, _)| name == event)
                .map(|(_, value)| *value)
                .collect()
        }

        fn names(&self) -> Vec<&str> {
            self.events.iter().map(|(name, _)| name.as_str()).collect()
        }
    }

    impl Host for FakeHost {
        type Worker = FakeWorker;

        fn spawn_unless_fenced(&mut self) -> io::Result<Spawn<FakeWorker>> {
            if self.fenced() {
                return Ok(Spawn::Fenced);
            }
            let next = self
                .spawns_left
                .pop_front()
                .expect("감독 프로세스가 예정에 없던 작업자를 만들었다");
            let now = self.clock.elapsed.get();
            self.spawns.push(now);
            match next {
                Err(()) => Err(io::Error::other("spawn failed")),
                Ok(script) => {
                    self.next_pid += 1;
                    Ok(Spawn::Started(FakeWorker {
                        pid: self.next_pid,
                        clock: Rc::clone(&self.clock),
                        stop_at: self.stop_at,
                        spawned_at: now,
                        script,
                    }))
                }
            }
        }

        fn fenced(&mut self) -> bool {
            let now = self.clock.elapsed.get();
            self.windows
                .iter()
                .any(|(from, to)| now >= *from && to.map_or(true, |to| now < to))
        }

        fn retired(&mut self) -> bool {
            self.retire_at.is_some_and(|at| self.clock.elapsed.get() >= at)
        }

        fn peer_worker_alive(&mut self) -> bool {
            self.clock.elapsed.get() < self.peer_until
        }

        fn now(&self) -> Instant {
            self.clock.base + self.clock.elapsed.get()
        }

        fn sleep(&mut self, duration: Duration) {
            let elapsed = self.clock.elapsed.get() + duration;
            assert!(
                elapsed < Duration::from_secs(10 * 24 * 3600),
                "감독 루프가 끝나지 않는다"
            );
            self.clock.elapsed.set(elapsed);
        }

        fn record(&mut self, event: &str, value: Option<u64>) {
            self.events.push((event.to_owned(), value));
        }
    }

    #[test]
    fn restart_delays_double_and_stay_bounded() {
        let policy = policy();
        let delays: Vec<u64> = (0..=7).map(|failures| millis(policy.delay(failures))).collect();
        assert_eq!(delays, [1000, 1000, 2000, 4000, 8000, 8000, 8000, 8000]);
        // 아주 많은 실패에도 넘치지 않고 상한에 머문다.
        assert_eq!(policy.delay(u32::MAX), Duration::from_secs(8));
        let production = Policy::production();
        assert_eq!(production.delay(1), Duration::from_secs(1));
        assert_eq!(production.delay(7), Duration::from_secs(60));
        assert_eq!(production.delay(1_000), Duration::from_secs(60));
        assert!(production.max_consecutive_failures >= 2);
        assert!(production.base_delay >= Duration::from_millis(500));
    }

    #[test]
    fn a_crashed_worker_is_replaced_by_a_new_one_after_the_first_delay() {
        let mut host = FakeHost::new(vec![Ok(crashes_after(5, CRASH)), Ok(until_stopped())]).committed_stop_at(20);
        let outcome = supervise(&mut host, &policy());
        assert_eq!(outcome, Outcome::Retired);
        assert_eq!(host.spawns.len(), 2, "충돌한 뒤 새 작업자가 정확히 하나 생겨야 한다");
        let gap = host.spawns[1] - host.spawns[0];
        assert!(gap >= Duration::from_secs(6) && gap < Duration::from_millis(6200), "{gap:?}");
        assert_eq!(host.values("supervisor_worker_exit"), [Some(u64::from(CRASH)), Some(0)]);
        assert_eq!(host.values("supervisor_restart_scheduled"), [Some(1000)]);
        assert_eq!(host.names().last(), Some(&"supervisor_retired"));
        assert_eq!(host.names().first(), Some(&"supervisor_start"));
    }

    #[test]
    fn repeated_crashes_back_off_exponentially_and_then_stop() {
        let crashes = (0..5).map(|_| Ok(crashes_after(1, 1))).collect();
        let mut host = FakeHost::new(crashes);
        let outcome = supervise(&mut host, &policy());
        assert_eq!(outcome, Outcome::BudgetExhausted { failures: 5 });
        assert_eq!(host.spawns.len(), 5);
        let delays = host.values("supervisor_restart_scheduled");
        assert_eq!(delays, [Some(1000), Some(2000), Some(4000), Some(8000)]);
        // 대기 없이 곧바로 다시 만들지 않는다: 가동 1초 + 예약된 간격 이상 벌어진다.
        for (index, delay) in [1000u64, 2000, 4000, 8000].iter().enumerate() {
            let gap = host.spawns[index + 1] - host.spawns[index];
            assert!(gap >= Duration::from_secs(1) + Duration::from_millis(*delay), "{index}: {gap:?}");
        }
        assert_eq!(host.names().last(), Some(&"supervisor_budget_exhausted"));
        assert_eq!(host.values("supervisor_budget_exhausted"), [Some(5)]);
    }

    #[test]
    fn a_stable_run_resets_the_failure_budget() {
        let scripts = vec![
            Ok(crashes_after(1, 1)),
            Ok(crashes_after(1, 1)),
            Ok(crashes_after(200, CRASH)), // 안정 시간을 넘긴 뒤 충돌: 예산이 초기화된다.
            Ok(crashes_after(1, 1)),
            Ok(crashes_after(1, 1)),
            Ok(crashes_after(1, 1)),
            Ok(until_stopped()),
        ];
        let mut host = FakeHost::new(scripts).committed_stop_at(2_000);
        // 초기화가 없었다면 다섯 번째 연속 실패에서 예산이 다했을 것이다.
        assert_eq!(supervise(&mut host, &policy()), Outcome::Retired);
        assert_eq!(host.spawns.len(), 7);
        assert_eq!(
            host.values("supervisor_restart_scheduled"),
            [Some(1000), Some(2000), Some(1000), Some(2000), Some(4000), Some(8000)]
        );
        assert!(host.values("supervisor_budget_exhausted").is_empty());
    }

    #[test]
    fn a_committed_retirement_raised_before_a_crash_prevents_any_restart() {
        // 중지 작업이 서비스가 내려간 것을 확인하고 퇴역을 확정한 뒤에는 작업자가 (중지 때문이든 충돌이든) 끝나도 되살리지 않는다.
        let mut host = FakeHost::new(vec![Ok(crashes_after(5, CRASH))]).committed_stop_at(3);
        assert_eq!(supervise(&mut host, &policy()), Outcome::Retired);
        assert_eq!(host.spawns.len(), 1);
        assert!(host.values("supervisor_restart_scheduled").is_empty());
        assert_eq!(host.values("supervisor_worker_exit"), [Some(u64::from(CRASH))]);
        assert_eq!(host.names().last(), Some(&"supervisor_retired"));
    }

    #[test]
    fn a_committed_retirement_during_the_backoff_cancels_the_scheduled_restart() {
        let mut host = FakeHost::new(vec![Ok(crashes_after(1, 1))]).committed_stop_after_millis(1500);
        assert_eq!(supervise(&mut host, &policy()), Outcome::Retired);
        assert_eq!(host.spawns.len(), 1, "예약된 재시작은 퇴역이 확정되면 실행되지 않는다");
        assert_eq!(host.values("supervisor_restart_scheduled"), [Some(1000)]);
        assert_eq!(host.names().last(), Some(&"supervisor_retired"));
    }

    #[test]
    fn a_supervisor_started_during_a_committed_stop_exits_without_spawning() {
        let mut host = FakeHost::new(Vec::new()).committed_stop_at(0);
        assert_eq!(supervise(&mut host, &policy()), Outcome::Retired);
        assert!(host.spawns.is_empty());
    }

    #[test]
    fn a_refused_stop_during_the_backoff_pauses_and_then_resumes_supervision() {
        // 충돌 뒤 재시작을 기다리는 동안 중지 작업이 관문을 걸었다가 거절되어 놓으면, 감독은 끝나지 않고 관문이 풀린 뒤 작업자를 다시 만든다.
        let mut host = FakeHost::new(vec![Ok(crashes_after(1, 1)), Ok(until_stopped())])
            .refused_stop(1500, 5000)
            .committed_stop_at(100);
        assert_eq!(supervise(&mut host, &policy()), Outcome::Retired);
        assert_eq!(host.spawns.len(), 2);
        assert!(
            host.spawns[1] >= Duration::from_secs(5) && host.spawns[1] < Duration::from_millis(5200),
            "{:?}",
            host.spawns[1]
        );
        assert_eq!(host.values("supervisor_paused").len(), 1);
        assert_eq!(host.values("supervisor_resumed").len(), 1);
        assert_eq!(host.values("supervisor_worker_exit"), [Some(1), Some(0)]);
        // 퇴역은 마지막에 확정된 중지 때문에 한 번만 일어난다.
        assert_eq!(host.values("supervisor_retired").len(), 1);
        assert_eq!(host.names().last(), Some(&"supervisor_retired"));
    }

    #[test]
    fn a_paused_supervisor_retires_only_when_the_retirement_is_committed() {
        let mut host = FakeHost::new(vec![Ok(crashes_after(1, 1))]);
        host.windows.push((Duration::from_millis(1500), None));
        host.retire_at = Some(Duration::from_secs(10));
        assert_eq!(supervise(&mut host, &policy()), Outcome::Retired);
        assert_eq!(host.spawns.len(), 1, "관문 때문에 정지한 동안 새 작업자를 만들지 않는다");
        assert_eq!(host.values("supervisor_paused").len(), 1);
        assert!(host.values("supervisor_resumed").is_empty());
        assert!(host.clock.elapsed.get() >= Duration::from_secs(10));
    }

    #[test]
    fn a_refused_stop_leaves_a_running_worker_alone_and_supervised() {
        // 거절된 중지는 관문만 잠깐 걸었을 뿐이다. 작업자가 계속 돌고, 나중에 확정된 중지 때 한 번만 끝난다.
        let mut host = FakeHost::new(vec![Ok(until_stopped())])
            .refused_stop(3000, 6000)
            .committed_stop_at(100);
        assert_eq!(supervise(&mut host, &policy()), Outcome::Retired);
        assert_eq!(host.spawns.len(), 1);
        assert!(host.values("supervisor_paused").is_empty());
        assert_eq!(host.values("supervisor_worker_exit"), [Some(0)]);
    }

    #[test]
    fn an_adopted_peer_stays_watched_while_a_stop_is_refused() {
        // 넘겨받은 작업자가 아직 도는 동안 관문이 잡혔다 풀려도 감독은 끝나지 않고, 그 작업자가 끝난 뒤에야 이어서 작업자를 만든다.
        let mut host = FakeHost::new(vec![Ok(until_stopped())])
            .refused_stop(3000, 6000)
            .committed_stop_at(100);
        host.peer_until = Duration::from_secs(20);
        assert_eq!(supervise(&mut host, &policy()), Outcome::Retired);
        assert_eq!(host.spawns.len(), 1);
        assert!(host.spawns[0] >= Duration::from_secs(20), "{:?}", host.spawns[0]);
        assert_eq!(host.values("supervisor_peer_worker").len(), 1);
        assert_eq!(host.values("supervisor_retired").len(), 1);
    }

    #[test]
    fn a_peer_exists_exit_under_a_refused_stop_keeps_supervising() {
        // 시작한 작업자가 다른 작업자 때문에 `PEER_EXISTS_EXIT`로 끝난 순간 관문이 잡혀 있어도 감독을 포기하지 않는다.
        let mut host = FakeHost::new(vec![Ok(crashes_after(1, PEER_EXISTS_EXIT)), Ok(until_stopped())])
            .refused_stop(500, 2000)
            .committed_stop_at(100);
        assert_eq!(supervise(&mut host, &policy()), Outcome::Retired);
        assert_eq!(host.spawns.len(), 2);
        assert!(host.spawns[1] >= Duration::from_secs(3), "{:?}", host.spawns[1]);
        assert!(host.values("supervisor_restart_scheduled").is_empty());
        assert!(host.values("supervisor_budget_exhausted").is_empty());
    }

    #[test]
    fn a_worker_that_ends_under_a_refused_stop_is_replaced_without_spending_the_budget() {
        let mut host = FakeHost::new(vec![Ok(crashes_after(1, CRASH)), Ok(until_stopped())])
            .refused_stop(500, 2500)
            .committed_stop_at(100);
        assert_eq!(supervise(&mut host, &policy()), Outcome::Retired);
        assert_eq!(host.spawns.len(), 2);
        assert!(host.spawns[1] >= Duration::from_millis(2500), "{:?}", host.spawns[1]);
        assert!(host.values("supervisor_restart_scheduled").is_empty());
        assert_eq!(host.values("supervisor_paused").len(), 1);
        assert_eq!(host.values("supervisor_resumed").len(), 1);
    }

    #[test]
    fn a_peer_worker_is_waited_for_without_spending_the_budget() {
        // 다른 작업자가 수명 잠금을 쥔 동안은 만들지 않고, 그 뒤 경쟁으로 `PEER_EXISTS_EXIT`로 끝나는 시도는 예산을 쓰지 않는다.
        let mut scripts: Vec<Result<Script, ()>> = (0..8).map(|_| Ok(crashes_after(1, PEER_EXISTS_EXIT))).collect();
        scripts.push(Ok(until_stopped()));
        let mut host = FakeHost::new(scripts).committed_stop_at(500);
        host.peer_until = Duration::from_secs(9);
        assert_eq!(supervise(&mut host, &policy()), Outcome::Retired);
        assert!(host.spawns[0] >= Duration::from_secs(9), "{:?}", host.spawns[0]);
        assert_eq!(host.spawns.len(), 9);
        assert!(host.values("supervisor_restart_scheduled").is_empty());
        assert!(host.values("supervisor_budget_exhausted").is_empty());
        assert_eq!(host.values("supervisor_peer_worker").len(), 1, "기다림은 전환 때만 한 번 기록한다");
        // 같은 이유로 끝나는 시도도 최소 peer_poll 간격을 둔다.
        for pair in host.spawns.windows(2).take(8) {
            assert!(pair[1] - pair[0] >= Duration::from_secs(3), "{pair:?}");
        }
    }

    #[test]
    fn spawn_failures_back_off_and_eventually_stop() {
        let mut host = FakeHost::new((0..5).map(|_| Err(())).collect());
        assert_eq!(supervise(&mut host, &policy()), Outcome::BudgetExhausted { failures: 5 });
        assert_eq!(host.spawns.len(), 5);
        assert_eq!(
            host.values("supervisor_spawn_failed"),
            [Some(1), Some(2), Some(3), Some(4), Some(5)]
        );
        assert_eq!(
            host.values("supervisor_restart_scheduled"),
            [Some(1000), Some(2000), Some(4000), Some(8000)]
        );
        for pair in host.spawns.windows(2) {
            assert!(pair[1] - pair[0] >= Duration::from_secs(1), "{pair:?}");
        }
    }

    #[test]
    fn readiness_is_recorded_as_evidence_and_never_kills_the_worker() {
        let ready = Script {
            lives: Some(Duration::from_secs(10)),
            code: 1,
            ready_after: Some(Duration::from_millis(2500)),
        };
        // 두 번째 작업자는 끝내 준비되지 않지만 감독 프로세스는 중지 작업이 올 때까지 그대로 둔다.
        let mut host = FakeHost::new(vec![Ok(ready), Ok(until_stopped())]).committed_stop_at(100);
        assert_eq!(supervise(&mut host, &policy()), Outcome::Retired);
        assert_eq!(host.values("supervisor_worker_ready"), [Some(3000)]);
        assert_eq!(host.values("supervisor_worker_ready_timeout"), [Some(4000)]);
        assert_eq!(host.spawns.len(), 2);
        assert_eq!(host.values("supervisor_worker_exit").len(), 2);
    }

    // ───────────── 실제 잠금과 실제 자식 프로세스 ─────────────

    fn temp_paths() -> Paths {
        let home = std::env::temp_dir().join(format!("ojak-supervisor-{}", uuid::Uuid::new_v4()));
        secure::restrict_dir(&home).unwrap();
        Paths {
            socket: home.join("service.sock"),
            database: home.join("state.db"),
            profiles: home.join("profiles"),
            home,
        }
    }

    /// 주어진 코드로 곧바로 끝나는 자식을 띄우는 명령.
    fn exits_with(code: u32) -> Launch {
        Launch {
            program: std::env::var_os("ComSpec").map_or_else(|| PathBuf::from("cmd.exe"), PathBuf::from),
            args: vec!["/C".into(), "exit".into(), code.to_string().into()],
        }
    }

    fn wait_for_exit(worker: &mut SystemWorker) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(code) = worker.exit_code() {
                return code;
            }
            assert!(Instant::now() < deadline, "자식이 끝나지 않았다");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn an_engaged_fence_blocks_spawns_until_it_is_dropped() {
        let paths = temp_paths();
        let mut host = SystemHost::open(&paths, exits_with(0)).unwrap();
        let fence = engage(&paths).unwrap();
        assert!(matches!(host.spawn_unless_fenced().unwrap(), Spawn::Fenced));
        assert!(host.fenced());
        drop(fence);
        assert!(!host.fenced());
        let Spawn::Started(mut worker) = host.spawn_unless_fenced().unwrap() else {
            panic!("관문이 풀린 뒤에는 작업자를 만들어야 한다");
        };
        assert_eq!(wait_for_exit(&mut worker), 0);
        std::fs::remove_dir_all(&paths.home).unwrap();
    }

    #[test]
    fn the_exit_code_of_a_real_child_is_reported_unchanged() {
        let paths = temp_paths();
        let mut host = SystemHost::open(&paths, exits_with(7)).unwrap();
        let Spawn::Started(mut worker) = host.spawn_unless_fenced().unwrap() else {
            panic!("관문이 없으면 작업자를 만들어야 한다");
        };
        assert_eq!(wait_for_exit(&mut worker), 7);
        std::fs::remove_dir_all(&paths.home).unwrap();
    }

    #[test]
    fn only_one_supervisor_can_hold_the_singleton_lock() {
        let paths = temp_paths();
        assert!(!running(&paths));
        let first = singleton(&paths).unwrap();
        assert_eq!(singleton(&paths).unwrap_err().code, "SUPERVISOR_ALREADY_RUNNING");
        assert!(running(&paths));
        drop(first);
        assert!(!running(&paths));
        let _second = singleton(&paths).unwrap();
        std::fs::remove_dir_all(&paths.home).ok();
    }

    fn long_lived_child() -> Child {
        Command::new("ping")
            .args(["-n", "120", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn creation_of(pid: u32) -> u64 {
        ProcessHandle::open(pid).unwrap().unwrap().created().unwrap()
    }

    #[test]
    fn a_fence_is_a_pause_until_the_retirement_is_committed() {
        let paths = temp_paths();
        let mut host = SystemHost::open(&paths, exits_with(0)).unwrap();
        let mut fence = engage(&paths).unwrap();
        assert!(host.fenced());
        assert!(!host.retired(), "관문만으로는 퇴역이 아니다");
        fence.commit_retirement();
        assert!(host.retired());
        drop(fence);
        assert!(!host.fenced());
        assert!(!host.retired());
        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn waiting_for_a_live_supervisor_requires_a_committed_retirement() {
        let paths = temp_paths();
        let _supervisor = singleton(&paths).unwrap();
        let mut fence = engage(&paths).unwrap();
        assert!(fence.supervisor_alive());
        assert_eq!(
            fence.wait_supervisor_exit(Duration::from_millis(100)).unwrap_err().code,
            "SUPERVISOR_RETIREMENT_UNCOMMITTED"
        );
        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn a_committed_fence_waits_for_the_supervisor_lock_and_then_keeps_new_ones_out() {
        let paths = temp_paths();
        let supervisor = singleton(&paths).unwrap();
        let mut fence = engage(&paths).unwrap();
        fence.commit_retirement();
        assert!(fence.supervisor_alive());
        assert_eq!(
            fence.wait_supervisor_exit(Duration::from_millis(100)).unwrap_err().code,
            "SUPERVISOR_STOP_FAILED"
        );
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            drop(supervisor);
        });
        fence.wait_supervisor_exit(Duration::from_secs(10)).unwrap();
        releaser.join().unwrap();
        assert!(!fence.supervisor_alive());
        // 감독 프로세스가 사라진 것을 확인한 뒤에는 이 작업이 끝날 때까지 새 감독 프로세스가 시작하지 못한다.
        assert_eq!(singleton(&paths).unwrap_err().code, "SUPERVISOR_ALREADY_RUNNING");
        drop(fence);
        let _revived = singleton(&paths).unwrap();
        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn a_released_singleton_lock_is_not_proof_that_the_supervisor_process_ended() {
        // 잠금은 프로세스 본체가 끝나기 전에 풀릴 수 있다. 기록된 프로세스 핸들이 종료로 신호될 때까지 끝난 것으로 보지 않는다.
        let paths = temp_paths();
        let mut child = long_lived_child();
        write_process_record(&paths, child.id(), creation_of(child.id())).unwrap();
        let mut fence = engage(&paths).unwrap();
        fence.commit_retirement();
        assert!(fence.supervisor_alive(), "잠금은 비었지만 프로세스가 살아 있다");
        assert_eq!(
            fence.wait_supervisor_exit(Duration::from_millis(200)).unwrap_err().code,
            "SUPERVISOR_STOP_FAILED"
        );
        child.kill().unwrap();
        child.wait().unwrap();
        fence.wait_supervisor_exit(Duration::from_secs(10)).unwrap();
        assert!(!fence.supervisor_alive());
        drop(fence);
        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn a_stale_supervisor_record_does_not_block_a_stop() {
        // 끝난 프로세스의 기록이나 PID가 다른 프로세스에 재사용된 기록(생성 시각이 다르다)은 감독 프로세스의 것이 아니다.
        let paths = temp_paths();
        let mut child = long_lived_child();
        let created = creation_of(child.id());
        write_process_record(&paths, child.id(), created + 1).unwrap();
        let mut reused = engage(&paths).unwrap();
        reused.wait_supervisor_exit(Duration::from_secs(5)).unwrap();
        drop(reused);
        let pid = child.id();
        child.kill().unwrap();
        child.wait().unwrap();
        write_process_record(&paths, pid, created).unwrap();
        let mut ended = engage(&paths).unwrap();
        ended.wait_supervisor_exit(Duration::from_secs(5)).unwrap();
        drop(ended);
        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn the_supervisor_publishes_its_own_process_record() {
        let paths = temp_paths();
        record_current_process(&paths).unwrap();
        let (pid, created) = read_process_record(&paths).unwrap().unwrap();
        assert_eq!(pid, std::process::id());
        assert_eq!(created, creation_of(pid));
        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn idle_requires_a_live_supervisor_without_a_child_or_an_adopted_peer() {
        let paths = temp_paths();
        let _supervisor = singleton(&paths).unwrap();
        let mut host = SystemHost::open(&paths, exits_with(0)).unwrap();
        // 관문을 걸기 전에 시작한 자식: 끝나서 `drop`되기 전까지 자식 잠금이 잡혀 있다.
        let Spawn::Started(mut worker) = host.spawn_unless_fenced().unwrap() else {
            panic!("관문이 없으면 작업자를 만들어야 한다");
        };
        let mut fence = engage(&paths).unwrap();
        assert!(fence.supervisor_alive());
        assert!(!fence.supervisor_idle(), "시작 중인 자식이 있으면 한가하지 않다");
        assert_eq!(wait_for_exit(&mut worker), 0);
        assert!(!fence.supervisor_idle(), "끝난 것을 감독이 확인하기 전에는 자식 잠금이 남아 있다");
        drop(worker);
        assert!(fence.supervisor_idle());
        let held = crate::server::service_lifetime_lock(&paths).unwrap();
        assert!(!fence.supervisor_idle(), "넘겨받은 작업자가 있으면 한가하지 않다");
        drop(held);
        assert!(fence.supervisor_idle());
        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn idle_is_false_when_there_is_no_supervisor() {
        let paths = temp_paths();
        let mut fence = engage(&paths).unwrap();
        assert!(!fence.supervisor_idle());
        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn a_second_concurrent_fence_is_refused_as_busy() {
        let paths = temp_paths();
        let _first = engage_within(&paths, Duration::from_millis(500)).unwrap();
        assert_eq!(
            engage_within(&paths, Duration::from_millis(100)).unwrap_err().code,
            "SERVICE_OPERATION_BUSY"
        );
        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn a_peer_worker_is_detected_through_the_service_lifetime_lock() {
        let paths = temp_paths();
        let mut host = SystemHost::open(&paths, exits_with(0)).unwrap();
        assert!(!host.peer_worker_alive());
        let held = crate::server::service_lifetime_lock(&paths).unwrap();
        assert!(host.peer_worker_alive());
        drop(held);
        assert!(!host.peer_worker_alive());
        std::fs::remove_dir_all(&paths.home).ok();
    }

    #[test]
    fn real_children_that_keep_crashing_are_restarted_a_bounded_number_of_times_and_logged() {
        let paths = temp_paths();
        let mut host = SystemHost::open(&paths, exits_with(7)).unwrap();
        let fast = Policy {
            tick: Duration::from_millis(10),
            probe: Duration::from_millis(20),
            slow_probe: Duration::from_millis(100),
            ready_timeout: Duration::from_secs(30),
            peer_poll: Duration::from_millis(20),
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(40),
            max_consecutive_failures: 3,
            stable_after: Duration::from_secs(3600),
        };
        assert_eq!(supervise(&mut host, &fast), Outcome::BudgetExhausted { failures: 3 });
        let log = std::fs::read_to_string(paths.home.join("logs").join("service-health.log")).unwrap();
        assert_eq!(log.matches("event=supervisor_worker_spawned").count(), 3, "{log}");
        assert_eq!(log.matches("event=supervisor_worker_exit value=7").count(), 3, "{log}");
        assert!(log.contains("event=supervisor_budget_exhausted value=3"), "{log}");
        std::fs::remove_dir_all(&paths.home).ok();
    }
}
