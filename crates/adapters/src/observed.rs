#![cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
use aam_protocol::{Account, Notice, ObservedSession, Paths, ProcessIdentity};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};
#[cfg(any(target_os = "macos", windows))]
use std::time::{Duration, Instant};

const MAX_ANCESTORS: usize = 64;
const MAX_SESSIONS: usize = 256;
const UNKNOWN_IDENTITY: &str = "검증된 OMP 프로세스가 쓰는 세션 파일의 구조 메타데이터와 선택적 확장 관측만 조회합니다. 저장 pin·세션 선택은 개별 요청 계정의 확정이 아니며 API 키 우선순위·동시 호출·재시도 계정은 미확인입니다. 현재 분기는 확장 근거가 있을 때만 구분하고 관측에 예약·전환·재개 권한을 부여하지 않습니다.";

#[derive(Default)]
pub struct ObservedScan {
    pub sessions: Vec<ObservedSession>,
    pub notices: Vec<Notice>,
}
impl ObservedScan {
    fn incomplete(&mut self) {
        if self.notices.is_empty() {
            self.notices.push(Notice {
                id: "observed-session-discovery".into(),
                level: "warning".into(),
                title: "외부 세션 관측 불완전".into(),
                message: "일부 실행 파일·프로세스·상위 경로를 확인하지 못했거나 조회 한도에 도달했습니다. 표시된 목록은 확인된 항목뿐이며, 0개도 실행 중인 작업이 없다는 뜻은 아닙니다. 다음 상태 조회에서 다시 확인합니다.".into(),
            });
        }
    }
}

#[derive(Clone)]
struct Record {
    pid: u32,
    parent_pid: u32,
    uid: u32,
    real_uid: u32,
    identity: Option<ProcessIdentity>,
    executable: Option<PathBuf>,
    cwd: Option<String>,
    terminal: bool,
}

fn project(
    records: &BTreeMap<u32, Record>,
    binary: &Path,
    orca_paths: &[PathBuf],
    uid: u32,
    mut current: impl FnMut(&Record) -> bool,
) -> ObservedScan {
    let mut result = ObservedScan::default();
    let mut checked = BTreeMap::new();
    let mut valid = |record: &Record| *checked.entry(record.pid).or_insert_with(|| current(record));
    for record in records.values().filter(|record| {
        record.uid == uid
            && record.real_uid == uid
            && record.identity.is_some()
            && record.executable.as_deref() == Some(binary)
    }) {
        if !valid(record) {
            continue;
        }
        let mut seen = BTreeSet::from([record.pid]);
        let mut parent_pid = record.parent_pid;
        let mut parent_process = None;
        let mut host = if record.terminal {
            "terminal"
        } else {
            "unknown"
        };
        let mut nested = false;
        for depth in 0..MAX_ANCESTORS {
            if parent_pid <= 1 {
                break;
            }
            if !seen.insert(parent_pid) {
                result.incomplete();
                break;
            }
            let Some(parent) = records.get(&parent_pid).filter(|parent| valid(parent)) else {
                result.incomplete();
                break;
            };
            if depth == 0 {
                parent_process = parent.identity.clone();
            }
            // 검증된 조상 관계로 하위 worker와 서비스 자신의 usage/version 조회를 제외합니다.
            if parent.uid == uid
                && parent.real_uid == uid
                && parent.identity.is_some()
                && (parent.pid == std::process::id()
                    || parent.executable.as_deref() == Some(binary))
            {
                nested = true;
                break;
            }
            if parent.uid == uid
                && parent.identity.is_some()
                && parent
                    .executable
                    .as_ref()
                    .is_some_and(|path| orca_paths.contains(path))
            {
                host = "orca";
            }
            parent_pid = parent.parent_pid;
            if depth + 1 == MAX_ANCESTORS && parent_pid > 1 {
                result.incomplete();
            }
        }
        if nested {
            continue;
        }
        if result.sessions.len() == MAX_SESSIONS {
            result.incomplete();
            break;
        }
        let identity = record.identity.as_ref().unwrap();
        result.sessions.push(ObservedSession {
            id: format!(
                "observed:omp:{}:{}:{}",
                identity.boot_id, identity.started_at, identity.pid
            ),
            tool: "omp".into(),
            process: identity.clone(),
            parent_process,
            host: host.into(),
            cwd: record.cwd.clone(),
            model: None,
            account_id: None,
            verification: "observed".into(),
            reason: Some(UNKNOWN_IDENTITY.into()),
            attributions: Vec::new(),
            native_session_id: None,
        });
    }
    result
}

/// 인증 저장소·argv/env·본문 필드를 수집하지 않는 읽기 전용 관측입니다.
pub fn discover_sessions(paths: &Paths, accounts: &[Account]) -> ObservedScan {
    let Some(binary) = crate::process::discover(paths, "omp").and_then(|entry| entry.canonicalize().ok()) else {
        let mut result = ObservedScan::default();
        result.incomplete();
        return result;
    };
    let mut orca_paths = vec![PathBuf::from("/Applications/Orca.app/Contents/MacOS/Orca")];
    if let Some(home) = aam_protocol::user_home() {
        orca_paths.push(PathBuf::from(home).join("Applications/Orca.app/Contents/MacOS/Orca"));
    }
    let orca_paths: Vec<_> = orca_paths
        .into_iter()
        .filter_map(|path| path.canonicalize().ok())
        .collect();
    native::scan(&binary, &orca_paths, paths, accounts)
}


#[cfg(any(target_os = "macos", windows))]
    fn root_index(
        item: &Record,
        records: &BTreeMap<u32, Record>,
        sessions: &[ObservedSession],
        binary: &Path,
    ) -> Option<usize> {
        let mut current = item;
        let mut seen = BTreeSet::new();
        for _ in 0..MAX_ANCESTORS {
            if !seen.insert(current.pid) {
                return None;
            }
            let identity = current.identity.as_ref()?;
            let fresh = native::record(current.pid, &identity.boot_id).ok()?;
            if fresh.identity != current.identity
                || fresh.parent_pid != current.parent_pid
                || fresh.uid != current.uid
                || fresh.real_uid != current.real_uid
                || fresh.executable != current.executable
            {
                return None;
            }
            if current.pid == std::process::id() {
                return None;
            }
            if current.executable.as_deref() == Some(binary) {
                if let Some(index) = sessions
                    .iter()
                    .position(|session| &session.process == identity)
                {
                    return Some(index);
                }
            }
            current = records.get(&current.parent_pid)?;
        }
        None
    }

#[cfg(any(target_os = "macos", windows))]
    fn enrich(
        result: &mut ObservedScan,
        records: &BTreeMap<u32, Record>,
        binary: &Path,
        paths: &Paths,
        accounts: &[Account],
    ) {
        use crate::observed_metadata::{attribute, read_sessions, MAX_FILES};
        let deadline = Instant::now() + Duration::from_millis(750);
        let mut budget = 8 * 1024 * 1024;
        let uid = native::uid();
        let mut collected = Vec::new();
        let mut partial = false;
        for item in records.values().filter(|item| {
            item.uid == uid && item.real_uid == uid && item.executable.as_deref() == Some(binary)
        }) {
            if Instant::now() >= deadline || collected.len() >= MAX_FILES || budget == 0 {
                partial = true;
                break;
            }
            let Some(root) = root_index(item, records, &result.sessions, binary) else {
                continue;
            };
            let identity = item.identity.as_ref().unwrap();
            let (mut files, limited) = native::writers(item.pid);
            partial |= limited;
            if files.len() > MAX_FILES - collected.len() {
                files.truncate(MAX_FILES - collected.len());
                partial = true;
            }
            let (metadata, limited) = read_sessions(identity, &files, &mut budget);
            partial |= limited;
            let (fresh_files, limited) = native::writers(item.pid);
            partial |= limited;
            if aam_protocol::process_identity(item.pid).as_ref().ok() != Some(identity) {
                partial = true;
                continue;
            }
            for (file, session) in metadata {
                if fresh_files.iter().any(|fresh| {
                    fresh.path == file.path
                        && fresh.device == file.device
                        && fresh.inode == file.inode
                }) {
                    collected.push((root, identity.clone(), file, session));
                } else {
                    partial = true;
                }
            }
        }
        let mut remaining_rows = 256usize;
        let mut mains: Vec<(usize, PathBuf)> = Vec::new();
        for (root, identity, file, metadata) in &collected {
            let root_pid = result.sessions[*root].process.pid;
            let mut child = metadata;
            let mut seen = BTreeSet::from([file.path.as_path()]);
            let mut linked = identity.pid == root_pid;
            for _ in 0..MAX_FILES {
                let Some((_, owner, parent_file, ancestor)) =
                    collected.iter().find(|(index, _, file, session)| {
                        index == root
                            && (child.parent.as_ref() == Some(&file.path)
                                || child.parent_id.as_deref() == Some(session.id.as_str()))
                    })
                else {
                    break;
                };
                if !seen.insert(parent_file.path.as_path()) {
                    linked = false;
                    break;
                }
                linked |= owner.pid == root_pid;
                child = ancestor;
            }
            if !linked {
                partial = true;
                continue;
            }
            let role = if metadata.subagent || identity.pid != root_pid {
                "subagent"
            } else {
                "main"
            };
            let (mut rows, issue) = attribute(paths, identity, file, metadata, role, accounts);
            if aam_protocol::process_identity(identity.pid).as_ref().ok() != Some(identity) {
                partial = true;
                continue;
            }
            if rows.len() > remaining_rows {
                rows.truncate(remaining_rows);
                partial = true;
            }
            remaining_rows -= rows.len();
            if let Some(issue) = issue {
                result.sessions[*root].reason = Some(format!("{UNKNOWN_IDENTITY} {issue}"));
            }
            if metadata.incomplete {
                result.sessions[*root].reason = Some(format!("{UNKNOWN_IDENTITY} 파일이 기록 중이거나 파싱·읽기 한도에 도달하여 일부 저장 이력이 누락되었습니다."));
            }
            result.sessions[*root].attributions.extend(rows);
            if role == "main" {
                mains.push((*root, file.path.clone()));
            }
        }
        // 인계 대상은 부모로 참조되지 않고 다른 대화의 하위 폴더에도 없는 현재 대화 파일만 사용합니다.
        let parents: BTreeSet<&Path> = collected
            .iter()
            .filter_map(|(_, _, _, session)| session.parent.as_deref())
            .collect();
        // OMP는 확장·보조 세션을 상위 대화 파일 이름의 하위 폴더에 기록합니다.
        let nested = |root: usize, path: &Path| {
            mains.iter().any(|(other_root, other)| {
                *other_root == root && other != path && path.starts_with(other.with_extension(""))
            })
        };
        for index in 0..result.sessions.len() {
            let candidates: Vec<&PathBuf> = mains
                .iter()
                .filter(|(root, path)| {
                    *root == index && !parents.contains(path.as_path()) && !nested(index, path)
                })
                .map(|(_, path)| path)
                .collect();
            let mut current = candidates.into_iter();
            match (current.next(), current.next()) {
                (Some(path), None) => {
                    result.sessions[index].native_session_id =
                        path.to_str().map(str::to_owned);
                }
                (Some(_), Some(_)) => {
                    result.sessions[index].reason = Some(format!("{UNKNOWN_IDENTITY} 최상위 대화 파일이 여러 개로 관측되어 인계 대상을 확정하지 않았습니다."));
                }
                _ => (),
            }
        }
        // 메인·보조 모델은 행별로 표시합니다. 단일 최상위 값으로 현재 모델/계정을 추정하지 않습니다.
        if partial {
            result.incomplete();
        }
    }

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use aam_protocol::{process_identity, ApiError};
    use std::{
        mem::{size_of, MaybeUninit},
        os::unix::ffi::OsStringExt,
        time::{Duration, Instant},
    };

    const MAX_PIDS: usize = 16_384;
    const SCAN_LIMIT: Duration = Duration::from_millis(750);
    const PROC_UID_ONLY: u32 = 4;
    pub(super) fn uid() -> u32 { unsafe { libc::geteuid() } }

    fn info<T>(pid: u32, flavor: i32) -> Result<T, ApiError> {
        let mut value = MaybeUninit::<T>::zeroed();
        let size = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                flavor,
                0,
                value.as_mut_ptr().cast(),
                size_of::<T>() as i32,
            )
        };
        if size as usize != size_of::<T>() {
            let gone = matches!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH) | Some(libc::ENOENT)
            );
            return Err(ApiError::new(
                if gone {
                    "PROCESS_NOT_FOUND"
                } else {
                    "PROCESS_INSPECTION_FAILED"
                },
                "프로세스 메타데이터를 확인하지 못했습니다.",
            ));
        }
        Ok(unsafe { value.assume_init() })
    }

    fn executable(pid: u32) -> Option<PathBuf> {
        let mut bytes = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let count = unsafe {
            libc::proc_pidpath(pid as i32, bytes.as_mut_ptr().cast(), bytes.len() as u32)
        };
        if count <= 0 || count as usize >= bytes.len() {
            return None;
        }
        let end = bytes.iter().position(|byte| *byte == 0)?;
        let path = PathBuf::from(std::ffi::OsString::from_vec(bytes[..end].to_vec()));
        path.is_absolute().then_some(path)
    }

    pub(super) fn record(pid: u32, boot: &str) -> Result<Record, ApiError> {
        let data: libc::proc_bsdinfo = info(pid, libc::PROC_PIDTBSDINFO)?;
        if data.pbi_pid != pid || data.pbi_start_tvsec == 0 || data.pbi_status == libc::SZOMB {
            return Err(ApiError::new(
                "PROCESS_NOT_FOUND",
                "프로세스가 종료되었거나 식별 정보가 변경되었습니다.",
            ));
        }
        Ok(Record {
            pid,
            parent_pid: data.pbi_ppid,
            uid: data.pbi_uid,
            real_uid: data.pbi_ruid,
            identity: Some(ProcessIdentity {
                pid,
                started_at: format!("{}:{:06}", data.pbi_start_tvsec, data.pbi_start_tvusec),
                boot_id: boot.into(),
            }),
            executable: executable(pid),
            cwd: None,
            terminal: data.e_tdev != u32::MAX,
        })
    }

    // root 소유 login은 full BSD가 EPERM이어도 short BSD의 PPID는 공개됩니다.
    // birth identity가 없는 이 연결은 host 표시에만 사용하며 관측 세션/제어 근거로 삼지 않습니다.
    fn bridge(pid: u32, uid: u32) -> Result<Record, ApiError> {
        let data: libc::proc_bsdshortinfo = info(pid, libc::PROC_PIDT_SHORTBSDINFO)?;
        if data.pbsi_pid != pid || data.pbsi_uid == uid || data.pbsi_status == libc::SZOMB {
            return Err(ApiError::new(
                "PROCESS_INSPECTION_FAILED",
                "상위 프로세스 연결을 확인하지 못했습니다.",
            ));
        }
        Ok(Record {
            pid,
            parent_pid: data.pbsi_ppid,
            uid: data.pbsi_uid,
            real_uid: data.pbsi_ruid,
            identity: None,
            executable: None,
            cwd: None,
            terminal: false,
        })
    }

    fn cwd(pid: u32) -> Option<String> {
        let data: libc::proc_vnodepathinfo = info(pid, libc::PROC_PIDVNODEPATHINFO).ok()?;
        let bytes: Vec<u8> = data
            .pvi_cdir
            .vip_path
            .iter()
            .flatten()
            .map(|byte| *byte as u8)
            .take_while(|byte| *byte != 0)
            .collect();
        let path = String::from_utf8(bytes).ok()?;
        Path::new(&path).is_absolute().then_some(path)
    }

    // SDK sys/proc_info.h의 libproc ABI. libc는 이 두 구조체를 노출하지 않습니다.
    #[repr(C)]
    struct FileInfo {
        open_flags: u32,
        status: u32,
        offset: i64,
        file_type: i32,
        guard_flags: u32,
    }
    #[repr(C)]
    struct VnodeFdPath {
        file: FileInfo,
        vnode: libc::vnode_info_path,
    }
    /// 프로세스가 쓰기 모드로 연 `.jsonl` 파일 경로(현재 사용자 소유 일반 파일만).
    pub(crate) fn open_jsonl_writers(pid: u32) -> Vec<PathBuf> {
        writers(pid).0.into_iter().map(|writer| writer.path).collect()
    }
    pub(super) fn writers(pid: u32) -> (Vec<crate::observed_metadata::WriterFile>, bool) {
        use crate::observed_metadata::{WriterFile, MAX_FILES};
        const MAX_FDS: usize = 4096;
        let mut descriptors: Vec<libc::proc_fdinfo> = (0..MAX_FDS)
            .map(|_| libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0,
            })
            .collect();
        let capacity = descriptors.len() * size_of::<libc::proc_fdinfo>();
        let count = unsafe {
            libc::proc_pidinfo(
                pid as i32,
                libc::PROC_PIDLISTFDS,
                0,
                descriptors.as_mut_ptr().cast(),
                capacity as i32,
            )
        };
        if count <= 0
            || count as usize > capacity
            || !(count as usize).is_multiple_of(size_of::<libc::proc_fdinfo>())
        {
            return (Vec::new(), true);
        }
        let mut partial = count as usize == capacity;
        let mut files = BTreeMap::new();
        for fd in &descriptors[..count as usize / size_of::<libc::proc_fdinfo>()] {
            if fd.proc_fdtype != libc::PROX_FDTYPE_VNODE as u32 {
                continue;
            }
            let mut info = MaybeUninit::<VnodeFdPath>::zeroed();
            let count = unsafe {
                libc::proc_pidfdinfo(
                    pid as i32,
                    fd.proc_fd,
                    2,
                    info.as_mut_ptr().cast(),
                    size_of::<VnodeFdPath>() as i32,
                )
            };
            if count as usize != size_of::<VnodeFdPath>() {
                partial = true;
                continue;
            }
            let info = unsafe { info.assume_init() };
            // fi_openflags는 O_*가 아닌 커널 FREAD/FWRITE(2) 플래그입니다.
            if info.file.open_flags & 2 == 0 {
                continue;
            }
            let stat = &info.vnode.vip_vi.vi_stat;
            if stat.vst_uid != unsafe { libc::geteuid() }
                || stat.vst_mode & libc::S_IFMT != libc::S_IFREG
            {
                continue;
            }
            let bytes: Vec<u8> = info
                .vnode
                .vip_path
                .iter()
                .flatten()
                .map(|byte| *byte as u8)
                .take_while(|byte| *byte != 0)
                .collect();
            let path = PathBuf::from(std::ffi::OsString::from_vec(bytes));
            if !path.is_absolute()
                || path
                    .extension()
                    .is_none_or(|extension| extension != "jsonl")
            {
                continue;
            }
            if files.len() >= MAX_FILES {
                partial = true;
                break;
            }
            files.insert(
                path.clone(),
                WriterFile {
                    path,
                    device: stat.vst_dev as u64,
                    inode: stat.vst_ino,
                },
            );
        }
        (files.into_values().collect(), partial)
    }


    pub(super) fn scan(
        binary: &Path,
        orca_paths: &[PathBuf],
        paths: &Paths,
        accounts: &[Account],
    ) -> ObservedScan {
        let mut result = ObservedScan::default();
        let deadline = Instant::now() + SCAN_LIMIT;
        let uid = unsafe { libc::geteuid() };
        let Ok(own) = process_identity(std::process::id()) else {
            result.incomplete();
            return result;
        };
        let mut pids = vec![0i32; MAX_PIDS];
        let capacity = pids.len() * size_of::<i32>();
        let count = unsafe {
            libc::proc_listpids(
                PROC_UID_ONLY,
                uid,
                pids.as_mut_ptr().cast(),
                capacity as i32,
            )
        };
        if count <= 0
            || count as usize > capacity
            || !(count as usize).is_multiple_of(size_of::<i32>())
        {
            result.incomplete();
            return result;
        }
        let mut partial = count as usize == capacity;
        let mut records = BTreeMap::new();
        for &pid in &pids[..count as usize / size_of::<i32>()] {
            if Instant::now() >= deadline {
                partial = true;
                break;
            }
            if pid <= 0 {
                continue;
            }
            match record(pid as u32, &own.boot_id) {
                Ok(mut item) if item.uid == uid && item.real_uid == uid => {
                    partial |= item.executable.is_none();
                    if item.executable.as_deref() == Some(binary) {
                        item.cwd = cwd(item.pid);
                        partial |= item.cwd.is_none();
                    }
                    records.insert(item.pid, item);
                }
                Err(error) if error.code != "PROCESS_NOT_FOUND" => partial = true,
                _ => {}
            }
        }
        let candidates: Vec<_> = records
            .values()
            .filter(|item| item.executable.as_deref() == Some(binary))
            .map(|item| item.pid)
            .collect();
        for pid in candidates {
            let mut parent = records[&pid].parent_pid;
            let mut seen = BTreeSet::from([pid]);
            for _ in 0..MAX_ANCESTORS {
                if parent <= 1 || !seen.insert(parent) {
                    break;
                }
                if Instant::now() >= deadline {
                    partial = true;
                    break;
                }
                if let std::collections::btree_map::Entry::Vacant(entry) = records.entry(parent) {
                    match record(parent, &own.boot_id).or_else(|_| bridge(parent, uid)) {
                        Ok(item) => {
                            entry.insert(item);
                        }
                        Err(_) => {
                            partial = true;
                            break;
                        }
                    }
                }
                parent = records[&parent].parent_pid;
            }
        }
        result = project(&records, binary, orca_paths, uid, |item| {
            if Instant::now() >= deadline {
                partial = true;
                return false;
            }
            let fresh = if item.identity.is_some() {
                record(item.pid, &own.boot_id)
            } else {
                bridge(item.pid, uid)
            };
            match fresh {
                Ok(fresh) => {
                    fresh.identity == item.identity
                        && fresh.uid == item.uid
                        && fresh.real_uid == item.real_uid
                        && fresh.parent_pid == item.parent_pid
                        && fresh.executable == item.executable
                }
                Err(error) => {
                    partial |= error.code != "PROCESS_NOT_FOUND";
                    false
                }
            }
        });
        enrich(&mut result, &records, binary, paths, accounts);
        if partial {
            result.incomplete();
        }
        result
    }
}

#[cfg(windows)]
#[path = "observed_windows.rs"]
mod native;

#[cfg(not(any(target_os = "macos", windows)))]
mod native {
    use super::*;
    pub(super) fn scan(
        _binary: &Path,
        _orca_paths: &[PathBuf],
        _paths: &Paths,
        _accounts: &[Account],
    ) -> ObservedScan {
        let mut result = ObservedScan::default();
        result.incomplete();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(pid: u32, parent_pid: u32, executable: &str) -> Record {
        Record {
            pid,
            parent_pid,
            uid: 501,
            real_uid: 501,
            identity: Some(ProcessIdentity {
                pid,
                started_at: "100:000001".into(),
                boot_id: "boot-a".into(),
            }),
            executable: Some(executable.into()),
            cwd: Some("/work/project".into()),
            terminal: true,
        }
    }
    fn collect(records: Vec<Record>, current: impl FnMut(&Record) -> bool) -> ObservedScan {
        project(
            &records.into_iter().map(|item| (item.pid, item)).collect(),
            Path::new("/native/omp"),
            &["/Applications/Orca.app/Contents/MacOS/Orca".into()],
            501,
            current,
        )
    }
    #[test]
    fn manager_usage_probes_are_not_external_sessions() {
        let observer = std::process::id();
        let scan = collect(
            vec![
                fixture(observer, 1, "/app/aam-service"),
                fixture(70, observer, "/native/omp"),
                fixture(71, 1, "/native/omp"),
            ],
            |_| true,
        );
        let pids: Vec<_> = scan
            .sessions
            .iter()
            .map(|session| session.process.pid)
            .collect();
        assert_eq!(pids, vec![71]);
        assert!(scan.notices.is_empty());
    }
    #[test]
    fn exact_executable_and_same_user_are_required_without_guessing_identity() {
        let mut foreign = fixture(4, 1, "/native/omp");
        foreign.uid = 502;
        let mut setuid = fixture(5, 1, "/native/omp");
        setuid.real_uid = 0;
        let mut missing = fixture(6, 1, "/native/omp");
        missing.identity = None;
        let scan = collect(
            vec![
                fixture(2, 1, "/native/omp"),
                fixture(3, 1, "/unrelated/omp"),
                foreign,
                setuid,
                missing,
            ],
            |_| true,
        );
        assert_eq!(
            scan.sessions
                .iter()
                .map(|session| session.process.pid)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert!(scan.sessions[0].account_id.is_none());
        assert!(scan.sessions[0].model.is_none());
        assert_eq!(scan.sessions[0].verification, "observed");
    }
    #[test]
    fn worker_ancestry_is_not_counted_as_an_independent_session() {
        let mut login = fixture(12, 11, "/usr/bin/login");
        login.uid = 0;
        login.identity = None;
        login.executable = None;
        let scan = collect(
            vec![
                fixture(10, 1, "/Applications/Orca.app/Contents/MacOS/Orca"),
                fixture(11, 10, "/helper"),
                login,
                fixture(13, 12, "/bin/zsh"),
                fixture(14, 13, "/native/omp"),
                fixture(15, 14, "/native/omp"),
                fixture(16, 15, "/native/omp"),
            ],
            |_| true,
        );
        assert_eq!(
            scan.sessions
                .iter()
                .map(|session| session.process.pid)
                .collect::<Vec<_>>(),
            vec![14]
        );
        assert_eq!(scan.sessions[0].host, "orca");
        assert_eq!(scan.sessions[0].parent_process.as_ref().unwrap().pid, 13);
    }
    #[test]
    fn exited_or_reused_pid_is_not_reported_and_new_birth_gets_new_id() {
        let first = fixture(2, 1, "/native/omp");
        let old_id = collect(vec![first.clone()], |_| true).sessions[0]
            .id
            .clone();
        assert!(collect(vec![first.clone()], |_| false).sessions.is_empty());
        let mut reused = first;
        reused.identity.as_mut().unwrap().started_at = "101:000001".into();
        assert_ne!(collect(vec![reused], |_| true).sessions[0].id, old_id);
    }
    #[test]
    fn missing_or_cyclic_ancestry_is_explicitly_partial() {
        let missing = collect(vec![fixture(2, 9, "/native/omp")], |_| true);
        assert_eq!(missing.notices[0].id, "observed-session-discovery");
        assert!(missing.sessions[0].parent_process.is_none());
        let cyclic = collect(
            vec![fixture(2, 3, "/native/omp"), fixture(3, 2, "/bin/zsh")],
            |_| true,
        );
        assert_eq!(cyclic.notices[0].id, "observed-session-discovery");
    }
    #[test]
    fn bounded_projection_never_silently_reports_a_complete_list() {
        let records = (2..MAX_SESSIONS as u32 + 3)
            .map(|pid| fixture(pid, 1, "/native/omp"))
            .collect();
        let scan = collect(records, |_| true);
        assert_eq!(scan.sessions.len(), MAX_SESSIONS);
        assert_eq!(scan.notices[0].id, "observed-session-discovery");
    }
}

/// 프로세스가 쓰기 모드로 연 `.jsonl` 파일. 관측할 수 없는 OS에서는 비어 있다(매핑하지 않는다).
pub fn open_jsonl_writers(pid: u32) -> Vec<PathBuf> {
    #[cfg(any(target_os = "macos", windows))]
    {
        native::open_jsonl_writers(pid)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = pid;
        Vec::new()
    }
}
