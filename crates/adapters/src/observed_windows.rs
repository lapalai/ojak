//! Read-only Windows OMP observation: exact executable, token owner, creation identity,
//! and positively observed writable disk handles. Never inspect argv, env or the PEB.
use super::*;
use aam_protocol::{winutil, ApiError};
use std::{ffi::{c_void, OsString}, fs::File, mem::size_of, os::windows::{ffi::OsStringExt, io::{AsRawHandle, FromRawHandle, OwnedHandle}}, ptr};
use windows_sys::Win32::{
    Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, FILETIME, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{GetFileType, GetFinalPathNameByHandleW, FILE_TYPE_DISK, FILE_WRITE_DATA, FILE_APPEND_DATA},
    System::{Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS},
        Threading::{GetCurrentProcess, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_DUP_HANDLE, PROCESS_QUERY_INFORMATION}},
};

#[link(name = "ntdll")]
extern "system" {
    fn NtQueryInformationProcess(process: HANDLE, class: u32, buffer: *mut c_void, length: u32, returned: *mut u32) -> i32;
}
#[repr(C)]
struct BasicInformation {
    exit_status: usize,
    peb: usize,
    affinity: usize,
    priority: usize,
    pid: usize,
    parent: usize,
}
#[repr(C)]
struct HandleEntry {
    value: HANDLE,
    handle_count: usize,
    pointer_count: usize,
    access: u32,
    object_type: u32,
    attributes: u32,
    reserved: u32,
}
fn failed() -> ApiError { ApiError::new("PROCESS_INSPECTION_FAILED", "프로세스 관측 근거를 확인하지 못했습니다.") }
pub(super) fn uid() -> u32 { 0 } // Only same-user records enter this platform's record set.
fn process(pid: u32, access: u32) -> Result<OwnedHandle, ApiError> {
    if !winutil::same_user_process(pid) { return Err(failed()); }
    let raw = unsafe { OpenProcess(access, 0, pid) };
    if raw.is_null() { return Err(failed()); }
    Ok(unsafe { OwnedHandle::from_raw_handle(raw) })
}
pub(super) fn record(pid: u32, boot: &str) -> Result<Record, ApiError> {
    let process = process(pid, PROCESS_QUERY_INFORMATION)?;
    let handle = process.as_raw_handle();
    let mut basic = unsafe { std::mem::zeroed::<BasicInformation>() };
    if unsafe { NtQueryInformationProcess(handle, 0, (&mut basic as *mut BasicInformation).cast(), size_of::<BasicInformation>() as u32, ptr::null_mut()) } < 0 || basic.pid != pid as usize {
        return Err(failed());
    }
    let (mut created, mut exited, mut kernel, mut user): (FILETIME, FILETIME, FILETIME, FILETIME) = unsafe { std::mem::zeroed() };
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 || exited.dwHighDateTime != 0 || exited.dwLowDateTime != 0 {
        return Err(failed());
    }
    let mut name = [0u16; 32768];
    let mut length = name.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle, 0, name.as_mut_ptr(), &mut length) } == 0 { return Err(failed()); }
    let executable = PathBuf::from(OsString::from_wide(&name[..length as usize])).canonicalize().map_err(|_| failed())?;
    let started = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    Ok(Record { pid, parent_pid: u32::try_from(basic.parent).map_err(|_| failed())?, uid: 0, real_uid: 0,
        identity: Some(ProcessIdentity { pid, started_at: started.to_string(), boot_id: boot.into() }),
        executable: Some(executable), cwd: None, terminal: false })
}

pub(super) fn writers(pid: u32) -> (Vec<crate::observed_metadata::WriterFile>, bool) {
    use crate::observed_metadata::{WriterFile, MAX_FILES};
    const MAX_HANDLES: usize = 4096;
    let Ok(process) = process(pid, PROCESS_QUERY_INFORMATION | PROCESS_DUP_HANDLE) else { return (Vec::new(), true); };
    let header = 2 * size_of::<usize>();
    let cap = header + MAX_HANDLES * size_of::<HandleEntry>();
    // usize storage provides native pointer alignment for the variable-length NT structure.
    let mut storage = vec![0usize; (header + 256 * size_of::<HandleEntry>()).div_ceil(size_of::<usize>())];
    let mut returned = 0u32;
    let mut status = unsafe { NtQueryInformationProcess(process.as_raw_handle(), 51, storage.as_mut_ptr().cast(), (storage.len() * size_of::<usize>()) as u32, &mut returned) };
    if status < 0 && returned as usize > storage.len() * size_of::<usize>() && returned as usize <= cap {
        storage.resize((returned as usize).div_ceil(size_of::<usize>()), 0);
        status = unsafe { NtQueryInformationProcess(process.as_raw_handle(), 51, storage.as_mut_ptr().cast(), (storage.len() * size_of::<usize>()) as u32, &mut returned) };
    }
    if status < 0 || (returned as usize) < header { return (Vec::new(), true); }
    let count = storage[0];
    if count > MAX_HANDLES || header + count * size_of::<HandleEntry>() > returned as usize || returned as usize > storage.len() * size_of::<usize>() { return (Vec::new(), true); }
    let entries = unsafe { std::slice::from_raw_parts(storage.as_ptr().cast::<u8>().add(header).cast::<HandleEntry>(), count) };
    let deadline = Instant::now() + Duration::from_millis(250);
    let mut files = Vec::new();
    let mut partial = false;
    let mut path = [0u16; 32768];
    for entry in entries.iter().filter(|entry| entry.access & (FILE_WRITE_DATA | FILE_APPEND_DATA) != 0) {
        if files.len() == MAX_FILES || Instant::now() >= deadline { partial = true; break; }
        let mut duplicate = ptr::null_mut();
        if unsafe { DuplicateHandle(process.as_raw_handle(), entry.value, GetCurrentProcess(), &mut duplicate, 0, 0, DUPLICATE_SAME_ACCESS) } == 0 { partial = true; continue; }
        let file = unsafe { File::from_raw_handle(duplicate) };
        if unsafe { GetFileType(file.as_raw_handle()) } != FILE_TYPE_DISK { continue; }
        let length = unsafe { GetFinalPathNameByHandleW(file.as_raw_handle(), path.as_mut_ptr(), path.len() as u32, 0) } as usize;
        if length == 0 || length >= path.len() { partial = true; continue; }
        let name = PathBuf::from(OsString::from_wide(&path[..length]));
        if name.extension().is_none_or(|ext| !ext.eq_ignore_ascii_case("jsonl")) { continue; }
        if !file.metadata().is_ok_and(|stat| stat.is_file()) || !winutil::single_link_regular_file(&file).unwrap_or(false) || !winutil::handle_access_is_safe(&file, false).unwrap_or(false) {
            partial = true; continue;
        }
        let Ok((device, inode)) = winutil::file_identity(&file) else { partial = true; continue; };
        if !files.iter().any(|other: &WriterFile| other.device == device && other.inode == inode) {
            files.push(WriterFile { path: name, device, inode });
        }
    }
    (files, partial)
}
pub(crate) fn open_jsonl_writers(pid: u32) -> Vec<PathBuf> { writers(pid).0.into_iter().map(|file| file.path).collect() }

pub(super) fn scan(binary: &Path, orca_paths: &[PathBuf], paths: &Paths, accounts: &[Account]) -> ObservedScan {
    let mut result = ObservedScan::default();
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw == INVALID_HANDLE_VALUE || raw.is_null() { result.incomplete(); return result; }
    let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut entry = unsafe { std::mem::zeroed::<PROCESSENTRY32W>() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    if unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) } == 0 { result.incomplete(); return result; }
    let boot = winutil::boot_id_candidate();
    let deadline = Instant::now() + Duration::from_millis(750);
    let mut records = BTreeMap::new();
    let mut partial = false;
    loop {
        if Instant::now() >= deadline || records.len() >= 8192 { partial = true; break; }
        let pid = entry.th32ProcessID;
        // Foreign/system processes are outside our observation scope, not failed sessions.
        if let Ok(item) = record(pid, &boot) { records.insert(pid, item); }
        if unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) } == 0 { break; }
    }
    result = project(&records, binary, orca_paths, uid(), |old| {
        record(old.pid, &boot).is_ok_and(|fresh| fresh.identity == old.identity && fresh.parent_pid == old.parent_pid && fresh.executable == old.executable)
            && records.get(&old.parent_pid).is_none_or(|parent| {
                let birth = |item: &Record| item.identity.as_ref().and_then(|id| id.started_at.parse::<u64>().ok());
                matches!((birth(parent), birth(old)), (Some(parent), Some(child)) if parent <= child)
            })
    });
    if partial { result.incomplete(); }
    enrich(&mut result, &records, binary, paths, accounts);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_open_writable_single_link_files_are_observed() {
        let directory = std::env::temp_dir().join(format!("aam-writers-{}", aam_protocol::new_id()));
        aam_protocol::secure::restrict_dir(&directory).unwrap();
        let writable = directory.join("writable.jsonl");
        let readonly = directory.join("readonly.jsonl");
        std::fs::write(&readonly, b"read-only").unwrap();
        let read_handle = File::open(&readonly).unwrap();
        let write_handle = std::fs::OpenOptions::new().create_new(true).append(true).open(&writable).unwrap();
        let expected = winutil::file_identity(&write_handle).unwrap();
        let read_id = winutil::file_identity(&read_handle).unwrap();
        let (files, _) = writers(std::process::id());
        assert!(files.iter().any(|file| (file.device, file.inode) == expected));
        assert!(!files.iter().any(|file| (file.device, file.inode) == read_id));
        std::fs::hard_link(&writable, directory.join("alias.jsonl")).unwrap();
        assert!(!writers(std::process::id()).0.iter().any(|file| (file.device, file.inode) == expected));
        drop(write_handle);
        drop(read_handle);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
