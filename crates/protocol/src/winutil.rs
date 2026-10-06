//! Windows 전용 보조 함수. macOS 경로는 이 파일을 컴파일하지 않는다.
#![cfg(windows)]

use std::{
    ffi::c_void,
    io,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    path::Path,
    ptr,
};

use windows_sys::Win32::{
    Foundation::{
        GetLastError, LocalFree, ERROR_ACCESS_DENIED, ERROR_BROKEN_PIPE, ERROR_INVALID_PARAMETER,
        ERROR_IO_PENDING, ERROR_NOT_FOUND, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        FILETIME, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetNamedSecurityInfoW, GetSecurityInfo, SetNamedSecurityInfoW, SetSecurityInfo,
            SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        EqualSid, GetSecurityDescriptorDacl, GetTokenInformation, TokenOwner, TokenUser,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, TOKEN_OWNER, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateFileW, GetFileAttributesW, GetFileInformationByHandle, ReadFile, WriteFile,
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_FIRST_PIPE_INSTANCE,
        FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, INVALID_FILE_ATTRIBUTES,
        OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    },
    System::{
        Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList, OpenJobObjectW,
            JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
            JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        },
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId,
            GetNamedPipeServerProcessId, WaitNamedPipeW, PIPE_NOWAIT, PIPE_READMODE_BYTE,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
        },
        Registry::{
            RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
            REG_DWORD,
        },
        Threading::{
            CreateEventW, GetCurrentProcess, GetExitCodeProcess, GetProcessTimes, OpenProcess,
            OpenProcessToken, WaitForSingleObject, INFINITE, PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
};

/// 마지막 Win32 오류. 메시지에 경로·사용자 정보를 넣지 않는다.
fn io_error() -> io::Error {
    io::Error::last_os_error()
}

const STILL_ACTIVE: u32 = 259;

pub fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn path_wide(path: &Path) -> io::Result<Vec<u16>> {
    let text = path.to_str().ok_or_else(|| io::Error::other("경로는 UTF-8이어야 합니다"))?;
    Ok(wide(text))
}

pub fn current_user_sid() -> io::Result<String> {
    UserSid::current()?.string()
}

struct UserSid {
    buffer: Vec<u8>,
    /// TOKEN_USER(사용자) 또는 TOKEN_OWNER(새 객체의 기본 소유자). 둘 다 첫 필드가 SID 포인터다.
    owner: bool,
}

impl UserSid {
    fn current() -> io::Result<Self> {
        token_user(unsafe { GetCurrentProcess() })
    }

    fn sid(&self) -> *mut c_void {
        if self.owner {
            unsafe { (*self.buffer.as_ptr().cast::<TOKEN_OWNER>()).Owner }
        } else {
            unsafe { (*self.buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid }
        }
    }

    fn string(&self) -> io::Result<String> {
        sid_string(self.sid())
    }
}

/// SID를 `S-1-5-…` 문자열로 바꾼다.
fn sid_string(sid: *mut c_void) -> io::Result<String> {
    let mut raw = ptr::null_mut();
    if unsafe { ConvertSidToStringSidW(sid, &mut raw) } == 0 || raw.is_null() {
        return Err(io_error());
    }
    let mut len = 0usize;
    unsafe {
        while *raw.add(len) != 0 {
            len += 1;
        }
    }
    let text = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(raw, len) });
    unsafe {
        LocalFree(raw.cast());
    }
    if text.is_empty() || text.contains(|c: char| c.is_control() || c == '"') {
        return Err(io::Error::other("사용자 SID를 확인하지 못했습니다"));
    }
    Ok(text)
}

fn token_user(process: HANDLE) -> io::Result<UserSid> {
    token_sid(process, false)
}

/// 소유자가 현재 사용자인지. 관리자 권한으로 실행되면 새 파일의 기본 소유자가 사용자 대신
/// Administrators 그룹(토큰의 TOKEN_OWNER)이 되므로 그 SID도 "내가 만든 것"으로 인정한다.
fn owner_is_me(owner: *mut c_void) -> bool {
    if owner.is_null() {
        return false;
    }
    let process = unsafe { GetCurrentProcess() };
    [false, true].into_iter().any(|default_owner| {
        token_sid(process, default_owner).is_ok_and(|sid| unsafe { EqualSid(owner, sid.sid()) } != 0)
    })
}

/// 이미 연 토큰에서 사용자 SID를 읽는다.
fn token_sid_from(token: HANDLE) -> io::Result<UserSid> {
    read_token(token, false)
}

fn token_sid(process: HANDLE, owner: bool) -> io::Result<UserSid> {
    let mut token = INVALID_HANDLE_VALUE;
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io_error());
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token as RawHandle) };
    read_token(token.as_raw_handle() as HANDLE, owner)
}

fn read_token(token: HANDLE, owner: bool) -> io::Result<UserSid> {
    let class = if owner { TokenOwner } else { TokenUser };
    let mut needed = 0u32;
    unsafe {
        GetTokenInformation(token, class, ptr::null_mut(), 0, &mut needed);
    }
    if needed == 0 || needed > 64 * 1024 {
        return Err(io::Error::other("프로세스 토큰을 확인하지 못했습니다"));
    }
    let mut buffer = vec![0u8; needed as usize];
    if unsafe { GetTokenInformation(token, class, buffer.as_mut_ptr().cast(), needed, &mut needed) } == 0 {
        return Err(io_error());
    }
    Ok(UserSid { buffer, owner })
}

struct SecurityDescriptor {
    raw: *mut c_void,
}

impl Drop for SecurityDescriptor {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            unsafe {
                LocalFree(self.raw);
            }
        }
    }
}

fn owner_only_descriptor() -> io::Result<SecurityDescriptor> {
    let sid = UserSid::current()?.string()?;
    // 보호된 DACL. 현재 사용자만 Generic All. 원격·다른 사용자 ACE는 넣지 않는다.
    // OICI: 폴더 안에 새로 생기는 파일·하위 폴더(프로필 자격 증명 등)도 같은 ACE를 상속한다.
    let sddl = format!("O:{sid}D:P(A;OICI;GA;;;{sid})");
    let wide_sddl = wide(&sddl);
    let mut raw = ptr::null_mut();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            wide_sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut raw,
            ptr::null_mut(),
        )
    } == 0
        || raw.is_null()
    {
        return Err(io_error());
    }
    Ok(SecurityDescriptor { raw })
}

fn security_attributes(sd: &SecurityDescriptor) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: sd.raw,
        bInheritHandle: 0,
    }
}

pub fn restrict_to_owner(path: &Path) -> io::Result<()> {
    let sd = owner_only_descriptor()?;
    let mut present = 0i32;
    let mut dacl = ptr::null_mut();
    let mut defaulted = 0i32;
    if unsafe { GetSecurityDescriptorDacl(sd.raw, &mut present, &mut dacl, &mut defaulted) } == 0
        || present == 0
        || dacl.is_null()
    {
        return Err(io::Error::other("소유자 전용 ACL을 만들지 못했습니다"));
    }
    let user = UserSid::current()?;
    let wide_path = path_wide(path)?;
    let status = unsafe {
        SetNamedSecurityInfoW(
            wide_path.as_ptr().cast_mut(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            user.sid(),
            ptr::null_mut(),
            dacl,
            ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}

pub fn owned_by_current_user(path: &Path) -> io::Result<bool> {
    let wide_path = path_wide(path)?;
    let mut owner = ptr::null_mut();
    let mut sd = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut sd,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let same = owner_is_me(owner);
    if !sd.is_null() {
        unsafe {
            LocalFree(sd);
        }
    }
    Ok(same)
}

/// 파일 소유자 SID 문자열. `owned_by_current_user`와 달리 TOKEN_OWNER(승격 시 Administrators)를 인정하지 않는
/// 엄격한 값이라, "승격 실행이 남긴 기록의 소유자가 정확히 사용자 SID인지" 검사할 때 쓴다.
pub fn file_owner_sid(path: &Path) -> io::Result<String> {
    let wide_path = path_wide(path)?;
    let mut owner = ptr::null_mut();
    let mut sd = ptr::null_mut();
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide_path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut sd,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let text = if owner.is_null() { Err(io::Error::other("파일 소유자를 확인하지 못했습니다")) } else { sid_string(owner) };
    if !sd.is_null() {
        unsafe {
            LocalFree(sd);
        }
    }
    text
}

pub fn is_reparse_point(path: &Path) -> io::Result<bool> {
    let wide_path = path_wide(path)?;
    let attrs = unsafe { GetFileAttributesW(wide_path.as_ptr()) };
    if attrs == INVALID_FILE_ATTRIBUTES {
        return Err(io_error());
    }
    Ok(attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// Check access on the opened object, not a path that could be replaced after validation.
/// Unknown ACE types fail closed. Deny ACEs never grant access and may be ignored here.
pub fn handle_access_is_safe(file: &std::fs::File, private: bool) -> io::Result<bool> {
    use windows_sys::Win32::Security::{
        GetAce, IsValidAcl, IsWellKnownSid, ACCESS_ALLOWED_ACE, ACE_HEADER,
        WinBuiltinAdministratorsSid, WinLocalSystemSid,
    };
    let mut owner = ptr::null_mut();
    let mut dacl = ptr::null_mut();
    let mut descriptor = ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            &mut dacl,
            ptr::null_mut(),
            &mut descriptor,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let _descriptor = SecurityDescriptor { raw: descriptor };
    if !owner_is_me(owner) || dacl.is_null() || unsafe { IsValidAcl(dacl) } == 0 {
        return Ok(false);
    }
    let user = UserSid::current()?;
    for index in 0..unsafe { (*dacl).AceCount } {
        let mut raw = ptr::null_mut();
        if unsafe { GetAce(dacl, u32::from(index), &mut raw) } == 0 || raw.is_null() {
            return Err(io_error());
        }
        let header = unsafe { &*raw.cast::<ACE_HEADER>() };
        match header.AceType {
            0 => {
                let ace = unsafe { &*raw.cast::<ACCESS_ALLOWED_ACE>() };
                let sid = ptr::addr_of!(ace.SidStart).cast_mut().cast();
                // A shared parent may be readable by others, but not writable. Administrators
                // and SYSTEM already control the host; private managed objects admit only us.
                const WRITE_ACCESS: u32 = 0x500d0156;
                let trusted_system = !private && unsafe {
                    IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
                        || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                };
                let forbidden = if private { ace.Mask != 0 } else { ace.Mask & WRITE_ACCESS != 0 };
                if forbidden && !trusted_system && unsafe { EqualSid(sid, user.sid()) } == 0 {
                    return Ok(false);
                }
            }
            1 => {}
            _ => return Ok(false),
        }
    }
    Ok(true)
}

pub fn single_link_regular_file(file: &std::fs::File) -> io::Result<bool> {
    let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(io_error());
    }
    Ok(info.nNumberOfLinks == 1 && info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT == 0)
}

/// Stable volume/file identity from the opened object; paths and timestamps are not identity.
pub fn file_identity(file: &std::fs::File) -> io::Result<(u64, u64)> {
    let mut info = unsafe { std::mem::zeroed::<BY_HANDLE_FILE_INFORMATION>() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as HANDLE, &mut info) } == 0 {
        return Err(io_error());
    }
    Ok((u64::from(info.dwVolumeSerialNumber), (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow)))
}

pub fn pipe_name_for(path: &Path) -> String {
    let text = path.to_string_lossy();
    if text.starts_with(r"\\.\pipe\") || text.starts_with(r"\\?\pipe\") {
        return text.into_owned();
    }
    let hash = text.bytes().fold(14695981039346656037u64, |h, b| {
        (h ^ b as u64).wrapping_mul(1099511628211)
    });
    format!(r"\\.\pipe\aam-{hash:x}")
}

/// `attributes`가 가리키는 보안 설명자를 파이프 생성이 끝날 때까지 살려 둔다.
pub struct PipeSecurity {
    _sd: SecurityDescriptor,
    attributes: SECURITY_ATTRIBUTES,
}

impl PipeSecurity {
    pub fn owner_only() -> io::Result<Self> {
        let sd = owner_only_descriptor()?;
        let attributes = security_attributes(&sd);
        Ok(Self { _sd: sd, attributes })
    }

    fn attributes_ptr(&self) -> *mut SECURITY_ATTRIBUTES {
        &self.attributes as *const SECURITY_ATTRIBUTES as *mut SECURITY_ATTRIBUTES
    }
}

pub fn create_pipe_instance(name: &str, first: bool, nonblocking: bool) -> io::Result<OwnedHandle> {
    let security = PipeSecurity::owner_only()?;
    let wide_name = wide(name);
    let mut open_mode = PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED;
    if first {
        open_mode |= FILE_FLAG_FIRST_PIPE_INSTANCE;
    }
    let mut mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_REJECT_REMOTE_CLIENTS;
    mode |= if nonblocking { PIPE_NOWAIT } else { PIPE_WAIT };
    let handle = unsafe {
        CreateNamedPipeW(
            wide_name.as_ptr(),
            open_mode,
            mode,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            security.attributes_ptr(),
        )
    };
    if handle == INVALID_HANDLE_VALUE || handle.is_null() {
        return Err(io_error());
    }
    let _ = security;
    Ok(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) })
}

pub fn open_pipe_client(name: &str) -> io::Result<OwnedHandle> {
    let wide_name = wide(name);
    for _ in 0..20 {
        let handle = unsafe {
            CreateFileW(
                wide_name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE && !handle.is_null() {
            return Ok(unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) });
        }
        if unsafe { GetLastError() } != ERROR_PIPE_BUSY {
            return Err(io_error());
        }
        unsafe {
            WaitNamedPipeW(wide_name.as_ptr(), 200);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::ConnectionRefused,
        "named pipe가 바쁩니다",
    ))
}

/// 파이프 서버(서비스) 프로세스 ID. 연결한 뒤 상대가 현재 사용자인지 확인한 경우에만 돌려준다.
pub fn pipe_server_pid(name: &str) -> io::Result<u32> {
    let handle = open_pipe_client(name)?;
    let mut pid = 0u32;
    if unsafe { GetNamedPipeServerProcessId(handle.as_raw_handle() as HANDLE, &mut pid) } == 0 || pid == 0 {
        return Err(io_error());
    }
    if !same_user_process(pid) {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "서비스가 현재 사용자 프로세스가 아닙니다"));
    }
    Ok(pid)
}

pub fn same_user_process(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return false;
    }
    let process = unsafe { OwnedHandle::from_raw_handle(process as RawHandle) };
    let Ok(peer) = token_user(process.as_raw_handle() as HANDLE) else {
        return false;
    };
    let Ok(current) = UserSid::current() else {
        return false;
    };
    (unsafe { EqualSid(peer.sid(), current.sid()) }) != 0
}

/// 서버 쪽: 클라이언트 프로세스를 여는 대신 파이프 가장(impersonation)으로 클라이언트 토큰을 직접 읽는다.
/// 일반 권한 서비스는 관리자 권한(상승) 클라이언트의 프로세스 토큰을 열 수 없어, 같은 사용자라도 거부되던 문제를 피한다.
fn pipe_client_is_self(handle: &OwnedHandle) -> bool {
    use windows_sys::Win32::{
        Security::RevertToSelf,
        System::{Pipes::ImpersonateNamedPipeClient, Threading::{GetCurrentThread, OpenThreadToken}},
    };
    if unsafe { ImpersonateNamedPipeClient(handle.as_raw_handle() as HANDLE) } == 0 {
        return false;
    }
    let mut token: HANDLE = ptr::null_mut();
    let opened = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 1, &mut token) } != 0;
    // 가장은 토큰을 연 직후 되돌린다. 되돌리지 못하면 이 스레드를 계속 쓰면 안 되므로 프로세스를 끝낸다.
    if unsafe { RevertToSelf() } == 0 {
        std::process::abort();
    }
    if !opened {
        return false;
    }
    let token = unsafe { OwnedHandle::from_raw_handle(token as RawHandle) };
    let Ok(peer) = token_sid_from(token.as_raw_handle() as HANDLE) else {
        return false;
    };
    let Ok(current) = UserSid::current() else {
        return false;
    };
    (unsafe { EqualSid(peer.sid(), current.sid()) }) != 0
}

pub fn pipe_peer_is_self(handle: &OwnedHandle, server_end: bool) -> bool {
    if server_end {
        return pipe_client_is_self(handle);
    }
    let mut pid = 0u32;
    let ok = unsafe {
        if server_end {
            GetNamedPipeClientProcessId(handle.as_raw_handle() as HANDLE, &mut pid)
        } else {
            GetNamedPipeServerProcessId(handle.as_raw_handle() as HANDLE, &mut pid)
        }
    };
    ok != 0 && same_user_process(pid)
}

pub fn overlapped_transfer(
    handle: &OwnedHandle,
    buf: &mut [u8],
    write: bool,
    timeout: Option<std::time::Duration>,
) -> io::Result<usize> {
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        return Err(io_error());
    }
    let event = unsafe { OwnedHandle::from_raw_handle(event as RawHandle) };
    let mut overlapped = unsafe { std::mem::zeroed::<OVERLAPPED>() };
    overlapped.hEvent = event.as_raw_handle() as HANDLE;
    let mut transferred = 0u32;
    let ok = unsafe {
        if write {
            WriteFile(
                handle.as_raw_handle() as HANDLE,
                buf.as_ptr().cast(),
                buf.len() as u32,
                &mut transferred,
                &mut overlapped,
            )
        } else {
            ReadFile(
                handle.as_raw_handle() as HANDLE,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
                &mut transferred,
                &mut overlapped,
            )
        }
    };
    if ok == 0 {
        let err = unsafe { GetLastError() };
        if err != ERROR_IO_PENDING {
            if !write && (err == ERROR_BROKEN_PIPE || err == ERROR_NO_DATA) {
                return Ok(0);
            }
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        let wait_ms = timeout.map(|d| d.as_millis().min(u128::from(u32::MAX - 1)) as u32).unwrap_or(INFINITE);
        let wait = unsafe { WaitForSingleObject(event.as_raw_handle() as HANDLE, wait_ms) };
        if wait == WAIT_TIMEOUT {
            unsafe {
                CancelIoEx(handle.as_raw_handle() as HANDLE, &overlapped);
                WaitForSingleObject(event.as_raw_handle() as HANDLE, INFINITE);
            }
            return Err(io::Error::new(io::ErrorKind::TimedOut, "IPC 시간이 초과되었습니다"));
        }
        if wait != WAIT_OBJECT_0 {
            return Err(io_error());
        }
    }
    if unsafe {
        GetOverlappedResult(
            handle.as_raw_handle() as HANDLE,
            &overlapped,
            &mut transferred,
            0,
        )
    } == 0
    {
        let err = unsafe { GetLastError() };
        if !write && (err == ERROR_BROKEN_PIPE || err == ERROR_NO_DATA) {
            return Ok(0);
        }
        return Err(io::Error::from_raw_os_error(err as i32));
    }
    Ok(transferred as usize)
}

pub fn creation_filetime(pid: u32) -> Result<u64, super::ApiError> {
    if pid == 0 {
        return Err(super::ApiError::new(
            "PROCESS_NOT_FOUND",
            "유효하지 않은 프로세스 ID입니다",
        ));
    }
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        let err = unsafe { GetLastError() };
        let code = if err == ERROR_INVALID_PARAMETER || err == ERROR_NOT_FOUND {
            "PROCESS_NOT_FOUND"
        } else if err == ERROR_ACCESS_DENIED {
            "PROCESS_INSPECTION_FAILED"
        } else {
            "PROCESS_INSPECTION_FAILED"
        };
        return Err(super::ApiError::new(
            code,
            "프로세스 시작 시각을 확인하지 못했습니다",
        ));
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) };
    let mut exit = 0u32;
    if unsafe { GetExitCodeProcess(handle.as_raw_handle() as HANDLE, &mut exit) } == 0 {
        return Err(super::ApiError::new(
            "PROCESS_INSPECTION_FAILED",
            "프로세스 시작 시각을 확인하지 못했습니다",
        ));
    }
    if exit != STILL_ACTIVE {
        return Err(super::ApiError::new(
            "PROCESS_NOT_FOUND",
            "프로세스가 종료되었습니다",
        ));
    }
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut creation, mut exit_time, mut kernel, mut user) = (zero, zero, zero, zero);
    if unsafe {
        GetProcessTimes(
            handle.as_raw_handle() as HANDLE,
            &mut creation,
            &mut exit_time,
            &mut kernel,
            &mut user,
        )
    } == 0
    {
        return Err(super::ApiError::new(
            "PROCESS_INSPECTION_FAILED",
            "프로세스 시작 시각을 확인하지 못했습니다",
        ));
    }
    let value = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    if value == 0 {
        return Err(super::ApiError::new(
            "PROCESS_INSPECTION_FAILED",
            "프로세스 시작 시각을 확인하지 못했습니다",
        ));
    }
    Ok(value)
}

/// PrefetchParameters\BootId. 재부팅 검증 전이므로 슬롯 반환에는 쓰지 않는다.
pub fn boot_id_candidate() -> String {
    let key_name = wide(
        r"SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters",
    );
    let mut key: HKEY = ptr::null_mut();
    if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, key_name.as_ptr(), 0, KEY_READ, &mut key) } != 0 {
        return "windows-boot-unverified".into();
    }
    let value_name = wide("BootId");
    let mut kind = 0u32;
    let mut data = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegQueryValueExW(
            key,
            value_name.as_ptr(),
            ptr::null_mut(),
            &mut kind,
            &mut data as *mut u32 as *mut u8,
            &mut size,
        )
    };
    unsafe {
        RegCloseKey(key);
    }
    if status != 0 || kind != REG_DWORD || size != 4 {
        return "windows-boot-unverified".into();
    }
    // 값은 저장만 한다. all_from_previous_boot는 이 값으로 반환하지 않는다.
    format!("bootid:{data}")
}

pub fn process_parent(pid: u32) -> Result<u32, super::ApiError> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == INVALID_HANDLE_VALUE || snap.is_null() {
        return Err(super::ApiError::new(
            "PROCESS_INSPECTION_FAILED",
            "프로세스 부모를 확인하지 못했습니다",
        ));
    }
    let snap = unsafe { OwnedHandle::from_raw_handle(snap as RawHandle) };
    let mut entry = unsafe { std::mem::zeroed::<PROCESSENTRY32W>() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    if unsafe { Process32FirstW(snap.as_raw_handle() as HANDLE, &mut entry) } == 0 {
        return Err(super::ApiError::new(
            "PROCESS_INSPECTION_FAILED",
            "프로세스 부모를 확인하지 못했습니다",
        ));
    }
    loop {
        if entry.th32ProcessID == pid {
            let parent = entry.th32ParentProcessID;
            return if parent == 0 {
                Err(super::ApiError::new(
                    "PROCESS_NOT_FOUND",
                    "프로세스 부모가 없습니다",
                ))
            } else {
                Ok(parent)
            };
        }
        if unsafe { Process32NextW(snap.as_raw_handle() as HANDLE, &mut entry) } == 0 {
            break;
        }
    }
    Err(super::ApiError::new(
        "PROCESS_NOT_FOUND",
        "프로세스를 찾지 못했습니다",
    ))
}

pub struct ProcessJob {
    handle: OwnedHandle,
}

impl ProcessJob {
    /// `name`이 있으면 소유자 전용 보안 설명자로 이름 있는 Job을 만든다. 같은 이름이 이미 있으면 실패한다
    /// (다른 프로세스가 미리 만든 Job에 넣지 않는다).
    pub fn new(kill_on_close: bool, name: Option<&str>) -> io::Result<Self> {
        let security = match name {
            Some(_) => Some(PipeSecurity::owner_only()?),
            None => None,
        };
        let wide_name = name.map(wide);
        let handle = unsafe {
            CreateJobObjectW(
                security.as_ref().map_or(ptr::null(), |s| s.attributes_ptr() as *const _),
                wide_name.as_ref().map_or(ptr::null(), |n| n.as_ptr()),
            )
        };
        if !handle.is_null() && name.is_some() && unsafe { GetLastError() } == 183 {
            // ERROR_ALREADY_EXISTS: 우리가 만든 Job이 아니다.
            unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) };
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "같은 이름의 Job이 이미 있습니다"));
        }
        if handle.is_null() {
            return Err(io_error());
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) };
        if kill_on_close {
            let mut info = unsafe { std::mem::zeroed::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if unsafe {
                SetInformationJobObject(
                    handle.as_raw_handle() as HANDLE,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            } == 0
            {
                return Err(io_error());
            }
        }
        Ok(Self { handle })
    }

    /// 실행기가 만든 이름 있는 Job을 조회 권한으로 연다. 이름은 마지막 핸들이 닫히면 사라지므로
    /// 실행기가 살아 있는 동안(lease.started) 열어 두어야 한다.
    pub fn open(name: &str) -> io::Result<Self> {
        const JOB_OBJECT_QUERY: u32 = 0x0004;
        let wide_name = wide(name);
        let handle = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, 0, wide_name.as_ptr()) };
        if handle.is_null() {
            return Err(io_error());
        }
        Ok(Self {
            handle: unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) },
        })
    }

    /// Job 안에서 아직 실행 중인 프로세스 수. 자손은 Job을 벗어날 수 없으므로 0은 전체 종료의 근거다.
    pub fn active_processes(&self) -> io::Result<u32> {
        let mut info = unsafe { std::mem::zeroed::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() };
        let mut returned = 0u32;
        if unsafe {
            QueryInformationJobObject(
                self.handle.as_raw_handle() as HANDLE,
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                &mut returned,
            )
        } == 0
        {
            return Err(io_error());
        }
        Ok(info.ActiveProcesses)
    }

    pub fn assign_raw(&self, process: RawHandle) -> io::Result<()> {
        if unsafe { AssignProcessToJobObject(self.handle.as_raw_handle() as HANDLE, process as HANDLE) }
            == 0
        {
            return Err(io_error());
        }
        Ok(())
    }

    pub fn pids(&self) -> io::Result<Vec<u32>> {
        // 고정 배열 1개짜리 타입 대신 직접 버퍼를 쓴다. 잘린 목록은 불확실로 본다.
        #[repr(C)]
        struct List {
            assigned: u32,
            count: u32,
            ids: [usize; 2048],
        }
        let mut buffer = List {
            assigned: 0,
            count: 0,
            ids: [0; 2048],
        };
        let mut returned = 0u32;
        if unsafe {
            QueryInformationJobObject(
                self.handle.as_raw_handle() as HANDLE,
                JobObjectBasicProcessIdList,
                &mut buffer as *mut _ as *mut c_void,
                std::mem::size_of::<List>() as u32,
                &mut returned,
            )
        } == 0
        {
            return Err(io_error());
        }
        if buffer.assigned > buffer.count || buffer.count as usize > buffer.ids.len() {
            return Err(io::Error::other("Job Object 프로세스 목록이 잘렸습니다"));
        }
        Ok(buffer.ids[..buffer.count as usize]
            .iter()
            .map(|pid| *pid as u32)
            .filter(|pid| *pid != 0)
            .collect())
    }
}

pub struct PendingConnect {
    handle: OwnedHandle,
    event: OwnedHandle,
    overlapped: Box<OVERLAPPED>,
    waiting: bool,
}

impl PendingConnect {
    pub fn begin(name: &str, first: bool) -> io::Result<Self> {
        let handle = create_pipe_instance(name, first, false)?;
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            return Err(io_error());
        }
        let event = unsafe { OwnedHandle::from_raw_handle(event as RawHandle) };
        let mut overlapped = Box::new(unsafe { std::mem::zeroed::<OVERLAPPED>() });
        overlapped.hEvent = event.as_raw_handle() as HANDLE;
        let ok = unsafe { ConnectNamedPipe(handle.as_raw_handle() as HANDLE, overlapped.as_mut()) };
        let err = unsafe { GetLastError() };
        let waiting = ok == 0 && err == ERROR_IO_PENDING;
        if ok == 0 && err != ERROR_IO_PENDING && err != ERROR_PIPE_CONNECTED {
            return Err(io::Error::from_raw_os_error(err as i32));
        }
        Ok(Self {
            handle,
            event,
            overlapped,
            waiting,
        })
    }

    /// None은 아직 클라이언트가 없다는 뜻이다. OVERLAPPED를 옮기지 않는다.
    pub fn poll(&mut self, timeout_ms: u32) -> io::Result<Option<()>> {
        if !self.waiting {
            return Ok(Some(()));
        }
        let wait = unsafe { WaitForSingleObject(self.event.as_raw_handle() as HANDLE, timeout_ms) };
        if wait == WAIT_TIMEOUT {
            return Ok(None);
        }
        if wait != WAIT_OBJECT_0 {
            return Err(io_error());
        }
        let mut transferred = 0u32;
        if unsafe {
            GetOverlappedResult(
                self.handle.as_raw_handle() as HANDLE,
                self.overlapped.as_mut(),
                &mut transferred,
                0,
            )
        } == 0
        {
            return Err(io_error());
        }
        self.waiting = false;
        Ok(Some(()))
    }

    pub fn into_handle(mut self) -> OwnedHandle {
        // 연결이 끝난 인스턴스만 넘긴다(poll이 Some을 돌려준 뒤). Drop이 취소하지 않도록 표시한다.
        self.waiting = false;
        let placeholder = unsafe { OwnedHandle::from_raw_handle(INVALID_HANDLE_VALUE as RawHandle) };
        std::mem::replace(&mut self.handle, placeholder)
    }
}

// OVERLAPPED의 hEvent 원시 포인터 때문에 자동 Send가 아니다. 핸들과 Box 고정 OVERLAPPED를 모두 소유하고,
// 리스너의 Mutex 안에서만 접근하므로 스레드를 옮겨도 안전하다.
unsafe impl Send for PendingConnect {}

impl Drop for PendingConnect {
    fn drop(&mut self) {
        // 대기 중인 ConnectNamedPipe가 OVERLAPPED에 쓸 수 있으므로, 취소하고 완료를 기다린 뒤에 해제한다.
        if self.waiting {
            let mut transferred = 0u32;
            unsafe {
                CancelIoEx(self.handle.as_raw_handle() as HANDLE, self.overlapped.as_ref());
                GetOverlappedResult(
                    self.handle.as_raw_handle() as HANDLE,
                    self.overlapped.as_ref(),
                    &mut transferred,
                    1,
                );
            }
        }
    }
}

#[link(name = "ntdll")]
extern "system" {
    fn NtResumeProcess(process: HANDLE) -> i32;
}

/// 일시 정지로 시작해 Job에 넣은 뒤 재개한다. `new_group`은 Ctrl+C를 받지 않는 새 콘솔 그룹의 백그라운드 자식용이다.
/// 이런 자식은 입출력을 모두 돌려받으므로 콘솔 창을 만들지 않는다(창 없는 서비스가 띄우면 새 터미널 창이 열린다).
/// `named`이면 Job 이름을 native identity에서 만들어(`job_name`), 실행기가 끝난 뒤에도 서비스가 종료를 확인할 수 있게 한다.
pub fn spawn_in_job(
    command: &mut std::process::Command,
    kill_on_close: bool,
    new_group: bool,
    named: bool,
) -> io::Result<(std::process::Child, ProcessJob)> {
    use std::os::windows::process::CommandExt;
    const CREATE_SUSPENDED: u32 = 0x4;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_SUSPENDED | if new_group { CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW } else { 0 });
    let mut child = command.spawn()?;
    let name = if named {
        match creation_filetime(child.id()) {
            Ok(started) => Some(job_name(child.id(), &started.to_string())),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::other("자식 프로세스 identity를 확인하지 못했습니다"));
            }
        }
    } else {
        None
    };
    let job = match ProcessJob::new(kill_on_close, name.as_deref()) {
        Ok(job) => job,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
    };
    // std::Command는 주 스레드 핸들을 주지 않는다. 일시 정지 상태로 만든 뒤 Job에 넣고
    // NtResumeProcess로 재개해야 손자가 Job 밖에서 생기지 않는다.
    if let Err(error) = job.assign_raw(child.as_raw_handle()) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let status = unsafe { NtResumeProcess(child.as_raw_handle() as HANDLE) };
    if status < 0 {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other("일시 정지된 프로세스를 재개하지 못했습니다"));
    }
    Ok((child, job))
}

pub fn handle_owned_by_us(file: &std::fs::File) -> io::Result<bool> {
    let mut owner = ptr::null_mut();
    let mut sd = ptr::null_mut();
    let status = unsafe {
        GetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            &mut sd,
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    let same = owner_is_me(owner);
    if !sd.is_null() {
        unsafe {
            LocalFree(sd);
        }
    }
    Ok(same)
}

pub fn lock_down_handle(file: &std::fs::File) -> io::Result<()> {
    let sd = owner_only_descriptor()?;
    let mut present = 0i32;
    let mut dacl = ptr::null_mut();
    let mut defaulted = 0i32;
    if unsafe { GetSecurityDescriptorDacl(sd.raw, &mut present, &mut dacl, &mut defaulted) } == 0
        || present == 0
        || dacl.is_null()
    {
        return Err(io::Error::other("소유자 전용 ACL을 만들지 못했습니다"));
    }
    let user = UserSid::current()?;
    let status = unsafe {
        SetSecurityInfo(
            file.as_raw_handle() as HANDLE,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION
                | DACL_SECURITY_INFORMATION
                | PROTECTED_DACL_SECURITY_INFORMATION,
            user.sid(),
            ptr::null_mut(),
            dacl,
            ptr::null_mut(),
        )
    };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status as i32));
    }
    Ok(())
}

/// native 프로세스 identity(pid + 생성 시각)로 만드는 Job 이름. 실행기와 서비스가 같은 규칙으로 계산한다.
pub fn job_name(pid: u32, started_at: &str) -> String {
    let safe: String = started_at.chars().filter(char::is_ascii_digit).collect();
    format!(r"Local\aam-job-{pid}-{safe}")
}
