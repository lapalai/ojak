#[cfg(unix)]
use aam_protocol::process_alive;
use aam_protocol::{process_identity, ApiError, ProcessIdentity};
#[cfg(unix)]
use std::{
    cell::Cell,
    fs::OpenOptions,
    io,
    os::{
        fd::AsRawFd,
        unix::process::{CommandExt, ExitStatusExt},
    },
    sync::atomic::{AtomicU64, Ordering},
};
use std::{
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
static SIGNALS: AtomicU64 = AtomicU64::new(0);
#[cfg(unix)]
const FORWARDED: [i32; 7] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGTSTP,
    libc::SIGCONT,
    libc::SIGWINCH,
];

#[cfg(unix)]
extern "C" fn record_signal(signal: libc::c_int) {
    SIGNALS.fetch_or(1u64 << signal, Ordering::Relaxed);
}

#[cfg(unix)]
struct SignalGuard(Vec<(i32, libc::sigaction)>);
#[cfg(unix)]
impl SignalGuard {
    fn install() -> io::Result<Self> {
        SIGNALS.store(0, Ordering::Relaxed);
        let mut guard = Self(Vec::new());
        for signal in FORWARDED {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = record_signal as *const () as usize;
            action.sa_flags = libc::SA_RESTART;
            unsafe {
                libc::sigemptyset(&mut action.sa_mask);
            }
            if unsafe { libc::sigaction(signal, &action, &mut previous) } != 0 {
                return Err(io::Error::last_os_error());
            }
            guard.0.push((signal, previous));
        }
        Ok(guard)
    }
}
#[cfg(unix)]
impl Drop for SignalGuard {
    fn drop(&mut self) {
        for (signal, previous) in &self.0 {
            unsafe {
                libc::sigaction(*signal, previous, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(unix)]
// 터미널의 foreground group만 잠깐 바꿉니다. stdin/stdout/stderr는 가공하지 않습니다.
fn foreground(fd: i32, group: i32) -> io::Result<()> {
    unsafe {
        let mut blocked: libc::sigset_t = std::mem::zeroed();
        let mut previous: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut blocked);
        libc::sigaddset(&mut blocked, libc::SIGTTOU);
        let error = libc::pthread_sigmask(libc::SIG_BLOCK, &blocked, &mut previous);
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
        let result = libc::tcsetpgrp(fd, group);
        let error = io::Error::last_os_error();
        libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut());
        if result == 0 {
            Ok(())
        } else {
            Err(error)
        }
    }
}

#[cfg(unix)]
struct Terminal {
    file: Option<std::fs::File>,
    parent_group: i32,
    child_group: Cell<Option<i32>>,
}
#[cfg(unix)]
impl Terminal {
    fn capture() -> Self {
        let group = unsafe { libc::getpgrp() };
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok();
        Self {
            file,
            parent_group: group,
            child_group: Cell::new(None),
        }
    }
    fn give_to(&self, group: i32) {
        if let Some(file) = &self.file {
            let _ = foreground(file.as_raw_fd(), group);
        }
    }
    fn restore(&self) {
        if let Some(file) = &self.file {
            if self.child_group.get() == Some(unsafe { libc::tcgetpgrp(file.as_raw_fd()) }) {
                self.give_to(self.parent_group);
            }
        }
    }
}
#[cfg(unix)]
impl Drop for Terminal {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(unix)]
fn forward(identity: &ProcessIdentity, signal: i32) {
    // PID 재사용 시 다른 프로세스나 그 프로세스 그룹에 신호를 보내지 않습니다.
    if process_alive(identity)
        && unsafe { libc::getpgid(identity.pid as i32) } == identity.pid as i32
    {
        unsafe {
            libc::kill(-(identity.pid as i32), signal);
        }
    }
}

pub struct ChildFailure {
    pub error: ApiError,
    pub spawned: bool,
}

pub struct ChildOutcome {
    pub status: ExitStatus,
    pub background_processes: Option<Vec<ProcessIdentity>>,
}

#[cfg(unix)]
pub fn supervise<F>(mut command: Command, on_spawn: F) -> Result<ChildOutcome, ChildFailure>
where
    F: FnOnce(Option<&ProcessIdentity>),
{
    let failure = |code: &str, message: &str, spawned| ChildFailure {
        error: ApiError::new(code, message),
        spawned,
    };
    let _signals = SignalGuard::install().map_err(|_| {
        failure(
            "SIGNAL_SETUP_FAILED",
            "native 신호 전달을 준비하지 못했습니다.",
            false,
        )
    })?;
    let terminal = Terminal::capture();
    let tty = terminal
        .file
        .as_ref()
        .filter(|file| unsafe { libc::tcgetpgrp(file.as_raw_fd()) } == terminal.parent_group)
        .map(AsRawFd::as_raw_fd);
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    unsafe {
        command.pre_exec(move || {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            if let Some(fd) = tty {
                foreground(fd, libc::getpgrp())?;
            }
            for signal in FORWARDED {
                libc::signal(signal, libc::SIG_DFL);
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(|_| {
        failure(
            "SPAWN_FAILED",
            "native 실행 파일을 시작하지 못했습니다. 바이너리와 작업 폴더 권한을 확인하세요.",
            false,
        )
    })?;
    let pid = child.id();
    terminal.child_group.set(Some(pid as i32));
    let mut descendants = crate::descendants::Descendants::new(pid);
    let mut observed_at = Instant::now() - Duration::from_secs(1);
    // waitpid가 종료된 자식을 회수하기 전이므로 짧은 실행도 시작 시각을 조회할 수 있습니다.
    let identity = process_identity(pid).ok();
    if tty.is_some() {
        terminal.give_to(pid as i32);
    }
    on_spawn(identity.as_ref());
    loop {
        if observed_at.elapsed() >= Duration::from_millis(250) {
            descendants.observe();
            observed_at = Instant::now();
        }
        let pending = SIGNALS.swap(0, Ordering::Relaxed);
        if let Some(identity) = &identity {
            for signal in FORWARDED {
                if pending & (1u64 << signal) != 0 {
                    forward(identity, signal);
                }
            }
        }
        let mut status = 0;
        let result = unsafe {
            libc::waitpid(
                pid as i32,
                &mut status,
                libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED,
            )
        };
        if result < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(failure("CHILD_STATE_UNKNOWN", "native 프로세스 종료를 확인하지 못했습니다. 살아 있는 작업을 중단하거나 예약을 해제하지 않았습니다.", true));
        }
        if result > 0 {
            if libc::WIFEXITED(status) || libc::WIFSIGNALED(status) {
                terminal.restore();
                // supervise가 반환되기 전까지 호출자의 heartbeat는 계속 유지됩니다.
                let background_processes = descendants.finished(Duration::from_secs(5));
                return Ok(ChildOutcome {
                    status: ExitStatus::from_raw(status),
                    background_processes,
                });
            }
            if libc::WIFSTOPPED(status) {
                terminal.restore();
                // 셸의 fg/bg가 wrapper와 native 작업을 하나의 작업처럼 재개할 수 있게 합니다.
                unsafe {
                    libc::raise(libc::SIGSTOP);
                }
                if let Some(file) = &terminal.file {
                    if unsafe { libc::tcgetpgrp(file.as_raw_fd()) } == terminal.parent_group {
                        terminal.give_to(pid as i32);
                    }
                }
                if let Some(identity) = &identity {
                    forward(identity, libc::SIGCONT);
                }
            }
        }
        thread::sleep(Duration::from_millis(30));
    }
}

#[cfg(unix)]
pub fn finish_with_status(status: ExitStatus) -> ! {
    if let Some(signal) = status.signal() {
        unsafe {
            libc::signal(signal, libc::SIG_DFL);
            let mut signals: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut signals);
            libc::sigaddset(&mut signals, signal);
            libc::pthread_sigmask(libc::SIG_UNBLOCK, &signals, std::ptr::null_mut());
            libc::raise(signal);
        }
        std::process::exit(128 + signal);
    }
    std::process::exit(status.code().unwrap_or(1));
}

/// 콘솔 Ctrl+C·Ctrl+Break를 실행기만 처리한 것으로 표시한다. 같은 콘솔의 native 자식은 그대로 받아 스스로 처리한다.
/// 핸들러 목록은 자식에 상속되지 않는다(`SetConsoleCtrlHandler(NULL, TRUE)`와 달리 자식의 Ctrl+C를 끄지 않는다).
#[cfg(windows)]
unsafe extern "system" fn swallow_console_event(event: u32) -> windows_sys::Win32::Foundation::BOOL {
    const CTRL_C_EVENT: u32 = 0;
    const CTRL_BREAK_EVENT: u32 = 1;
    i32::from(event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT)
}

#[cfg(windows)]
struct ConsoleGuard;
#[cfg(windows)]
impl ConsoleGuard {
    fn install() -> Option<Self> {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        (unsafe { SetConsoleCtrlHandler(Some(swallow_console_event), 1) } != 0).then_some(Self)
    }
}
#[cfg(windows)]
impl Drop for ConsoleGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
        unsafe {
            SetConsoleCtrlHandler(Some(swallow_console_event), 0);
        }
    }
}

/// Windows 실행 감독. native를 일시 정지 상태로 만들어 이름 있는 Job에 넣은 뒤 재개하므로 모든 자손이 Job 안에 생긴다.
/// Job은 실행기가 끝나도 프로세스를 죽이지 않는다(작업 보존). 서비스는 Job 이름으로 전체 종료를 확인한다.
#[cfg(windows)]
pub fn supervise<F>(mut command: Command, on_spawn: F) -> Result<ChildOutcome, ChildFailure>
where
    F: FnOnce(Option<&ProcessIdentity>),
{
    let failure = |code: &str, message: &str, spawned| ChildFailure {
        error: ApiError::new(code, message),
        spawned,
    };
    let _console = ConsoleGuard::install().ok_or_else(|| {
        failure(
            "SIGNAL_SETUP_FAILED",
            "native 콘솔 이벤트 전달을 준비하지 못했습니다.",
            false,
        )
    })?;
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let (mut child, job) = aam_protocol::spawn_in_job(&mut command, false, false, true).map_err(|_| {
        failure(
            "SPAWN_FAILED",
            "native 실행 파일을 시작하지 못했습니다. 바이너리와 작업 폴더 권한을 확인하세요.",
            false,
        )
    })?;
    let pid = child.id();
    // 자식 핸들을 쥐고 있으므로 종료 뒤에도 PID가 재사용되지 않고 시작 시각을 조회할 수 있다.
    let identity = process_identity(pid).ok();
    let mut descendants = crate::descendants::Descendants::with_job(pid, job);
    let mut observed_at = Instant::now() - Duration::from_secs(1);
    on_spawn(identity.as_ref());
    loop {
        if observed_at.elapsed() >= Duration::from_millis(250) {
            descendants.observe();
            observed_at = Instant::now();
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let background_processes = descendants.finished(Duration::from_secs(5));
                return Ok(ChildOutcome {
                    status,
                    background_processes,
                });
            }
            Ok(None) => {}
            Err(_) => return Err(failure("CHILD_STATE_UNKNOWN", "native 프로세스 종료를 확인하지 못했습니다. 살아 있는 작업을 중단하거나 예약을 해제하지 않았습니다.", true)),
        }
        thread::sleep(Duration::from_millis(30));
    }
}

#[cfg(windows)]
pub fn finish_with_status(status: ExitStatus) -> ! {
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// 루트가 끝나도 Job에 남은 손자가 끝날 때까지 기다렸다가 "자손 없음"을 보고한다.
    #[test]
    fn windows_supervise_waits_for_a_lingering_grandchild_before_reporting_clear() {
        let mut command = Command::new("cmd");
        command.args(["/d", "/c", "start /b ping -n 3 127.0.0.1 >nul & exit 0"]);
        let mut native = None;
        let started = Instant::now();
        let outcome = supervise(command, |identity| native = identity.cloned())
            .unwrap_or_else(|failure| panic!("{}", failure.error.code));
        assert!(native.is_some(), "시작 identity를 보고해야 한다");
        assert_eq!(outcome.status.code(), Some(0));
        assert_eq!(outcome.background_processes, Some(Vec::new()));
        // ping(약 2초)이 끝나기 전에 비었다고 판단하지 않았다.
        assert!(started.elapsed() >= Duration::from_millis(1500));
    }
}
