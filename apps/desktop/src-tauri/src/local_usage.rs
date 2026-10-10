//! 이 PC에 남은 omp·Claude CLI·Codex 사용 기록을 읽어 토큰과 API 요금 환산액을 집계한다.
//!
//! - 원본 기록(JSONL)은 읽기만 한다. Ojak 서비스 DB와 브릿지 카운터는 건드리지도 더하지도 않는다.
//! - 색인은 `AAM_HOME/local-usage.sqlite3`(WAL)에 둔다. 응답 단위 숫자·시각·모델 이름·해시된 식별자만 저장하고
//!   대화 내용, 경로, 이메일, 인증 정보는 저장하지 않는다.
//! - 파일마다 읽은 위치를 저장해 늘어난 부분만 읽는다. 호출 한 번이 읽는 양(바이트·시간)은 고정 상한이고, 남은 일이
//!   있으면 `scanning`으로 알린다. 기간(`sinceMs`·`untilMs`)은 색인 조회에만 쓰며 기록을 다시 읽지 않는다.
//! - 읽을 수 없거나 깨진 파일은 크기·수정 시각이 바뀔 때까지 다시 시도하지 않고 경고 코드로만 알린다
//!   (`scanning`이 영원히 켜져 있지 않다).
//!
//! 경고 코드(화면이 `tokens.warning.<코드>`로 옮긴다): `INDEX_UNAVAILABLE`, `NO_SOURCES`, `INDEX_TRUNCATED`,
//! `SOURCE_UNREADABLE`, `SOURCE_PARTIAL`, `CLOCK_SKEW`, `PRICING_UNKNOWN`, `CROSS_TOOL_OVERLAP`, `ATTRIBUTION_UNAVAILABLE`.

use aam_protocol::{ApiError, Paths};
use rusqlite::{params, Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    fs,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{mpsc, Mutex, MutexGuard, TryLockError},
    time::{Duration, Instant, UNIX_EPOCH},
};

const INDEX_VERSION: i32 = 2;
/// 호출 한 번이 읽는 최대 바이트와 최대 시간. 먼저 닿는 쪽에서 멈춘다.
const PASS_BYTES: u64 = 64 << 20;
const PASS_TIME: Duration = Duration::from_millis(600);
/// 기록 폴더를 다시 훑는 최소 간격과 읽을 폴더 목록을 다시 찾는 간격.
const ENUM_EVERY: Duration = Duration::from_secs(20);
const DISCOVER_EVERY: Duration = Duration::from_secs(300);
const MAX_LINE: usize = 32 << 20;
const TAIL_WINDOW: u64 = 4096;
const MAX_FILES: usize = 250_000;
const MAX_DEPTH: usize = 12;
const FUTURE_SLACK_MS: i64 = 86_400_000;
const MAX_COMPONENT: u64 = 4_000_000_000;
const MAX_SAFE: u64 = (1 << 53) - 1;
const STALE_TAIL_MS: i64 = 600_000;

const TOOL_OMP: u8 = 0;
const TOOL_CLAUDE: u8 = 1;
const TOOL_CODEX: u8 = 2;

const FLAG_TIME: i64 = 1;
const FLAG_FUTURE: i64 = 2;
const FLAG_IMPLAUSIBLE: i64 = 4;

fn tool_name(tool: i64) -> &'static str {
    match tool {
        0 => "omp",
        1 => "claude",
        _ => "codex",
    }
}

// ───────────────────────────── 응답 타입 ─────────────────────────────

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    pub cost_usd: Option<f64>,
    pub unpriced_tokens: u64,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ToolUsage {
    pub tool: String,
    pub sessions: u64,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub tool: String,
    pub model: String,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LocalUsageReport {
    pub indexed_at: Option<i64>,
    pub scanning: bool,
    pub truncated: bool,
    pub coverage_start: Option<i64>,
    pub coverage_end: Option<i64>,
    pub files_scanned: u64,
    pub files_pending: u64,
    /// 처음 읽기의 진행률을 바이트로 센다. 파일 크기가 제각각이라 파일 수보다 고르게 늘어난다.
    pub bytes_read: u64,
    pub bytes_total: u64,
    pub totals: UsageTotals,
    pub tools: Vec<ToolUsage>,
    pub models: Vec<ModelUsage>,
    pub warnings: Vec<String>,
    pub excluded_records: u64,
    pub duplicate_records: u64,
}

fn empty_report(warnings: &[&str]) -> LocalUsageReport {
    LocalUsageReport {
        indexed_at: None,
        scanning: false,
        truncated: false,
        coverage_start: None,
        coverage_end: None,
        files_scanned: 0,
        files_pending: 0,
        bytes_read: 0,
        bytes_total: 0,
        totals: UsageTotals::default(),
        tools: Vec::new(),
        models: Vec::new(),
        warnings: warnings.iter().map(|w| (*w).to_owned()).collect(),
        excluded_records: 0,
        duplicate_records: 0,
    }
}

// ───────────────────────────── 해시·시각·숫자 ─────────────────────────────

fn mix(mut h: u64) -> u64 {
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    h ^= h >> 33;
    h
}

/// 버전이 바뀌어도 값이 같아야 하는 64비트 해시(FNV-1a + 마무리). 색인 키라서 `DefaultHasher`는 쓰지 않는다.
fn hash_parts(parts: &[&[u8]]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in *part {
            h ^= u64::from(*byte);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    mix(h)
}

fn hash_strs(parts: &[&str]) -> i64 {
    let bytes: Vec<&[u8]> = parts.iter().map(|p| p.as_bytes()).collect();
    hash_parts(&bytes) as i64
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// RFC 3339 시각을 Unix ms로. `Z`·`±hh:mm`·`±hhmm`·시간대 없음(UTC로 본다)을 받는다. 1970년 이전·잘못된 날짜는 `None`.
pub(crate) fn parse_iso_ms(text: &str) -> Option<i64> {
    let b = text.trim().as_bytes();
    if b.len() < 19 {
        return None;
    }
    let digits = |start: usize, len: usize| -> Option<i64> {
        let mut value = 0i64;
        for k in start..start + len {
            let c = *b.get(k)?;
            if !c.is_ascii_digit() {
                return None;
            }
            value = value * 10 + i64::from(c - b'0');
        }
        Some(value)
    };
    if b[4] != b'-' || b[7] != b'-' || !(b[10] == b'T' || b[10] == b' ') || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let (y, mo, da, h, mi, se) = (digits(0, 4)?, digits(5, 2)?, digits(8, 2)?, digits(11, 2)?, digits(14, 2)?, digits(17, 2)?);
    if y < 1970 || !(1..=12).contains(&mo) || da < 1 || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if leap => 29,
        _ => 28,
    };
    if da > dim {
        return None;
    }
    let mut i = 19;
    let mut ms = 0i64;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let (mut count, mut frac) = (0, 0i64);
        while let Some(c) = b.get(i).copied().filter(u8::is_ascii_digit) {
            if count < 3 {
                frac = frac * 10 + i64::from(c - b'0');
                count += 1;
            }
            i += 1;
        }
        if count == 0 {
            return None;
        }
        while count < 3 {
            frac *= 10;
            count += 1;
        }
        ms = frac;
    }
    let mut offset = 0i64;
    if let Some(&sign) = b.get(i) {
        if sign == b'+' || sign == b'-' {
            let hh = digits(i + 1, 2)?;
            let mm = if b.get(i + 3) == Some(&b':') { digits(i + 4, 2)? } else { digits(i + 3, 2).unwrap_or(0) };
            if hh > 23 || mm > 59 {
                return None;
            }
            offset = (hh * 60 + mm) * 60_000;
            if sign == b'-' {
                offset = -offset;
            }
        }
    }
    Some((days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 + se) * 1000 + ms - offset)
}

fn ts_ms(value: &Option<Value>) -> Option<i64> {
    match value.as_ref()? {
        Value::String(text) => parse_iso_ms(text),
        Value::Number(n) => {
            let f = n.as_f64()?;
            if !f.is_finite() || f <= 0.0 {
                None
            } else if f < 1e11 {
                Some((f * 1000.0) as i64)
            } else {
                Some(f as i64)
            }
        }
        _ => None,
    }
}

fn num(value: &Option<Value>) -> u64 {
    match value {
        Some(Value::Number(n)) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| f.is_finite() && *f > 0.0).map(|f| f as u64))
            .unwrap_or(0),
        _ => 0,
    }
}

fn safe(n: u64) -> u64 {
    n.min(MAX_SAFE)
}

// ───────────────────────────── 이름 정리·가격 ─────────────────────────────

/// 모델 이름을 색인·응답에 넣어도 되는 모양으로 줄인다. 경로·이메일처럼 보이는 값은 `unknown`.
fn sanitize_model(text: &str) -> String {
    let t = text.trim();
    let allowed = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'+' | b'-' | b'[' | b']');
    let ok = !t.is_empty()
        && t.len() <= 96
        && t.bytes().all(allowed)
        && t.matches('/').count() <= 1
        && !t.starts_with('/')
        && !t.ends_with('/')
        && !t.starts_with('.')
        && !t.contains("..")
        && !t.contains(":/");
    if ok {
        t.to_owned()
    } else {
        "unknown".into()
    }
}

fn model_label(provider: Option<&str>, model: Option<&str>) -> String {
    match (provider, model) {
        (Some(p), Some(m)) if !m.contains('/') && !p.is_empty() => sanitize_model(&format!("{p}/{m}")),
        (_, Some(m)) => sanitize_model(m),
        _ => "unknown".into(),
    }
}

struct Long {
    threshold: u64,
    inclusive: bool,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
}
struct Price {
    id: &'static str,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    long: Option<Long>,
}
const fn p(id: &'static str, input: f64, output: f64, cache_read: f64, cache_write: f64) -> Price {
    Price { id, input, output, cache_read, cache_write, long: None }
}
#[allow(clippy::too_many_arguments)]
const fn pl(
    id: &'static str,
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    threshold: u64,
    inclusive: bool,
    long: (f64, f64, f64, f64),
) -> Price {
    Price { id, input, output, cache_read, cache_write, long: Some(Long { threshold, inclusive, input: long.0, output: long.1, cache_read: long.2, cache_write: long.3 }) }
}

/// 100만 토큰당 USD. 근거는 이 PC의 omp 모델 카탈로그(`~/.omp/agent/models.db`의 `cost`) 항목이다.
/// 여기 없는 모델(예: Spark·알 수 없는 모델)은 가격을 만들지 않고 미환산으로 남긴다.
/// `-pro` 변종처럼 카탈로그가 기본 모델 값을 그대로 복사한 항목은 근거가 약해 넣지 않았다.
static PRICES: &[Price] = &[
    p("claude-3-5-sonnet-20240620", 3.0, 15.0, 0.3, 3.75),
    p("claude-3-5-sonnet-20241022", 3.0, 15.0, 0.3, 3.75),
    p("claude-3-haiku-20240307", 0.25, 1.25, 0.03, 0.3),
    p("claude-fable-5", 10.0, 50.0, 1.0, 12.5),
    p("claude-fable-5-1", 10.0, 50.0, 0.25, 12.5),
    p("claude-haiku-4-5", 1.0, 5.0, 0.1, 1.25),
    p("claude-haiku-4-5-20251001", 1.0, 5.0, 0.1, 1.25),
    pl("claude-haiku-5-5", 0.1, 0.5, 0.01, 0.125, 100_000, false, (0.5, 2.5, 0.05, 0.625)),
    p("claude-mythos-5", 10.0, 50.0, 1.0, 12.5),
    p("claude-mythos-5-1", 10.0, 50.0, 0.25, 12.5),
    p("claude-opus-4-0", 15.0, 75.0, 1.5, 18.75),
    p("claude-opus-4-1", 15.0, 75.0, 1.5, 18.75),
    p("claude-opus-4-1-20250805", 15.0, 75.0, 1.5, 18.75),
    p("claude-opus-4-20250514", 15.0, 75.0, 1.5, 18.75),
    p("claude-opus-4-5", 5.0, 25.0, 0.5, 6.25),
    p("claude-opus-4-5-20251101", 5.0, 25.0, 0.5, 6.25),
    p("claude-opus-4-6", 5.0, 25.0, 0.5, 6.25),
    p("claude-opus-4-7", 5.0, 25.0, 0.5, 6.25),
    p("claude-opus-4-8", 5.0, 25.0, 0.5, 6.25),
    p("claude-opus-5", 5.0, 25.0, 0.5, 6.25),
    p("claude-opus-5-5", 4.0, 20.0, 0.2, 5.0),
    p("claude-sonnet-4-0", 3.0, 15.0, 0.3, 3.75),
    p("claude-sonnet-4-20250514", 3.0, 15.0, 0.3, 3.75),
    p("claude-sonnet-4-5", 3.0, 15.0, 0.3, 3.75),
    p("claude-sonnet-4-5-20250929", 3.0, 15.0, 0.3, 3.75),
    p("claude-sonnet-4-6", 3.0, 15.0, 0.3, 3.75),
    p("claude-sonnet-5", 2.0, 10.0, 0.2, 2.5),
    p("claude-sonnet-5-5", 2.0, 10.0, 0.2, 2.5),
    p("gpt-5", 1.25, 10.0, 0.125, 0.0),
    p("gpt-5-codex", 1.25, 10.0, 0.125, 0.0),
    p("gpt-5-mini", 0.25, 2.0, 0.025, 0.0),
    p("gpt-5-nano", 0.05, 0.4, 0.005, 0.0),
    p("gpt-5.1", 1.25, 10.0, 0.125, 0.0),
    p("gpt-5.1-codex", 1.25, 10.0, 0.125, 0.0),
    p("gpt-5.1-codex-max", 1.25, 10.0, 0.125, 0.0),
    p("gpt-5.1-codex-mini", 0.25, 2.0, 0.025, 0.0),
    p("gpt-5.2", 1.75, 14.0, 0.175, 0.0),
    p("gpt-5.2-codex", 1.75, 14.0, 0.175, 0.0),
    p("gpt-5.3-codex", 1.75, 14.0, 0.175, 0.0),
    p("gpt-5.4", 2.5, 15.0, 0.25, 0.0),
    p("gpt-5.4-mini", 0.75, 4.5, 0.075, 0.0),
    p("gpt-5.4-nano", 0.2, 1.25, 0.02, 0.0),
    p("gpt-5.4-pro", 30.0, 180.0, 0.0, 0.0),
    p("gpt-5.5", 5.0, 30.0, 0.5, 0.0),
    p("gpt-5.5-pro", 30.0, 180.0, 0.0, 0.0),
    pl("gpt-5.6", 4.0, 20.0, 0.4, 5.0, 272_000, false, (10.0, 45.0, 1.0, 12.5)),
    pl("gpt-5.6-sol", 4.0, 20.0, 0.4, 5.0, 272_000, false, (10.0, 45.0, 1.0, 12.5)),
    pl("gpt-5.6-terra", 2.0, 12.0, 0.2, 2.5, 272_000, false, (4.0, 18.0, 0.4, 5.0)),
    pl("gpt-5.6-luna", 0.2, 1.2, 0.02, 0.25, 272_000, false, (0.4, 1.8, 0.04, 0.5)),
    pl("gpt-6-astra", 10.0, 50.0, 1.0, 12.5, 272_000, false, (20.0, 75.0, 2.0, 25.0)),
    p("gpt-6-luna", 0.1, 0.5, 0.01, 0.125),
    p("gpt-6-sol", 2.0, 10.0, 0.2, 2.5),
    p("gpt-6.1-sol", 2.0, 10.0, 0.1, 2.5),
    p("gemini-2.5-flash", 0.3, 2.5, 0.03, 0.0),
    p("gemini-2.5-flash-lite", 0.1, 0.4, 0.01, 0.0),
    p("gemini-3-flash", 0.5, 3.0, 0.05, 0.0),
    p("gemini-3.1-flash-lite", 0.25, 1.5, 0.025, 0.0),
    p("gemini-3.1-pro", 2.0, 12.0, 0.2, 0.0),
    p("gemini-3.5-flash", 1.5, 9.0, 0.15, 0.0),
    p("gemini-3.5-flash-lite", 0.3, 2.5, 0.03, 0.0),
    p("gemini-3.6-flash", 0.75, 3.75, 0.075, 0.0),
    p("gemini-3.7-flash", 0.75, 3.75, 0.075, 0.0),
    p("gemini-3.8-flash", 0.75, 3.75, 0.075, 0.0),
    pl("grok-build", 1.25, 2.5, 0.2, 0.0, 200_000, true, (2.5, 5.0, 0.4, 0.0)),
    pl("grok-build-0.1", 1.0, 2.0, 0.2, 0.0, 200_000, true, (2.0, 4.0, 0.4, 0.0)),
    pl("grok-4.3", 1.25, 2.5, 0.2, 0.0, 200_000, true, (2.5, 5.0, 0.4, 0.0)),
    pl("grok-4.5", 2.0, 6.0, 0.3, 0.0, 200_000, true, (4.0, 12.0, 0.6, 0.0)),
    pl("grok-4.6", 2.0, 6.0, 0.5, 0.0, 200_000, true, (4.0, 12.0, 1.0, 0.0)),
    pl("grok-4.7", 2.0, 6.0, 0.5, 0.0, 200_000, true, (4.0, 12.0, 1.0, 0.0)),
    pl("grok-4.20-multi-agent-0309", 2.0, 6.0, 0.2, 0.0, 200_000, true, (4.0, 12.0, 0.4, 0.0)),
    pl("grok-4.20-0309-reasoning", 1.25, 2.5, 0.2, 0.0, 200_000, true, (2.5, 5.0, 0.4, 0.0)),
    pl("grok-4.20-0309-non-reasoning", 1.25, 2.5, 0.2, 0.0, 200_000, true, (2.5, 5.0, 0.4, 0.0)),
    pl("grok-composer-2.5-fast", 1.25, 2.5, 0.2, 0.0, 200_000, true, (2.5, 5.0, 0.4, 0.0)),
];

fn lookup_price(label: &str) -> Option<&'static Price> {
    let id = label.rsplit('/').next()?.to_ascii_lowercase();
    let id = id.split('[').next().unwrap_or("").to_owned();
    let find = |name: &str| PRICES.iter().find(|price| price.id == name);
    if let Some(found) = find(&id) {
        return Some(found);
    }
    // `claude-sonnet-4-5-20250929` 같은 날짜 꼬리표는 날짜 없는 이름으로 한 번만 되돌려 본다.
    let (head, tail) = id.rsplit_once('-')?;
    if tail.len() == 8 && tail.bytes().all(|b| b.is_ascii_digit()) {
        return find(head);
    }
    None
}

// ───────────────────────────── 기록 해석 ─────────────────────────────

#[derive(Debug, Clone)]
struct Rec {
    key: i64,
    ts: i64,
    tool: u8,
    model: String,
    session: i64,
    inp: u64,
    out: u64,
    cr: u64,
    cw: u64,
    /// 5분보다 긴(1시간) 캐시 쓰기로 알려진 토큰. `cw`의 일부다.
    cw1h: u64,
    /// 긴 컨텍스트 요금 구간 판단에 쓰는 한 요청의 입력 크기.
    ctx: u64,
    recorded_cost: Option<f64>,
    flags: i64,
    cost: Option<f64>,
}

fn price_cost(model: &str, rec: &Rec) -> Option<f64> {
    let price = lookup_price(model)?;
    let long = price.long.as_ref().filter(|l| if l.inclusive { rec.ctx >= l.threshold } else { rec.ctx > l.threshold });
    let (i, o, cr, cw) = match long {
        Some(l) => (l.input, l.output, l.cache_read, l.cache_write),
        None => (price.input, price.output, price.cache_read, price.cache_write),
    };
    let cw5 = rec.cw.saturating_sub(rec.cw1h);
    let mut cost = (rec.inp as f64 * i + rec.out as f64 * o + rec.cr as f64 * cr + cw5 as f64 * cw) / 1e6;
    if rec.cw1h > 0 {
        // Anthropic 1시간 캐시 쓰기는 기본 입력 단가의 2배다.
        cost += rec.cw1h as f64 * i * 2.0 / 1e6;
    }
    Some(cost)
}

/// 토큰이 하나도 없으면 `None`(색인에 넣지 않는다). 시각·크기가 이상하면 제외 표시를 단다.
#[allow(clippy::too_many_arguments)]
fn make_rec(
    tool: u8,
    key: i64,
    ts: Option<i64>,
    model: String,
    session: i64,
    parts: (u64, u64, u64, u64),
    cw1h: u64,
    ctx: u64,
    recorded: Option<f64>,
    now_ms: i64,
) -> Option<Rec> {
    let (inp, out, cr, cw) = parts;
    if inp == 0 && out == 0 && cr == 0 && cw == 0 {
        return None;
    }
    let mut flags = 0;
    let ts = match ts {
        Some(t) if t > 0 => {
            if t > now_ms.saturating_add(FUTURE_SLACK_MS) {
                flags |= FLAG_FUTURE;
            }
            t
        }
        _ => {
            flags |= FLAG_TIME;
            0
        }
    };
    if inp.max(out).max(cr).max(cw) > MAX_COMPONENT {
        flags |= FLAG_IMPLAUSIBLE;
    }
    let mut rec = Rec { key, ts, tool, model, session, inp, out, cr, cw, cw1h: cw1h.min(cw), ctx, recorded_cost: recorded, flags, cost: None };
    rec.cost = rec.recorded_cost.filter(|c| *c > 0.0).or_else(|| price_cost(&rec.model, &rec));
    Some(rec)
}

#[derive(Default, Clone, Debug)]
struct Cursor {
    model: String,
    session: i64,
    c_in: u64,
    c_cache: u64,
    c_out: u64,
}

/// 비용 객체 안의 음이 아닌 유한한 숫자 한 칸. 모양이 다르면 `None`.
fn cost_field(cost: &Value, key: &str) -> Option<f64> {
    cost.get(key).and_then(Value::as_f64).filter(|f| f.is_finite() && *f >= 0.0)
}
#[derive(Deserialize, Default)]
struct OmpUsage {
    input: Option<Value>,
    output: Option<Value>,
    #[serde(rename = "cacheRead")]
    cache_read: Option<Value>,
    #[serde(rename = "cacheWrite")]
    cache_write: Option<Value>,
    cost: Option<Value>,
}
#[derive(Deserialize, Default)]
struct OmpMessage {
    role: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    #[serde(rename = "responseId")]
    response_id: Option<String>,
    timestamp: Option<Value>,
    usage: Option<OmpUsage>,
}
#[derive(Deserialize, Default)]
struct CompUsage {
    #[serde(rename = "inputTokens")]
    input: Option<Value>,
    #[serde(rename = "outputTokens")]
    output: Option<Value>,
    #[serde(rename = "cachedInputTokens")]
    cached: Option<Value>,
}
#[derive(Deserialize, Default)]
struct RemoteCompaction {
    usage: Option<CompUsage>,
}
#[derive(Deserialize, Default)]
struct Preserve {
    #[serde(rename = "openaiRemoteCompaction")]
    remote: Option<RemoteCompaction>,
}
#[derive(Deserialize, Default)]
struct OmpLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    timestamp: Option<Value>,
    message: Option<OmpMessage>,
    provider: Option<String>,
    model: Option<String>,
    usage: Option<OmpUsage>,
    #[serde(rename = "preserveData")]
    preserve: Option<Preserve>,
}

#[derive(Deserialize, Default)]
struct ClCache {
    ephemeral_1h_input_tokens: Option<Value>,
}
#[derive(Deserialize, Default)]
struct ClUsage {
    input_tokens: Option<Value>,
    output_tokens: Option<Value>,
    cache_read_input_tokens: Option<Value>,
    cache_creation_input_tokens: Option<Value>,
    cache_creation: Option<ClCache>,
}
#[derive(Deserialize, Default)]
struct ClMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<ClUsage>,
}
#[derive(Deserialize, Default)]
struct ClLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<Value>,
    #[serde(rename = "sessionId")]
    session: Option<String>,
    #[serde(rename = "requestId")]
    request: Option<String>,
    uuid: Option<String>,
    message: Option<ClMsg>,
}
/// 사용자·시스템 줄의 `message`는 문자열일 수 있다. 그런 줄 때문에 파싱이 실패해 부분 읽기로 보이지 않게 한다.
#[derive(Deserialize)]
#[serde(untagged)]
enum ClMsg {
    Msg(ClMessage),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize, Default)]
struct CxUsage {
    input_tokens: Option<Value>,
    cached_input_tokens: Option<Value>,
    output_tokens: Option<Value>,
    reasoning_output_tokens: Option<Value>,
}
#[derive(Deserialize, Default)]
struct CxInfo {
    total_token_usage: Option<CxUsage>,
    last_token_usage: Option<CxUsage>,
}
#[derive(Deserialize, Default)]
struct CxPayload {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    model: Option<String>,
    info: Option<CxInfo>,
}
#[derive(Deserialize, Default)]
struct CxLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<Value>,
    payload: Option<CxPayload>,
}

/// JSON 문자열의 짝 없는 서로게이트 이스케이프(`\ud83d` 단독 등)를 `\ufffd`로 바꾼다. 바꿀 것이 없으면 `None`.
/// 일부 기록에는 이런 줄이 있어 serde_json이 줄 전체를 거부한다. 숫자 필드는 영향이 없으니 고쳐서 다시 읽는다.
fn repair_surrogates(line: &[u8]) -> Option<Vec<u8>> {
    let hex = |slice: &[u8]| -> Option<u32> {
        if slice.len() != 4 {
            return None;
        }
        let mut v = 0u32;
        for b in slice {
            v = v * 16 + (*b as char).to_digit(16)?;
        }
        Some(v)
    };
    let mut out = Vec::with_capacity(line.len());
    let mut changed = false;
    let mut i = 0;
    while i < line.len() {
        if line[i] != b'\\' || i + 1 >= line.len() {
            out.push(line[i]);
            i += 1;
            continue;
        }
        if line[i + 1] != b'u' {
            out.extend_from_slice(&line[i..i + 2]);
            i += 2;
            continue;
        }
        let Some(v) = line.get(i + 2..i + 6).and_then(hex) else {
            out.extend_from_slice(&line[i..i + 2]);
            i += 2;
            continue;
        };
        if (0xD800..=0xDBFF).contains(&v) {
            let low = line
                .get(i + 6..i + 12)
                .filter(|s| s[0] == b'\\' && s[1] == b'u')
                .and_then(|s| hex(&s[2..6]))
                .filter(|l| (0xDC00..=0xDFFF).contains(l));
            if low.is_some() {
                out.extend_from_slice(&line[i..i + 12]);
                i += 12;
                continue;
            }
            out.extend_from_slice(b"\\ufffd");
            changed = true;
            i += 6;
        } else if (0xDC00..=0xDFFF).contains(&v) {
            out.extend_from_slice(b"\\ufffd");
            changed = true;
            i += 6;
        } else {
            out.extend_from_slice(&line[i..i + 6]);
            i += 6;
        }
    }
    changed.then_some(out)
}

fn decode<T: serde::de::DeserializeOwned>(line: &[u8]) -> Option<T> {
    if let Ok(value) = serde_json::from_slice::<T>(line) {
        return Some(value);
    }
    repair_surrogates(line).and_then(|fixed| serde_json::from_slice::<T>(&fixed).ok())
}

fn has(line: &[u8], needle: &[u8]) -> bool {
    memchr::memmem::find(line, needle).is_some()
}

/// JSON 파싱 전에 이 도구에서 쓸 수 있는 줄인지 바이트로만 거른다.
fn wanted(tool: u8, line: &[u8]) -> bool {
    match tool {
        TOOL_OMP => has(line, b"\"usage\"") || has(line, b"model_change") || has(line, b"\"type\":\"session\""),
        TOOL_CLAUDE => has(line, b"\"usage\""),
        _ => has(line, b"token_count") || has(line, b"turn_context") || has(line, b"session_meta"),
    }
}

fn omp_record(
    u: &OmpUsage,
    label: String,
    response_id: Option<&str>,
    entry_id: Option<&str>,
    ts: Option<i64>,
    session: i64,
    now_ms: i64,
) -> Option<Rec> {
    let parts = (num(&u.input), num(&u.output), num(&u.cache_read), num(&u.cache_write));
    let recorded = u.cost.as_ref().and_then(|c| {
        cost_field(c, "total").or_else(|| {
            let sum: f64 = ["input", "output", "cacheRead", "cacheWrite"].iter().filter_map(|k| cost_field(c, k)).sum();
            (sum > 0.0).then_some(sum)
        })
    });
    let stamp = ts.unwrap_or(0).to_string();
    let key = match response_id.filter(|r| !r.is_empty()) {
        // 같은 응답은 도구와 상관없이 같은 키를 쓴다. 가져온(import)·이어받은 기록과 Claude의 같은 메시지 ID가 한 번만 센다.
        Some(r) => hash_strs(&["r", r]),
        None => match entry_id.filter(|r| !r.is_empty()) {
            Some(id) => hash_strs(&["e", id, &stamp]),
            None => hash_strs(&["n", &label, &stamp, &parts.0.to_string(), &parts.1.to_string(), &parts.2.to_string(), &parts.3.to_string()]),
        },
    };
    let ctx = parts.0 + parts.2 + parts.3;
    make_rec(TOOL_OMP, key, ts, label, session, parts, 0, ctx, recorded, now_ms)
}

fn parse_omp(line: &[u8], cur: &mut Cursor, now_ms: i64) -> Result<Option<Rec>, ()> {
    let v: OmpLine = decode(line).ok_or(())?;
    match v.kind.as_deref().unwrap_or("") {
        "session" => {
            if let Some(id) = v.id.as_deref().filter(|s| !s.is_empty()) {
                cur.session = hash_strs(&["s", id]);
            }
            Ok(None)
        }
        "model_change" => {
            if let Some(model) = v.model.as_deref() {
                let label = sanitize_model(model);
                if label != "unknown" {
                    cur.model = label;
                }
            }
            Ok(None)
        }
        "message" => {
            let Some(m) = v.message.as_ref() else { return Ok(None) };
            if m.role.as_deref() != Some("assistant") {
                return Ok(None);
            }
            let label = model_label(m.provider.as_deref(), m.model.as_deref());
            if label != "unknown" {
                cur.model = label.clone();
            }
            let Some(u) = m.usage.as_ref() else { return Ok(None) };
            let ts = ts_ms(&v.timestamp).or_else(|| ts_ms(&m.timestamp));
            Ok(omp_record(u, label, m.response_id.as_deref(), v.id.as_deref(), ts, cur.session, now_ms))
        }
        "model_usage" => {
            let Some(u) = v.usage.as_ref() else { return Ok(None) };
            let label = model_label(v.provider.as_deref(), v.model.as_deref());
            Ok(omp_record(u, label, None, v.id.as_deref(), ts_ms(&v.timestamp), cur.session, now_ms))
        }
        "compaction" => {
            let Some(u) = v.preserve.as_ref().and_then(|p| p.remote.as_ref()).and_then(|r| r.usage.as_ref()) else { return Ok(None) };
            // 압축 호출은 모델 이름이 없어 그 시점의 모델을 쓴다. OpenAI는 입력에 캐시가 들어 있어 뺀다.
            let cached = num(&u.cached);
            let inp = num(&u.input).saturating_sub(cached);
            let ts = ts_ms(&v.timestamp);
            let model = if cur.model.is_empty() { "unknown".to_owned() } else { cur.model.clone() };
            let stamp = ts.unwrap_or(0).to_string();
            let key = hash_strs(&["c", v.id.as_deref().unwrap_or(""), &stamp]);
            Ok(make_rec(TOOL_OMP, key, ts, model, cur.session, (inp, num(&u.output), cached, 0), 0, inp + cached, None, now_ms))
        }
        _ => Ok(None),
    }
}

fn parse_claude(line: &[u8], cur: &mut Cursor, now_ms: i64) -> Result<Option<Rec>, ()> {
    let v: ClLine = decode(line).ok_or(())?;
    if v.kind.as_deref() != Some("assistant") {
        return Ok(None);
    }
    let Some(ClMsg::Msg(m)) = v.message.as_ref() else { return Ok(None) };
    let Some(u) = m.usage.as_ref() else { return Ok(None) };
    let model = m.model.as_deref().map(sanitize_model).unwrap_or_else(|| "unknown".into());
    if let Some(session) = v.session.as_deref().filter(|s| !s.is_empty()) {
        cur.session = hash_strs(&["s", session]);
    }
    let cw = num(&u.cache_creation_input_tokens);
    let cw1h = u.cache_creation.as_ref().map_or(0, |c| num(&c.ephemeral_1h_input_tokens));
    let parts = (num(&u.input_tokens), num(&u.output_tokens), num(&u.cache_read_input_tokens), cw);
    let key = if let Some(id) = m.id.as_deref().filter(|s| !s.is_empty()) {
        hash_strs(&["r", id])
    } else if let Some(id) = v.request.as_deref().filter(|s| !s.is_empty()) {
        hash_strs(&["q", id])
    } else if let Some(id) = v.uuid.as_deref().filter(|s| !s.is_empty()) {
        hash_strs(&["u", id])
    } else {
        return Ok(None);
    };
    let ctx = parts.0 + parts.2 + parts.3;
    Ok(make_rec(TOOL_CLAUDE, key, ts_ms(&v.timestamp), model, cur.session, parts, cw1h, ctx, None, now_ms))
}

fn parse_codex(line: &[u8], cur: &mut Cursor, now_ms: i64) -> Result<Option<Rec>, ()> {
    let v: CxLine = decode(line).ok_or(())?;
    let Some(payload) = v.payload.as_ref() else { return Ok(None) };
    match v.kind.as_deref().unwrap_or("") {
        "session_meta" => {
            if let Some(id) = payload.id.as_deref().filter(|s| !s.is_empty()) {
                cur.session = hash_strs(&["s", id]);
            }
            Ok(None)
        }
        "turn_context" => {
            if let Some(model) = payload.model.as_deref() {
                cur.model = sanitize_model(model);
            }
            Ok(None)
        }
        "event_msg" if payload.kind.as_deref() == Some("token_count") => {
            let Some(info) = payload.info.as_ref() else { return Ok(None) };
            let Some(total) = info.total_token_usage.as_ref() else { return Ok(None) };
            let (t_in, t_cache, t_out) = (num(&total.input_tokens), num(&total.cached_input_tokens), num(&total.output_tokens));
            // 포크·이어받은 파일은 부모의 누적값을 지닌 채 시작한다(Codex가 새 시각으로 다시 기록한다).
            // 이 파일에서 처음 보는 누적값이 직전 요청분(last)보다 크면 차이는 부모 몫이므로 기준선으로만 삼는다.
            let fresh_file = cur.c_in == 0 && cur.c_cache == 0 && cur.c_out == 0;
            let (b_in, b_cache, b_out) = match info.last_token_usage.as_ref().filter(|_| fresh_file) {
                Some(last) => (
                    t_in.saturating_sub(num(&last.input_tokens)),
                    t_cache.saturating_sub(num(&last.cached_input_tokens)),
                    t_out.saturating_sub(num(&last.output_tokens)),
                ),
                None => (cur.c_in, cur.c_cache, cur.c_out),
            };
            // 누적 카운터가 이전 최댓값을 넘은 만큼만 센다. 같은 값이 반복되거나 줄어들면 새로 센 것이 없다.
            let d_in = t_in.saturating_sub(b_in);
            let d_cache = t_cache.saturating_sub(b_cache);
            let d_out = t_out.saturating_sub(b_out);
            cur.c_in = cur.c_in.max(t_in);
            cur.c_cache = cur.c_cache.max(t_cache);
            cur.c_out = cur.c_out.max(t_out);
            // Codex의 input_tokens에는 캐시 입력이 들어 있다. 캐시를 따로 세므로 입력에서 뺀다. 추론 토큰은 output_tokens에 이미 있다.
            let inp = d_in.saturating_sub(d_cache);
            let ts = ts_ms(&v.timestamp);
            // 포크한 Codex 기록은 부모 이벤트를 새 시각으로 다시 쓴다. 누적값·직전 요청값이 같으면 같은 이벤트이므로 시각은 키에 넣지 않는다.
            let last = info.last_token_usage.as_ref();
            let key = hash_strs(&["x", &t_in.to_string(), &t_cache.to_string(), &t_out.to_string(), &num(&total.reasoning_output_tokens).to_string(),
                &last.map_or(0, |l| num(&l.input_tokens)).to_string(), &last.map_or(0, |l| num(&l.output_tokens)).to_string()]);
            let ctx = info.last_token_usage.as_ref().map_or(d_in, |l| num(&l.input_tokens));
            let model = if cur.model.is_empty() { "unknown".to_owned() } else { cur.model.clone() };
            Ok(make_rec(TOOL_CODEX, key, ts, model, cur.session, (inp, d_out, d_cache, 0), 0, ctx, None, now_ms))
        }
        _ => Ok(None),
    }
}

/// 사용량을 담는 종류의 줄인가(바이트 검사). 이런 줄이 해석되지 않을 때만 읽기 오류로 센다.
fn core_line(tool: u8, line: &[u8]) -> bool {
    match tool {
        TOOL_OMP => has(line, b"\"type\":\"message\"") || has(line, b"\"type\":\"model_usage\"") || has(line, b"\"type\":\"compaction\""),
        TOOL_CLAUDE => has(line, b"\"type\":\"assistant\""),
        _ => has(line, b"\"type\":\"token_count\""),
    }
}

fn parse_line(tool: u8, line: &[u8], cur: &mut Cursor, now_ms: i64) -> Result<Option<Rec>, ()> {
    if !wanted(tool, line) {
        return Ok(None);
    }
    let parsed = match tool {
        TOOL_OMP => parse_omp(line, cur, now_ms),
        TOOL_CLAUDE => parse_claude(line, cur, now_ms),
        _ => parse_codex(line, cur, now_ms),
    };
    match parsed {
        Err(()) if !core_line(tool, line) => Ok(None),
        other => other,
    }
}

// ───────────────────────────── 파일 읽기 ─────────────────────────────

/// 한 줄을 읽는다. 반환은 (소비한 바이트, 줄바꿈 만남, 상한 초과). 상한을 넘은 줄은 버리면서 끝까지 건너뛴다.
fn read_line<R: BufRead>(reader: &mut R, buf: &mut Vec<u8>, cap: usize) -> io::Result<(u64, bool, bool)> {
    buf.clear();
    let (mut total, mut overflow) = (0u64, false);
    loop {
        let available = match reader.fill_buf() {
            Ok(a) => a,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok((total, false, overflow));
        }
        match memchr::memchr(b'\n', available) {
            Some(i) => {
                if !overflow {
                    if buf.len() + i > cap {
                        overflow = true;
                        buf.clear();
                    } else {
                        buf.extend_from_slice(&available[..i]);
                    }
                }
                reader.consume(i + 1);
                return Ok((total + i as u64 + 1, true, overflow));
            }
            None => {
                let n = available.len();
                if !overflow {
                    if buf.len() + n > cap {
                        overflow = true;
                        buf.clear();
                    } else {
                        buf.extend_from_slice(available);
                    }
                }
                reader.consume(n);
                total += n as u64;
            }
        }
    }
}

fn seek_to(file: &fs::File, pos: u64) -> io::Result<()> {
    let mut f = file;
    f.seek(SeekFrom::Start(pos)).map(|_| ())
}

/// 읽은 위치 바로 앞 최대 4KiB의 해시. 파일이 이어 쓰인 것인지 덮어쓰인 것인지 가려 낸다.
/// 앞부분(omp의 제목 줄처럼 제자리에서 바뀌는 곳)은 보지 않는다.
fn window_hash(file: &fs::File, pos: u64) -> io::Result<i64> {
    if pos == 0 {
        return Ok(0);
    }
    let n = pos.min(TAIL_WINDOW);
    seek_to(file, pos - n)?;
    let mut buf = vec![0u8; n as usize];
    let mut f = file;
    f.read_exact(&mut buf)?;
    Ok(hash_parts(&[buf.as_slice()]) as i64)
}

fn mtime_ms(meta: &fs::Metadata) -> i64 {
    meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64)
}

/// 줄바꿈 없이 끝난 마지막 줄이 이미 완결된 JSON 객체이면 받아들인다(쓰는 중인 줄은 닫는 괄호가 없다).
fn complete_tail(line: &[u8]) -> bool {
    let t = line.trim_ascii();
    t.first() == Some(&b'{') && t.last() == Some(&b'}') && serde_json::from_slice::<serde::de::IgnoredAny>(t).is_ok()
}

// ───────────────────────────── 색인 DB ─────────────────────────────

const DDL: &str = "
CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v INTEGER NOT NULL) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS models(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE IF NOT EXISTS files(
  id INTEGER PRIMARY KEY, path_hash INTEGER NOT NULL UNIQUE, tool INTEGER NOT NULL,
  state INTEGER NOT NULL DEFAULT 0, size INTEGER NOT NULL DEFAULT 0, mtime INTEGER NOT NULL DEFAULT 0,
  pos INTEGER NOT NULL DEFAULT 0, p_mtime INTEGER NOT NULL DEFAULT -1, tail INTEGER NOT NULL DEFAULT 0,
  wait INTEGER NOT NULL DEFAULT -1, fail_size INTEGER NOT NULL DEFAULT -1, fail_mtime INTEGER NOT NULL DEFAULT -1,
  cur_model TEXT NOT NULL DEFAULT '', session INTEGER NOT NULL DEFAULT 0,
  c_in INTEGER NOT NULL DEFAULT 0, c_cache INTEGER NOT NULL DEFAULT 0, c_out INTEGER NOT NULL DEFAULT 0,
  bad INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS records(
  rk INTEGER PRIMARY KEY, file_id INTEGER NOT NULL, ts INTEGER NOT NULL, tool INTEGER NOT NULL,
  model_id INTEGER NOT NULL, session INTEGER NOT NULL, inp INTEGER NOT NULL, outp INTEGER NOT NULL,
  cr INTEGER NOT NULL, cw INTEGER NOT NULL, cost REAL, flags INTEGER NOT NULL DEFAULT 0, dups INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS records_ts ON records(ts);
CREATE INDEX IF NOT EXISTS records_flagged ON records(ts) WHERE flags <> 0;
";

const UPSERT: &str = "INSERT INTO records(rk,file_id,ts,tool,model_id,session,inp,outp,cr,cw,cost,flags,dups)
 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,0)
 ON CONFLICT(rk) DO UPDATE SET
  dups = dups + CASE WHEN ?13 = 1 THEN 0 ELSE 1 END,
  inp = CASE WHEN excluded.outp > outp THEN excluded.inp ELSE inp END,
  cr = CASE WHEN excluded.outp > outp THEN excluded.cr ELSE cr END,
  cw = CASE WHEN excluded.outp > outp THEN excluded.cw ELSE cw END,
  cost = CASE WHEN excluded.outp > outp THEN excluded.cost ELSE cost END,
  flags = CASE WHEN excluded.outp > outp THEN excluded.flags ELSE flags END,
  outp = CASE WHEN excluded.outp > outp THEN excluded.outp ELSE outp END";

/// 아직 할 일이 있는 파일(읽을 새 바이트, 줄어듦, 수정 시각 변화, 크기·시각이 바뀐 읽기 실패 파일)의 SQL 조건.
const PENDING_SQL: &str = "((state = 0 AND (size < pos OR (size > pos AND wait <> size) OR (size = pos AND mtime <> p_mtime))) \
 OR (state = 1 AND (size <> fail_size OR mtime <> fail_mtime)))";

fn set_meta(conn: &Connection, key: &str, value: i64) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO meta(k,v) VALUES(?1,?2) ON CONFLICT(k) DO UPDATE SET v=excluded.v", params![key, value]).map(|_| ())
}
fn get_meta(conn: &Connection, key: &str) -> i64 {
    conn.query_row("SELECT v FROM meta WHERE k=?1", params![key], |r| r.get(0)).unwrap_or(0)
}

fn init_db(conn: &Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(Duration::from_millis(3000))?;
    conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get::<_, String>(0))?;
    conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA cache_size=-32768;")?;
    let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version != INDEX_VERSION {
        // 해석 규칙·가격표가 바뀌었다. 색인은 원본에서 다시 만들 수 있는 파생 데이터라 비운다.
        conn.execute_batch("DROP TABLE IF EXISTS records; DROP TABLE IF EXISTS files; DROP TABLE IF EXISTS models; DROP TABLE IF EXISTS meta;")?;
        conn.execute_batch(DDL)?;
        conn.pragma_update(None, "user_version", INDEX_VERSION)?;
    } else {
        conn.execute_batch(DDL)?;
    }
    Ok(())
}

#[derive(Clone)]
struct FileRow {
    id: i64,
    tool: u8,
    path: Option<PathBuf>,
    /// 0 정상, 1 읽기 실패(크기·시각이 바뀔 때까지 보류), 2 기록 폴더에서 사라짐.
    state: u8,
    size: i64,
    mtime: i64,
    pos: i64,
    p_mtime: i64,
    tail: i64,
    wait: i64,
    fail_size: i64,
    fail_mtime: i64,
    cur: Cursor,
    bad: i64,
}

fn pending(row: &FileRow) -> bool {
    if row.path.is_none() || row.state == 2 {
        return false;
    }
    if row.state == 1 {
        return row.size != row.fail_size || row.mtime != row.fail_mtime;
    }
    if row.size < row.pos {
        return true;
    }
    if row.size > row.pos {
        return row.wait != row.size;
    }
    row.mtime != row.p_mtime
}

#[derive(Clone, Debug)]
struct Source {
    tool: u8,
    root: PathBuf,
}
#[derive(Clone, Debug, Default)]
struct SourceSet {
    sources: Vec<Source>,
    /// 있는데 열 수 없던 폴더 수.
    bad_roots: i64,
}
impl SourceSet {
    fn signature(&self) -> i64 {
        let mut text = String::new();
        for s in &self.sources {
            text.push_str(&format!("{}:{}\n", s.tool, s.root.to_string_lossy()));
        }
        hash_strs(&[text.as_str()])
    }
}

struct Found {
    tool: u8,
    path: PathBuf,
    size: i64,
    mtime: i64,
}

fn skip_dir(tool: u8, name: &str) -> bool {
    // omp 세션 폴더 안 `local/`은 에이전트가 만든 산출물이라 사용 기록이 없고 크다.
    tool == TOOL_OMP && name == "local"
}

fn walk(source: &Source, out: &mut Vec<Found>, bad: &mut i64, truncated: &mut bool) {
    let mut stack = vec![(source.root.clone(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                if e.kind() != io::ErrorKind::NotFound {
                    *bad += 1;
                }
                continue;
            }
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if kind.is_dir() {
                if depth < MAX_DEPTH && !skip_dir(source.tool, &name) {
                    stack.push((entry.path(), depth + 1));
                }
            } else if kind.is_file() && name.len() > 6 && name.as_bytes()[name.len() - 6..].eq_ignore_ascii_case(b".jsonl") {
                if out.len() >= MAX_FILES {
                    *truncated = true;
                    return;
                }
                let Ok(meta) = entry.metadata() else {
                    *bad += 1;
                    continue;
                };
                out.push(Found { tool: source.tool, path: entry.path(), size: meta.len() as i64, mtime: mtime_ms(&meta) });
            }
        }
    }
}

enum IngestError {
    /// 이 파일만 읽지 못했다.
    File,
    Db(rusqlite::Error),
}
impl From<rusqlite::Error> for IngestError {
    fn from(e: rusqlite::Error) -> Self {
        IngestError::Db(e)
    }
}
impl From<io::Error> for IngestError {
    fn from(_: io::Error) -> Self {
        IngestError::File
    }
}

#[derive(Clone, Copy)]
struct Budget {
    bytes: u64,
    time: Duration,
}
/// 한 번 훑은 결과. `bytes`·`files`는 시험과 진단에서 읽는다.
#[allow(dead_code)]
#[derive(Default, Debug)]
struct PassInfo {
    bytes: u64,
    files: usize,
    pending: usize,
}

struct Index {
    conn: Connection,
    files: Vec<FileRow>,
    by_hash: HashMap<i64, usize>,
    models: HashMap<String, i64>,
    last_enum: Option<Instant>,
    set_sig: i64,
}

fn intern_model(conn: &Connection, models: &mut HashMap<String, i64>, name: &str) -> rusqlite::Result<i64> {
    if let Some(id) = models.get(name) {
        return Ok(*id);
    }
    conn.execute("INSERT OR IGNORE INTO models(name) VALUES(?1)", params![name])?;
    let id: i64 = conn.query_row("SELECT id FROM models WHERE name=?1", params![name], |r| r.get(0))?;
    models.insert(name.to_owned(), id);
    Ok(id)
}

/// 사용 기록 파일을 연다. 링크(Unix symlink, Windows 재분석 지점)는 따라가지 않는다.
/// 하드 링크는 거부하지 않는다: Orca 같은 실행기가 Codex 기록을 하드 링크로 두 곳에 두는데, 같은 기록은
/// 응답·누적값 키로 한 번만 세므로 두 경로로 읽어도 합계가 늘지 않는다. 읽기 전용이며 자격 증명 파일이 아니다.
#[cfg(unix)]
fn open_log(path: &Path) -> io::Result<fs::File> {
    aam_protocol::secure::open_read_no_follow(path)
}
#[cfg(windows)]
fn open_log(path: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    const FILE_SHARE_ALL: u32 = 0x1 | 0x2 | 0x4;
    // 쓰는 중인 기록도 읽을 수 있게 읽기·쓰기·삭제 공유로 연다(실행 중인 omp·Codex를 막지 않는다).
    let file = fs::OpenOptions::new().read(true).share_mode(FILE_SHARE_ALL).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path)?;
    if file.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "link"));
    }
    Ok(file)
}

impl Index {
    fn open(path: &Path) -> rusqlite::Result<Index> {
        match Self::open_once(path) {
            Ok(index) => Ok(index),
            Err(error) => {
                let broken = matches!(
                    error.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase)
                );
                if !broken {
                    return Err(error);
                }
                // 색인 파일이 깨졌다. 원본에서 다시 만들 수 있으므로 지우고 새로 시작한다.
                for suffix in ["", "-wal", "-shm"] {
                    let mut name = path.as_os_str().to_owned();
                    name.push(suffix);
                    let _ = fs::remove_file(PathBuf::from(name));
                }
                Self::open_once(path)
            }
        }
    }

    fn open_once(path: &Path) -> rusqlite::Result<Index> {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        init_db(&conn)?;
        let _ = aam_protocol::secure::restrict_file(path);
        let mut models = HashMap::new();
        {
            let mut stmt = conn.prepare("SELECT id,name FROM models")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (id, name) = row?;
                models.insert(name, id);
            }
        }
        let mut files = Vec::new();
        let mut by_hash = HashMap::new();
        {
            let mut stmt = conn.prepare(
                "SELECT id,path_hash,tool,state,size,mtime,pos,p_mtime,tail,wait,fail_size,fail_mtime,cur_model,session,c_in,c_cache,c_out,bad FROM files",
            )?;
            let rows = stmt.query_map([], |r| {
                let hash: i64 = r.get(1)?;
                Ok((
                    hash,
                    FileRow {
                        id: r.get(0)?,
                        tool: r.get::<_, i64>(2)? as u8,
                        path: None,
                        state: r.get::<_, i64>(3)? as u8,
                        size: r.get(4)?,
                        mtime: r.get(5)?,
                        pos: r.get(6)?,
                        p_mtime: r.get(7)?,
                        tail: r.get(8)?,
                        wait: r.get(9)?,
                        fail_size: r.get(10)?,
                        fail_mtime: r.get(11)?,
                        cur: Cursor {
                            model: r.get(12)?,
                            session: r.get(13)?,
                            c_in: r.get::<_, i64>(14)? as u64,
                            c_cache: r.get::<_, i64>(15)? as u64,
                            c_out: r.get::<_, i64>(16)? as u64,
                        },
                        bad: r.get(17)?,
                    },
                ))
            })?;
            for row in rows {
                let (hash, file) = row?;
                by_hash.insert(hash, files.len());
                files.push(file);
            }
        }
        Ok(Index { conn, files, by_hash, models, last_enum: None, set_sig: 0 })
    }

    /// 기록 폴더를 훑어 파일 목록(크기·수정 시각)을 갱신한다. 파일 내용은 읽지 않는다.
    fn enumerate(&mut self, set: &SourceSet) -> rusqlite::Result<()> {
        let mut found = Vec::new();
        let (mut bad, mut truncated) = (set.bad_roots, false);
        for source in &set.sources {
            walk(source, &mut found, &mut bad, &mut truncated);
        }
        let tx = self.conn.unchecked_transaction()?;
        let mut seen = vec![false; self.files.len()];
        for f in found {
            let lossy = f.path.to_string_lossy();
            let hash = hash_strs(&[lossy.as_ref()]);
            match self.by_hash.get(&hash).copied() {
                Some(i) => {
                    seen[i] = true;
                    let row = &mut self.files[i];
                    row.path = Some(f.path);
                    let revived = row.state == 2;
                    if revived {
                        row.state = 0;
                    }
                    if revived || row.size != f.size || row.mtime != f.mtime {
                        row.size = f.size;
                        row.mtime = f.mtime;
                        tx.execute("UPDATE files SET size=?1, mtime=?2, state=?3 WHERE id=?4", params![f.size, f.mtime, i64::from(row.state), row.id])?;
                    }
                }
                None => {
                    tx.execute("INSERT INTO files(path_hash,tool,size,mtime) VALUES(?1,?2,?3,?4)", params![hash, i64::from(f.tool), f.size, f.mtime])?;
                    let id = tx.last_insert_rowid();
                    self.by_hash.insert(hash, self.files.len());
                    self.files.push(FileRow {
                        id,
                        tool: f.tool,
                        path: Some(f.path),
                        state: 0,
                        size: f.size,
                        mtime: f.mtime,
                        pos: 0,
                        p_mtime: -1,
                        tail: 0,
                        wait: -1,
                        fail_size: -1,
                        fail_mtime: -1,
                        cur: Cursor::default(),
                        bad: 0,
                    });
                    seen.push(true);
                }
            }
        }
        for (i, was_seen) in seen.iter().enumerate() {
            if *was_seen {
                continue;
            }
            let row = &mut self.files[i];
            row.path = None;
            if row.state != 2 {
                row.state = 2;
                tx.execute("UPDATE files SET state=2 WHERE id=?1", params![row.id])?;
            }
        }
        set_meta(&tx, "enum_ms", aam_protocol::now_ms())?;
        set_meta(&tx, "roots_found", set.sources.len() as i64)?;
        set_meta(&tx, "roots_bad", bad)?;
        set_meta(&tx, "enum_truncated", i64::from(truncated))?;
        tx.commit()
    }

    fn refresh(&mut self, set: &SourceSet, now_ms: i64, budget: Budget, enum_every: Duration) -> rusqlite::Result<PassInfo> {
        let started = Instant::now();
        let signature = set.signature();
        if signature != self.set_sig || self.last_enum.map_or(true, |t| t.elapsed() >= enum_every) {
            self.enumerate(set)?;
            self.set_sig = signature;
            self.last_enum = Some(Instant::now());
        }
        // 최근에 바뀐 파일부터 읽어 오늘·이번 달 숫자가 먼저 차게 한다.
        let mut order: Vec<usize> = (0..self.files.len()).filter(|i| pending(&self.files[*i])).collect();
        order.sort_by(|a, b| self.files[*b].mtime.cmp(&self.files[*a].mtime));
        let mut left = budget.bytes;
        let mut info = PassInfo::default();
        for i in order {
            if left == 0 || started.elapsed() >= budget.time {
                break;
            }
            let before = left;
            match self.ingest(i, &mut left, started + budget.time, now_ms) {
                Ok(()) => {}
                Err(IngestError::File) => {
                    // ingest의 트랜잭션이 롤백되면 그 안에서 얻은 모델 ID도 무효다.
                    self.models.clear();
                    self.mark_unreadable(i)?;
                }
                Err(IngestError::Db(e)) => {
                    self.models.clear();
                    return Err(e);
                }
            }
            info.bytes += before - left;
            info.files += 1;
        }
        set_meta(&self.conn, "last_pass_ms", now_ms)?;
        info.pending = self.files.iter().filter(|r| pending(r)).count();
        Ok(info)
    }

    fn mark_unreadable(&mut self, idx: usize) -> rusqlite::Result<()> {
        let row = &mut self.files[idx];
        row.state = 1;
        row.fail_size = row.size;
        row.fail_mtime = row.mtime;
        self.conn.execute("UPDATE files SET state=1, fail_size=?1, fail_mtime=?2 WHERE id=?3", params![row.size, row.mtime, row.id])?;
        Ok(())
    }

    /// 파일의 새 바이트를 읽어 색인에 넣는다. 읽기 위치·상태 갱신과 기록 삽입은 한 트랜잭션이다.
    fn ingest(&mut self, idx: usize, bytes_left: &mut u64, deadline: Instant, now_ms: i64) -> Result<(), IngestError> {
        let mut row = self.files[idx].clone();
        let path = row.path.clone().ok_or(IngestError::File)?;
        let file = open_log(&path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(IngestError::File);
        }
        let size = meta.len();
        let mtime = mtime_ms(&meta);

        let mut rescan = false;
        let mut reset = row.pos as u64 > size;
        if !reset && row.pos > 0 {
            reset = window_hash(&file, row.pos as u64)? != row.tail;
        }
        if reset {
            // 줄어들었거나 읽은 위치 앞이 덮어쓰였다. 기록은 키로 합쳐지므로 지우지 않고 처음부터 다시 읽는다.
            row.pos = 0;
            row.tail = 0;
            row.wait = -1;
            row.cur = Cursor::default();
            rescan = true;
        }
        seek_to(&file, row.pos as u64)?;
        if row.cur.session == 0 {
            row.cur.session = row.id.wrapping_mul(0x9E37_79B9) ^ 0x5bd1_e995;
        }

        let limit = size.saturating_sub(row.pos as u64);
        let mut reader = BufReader::with_capacity(256 * 1024, (&file).take(limit));
        let tx = self.conn.unchecked_transaction()?;
        let mut pos = row.pos as u64;
        let mut cur = row.cur.clone();
        let (mut bad, mut at_eof, mut lines) = (0i64, false, 0u32);
        let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
        {
            let mut stmt = tx.prepare_cached(UPSERT)?;
            loop {
                if *bytes_left == 0 || (lines & 0xff == 0 && Instant::now() >= deadline) {
                    break;
                }
                let (consumed, newline, over) = read_line(&mut reader, &mut buf, MAX_LINE)?;
                if consumed == 0 {
                    at_eof = true;
                    break;
                }
                lines = lines.wrapping_add(1);
                if !newline {
                    // 파일 끝의 줄바꿈 없는 줄. 쓰는 중일 수 있으므로 완결된 JSON 객체일 때만 소비한다.
                    at_eof = true;
                    if over || !complete_tail(&buf) {
                        break;
                    }
                }
                pos += consumed;
                *bytes_left = bytes_left.saturating_sub(consumed);
                if over {
                    bad += 1;
                    continue;
                }
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
                if buf.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match parse_line(row.tool, &buf, &mut cur, now_ms) {
                    Ok(Some(rec)) => {
                        let model_id = intern_model(&tx, &mut self.models, &rec.model)?;
                        stmt.execute(params![
                            rec.key,
                            row.id,
                            rec.ts,
                            i64::from(rec.tool),
                            model_id,
                            rec.session,
                            rec.inp as i64,
                            rec.out as i64,
                            rec.cr as i64,
                            rec.cw as i64,
                            rec.cost,
                            rec.flags,
                            i64::from(rescan),
                        ])?;
                    }
                    Ok(None) => {}
                    Err(()) => bad += 1,
                }
                if !newline {
                    break;
                }
            }
        }
        drop(reader);
        let tail = window_hash(&file, pos)?;
        row.pos = pos as i64;
        row.tail = tail;
        // 끝까지 읽었는데 남은 바이트가 있으면 쓰는 중인 줄이다. 파일이 더 자랄 때까지 기다린다.
        row.wait = if at_eof && pos < size { size as i64 } else { -1 };
        row.size = size as i64;
        row.mtime = mtime;
        row.p_mtime = mtime;
        row.state = 0;
        row.fail_size = -1;
        row.fail_mtime = -1;
        row.bad = if rescan { bad } else { row.bad + bad };
        row.cur = cur;
        tx.execute(
            "UPDATE files SET state=0, size=?1, mtime=?2, pos=?3, p_mtime=?4, tail=?5, wait=?6, fail_size=-1, fail_mtime=-1, cur_model=?7, session=?8, c_in=?9, c_cache=?10, c_out=?11, bad=?12 WHERE id=?13",
            params![row.size, row.mtime, row.pos, row.p_mtime, row.tail, row.wait, row.cur.model, row.cur.session, row.cur.c_in as i64, row.cur.c_cache as i64, row.cur.c_out as i64, row.bad, row.id],
        )?;
        tx.commit()?;
        self.files[idx] = row;
        Ok(())
    }
}

// ───────────────────────────── 조회 ─────────────────────────────

#[derive(Default, Clone)]
struct Acc {
    inp: u64,
    out: u64,
    cr: u64,
    cw: u64,
    cost: f64,
    priced: u64,
    unpriced: u64,
}
impl Acc {
    fn add(&mut self, other: &Acc) {
        self.inp = self.inp.saturating_add(other.inp);
        self.out = self.out.saturating_add(other.out);
        self.cr = self.cr.saturating_add(other.cr);
        self.cw = self.cw.saturating_add(other.cw);
        self.cost += other.cost;
        self.priced += other.priced;
        self.unpriced = self.unpriced.saturating_add(other.unpriced);
    }
    fn totals(&self) -> UsageTotals {
        let total = self.inp.saturating_add(self.out).saturating_add(self.cr).saturating_add(self.cw);
        let cost = if self.priced > 0 && self.cost.is_finite() { Some((self.cost * 1e6).round() / 1e6) } else { None };
        UsageTotals {
            input_tokens: safe(self.inp),
            output_tokens: safe(self.out),
            cache_read_tokens: safe(self.cr),
            cache_write_tokens: safe(self.cw),
            total_tokens: safe(total),
            cost_usd: cost,
            unpriced_tokens: safe(self.unpriced),
        }
    }
}

fn build_report(conn: &Connection, since: Option<i64>, until: i64, now_ms: i64) -> rusqlite::Result<LocalUsageReport> {
    let lo = since.unwrap_or(i64::MIN);
    let mut tools: BTreeMap<i64, Acc> = BTreeMap::new();
    let mut models: Vec<(i64, String, Acc)> = Vec::new();
    let mut duplicates = 0u64;
    {
        let mut stmt = conn.prepare(
            "SELECT r.tool, m.name, SUM(r.inp), SUM(r.outp), SUM(r.cr), SUM(r.cw), SUM(r.cost), COUNT(r.cost),
                    SUM(CASE WHEN r.cost IS NULL THEN r.inp + r.outp + r.cr + r.cw ELSE 0 END), SUM(r.dups)
             FROM records r JOIN models m ON m.id = r.model_id
             WHERE r.ts >= ?1 AND r.ts < ?2 AND r.flags = 0
             GROUP BY r.tool, r.model_id",
        )?;
        let rows = stmt.query_map(params![lo, until], |r| {
            let big = |i: usize| -> rusqlite::Result<u64> { Ok(r.get::<_, Option<i64>>(i)?.unwrap_or(0).max(0) as u64) };
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                Acc {
                    inp: big(2)?,
                    out: big(3)?,
                    cr: big(4)?,
                    cw: big(5)?,
                    cost: r.get::<_, Option<f64>>(6)?.unwrap_or(0.0),
                    priced: big(7)?,
                    unpriced: big(8)?,
                },
                big(9)?,
            ))
        })?;
        for row in rows {
            let (tool, name, acc, dups) = row?;
            duplicates = duplicates.saturating_add(dups);
            tools.entry(tool).or_default().add(&acc);
            models.push((tool, name, acc));
        }
    }
    models.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut sessions: HashMap<i64, u64> = HashMap::new();
    {
        let mut stmt = conn.prepare("SELECT tool, COUNT(DISTINCT session) FROM records WHERE ts >= ?1 AND ts < ?2 AND flags = 0 GROUP BY tool")?;
        let rows = stmt.query_map(params![lo, until], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?.max(0) as u64)))?;
        for row in rows {
            let (tool, count) = row?;
            sessions.insert(tool, count);
        }
    }

    let (excluded, skewed): (i64, bool) = {
        let excluded = conn.query_row(
            "SELECT COUNT(*) FROM records WHERE flags <> 0 AND ((ts >= ?1 AND ts < ?2) OR ts <= 0 OR ts > ?3)",
            params![lo, until, now_ms],
            |r| r.get(0),
        )?;
        let skewed = conn
            .query_row("SELECT 1 FROM records WHERE flags <> 0 AND (flags & 3) <> 0 LIMIT 1", [], |r| r.get::<_, i64>(0))
            .is_ok();
        (excluded, skewed)
    };
    let (pending_files, scanned, unreadable, partial_files, stale_tails, bytes_read, bytes_total): (i64, i64, i64, i64, i64, i64, i64) = conn.query_row(
        &format!(
            "SELECT COALESCE(SUM(CASE WHEN {p} THEN 1 ELSE 0 END),0),
                    COALESCE(SUM(CASE WHEN state = 0 AND NOT {p} THEN 1 ELSE 0 END),0),
                    COALESCE(SUM(CASE WHEN state = 1 THEN 1 ELSE 0 END),0),
                    COALESCE(SUM(CASE WHEN bad > 0 THEN 1 ELSE 0 END),0),
                    COALESCE(SUM(CASE WHEN state = 0 AND wait >= 0 AND wait = size AND pos < size AND mtime < ?1 THEN 1 ELSE 0 END),0),
                    COALESCE(SUM(CASE WHEN state = 1 THEN size ELSE MIN(pos, size) END),0),
                    COALESCE(SUM(size),0)
             FROM files",
            p = PENDING_SQL
        ),
        params![now_ms - STALE_TAIL_MS],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
    )?;
    let coverage = |order: &str| -> Option<i64> {
        conn.query_row(&format!("SELECT ts FROM records WHERE ts > 0 AND flags = 0 ORDER BY ts {order} LIMIT 1"), [], |r| r.get(0)).ok()
    };

    let mut all = Acc::default();
    for acc in tools.values() {
        all.add(acc);
    }
    let tools_out: Vec<ToolUsage> = tools
        .iter()
        .map(|(tool, acc)| ToolUsage { tool: tool_name(*tool).into(), sessions: sessions.get(tool).copied().unwrap_or(0), totals: acc.totals() })
        .collect();
    let models_out: Vec<ModelUsage> = models.iter().map(|(tool, name, acc)| ModelUsage { tool: tool_name(*tool).into(), model: name.clone(), totals: acc.totals() }).collect();

    let totals = all.totals();
    let truncated = pending_files > 0 || get_meta(conn, "enum_truncated") != 0;
    let mut warnings: Vec<String> = Vec::new();
    let mut warn = |code: &str| warnings.push(code.to_owned());
    if get_meta(conn, "roots_found") == 0 {
        warn("NO_SOURCES");
    }
    if truncated {
        warn("INDEX_TRUNCATED");
    }
    if unreadable > 0 || get_meta(conn, "roots_bad") > 0 {
        warn("SOURCE_UNREADABLE");
    }
    if partial_files > 0 || stale_tails > 0 {
        warn("SOURCE_PARTIAL");
    }
    if skewed {
        warn("CLOCK_SKEW");
    }
    if totals.unpriced_tokens > 0 {
        warn("PRICING_UNKNOWN");
    }
    if tools_out.len() >= 2 {
        warn("CROSS_TOOL_OVERLAP");
    }
    if totals.total_tokens > 0 {
        warn("ATTRIBUTION_UNAVAILABLE");
    }
    let indexed = get_meta(conn, "last_pass_ms");
    Ok(LocalUsageReport {
        indexed_at: (indexed > 0).then_some(indexed),
        scanning: pending_files > 0,
        truncated,
        coverage_start: coverage("ASC"),
        coverage_end: coverage("DESC"),
        files_scanned: scanned.max(0) as u64,
        files_pending: pending_files.max(0) as u64,
        bytes_read: bytes_read.max(0) as u64,
        bytes_total: bytes_total.max(0) as u64,
        totals,
        tools: tools_out,
        models: models_out,
        warnings,
        excluded_records: excluded.max(0) as u64,
        duplicate_records: duplicates,
    })
}

// ───────────────────────────── 기록 폴더 찾기 ─────────────────────────────

/// 환경변수와 홈 경로로 읽을 후보 폴더를 만든다(존재 여부는 보지 않는다).
/// 기본 홈, 환경변수가 가리키는 홈, 등록된 Ojak 네이티브 프로필 홈을 모두 포함한다.
fn candidate_roots(
    home: Option<&Path>,
    env: &dyn Fn(&str) -> Option<OsString>,
    profiles: &[(String, PathBuf)],
) -> Vec<(u8, PathBuf)> {
    let expand = |value: OsString| -> Option<PathBuf> {
        if value.is_empty() {
            return None;
        }
        let path = PathBuf::from(value);
        if let Ok(rest) = path.strip_prefix("~") {
            return home.map(|h| h.join(rest));
        }
        path.is_absolute().then_some(path)
    };
    let mut out: Vec<(u8, PathBuf)> = Vec::new();
    if let Some(agent) = env("PI_CODING_AGENT_DIR").and_then(expand) {
        out.push((TOOL_OMP, agent.join("sessions")));
    }
    if let Some(dir) = env("PI_CODING_AGENT_SESSION_DIR").and_then(expand) {
        out.push((TOOL_OMP, dir));
    }
    if let Some(dir) = env("CLAUDE_CONFIG_DIR").and_then(expand) {
        out.push((TOOL_CLAUDE, dir.join("projects")));
    }
    if let Some(dir) = env("XDG_CONFIG_HOME").and_then(expand) {
        out.push((TOOL_CLAUDE, dir.join("claude").join("projects")));
    }
    if let Some(dir) = env("CODEX_HOME").and_then(expand) {
        out.push((TOOL_CODEX, dir.join("sessions")));
        out.push((TOOL_CODEX, dir.join("archived_sessions")));
    }
    if let Some(home) = home {
        out.push((TOOL_OMP, home.join(".omp").join("agent").join("sessions")));
        out.push((TOOL_OMP, home.join(".pi").join("agent").join("sessions")));
        out.push((TOOL_CLAUDE, home.join(".claude").join("projects")));
        out.push((TOOL_CLAUDE, home.join(".config").join("claude").join("projects")));
        out.push((TOOL_CODEX, home.join(".codex").join("sessions")));
        out.push((TOOL_CODEX, home.join(".codex").join("archived_sessions")));
    }
    for (tool, profile) in profiles {
        match tool.as_str() {
            "claude" => out.push((TOOL_CLAUDE, profile.join("projects"))),
            "codex" => {
                out.push((TOOL_CODEX, profile.join("sessions")));
                out.push((TOOL_CODEX, profile.join("archived_sessions")));
            }
            _ => {}
        }
    }
    out
}

/// 후보 중 실제 폴더만 남기고 같은 폴더(링크 포함)는 하나로 합친다.
fn resolve_roots(candidates: Vec<(u8, PathBuf)>) -> SourceSet {
    let mut set = SourceSet::default();
    let mut seen: Vec<(u8, PathBuf)> = Vec::new();
    for (tool, path) in candidates {
        match fs::metadata(&path) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(_) => {
                set.bad_roots += 1;
                continue;
            }
        }
        let Ok(canonical) = fs::canonicalize(&path) else {
            set.bad_roots += 1;
            continue;
        };
        if seen.iter().any(|(t, p)| *t == tool && *p == canonical) {
            continue;
        }
        seen.push((tool, canonical.clone()));
        set.sources.push(Source { tool, root: canonical });
    }
    set
}

/// 서비스가 등록한 계정의 프로필과 `profiles/` 폴더의 Claude·Codex 프로필 홈. 서비스에 닿지 않아도 폴더 목록으로 찾는다.
fn registered_profiles(paths: &Paths) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    let call_paths = paths.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(aam_protocol::call(&call_paths, "status.read", serde_json::json!({})));
    });
    // 서비스가 바쁘면 오래 기다리지 않는다. 읽기 전용 호출이며 서비스 DB는 건드리지 않는다.
    if let Ok(Ok(status)) = rx.recv_timeout(Duration::from_secs(3)) {
        for account in status.get("accounts").and_then(Value::as_array).into_iter().flatten() {
            let tool = account.get("tool").and_then(Value::as_str);
            let profile = account.get("profilePath").and_then(Value::as_str).map(PathBuf::from);
            if let (Some(tool), Some(profile)) = (tool, profile) {
                if matches!(tool, "claude" | "codex") && profile.is_absolute() {
                    out.push((tool.to_owned(), profile));
                }
            }
        }
    }
    if let Ok(entries) = fs::read_dir(&paths.profiles) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let tool = if name.starts_with("claude-") {
                "claude"
            } else if name.starts_with("codex-") {
                "codex"
            } else {
                continue;
            };
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                out.push((tool.to_owned(), entry.path()));
            }
        }
    }
    out
}

fn discover(paths: &Paths) -> SourceSet {
    let home = aam_protocol::user_home();
    let profiles = registered_profiles(paths);
    resolve_roots(candidate_roots(home.as_deref(), &|key| std::env::var_os(key), &profiles))
}

// ───────────────────────────── 명령 ─────────────────────────────

struct Shared {
    index: Option<Index>,
    set: Option<(Instant, SourceSet)>,
}
static SHARED: Mutex<Shared> = Mutex::new(Shared { index: None, set: None });

fn run_locked(mut guard: MutexGuard<'_, Shared>, db: &Path, paths: &Paths, since: Option<i64>, until: i64, now_ms: i64) -> LocalUsageReport {
    let shared = &mut *guard;
    if shared.index.is_none() {
        match Index::open(db) {
            Ok(index) => shared.index = Some(index),
            Err(_) => return empty_report(&["INDEX_UNAVAILABLE"]),
        }
    }
    let stale = shared
        .set
        .as_ref()
        .map_or(true, |(at, set)| at.elapsed() >= DISCOVER_EVERY || (set.sources.is_empty() && at.elapsed() >= Duration::from_secs(30)));
    if stale {
        shared.set = Some((Instant::now(), discover(paths)));
    }
    let set = shared.set.as_ref().map(|(_, set)| set.clone()).unwrap_or_default();
    let Some(index) = shared.index.as_mut() else { return empty_report(&["INDEX_UNAVAILABLE"]) };
    let outcome = index
        .refresh(&set, now_ms, Budget { bytes: PASS_BYTES, time: PASS_TIME }, ENUM_EVERY)
        .and_then(|_| build_report(&index.conn, since, until, now_ms));
    match outcome {
        Ok(report) => report,
        Err(_) => {
            // 저장소 오류. 다음 호출이 다시 열도록 버리고, 이번에는 읽을 수 없다고 알린다(스피너를 켜 두지 않는다).
            shared.index = None;
            empty_report(&["INDEX_UNAVAILABLE"])
        }
    }
}

/// 다른 호출이 색인을 읽는 중이면 기다리지 않고 지금까지의 색인만 조회한다.
fn run_readonly(db: &Path, since: Option<i64>, until: i64, now_ms: i64) -> LocalUsageReport {
    if !db.exists() {
        let mut report = empty_report(&[]);
        report.scanning = true;
        return report;
    }
    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)
        .and_then(|conn| conn.busy_timeout(Duration::from_millis(3000)).map(|_| conn));
    let mut report = conn.and_then(|conn| build_report(&conn, since, until, now_ms))
        .unwrap_or_else(|_| empty_report(&[]));
    // 호출자가 잠금을 얻지 못했다면 다른 작업이 발견·초기화·쓰기 중이다.
    // 마지막 커밋의 pending=0을 완료로 오인하면 기간 전환 후 갱신이 멈춘다.
    report.scanning = true;
    report
}

fn run_at(db: &Path, paths: &Paths, since: Option<i64>, until: i64) -> LocalUsageReport {
    let now_ms = aam_protocol::now_ms();
    match SHARED.try_lock() {
        Ok(guard) => run_locked(guard, db, paths, since, until, now_ms),
        Err(TryLockError::Poisoned(poisoned)) => {
            let mut guard = poisoned.into_inner();
            guard.index = None;
            SHARED.clear_poison();
            run_locked(guard, db, paths, since, until, now_ms)
        }
        Err(TryLockError::WouldBlock) => run_readonly(db, since, until, now_ms),
    }
}

/// 처음 읽기를 끝까지 이어 가는 백그라운드 작업. 화면을 닫아도 멈추지 않는다.
/// 운영체제에 낮은 우선순위로 알려(macOS background QoS, Windows background mode) 다른 작업이 있으면 양보한다.
/// 한 번에 하나만 돌며, 읽을 파일이 없으면 끝난다. 이후 늘어난 기록은 화면 조회가 조금씩 읽는다.
static WORKER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
fn ensure_worker(db: PathBuf, paths: Paths) {
    use std::sync::atomic::Ordering;
    if WORKER.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new().name("ojak-local-usage".into()).spawn(move || {
        lower_thread_priority();
        loop {
            // 기간 조회는 필요 없다. 빈 기간(0..0)으로 진행 상태만 받는다.
            let report = run_at(&db, &paths, Some(0), 0);
            if report.warnings.iter().any(|w| w == "INDEX_UNAVAILABLE") || report.files_pending == 0 {
                break;
            }
            // 한 번 읽은 뒤 잠깐 쉬어 화면 조회가 잠금을 얻을 틈을 준다. 속도 조절은 운영체제 우선순위에 맡긴다.
            std::thread::sleep(Duration::from_millis(50));
        }
        WORKER.store(false, Ordering::Release);
    });
    if spawned.is_err() {
        WORKER.store(false, Ordering::Release);
    }
}

#[cfg(target_os = "macos")]
fn lower_thread_priority() {
    // QOS_CLASS_BACKGROUND: 색인처럼 사용자가 결과를 기다리지 않는 일. CPU·디스크 I/O 모두 낮은 우선순위가 된다.
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(qos: u32, relpri: i32) -> i32;
    }
    const QOS_CLASS_BACKGROUND: u32 = 0x09;
    unsafe {
        pthread_set_qos_class_self_np(QOS_CLASS_BACKGROUND, 0);
    }
}
#[cfg(windows)]
fn lower_thread_priority() {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN};
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }
}
#[cfg(not(any(target_os = "macos", windows)))]
fn lower_thread_priority() {}

/// `sinceMs`는 포함, `untilMs`는 제외(Unix ms), `sinceMs`가 null이면 전체 기간.
/// 호출마다 읽는 양에 상한이 있고 남은 일은 `scanning`으로 알린다. 기간을 바꿔도 기록 파일을 다시 읽지 않는다.
#[tauri::command]
pub async fn local_usage(since_ms: Option<i64>, until_ms: i64) -> Result<LocalUsageReport, ApiError> {
    if until_ms < 0 {
        return Err(ApiError::new("INVALID_PARAMS", "조회 기간을 확인해 주세요."));
    }
    let paths = Paths::discover().map_err(|e| ApiError::new("PATH_ERROR", e.to_string()))?;
    tauri::async_runtime::spawn_blocking(move || {
        let db = paths.home.join("local-usage.sqlite3");
        let mut report = run_at(&db, &paths, since_ms, until_ms);
        // 처음 읽기가 남아 있으면 화면과 분리된 작업에 맡긴다. 창을 닫아도 계속된다.
        if report.files_pending > 0 {
            ensure_worker(db, paths);
            report.scanning = true;
        }
        report
    })
    .await
    .map_err(|_| ApiError::new("INTERNAL_ERROR", "로컬 사용량 조회 중 오류가 발생했습니다."))
}

// ───────────────────────────── 테스트 ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::Write,
        sync::atomic::{AtomicUsize, Ordering},
    };

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Temp {
            let dir = std::env::temp_dir().join(format!("ojak-local-usage-{}-{}", std::process::id(), COUNTER.fetch_add(1, Ordering::SeqCst)));
            fs::create_dir_all(&dir).unwrap();
            Temp(dir)
        }
        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn t(text: &str) -> i64 {
        parse_iso_ms(text).unwrap()
    }
    fn now() -> i64 {
        t("2030-01-01T00:00:00Z")
    }
    fn set_of(tool: u8, root: &Path) -> SourceSet {
        SourceSet { sources: vec![Source { tool, root: root.to_path_buf() }], bad_roots: 0 }
    }
    fn big() -> Budget {
        Budget { bytes: u64::MAX / 2, time: Duration::from_secs(60) }
    }
    fn open(dir: &Temp) -> Index {
        Index::open(&dir.join("index.sqlite3")).unwrap()
    }
    fn drain(index: &mut Index, set: &SourceSet) -> PassInfo {
        for _ in 0..300 {
            let last = index.refresh(set, now(), big(), Duration::ZERO).unwrap();
            if last.pending == 0 {
                return last;
            }
        }
        panic!("pending never reached zero");
    }
    fn report(index: &Index, since: Option<i64>, until: i64) -> LocalUsageReport {
        build_report(&index.conn, since, until, now()).unwrap()
    }
    fn write_lines(path: &Path, lines: &[String]) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut text = lines.join("\n");
        text.push('\n');
        fs::write(path, text).unwrap();
    }
    fn append(path: &Path, text: &str) {
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    #[test]
    fn hard_linked_codex_log_is_read_and_counted_once() {
        let dir = Temp::new();
        let root = dir.join("sessions");
        let mut lines = codex_head("sess-1", "gpt-5.5");
        lines.push(codex_total("2026-05-20T06:30:06.000Z", (1000, 400, 100, 30), (1000, 400, 100)));
        let original = root.join("a/rollout-1.jsonl");
        write_lines(&original, &lines);
        // Orca keeps a second hard link to the same Codex log under its own runtime home.
        let linked = root.join("orca/rollout-1.jsonl");
        fs::create_dir_all(linked.parent().unwrap()).unwrap();
        fs::hard_link(&original, &linked).unwrap();
        let mut index = open(&dir);
        drain(&mut index, &set_of(TOOL_CODEX, &root));
        let r = report(&index, None, now());
        assert!(!r.warnings.iter().any(|w| w == "SOURCE_UNREADABLE"), "{:?}", r.warnings);
        assert_eq!(r.totals.input_tokens, 600);
        assert_eq!(r.totals.output_tokens, 100);
    }

    #[test]
    fn byte_progress_reaches_total_only_after_every_file_is_read() {
        let dir = Temp::new();
        let root = dir.join("projects");
        write_lines(&root.join("p/a.jsonl"), &[claude_msg("m1", "2026-05-20T06:30:06.000Z", (10, 5, 0, 0), 0)]);
        write_lines(&root.join("p/b.jsonl"), &[claude_msg("m2", "2026-05-20T06:31:06.000Z", (10, 5, 0, 0), 0)]);
        let mut index = open(&dir);
        let set = set_of(TOOL_CLAUDE, &root);
        // A one-byte budget reads part of the history and must not report it as complete.
        index.refresh(&set, now(), Budget { bytes: 1, time: Duration::from_secs(60) }, Duration::ZERO).unwrap();
        let partial = report(&index, None, now());
        assert!(partial.bytes_total > 0 && partial.bytes_read < partial.bytes_total);
        drain(&mut index, &set);
        let done = report(&index, None, now());
        assert_eq!(done.bytes_read, done.bytes_total);
        assert_eq!(done.files_pending, 0);
    }

    #[cfg(windows)]
    #[test]
    fn background_reader_thread_runs_at_windows_background_priority() {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadPriority, THREAD_PRIORITY_NORMAL};
        let priority = std::thread::spawn(|| {
            lower_thread_priority();
            unsafe { GetThreadPriority(GetCurrentThread()) }
        })
        .join()
        .unwrap();
        // THREAD_MODE_BACKGROUND_BEGIN lowers CPU, I/O and memory priority; GetThreadPriority then reports -4
        // (below THREAD_PRIORITY_LOWEST). Without it the thread stays at THREAD_PRIORITY_NORMAL (0).
        assert_eq!(priority, -4);
        assert!(priority < THREAD_PRIORITY_NORMAL);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn background_reader_thread_runs_at_macos_background_qos() {
        unsafe extern "C" {
            fn qos_class_self() -> u32;
        }
        let qos = std::thread::spawn(|| {
            lower_thread_priority();
            unsafe { qos_class_self() }
        })
        .join()
        .unwrap();
        assert_eq!(qos, 0x09, "QOS_CLASS_BACKGROUND");
    }

    #[test]
    fn filter_change_during_initialization_or_refresh_keeps_polling() {
        let dir = Temp::new();
        let path = dir.join("index.sqlite3");
        // Another request owns the writer lock, with no schema committed yet.
        let db = Connection::open(&path).unwrap();
        let pending = run_readonly(&path, Some(0), now(), now());
        assert!(pending.scanning);
        assert!(!pending.warnings.iter().any(|w| w == "INDEX_UNAVAILABLE"));
        init_db(&db).unwrap();
        // A last committed snapshot with zero pending files is not proof the writer finished.
        let pending = run_readonly(&path, None, now(), now());
        assert!(pending.scanning);
        assert_eq!(pending.files_pending, 0);
    }

    fn omp_msg(id: &str, ts: &str, response: Option<&str>, usage: (u64, u64, u64, u64), cost: f64) -> String {
        let mut message = json!({"role":"assistant","provider":"anthropic","model":"claude-opus-5",
            "usage":{"input":usage.0,"output":usage.1,"cacheRead":usage.2,"cacheWrite":usage.3,
                     "totalTokens":usage.0+usage.1+usage.2+usage.3,"reasoningTokens":usage.1/2,"cost":{"total":cost}}});
        if let Some(r) = response {
            message["responseId"] = json!(r);
        }
        json!({"type":"message","id":id,"timestamp":ts,"message":message}).to_string()
    }
    fn claude_msg(id: &str, ts: &str, usage: (u64, u64, u64, u64), one_hour: u64) -> String {
        json!({"type":"assistant","timestamp":ts,"sessionId":"sess-1","requestId":"req","uuid":"u",
            "message":{"id":id,"model":"claude-opus-5","usage":{"input_tokens":usage.0,"output_tokens":usage.1,
              "cache_read_input_tokens":usage.2,"cache_creation_input_tokens":usage.3,
              "cache_creation":{"ephemeral_5m_input_tokens":usage.3-one_hour,"ephemeral_1h_input_tokens":one_hour}}}})
        .to_string()
    }
    /// `last`는 이 이벤트 직전 요청 한 건의 (입력, 캐시 입력, 출력)이다. 실제 Codex 기록처럼 세션 첫 이벤트는 누적값과 같다.
    fn codex_total(ts: &str, total: (u64, u64, u64, u64), last: (u64, u64, u64)) -> String {
        json!({"timestamp":ts,"type":"event_msg","payload":{"type":"token_count","info":{
            "total_token_usage":{"input_tokens":total.0,"cached_input_tokens":total.1,"output_tokens":total.2,"reasoning_output_tokens":total.3,"total_tokens":total.0+total.2},
            "last_token_usage":{"input_tokens":last.0,"cached_input_tokens":last.1,"output_tokens":last.2,"reasoning_output_tokens":0,"total_tokens":last.0+last.2}}}})
        .to_string()
    }
    fn codex_head(session: &str, model: &str) -> Vec<String> {
        vec![
            json!({"timestamp":"2026-05-20T06:29:55.418Z","type":"session_meta","payload":{"id":session,"forked_from_id":null}}).to_string(),
            json!({"timestamp":"2026-05-20T06:29:56.000Z","type":"turn_context","payload":{"model":model}}).to_string(),
        ]
    }

    #[test]
    fn iso_parsing_handles_zones_fractions_and_garbage() {
        assert_eq!(parse_iso_ms("1970-01-02T00:00:00Z"), Some(86_400_000));
        assert_eq!(parse_iso_ms("2000-03-01T00:00:00Z"), Some(951_868_800_000));
        assert_eq!(parse_iso_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_iso_ms("1970-01-01T00:00:01.123456Z"), Some(1123));
        assert_eq!(parse_iso_ms("2026-01-01T09:00:00+09:00"), parse_iso_ms("2026-01-01T00:00:00Z"));
        assert_eq!(parse_iso_ms("2026-01-01T00:00:00-0230"), parse_iso_ms("2026-01-01T02:30:00Z"));
        assert_eq!(parse_iso_ms("2026-01-01T00:00:00"), parse_iso_ms("2026-01-01T00:00:00Z"));
        assert_eq!(parse_iso_ms("2026-02-30T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("2024-02-29T00:00:00Z").is_some(), true);
        assert_eq!(parse_iso_ms("2025-02-29T00:00:00Z"), None);
        assert_eq!(parse_iso_ms("not a time at all, nope"), None);
        assert_eq!(parse_iso_ms("1969-12-31T23:59:59Z"), None);
    }

    #[test]
    fn model_names_never_leak_paths_or_emails() {
        assert_eq!(sanitize_model("claude-opus-5"), "claude-opus-5");
        assert_eq!(sanitize_model("anthropic/claude-opus-5"), "anthropic/claude-opus-5");
        let long = "a".repeat(200);
        for bad in ["me@example.com/model", "/Users/x/model", "C:\\Users\\x", "C:/Users/x", "a/b/c", "../x", "", "x y", long.as_str()] {
            assert_eq!(sanitize_model(bad), "unknown", "{bad}");
        }
        assert_eq!(model_label(Some("openai-codex"), Some("gpt-5.6-sol")), "openai-codex/gpt-5.6-sol");
        assert_eq!(model_label(Some("p@x.io"), Some("m")), "unknown");
    }

    #[test]
    fn surrogate_repair_only_touches_unpaired_escapes() {
        let lone = br#"{"a":"\ud83d x","b":"\ud83d\ude00","c":"\\ud83d"}"#;
        let fixed = repair_surrogates(lone).unwrap();
        let text = String::from_utf8(fixed).unwrap();
        assert!(text.contains(r#""a":"\ufffd x""#));
        assert!(text.contains(r#"\ud83d\ude00"#));
        assert!(text.contains(r#""c":"\\ud83d""#));
        assert!(serde_json::from_str::<Value>(&text).is_ok());
        assert!(repair_surrogates(br#"{"a":"\ud83d\ude00"}"#).is_none());
    }

    #[test]
    fn line_with_lone_surrogate_still_counts_its_usage() {
        let dir = Temp::new();
        let root = dir.join("projects");
        let mut line = claude_msg("msg_1", "2026-08-04T00:00:00Z", (10, 20, 30, 40), 0);
        line = line.replace("\"sessionId\"", "\"note\":\"\\ud83d\",\"sessionId\"");
        write_lines(&root.join("p/a.jsonl"), &[line]);
        let mut index = open(&dir);
        let set = set_of(TOOL_CLAUDE, &root);
        drain(&mut index, &set);
        let r = report(&index, None, now());
        assert_eq!(r.totals.input_tokens, 10);
        assert_eq!(r.totals.output_tokens, 20);
        assert!(!r.warnings.iter().any(|w| w == "SOURCE_PARTIAL"));
    }

    #[test]
    fn claude_input_excludes_cache_and_duplicate_message_ids_keep_largest_output() {
        let dir = Temp::new();
        let root = dir.join("projects");
        write_lines(
            &root.join("p/a.jsonl"),
            &[
                claude_msg("msg_a", "2026-08-04T00:00:01Z", (2, 100, 1000, 50), 0),
                // 스트리밍 중간 줄(출력이 더 작음)과 최종 줄이 같은 메시지 ID로 반복된다.
                claude_msg("msg_a", "2026-08-04T00:00:02Z", (2, 516, 1000, 50), 0),
                claude_msg("msg_a", "2026-08-04T00:00:03Z", (2, 516, 1000, 50), 0),
                claude_msg("msg_b", "2026-08-04T00:00:04Z", (0, 0, 0, 0), 0),
            ],
        );
        // 하위 에이전트 파일이 같은 메시지를 다시 싣는다.
        write_lines(&root.join("p/sess/subagents/b.jsonl"), &[claude_msg("msg_a", "2026-08-04T00:00:03Z", (2, 516, 1000, 50), 0)]);
        let mut index = open(&dir);
        drain(&mut index, &set_of(TOOL_CLAUDE, &root));
        let r = report(&index, None, now());
        assert_eq!(r.totals.input_tokens, 2);
        assert_eq!(r.totals.output_tokens, 516);
        assert_eq!(r.totals.cache_read_tokens, 1000);
        assert_eq!(r.totals.cache_write_tokens, 50);
        assert_eq!(r.totals.total_tokens, 2 + 516 + 1000 + 50);
        assert_eq!(r.duplicate_records, 3);
        assert_eq!(r.tools.len(), 1);
        assert_eq!(r.tools[0].sessions, 1);
    }

    #[test]
    fn claude_one_hour_cache_writes_cost_double_the_input_rate() {
        let dir = Temp::new();
        let root = dir.join("projects");
        write_lines(&root.join("p/a.jsonl"), &[claude_msg("msg_1", "2026-08-04T00:00:01Z", (1000, 500, 2000, 4000), 1000)]);
        let mut index = open(&dir);
        drain(&mut index, &set_of(TOOL_CLAUDE, &root));
        let r = report(&index, None, now());
        // claude-opus-5: 입력 5, 출력 25, 캐시 읽기 0.5, 캐시 쓰기 6.25 (100만 토큰당). 1시간 쓰기 1000토큰은 입력 단가의 2배.
        let expected = (1000.0 * 5.0 + 500.0 * 25.0 + 2000.0 * 0.5 + 3000.0 * 6.25 + 1000.0 * 10.0) / 1e6;
        assert!((r.totals.cost_usd.unwrap() - expected).abs() < 1e-6, "{:?}", r.totals.cost_usd);
        assert_eq!(r.totals.unpriced_tokens, 0);
    }

    #[test]
    fn omp_total_never_double_counts_reasoning_and_dedups_imported_copies() {
        let dir = Temp::new();
        let root = dir.join("sessions");
        let a = omp_msg("a1", "2026-08-04T00:00:01.000Z", Some("resp_1"), (100, 40, 500, 7), 0.25);
        write_lines(&root.join("proj/one.jsonl"), &[a.clone(), omp_msg("a2", "2026-08-04T00:00:02.000Z", Some("resp_2"), (10, 5, 0, 0), 0.01)]);
        // 가져온 세션이 같은 응답 ID를 다시 담고 있다.
        write_lines(&root.join("proj/two.jsonl"), &[a]);
        let mut index = open(&dir);
        drain(&mut index, &set_of(TOOL_OMP, &root));
        let r = report(&index, None, now());
        assert_eq!(r.totals.input_tokens, 110);
        assert_eq!(r.totals.output_tokens, 45);
        assert_eq!(r.totals.total_tokens, 110 + 45 + 500 + 7);
        assert!((r.totals.cost_usd.unwrap() - 0.26).abs() < 1e-9);
        assert_eq!(r.duplicate_records, 1);
        assert_eq!(r.models[0].model, "anthropic/claude-opus-5");
    }

    #[test]
    fn unknown_model_is_unpriced_not_free_and_mixed_cost_is_partial() {
        let dir = Temp::new();
        let root = dir.join("sessions");
        let mut unknown = json!({"role":"assistant","provider":"mystery","model":"model-x","responseId":"resp_u",
            "usage":{"input":1000,"output":10,"cacheRead":0,"cacheWrite":0,"cost":{"total":0}}});
        unknown["usage"]["totalTokens"] = json!(1010);
        let unknown_line = json!({"type":"message","id":"u1","timestamp":"2026-08-04T00:00:03Z","message":unknown}).to_string();
        write_lines(&root.join("p/a.jsonl"), &[unknown_line.clone(), omp_msg("a1", "2026-08-04T00:00:01Z", Some("r1"), (100, 40, 0, 0), 0.5)]);
        let mut index = open(&dir);
        drain(&mut index, &set_of(TOOL_OMP, &root));
        let r = report(&index, None, now());
        assert_eq!(r.totals.unpriced_tokens, 1010);
        assert!((r.totals.cost_usd.unwrap() - 0.5).abs() < 1e-9);
        assert!(r.warnings.iter().any(|w| w == "PRICING_UNKNOWN"));
        let only_unknown = r.models.iter().find(|m| m.model == "mystery/model-x").unwrap();
        assert_eq!(only_unknown.totals.cost_usd, None);
        assert_eq!(only_unknown.totals.unpriced_tokens, 1010);
    }

    #[test]
    fn omp_zero_recorded_cost_falls_back_to_price_table_when_model_is_known() {
        let dir = Temp::new();
        let root = dir.join("sessions");
        write_lines(&root.join("p/a.jsonl"), &[omp_msg("a1", "2026-08-04T00:00:01Z", Some("r1"), (1_000_000, 0, 0, 0), 0.0)]);
        let mut index = open(&dir);
        drain(&mut index, &set_of(TOOL_OMP, &root));
        let r = report(&index, None, now());
        assert!((r.totals.cost_usd.unwrap() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn codex_subtracts_cached_input_uses_deltas_and_ignores_repeats() {
        let dir = Temp::new();
        let root = dir.join("sessions");
        let mut lines = codex_head("sess-1", "gpt-5.5");
        lines.push(codex_total("2026-05-20T06:30:06.000Z", (1000, 400, 100, 30), (1000, 400, 100)));
        // 같은 누적값이 다시 보고된다.
        lines.push(codex_total("2026-05-20T06:30:07.000Z", (1000, 400, 100, 30), (1000, 400, 100)));
        lines.push(codex_total("2026-05-20T06:30:15.000Z", (1500, 400, 160, 40), (500, 0, 60)));
        // 카운터가 줄어드는 비정상 보고는 새로 센 것이 없다.
        lines.push(codex_total("2026-05-20T06:30:20.000Z", (1200, 300, 120, 20), (200, 0, 0)));
        write_lines(&root.join("2026/05/20/rollout-a.jsonl"), &lines);
        let mut index = open(&dir);
        drain(&mut index, &set_of(TOOL_CODEX, &root));
        let r = report(&index, None, now());
        assert_eq!(r.totals.input_tokens, 600 + 500);
        assert_eq!(r.totals.cache_read_tokens, 400);
        assert_eq!(r.totals.output_tokens, 160);
        assert_eq!(r.totals.total_tokens, 1100 + 400 + 160);
        let expected = (1100.0 * 5.0 + 160.0 * 30.0 + 400.0 * 0.5) / 1e6;
        assert!((r.totals.cost_usd.unwrap() - expected).abs() < 1e-9);
    }

    #[test]
    fn codex_forked_session_replaying_history_is_not_counted_twice() {
        let dir = Temp::new();
        let root = dir.join("sessions");
        let mut parent = codex_head("sess-1", "gpt-5.5");
        parent.push(codex_total("2026-05-20T06:30:06.000Z", (1000, 400, 100, 30), (1000, 400, 100)));
        parent.push(codex_total("2026-05-20T06:30:15.000Z", (1500, 400, 160, 40), (500, 0, 60)));
        write_lines(&root.join("a/rollout-1.jsonl"), &parent);
        // Codex는 포크할 때 부모 이벤트를 포크 시각으로 다시 쓴다. 같은 이벤트라도 시각이 다르다.
        let mut fork = codex_head("sess-2-fork", "gpt-5.5");
        fork.push(codex_total("2026-05-21T09:00:00.000Z", (1000, 400, 100, 30), (1000, 400, 100)));
        fork.push(codex_total("2026-05-21T09:00:00.001Z", (1500, 400, 160, 40), (500, 0, 60)));
        fork.push(codex_total("2026-05-21T09:01:00.000Z", (1800, 400, 200, 50), (300, 0, 40)));
        write_lines(&root.join("b/rollout-2.jsonl"), &fork);
        // 부모 기록을 다시 싣지 않고 첫 이벤트부터 부모 누적값을 지닌 포크: 이 파일의 요청분만 센다.
        let mut referenced = codex_head("sess-3-fork", "gpt-5.5");
        referenced.push(codex_total("2026-05-21T10:00:00.000Z", (1700, 400, 190, 45), (200, 0, 30)));
        write_lines(&root.join("c/rollout-3.jsonl"), &referenced);
        let mut index = open(&dir);
        drain(&mut index, &set_of(TOOL_CODEX, &root));
        let r = report(&index, None, now());
        assert_eq!(r.totals.input_tokens, 1100 + 300 + 200);
        assert_eq!(r.totals.cache_read_tokens, 400);
        assert_eq!(r.totals.output_tokens, 160 + 40 + 30);
        assert_eq!(r.duplicate_records, 2);
    }

    #[test]
    fn codex_unknown_model_counts_tokens_but_leaves_them_unpriced() {
        let dir = Temp::new();
        let root = dir.join("sessions");
        let mut lines = codex_head("s", "gpt-5.3-codex-spark");
        lines.push(codex_total("2026-05-20T06:30:06.000Z", (1000, 0, 100, 0), (1000, 0, 100)));
        write_lines(&root.join("a/rollout-1.jsonl"), &lines);
        let mut index = open(&dir);
        drain(&mut index, &set_of(TOOL_CODEX, &root));
        let r = report(&index, None, now());
        assert_eq!(r.totals.total_tokens, 1100);
        assert_eq!(r.totals.cost_usd, None);
        assert_eq!(r.totals.unpriced_tokens, 1100);
    }

    #[test]
    fn partial_trailing_line_is_counted_once_after_it_completes() {
        let dir = Temp::new();
        let root = dir.join("projects");
        let file = root.join("p/a.jsonl");
        let first = claude_msg("msg_1", "2026-08-04T00:00:01Z", (1, 10, 0, 0), 0);
        let second = claude_msg("msg_2", "2026-08-04T00:00:02Z", (2, 20, 0, 0), 0);
        let split = second.len() / 2;
        write_lines(&file, &[first]);
        append(&file, &second[..split]);
        let mut index = open(&dir);
        let set = set_of(TOOL_CLAUDE, &root);
        let info = drain(&mut index, &set);
        assert_eq!(info.pending, 0, "a partial tail must not keep the scan spinning");
        assert_eq!(report(&index, None, now()).totals.output_tokens, 10);
        append(&file, &second[split..]);
        append(&file, "\n");
        drain(&mut index, &set);
        let r = report(&index, None, now());
        assert_eq!(r.totals.output_tokens, 30);
        assert_eq!(r.totals.input_tokens, 3);
        assert_eq!(r.duplicate_records, 0);
    }

    #[test]
    fn complete_json_tail_without_newline_is_accepted_without_double_counting() {
        let dir = Temp::new();
        let root = dir.join("projects");
        let file = root.join("p/a.jsonl");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, claude_msg("msg_1", "2026-08-04T00:00:01Z", (1, 10, 0, 0), 0)).unwrap();
        let mut index = open(&dir);
        let set = set_of(TOOL_CLAUDE, &root);
        drain(&mut index, &set);
        assert_eq!(report(&index, None, now()).totals.output_tokens, 10);
        append(&file, "\n");
        append(&file, &(claude_msg("msg_2", "2026-08-04T00:00:02Z", (1, 5, 0, 0), 0) + "\n"));
        drain(&mut index, &set);
        let r = report(&index, None, now());
        assert_eq!(r.totals.output_tokens, 15);
        assert_eq!(r.duplicate_records, 0);
    }

    #[test]
    fn growing_log_reads_only_new_bytes_and_unchanged_logs_read_nothing() {
        let dir = Temp::new();
        let root = dir.join("sessions");
        let file = root.join("p/a.jsonl");
        write_lines(&file, &[omp_msg("a1", "2026-08-04T00:00:01Z", Some("r1"), (1, 1, 0, 0), 0.1)]);
        let mut index = open(&dir);
        let set = set_of(TOOL_OMP, &root);
        drain(&mut index, &set);
        let idle = index.refresh(&set, now(), big(), Duration::ZERO).unwrap();
        assert_eq!(idle.bytes, 0, "an unchanged index must not reread logs");
        append(&file, &(omp_msg("a2", "2026-08-04T00:00:02Z", Some("r2"), (2, 2, 0, 0), 0.2) + "\n"));
        let grown = index.refresh(&set, now(), big(), Duration::ZERO).unwrap();
        assert!(grown.bytes > 0 && grown.bytes < 2000);
        let r = report(&index, None, now());
        assert_eq!(r.totals.input_tokens, 3);
        assert_eq!(r.duplicate_records, 0);
        // 새 Index(앱 재시작)도 저장된 위치에서 이어 간다.
        drop(index);
        let mut again = open(&dir);
        let restart = again.refresh(&set, now(), big(), Duration::ZERO).unwrap();
        assert_eq!(restart.bytes, 0);
        assert_eq!(report(&again, None, now()).totals.input_tokens, 3);
    }

    #[test]
    fn in_place_title_rewrite_does_not_trigger_a_rescan() {
        let dir = Temp::new();
        let root = dir.join("sessions");
        let file = root.join("p/a.jsonl");
        let title = |text: &str| json!({"type":"title","title":text,"pad":" ".repeat(60 - text.len())}).to_string();
        // 읽은 위치 앞 4KiB만 비교하므로 첫 줄이 그 창 밖에 있을 만큼 파일이 커야 한다(실제 omp 세션은 항상 그렇다).
        let mut lines = vec![title("a")];
        lines.extend((0..40).map(|i| omp_msg(&format!("f{i}"), "2026-08-04T00:00:00Z", Some(&format!("rf{i}")), (0, 0, 0, 0), 0.0)));
        lines.push(omp_msg("a1", "2026-08-04T00:00:01Z", Some("r1"), (5, 5, 0, 0), 0.1));
        write_lines(&file, &lines);
        assert!(fs::metadata(&file).unwrap().len() > 2 * TAIL_WINDOW);
        let mut index = open(&dir);
        let set = set_of(TOOL_OMP, &root);
        drain(&mut index, &set);
        // 첫 줄을 같은 길이로 덮어쓴다(수정 시각만 바뀐다).
        let mut content = fs::read(&file).unwrap();
        let replacement = title("zz");
        content[..replacement.len()].copy_from_slice(replacement.as_bytes());
        fs::write(&file, content).unwrap();
        let info = index.refresh(&set, now(), big(), Duration::ZERO).unwrap();
        assert_eq!(info.bytes, 0);
        assert_eq!(report(&index, None, now()).totals.input_tokens, 5);
        assert_eq!(report(&index, None, now()).duplicate_records, 0);
    }

    #[test]
    fn truncated_or_rewritten_log_is_reread_without_double_counting() {
        let dir = Temp::new();
        let root = dir.join("projects");
        let file = root.join("p/a.jsonl");
        write_lines(&file, &[claude_msg("m1", "2026-08-04T00:00:01Z", (1, 10, 0, 0), 0), claude_msg("m2", "2026-08-04T00:00:02Z", (1, 10, 0, 0), 0)]);
        let mut index = open(&dir);
        let set = set_of(TOOL_CLAUDE, &root);
        drain(&mut index, &set);
        assert_eq!(report(&index, None, now()).totals.output_tokens, 20);
        // 같은 크기로 내용이 바뀐다(이어 쓴 것이 아니다). 새 메시지가 반영돼야 한다.
        std::thread::sleep(Duration::from_millis(30));
        write_lines(&file, &[claude_msg("m3", "2026-08-04T00:00:03Z", (1, 10, 0, 0), 0), claude_msg("m4", "2026-08-04T00:00:04Z", (1, 10, 0, 0), 0)]);
        drain(&mut index, &set);
        assert_eq!(report(&index, None, now()).totals.output_tokens, 40);
        // 줄어든 파일(앞쪽 한 줄만 남김)을 다시 읽어도 이미 센 메시지는 더해지지 않는다.
        std::thread::sleep(Duration::from_millis(30));
        write_lines(&file, &[claude_msg("m3", "2026-08-04T00:00:03Z", (1, 10, 0, 0), 0)]);
        drain(&mut index, &set);
        let r = report(&index, None, now());
        assert_eq!(r.totals.output_tokens, 40);
        assert_eq!(r.duplicate_records, 0, "rescanning the same file must not inflate duplicates");
    }

    #[test]
    fn date_range_is_since_inclusive_until_exclusive_and_never_rereads() {
        let dir = Temp::new();
        let root = dir.join("sessions");
        let day = t("2026-08-04T00:00:00Z");
        write_lines(
            &root.join("p/a.jsonl"),
            &[
                omp_msg("a0", "2026-08-03T23:59:59.999Z", Some("r0"), (1, 0, 0, 0), 0.1),
                omp_msg("a1", "2026-08-04T00:00:00.000Z", Some("r1"), (10, 0, 0, 0), 0.1),
                omp_msg("a2", "2026-08-04T23:59:59.999Z", Some("r2"), (100, 0, 0, 0), 0.1),
                omp_msg("a3", "2026-08-05T00:00:00.000Z", Some("r3"), (1000, 0, 0, 0), 0.1),
            ],
        );
        let mut index = open(&dir);
        let set = set_of(TOOL_OMP, &root);
        drain(&mut index, &set);
        assert_eq!(report(&index, Some(day), day + 86_400_000).totals.input_tokens, 110);
        assert_eq!(report(&index, Some(day), day).totals.input_tokens, 0);
        assert_eq!(report(&index, Some(day + 1), day + 86_400_000).totals.input_tokens, 100);
        assert_eq!(report(&index, None, day).totals.input_tokens, 1);
        assert_eq!(report(&index, None, now()).totals.input_tokens, 1111);
        assert_eq!(report(&index, Some(day), day + 86_400_000 * 2).totals.input_tokens, 1110);
        let again = index.refresh(&set, now(), big(), Duration::ZERO).unwrap();
        assert_eq!(again.bytes, 0);
        let all = report(&index, None, now());
        assert_eq!(all.coverage_start, Some(t("2026-08-03T23:59:59.999Z")));
        assert_eq!(all.coverage_end, Some(t("2026-08-05T00:00:00Z")));
    }

    #[test]
    fn invalid_future_and_implausible_records_are_excluded_but_counted() {
        let dir = Temp::new();
        let root = dir.join("sessions");
        write_lines(
            &root.join("p/a.jsonl"),
            &[
                omp_msg("ok", "2026-08-04T00:00:00Z", Some("r_ok"), (10, 0, 0, 0), 0.1),
                omp_msg("future", "2099-01-01T00:00:00Z", Some("r_future"), (20, 0, 0, 0), 0.1),
                omp_msg("badtime", "garbage", Some("r_bad"), (30, 0, 0, 0), 0.1),
                omp_msg("huge", "2026-08-04T00:00:00Z", Some("r_huge"), (9_000_000_000, 0, 0, 0), 0.1),
            ],
        );
        let mut index = open(&dir);
        drain(&mut index, &set_of(TOOL_OMP, &root));
        let r = report(&index, None, now());
        assert_eq!(r.totals.input_tokens, 10);
        assert_eq!(r.excluded_records, 3);
        assert!(r.warnings.iter().any(|w| w == "CLOCK_SKEW"));
    }

    #[test]
    fn byte_budget_makes_progress_reports_truncation_then_finishes() {
        let dir = Temp::new();
        let root = dir.join("sessions");
        for n in 0..6 {
            let lines: Vec<String> = (0..20).map(|i| omp_msg(&format!("m{n}-{i}"), "2026-08-04T00:00:01Z", Some(&format!("r{n}-{i}")), (1, 1, 0, 0), 0.01)).collect();
            write_lines(&root.join(format!("p/{n}.jsonl")), &lines);
        }
        let mut index = open(&dir);
        let set = set_of(TOOL_OMP, &root);
        let small = Budget { bytes: 3000, time: Duration::from_secs(60) };
        let first = index.refresh(&set, now(), small, Duration::ZERO).unwrap();
        assert!(first.pending > 0);
        let partial = report(&index, None, now());
        assert!(partial.scanning && partial.truncated);
        assert!(partial.warnings.iter().any(|w| w == "INDEX_TRUNCATED"));
        assert!(partial.totals.input_tokens < 120);
        for _ in 0..500 {
            if index.refresh(&set, now(), small, Duration::ZERO).unwrap().pending == 0 {
                break;
            }
        }
        let done = report(&index, None, now());
        assert!(!done.scanning && !done.truncated);
        assert_eq!(done.totals.input_tokens, 120);
        assert_eq!(done.duplicate_records, 0);
        assert_eq!(done.files_pending, 0);
        assert_eq!(done.files_scanned, 6);
    }

    #[test]
    fn oversized_line_is_skipped_and_flagged_without_stalling() {
        let dir = Temp::new();
        let file = dir.join("p/a.jsonl");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        let mut reader = io::Cursor::new(b"aaaaaaaaaaaa\nbb\n".to_vec());
        let mut buf = Vec::new();
        let (consumed, newline, over) = read_line(&mut reader, &mut buf, 4).unwrap();
        assert_eq!((consumed, newline, over), (13, true, true));
        let (consumed, newline, over) = read_line(&mut reader, &mut buf, 4).unwrap();
        assert_eq!((consumed, newline, over, buf.as_slice()), (3, true, false, &b"bb"[..]));
        let (consumed, _, _) = read_line(&mut reader, &mut buf, 4).unwrap();
        assert_eq!(consumed, 0);
    }

    #[test]
    fn corrupt_lines_flag_partial_but_do_not_block_other_lines() {
        let dir = Temp::new();
        let root = dir.join("projects");
        write_lines(
            &root.join("p/a.jsonl"),
            &[
                "{\"type\":\"assistant\",\"message\":{\"usage\": BROKEN".to_owned(),
                claude_msg("m1", "2026-08-04T00:00:01Z", (1, 10, 0, 0), 0),
            ],
        );
        let mut index = open(&dir);
        let info = drain(&mut index, &set_of(TOOL_CLAUDE, &root));
        assert_eq!(info.pending, 0);
        let r = report(&index, None, now());
        assert_eq!(r.totals.output_tokens, 10);
        assert!(r.warnings.iter().any(|w| w == "SOURCE_PARTIAL"));
        assert!(!r.scanning);
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_file_pauses_instead_of_spinning_forever_and_retries_when_it_changes() {
        use std::os::unix::fs::PermissionsExt;
        let dir = Temp::new();
        let root = dir.join("projects");
        let locked = root.join("p/locked.jsonl");
        write_lines(&locked, &[claude_msg("m1", "2026-08-04T00:00:01Z", (1, 10, 0, 0), 0)]);
        write_lines(&root.join("p/ok.jsonl"), &[claude_msg("m2", "2026-08-04T00:00:02Z", (1, 5, 0, 0), 0)]);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        // root 권한이면 읽혀 버려 시험이 의미가 없다.
        if fs::File::open(&locked).is_ok() {
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
            return;
        }
        let mut index = open(&dir);
        let set = set_of(TOOL_CLAUDE, &root);
        let info = drain(&mut index, &set);
        assert_eq!(info.pending, 0);
        let r = report(&index, None, now());
        assert!(!r.scanning);
        assert!(r.warnings.iter().any(|w| w == "SOURCE_UNREADABLE"));
        assert_eq!(r.totals.output_tokens, 5);
        let idle = index.refresh(&set, now(), big(), Duration::ZERO).unwrap();
        assert_eq!((idle.files, idle.pending), (0, 0), "an unchanged unreadable file is not retried");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o644)).unwrap();
        append(&locked, &(claude_msg("m3", "2026-08-04T00:00:03Z", (1, 1, 0, 0), 0) + "\n"));
        drain(&mut index, &set);
        let r = report(&index, None, now());
        assert_eq!(r.totals.output_tokens, 5 + 10 + 1);
        assert!(!r.warnings.iter().any(|w| w == "SOURCE_UNREADABLE"));
    }

    #[test]
    fn missing_roots_report_no_sources_without_scanning() {
        let dir = Temp::new();
        let mut index = open(&dir);
        let set = resolve_roots(vec![(TOOL_CLAUDE, dir.join("does-not-exist"))]);
        assert!(set.sources.is_empty());
        let info = index.refresh(&set, now(), big(), Duration::ZERO).unwrap();
        assert_eq!(info.pending, 0);
        let r = report(&index, None, now());
        assert!(!r.scanning);
        assert!(r.warnings.iter().any(|w| w == "NO_SOURCES"));
        assert!(r.indexed_at.is_some());
    }

    #[test]
    fn cross_tool_overlap_and_attribution_warnings_follow_the_data() {
        let dir = Temp::new();
        let omp_root = dir.join("sessions");
        let claude_root = dir.join("projects");
        write_lines(&omp_root.join("p/a.jsonl"), &[omp_msg("a1", "2026-08-04T00:00:01Z", Some("r1"), (1, 1, 0, 0), 0.1)]);
        write_lines(&claude_root.join("p/a.jsonl"), &[claude_msg("m1", "2026-08-04T00:00:01Z", (1, 1, 0, 0), 0)]);
        let mut index = open(&dir);
        let set = SourceSet {
            sources: vec![Source { tool: TOOL_OMP, root: omp_root }, Source { tool: TOOL_CLAUDE, root: claude_root }],
            bad_roots: 0,
        };
        drain(&mut index, &set);
        let both = report(&index, None, now());
        assert_eq!(both.tools.len(), 2);
        assert!(both.warnings.iter().any(|w| w == "CROSS_TOOL_OVERLAP"));
        assert!(both.warnings.iter().any(|w| w == "ATTRIBUTION_UNAVAILABLE"));
        let none = report(&index, Some(now() - 1000), now());
        assert!(none.tools.is_empty());
        assert!(!none.warnings.iter().any(|w| w == "CROSS_TOOL_OVERLAP" || w == "ATTRIBUTION_UNAVAILABLE"));
    }

    #[test]
    fn same_response_id_across_omp_and_claude_is_counted_once() {
        let dir = Temp::new();
        let omp_root = dir.join("sessions");
        let claude_root = dir.join("projects");
        write_lines(&omp_root.join("p/a.jsonl"), &[omp_msg("a1", "2026-08-04T00:00:01Z", Some("msg_same"), (7, 9, 0, 0), 0.1)]);
        write_lines(&claude_root.join("p/a.jsonl"), &[claude_msg("msg_same", "2026-08-04T00:00:01Z", (7, 9, 0, 0), 0)]);
        let mut index = open(&dir);
        let set = SourceSet {
            sources: vec![Source { tool: TOOL_OMP, root: omp_root }, Source { tool: TOOL_CLAUDE, root: claude_root }],
            bad_roots: 0,
        };
        drain(&mut index, &set);
        let r = report(&index, None, now());
        assert_eq!(r.totals.input_tokens, 7);
        assert_eq!(r.duplicate_records, 1);
    }

    #[test]
    fn omp_compaction_and_cache_warm_calls_are_counted_with_their_own_keys() {
        let dir = Temp::new();
        let root = dir.join("sessions");
        let lines = vec![
            json!({"type":"model_change","id":"mc","timestamp":"2026-08-04T00:00:00Z","model":"openai-codex/gpt-5.6-sol"}).to_string(),
            json!({"type":"compaction","id":"c1","timestamp":"2026-08-04T00:01:00Z","preserveData":{"openaiRemoteCompaction":{"usage":{"inputTokens":1000,"outputTokens":50,"cachedInputTokens":400,"reasoningOutputTokens":10}}}}).to_string(),
            json!({"type":"model_usage","id":"w1","timestamp":"2026-08-04T00:02:00Z","purpose":"cache-warm","provider":"anthropic","model":"claude-sonnet-5-5",
                   "usage":{"input":2,"output":15,"cacheRead":245567,"cacheWrite":0,"cost":{"total":0.05}}}).to_string(),
        ];
        write_lines(&root.join("p/a.jsonl"), &lines);
        let mut index = open(&dir);
        drain(&mut index, &set_of(TOOL_OMP, &root));
        let r = report(&index, None, now());
        let sol = r.models.iter().find(|m| m.model == "openai-codex/gpt-5.6-sol").unwrap();
        assert_eq!((sol.totals.input_tokens, sol.totals.cache_read_tokens, sol.totals.output_tokens), (600, 400, 50));
        let warm = r.models.iter().find(|m| m.model == "anthropic/claude-sonnet-5-5").unwrap();
        assert_eq!(warm.totals.cache_read_tokens, 245_567);
    }

    #[test]
    fn long_context_tier_applies_above_the_threshold_only() {
        let rec = |ctx: u64| Rec { key: 0, ts: 1, tool: TOOL_CODEX, model: "gpt-5.6-sol".into(), session: 0, inp: 1_000_000, out: 0, cr: 0, cw: 0, cw1h: 0, ctx, recorded_cost: None, flags: 0, cost: None };
        assert!((price_cost("gpt-5.6-sol", &rec(272_000)).unwrap() - 4.0).abs() < 1e-9);
        assert!((price_cost("gpt-5.6-sol", &rec(272_001)).unwrap() - 10.0).abs() < 1e-9);
        assert!(price_cost("gpt-5.3-codex-spark", &rec(1)).is_none());
        assert!(lookup_price("anthropic/claude-sonnet-4-5-20250929").is_some());
        assert!(lookup_price("anthropic/claude-opus-5[1m]").is_some());
    }

    #[test]
    fn candidate_roots_cover_defaults_env_homes_and_profile_homes() {
        let home = PathBuf::from(if cfg!(windows) { "C:\\Users\\tester" } else { "/home/tester" });
        let abs = |name: &str| if cfg!(windows) { PathBuf::from(format!("C:\\{name}")) } else { PathBuf::from(format!("/{name}")) };
        let env = |key: &str| -> Option<OsString> {
            match key {
                "CLAUDE_CONFIG_DIR" => Some(abs("claude-home").into_os_string()),
                "CODEX_HOME" => Some(abs("codex-home").into_os_string()),
                "PI_CODING_AGENT_DIR" => Some(OsString::from("~/custom-agent")),
                _ => None,
            }
        };
        let profiles = vec![("claude".to_owned(), abs("profiles/claude-1")), ("codex".to_owned(), abs("profiles/codex-1")), ("omp".to_owned(), abs("ignored"))];
        let roots = candidate_roots(Some(home.as_path()), &env, &profiles);
        let has = |tool: u8, path: PathBuf| roots.iter().any(|(t, p)| *t == tool && *p == path);
        assert!(has(TOOL_OMP, home.join(".omp").join("agent").join("sessions")));
        assert!(has(TOOL_OMP, home.join("custom-agent").join("sessions")));
        assert!(has(TOOL_CLAUDE, home.join(".claude").join("projects")));
        assert!(has(TOOL_CLAUDE, abs("claude-home").join("projects")));
        assert!(has(TOOL_CLAUDE, abs("profiles/claude-1").join("projects")));
        assert!(has(TOOL_CODEX, home.join(".codex").join("sessions")));
        assert!(has(TOOL_CODEX, home.join(".codex").join("archived_sessions")));
        assert!(has(TOOL_CODEX, abs("codex-home").join("sessions")));
        assert!(has(TOOL_CODEX, abs("profiles/codex-1").join("sessions")));
        assert!(!roots.iter().any(|(_, p)| p.starts_with(abs("ignored"))));
        // 환경변수가 상대 경로이면 쓰지 않는다.
        let relative = |key: &str| (key == "CODEX_HOME").then(|| OsString::from("relative/dir"));
        assert!(!candidate_roots(Some(home.as_path()), &relative, &[]).iter().any(|(_, p)| p.starts_with("relative")));
    }

    #[test]
    fn resolve_roots_merges_the_same_directory_and_skips_missing_ones() {
        let dir = Temp::new();
        let root = dir.join("projects");
        fs::create_dir_all(&root).unwrap();
        let set = resolve_roots(vec![(TOOL_CLAUDE, root.clone()), (TOOL_CLAUDE, root.join(".").join("..").join("projects")), (TOOL_CLAUDE, dir.join("missing")), (TOOL_CODEX, root.clone())]);
        assert_eq!(set.sources.len(), 2);
        assert_eq!(set.bad_roots, 0);
    }

    #[test]
    fn index_file_holds_no_paths_content_or_email() {
        let dir = Temp::new();
        let root = dir.join("projects").join("secret-project-name");
        let mut line = claude_msg("msg_1", "2026-08-04T00:00:01Z", (1, 10, 0, 0), 0);
        line = line.replace("\"sessionId\"", "\"cwd\":\"/Users/someone/private\",\"email\":\"person@example.com\",\"sessionId\"");
        write_lines(&root.join("conversation-title-xyz.jsonl"), &[line]);
        let mut index = open(&dir);
        drain(&mut index, &set_of(TOOL_CLAUDE, &dir.join("projects")));
        drop(index);
        let bytes = fs::read(dir.join("index.sqlite3")).unwrap();
        let wal = fs::read(dir.join("index.sqlite3-wal")).unwrap_or_default();
        for needle in ["secret-project-name", "conversation-title-xyz", "person@example.com", "/Users/someone", "sess-1", "msg_1"] {
            assert!(!has(&bytes, needle.as_bytes()) && !has(&wal, needle.as_bytes()), "index leaked {needle}");
        }
    }

    #[test]
    fn report_serializes_camel_case_with_safe_integers_and_flattened_totals() {
        let dir = Temp::new();
        let root = dir.join("projects");
        write_lines(&root.join("p/a.jsonl"), &[claude_msg("m1", "2026-08-04T00:00:01Z", (1, 10, 0, 0), 0)]);
        let mut index = open(&dir);
        drain(&mut index, &set_of(TOOL_CLAUDE, &root));
        let value = serde_json::to_value(report(&index, None, now())).unwrap();
        for key in ["indexedAt", "scanning", "truncated", "coverageStart", "coverageEnd", "filesScanned", "filesPending", "totals", "tools", "models", "warnings", "excludedRecords", "duplicateRecords"] {
            assert!(value.get(key).is_some(), "{key}");
        }
        let tool = &value["tools"][0];
        assert_eq!(tool["tool"], "claude");
        assert_eq!(tool["sessions"], 1);
        assert_eq!(tool["inputTokens"], 1);
        assert!(tool["costUsd"].is_number());
        assert_eq!(value["models"][0]["model"], "claude-opus-5");
        assert_eq!(safe(u64::MAX), MAX_SAFE);
    }

    /// 실제 이 PC의 기록으로 끝까지 색인해 숫자 요약만 출력한다(내용·경로·계정은 출력하지 않는다).
    /// 임시 색인 파일을 쓰므로 앱 색인과 서비스 DB는 건드리지 않는다.
    /// 실행: `~/.cargo/bin/cargo test -p ai-account-manager --release local_usage::tests::real_homes_smoke -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn real_homes_smoke() {
        let paths = Paths::discover().unwrap();
        let set = discover(&paths);
        println!("roots: omp={} claude={} codex={} unreadable={}", set.sources.iter().filter(|s| s.tool == TOOL_OMP).count(), set.sources.iter().filter(|s| s.tool == TOOL_CLAUDE).count(), set.sources.iter().filter(|s| s.tool == TOOL_CODEX).count(), set.bad_roots);
        let dir = Temp::new();
        let mut index = open(&dir);
        let started = Instant::now();
        let mut passes = 0;
        let budget = Budget { bytes: PASS_BYTES, time: PASS_TIME };
        loop {
            let info = index.refresh(&set, aam_protocol::now_ms(), budget, Duration::from_secs(3600)).unwrap();
            passes += 1;
            if info.pending == 0 {
                break;
            }
        }
        println!("indexed in {} passes, {:.1}s", passes, started.elapsed().as_secs_f64());
        let query = Instant::now();
        let now_ms = aam_protocol::now_ms();
        let all = build_report(&index.conn, None, now_ms + 1, now_ms).unwrap();
        println!("all-time query {} ms", query.elapsed().as_millis());
        println!("{}", serde_json::to_string_pretty(&all).unwrap());
        let idle = index.refresh(&set, now_ms, budget, Duration::from_secs(3600)).unwrap();
        println!("second refresh read {} bytes (expect 0)", idle.bytes);
        let size = fs::metadata(dir.join("index.sqlite3")).map(|m| m.len()).unwrap_or(0);
        println!("index size {} bytes", size);
    }
}
