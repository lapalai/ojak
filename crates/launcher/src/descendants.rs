use aam_protocol::{process_identity, ProcessIdentity};
use std::{
    collections::{BTreeMap, BTreeSet},
    thread,
    time::{Duration, Instant},
};

#[cfg(target_os = "macos")]
extern "C" {
    fn proc_listchildpids(parent: i32, buffer: *mut libc::c_void, size: i32) -> i32;
    fn proc_listpgrppids(group: i32, buffer: *mut libc::c_void, size: i32) -> i32;
}

pub(crate) struct Descendants {
    root: u32,
    observed: BTreeMap<u32, ProcessIdentity>,
    unidentified: BTreeSet<u32>,
    uncertain: bool,
    root_exited: bool,
    /// 루트 birth를 잃었다. 숫자 PGID·PID만으로는 새 구성원을 채택하지 않는다.
    root_lost: bool,
    /// Windows: native와 모든 자손이 들어 있는 Job. 자손은 Job을 벗어날 수 없어 목록이 곧 전체다.
    #[cfg(windows)]
    job: Option<aam_protocol::ProcessJob>,
}
impl Descendants {
    pub(crate) fn new(root: u32) -> Self {
        Self {
            root,
            observed: BTreeMap::new(),
            unidentified: BTreeSet::new(),
            uncertain: false,
            root_exited: false,
            root_lost: false,
            #[cfg(windows)]
            job: None,
        }
    }

    #[cfg(windows)]
    pub(crate) fn with_job(root: u32, job: aam_protocol::ProcessJob) -> Self {
        let mut descendants = Self::new(root);
        descendants.job = Some(job);
        descendants
    }

    /// Job에 남은 프로세스를 모두 관측한다. 그룹·자식 구분 없이 한 번에 얻으므로 자식 조회는 하지 않는다.
    #[cfg(windows)]
    fn collect(&mut self, _pid: u32, group: bool) {
        if !group {
            return;
        }
        let Some(pids) = self.job.as_ref().map(aam_protocol::ProcessJob::pids) else {
            self.uncertain = true;
            return;
        };
        let Ok(pids) = pids else {
            self.uncertain = true;
            return;
        };
        for pid in pids.into_iter().filter(|p| self.root_exited || *p != self.root) {
            match process_identity(pid) {
                Ok(identity) => {
                    self.unidentified.remove(&pid);
                    self.observed.insert(pid, identity);
                }
                Err(error) if error.code == "PROCESS_NOT_FOUND" => {
                    self.unidentified.remove(&pid);
                }
                Err(_) => {
                    if !self.observed.contains_key(&pid) {
                        self.unidentified.insert(pid);
                    }
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn collect(&mut self, pid: u32, group: bool) {
        let mut pids = [0i32; 2048];
        let count = unsafe {
            *libc::__error() = 0;
            let bytes = std::mem::size_of_val(&pids) as i32;
            if group {
                proc_listpgrppids(pid as i32, pids.as_mut_ptr().cast(), bytes)
            } else {
                proc_listchildpids(pid as i32, pids.as_mut_ptr().cast(), bytes)
            }
        };
        let errno = unsafe { *libc::__error() };
        if count < 0
            || count as usize >= pids.len()
            || (count == 0 && errno != 0 && errno != libc::ESRCH)
        {
            self.uncertain = true;
            return;
        }
        for pid in pids
            .into_iter()
            .take(count as usize)
            .filter(|p| *p > 0 && (self.root_exited || *p as u32 != self.root))
        {
            match process_identity(pid as u32) {
                Ok(identity) => {
                    self.unidentified.remove(&(pid as u32));
                    self.observed.insert(pid as u32, identity);
                }
                Err(error) if error.code == "PROCESS_NOT_FOUND" => {
                    self.unidentified.remove(&(pid as u32));
                }
                Err(_) => {
                    // 일시적인 식별 실패를 전체 관측 실패로 고정하지 않고 소멸까지 추적합니다.
                    if !self.observed.contains_key(&(pid as u32)) {
                        self.unidentified.insert(pid as u32);
                    }
                }
            }
        }
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    fn collect(&mut self, _pid: u32, _group: bool) {
        self.uncertain = true;
    }

    pub(crate) fn observe(&mut self) {
        if !self.root_lost {
            self.collect(self.root, true);
        }
        // 회수된 루트 PID는 재사용될 수 있으므로 더 이상 부모로 조회하지 않습니다.
        if !self.root_exited {
            self.collect(self.root, false);
        }
        let parents: Vec<_> = self.observed.keys().copied().collect();
        for parent in parents {
            match process_identity(parent) {
                Ok(current) if self.observed.get(&parent) == Some(&current) => {
                    self.collect(parent, false)
                }
                Ok(_) => {
                    self.observed.remove(&parent);
                }
                Err(error) if error.code == "PROCESS_NOT_FOUND" => {
                    self.observed.remove(&parent);
                }
                // 기존 birth identity를 유지하며 다음 관측에서 생존 여부를 다시 확인합니다.
                Err(_) => {}
            }
        }
        // birth가 없는 PID는 살아 있는 동안 재채택하지 않습니다. 확실한 소멸만 인정합니다.
        self.unidentified.retain(|pid| {
            !matches!(process_identity(*pid), Err(error) if error.code == "PROCESS_NOT_FOUND")
        });
    }

    /// 루트 birth를 잃었다(종료 또는 PID 재사용). 이후 루트 PID·PGID로는 아무도 새로 채택하지 않고,
    /// 이미 birth로 확인한 자손의 자식만 계속 따라간다.
    pub(crate) fn root_lost(&mut self) {
        self.root_exited = true;
        self.root_lost = true;
    }

    /// 지금까지 관측한 자손의 birth identity(루트 제외).
    pub(crate) fn identities(&self) -> Vec<ProcessIdentity> {
        self.observed.values().cloned().collect()
    }

    pub(crate) fn finished(mut self, grace: Duration) -> Option<Vec<ProcessIdentity>> {
        self.root_exited = true;
        let deadline = Instant::now() + grace;
        loop {
            self.observe();
            if self.uncertain {
                return None;
            }
            #[cfg(windows)]
            if self.observed.is_empty() && self.unidentified.is_empty() {
                // Job 목록이 비었으면 Job 안에 살아 있는 프로세스가 없다. 자손은 Job을 벗어날 수 없다.
                return Some(Vec::new());
            }
            #[cfg(unix)]
            if self.observed.is_empty() && self.unidentified.is_empty() {
                // 한 번의 목록 스냅샷만으로 비었다고 판단하지 않고 그룹 소멸도 확인합니다.
                let exists = unsafe { libc::kill(-(self.root as i32), 0) };
                if exists < 0 {
                    match std::io::Error::last_os_error().raw_os_error() {
                        Some(libc::ESRCH) => return Some(Vec::new()),
                        // macOS는 그룹의 마지막 구성원이 아직 회수되지 않은 좀비일 때 EPERM을 돌려준다.
                        // 그룹은 아직 있으므로 판정하지 않고 유예 시간 안에서 다시 확인한다.
                        Some(libc::EPERM) => {}
                        _ => return None,
                    }
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                // 유예 만료는 해제 근거가 아닙니다. 남은 프로세스의 identity를 넘깁니다.
                return if self.observed.is_empty() || !self.unidentified.is_empty() {
                    None
                } else {
                    Some(self.observed.into_values().collect())
                };
            }
            thread::sleep(remaining.min(Duration::from_millis(30)));
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::{
        os::unix::process::CommandExt,
        process::{Child, Command, Stdio},
        sync::mpsc,
    };

    struct OwnedProcess(Child);

    impl OwnedProcess {
        fn spawn(group: i32) -> Self {
            Self(
                Command::new("/bin/sh")
                    .args(["-c", "read -r line; exit 0"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .process_group(group)
                    .spawn()
                    .unwrap(),
            )
        }

        fn finish(&mut self) {
            drop(self.0.stdin.take());
            self.0.wait().unwrap();
        }
    }

    impl Drop for OwnedProcess {
        fn drop(&mut self) {
            // 테스트 소유 프로세스도 신호로 죽이지 않고 stdin EOF로 자연 종료시킵니다.
            self.finish();
        }
    }

    fn exited_root_with_member() -> (Descendants, OwnedProcess) {
        let mut root = OwnedProcess::spawn(0);
        // 직접 소유한 그룹 구성원으로 MCP 수명을 재현하여 고아 프로세스를 남기지 않습니다.
        let member = OwnedProcess::spawn(root.0.id() as i32);
        let mut descendants = Descendants::new(root.0.id());
        descendants.observe();
        root.finish();
        (descendants, member)
    }

    #[test]
    fn natural_exit_waits_for_briefly_lingering_group_member() {
        let (descendants, mut member) = exited_root_with_member();
        let (done, completed) = mpsc::channel();
        let drain = thread::spawn(move || {
            let evidence = descendants.finished(Duration::from_secs(5));
            done.send(()).unwrap();
            evidence
        });
        // 자식 종료는 이 스레드만 허용합니다. 이전 즉시 판정은 여기서 먼저 반환합니다.
        let premature = completed.recv_timeout(Duration::from_millis(100));
        member.finish();
        let evidence = drain.join().unwrap();
        assert!(matches!(premature, Err(mpsc::RecvTimeoutError::Timeout)));
        assert_eq!(evidence, Some(Vec::new()));
    }

    #[test]
    fn grace_timeout_preserves_live_identity() {
        let (descendants, member) = exited_root_with_member();
        let expected = process_identity(member.0.id()).unwrap();
        let evidence = descendants.finished(Duration::from_millis(60));
        assert_eq!(evidence, Some(vec![expected]));
    }

    #[test]
    fn incomplete_observation_never_reports_clear() {
        let (mut descendants, _member) = exited_root_with_member();
        descendants.uncertain = true;
        assert_eq!(descendants.finished(Duration::ZERO), None);
    }

    #[test]
    fn unidentified_process_blocks_until_confirmed_disappearance() {
        let mut root = OwnedProcess::spawn(0);
        let mut unidentified = OwnedProcess::spawn(0);
        let mut pending = Descendants::new(root.0.id());
        pending.unidentified.insert(unidentified.0.id());
        let mut after_exit = Descendants::new(root.0.id());
        after_exit.unidentified.insert(unidentified.0.id());
        root.finish();
        assert_eq!(pending.finished(Duration::ZERO), None);
        unidentified.finish();
        assert_eq!(after_exit.finished(Duration::ZERO), Some(Vec::new()));
    }

    #[test]
    fn tracked_pid_with_different_birth_is_not_adopted() {
        let mut root = OwnedProcess::spawn(0);
        let mut unrelated = OwnedProcess::spawn(0);
        let mut descendants = Descendants::new(root.0.id());
        let mut old_identity = process_identity(unrelated.0.id()).unwrap();
        old_identity.started_at.push_str("-previous");
        descendants.observed.insert(unrelated.0.id(), old_identity);
        root.finish();
        assert_eq!(descendants.finished(Duration::ZERO), Some(Vec::new()));
        assert!(unrelated.0.try_wait().unwrap().is_none());
    }
}
