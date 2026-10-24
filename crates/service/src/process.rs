use aam_protocol::{process_identity, ProcessIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Alive,
    Dead,
    Unknown,
}

pub fn inspect(expected: &ProcessIdentity) -> Liveness {
    match process_identity(expected.pid) {
        Ok(actual) if &actual == expected => Liveness::Alive,
        Ok(_) => Liveness::Dead,
        Err(error) if error.code == "PROCESS_NOT_FOUND" => Liveness::Dead,
        Err(_) => Liveness::Unknown,
    }
}

/// 기록된 identity가 모두 이전 부팅의 것이면 참이다. 재부팅을 넘어 살아남는 프로세스와 자손은 없으므로
/// 자손 기록이 없는 세션도 종료로 확정할 수 있다. 부팅 ID를 읽지 못하거나 하나라도 현재 부팅이면 거짓이다.
pub fn all_from_previous_boot(identities: &[&ProcessIdentity]) -> bool {
    let Ok(current) = process_identity(std::process::id()) else {
        return false;
    };
    previous_boot(identities, &current.boot_id)
}

/// macOS는 `kern.bootsessionuuid`(부팅마다 새 UUID), Windows는 `PrefetchParameters\BootId`(부팅마다 증가하는 정수,
/// `bootid:N`)다. Windows는 실제 기록값이 `bootid:N` 형식이고 현재 값보다 작을 때만 이전 부팅으로 본다.
/// 읽지 못한 값(`windows-…`)·같은 값·더 큰 값(시계·레지스트리 이상)은 근거가 아니다.
pub fn previous_boot(identities: &[&ProcessIdentity], current: &str) -> bool {
    if current.is_empty() || identities.is_empty() {
        return false;
    }
    let counter = |value: &str| value.strip_prefix("bootid:").and_then(|n| n.parse::<u64>().ok());
    let unverified = |value: &str| value.is_empty() || value.starts_with("windows-") || value.starts_with("bootid:");
    match counter(current) {
        Some(now) => identities.iter().all(|identity| counter(&identity.boot_id).is_some_and(|then| then < now)),
        None if !unverified(current) => {
            identities.iter().all(|identity| !unverified(&identity.boot_id) && identity.boot_id != current)
        }
        None => false,
    }
}

pub fn background_exit_confirmed(native: &ProcessIdentity, background: &[ProcessIdentity]) -> bool {
    let valid = |identity: &ProcessIdentity| {
        identity.pid > 0
            && identity.pid <= i32::MAX as u32
            && !identity.started_at.is_empty()
            && !identity.boot_id.is_empty()
    };
    if !valid(native)
        || background
            .iter()
            .any(|identity| !valid(identity) || identity.boot_id != native.boot_id)
    {
        return false;
    }
    let Ok(current) = process_identity(std::process::id()) else {
        return false;
    };
    // 이전 부팅의 프로세스와 그룹은 현재 부팅까지 살아남을 수 없습니다. 검증된 부팅 식별자(macOS)에서만 씁니다.
    if cfg!(target_os = "macos") && current.boot_id != native.boot_id {
        return true;
    }
    if inspect(native) != Liveness::Dead
        || background
            .iter()
            .any(|identity| inspect(identity) != Liveness::Dead)
    {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        // 실행기는 native PID를 그룹 ID로 사용합니다. 신호 0은 종료시키지 않습니다.
        // 기록된 자손이 죽었어도 같은 그룹의 새 자손이 남아 있으면 반환하지 않습니다.
        let result = unsafe { libc::kill(-(native.pid as i32), 0) };
        result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }
    // Windows는 프로세스 그룹 대신 서비스가 쥔 Job 핸들로 확인한다(Service::job_finished).
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

pub fn is_child(child: &ProcessIdentity, supervisor: &ProcessIdentity) -> bool {
    #[cfg(target_os = "macos")]
    {
        if inspect(child) != Liveness::Alive || inspect(supervisor) != Liveness::Alive {
            return false;
        }
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>();
        let result = unsafe {
            libc::proc_pidinfo(
                child.pid as i32,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size as i32,
            )
        };
        result == size as i32
            && info.pbi_ppid == supervisor.pid
            && child.started_at == format!("{}:{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec)
    }
    #[cfg(windows)]
    {
        if inspect(child) != Liveness::Alive || inspect(supervisor) != Liveness::Alive {
            return false;
        }
        windows_parent_of(child).as_ref() == Some(supervisor) && inspect(child) == Liveness::Alive
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = (child, supervisor);
        false
    }
}

/// Windows 부모 identity. Toolhelp의 부모 PID는 부모가 끝난 뒤 재사용될 수 있으므로, 그 PID의 현재
/// identity(생성 시각 포함)를 읽고 자식보다 늦게 생긴 프로세스면 부모로 인정하지 않는다.
#[cfg(windows)]
fn windows_parent_of(child: &ProcessIdentity) -> Option<ProcessIdentity> {
    let parent_pid = aam_protocol::process_parent(child.pid).ok()?;
    let parent = process_identity(parent_pid).ok()?;
    let created = |identity: &ProcessIdentity| identity.started_at.parse::<u64>().ok();
    (created(&parent)? <= created(child)?).then_some(parent)
}

pub fn is_descendant(child: &ProcessIdentity, ancestor: &ProcessIdentity) -> bool {
    if child == ancestor
        || inspect(child) != Liveness::Alive
        || inspect(ancestor) != Liveness::Alive
    {
        return false;
    }
    #[cfg(target_os = "macos")]
    {
        let mut current = child.clone();
        for _ in 0..128 {
            let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
            let size = std::mem::size_of::<libc::proc_bsdinfo>();
            let result = unsafe {
                libc::proc_pidinfo(
                    current.pid as i32,
                    libc::PROC_PIDTBSDINFO,
                    0,
                    &mut info as *mut _ as *mut libc::c_void,
                    size as i32,
                )
            };
            if result != size as i32
                || current.started_at
                    != format!("{}:{:06}", info.pbi_start_tvsec, info.pbi_start_tvusec)
                || info.pbi_ppid <= 1
            {
                return false;
            }
            let Ok(parent) = process_identity(info.pbi_ppid) else {
                return false;
            };
            if &parent == ancestor {
                return inspect(child) == Liveness::Alive && inspect(ancestor) == Liveness::Alive;
            }
            current = parent;
        }
        false
    }
    #[cfg(windows)]
    {
        let mut current = child.clone();
        for _ in 0..128 {
            // 현재 단계의 identity가 그대로인지 확인한 뒤 부모로 올라간다.
            if inspect(&current) != Liveness::Alive {
                return false;
            }
            let Some(parent) = windows_parent_of(&current) else {
                return false;
            };
            if &parent == ancestor {
                return inspect(child) == Liveness::Alive && inspect(ancestor) == Liveness::Alive;
            }
            current = parent;
        }
        false
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        false
    }
}

#[cfg(test)]
mod boot_tests {
    use super::*;
    fn id(boot: &str) -> ProcessIdentity { ProcessIdentity { pid: 1, started_at: "1".into(), boot_id: boot.into() } }

    #[test]
    fn windows_boot_counter_only_counts_strictly_older_verified_boots() {
        let (old, same, newer, bad) = (id("bootid:19"), id("bootid:21"), id("bootid:22"), id("windows-boot-unverified"));
        assert!(previous_boot(&[&old, &id("bootid:20")], "bootid:21"));
        // 같은 부팅·더 큰 값·확인 못 한 값이 하나라도 있으면 근거가 아니다.
        assert!(!previous_boot(&[&old, &same], "bootid:21"));
        assert!(!previous_boot(&[&newer], "bootid:21"));
        assert!(!previous_boot(&[&bad], "bootid:21"));
        assert!(!previous_boot(&[&old], "windows-boot-unverified"));
        assert!(!previous_boot(&[], "bootid:21"));
        assert!(!previous_boot(&[&old], ""));
    }
}
