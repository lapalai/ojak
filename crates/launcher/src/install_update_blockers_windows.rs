//! 업데이트가 `aam.exe`를 교체하려는데 그 파일을 쓰는 프로세스가 있을 때, 사용자 동의를 받아 닫는다.
//!
//! 안전 설계(모두 이 모듈 안에서 강제한다):
//! - 대상은 Restart Manager가 `설치폴더\aam.exe` 사용자로 보고한 프로세스 가운데, 같은 사용자·같은 세션이고
//!   실행 파일 경로가 정확히 그 `aam.exe`인 것뿐이다. 서비스·탐색기·critical·다른 사용자/세션·설치기 자신과 그 조상·
//!   경로가 다른 프로그램·RM이 이유 없이 보고한 reboot reason이 하나라도 있으면 아무것도 닫지 않고 `INSTALLER_FILES_BUSY`다.
//! - 화면에는 실행 파일 이름과 PID만 보인다. 명령줄·창 제목·RM의 앱 이름(문서 이름이 섞일 수 있다)은 읽지도 보이지도 않는다.
//! - 대상은 PID가 아니라 (PID, 시작 시각)으로 식별하고, 확인 시점부터 프로세스 핸들을 쥐어 PID 재사용을 막는다.
//!   닫는 작업은 사용자가 본 목록으로 새로 연 별도 RM 세션(`RM_UNIQUE_PROCESS` 등록)에서만 한다. 파일 등록 세션에
//!   force를 쓰면 RM이 대상을 다시 열거해 동의하지 않은 프로세스까지 닫을 수 있어 쓰지 않는다.
//! - 1차 동의는 정상 종료 요청(RmShutdown 0)만 뜻한다. 그래도 남으면 남은 프로그램만 다시 열거해 별도의 2차 동의
//!   (기본 버튼 `아니요`)를 받고 나서야 RM force 종료, 그래도 남은 것은 같은 핸들로 TerminateProcess 한다.
//! - 닫은 뒤 새로 나타난 프로세스(예: omp가 observer 작업자를 다시 띄움)는 이전 동의에 포함되지 않는다.
//!   매 라운드 새로 열거해 처음부터 동의를 받고, 라운드는 `MAX_ROUNDS`로 제한한다. 실행한 앱(omp/Orca)은 닫지 않는다.
//! - 끝은 항상 실제 쓰기 잠금 확인(`write_gate`)이다. 취소·불확실·미해결은 파일 시스템을 건드리기 전에 오류로 끝난다.
use super::*;
use std::{
    collections::{BTreeSet, HashMap},
    ffi::OsString,
    os::windows::{
        ffi::OsStringExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    ptr,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_MORE_DATA, FILETIME, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0},
    System::{
        Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS},
        RemoteDesktop::ProcessIdToSessionId,
        RestartManager::{
            RmConsole, RmEndSession, RmForceShutdown, RmGetList, RmMainWindow, RmOtherWindow, RmRegisterResources, RmShutdown,
            RmStartSession, RmUnknownApp, RM_PROCESS_INFO, RM_UNIQUE_PROCESS,
        },
        Threading::{
            GetCurrentProcessId, GetProcessTimes, IsProcessCritical, OpenProcess, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
            PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
        },
    },
    UI::WindowsAndMessaging::{MessageBoxW, IDNO, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_YESNO},
};

/// 새 동의 라운드 상한. 닫는 족족 같은 작업자가 다시 뜨면 무한히 묻지 않고 멈춘다.
pub(super) const MAX_ROUNDS: usize = 3;
const NORMAL_WAIT: Duration = Duration::from_secs(3);
const FORCE_WAIT: Duration = Duration::from_secs(2);
const GATE_WAIT: Duration = Duration::from_millis(1500);
const MAX_LISTED: usize = 8;

/// 사용자에게 보이고 닫기 대상이 되는 프로세스. `(pid, started)`가 식별자다.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Blocker {
    pub pid: u32,
    /// 프로세스 생성 시각(FILETIME). PID 재사용을 구분한다.
    pub started: u64,
    /// 실행 파일 이름만.
    pub name: String,
    /// 이 프로세스를 실행한 앱(이름, PID). 표시용이며 닫지 않는다.
    pub owner: Option<(String, u32)>,
}
impl Blocker {
    fn id(&self) -> (u32, u64) { (self.pid, self.started) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step { Normal, Force }
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Answer { Yes, No, Unavailable }

pub(super) trait Consent {
    fn ask(&mut self, step: Step, programs: &[Blocker]) -> Answer;
}
pub(super) trait World {
    /// 쓰기 잠금이 풀렸으면 true. 잠금 외 오류는 그대로 돌려준다.
    fn gate(&mut self) -> Result<bool, ApiError>;
    /// 지금 잠금을 잡은 프로세스. 닫아도 되는지 확인하지 못한 것이 있으면 `INSTALLER_FILES_BUSY`.
    fn blockers(&mut self) -> Result<Vec<Blocker>, ApiError>;
    /// 넘겨받은 대상만 닫고, 끝나거나 대기 시간이 지나면 돌아온다. 이미 사라졌거나 확인되지 않은 대상은 건드리지 않는다.
    fn close(&mut self, targets: &[Blocker], step: Step);
}

fn cancelled() -> ApiError {
    error("INSTALLER_CLOSE_CANCELLED", "프로그램 종료에 동의하지 않아 업데이트를 중단했어요. 이후에는 어떤 프로그램도 닫지 않았고 앱 실행 파일은 바꾸지 않았어요.")
}

/// 동의 → 정상 종료 → (남으면) 2차 동의 → 강제 종료를 쓰기 잠금이 풀릴 때까지 반복한다.
pub(super) fn negotiate(world: &mut dyn World, consent: &mut dyn Consent) -> Result<(), ApiError> {
    fn decide(answer: Answer) -> Result<(), ApiError> {
        match answer {
            Answer::Yes => Ok(()),
            Answer::No => Err(cancelled()),
            Answer::Unavailable => Err(files_busy()),
        }
    }
    for _ in 0..MAX_ROUNDS {
        if world.gate()? { return Ok(()); }
        let mut shown = world.blockers()?;
        // 한 화면에 모두 표시할 수 있는 대상만 동의받는다. 나머지는 새 라운드에서 따로 묻는다.
        shown.truncate(MAX_LISTED);
        // RM이 보지 못하는 프로세스가 잡고 있다. 무엇도 닫지 않는다.
        if shown.is_empty() { break; }
        decide(consent.ask(Step::Normal, &shown))?;
        world.close(&shown, Step::Normal);
        if world.gate()? { return Ok(()); }
        // 2차 동의는 1차에 본 프로세스 중 지금도 잠금을 잡은 것만 대상으로 한다. 새로 나타난 것은 다음 라운드에서 처음부터 묻는다.
        let seen: BTreeSet<_> = shown.iter().map(Blocker::id).collect();
        let still: Vec<Blocker> = world.blockers()?.into_iter().filter(|b| seen.contains(&b.id())).collect();
        if still.is_empty() { continue; }
        decide(consent.ask(Step::Force, &still))?;
        world.close(&still, Step::Force);
    }
    if world.gate()? { Ok(()) } else { Err(files_busy()) }
}

// ---- 동의 창 -------------------------------------------------------------------------------------------------

fn clean(name: &str) -> String { name.chars().filter(|c| !c.is_control()).take(64).collect() }
fn describe(program: &Blocker) -> String {
    match &program.owner {
        Some((owner, pid)) => format!("• {} (PID {}) — 실행한 앱: {} (PID {})", clean(&program.name), program.pid, clean(owner), pid),
        None => format!("• {} (PID {})", clean(&program.name), program.pid),
    }
}
fn list(programs: &[Blocker]) -> String {
    programs.iter().map(describe).collect::<Vec<_>>().join("\n")
}
pub(super) fn message(step: Step, programs: &[Blocker]) -> String {
    match step {
        Step::Normal => format!(
            "Ojak 실행 파일을 쓰고 있는 프로그램이 있어 업데이트를 이어갈 수 없어요.\n\n{}\n\n[예]를 누르면 위 프로그램에 정상 종료를 요청해요. 실행 중인 작업이 중단될 수 있으니 먼저 저장해 주세요. 위 프로그램을 실행한 앱은 닫지 않아요.\n[아니요]를 누르면 더 이상 프로그램을 닫지 않고 업데이트를 중단해요.",
            list(programs)
        ),
        Step::Force => format!(
            "정상 종료를 요청했지만 아래 프로그램이 아직 실행 중이에요.\n\n{}\n\n강제로 종료하면 저장하지 않은 작업이 사라질 수 있고, 종료한 프로그램은 자동으로 다시 시작되지 않아요. 위 프로그램만 종료하며 실행한 앱은 닫지 않아요. 실행한 앱이 켜져 있으면 같은 프로그램이 다시 나타날 수 있고, 그러면 다시 물어봐요.\n\n위 프로그램을 강제로 종료할까요? [아니요]를 누르면 아무것도 종료하지 않고 업데이트를 중단해요.",
            list(programs)
        ),
    }
}

/// 실제 MessageBox. 표준 `예`/`아니요` 두 버튼, 두 단계 모두 기본 버튼이 `아니요`.
pub(super) struct Native;
impl Consent for Native {
    fn ask(&mut self, step: Step, programs: &[Blocker]) -> Answer {
        let mut session = 0u32;
        // 세션 0(서비스)이나 확인 불가면 보이지 않는 창에서 영원히 기다릴 수 있다. 동의를 받지 못한 것으로 본다.
        if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 || session == 0 { return Answer::Unavailable; }
        let text = utf16(&message(step, programs));
        let title = utf16("Ojak 업데이트");
        match unsafe { MessageBoxW(ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_YESNO | MB_ICONWARNING | MB_SETFOREGROUND | MB_DEFBUTTON2) } {
            IDYES => Answer::Yes,
            IDNO => Answer::No,
            _ => Answer::Unavailable,
        }
    }
}
fn utf16(text: &str) -> Vec<u16> { text.encode_utf16().chain(std::iter::once(0)).collect() }

// ---- 실제 시스템 -----------------------------------------------------------------------------------------------

fn filetime(value: FILETIME) -> u64 { (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime) }
fn times(handle: HANDLE) -> Option<(u64, bool)> {
    let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) = unsafe { std::mem::zeroed() };
    (unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } != 0).then(|| (filetime(created), filetime(exited) != 0))
}
fn image(handle: HANDLE) -> Option<PathBuf> {
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) } == 0 { return None; }
    PathBuf::from(OsString::from_wide(&buffer[..length as usize])).canonicalize().ok()
}
fn same_path(a: &Path, b: &Path) -> bool { a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase() }
fn file_name(path: &Path) -> String { clean(&path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default()) }
fn open(pid: u32, access: u32) -> Option<OwnedHandle> {
    let raw = unsafe { OpenProcess(access, 0, pid) };
    (!raw.is_null()).then(|| unsafe { OwnedHandle::from_raw_handle(raw) })
}
fn exited(handle: &OwnedHandle) -> bool { unsafe { WaitForSingleObject(handle.as_raw_handle(), 0) == WAIT_OBJECT_0 } }
fn parent_map() -> Result<HashMap<u32, u32>, ApiError> {
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw == INVALID_HANDLE_VALUE { return Err(files_busy()); }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut map = HashMap::new();
    let mut more = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) } != 0;
    if !more { return Err(files_busy()); }
    while more {
        map.insert(entry.th32ProcessID, entry.th32ParentProcessID);
        more = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) } != 0;
    }
    Ok(map)
}
/// RM은 `\\?\` 접두 경로를 받지 못한다.
fn plain(path: &Path) -> Vec<u16> {
    let text = path.to_string_lossy();
    let text = match text.strip_prefix(r"\\?\") { Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest, _ => &text };
    utf16(text)
}

struct Session(u32);
impl Session {
    fn start() -> Result<Self, ApiError> {
        let mut handle = 0u32;
        let mut key = [0u16; 33];
        if unsafe { RmStartSession(&mut handle, 0, key.as_mut_ptr()) } != 0 { return Err(files_busy()); }
        Ok(Self(handle))
    }
    fn register(&self, file: Option<&Path>, processes: &[RM_UNIQUE_PROCESS]) -> Result<(), ApiError> {
        let name = file.map(plain);
        let files = [name.as_ref().map_or(ptr::null(), |name| name.as_ptr())];
        let status = unsafe {
            RmRegisterResources(
                self.0,
                u32::from(name.is_some()),
                if name.is_some() { files.as_ptr() } else { ptr::null() },
                processes.len() as u32,
                if processes.is_empty() { ptr::null() } else { processes.as_ptr() },
                0,
                ptr::null(),
            )
        };
        if status == 0 { Ok(()) } else { Err(files_busy()) }
    }
    /// 보고된 프로세스와 reboot reason(0이 아니면 RM이 못 닫는 것이 있다는 뜻).
    fn list(&self) -> Result<(Vec<RM_PROCESS_INFO>, u32), ApiError> {
        let mut needed = 0u32;
        let mut capacity = 0u32;
        let mut buffer: Vec<RM_PROCESS_INFO> = Vec::new();
        for _ in 0..4 {
            let mut reasons = 0u32;
            let status = unsafe { RmGetList(self.0, &mut needed, &mut capacity, if buffer.is_empty() { ptr::null_mut() } else { buffer.as_mut_ptr() }, &mut reasons) };
            match status {
                0 => {
                    buffer.truncate(capacity as usize);
                    return Ok((buffer, reasons));
                }
                ERROR_MORE_DATA if needed <= 128 => {
                    buffer = vec![unsafe { std::mem::zeroed() }; needed as usize + 4];
                    capacity = buffer.len() as u32;
                }
                _ => return Err(files_busy()),
            }
        }
        Err(files_busy())
    }
}
impl Drop for Session {
    fn drop(&mut self) { unsafe { RmEndSession(self.0); } }
}

pub(super) struct System {
    executable: PathBuf,
    session: u32,
    /// 마지막 `blockers()`가 확인한 프로세스의 핸들. 핸들을 쥔 동안 PID는 재사용되지 않는다.
    pinned: HashMap<(u32, u64), OwnedHandle>,
}
impl System {
    pub(super) fn new(install_dir: &Path) -> Result<Self, ApiError> {
        let mut session = 0u32;
        if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 { return Err(files_busy()); }
        Ok(Self { executable: install_dir.join("aam.exe"), session, pinned: HashMap::new() })
    }
    fn owner(&self, pid: u32, started: u64, parents: &HashMap<u32, u32>) -> Option<(String, u32)> {
        let parent = *parents.get(&pid)?;
        let handle = open(parent, PROCESS_QUERY_LIMITED_INFORMATION)?;
        let (created, gone) = times(handle.as_raw_handle())?;
        // 부모가 먼저 끝나 PID가 재사용됐다면 생성 시각이 자식보다 늦다.
        if gone || created >= started || !aam_protocol::winutil::same_user_process(parent) { return None; }
        let name = file_name(&image(handle.as_raw_handle())?);
        (!name.is_empty()).then_some((name, parent))
    }
    fn alive(&self, target: &Blocker) -> bool {
        self.pinned.get(&target.id()).is_some_and(|handle| !exited(handle))
    }
    fn wait(&self, targets: &[&Blocker], limit: Duration) {
        let deadline = Instant::now() + limit;
        while targets.iter().any(|target| self.alive(target)) && Instant::now() < deadline { thread::sleep(Duration::from_millis(50)); }
    }
    /// 사용자가 본 대상만 등록한 새 세션으로 닫는다. 등록 외의 항목이 보고되면 아무것도 닫지 않는다.
    fn shutdown(&self, targets: &[&Blocker], force: bool) {
        let processes: Vec<RM_UNIQUE_PROCESS> = targets
            .iter()
            .map(|target| RM_UNIQUE_PROCESS {
                dwProcessId: target.pid,
                ProcessStartTime: FILETIME { dwLowDateTime: target.started as u32, dwHighDateTime: (target.started >> 32) as u32 },
            })
            .collect();
        let Ok(session) = Session::start() else { return };
        if session.register(None, &processes).is_err() { return; }
        let Ok((listed, reasons)) = session.list() else { return };
        let allowed: BTreeSet<_> = targets.iter().map(|target| target.id()).collect();
        if reasons != 0 || listed.iter().any(|info| !allowed.contains(&(info.Process.dwProcessId, filetime(info.Process.ProcessStartTime)))) { return; }
        // 정상 종료가 안 되면 351(ERROR_FAIL_SHUTDOWN)이 온다. 결과는 실제 생존 여부로만 판단한다.
        unsafe { RmShutdown(session.0, if force { RmForceShutdown as u32 } else { 0 }, None); }
    }
}
impl World for System {
    fn gate(&mut self) -> Result<bool, ApiError> {
        let install_dir = self.executable.parent().ok_or_else(files_busy)?;
        released(install_dir, GATE_WAIT)
    }
    fn blockers(&mut self) -> Result<Vec<Blocker>, ApiError> {
        let session = Session::start()?;
        session.register(Some(&self.executable), &[])?;
        let (listed, reasons) = session.list()?;
        if reasons != 0 { return Err(files_busy()); }
        let parents = parent_map()?;
        let mut chain = BTreeSet::new();
        let mut current = unsafe { GetCurrentProcessId() };
        for _ in 0..32 {
            chain.insert(current);
            match parents.get(&current) { Some(next) if *next != 0 && !chain.contains(next) => current = *next, _ => break }
        }
        let mut found = BTreeMap::new();
        let mut pinned = HashMap::new();
        let expected_path = image_target(&self.executable)?;
        for info in &listed {
            let pid = info.Process.dwProcessId;
            let started = filetime(info.Process.ProcessStartTime);
            let closable = [RmUnknownApp, RmMainWindow, RmOtherWindow, RmConsole].contains(&info.ApplicationType);
            if !closable || info.TSSessionId != self.session || chain.contains(&pid) { return Err(files_busy()); }
            let handle = open(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE).ok_or_else(files_busy)?;
            // 핸들을 쥔 뒤 시작 시각을 확인하면 RM이 말한 바로 그 프로세스임이 고정된다. 그다음에야 사용자·경로를 믿는다.
            match times(handle.as_raw_handle()) {
                Some((created, false)) if created == started => {}
                _ => return Err(files_busy()),
            }
            if !aam_protocol::winutil::same_user_process(pid) { return Err(files_busy()); }
            let mut critical = 0;
            if unsafe { IsProcessCritical(handle.as_raw_handle(), &mut critical) } == 0 || critical != 0 { return Err(files_busy()); }
            let path = image(handle.as_raw_handle()).ok_or_else(files_busy)?;
            if !same_path(&path, &expected_path) { return Err(files_busy()); }
            let name = file_name(&path);
            found.insert((pid, started), Blocker { pid, started, name, owner: self.owner(pid, started, &parents) });
            pinned.insert((pid, started), handle);
        }
        self.pinned = pinned;
        Ok(found.into_values().collect())
    }
    fn close(&mut self, targets: &[Blocker], step: Step) {
        let live: Vec<&Blocker> = targets.iter().filter(|target| self.alive(target)).collect();
        if live.is_empty() { return; }
        self.shutdown(&live, step == Step::Force);
        self.wait(&live, if step == Step::Force { FORCE_WAIT } else { NORMAL_WAIT });
        if step != Step::Force { return; }
        // RM force로도 남은 프로세스는 사용자가 두 번 동의한 바로 그 핸들로만 종료한다.
        let survivors: Vec<&Blocker> = live.into_iter().filter(|target| self.alive(target)).collect();
        for target in &survivors {
            if let Some(handle) = self.pinned.get(&target.id()) { unsafe { TerminateProcess(handle.as_raw_handle(), 1); } }
        }
        self.wait(&survivors, FORCE_WAIT);
    }
}
fn image_target(executable: &Path) -> Result<PathBuf, ApiError> { executable.canonicalize().map_err(|_| files_busy()) }

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn blocker(pid: u32) -> Blocker { Blocker { pid, started: u64::from(pid) * 10, name: "aam.exe".into(), owner: Some(("omp.exe".into(), 7)) } }

    struct Script { answers: VecDeque<Answer>, asked: Vec<(Step, Vec<u32>)> }
    impl Script { fn new(answers: &[Answer]) -> Self { Self { answers: answers.iter().copied().collect(), asked: Vec::new() } } }
    impl Consent for Script {
        fn ask(&mut self, step: Step, programs: &[Blocker]) -> Answer {
            self.asked.push((step, programs.iter().map(|b| b.pid).collect()));
            self.answers.pop_front().expect("unexpected extra prompt")
        }
    }

    /// 실제 협상 로직을 시뮬레이션 세계에서 돌린다. 세계는 PID 집합과 "닫은 직후 다시 뜨는 프로세스" 대본만 가진다.
    struct Sim {
        alive: BTreeSet<u32>,
        /// 정상 종료를 무시하는 프로세스(콘솔 작업자).
        stubborn: bool,
        /// 닫기 호출마다 하나씩 새로 뜬다.
        respawn: VecDeque<u32>,
        /// RM이 못 보는 잠금.
        ghost: bool,
        closed: Vec<(Step, Vec<u32>)>,
    }
    impl Sim {
        fn new(alive: &[u32], stubborn: bool, respawn: &[u32]) -> Self {
            Self { alive: alive.iter().copied().collect(), stubborn, respawn: respawn.iter().copied().collect(), ghost: false, closed: Vec::new() }
        }
    }
    impl World for Sim {
        fn gate(&mut self) -> Result<bool, ApiError> { Ok(self.alive.is_empty() && !self.ghost) }
        fn blockers(&mut self) -> Result<Vec<Blocker>, ApiError> { Ok(self.alive.iter().copied().map(blocker).collect()) }
        fn close(&mut self, targets: &[Blocker], step: Step) {
            self.closed.push((step, targets.iter().map(|b| b.pid).collect()));
            for target in targets {
                if step == Step::Force || !self.stubborn { self.alive.remove(&target.pid); }
            }
            if let Some(next) = self.respawn.pop_front() { self.alive.insert(next); }
        }
    }
    fn code(result: Result<(), ApiError>) -> String { result.unwrap_err().code }

    #[test]
    fn refusing_the_first_prompt_closes_nothing() {
        let mut world = Sim::new(&[10], true, &[]);
        let mut consent = Script::new(&[Answer::No]);
        assert_eq!(code(negotiate(&mut world, &mut consent)), "INSTALLER_CLOSE_CANCELLED");
        assert!(world.closed.is_empty());
        assert_eq!(consent.asked, vec![(Step::Normal, vec![10])]);
    }
    #[test]
    fn force_needs_its_own_consent_and_refusal_stops_after_the_polite_attempt() {
        let mut world = Sim::new(&[10], true, &[]);
        let mut consent = Script::new(&[Answer::Yes, Answer::No]);
        assert_eq!(code(negotiate(&mut world, &mut consent)), "INSTALLER_CLOSE_CANCELLED");
        assert_eq!(world.closed, vec![(Step::Normal, vec![10])]);
        assert!(world.alive.contains(&10));
        assert_eq!(consent.asked, vec![(Step::Normal, vec![10]), (Step::Force, vec![10])]);
    }
    #[test]
    fn both_consents_close_the_stubborn_worker() {
        let mut world = Sim::new(&[10], true, &[]);
        let mut consent = Script::new(&[Answer::Yes, Answer::Yes]);
        negotiate(&mut world, &mut consent).unwrap();
        assert_eq!(world.closed, vec![(Step::Normal, vec![10]), (Step::Force, vec![10])]);
    }
    #[test]
    fn polite_close_that_works_never_prompts_for_force() {
        let mut world = Sim::new(&[10], false, &[]);
        let mut consent = Script::new(&[Answer::Yes]);
        negotiate(&mut world, &mut consent).unwrap();
        assert_eq!(consent.asked, vec![(Step::Normal, vec![10])]);
    }
    #[test]
    fn nothing_blocking_means_no_prompt() {
        let mut world = Sim::new(&[], true, &[]);
        let mut consent = Script::new(&[]);
        negotiate(&mut world, &mut consent).unwrap();
        assert!(consent.asked.is_empty() && world.closed.is_empty());
    }
    #[test]
    fn respawned_worker_gets_a_fresh_prompt_and_the_stale_pid_is_never_targeted() {
        let mut world = Sim::new(&[10], false, &[11]);
        let mut consent = Script::new(&[Answer::Yes, Answer::Yes]);
        negotiate(&mut world, &mut consent).unwrap();
        assert_eq!(consent.asked, vec![(Step::Normal, vec![10]), (Step::Normal, vec![11])]);
        assert_eq!(world.closed, vec![(Step::Normal, vec![10]), (Step::Normal, vec![11])]);
    }
    #[test]
    fn a_blocker_that_appears_during_the_polite_attempt_is_not_force_closed_without_asking() {
        let mut world = Sim::new(&[10], true, &[12]);
        let mut consent = Script::new(&[Answer::Yes, Answer::Yes, Answer::Yes, Answer::Yes]);
        negotiate(&mut world, &mut consent).unwrap();
        assert_eq!(consent.asked, vec![(Step::Normal, vec![10]), (Step::Force, vec![10]), (Step::Normal, vec![12]), (Step::Force, vec![12])]);
        assert!(world.closed.iter().all(|(_, pids)| pids.len() == 1));
        assert_eq!(world.closed.iter().filter(|(step, _)| *step == Step::Force).count(), 2);
    }
    #[test]
    fn declining_the_prompt_for_a_new_blocker_cancels_without_closing_it() {
        let mut world = Sim::new(&[10], false, &[11]);
        let mut consent = Script::new(&[Answer::Yes, Answer::No]);
        assert_eq!(code(negotiate(&mut world, &mut consent)), "INSTALLER_CLOSE_CANCELLED");
        assert_eq!(world.closed, vec![(Step::Normal, vec![10])]);
        assert!(world.alive.contains(&11));
    }
    #[test]
    fn endless_respawn_ends_with_files_busy_after_a_bounded_number_of_prompts() {
        let pids: Vec<u32> = (11..40).collect();
        let mut world = Sim::new(&[10], false, &pids);
        let mut consent = Script::new(&[Answer::Yes; 2 * MAX_ROUNDS]);
        assert_eq!(code(negotiate(&mut world, &mut consent)), "INSTALLER_FILES_BUSY");
        assert_eq!(consent.asked.len(), MAX_ROUNDS);
    }
    #[test]
    fn a_holder_restart_manager_cannot_see_is_not_prompted_for() {
        let mut world = Sim::new(&[], true, &[]);
        world.ghost = true;
        let mut consent = Script::new(&[]);
        assert_eq!(code(negotiate(&mut world, &mut consent)), "INSTALLER_FILES_BUSY");
        assert!(consent.asked.is_empty() && world.closed.is_empty());
    }
    #[test]
    fn an_unavailable_prompt_is_not_consent() {
        let mut world = Sim::new(&[10], true, &[]);
        let mut consent = Script::new(&[Answer::Unavailable]);
        assert_eq!(code(negotiate(&mut world, &mut consent)), "INSTALLER_FILES_BUSY");
        assert!(world.closed.is_empty());
    }
    #[test]
    fn programs_not_shown_in_the_first_batch_need_separate_consent() {
        let pids: Vec<u32> = (1..=MAX_LISTED as u32 + 1).collect();
        let mut world = Sim::new(&pids, false, &[]);
        let mut consent = Script::new(&[Answer::Yes, Answer::No]);
        assert_eq!(code(negotiate(&mut world, &mut consent)), "INSTALLER_CLOSE_CANCELLED");
        assert_eq!(world.alive, BTreeSet::from([MAX_LISTED as u32 + 1]));
        assert_eq!(world.closed, vec![(Step::Normal, pids[..MAX_LISTED].to_vec())]);
    }
}
