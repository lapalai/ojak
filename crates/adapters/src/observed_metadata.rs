use aam_protocol::{
    now_ms, Account, ObservedAttribution, OmpCredentialPin, Paths, ProcessIdentity,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{LazyLock, Mutex},
};
#[cfg(unix)]
use std::{os::fd::{AsRawFd, FromRawFd}, os::unix::fs::{MetadataExt, OpenOptionsExt}};

pub(super) const MAX_FILES: usize = 64;
const MAX_LINE: usize = 256 * 1024;
const MAX_READ: usize = 2 * 1024 * 1024;
const MAX_ROWS: usize = 64;
const BRIDGE_MAX: u64 = 128 * 1024;
const FRESH_MS: i64 = 90_000;
const HISTORY: &str = "세션 파일의 저장 이력입니다. 현재 분기·요청의 계정/모델을 확정하지 않으며 API 키 우선순위·동시 호출·재시도 계정은 미확인입니다.";

#[derive(Clone, Debug)]
pub(super) struct WriterFile {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Default)]
pub(super) struct SessionMetadata {
    pub id: String,
    pub parent: Option<PathBuf>,
    pub parent_id: Option<String>,
    pub subagent: bool,
    pins: BTreeMap<String, BTreeMap<String, i64>>,
    calls: BTreeMap<(String, String, String, String, String), Call>,
    pub incomplete: bool,
}

// 알려지지 않은 필드(프롬프트·본문·도구 결과·오류)는 serde의 IgnoredAny로 건너뜁니다.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Entry {
    #[serde(rename = "type")]
    kind: String,
    id: String,
    version: Option<u32>,
    parent_session: Option<String>,
    timestamp: Option<String>,
    provider: String,
    hash: String,
    model: String,
    role: String,
    purpose: String,
    stop_reason: Option<String>,
    message: Option<Assistant>,
    custom_type: String,
}
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Assistant {
    role: String,
    provider: String,
    model: String,
    stop_reason: Option<String>,
    timestamp: Option<i64>,
}
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Call {
    provider: String,
    model: String,
    purpose: String,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    route: Option<String>,
    recorded_at: i64,
}

#[derive(Deserialize)]
struct RouteEntry {
    data: RouteRecord,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RouteRecord {
    version: u32,
    provider: String,
    model: String,
    route: String,
    recorded_at: i64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Pin {
    provider: String,
    hash: String,
    last_used_at: i64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Selection {
    provider: String,
    hash: String,
    #[serde(default)]
    selected_at: Option<i64>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bridge {
    version: u32,
    pid: u32,
    session_id: String,
    session_file: String,
    #[serde(default)]
    parent_session_file: Option<String>,
    observed_at: i64,
    started_at: i64,
    #[serde(default)]
    leaf_id: Option<String>,
    lifecycle: String,
    pins: Vec<Pin>,
    #[serde(default)]
    selections: Vec<Selection>,
    calls: Vec<Call>,
    #[serde(default)]
    issues: Vec<String>,
}

fn label(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}
fn hash_valid(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}
fn stop_reason(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .filter(|value| matches!(*value, "stop" | "length" | "toolUse" | "error" | "aborted"))
        .map(str::to_owned)
}
#[cfg(unix)]
fn birth_ms(identity: &ProcessIdentity) -> Option<i64> {
    let (seconds, micros) = identity.started_at.split_once(':')?;
    seconds
        .parse::<i64>()
        .ok()?
        .checked_mul(1000)?
        .checked_add(micros.parse::<i64>().ok()? / 1000)
}
#[cfg(windows)]
fn birth_ms(identity: &ProcessIdentity) -> Option<i64> {
    let ticks = identity.started_at.parse::<u64>().ok()?;
    i64::try_from(ticks.checked_div(10_000)?.checked_sub(11_644_473_600_000)?).ok()
}
fn iso_ms(value: Option<&str>) -> i64 {
    let Some(value) = value.filter(|value| value.len() == 24 && value.ends_with('Z')) else {
        return 0;
    };
    time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
        .ok().and_then(|at| i64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok()).unwrap_or(0)
}
fn open_owned(path: &Path) -> std::io::Result<File> {
    let file = aam_protocol::secure::open_read_no_follow(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || !aam_protocol::secure::file_owned_by_me(&file)? {
        return Err(std::io::Error::other(
            "관측 파일 소유권/유형을 확인하지 못했습니다.",
        ));
    }
    Ok(file)
}

fn file_identity(file: &File) -> std::io::Result<(u64, u64)> {
    #[cfg(unix)]
    { let stat = file.metadata()?; Ok((stat.dev(), stat.ino())) }
    #[cfg(windows)]
    { aam_protocol::winutil::file_identity(file) }
}
fn modified(stat: &std::fs::Metadata) -> std::io::Result<(i64, i64)> {
    let at = stat.modified()?.duration_since(std::time::UNIX_EPOCH).map_err(std::io::Error::other)?;
    Ok((at.as_secs() as i64, i64::from(at.subsec_nanos())))
}

fn open_snapshot(directory: &Path, name: &str) -> std::io::Result<File> {
    #[cfg(unix)]
    {
        let directory = OpenOptions::new().read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC).open(directory)?;
        let metadata = directory.metadata()?;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(std::io::Error::other("unsafe-directory"));
        }
        let name = std::ffi::CString::new(name).map_err(std::io::Error::other)?;
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 { return Err(std::io::Error::last_os_error()); }
        let file = unsafe { File::from_raw_fd(fd) };
        let stat = file.metadata()?;
        if !stat.is_file() || stat.uid() != unsafe { libc::geteuid() } || stat.mode() & 0o077 != 0 || stat.nlink() != 1 {
            return Err(std::io::Error::other("unsafe-file"));
        }
        Ok(file)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        // Deny directory rename while resolving the child, and never follow a reparse point.
        let held = OpenOptions::new().read(true).share_mode(3).custom_flags(0x02200000).open(directory)?;
        let stat = held.metadata()?;
        if !stat.is_dir() || stat.file_attributes() & 0x400 != 0
            || !aam_protocol::winutil::handle_access_is_safe(&held, true)? {
            return Err(std::io::Error::other("unsafe-directory"));
        }
        let file = aam_protocol::secure::open_read_no_follow(&directory.join(name))?;
        if !file.metadata()?.is_file() || !aam_protocol::winutil::single_link_regular_file(&file)?
            || !aam_protocol::winutil::handle_access_is_safe(&file, true)? {
            return Err(std::io::Error::other("unsafe-file"));
        }
        Ok(file)
    }
}


impl SessionMetadata {
    fn entry(&mut self, bytes: &[u8]) {
        let Ok(entry) = serde_json::from_slice::<Entry>(bytes) else {
            self.incomplete = true;
            return;
        };
        if entry.kind == "session" {
            if !self.id.is_empty()
                || entry.version != Some(3)
                || uuid::Uuid::parse_str(&entry.id).is_err()
            {
                self.incomplete = true;
                return;
            }
            self.id = entry.id;
            self.parent_id = entry
                .parent_session
                .as_ref()
                .filter(|id| uuid::Uuid::parse_str(id).is_ok())
                .cloned();
            self.parent = entry
                .parent_session
                .filter(|path| path.len() <= 4096 && Path::new(path).is_absolute())
                .map(PathBuf::from);
            #[cfg(windows)]
            if let Some(parent) = &mut self.parent {
                if let Ok(canonical) = parent.canonicalize() { *parent = canonical; }
            }
            return;
        }
        if self.id.is_empty() {
            return;
        }
        let recorded_at = iso_ms(entry.timestamp.as_deref());
        match entry.kind.as_str() {
            "session_init" => self.subagent = true,
            "custom" if entry.custom_type == "aam-route" => {
                // 다른 custom payload는 계속 IgnoredAny로 건너뛴다.
                if let Ok(RouteEntry { data }) = serde_json::from_slice::<RouteEntry>(bytes) {
                    if data.version == 1 && matches!(data.route.as_str(), "bridge" | "direct" | "unknown") {
                        self.call(Call {
                            provider: data.provider, model: data.model, purpose: "provider-route".into(),
                            role: None, stop_reason: None, route: Some(data.route), recorded_at: data.recorded_at,
                        });
                    }
                }
            }
            "credential_pin" if label(&entry.provider) && hash_valid(&entry.hash) => {
                let count: usize = self.pins.values().map(BTreeMap::len).sum();
                if count >= MAX_ROWS
                    && !self
                        .pins
                        .get(&entry.provider)
                        .is_some_and(|pins| pins.contains_key(&entry.hash))
                {
                    self.incomplete = true;
                    return;
                }
                self.pins
                    .entry(entry.provider)
                    .or_default()
                    .insert(entry.hash, recorded_at);
            }
            "model_usage" => self.call(Call {
                provider: entry.provider,
                model: entry.model,
                purpose: entry.purpose,
                role: Some(entry.role),
                stop_reason: entry.stop_reason,
                route: None,
                recorded_at,
            }),
            "model_change" => {
                if let Some((provider, model)) = entry.model.split_once('/') {
                    self.call(Call {
                        provider: provider.into(),
                        model: model.into(),
                        purpose: "model-change".into(),
                        role: Some(entry.role),
                        stop_reason: None,
                        route: None,
                        recorded_at,
                    });
                }
            }
            "message" => {
                if let Some(message) = entry.message.filter(|message| message.role == "assistant") {
                    self.call(Call {
                        provider: message.provider,
                        model: message.model,
                        purpose: "assistant".into(),
                        role: None,
                        stop_reason: message.stop_reason,
                        route: None,
                        recorded_at: message.timestamp.unwrap_or(recorded_at),
                    });
                }
            }
            _ => {}
        }
    }
    fn call(&mut self, mut call: Call) {
        if !label(&call.provider)
            || !label(&call.model)
            || !label(&call.purpose)
            || call
                .role
                .as_ref()
                .is_some_and(|role| !role.is_empty() && !label(role))
        {
            return;
        }
        let key = (
            call.provider.clone(),
            call.model.clone(),
            call.purpose.clone(),
            call.role.clone().unwrap_or_default(),
            call.route.clone().unwrap_or_default(),
        );
        call.stop_reason = stop_reason(&call.stop_reason);
        if self.calls.len() >= MAX_ROWS && !self.calls.contains_key(&key) {
            if let Some(oldest) = self
                .calls
                .iter()
                .min_by_key(|(_, call)| call.recorded_at)
                .map(|(key, _)| key.clone())
            {
                self.calls.remove(&oldest);
            }
            self.incomplete = true;
        }
        self.calls.insert(key, call);
    }
}

#[derive(Default)]
struct Cached {
    device: u64,
    inode: u64,
    offset: u64,
    length: u64,
    modified: (i64, i64),
    pending: Vec<u8>,
    skipping: bool,
    guard: [u8; 32],
    header_guard: [u8; 32],
    metadata: SessionMetadata,
}
impl Cached {
    fn consume(&mut self, bytes: &[u8]) {
        for part in bytes.split_inclusive(|byte| *byte == b'\n') {
            let ended = part.last() == Some(&b'\n');
            if !self.skipping {
                if self.pending.len() + part.len() > MAX_LINE {
                    self.pending.clear();
                    self.skipping = true;
                    self.metadata.incomplete = true;
                } else {
                    self.pending.extend_from_slice(part);
                }
            }
            if ended {
                if !self.skipping {
                    self.metadata.entry(&self.pending);
                }
                self.pending.clear();
                self.skipping = false;
            }
        }
    }
    fn update(
        &mut self,
        file: &mut File,
        writer: &WriterFile,
        budget: &mut usize,
    ) -> std::io::Result<SessionMetadata> {
        let stat = file.metadata()?;
        let (device, inode) = file_identity(file)?;
        if device != writer.device || inode != writer.inode {
            return Err(std::io::Error::other("writer 파일이 변경되었습니다."));
        }
        // 헤더·체크포인트는 digest만 보관합니다. 크기가 늘지 않은 변경은 재스캔합니다.
        let mut prefix = vec![0; stat.len().min(8192) as usize];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut prefix)?;
        let mut header = SessionMetadata::default();
        for line in prefix.split_inclusive(|byte| *byte == b'\n').take(2) {
            if line.last() == Some(&b'\n') {
                header.entry(line);
            }
        }
        if header.id.is_empty() {
            return Err(std::io::Error::other(
                "지원하는 세션 헤더를 확인하지 못했습니다.",
            ));
        }
        let header_line = prefix
            .split_inclusive(|byte| *byte == b'\n')
            .take(2)
            .find(|line| {
                serde_json::from_slice::<Entry>(line).is_ok_and(|entry| entry.kind == "session")
            })
            .unwrap_or_default();
        let header_guard: [u8; 32] = Sha256::digest(header_line).into();
        let modified = modified(&stat)?;
        let mut same = self.device == device
            && self.inode == inode
            && stat.len() >= self.offset
            && self.header_guard == header_guard
            && (stat.len() > self.length || modified == self.modified);
        if same && self.offset > 0 {
            let mut guard = vec![0; self.offset.min(64) as usize];
            file.seek(SeekFrom::Start(self.offset - guard.len() as u64))?;
            file.read_exact(&mut guard)?;
            same = <[u8; 32]>::from(Sha256::digest(&guard)) == self.guard;
        }
        if !same {
            *self = Self {
                device,
                inode,
                header_guard,
                ..Self::default()
            };
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let count = (stat.len() - self.offset).min(MAX_READ.min(*budget) as u64) as usize;
        let mut remaining = count;
        let mut buffer = [0; 16 * 1024];
        while remaining > 0 {
            let wanted = remaining.min(buffer.len());
            let read = file.read(&mut buffer[..wanted])?;
            if read == 0 {
                break;
            }
            self.consume(&buffer[..read]);
            self.offset += read as u64;
            *budget -= read;
            remaining -= read;
        }
        let mut guard = vec![0; self.offset.min(64) as usize];
        file.seek(SeekFrom::Start(self.offset - guard.len() as u64))?;
        file.read_exact(&mut guard)?;
        self.guard = Sha256::digest(&guard).into();
        self.length = stat.len();
        self.modified = modified;
        let mut metadata = self.metadata.clone();
        metadata.incomplete |=
            self.offset < stat.len() || !self.pending.is_empty() || self.skipping;
        Ok(metadata)
    }
}

type Cache = BTreeMap<(String, PathBuf), Cached>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

pub(super) fn read_sessions(
    identity: &ProcessIdentity,
    writers: &[WriterFile],
    budget: &mut usize,
) -> (Vec<(WriterFile, SessionMetadata)>, bool) {
    let mut result = Vec::new();
    let Ok(mut cache) = CACHE.lock() else {
        return (result, true);
    };
    let identity_key = format!(
        "{}:{}:{}",
        identity.boot_id, identity.started_at, identity.pid
    );
    let mut partial = false;
    for writer in writers.iter().take(MAX_FILES) {
        if *budget == 0 {
            partial = true;
            break;
        }
        let key = (identity_key.clone(), writer.path.clone());
        if cache.len() >= MAX_FILES * 4 && !cache.contains_key(&key) {
            cache.clear();
        }
        match open_owned(&writer.path).and_then(|mut file| {
            cache
                .entry(key)
                .or_default()
                .update(&mut file, writer, budget)
        }) {
            Ok(metadata) if !metadata.id.is_empty() => {
                partial |= metadata.incomplete;
                result.push((writer.clone(), metadata));
            }
            _ => partial = true,
        }
    }
    (result, partial)
}

fn match_pin(accounts: &[Account], provider: &str, hash: &str) -> (Option<String>, Option<String>) {
    let pin = OmpCredentialPin {
        provider: provider.into(),
        hash: hash.into(),
    };
    let ids: BTreeSet<_> = accounts
        .iter()
        .filter(|account| account.omp_credential_pins.contains(&pin))
        .map(|account| account.id.as_str())
        .collect();
    match ids.len() {
        1 => (ids.first().map(|id| (*id).into()), None),
        0 => (None, Some("관측된 OAuth pin과 일치하는 계정이 없습니다. API 키·누락된 identity는 추정하지 않습니다.".into())),
        _ => (None, Some("같은 provider/pin에 여러 계정 binding이 있어 하나로 확정하지 않습니다.".into())),
    }
}
fn pin_row(
    session: &SessionMetadata,
    role: &str,
    (provider, hash): (&str, &str),
    recorded_at: i64,
    source: &str,
    verification: &str,
    accounts: &[Account],
) -> ObservedAttribution {
    let (account_id, reason) = match_pin(accounts, provider, hash);
    ObservedAttribution {
        session_id: session.id.clone(), role: role.into(), provider: provider.into(), model: None,
        verification: if account_id.is_some() { verification } else { "unverified" }.into(), account_id,
        recorded_at, stop_reason: None, source: source.into(),
        route: None,
        reason: Some(match (source, reason) {
            ("session-file", Some(reason)) => format!("{HISTORY} {reason}"),
            ("session-file", None) => HISTORY.into(),
            (_, Some(reason)) => reason,
            (_, None) => "확장이 보고한 세션 pin/선택입니다. API 키 우선순위·동시 호출·재시도로 인해 개별 요청·보조 호출의 실제 사용 계정을 보장하지 않습니다.".into(),
        }),
    }
}
fn call_row(
    session: &SessionMetadata,
    call: &Call,
    role: &str,
    source: &str,
) -> ObservedAttribution {
    ObservedAttribution {
        session_id: session.id.clone(), role: match call.purpose.as_str() {
            "provider-route" => "unknown",
            "assistant" | "model-change" => role,
            _ => "auxiliary",
        }.into(),
        provider: call.provider.clone(), model: Some(call.model.clone()), account_id: None, verification: if call.purpose == "model-change" { "configured" } else { "unverified" }.into(),
        recorded_at: call.recorded_at, stop_reason: stop_reason(&call.stop_reason), source: source.into(),
        route: if call.purpose == "provider-route" {
            call.route.as_ref().filter(|route| matches!(route.as_str(), "bridge" | "direct" | "unknown")).cloned()
        } else { None },
        reason: Some(if call.purpose == "provider-route" { "확장이 요청·응답 hook의 실제 요청 모델에서 기록한 경로입니다. 응답 완료·역할·계정 identity·호출 횟수를 확정하지 않습니다." } else if source == "session-file" { HISTORY } else { "확장이 관측한 모델 호출 메타데이터입니다. 호출별 계정 identity는 제공되지 않습니다." }.into()),
    }
}

fn observed_path_matches(path: &Path, writer: &WriterFile) -> bool {
    if path == writer.path { return true; }
    #[cfg(windows)]
    return open_owned(path).and_then(|file| file_identity(&file))
        .is_ok_and(|identity| identity == (writer.device, writer.inode));
    #[cfg(not(windows))]
    false
}

fn parent_path_matches(left: Option<&str>, right: Option<&Path>) -> bool {
    let left = left.map(Path::new);
    if left == right { return true; }
    #[cfg(windows)]
    if let (Some(left), Some(right)) = (left, right) {
        return left.canonicalize().ok().zip(right.canonicalize().ok()).is_some_and(|(left, right)| left == right);
    }
    false
}

fn bridge_valid(
    bridge: &Bridge,
    identity: &ProcessIdentity,
    writer: &WriterFile,
    session: &SessionMetadata,
    mtime: i64,
    now: i64,
) -> bool {
    let Some(birth) = birth_ms(identity) else {
        return false;
    };
    bridge.version == 1
        && bridge.pid == identity.pid
        && bridge.session_id == session.id
        && observed_path_matches(Path::new(&bridge.session_file), writer)
        && parent_path_matches(bridge.parent_session_file.as_deref(), session.parent.as_deref())
        && bridge.started_at >= birth
        && bridge.started_at <= bridge.observed_at
        && bridge.observed_at >= now - FRESH_MS
        && bridge.observed_at <= now + 5000
        && mtime >= birth
        && mtime >= now - FRESH_MS
        && mtime <= now + 5000
        && (mtime - bridge.observed_at).abs() <= 10_000
        && bridge.leaf_id.as_ref().is_none_or(|id| label(id))
        && matches!(bridge.lifecycle.as_str(), "running" | "idle")
        && bridge.pins.len() <= MAX_ROWS
        && bridge.selections.len() <= MAX_ROWS
        && bridge.calls.len() <= MAX_ROWS
        && bridge.issues.len() <= 16
        && bridge.pins.iter().all(|pin| {
            label(&pin.provider)
                && hash_valid(&pin.hash)
                && pin.last_used_at >= 0
                && pin.last_used_at <= now + 5000
        })
        && bridge.selections.iter().all(|pin| {
            label(&pin.provider)
                && hash_valid(&pin.hash)
                && pin
                    .selected_at
                    .is_none_or(|at| at >= birth && at <= now + 5000)
        })
        && bridge.calls.iter().all(|call| {
            label(&call.provider)
                && label(&call.model)
                && label(&call.purpose)
                && call.recorded_at >= 0
                && call.recorded_at <= now + 5000
                && call.role.as_ref().is_none_or(|role| label(role))
                && call.route.as_ref().is_none_or(|route| call.purpose == "provider-route" && matches!(route.as_str(), "bridge" | "direct" | "unknown"))
        })
        && bridge.issues.iter().all(|issue| label(issue))
}

pub(super) fn attribute(
    paths: &Paths,
    identity: &ProcessIdentity,
    writer: &WriterFile,
    session: &SessionMetadata,
    role: &str,
    accounts: &[Account],
) -> (Vec<ObservedAttribution>, Option<&'static str>) {
    let dir = paths.home.join("omp-observations");
    let bridge_name = format!(
        "{}-{:x}.json",
        identity.pid,
        Sha256::digest(session.id.as_bytes())
    );
    let mut bridge_issue = None;
    let bridge = (|| {
        let file = match open_snapshot(&dir, &bridge_name) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => {
                bridge_issue = Some("확장 관측 디렉터리·파일의 소유권·권한을 확인하지 못했습니다.");
                return None;
            }
        };
        let stat = file.metadata().ok()?;
        if !stat.is_file() {
            bridge_issue = Some("확장 관측 파일의 소유권·유형을 확인하지 못했습니다.");
            return None;
        }
        bridge_issue = Some(
            "확장 관측이 오래되었거나 프로세스·세션 근거와 일치하지 않아 저장 이력만 표시합니다.",
        );
        if stat.len() > BRIDGE_MAX {
            return None;
        }
        let mut bytes = Vec::with_capacity(stat.len() as usize);
        file.take(BRIDGE_MAX + 1).read_to_end(&mut bytes).ok()?;
        if bytes.len() as u64 > BRIDGE_MAX {
            return None;
        }
        let bridge: Bridge = serde_json::from_slice(&bytes).ok()?;
        let (seconds, nanos) = modified(&stat).ok()?;
        let mtime = seconds.saturating_mul(1000) + nanos / 1_000_000;
        if !bridge_valid(&bridge, identity, writer, session, mtime, now_ms()) {
            return None;
        }
        bridge_issue = bridge.issues.iter().any(|issue| matches!(issue.as_str(),
            "pin-source-unavailable" | "usage-source-unavailable" | "selection-source-unavailable" | "previous-write-failed"
            | "branch-scan-limit" | "usage-scan-limit" | "provider-limit" | "snapshot-call-limit"
            | "branch-incomplete" | "selection-ambiguous" | "selection-identity-unavailable"
        )).then_some("확장에서 일부 관측 근거를 수집하지 못했습니다. 저장 이력을 함께 표시하며 누락된 identity는 추정하지 않습니다.");
        Some(bridge)
    })();
    let mut rows = Vec::new();
    if let Some(bridge) = bridge {
        // provider당 상충하는 pin/선택은 승격하지 않습니다.
        let mut pins: BTreeMap<&str, Vec<(&str, i64, &str)>> = BTreeMap::new();
        for pin in &bridge.pins {
            pins.entry(&pin.provider).or_default().push((
                &pin.hash,
                pin.last_used_at,
                "session-pin",
            ));
        }
        for pin in &bridge.selections {
            pins.entry(&pin.provider).or_default().push((
                &pin.hash,
                bridge.observed_at,
                "session-selection",
            ));
        }
        for (provider, candidates) in pins {
            // pin과 현재 sticky 선택은 서로 다른 증거이므로 별개로 유지합니다.
            for verification in ["session-pin", "session-selection"] {
                let candidates: Vec<_> = candidates
                    .iter()
                    .filter(|(_, _, kind)| *kind == verification)
                    .collect();
                if candidates.len() == 1 {
                    let (hash, at, _) = candidates[0];
                    rows.push(pin_row(
                        session,
                        role,
                        (provider, hash),
                        *at,
                        "extension",
                        verification,
                        accounts,
                    ));
                } else if !candidates.is_empty() {
                    bridge_issue = Some(
                        "확장에 동일 provider의 중복 identity가 있어 해당 연결을 생략했습니다.",
                    );
                }
            }
        }
        for call in &bridge.calls {
            rows.push(call_row(session, call, role, "extension"));
        }
    }
    if rows.is_empty() || bridge_issue.is_some() {
        for (provider, pins) in &session.pins {
            if pins.len() == 1 {
                let (hash, at) = pins.first_key_value().unwrap();
                rows.push(pin_row(
                    session,
                    role,
                    (provider, hash),
                    *at,
                    "session-file",
                    "session-pin",
                    accounts,
                ));
            } else {
                rows.push(ObservedAttribution { session_id: session.id.clone(), role: role.into(), provider: provider.clone(), model: None, account_id: None, verification: "unverified".into(), recorded_at: pins.values().copied().max().unwrap_or(0), stop_reason: None, source: "session-file".into(), route: None, reason: Some("저장 이력에 여러 OAuth pin이 있습니다. 현재 분기를 알 수 없어 하나를 선택하지 않습니다.".into()) });
            }
        }
        for call in session.calls.values() {
            rows.push(call_row(session, call, role, "session-file"));
        }
    }
    (rows, bridge_issue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    const ID: &str = "019a1234-1234-7123-8123-123456789abc";
    fn header() -> String {
        format!(
            "{}\n",
            json!({"type":"session","version":3,"id":ID,"timestamp":"2026-09-20T00:00:00.000Z"})
        )
    }
    fn pin(hash: &str) -> String {
        format!(
            "{}\n",
            json!({"type":"credential_pin","provider":"anthropic","hash":hash,"timestamp":"2026-09-20T00:00:01.001Z"})
        )
    }
    fn account(id: &str, hash: &str) -> Account {
        Account {
            id: id.into(),
            omp_credential_pins: vec![OmpCredentialPin {
                provider: "anthropic".into(),
                hash: hash.into(),
            }],
            ..Account::default()
        }
    }
    struct Fixture {
        dir: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("aam-observation-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&dir).unwrap();
            aam_protocol::secure::restrict_dir(&dir).unwrap();
            Self { dir }
        }
        fn paths(&self) -> Paths {
            Paths {
                home: self.dir.clone(),
                profiles: self.dir.join("profiles"),
                socket: self.dir.join("socket"),
                database: self.dir.join("unused.sqlite"),
            }
        }
        fn writer(&self, content: &str) -> WriterFile {
            let path = self.dir.join("session.jsonl");
            std::fs::write(&path, content).unwrap();
            let (device, inode) = file_identity(&File::open(&path).unwrap()).unwrap();
            WriterFile {
                path,
                inode,
                device,
            }
        }
        fn bridge(&self, identity: &ProcessIdentity, value: &serde_json::Value) -> PathBuf {
            let dir = self.dir.join("omp-observations");
            std::fs::create_dir_all(&dir).unwrap();
            aam_protocol::secure::restrict_dir(&dir).unwrap();
            let path = dir.join(format!(
                "{}-{:x}.json",
                identity.pid,
                Sha256::digest(ID.as_bytes())
            ));
            std::fs::write(&path, serde_json::to_vec(value).unwrap()).unwrap();
            aam_protocol::secure::restrict_file(&path).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
    fn bridge_value(writer: &WriterFile, now: i64) -> (ProcessIdentity, serde_json::Value) {
        let identity = ProcessIdentity {
            pid: 765,
            #[cfg(unix)]
            started_at: format!("{}:000000", now / 1000 - 2),
            #[cfg(windows)]
            started_at: ((now - 2000 + 11_644_473_600_000) * 10_000).to_string(),
            boot_id: "fixture-boot".into(),
        };
        let value = json!({
            "version":1,"pid":identity.pid,"sessionId":ID,"sessionFile":writer.path,
            "observedAt":now,"startedAt":now-1000,"lifecycle":"idle",
            "pins":[{"provider":"anthropic","hash":"a".repeat(64),"lastUsedAt":now-3000}],
            "selections":[{"provider":"anthropic","hash":"b".repeat(64)}],
            "calls":[{"provider":"anthropic","model":"example-model","purpose":"compaction","role":"smol","stopReason":"error","recordedAt":now}],
            "issues":["request-account-unavailable"]
        });
        (identity, value)
    }

    #[test]
    fn provider_scoped_pin_matching_never_chooses_an_ambiguous_binding() {
        let hash = "a".repeat(64);
        let a = account("a", &hash);
        assert_eq!(
            match_pin(std::slice::from_ref(&a), "anthropic", &hash)
                .0
                .as_deref(),
            Some("a")
        );
        assert!(match_pin(std::slice::from_ref(&a), "openai", &hash)
            .0
            .is_none());
        assert!(match_pin(&[a, account("b", &hash)], "anthropic", &hash)
            .0
            .is_none());
    }

    #[test]
    fn partial_and_oversized_lines_do_not_produce_partial_attribution_or_copy_body() {
        let mut cache = Cached::default();
        cache.consume(header().as_bytes());
        let line = pin(&"a".repeat(64));
        cache.consume(&line.as_bytes()[..line.len() - 1]);
        assert!(cache.metadata.pins.is_empty());
        cache.consume(b"\n");
        assert_eq!(cache.metadata.pins["anthropic"].len(), 1);
        cache.consume(&vec![b'x'; MAX_LINE + 1]);
        assert!(cache.pending.is_empty());
        cache.consume(b"\n");
        cache.consume(format!("{}\n", json!({"type":"message","message":{"role":"assistant","provider":"anthropic","model":"example-model","content":[{"text":"PRIVATE_BODY_SENTINEL"}],"errorMessage":"PRIVATE_ERROR_SENTINEL","stopReason":"error","timestamp":1234}})).as_bytes());
        let call = cache.metadata.calls.values().next().unwrap();
        let row = call_row(&cache.metadata, call, "main", "session-file");
        assert!(row.account_id.is_none());
        assert_eq!(row.stop_reason.as_deref(), Some("error"));
        assert_eq!(row.recorded_at, 1234);
        let public = serde_json::to_string(&row).unwrap();
        assert!(!public.contains("PRIVATE_"));
        assert!(cache.metadata.incomplete);
    }

    #[test]
    fn incremental_reads_bound_work_then_recover_after_append_and_truncation() {
        let fixture = Fixture::new();
        let mut content = header();
        let filler = format!(
            "{}\n",
            json!({"type":"custom","data":"ignored".repeat(100)})
        );
        while content.len() <= MAX_READ {
            content.push_str(&filler);
        }
        content.push_str(&pin(&"a".repeat(64)));
        let writer = fixture.writer(&content);
        let mut cache = Cached::default();
        let first = cache
            .update(
                &mut open_owned(&writer.path).unwrap(),
                &writer,
                &mut MAX_READ.clone(),
            )
            .unwrap();
        assert!(first.incomplete);
        assert!(first.pins.is_empty());
        let second = cache
            .update(
                &mut open_owned(&writer.path).unwrap(),
                &writer,
                &mut MAX_READ.clone(),
            )
            .unwrap();
        assert_eq!(second.pins["anthropic"].len(), 1);
        let mut budget = MAX_READ;
        cache
            .update(&mut open_owned(&writer.path).unwrap(), &writer, &mut budget)
            .unwrap();
        assert_eq!(budget, MAX_READ);
        let line = pin(&"b".repeat(64));
        let mut append = OpenOptions::new().append(true).open(&writer.path).unwrap();
        append
            .write_all(&line.as_bytes()[..line.len() - 1])
            .unwrap();
        let partial = cache
            .update(
                &mut open_owned(&writer.path).unwrap(),
                &writer,
                &mut MAX_READ.clone(),
            )
            .unwrap();
        assert_eq!(partial.pins["anthropic"].len(), 1);
        assert!(partial.incomplete);
        append.write_all(b"\n").unwrap();
        let complete = cache
            .update(
                &mut open_owned(&writer.path).unwrap(),
                &writer,
                &mut MAX_READ.clone(),
            )
            .unwrap();
        assert_eq!(complete.pins["anthropic"].len(), 2);
        std::fs::write(
            &writer.path,
            format!("{}{}", header(), pin(&"c".repeat(64))),
        )
        .unwrap();
        let replaced = cache
            .update(
                &mut open_owned(&writer.path).unwrap(),
                &writer,
                &mut MAX_READ.clone(),
            )
            .unwrap();
        assert_eq!(
            replaced.pins["anthropic"]
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["c".repeat(64)]
        );
        let wrong_inode = WriterFile {
            inode: writer.inode + 1,
            ..writer.clone()
        };
        assert!(cache
            .update(
                &mut open_owned(&writer.path).unwrap(),
                &wrong_inode,
                &mut MAX_READ.clone()
            )
            .is_err());
    }

    #[test]
    fn bridge_rejects_stale_reused_pid_future_and_foreign_session_evidence() {
        let fixture = Fixture::new();
        let writer = fixture.writer(&header());
        let session = SessionMetadata {
            id: ID.into(),
            ..SessionMetadata::default()
        };
        let now = now_ms();
        let (identity, value) = bridge_value(&writer, now);
        let decode = |value| serde_json::from_value::<Bridge>(value).unwrap();
        assert!(bridge_valid(
            &decode(value.clone()),
            &identity,
            &writer,
            &session,
            now,
            now
        ));
        for (key, bad) in [
            ("observedAt", json!(now - FRESH_MS - 1)),
            ("observedAt", json!(now + 60_000)),
            ("startedAt", json!(now - 60_000)),
            ("pid", json!(identity.pid + 1)),
            ("sessionId", json!("another-session")),
            ("sessionFile", json!("/unrelated.jsonl")),
            ("lifecycle", json!("shutdown")),
        ] {
            let mut changed = value.clone();
            changed[key] = bad;
            assert!(!bridge_valid(
                &decode(changed),
                &identity,
                &writer,
                &session,
                now,
                now
            ));
        }
        assert!(!bridge_valid(
            &decode(value),
            &identity,
            &writer,
            &session,
            now - FRESH_MS - 1,
            now
        ));
    }

    #[test]
    fn fresh_bridge_prefers_branch_evidence_without_promoting_call_accounts() {
        let fixture = Fixture::new();
        let writer = fixture.writer(&header());
        let mut session = SessionMetadata::default();
        session.entry(header().as_bytes());
        session.entry(pin(&"c".repeat(64)).as_bytes());
        let now = now_ms();
        let (identity, value) = bridge_value(&writer, now);
        fixture.bridge(&identity, &value);
        let accounts = [
            account("pinned", &"a".repeat(64)),
            account("selected", &"b".repeat(64)),
            account("old", &"c".repeat(64)),
        ];
        let (rows, issue) = attribute(
            &fixture.paths(),
            &identity,
            &writer,
            &session,
            "main",
            &accounts,
        );
        assert!(issue.is_none());
        assert!(rows.iter().all(|row| row.source == "extension"));
        assert!(rows
            .iter()
            .any(|row| row.account_id.as_deref() == Some("pinned")
                && row.verification == "session-pin"));
        assert!(rows
            .iter()
            .any(|row| row.account_id.as_deref() == Some("selected")
                && row.verification == "session-selection"
                && row.recorded_at == now));
        let call = rows.iter().find(|row| row.role == "auxiliary").unwrap();
        assert!(call.account_id.is_none());
        assert_eq!(call.stop_reason.as_deref(), Some("error"));
    }

    #[test]
    fn unsafe_bridge_files_fall_back_to_history_and_ambiguous_history_is_unknown() {
        let fixture = Fixture::new();
        let writer = fixture.writer(&header());
        let mut session = SessionMetadata::default();
        session.entry(header().as_bytes());
        session.entry(pin(&"a".repeat(64)).as_bytes());
        let (identity, value) = bridge_value(&writer, now_ms());
        let bridge_path = fixture.bridge(&identity, &value);
        let accounts = [account("a", &"a".repeat(64))];
        #[cfg(unix)]
        std::fs::set_permissions(&bridge_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        #[cfg(windows)]
        {
            let icacls = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/icacls.exe");
            assert!(std::process::Command::new(icacls).arg(&bridge_path).args(["/grant", "*S-1-1-0:R"]).output().unwrap().status.success());
        }
        let (rows, issue) = attribute(
            &fixture.paths(),
            &identity,
            &writer,
            &session,
            "main",
            &accounts,
        );
        assert!(issue.is_some());
        assert!(rows.iter().all(|row| row.source == "session-file"));
        assert_eq!(rows[0].account_id.as_deref(), Some("a"));
        let target = fixture.dir.join("payload.json");
        std::fs::rename(&bridge_path, &target).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &bridge_path).unwrap();
        #[cfg(windows)]
        std::fs::hard_link(&target, &bridge_path).unwrap();
        let (_, issue) = attribute(
            &fixture.paths(),
            &identity,
            &writer,
            &session,
            "main",
            &accounts,
        );
        assert!(issue.is_some());
        session.entry(pin(&"b".repeat(64)).as_bytes());
        let (rows, _) = attribute(
            &fixture.paths(),
            &identity,
            &writer,
            &session,
            "main",
            &accounts,
        );
        assert!(rows
            .iter()
            .all(|row| row.account_id.is_none() && row.verification == "unverified"));
    }

    #[test]
    fn persisted_routes_survive_stale_observer_without_promoting_account_or_role() {
        let fixture = Fixture::new();
        let now = now_ms();
        let mut content = header();
        for route in ["bridge", "direct"] {
            content.push_str(&format!("{}\n", json!({
                "type": "custom", "customType": "aam-route",
                "data": { "version": 1, "provider": "anthropic", "model": "same-model",
                    "route": route, "recordedAt": now - 2 * 60 * 60_000 }
            })));
        }
        content.push_str(&format!("{}\n", json!({
            "type": "message", "message": { "role": "assistant", "provider": "anthropic",
                "model": "same-model", "timestamp": now }
        })));
        let writer = fixture.writer(&content);
        let mut session = SessionMetadata::default();
        for line in content.lines() { session.entry(line.as_bytes()); }
        let (identity, mut value) = bridge_value(&writer, now);
        value["observedAt"] = json!(now - FRESH_MS - 1);
        fixture.bridge(&identity, &value);
        let (rows, issue) = attribute(&fixture.paths(), &identity, &writer, &session, "main", &[]);
        assert!(issue.is_some());
        let mut routes: Vec<_> = rows.iter().filter_map(|row| row.route.as_deref()).collect();
        routes.sort();
        assert_eq!(routes, ["bridge", "direct"]);
        assert!(rows.iter().all(|row| row.account_id.is_none() && row.source == "session-file"));
        assert!(rows.iter().filter(|row| row.route.is_some()).all(|row| row.role == "unknown"));
        assert!(rows.iter().any(|row| row.role == "main" && row.route.is_none()));
    }

    #[test]
    fn malformed_or_foreign_custom_records_cannot_confirm_routes() {
        let mut session = SessionMetadata::default();
        session.entry(header().as_bytes());
        for (kind, version, route) in [("unrelated", 1, "direct"), ("aam-route", 2, "direct"), ("aam-route", 1, "guessed")] {
            session.entry(json!({
                "type": "custom", "customType": kind,
                "data": { "version": version, "provider": "anthropic", "model": "m",
                    "route": route, "recordedAt": 1234 }
            }).to_string().as_bytes());
        }
        assert!(session.calls.is_empty());
        session.entry(br#"{"type":"custom","customType":"foreign","data":["PRIVATE_BODY"]}"#);
        assert!(!session.incomplete);
        session.entry(br#"{"type":"model_usage","provider":"ojak-claude","model":"m","purpose":"summary","route":"bridge"}"#);
        let call = session.calls.values().next().unwrap();
        assert!(call_row(&session, call, "main", "session-file").route.is_none());
    }

}
