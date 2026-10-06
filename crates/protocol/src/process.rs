use crate::{ApiError, ProcessIdentity};

#[cfg(target_os = "macos")]
#[repr(C)]
struct BsdInfo {
    flags: u32,
    status: u32,
    xstatus: u32,
    pid: u32,
    ppid: u32,
    uid: u32,
    gid: u32,
    ruid: u32,
    rgid: u32,
    svuid: u32,
    svgid: u32,
    reserved: u32,
    comm: [u8; 16],
    name: [u8; 32],
    nfiles: u32,
    pgid: u32,
    jobc: u32,
    tdev: u32,
    tpgid: u32,
    nice: i32,
    start_sec: u64,
    start_usec: u64,
}
#[cfg(target_os = "macos")]
extern "C" {
    fn proc_pidinfo(
        pid: libc::c_int,
        flavor: libc::c_int,
        arg: u64,
        buffer: *mut libc::c_void,
        size: libc::c_int,
    ) -> libc::c_int;
}

#[cfg(target_os = "macos")]
fn boot_id() -> Result<String, ApiError> {
    static BOOT: std::sync::LazyLock<Result<String, ApiError>> = std::sync::LazyLock::new(|| {
        let name = b"kern.bootsessionuuid\0";
        let mut buffer = [0u8; 128];
        let mut len = buffer.len();
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr().cast(),
                buffer.as_mut_ptr().cast(),
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc != 0 || len == 0 || len > buffer.len() {
            return Err(ApiError::new(
                "PROCESS_INSPECTION_FAILED",
                "부팅 세션을 확인하지 못했습니다",
            ));
        }
        Ok(String::from_utf8_lossy(&buffer[..len])
            .trim_end_matches('\0')
            .to_owned())
    });
    BOOT.clone()
}

/// PID 재사용을 프로세스 종료와 구별하기 위해 birth time과 boot identity를 함께 확인합니다.
#[cfg(target_os = "macos")]
pub fn process_identity(pid: u32) -> Result<ProcessIdentity, ApiError> {
    if pid == 0 || pid > i32::MAX as u32 {
        return Err(ApiError::new(
            "PROCESS_NOT_FOUND",
            "유효하지 않은 프로세스 ID입니다",
        ));
    }
    let mut info = std::mem::MaybeUninit::<BsdInfo>::zeroed();
    let n = unsafe {
        proc_pidinfo(
            pid as i32,
            3,
            0,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<BsdInfo>() as i32,
        )
    };
    if n as usize != std::mem::size_of::<BsdInfo>() {
        let err = std::io::Error::last_os_error();
        let code = if matches!(err.raw_os_error(), Some(libc::ESRCH) | Some(libc::ENOENT)) {
            "PROCESS_NOT_FOUND"
        } else {
            "PROCESS_INSPECTION_FAILED"
        };
        return Err(ApiError::new(
            code,
            "프로세스 시작 시각을 확인하지 못했습니다",
        ));
    }
    let info = unsafe { info.assume_init() };
    if info.pid != pid || info.start_sec == 0 {
        return Err(ApiError::new(
            "PROCESS_INSPECTION_FAILED",
            "프로세스 식별 정보가 일치하지 않습니다",
        ));
    }
    Ok(ProcessIdentity {
        pid,
        started_at: format!("{}:{:06}", info.start_sec, info.start_usec),
        boot_id: boot_id()?,
    })
}
#[cfg(windows)]
pub fn process_identity(pid: u32) -> Result<ProcessIdentity, ApiError> {
    // 생성 시각은 GetProcessTimes의 FILETIME(1601-01-01 이후 100ns)이다.
    // BootId는 레지스트리에서 읽어 저장만 한다. 재부팅 전후 검증 전이므로
    // all_from_previous_boot와 부팅 비교 반환은 이 값을 근거로 쓰지 않는다.
    let started = crate::winutil::creation_filetime(pid)?;
    Ok(ProcessIdentity {
        pid,
        started_at: started.to_string(),
        boot_id: crate::winutil::boot_id_candidate(),
    })
}
#[cfg(not(any(target_os = "macos", windows)))]
pub fn process_identity(_pid: u32) -> Result<ProcessIdentity, ApiError> {
    Err(ApiError::new(
        "UNSUPPORTED_PLATFORM",
        "현재 프로세스 검증은 macOS에서만 지원합니다",
    ))
}

/// false는 미확인 상태도 포함합니다. lease 해제 판단에는 process_identity의 오류 종류를 사용하세요.
pub fn process_alive(identity: &ProcessIdentity) -> bool {
    process_identity(identity.pid)
        .map(|actual| actual == *identity)
        .unwrap_or(false)
}
