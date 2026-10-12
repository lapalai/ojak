//! 실행 중인 공식 CLI가 "사용량 한도 소진"을 대화 기록에 남겼는지 감지한다.
//!
//! 근거는 공식 CLI가 직접 쓰는 구조화 기록 필드뿐이다(프롬프트·화면 문구·서비스의 used_percent는 쓰지 않는다).
//! - Claude Code `projects/*/<대화 UUID>.jsonl`: 합성 assistant 줄의 `isApiErrorMessage`·`error:"rate_limit"`·
//!   `apiErrorStatus:429`·`quotaLimits.status:"rejected"`(+ 추가 사용량으로 이어지지 않음). Claude 자신의
//!   `usage_limit` 분류(2.1.296 번들의 `e.quotaLimits!==void 0&&vI(e.quotaLimits)`)와 같은 조건이다.
//! - Codex `sessions/**/rollout-*.jsonl`: `event_msg`/`task_complete`의 `error.codex_error_info == "usage_limit_exceeded"`
//!   (`CodexErrorInfo::UsageLimitExceeded`). 일시 한도(`rate_limit_exceeded`)·과부하·연결 오류는 해당하지 않는다.
//!
//! 불확실하면 `None`이다. 시작 전 기준선(파일 길이)·기록 시각(시작 시각 이전 줄 거부)·파일 교체/축소·
//! 대화 불일치·실행 중 프로세스 트리가 쓰기로 열지 않은 Codex 파일은 모두 거부한다.
use crate::descendants::Descendants;
use aam_protocol::{process_identity, secure, Account, ProcessIdentity};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    io::{BufRead, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// 한 번의 폴링이 읽는 최대 바이트.
const READ_BUDGET: u64 = 1024 * 1024;
const CHUNK: usize = 64 * 1024;
/// 이보다 긴 줄은 버린다(한도 기록은 수 KiB다). 메모리에 쌓지 않는다.
const MAX_LINE: usize = 256 * 1024;
/// 읽지 못한 분량이 이보다 크면 끝쪽 `TAIL_WINDOW`로 건너뛴다. 한도 기록 뒤에는 턴이 끝나 더 쓰이지 않는다.
const MAX_BACKLOG: u64 = 4 * 1024 * 1024;
const TAIL_WINDOW: u64 = 1024 * 1024;
/// 시작 직전 기록과 시계 차이를 허용하는 폭.
const CLOCK_SKEW_MS: i64 = 2000;
const META_LINE_LIMIT: u64 = 1024 * 1024;

pub(crate) struct Exhausted {
    pub native_id: String,
}

type FileId = (u64, u64);

pub(crate) struct LimitWatch {
    started_ms: i64,
    tail_every: Duration,
    find_every: Duration,
    last_tail: Option<Instant>,
    last_find: Option<Instant>,
    /// 이미 알린 기록(파일 identity, 줄 시작 오프셋). 같은 기록을 다시 알리지 않는다.
    reported: Option<(FileId, u64)>,
    kind: Kind,
}

enum Kind {
    Inert,
    Claude(Claude),
    Codex(Codex),
}

struct Claude {
    profile: PathBuf,
    native: String,
    baseline: Baseline,
    tail: Option<Tail>,
}

struct Codex {
    profile: PathBuf,
    cwd: PathBuf,
    native: Option<String>,
    /// spawn 전 같은 대화 ID의 rollout 상태. `None`은 확인하지 못했다는 뜻이다.
    baseline: Option<Vec<(PathBuf, FileId, u64)>>,
    current: Option<String>,
    tail: Option<Tail>,
    metas: HashMap<PathBuf, Meta>,
    writers: Box<dyn FnMut(&ProcessIdentity) -> Vec<PathBuf> + Send>,
}

/// 실행 전에 본 대화 파일의 상태.
enum Baseline {
    Known(FileId, u64),
    /// 없었다: 실행 중 새로 만들어졌으므로 처음부터 본다.
    Absent,
    /// 확인하지 못했다: 처음 본 시점의 끝에서 시작한다.
    Unknown,
}

impl LimitWatch {
    /// 공식 CLI를 spawn하기 전에 호출한다. 기준선을 이때 잡으므로 이전 한도 기록은 절대 신호가 되지 않는다.
    pub(crate) fn new(account: &Account, cwd: &str, native_id: Option<&str>) -> Self {
        let started_ms = aam_protocol::now_ms();
        let kind = Self::kind(account, cwd, native_id).unwrap_or(Kind::Inert);
        Self {
            started_ms,
            tail_every: Duration::from_millis(250),
            find_every: Duration::from_secs(1),
            last_tail: None,
            last_find: None,
            reported: None,
            kind,
        }
    }

    fn kind(account: &Account, cwd: &str, native_id: Option<&str>) -> Option<Kind> {
        let profile = Path::new(account.profile_path.as_deref()?).canonicalize().ok()?;
        let native = native_id
            .map(|id| uuid::Uuid::parse_str(id).map(|id| id.to_string()))
            .transpose()
            .ok()?;
        match account.tool.as_str() {
            "claude" => {
                // Claude는 실행 전에 대화 ID를 알아야 정확한 파일을 가리킬 수 있다.
                let native = native?;
                let baseline = match find_claude(&profile, &native) {
                    Find::Found(path) => open_regular(&path)
                        .map_or(Baseline::Unknown, |(_, len, id)| Baseline::Known(id, len)),
                    Find::Missing => Baseline::Absent,
                    Find::Unknown => Baseline::Unknown,
                };
                Some(Kind::Claude(Claude { profile, native, baseline, tail: None }))
            }
            "codex" => Some(Kind::Codex(Codex {
                baseline: native.as_deref().and_then(|id| codex_baseline(&profile, id)),
                profile,
                cwd: Path::new(cwd).canonicalize().ok()?,
                native,
                current: None,
                tail: None,
                metas: HashMap::new(),
                writers: tree_writers(),
            })),
            _ => None,
        }
    }

    /// 새로 기록된 한도 소진 증거가 있으면 그 대화 ID. `root`는 spawn 때 확인한 birth identity다.
    pub(crate) fn poll(&mut self, root: &ProcessIdentity) -> Option<Exhausted> {
        if matches!(self.kind, Kind::Inert) || process_identity(root.pid).ok().as_ref() != Some(root) {
            return None;
        }
        let now = Instant::now();
        let started = self.started_ms;
        let find_due = due(&mut self.last_find, self.find_every, now);
        let tail_due = due(&mut self.last_tail, self.tail_every, now);
        let (id, native) = match &mut self.kind {
            Kind::Inert => return None,
            Kind::Claude(claude) => {
                if claude.tail.is_none() && find_due {
                    claude.attach();
                }
                let native = claude.native.clone();
                let tail = claude.tail.as_mut().filter(|_| tail_due)?;
                (tail.scan(&|line| claude_record(line, &native, started))?, native)
            }
            Kind::Codex(codex) => {
                if find_due {
                    codex.attribute(root, started);
                }
                let native = codex.current.clone()?;
                let tail = codex.tail.as_mut().filter(|_| tail_due)?;
                (tail.scan(&|line| codex_record(line, started))?, native)
            }
        };
        let ident = match &self.kind {
            Kind::Claude(Claude { tail: Some(tail), .. }) | Kind::Codex(Codex { tail: Some(tail), .. }) => tail.ident,
            _ => return None,
        };
        if self.reported == Some((ident, id)) {
            return None;
        }
        self.reported = Some((ident, id));
        Some(Exhausted { native_id: native })
    }
}

fn due(last: &mut Option<Instant>, every: Duration, now: Instant) -> bool {
    if last.is_some_and(|at| now.duration_since(at) < every) {
        return false;
    }
    *last = Some(now);
    true
}

impl Claude {
    fn attach(&mut self) {
        let Find::Found(path) = find_claude(&self.profile, &self.native) else {
            return;
        };
        let Some((_, len, ident)) = open_regular(&path) else {
            return;
        };
        let offset = match self.baseline {
            Baseline::Known(base, base_len) if base == ident => base_len.min(len),
            Baseline::Absent => 0,
            _ => len,
        };
        self.tail = Some(Tail::new(path, ident, offset));
    }
}

enum Find {
    Found(PathBuf),
    Missing,
    /// 둘 이상이거나 읽지 못했다.
    Unknown,
}

/// 계정 프로필의 `projects/<폴더>/<id>.jsonl`. 정확히 하나일 때만 쓴다(어댑터의 전달 규칙과 같다).
fn find_claude(profile: &Path, native: &str) -> Find {
    let name = format!("{native}.jsonl");
    let entries = match std::fs::read_dir(profile.join("projects")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Find::Missing,
        Err(_) => return Find::Unknown,
    };
    let mut found = Vec::new();
    for entry in entries.flatten().take(8192) {
        let candidate = entry.path().join(&name);
        if std::fs::symlink_metadata(&candidate).is_ok_and(|meta| meta.is_file()) {
            found.push(candidate);
        }
    }
    match found.len() {
        0 => Find::Missing,
        1 => Find::Found(found.remove(0)),
        _ => Find::Unknown,
    }
}

/// Claude 줄이 이 대화의 하드 사용량 한도 소진인지. 사용자 입력·도구 출력·일반 429·과부하·모델별 크레딧 안내는 아니다.
fn claude_record(line: &[u8], native: &str, started_ms: i64) -> bool {
    if !contains(line, b"isApiErrorMessage") {
        return false;
    }
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return false;
    };
    let text = |key: &str| value.get(key).and_then(Value::as_str);
    let message = value.get("message");
    let session = text("sessionId")
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .map(|id| id.to_string());
    let Some(limits) = value.get("quotaLimits").filter(|v| v.is_object()) else {
        return false;
    };
    let kind = limits.get("rateLimitType").and_then(Value::as_str);
    let resets = limits.get("resetsAt").and_then(Value::as_f64);
    let not_true = |v: Option<&Value>| v.is_none_or(|v| v == &Value::Bool(false));
    text("type") == Some("assistant")
        && value.get("isApiErrorMessage") == Some(&Value::Bool(true))
        && text("error") == Some("rate_limit")
        && value.get("apiErrorStatus").and_then(Value::as_u64) == Some(429)
        && not_true(value.get("isSidechain"))
        && session.as_deref() == Some(native)
        && message.and_then(|m| m.get("role")).and_then(Value::as_str) == Some("assistant")
        && message.and_then(|m| m.get("model")).and_then(Value::as_str) == Some("<synthetic>")
        && fresh(text("timestamp"), started_ms)
        && limits.get("status").and_then(Value::as_str) == Some("rejected")
        && resets.is_some_and(|at| at.is_finite() && at > 0.0)
        && matches!(
            kind,
            Some("five_hour" | "seven_day" | "seven_day_opus" | "seven_day_sonnet" | "seven_day_overage_included")
        )
        && not_true(limits.get("isUsingOverage"))
        && not_true(limits.get("overageInUse"))
        && not_true(limits.get("unifiedRateLimitFallbackAvailable"))
}

/// Codex 줄이 이 실행의 턴이 사용량 한도로 끝났다는 공식 기록인지.
fn codex_record(line: &[u8], started_ms: i64) -> bool {
    if !contains(line, b"usage_limit_exceeded") {
        return false;
    }
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return false;
    };
    let Some(payload) = value.get("payload") else {
        return false;
    };
    let completed = payload.get("completed_at").and_then(Value::as_i64);
    value.get("type").and_then(Value::as_str) == Some("event_msg")
        && payload.get("type").and_then(Value::as_str) == Some("task_complete")
        && payload
            .get("error")
            .and_then(|error| error.get("codex_error_info"))
            .and_then(Value::as_str)
            == Some("usage_limit_exceeded")
        && fresh(value.get("timestamp").and_then(Value::as_str), started_ms)
        && completed.is_none_or(|secs| secs.saturating_mul(1000) >= started_ms - CLOCK_SKEW_MS)
}

fn fresh(timestamp: Option<&str>, started_ms: i64) -> bool {
    timestamp.and_then(parse_ms).is_some_and(|at| at >= started_ms - CLOCK_SKEW_MS)
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

/// `YYYY-MM-DDTHH:MM:SS[.fff]Z`만 받는다. 그 밖의 형식은 확인 불가로 거부한다.
fn parse_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        part.bytes().all(|b| b.is_ascii_digit()).then(|| part.parse().ok()).flatten()
    };
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let mut index = 19;
    let mut millis = 0;
    if bytes[index] == b'.' {
        let start = index + 1;
        index = start;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        let digits = &bytes[start..index];
        if digits.is_empty() {
            return None;
        }
        // 소수 자리는 밀리초까지만 쓰고, 모자란 자리는 0으로 본다(할당 없음).
        for place in 0..3 {
            millis = millis * 10 + digits.get(place).map_or(0, |digit| i64::from(digit - b'0'));
        }
    }
    if bytes.get(index) != Some(&b'Z') || index + 1 != bytes.len() {
        return None;
    }
    // Howard Hinnant의 days_from_civil.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hour) * 60 + minute) * 60_000 + second * 1000 + millis)
}

// ---- 파일 증분 읽기 ----

fn identity(file: &File) -> Option<FileId> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata().ok()?;
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(windows)]
    {
        aam_protocol::winutil::file_identity(file).ok()
    }
}

/// 링크를 따라가지 않고, 현재 사용자 소유 일반 파일만 연다.
fn open_regular(path: &Path) -> Option<(File, u64, FileId)> {
    let file = secure::open_read_no_follow(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || !secure::file_owned_by_me(&file).ok()? {
        return None;
    }
    let ident = identity(&file)?;
    Some((file, meta.len(), ident))
}

struct Tail {
    path: PathBuf,
    ident: FileId,
    offset: u64,
    carry: Vec<u8>,
    /// 줄 중간에서 시작했거나 너무 긴 줄이라 다음 줄바꿈까지 버리는 중.
    discarding: bool,
    line_start: u64,
}

impl Tail {
    /// `offset`이 줄 경계인지 확인해 시작한다(아니면 다음 줄바꿈까지 버린다).
    fn new(path: PathBuf, ident: FileId, offset: u64) -> Self {
        let mut tail = Self { path, ident, offset, carry: Vec::new(), discarding: false, line_start: offset };
        match open_regular(&tail.path) {
            Some((mut file, _, _)) => tail.align(&mut file),
            None => tail.discarding = offset > 0,
        }
        tail
    }

    /// `offset` 직전 바이트가 줄바꿈이어야 줄 경계다. 확인하지 못하면 다음 줄바꿈까지 버린다.
    fn align(&mut self, file: &mut File) {
        self.carry.clear();
        self.discarding = true;
        if self.offset == 0 {
            self.discarding = false;
            return;
        }
        let mut byte = [0u8; 1];
        if file.seek(SeekFrom::Start(self.offset - 1)).is_ok() && file.read_exact(&mut byte).is_ok() {
            self.discarding = byte[0] != b'\n';
        }
    }

    /// 새로 읽은 줄 중 `hit`가 참인 마지막 줄의 시작 오프셋. 교체·축소된 파일은 현재 끝으로 다시 잡고 알리지 않는다.
    fn scan(&mut self, hit: &dyn Fn(&[u8]) -> bool) -> Option<u64> {
        let (mut file, len, ident) = open_regular(&self.path)?;
        if ident != self.ident || len < self.offset {
            self.ident = ident;
            self.offset = len;
            self.align(&mut file);
            return None;
        }
        if len - self.offset > MAX_BACKLOG {
            self.offset = len - TAIL_WINDOW;
            self.align(&mut file);
        }
        file.seek(SeekFrom::Start(self.offset)).ok()?;
        let mut buffer = vec![0u8; CHUNK];
        let mut budget = READ_BUDGET;
        let mut found = None;
        while budget > 0 && self.offset < len {
            let want = (CHUNK as u64).min(budget).min(len - self.offset) as usize;
            let read = match file.read(&mut buffer[..want]) {
                Ok(0) | Err(_) => break,
                Ok(read) => read,
            };
            let start = self.offset;
            self.offset += read as u64;
            budget -= read as u64;
            if let Some(at) = self.feed(&buffer[..read], start, hit) {
                found = Some(at);
            }
        }
        found
    }

    fn feed(&mut self, chunk: &[u8], chunk_start: u64, hit: &dyn Fn(&[u8]) -> bool) -> Option<u64> {
        let mut position = chunk_start;
        let mut found = None;
        for piece in chunk.split_inclusive(|byte| *byte == b'\n') {
            let piece_start = position;
            position += piece.len() as u64;
            let complete = piece.last() == Some(&b'\n');
            if self.discarding {
                self.discarding = !complete;
                continue;
            }
            if self.carry.is_empty() {
                self.line_start = piece_start;
            }
            if self.carry.len() + piece.len() > MAX_LINE {
                self.carry.clear();
                self.discarding = !complete;
                continue;
            }
            self.carry.extend_from_slice(piece);
            if complete {
                let line = std::mem::take(&mut self.carry);
                if hit(line.trim_ascii_end()) {
                    found = Some(self.line_start);
                }
            }
        }
        found
    }
}

// ---- Codex 대화 파일 귀속 ----

struct Meta {
    id: String,
    cwd: PathBuf,
    interactive: bool,
    created_ms: Option<i64>,
}

/// rollout 첫 줄(`session_meta`). 서브에이전트 스레드(`source`가 객체)는 이 대화가 아니다.
fn read_meta(path: &Path) -> Option<Meta> {
    let (file, _, _) = open_regular(path)?;
    let mut line = Vec::new();
    std::io::BufReader::new(file.take(META_LINE_LIMIT)).read_until(b'\n', &mut line).ok()?;
    if line.last() != Some(&b'\n') {
        return None;
    }
    let value: Value = serde_json::from_slice(&line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?;
    Some(Meta {
        id: uuid::Uuid::parse_str(payload.get("id")?.as_str()?).ok()?.to_string(),
        cwd: Path::new(payload.get("cwd")?.as_str()?).canonicalize().ok()?,
        interactive: matches!(payload.get("source").and_then(Value::as_str), Some("cli" | "exec" | "vscode" | "mcp")),
        created_ms: payload.get("timestamp").and_then(Value::as_str).and_then(parse_ms),
    })
}

/// spawn 전에 같은 대화 ID의 rollout(`sessions/<년>/<월>/<일>/rollout-*-<id>.jsonl`) 상태를 잡는다.
/// 폴더 항목 수가 상한을 넘거나 읽을 수 없으면 `None`(확인 불가)이다. 각 파일은 링크를 따라가지 않고 열어 identity·길이만 본다.
fn codex_baseline(profile: &Path, native: &str) -> Option<Vec<(PathBuf, FileId, u64)>> {
    const MAX_ENTRIES: usize = 200_000;
    let suffix = format!("-{native}.jsonl");
    let sessions = match profile.join("sessions").canonicalize() {
        Ok(sessions) => sessions,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Some(Vec::new()),
        Err(_) => return None,
    };
    let mut budget = MAX_ENTRIES;
    let mut list = |dir: &Path| -> Option<Vec<PathBuf>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(dir).ok()? {
            budget = budget.checked_sub(1)?;
            entries.push(entry.ok()?.path());
        }
        Some(entries)
    };
    let mut found = Vec::new();
    for year in list(&sessions)? {
        if !year.is_dir() {
            continue;
        }
        for month in list(&year)? {
            if !month.is_dir() {
                continue;
            }
            for day in list(&month)? {
                if !day.is_dir() {
                    continue;
                }
                for file in list(&day)? {
                    let matches = file
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix));
                    if !matches {
                        continue;
                    }
                    let path = file.canonicalize().ok()?;
                    let (_, len, ident) = open_regular(&path)?;
                    found.push((path, ident, len));
                }
            }
        }
    }
    Some(found)
}

impl Codex {
    /// 이 실행의 프로세스 트리가 쓰기로 연 이 프로필의 rollout 중, 작업 폴더가 같은 주 스레드가 정확히 하나일 때만 따라간다.
    fn attribute(&mut self, root: &ProcessIdentity, started_ms: i64) {
        let written = (self.writers)(root);
        let Ok(sessions) = self.profile.join("sessions").canonicalize() else {
            return;
        };
        if self.metas.len() > 64 {
            self.metas.clear();
        }
        let mut candidates = BTreeMap::new();
        for path in written.iter().filter_map(|path| path.canonicalize().ok()) {
            let rollout = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"));
            if !rollout || !path.starts_with(&sessions) {
                continue;
            }
            if !self.metas.contains_key(&path) {
                match read_meta(&path) {
                    Some(meta) => {
                        self.metas.insert(path.clone(), meta);
                    }
                    // 첫 줄이 아직 다 쓰이지 않았을 수 있다. 다음에 다시 본다.
                    None => continue,
                }
            }
            let meta = &self.metas[&path];
            if meta.interactive && meta.cwd == self.cwd && self.native.as_deref().is_none_or(|id| id == meta.id) {
                candidates.insert(path.clone(), (meta.id.clone(), meta.created_ms));
            }
        }
        match candidates.len() {
            // 파일을 잠시 닫았을 수 있다. 이미 귀속한 파일은 유지한다.
            0 => {}
            1 => {
                let (path, (id, created)) = candidates.into_iter().next().expect("one candidate");
                if self.tail.as_ref().is_some_and(|tail| tail.path == path) {
                    return;
                }
                let Some((_, len, ident)) = open_regular(&path) else {
                    return;
                };
                let known = self.native.as_deref() == Some(id.as_str());
                let offset = match (&self.baseline, known) {
                    // 이어 받은 대화: spawn 전에 본 같은 파일(identity 동일)의 길이에서 시작한다. 그 뒤에 쓰인 줄은 모두 새 기록이다.
                    (Some(before), true) => match before.iter().find(|(seen, _, _)| seen == &path) {
                        Some((_, base, base_len)) if *base == ident => (*base_len).min(len),
                        // 이전에 없던 파일: 실행 중 새로 만들어졌다.
                        None => 0,
                        // 교체된 파일은 이전 내용을 알 수 없다.
                        Some(_) => len,
                    },
                    // 새 대화: 이 실행이 만든 파일(생성 시각이 시작 이후)만 처음부터 본다.
                    _ if !known && created.is_some_and(|at| at >= started_ms - CLOCK_SKEW_MS) => 0,
                    // 기준선을 잡지 못했거나 외부에서 이미 있던 파일은 처음 본 끝에서 시작한다.
                    _ => len,
                };
                self.current = Some(id);
                self.tail = Some(Tail::new(path, ident, offset));
            }
            _ => {
                self.current = None;
                self.tail = None;
            }
        }
    }
}

/// 프로세스 트리(루트 + 관측한 자손, birth가 같은 프로세스만)가 쓰기로 연 `.jsonl` 경로.
/// Windows는 native와 자손이 든 이름 있는 Job으로 자손을 모두 본다(WriterWatch와 같은 규칙).
fn tree_writers() -> Box<dyn FnMut(&ProcessIdentity) -> Vec<PathBuf> + Send> {
    struct Tree {
        root: ProcessIdentity,
        alive: bool,
        tree: Descendants,
    }
    let mut state: Option<Tree> = None;
    Box::new(move |root| {
        if state.as_ref().is_none_or(|tree| &tree.root != root) {
            #[cfg(windows)]
            let tree = match aam_protocol::ProcessJob::open(&aam_protocol::job_name(root.pid, &root.started_at)) {
                Ok(job) => Descendants::with_job(root.pid, job),
                Err(_) => Descendants::new(root.pid),
            };
            #[cfg(not(windows))]
            let tree = Descendants::new(root.pid);
            state = Some(Tree { root: root.clone(), alive: true, tree });
        }
        let Some(tree) = state.as_mut() else {
            return Vec::new();
        };
        let same = |identity: &ProcessIdentity| process_identity(identity.pid).ok().as_ref() == Some(identity);
        if tree.alive && !same(&tree.root) {
            tree.alive = false;
            tree.tree.root_lost();
        }
        tree.tree.observe();
        let live: Vec<ProcessIdentity> = tree.alive.then(|| tree.root.clone()).into_iter().chain(tree.tree.identities()).collect();
        let mut files = Vec::new();
        for identity in live.into_iter().filter(|identity| same(identity)) {
            let opened = aam_adapters::open_jsonl_writers(identity.pid);
            if same(&identity) {
                files.extend(opened);
            }
        }
        files
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const CLAUDE_ID: &str = "11111111-2222-4333-8444-555555555555";
    const OTHER_ID: &str = "99999999-2222-4333-8444-555555555555";
    const CODEX_ID: &str = "01a119df-de6e-76f1-bddf-53e6261b2073";

    fn ms(text: &str) -> i64 {
        parse_ms(text).unwrap()
    }

    /// 실제 Claude Code 2.1.269 세션 한도 기록(2026-09-12)에서 식별자·경로·프롬프트를 지운 형태.
    fn claude_limit(session: &str, timestamp: &str) -> String {
        format!(
            r#"{{"parentUuid":"00000000-0000-4000-8000-000000000001","isSidechain":false,"type":"assistant","uuid":"00000000-0000-4000-8000-000000000002","timestamp":"{timestamp}","message":{{"diagnostics":null,"id":"00000000-0000-4000-8000-000000000003","container":null,"model":"<synthetic>","role":"assistant","stop_details":null,"stop_reason":"stop_sequence","stop_sequence":"","type":"message","usage":{{"input_tokens":0,"output_tokens":0}},"content":[{{"type":"text","text":"You've hit your session limit · resets 10pm (Asia/Seoul)"}}],"context_management":null}},"requestId":null,"quotaLimits":{{"status":"rejected","resetsAt":1789218000,"unifiedRateLimitFallbackAvailable":false,"rateLimitType":"five_hour","overageStatus":"rejected","overageDisabledReason":"org_level_disabled","upgradePaths":["upgrade_plan"],"isUsingOverage":false}},"error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429,"sessionId":"{session}","version":"2.1.269","userType":"external","entrypoint":"cli"}}"#
        )
    }

    /// 실제 Claude 2.1.278 모델별 크레딧 안내(하드 한도 아님): `quotaLimits` 없음.
    const CLAUDE_FABLE: &str = r#"{"isSidechain":false,"type":"assistant","timestamp":"2026-09-21T02:08:01.686Z","message":{"model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"You've reached your Fable limit. Switch to another model, or manage usage credits to continue."}]},"apiError":"model_requires_usage_credits","error":"rate_limit","errorDetails":"429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\"}}","isApiErrorMessage":true,"apiErrorStatus":429,"sessionId":"11111111-2222-4333-8444-555555555555"}"#;
    const CLAUDE_OVERLOAD: &str = r#"{"isSidechain":false,"type":"assistant","timestamp":"2026-09-22T01:06:25.113Z","message":{"model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"API Error: 529 Overloaded."}]},"error":"server_error","isApiErrorMessage":true,"apiErrorStatus":529,"sessionId":"11111111-2222-4333-8444-555555555555"}"#;

    fn codex_limit(timestamp: &str, info: &str) -> String {
        format!(
            r#"{{"timestamp":"{timestamp}","ordinal":8,"type":"event_msg","payload":{{"type":"task_complete","turn_id":"01a119df-e102-7001-8224-d68b3fa440f7","last_agent_message":null,"error":{{"message":"You've hit your usage limit. Try again later.","codex_error_info":{info}}},"started_at":1791435464,"completed_at":1791435482,"duration_ms":18003}}}}"#
        )
    }

    #[test]
    fn timestamps_parse_exactly() {
        assert_eq!(parse_ms("1970-01-01T00:00:00.000Z"), Some(0));
        assert_eq!(parse_ms("1970-01-01T00:00:01Z"), Some(1000));
        assert_eq!(parse_ms("2026-10-08T04:58:02.967Z"), Some(1_791_435_482_967));
        assert_eq!(parse_ms("2026-10-08T04:58:02.9Z"), Some(1_791_435_482_900));
        for bad in ["", "2026-10-08", "2026-10-08T04:58:02+09:00", "2026-13-08T04:58:02Z", "2026-10-08T04:58:02.Z", "x026-10-08T04:58:02Z"] {
            assert_eq!(parse_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn claude_positive_requires_every_structured_field() {
        let start = ms("2026-09-12T12:00:00.000Z");
        let line = claude_limit(CLAUDE_ID, "2026-09-12T12:41:15.349Z");
        assert!(claude_record(line.as_bytes(), CLAUDE_ID, start));
        // 주간·모델별 한도도 하드 한도다.
        for kind in ["seven_day", "seven_day_opus", "seven_day_sonnet", "seven_day_overage_included"] {
            let edited = line.replace("five_hour", kind);
            assert!(claude_record(edited.as_bytes(), CLAUDE_ID, start), "{kind}");
        }
    }

    #[test]
    fn claude_rejects_everything_but_hard_quota_of_this_session() {
        let start = ms("2026-09-12T12:00:00.000Z");
        let good = claude_limit(CLAUDE_ID, "2026-09-12T12:41:15.349Z");
        let check = |line: &str| claude_record(line.as_bytes(), CLAUDE_ID, start);
        // 다른 대화
        assert!(!check(&claude_limit(OTHER_ID, "2026-09-12T12:41:15.349Z")));
        // 시작 전 기록(이전 한도 오류)
        assert!(!check(&claude_limit(CLAUDE_ID, "2026-09-12T11:00:00.000Z")));
        // 일반 429·모델별 크레딧·과부하
        assert!(!check(CLAUDE_FABLE));
        assert!(!check(CLAUDE_OVERLOAD));
        // 추가 사용량으로 이어지거나 대체 경로가 있는 경우, 허용 상태, 알 수 없는 한도
        for (from, to) in [
            (r#""isUsingOverage":false"#, r#""isUsingOverage":true"#),
            (r#""unifiedRateLimitFallbackAvailable":false"#, r#""unifiedRateLimitFallbackAvailable":true"#),
            (r#""status":"rejected","resetsAt""#, r#""status":"allowed","resetsAt""#),
            (r#""rateLimitType":"five_hour""#, r#""rateLimitType":"overage""#),
            (r#""rateLimitType":"five_hour""#, r#""rateLimitType":"other""#),
            (r#""resetsAt":1789218000"#, r#""resetsAt":null"#),
            (r#""apiErrorStatus":429"#, r#""apiErrorStatus":500"#),
            (r#""error":"rate_limit""#, r#""error":"server_error""#),
            (r#""isSidechain":false"#, r#""isSidechain":true"#),
            (r#""model":"<synthetic>""#, r#""model":"claude-opus""#),
            (r#""isApiErrorMessage":true"#, r#""isApiErrorMessage":false"#),
            (r#""type":"assistant","uuid""#, r#""type":"user","uuid""#),
        ] {
            assert!(good.contains(from), "{from}");
            assert!(!check(&good.replace(from, to)), "{to}");
        }
        // 사용자가 같은 문구를 입력하거나 도구가 출력한 경우: 최상위 필드가 아니라 문자열 안에 있다.
        let spoof_user = r#"{"type":"user","timestamp":"2026-09-12T12:41:15.349Z","sessionId":"11111111-2222-4333-8444-555555555555","message":{"role":"user","content":"You've hit your session limit \"isApiErrorMessage\":true \"quotaLimits\":{\"status\":\"rejected\"}"}}"#;
        assert!(!check(spoof_user));
        let escaped = good.replace('"', "\\\"");
        let spoof_tool = format!(
            r#"{{"type":"user","timestamp":"2026-09-12T12:41:15.349Z","sessionId":"{CLAUDE_ID}","message":{{"role":"user","content":[{{"type":"tool_result","content":"{escaped}"}}]}},"isApiErrorMessage":false}}"#
        );
        assert!(!check(&spoof_tool));
        // 깨졌거나 JSON이 아닌 줄
        assert!(!check(&good[..good.len() - 1]));
        assert!(!check("isApiErrorMessage"));
    }

    #[test]
    fn codex_positive_is_only_usage_limit_exceeded_on_task_complete() {
        let start = ms("2026-10-08T04:00:00.000Z");
        let check = |line: &str| codex_record(line.as_bytes(), start);
        assert!(check(&codex_limit("2026-10-08T04:58:02.967Z", r#""usage_limit_exceeded""#)));
        // 실제 기록된 401 오류(`other`), 일시 한도, 과부하, 연결 오류는 한도 소진이 아니다.
        for info in [r#""other""#, r#""rate_limit_exceeded""#, r#""server_overloaded""#, r#""unauthorized""#, r#"{"http_connection_failed":{"http_status_code":429}}"#, "null"] {
            assert!(!check(&codex_limit("2026-10-08T04:58:02.967Z", info)), "{info}");
        }
        // 시작 전 기록
        assert!(!check(&codex_limit("2026-10-08T03:00:00.000Z", r#""usage_limit_exceeded""#)));
        // 다른 줄 종류: 사용자 입력·모델 응답·턴 중단에 같은 문자열이 있어도 아니다.
        let user = r#"{"timestamp":"2026-10-08T04:58:02.967Z","type":"event_msg","payload":{"type":"user_message","message":"codex_error_info usage_limit_exceeded"}}"#;
        let response = r#"{"timestamp":"2026-10-08T04:58:02.967Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"\"codex_error_info\":\"usage_limit_exceeded\""}]}}"#;
        let aborted = r#"{"timestamp":"2026-10-08T04:58:02.967Z","type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted","error":{"codex_error_info":"usage_limit_exceeded"}}}"#;
        let tool_output = r#"{"timestamp":"2026-10-08T04:58:02.967Z","type":"response_item","payload":{"type":"function_call_output","output":"{\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"error\":{\"codex_error_info\":\"usage_limit_exceeded\"}}}"}}"#;
        for line in [user, response, aborted, tool_output] {
            assert!(!check(line), "{line}");
        }
        // 이전 날짜의 completed_at이 최근 timestamp와 함께 와도 거부(재기록된 오래된 턴)
        let stale = codex_limit("2026-10-08T04:58:02.967Z", r#""usage_limit_exceeded""#).replace("1791435482", "1700000000");
        assert!(!check(&stale));
    }

    // ---- 증분 읽기 ----

    struct Dir(PathBuf);
    impl Dir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("aam-limit-{}", aam_protocol::new_id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path.canonicalize().unwrap())
        }
        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn append(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }
    fn tail_at(path: &Path, offset: u64) -> Tail {
        let (_, _, ident) = open_regular(path).unwrap();
        Tail::new(path.to_path_buf(), ident, offset)
    }
    fn hit(line: &[u8]) -> bool {
        line == b"HIT"
    }

    #[test]
    fn tail_waits_for_partial_lines_and_ignores_old_content() {
        let dir = Dir::new();
        let path = dir.join("a.jsonl");
        append(&path, "HIT\nnoise\n");
        let mut tail = tail_at(&path, 10);
        assert_eq!(tail.scan(&hit), None);
        append(&path, "HI");
        assert_eq!(tail.scan(&hit), None);
        append(&path, "T\n");
        assert_eq!(tail.scan(&hit), Some(10));
        assert_eq!(tail.scan(&hit), None);
    }

    #[test]
    fn tail_does_not_trigger_on_truncation_or_replacement() {
        let dir = Dir::new();
        let path = dir.join("a.jsonl");
        append(&path, "noise\nnoise\nHIT\n");
        let mut tail = tail_at(&path, 16);
        // 축소: 남은 내용에 있는 HIT는 새 기록이 아니다.
        std::fs::write(&path, "HIT\n").unwrap();
        assert_eq!(tail.scan(&hit), None);
        append(&path, "HIT\n");
        assert_eq!(tail.scan(&hit), Some(4));
        // 교체: 더 긴 새 파일의 기존 내용도 알리지 않는다.
        let replaced = dir.join("b.jsonl");
        std::fs::write(&replaced, "HIT\nHIT\nHIT\nHIT\nHIT\nHIT\n").unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&replaced, &path).unwrap();
        assert_eq!(tail.scan(&hit), None);
        append(&path, "HIT\n");
        assert!(tail.scan(&hit).is_some());
    }

    #[test]
    fn tail_handles_chunk_boundaries_and_oversize_lines() {
        let dir = Dir::new();
        let path = dir.join("a.jsonl");
        // 64KiB 경계를 가로지르는 줄.
        let filler = "x".repeat(CHUNK - 3);
        append(&path, &format!("{filler}\nHIT\n"));
        let mut tail = tail_at(&path, 0);
        assert_eq!(tail.scan(&hit), Some(CHUNK as u64 - 2));
        // 너무 긴 줄은 버리고 다음 줄은 정상 처리.
        let before = std::fs::metadata(&path).unwrap().len();
        append(&path, &format!("HIT{}\nHIT\n", "y".repeat(MAX_LINE + 10)));
        let hit_at = std::fs::metadata(&path).unwrap().len() - 4;
        assert!(hit_at > before);
        assert_eq!(tail.scan(&hit), Some(hit_at));
    }

    #[test]
    fn tail_skips_huge_backlog_without_misreading_a_cut_line() {
        let dir = Dir::new();
        let path = dir.join("a.jsonl");
        let line = format!("{}\n", "z".repeat(99));
        let mut text = line.repeat((MAX_BACKLOG as usize + 2 * TAIL_WINDOW as usize) / 100);
        text.push_str("HIT\n");
        append(&path, &text);
        let mut tail = tail_at(&path, 0);
        let at = (text.len() - 4) as u64;
        assert_eq!(tail.scan(&hit), Some(at));
    }

    // ---- LimitWatch ----

    fn root() -> ProcessIdentity {
        process_identity(std::process::id()).unwrap()
    }

    fn claude_watch(profile: &Dir, native: Option<&str>) -> LimitWatch {
        let account = Account { tool: "claude".into(), profile_path: Some(profile.0.to_string_lossy().into_owned()), ..Account::default() };
        let mut watch = LimitWatch::new(&account, &profile.0.to_string_lossy(), native);
        watch.started_ms = ms("2026-09-12T12:00:00.000Z");
        watch.tail_every = Duration::ZERO;
        watch.find_every = Duration::ZERO;
        watch
    }

    #[test]
    fn claude_poll_uses_baseline_and_reports_each_record_once() {
        let profile = Dir::new();
        let folder = profile.join("projects/-tmp-work");
        std::fs::create_dir_all(&folder).unwrap();
        let transcript = folder.join(format!("{CLAUDE_ID}.jsonl"));
        let old = claude_limit(CLAUDE_ID, "2026-09-12T12:30:00.000Z");
        append(&transcript, &format!("{old}\n"));
        let mut watch = claude_watch(&profile, Some(CLAUDE_ID));
        // 실행 전에 있던 기록(시각이 시작 이후여도)은 기준선 앞이라 신호가 아니다.
        assert!(watch.poll(&root()).is_none());
        append(&transcript, &format!("{}\n", CLAUDE_OVERLOAD));
        assert!(watch.poll(&root()).is_none());
        // 이 대화가 아닌 기록, 부분 줄.
        append(&transcript, &format!("{}\n", claude_limit(OTHER_ID, "2026-09-12T12:41:15.349Z")));
        let fresh_line = claude_limit(CLAUDE_ID, "2026-09-12T12:41:15.349Z");
        append(&transcript, &fresh_line[..40]);
        assert!(watch.poll(&root()).is_none());
        append(&transcript, &format!("{}\n", &fresh_line[40..]));
        let exhausted = watch.poll(&root()).expect("fresh exhaustion");
        assert_eq!(exhausted.native_id, CLAUDE_ID);
        assert!(watch.poll(&root()).is_none());
        append(&transcript, &format!("{}\n", claude_limit(CLAUDE_ID, "2026-09-12T12:50:00.000Z")));
        assert!(watch.poll(&root()).is_some());
    }

    #[test]
    fn claude_file_created_after_spawn_is_read_from_the_start() {
        let profile = Dir::new();
        let mut watch = claude_watch(&profile, Some(CLAUDE_ID));
        assert!(watch.poll(&root()).is_none());
        let folder = profile.join("projects/-tmp-work");
        std::fs::create_dir_all(&folder).unwrap();
        append(&folder.join(format!("{CLAUDE_ID}.jsonl")), &format!("{}\n", claude_limit(CLAUDE_ID, "2026-09-12T12:41:15.349Z")));
        assert!(watch.poll(&root()).is_some());
    }

    #[test]
    fn claude_without_a_known_session_or_with_ambiguous_files_never_fires() {
        let profile = Dir::new();
        let mut watch = claude_watch(&profile, None);
        assert!(watch.poll(&root()).is_none());
        let mut watch = claude_watch(&profile, Some("not-a-uuid"));
        assert!(watch.poll(&root()).is_none());
        for folder in ["-a", "-b"] {
            let dir = profile.join(&format!("projects/{folder}"));
            std::fs::create_dir_all(&dir).unwrap();
            append(&dir.join(format!("{CLAUDE_ID}.jsonl")), &format!("{}\n", claude_limit(CLAUDE_ID, "2026-09-12T12:41:15.349Z")));
        }
        let mut watch = claude_watch(&profile, Some(CLAUDE_ID));
        assert!(watch.poll(&root()).is_none());
    }

    #[test]
    fn exited_root_stops_detection() {
        let profile = Dir::new();
        let folder = profile.join("projects/-tmp-work");
        std::fs::create_dir_all(&folder).unwrap();
        let mut watch = claude_watch(&profile, Some(CLAUDE_ID));
        append(&folder.join(format!("{CLAUDE_ID}.jsonl")), &format!("{}\n", claude_limit(CLAUDE_ID, "2026-09-12T12:41:15.349Z")));
        let mut gone = root();
        gone.started_at.push('0');
        assert!(watch.poll(&gone).is_none());
        assert!(watch.poll(&root()).is_some());
    }

    fn codex_fixture(source: &str, created: &str, id: &str) -> (Dir, Dir, PathBuf) {
        let profile = Dir::new();
        let cwd = Dir::new();
        let day = profile.join("sessions/2026/10/08");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("rollout-2026-10-08T13-57-44-{id}.jsonl"));
        let meta = format!(
            r#"{{"timestamp":"{created}","ordinal":0,"type":"session_meta","payload":{{"session_id":"{id}","id":"{id}","timestamp":"{created}","cwd":{},"originator":"codex_cli_rs","cli_version":"0.154.0","source":{source},"model_provider":"openai"}}}}"#,
            serde_json::to_string(&cwd.0.to_string_lossy()).unwrap()
        );
        append(&path, &format!("{meta}\n"));
        (profile, cwd, path)
    }

    fn codex_watch(profile: &Dir, cwd: &Dir, native: Option<&str>, writers: Vec<PathBuf>) -> LimitWatch {
        let account = Account { tool: "codex".into(), profile_path: Some(profile.0.to_string_lossy().into_owned()), ..Account::default() };
        let mut watch = LimitWatch::new(&account, &cwd.0.to_string_lossy(), native);
        watch.started_ms = ms("2026-10-08T04:00:00.000Z");
        watch.tail_every = Duration::ZERO;
        watch.find_every = Duration::ZERO;
        let Kind::Codex(codex) = &mut watch.kind else { panic!("codex watch") };
        codex.writers = Box::new(move |_| writers.clone());
        watch
    }

    #[test]
    fn codex_new_rollout_is_attributed_by_writer_and_read_from_the_start() {
        let (profile, cwd, path) = codex_fixture(r#""cli""#, "2026-10-08T04:57:44.325Z", CODEX_ID);
        let mut watch = codex_watch(&profile, &cwd, None, vec![path.clone()]);
        let user = r#"{"timestamp":"2026-10-08T04:57:50.000Z","type":"event_msg","payload":{"type":"user_message","message":"usage_limit_exceeded"}}"#;
        append(&path, &format!("{user}\n{}\n", codex_limit("2026-10-08T04:58:01.000Z", r#""other""#)));
        assert!(watch.poll(&root()).is_none());
        append(&path, &format!("{}\n", codex_limit("2026-10-08T04:58:02.967Z", r#""usage_limit_exceeded""#)));
        let exhausted = watch.poll(&root()).expect("usage limit");
        assert_eq!(exhausted.native_id, CODEX_ID);
        assert!(watch.poll(&root()).is_none());
    }

    #[test]
    fn codex_resumed_rollout_skips_existing_records() {
        let (profile, cwd, path) = codex_fixture(r#""cli""#, "2026-10-01T00:00:00.000Z", CODEX_ID);
        append(&path, &format!("{}\n", codex_limit("2026-10-08T04:30:00.000Z", r#""usage_limit_exceeded""#)));
        let mut watch = codex_watch(&profile, &cwd, Some(CODEX_ID), vec![path.clone()]);
        assert!(watch.poll(&root()).is_none());
        append(&path, &format!("{}\n", codex_limit("2026-10-08T04:58:02.967Z", r#""usage_limit_exceeded""#)));
        assert_eq!(watch.poll(&root()).map(|e| e.native_id), Some(CODEX_ID.to_owned()));
    }

    #[test]
    fn codex_resumed_rollout_appended_before_first_poll_is_not_lost() {
        let (profile, cwd, path) = codex_fixture(r#""cli""#, "2026-10-01T00:00:00.000Z", CODEX_ID);
        // 실행 전에 이미 있던 하드 한도 기록은 무시한다.
        append(&path, &format!("{}\n", codex_limit("2026-10-08T04:30:00.000Z", r#""usage_limit_exceeded""#)));
        let mut watch = codex_watch(&profile, &cwd, Some(CODEX_ID), vec![path.clone()]);
        // spawn 뒤 첫 poll 전에 CLI가 한도 기록을 쓴다.
        append(&path, &format!("{}\n", codex_limit("2026-10-08T04:58:02.967Z", r#""usage_limit_exceeded""#)));
        assert_eq!(watch.poll(&root()).map(|e| e.native_id), Some(CODEX_ID.to_owned()));
        assert!(watch.poll(&root()).is_none());
    }

    #[test]
    fn codex_resumed_rollout_replaced_before_attribution_is_not_trusted() {
        let (profile, cwd, path) = codex_fixture(r#""cli""#, "2026-10-01T00:00:00.000Z", CODEX_ID);
        let mut watch = codex_watch(&profile, &cwd, Some(CODEX_ID), vec![path.clone()]);
        // 같은 이름의 다른 파일로 교체: 이전 내용을 알 수 없어 기존 기록을 신호로 쓰지 않는다.
        let swap = path.with_extension("swap");
        std::fs::copy(&path, &swap).unwrap();
        append(&swap, &format!("{}\n", codex_limit("2026-10-08T04:58:02.967Z", r#""usage_limit_exceeded""#)));
        std::fs::remove_file(&path).unwrap();
        std::fs::rename(&swap, &path).unwrap();
        assert!(watch.poll(&root()).is_none());
    }

    #[test]
    fn codex_requires_exact_writer_owned_main_thread_in_this_profile_and_cwd() {
        let limit = format!("{}\n", codex_limit("2026-10-08T04:58:02.967Z", r#""usage_limit_exceeded""#));
        // 서브에이전트 스레드
        let (profile, cwd, path) = codex_fixture(r#"{"subagent":{"thread_spawn":{"depth":1}}}"#, "2026-10-08T04:57:44.325Z", CODEX_ID);
        append(&path, &limit);
        assert!(codex_watch(&profile, &cwd, None, vec![path]).poll(&root()).is_none());
        // 프로세스가 쓰지 않는 파일(같은 프로필의 외부 Codex)
        let (profile, cwd, path) = codex_fixture(r#""cli""#, "2026-10-08T04:57:44.325Z", CODEX_ID);
        append(&path, &limit);
        assert!(codex_watch(&profile, &cwd, None, vec![]).poll(&root()).is_none());
        // 작업 폴더가 다른 대화
        let other_cwd = Dir::new();
        assert!(codex_watch(&profile, &other_cwd, None, vec![path.clone()]).poll(&root()).is_none());
        // 기대한 대화 ID와 다른 파일
        assert!(codex_watch(&profile, &cwd, Some(OTHER_ID), vec![path.clone()]).poll(&root()).is_none());
        // 다른 프로필 밖의 파일
        let elsewhere = Dir::new();
        assert!(codex_watch(&elsewhere, &cwd, None, vec![path.clone()]).poll(&root()).is_none());
        // 같은 프로세스가 주 스레드 둘을 열고 있으면 모호하다.
        let second = path.with_file_name(format!("rollout-2026-10-08T13-58-00-{OTHER_ID}.jsonl"));
        let meta = std::fs::read_to_string(&path).unwrap().lines().next().unwrap().replace(CODEX_ID, OTHER_ID);
        append(&second, &format!("{meta}\n{limit}"));
        assert!(codex_watch(&profile, &cwd, None, vec![path.clone(), second]).poll(&root()).is_none());
        // 정상 확인
        assert!(codex_watch(&profile, &cwd, None, vec![path]).poll(&root()).is_some());
    }
}
