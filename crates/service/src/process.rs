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
    // 부팅 식별자는 macOS `kern.bootsessionuuid`만 검증했다. Windows의 레지스트리 BootId는 재부팅 전후
    // 동작을 확인하기 전까지 저장만 하고 반환 근거로 쓰지 않는다(docs/specs/2026-09-29-windows-port.md).
    if !cfg!(target_os = "macos") {
        return false;
    }
    if identities.is_empty() || identities.iter().any(|identity| identity.boot_id.is_empty()) {
        return false;
    }
    let Ok(current) = process_identity(std::process::id()) else {
        return false;
    };
    !current.boot_id.is_empty() && identities.iter().all(|identity| identity.boot_id != current.boot_id)
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
