//! omp 계정 브릿지.
//!
//! omp는 `anthropic`·`openai-codex` 요청을 pi-native 형식으로 이 브릿지에 보낸다.
//! 브릿지는 계정마다 하나씩 띄운 `omp auth-gateway`(계정 풀 1개로 고정) 중 하나를 골라 전달한다.
//! 계정 선택은 AAM이 관측한 사용량과 브릿지가 직접 본 한도 응답으로 결정하고, omp 세션 단위로 고정한다.

use crate::{scheduler, store::{accounts, policy}, Service};
use aam_protocol::{now_ms, Account, ApiError, Paths, QuotaBucket};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs,
    io::{self, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

pub const PORT: u16 = aam_protocol::BRIDGE_PORT;
const GATEWAY_FIRST_PORT: u16 = 4101;
const GATEWAY_LAST_PORT: u16 = 4199;
const BROKER_URL: &str = "http://127.0.0.1:8765";
const BROKER_PORT: u16 = 8765;
/// 인증 전 자원 소모 상한. 동시 연결 수와 한 요청의 읽기·쓰기 대기 시간.
const MAX_CONNECTIONS: usize = 64;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// gateway 응답 대기. 긴 추론에서도 토큰 사이 간격은 이보다 짧다.
const UPSTREAM_READ_TIMEOUT: Duration = Duration::from_secs(300);
/// omp 확장(integrations/omp/aam-accounts.js)이 `/login`에 등록하는 Ojak 공급자와 원래 공급자.
/// 두 파일이 같은 표를 쓴다. 브릿지는 원래 공급자마다 계정 gateway를 띄운다.
pub const ALIASES: [(&str, &str); 5] = [
    ("ojak-claude", "anthropic"),
    ("ojak-codex", "openai-codex"),
    ("ojak-antigravity", "google-antigravity"),
    ("ojak-grok", "xai-oauth"),
    ("ojak-zai", "zai"),
];
/// 이름을 바꾸기 전 공급자 ID. 이미 이 이름으로 모델을 불러 둔 실행 중인 omp 세션이 끊기지 않도록 요청만 받는다.
const LEGACY_ALIASES: [(&str, &str); 5] = [
    ("aam-claude", "anthropic"),
    ("aam-codex", "openai-codex"),
    ("aam-antigravity", "google-antigravity"),
    ("aam-grok", "xai-oauth"),
    ("aam-zai", "zai"),
];
/// 브릿지 토큰은 만료되지 않는다. omp 확장의 로그인과 같은 먼 만료 시각을 준다.
const LOGIN_LIFETIME_MS: u128 = 10 * 365 * 24 * 60 * 60 * 1000;


/// Ojak 공급자 이름이나 원래 공급자 이름을 원래 공급자로 바꾼다. 브릿지가 맡지 않는 공급자면 `None`.
fn upstream(provider: &str) -> Option<&'static str> {
    ALIASES
        .iter()
        .chain(&LEGACY_ALIASES)
        .find(|(alias, original)| *alias == provider || *original == provider)
        .map(|(_, original)| *original)
}

/// 아직 broker에 없는 Ojak 공급자. 원래 공급자 gateway가 실행 중이거나 이전 ID로 로그인돼 있으면 올린다.
/// 이미 있는 항목은 다시 쓰지 않고, 어떤 자격 증명도 지우지 않는다.
pub(crate) fn missing_ojak_logins(logged_in: &[String], running: &[String]) -> Vec<&'static str> {
    let has = |name: &str| logged_in.iter().any(|item| item == name);
    ALIASES
        .iter()
        .filter(|(ojak, upstream)| {
            if has(ojak) {
                return false;
            }
            let legacy = LEGACY_ALIASES.iter().any(|(old, original)| original == upstream && has(old));
            legacy || running.iter().any(|item| item == *upstream)
        })
        .map(|(ojak, _)| *ojak)
        .collect()
}

/// Anthropic 프롬프트 캐시 수명과 맞춘 세션 고정 유지 시간.
const STICKY_MS: i64 = 60 * 60_000;
/// 동시 세션 분산에 쓰는 "사용 중" 판정 시간.
const ACTIVE_MS: i64 = 15 * 60_000;
/// omp와 같은 기준: 5시간 한도를 이만큼 쓴 계정은 뒤로 미룬다.
const HOT_PRIMARY_PERCENT: f64 = 85.0;
const SYNC_MS: i64 = 60_000;
const RESPAWN_BACKOFF_MS: i64 = 10_000;
const UNHEALTHY_MS: i64 = 15_000;
const QUOTA_BLOCK_MS: i64 = 30 * 60_000;
const RATE_BLOCK_MS: i64 = 60_000;
const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 64 * 1024 * 1024;
const PEEK_LIMIT: usize = 64 * 1024;
/// bridge.log·gateway 로그가 이 크기를 넘으면 회전한다.
const LOG_ROTATE_BYTES: u64 = 4 * 1024 * 1024;
/// sticky 세션 맵 상한. 넘으면 last_used가 오래된 항목부터 버린다.
const MAX_STICKY_SESSIONS: usize = 512;
/// 로그에 남기는 계정 별칭. identity의 sha256 앞 12 hex. 이메일·조직 ID는 넣지 않는다.
const ACCOUNT_KEY_HEX: usize = 12;
/// 로그 한 칸에 허용하는 모델 문자열 길이.
const LOG_MODEL_LIMIT: usize = 128;

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub enabled: bool,
}

fn read_settings(paths: &Paths) -> Settings {
    fs::read(paths.bridge_settings())
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn user_home() -> Option<PathBuf> {
    aam_protocol::user_home()
}
fn native_omp() -> Option<PathBuf> {
    std::env::var_os("OMP_NATIVE_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            #[cfg(windows)]
            { std::env::var_os("LOCALAPPDATA").map(PathBuf::from).map(|home| home.join("omp/omp.exe")) }
            #[cfg(unix)]
            { user_home().map(|home| home.join(".local/bin/omp")) }
        })
        .filter(|path| path.is_absolute())
}
fn read_secret(path: PathBuf) -> Option<String> {
    fs::read_to_string(path).ok().map(|text| text.trim().to_owned()).filter(|text| !text.is_empty())
}
fn gateway_token() -> Option<String> {
    read_secret(user_home()?.join(".omp/auth-gateway.token"))
}
fn broker_token() -> Option<String> {
    read_secret(user_home()?.join(".omp/auth-broker.token"))
}

/// 브릿지 인증 토큰. omp는 `models.yml`의 `!cat` 명령으로 읽으므로 파일에 평문 복사하지 않는다.
fn ensure_token(paths: &Paths) -> io::Result<String> {
    let path = paths.bridge_token();
    if let Some(token) = read_secret(path.clone()) {
        return Ok(token);
    }
    fs::create_dir_all(&paths.home)?;
    let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let mut file = aam_protocol::secure::private_options(fs::OpenOptions::new().write(true).create_new(true)).open(&path)?;
    file.write_all(token.as_bytes())?;
    Ok(token)
}

// ───────────────────────────── 계정 선택 ─────────────────────────────

/// 선택기에 넘기는 계정 후보 한 개.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub key: String,
    pub buckets: Vec<QuotaBucket>,
    pub healthy: bool,
    /// AAM 배정 정책의 "배정 포함". 끈 계정은 omp 요청에도 쓰지 않는다.
    pub enabled: bool,
    /// 공급자 수동 배정 계정. 새 대화는 이 계정이 받을 수 있으면 먼저 준다.
    pub pinned: bool,
    /// 정책이 추가 과금 경로(Codex 크레딧·Claude 추가 사용량)를 켰고, 이 계정에 신선한 관측이 그 경로를 명시하면
    /// 관측 신선도 기준(ms). 구독 한도가 남은 계정이 하나도 없을 때만 `choose`가 쓴다. 선택 상태는 저장하지 않는다.
    pub paid_fallback: Option<i64>,
}

/// 정책의 추가 과금 경로 옵트인. 모두 꺼져 있으면(기본) 브릿지 선택은 바뀌지 않는다.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PaidPolicy {
    pub credits: bool,
    pub extra: bool,
    /// 두 옵트인 중 하나라도 켜졌을 때만 관측 신선도 기준(ms).
    pub fresh_ms: Option<i64>,
    pub stale_seconds: u64,
}
impl PaidPolicy {
    pub(crate) fn of(policy: &aam_protocol::Policy) -> Self {
        let fresh = policy.stale_after_seconds.saturating_mul(1000).min(i64::MAX as u64) as i64;
        Self {
            credits: policy.use_credits_after_limit,
            extra: policy.use_extra_usage_after_limit,
            fresh_ms: (policy.use_credits_after_limit || policy.use_extra_usage_after_limit).then_some(fresh),
            stale_seconds: policy.stale_after_seconds,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Sticky {
    pub key: String,
    pub model: String,
    /// omp가 요청에 싣는 작업 폴더. 프로젝트별 사용 현황 표시에 쓴다.
    pub cwd: Option<String>,
    pub last_used: i64,
    pub requests: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct Block {
    pub key: String,
    /// `None`이면 계정 전체, `Some`이면 그 모델 계열만 막는다.
    pub scope: Option<String>,
    pub until: i64,
    pub reason: String,
    /// 사용량 한도 응답이면 참, 요청 속도 제한이면 거짓.
    pub quota: bool,
    /// `until`이 Ojak의 추정(응답이 리셋 시각을 알려 주지 않아 쓴 기본 대기)이면 참. 공급자가 알려 준 시각이면 거짓.
    pub estimate: bool,
}

/// 모델 전용 한도 범위. Fable·Mythos·Codex Spark처럼 따로 계산되는 한도만 범위를 갖는다.
pub(crate) fn block_scope(provider: &str, model: &str) -> Option<String> {
    let model = model.to_ascii_lowercase();
    match provider {
        "anthropic" => ["fable", "mythos"].into_iter().find(|tier| model.contains(tier)).map(str::to_owned),
        "openai-codex" => Some(if model.contains("spark") { "spark" } else { "chat" }.to_owned()),
        "google-antigravity" => Some(if model.contains("gemini") { "gemini" } else { "claude-gpt" }.to_owned()),
        _ => None,
    }
}

fn exhausted(bucket: &QuotaBucket, now: i64) -> bool {
    let spent = bucket.status == "exhausted" || bucket.used_percent.is_some_and(|used| used >= 100.0);
    // 리셋 시각이 지난 소진 기록은 오래된 관측이므로 막지 않는다.
    spent && bucket.resets_at.is_none_or(|reset| reset > now)
}

fn blocked(blocks: &[Block], key: &str, scope: Option<&str>, now: i64) -> bool {
    blocks
        .iter()
        .any(|block| block.key == key && block.until > now && (block.scope.is_none() || block.scope.as_deref() == scope))
}

/// 리셋 전까지 남은 한도를 시간당 얼마나 써야 다 쓰는지. 클수록 먼저 쓰는 게 이득이다.
/// 요청에 걸리는 주간 한도 중 가장 빡빡한 값을 쓴다.
fn weekly_drain(buckets: &[QuotaBucket], model: &str, now: i64) -> f64 {
    buckets
        .iter()
        .filter(|bucket| scheduler::applies(bucket, model) && !scheduler::short_window(bucket))
        .filter_map(|bucket| {
            let used = bucket.used_percent?;
            let remaining = ((100.0 - used) / 100.0).clamp(0.0, 1.0);
            let hours = bucket
                .resets_at
                .map_or(168.0, |reset| ((reset - now).max(60_000) as f64 / 3_600_000.0).min(168.0));
            Some(remaining / hours)
        })
        .fold(None, |min: Option<f64>, value| Some(min.map_or(value, |current| current.min(value))))
        .unwrap_or(0.0)
}

fn primary_used(buckets: &[QuotaBucket], model: &str) -> f64 {
    buckets
        .iter()
        .filter(|bucket| scheduler::applies(bucket, model) && scheduler::short_window(bucket))
        .filter_map(|bucket| bucket.used_percent)
        .fold(0.0, f64::max)
}

/// 요청 모델에 걸리는 한도 중 하나라도 안전 여유량 안쪽까지 썼는지.
fn in_reserve(buckets: &[QuotaBucket], model: &str, reserve: f64) -> bool {
    reserve > 0.0 && buckets.iter().any(|bucket| scheduler::applies(bucket, model) && bucket.used_percent.is_some_and(|used| used >= 100.0 - reserve))
}

/// 요청에 걸리는 한도가 전부 "신선한 소진 관측"이고 공용 한도(모델 전용 아님)일 때만 추가 과금 경로가 받는다.
fn paid_cover(buckets: &[QuotaBucket], model: &str, now: i64, fresh_ms: i64) -> bool {
    let mut spent = false;
    for bucket in buckets.iter().filter(|bucket| scheduler::applies(bucket, model)) {
        if exhausted(bucket, now) {
            if bucket.model.is_some() || !scheduler::exhausted_observation_fresh(bucket, now, fresh_ms) {
                return false;
            }
            spent = true;
        }
    }
    spent
}

/// 요청에 쓸 계정을 고른다. 고를 수 없으면 `None`.
/// 순서: 세션 고정 → 안전 여유량이 남은 계정 우선 → (5시간 과열 여부, 동시 사용 세션 수, 주간 소진 급한 정도, 5시간 사용률).
/// 여유량 안쪽 계정은 여유 있는 계정이 하나도 없을 때만 새 세션을 받는다. 이미 고정된 세션은 소진 전까지 유지한다.
pub(crate) fn choose(
    candidates: &[Candidate],
    provider: &str,
    model: &str,
    session: Option<&str>,
    stickies: &HashMap<String, Sticky>,
    blocks: &[Block],
    tried: &[String],
    reserve: f64,
    now: i64,
) -> Option<usize> {
    let scope = block_scope(provider, model);
    let eligible: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            candidate.healthy
                && candidate.enabled
                && !tried.contains(&candidate.key)
                && !blocked(blocks, &candidate.key, scope.as_deref(), now)
                && !candidate
                    .buckets
                    .iter()
                    .any(|bucket| scheduler::applies(bucket, model) && exhausted(bucket, now))
        })
        .map(|(index, _)| index)
        .collect();
    // 크레딧 fallback: 구독 한도가 남아 쓸 수 있는 계정이 하나도 없을 때만, 한도가 막은 계정 중 신선한 소진 관측과
    // 명시된 추가 과금 경로(Codex 크레딧·Claude 추가 사용량)가 있는 계정을 후보로 삼는다. 매 요청마다 새로 계산하므로
    // 리셋·회복 관측이 들어오면 다음 요청부터 구독 계정이 이기고, 그 계정에 붙은 세션 고정도 그때 풀린다
    // (고정은 eligible 안에서만 유지된다). 한도가 남았지만 일시 차단·장애로 못 쓰는 구독 계정이 있으면 기다린다.
    let eligible = if eligible.is_empty()
        && candidates.iter().all(|candidate| {
            !candidate.enabled
                || candidate.buckets.iter().any(|bucket| scheduler::applies(bucket, model) && exhausted(bucket, now))
        })
    {
        candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                // 사용자가 제외한(enabled=false) 계정에는 절대 과금 경로를 쓰지 않는다. 일반 경로와 같은 조건이다.
                candidate.healthy
                    && candidate.enabled
                    && !tried.contains(&candidate.key)
                    && !blocked(blocks, &candidate.key, scope.as_deref(), now)
                    && candidate.paid_fallback.is_some_and(|fresh_ms| paid_cover(&candidate.buckets, model, now, fresh_ms))
            })
            .map(|(index, _)| index)
            .collect()
    } else {
        eligible
    };
    let roomy: Vec<usize> = eligible.iter().copied().filter(|index| !in_reserve(&candidates[*index].buckets, model, reserve)).collect();
    if let Some(sticky) = session.and_then(|id| stickies.get(id)) {
        if now - sticky.last_used < STICKY_MS {
            // 진행 중인 대화는 프롬프트 캐시를 지키려고 안전 여유량 안쪽이어도 같은 계정에 둔다. 소진·차단·장애로
            // eligible에서 빠졌을 때만 옮긴다(그때는 어차피 캐시를 잃는다). 여유량은 새 대화의 순위에만 쓴다.
            if let Some(index) = eligible.iter().copied().find(|index| candidates[*index].key == sticky.key) {
                return Some(index);
            }
        }
    }
    let active = |key: &str| {
        stickies
            .iter()
            .filter(|(id, sticky)| Some(id.as_str()) != session && sticky.key == key && now - sticky.last_used < ACTIVE_MS)
            .count()
    };
    let pool = if roomy.is_empty() { eligible } else { roomy };
    // 공급자 수동 배정: 진행 중인 대화(위 sticky)는 그대로 두고, 새 대화는 고정 계정이 받을 수 있으면 그 계정으로.
    // 고정 계정이 소진·차단·여유량 부족이면 pool에 없으므로 아래 자동 선택으로 넘어간다.
    if let Some(index) = pool.iter().copied().find(|index| candidates[*index].pinned) {
        return Some(index);
    }
    pool.into_iter().min_by(|a, b| {
        let (left, right) = (&candidates[*a], &candidates[*b]);
        let hot = |candidate: &Candidate| primary_used(&candidate.buckets, model) >= HOT_PRIMARY_PERCENT;
        hot(left)
            .cmp(&hot(right))
            .then(active(&left.key).cmp(&active(&right.key)))
            .then(weekly_drain(&right.buckets, model, now).total_cmp(&weekly_drain(&left.buckets, model, now)))
            .then(primary_used(&left.buckets, model).total_cmp(&primary_used(&right.buckets, model)))
            .then(left.key.cmp(&right.key))
    })
}

/// 한도 응답을 받았을 때 막을 시각과, 그 시각이 Ojak의 추정인지. 관측 사용량에 소진된 한도가 있으면 그 리셋까지,
/// 없으면 기본 대기(`QUOTA_BLOCK_MS`/`RATE_BLOCK_MS`)를 쓴다. 응답이 알려 준 대기(`hinted`)가 더 늦으면 그 시각까지 막는다.
/// 기본 대기가 이겼을 때만 추정이다(공급자가 리셋 시각을 알려 주지 않았다는 뜻).
fn block_until(buckets: &[QuotaBucket], model: &str, quota: bool, hinted: Option<i64>, now: i64) -> (i64, bool) {
    let observed = buckets
        .iter()
        .filter(|bucket| scheduler::applies(bucket, model) && exhausted(bucket, now))
        .filter_map(|bucket| bucket.resets_at)
        .max();
    match observed {
        Some(reset) => (reset.max(hinted.unwrap_or(0)), false),
        None => {
            let default = now + if quota { QUOTA_BLOCK_MS } else { RATE_BLOCK_MS };
            match hinted {
                Some(hint) if hint >= default => (hint, false),
                _ => (default, true),
            }
        }
    }
}

// ───────────────────────────── gateway 감독 ─────────────────────────────

/// Windows: gateway를 kill-on-close Job에 넣는다. 서비스가 어떤 식으로 끝나든(강제 종료 포함) 핸들이 닫히며
/// gateway와 그 자손도 함께 끝난다. 그렇지 않으면 서비스를 재시작할 때마다 고아 gateway가 포트를 쥐고 남는다.
#[cfg(windows)]
type GatewayJob = aam_protocol::ProcessJob;
/// Unix는 launchd가 서비스 종료 때 프로세스 그룹을 정리한다.
#[cfg(unix)]
type GatewayJob = ();

struct Gateway {
    provider: String,
    identity: String,
    email: String,
    org: String,
    port: u16,
    child: Option<(Child, GatewayJob)>,
    spawned_at: i64,
    unhealthy_until: i64,
}
impl Gateway {
    fn key(&self) -> String {
        format!("{}|{}", self.provider, self.identity)
    }
    fn running(&mut self) -> bool {
        match self.child.as_mut().map(|(child, _)| child.try_wait()) {
            Some(Ok(None)) => true,
            Some(_) => {
                self.child = None;
                false
            }
            None => false,
        }
    }
    fn stop(&mut self) {
        // job은 child를 끝낸 뒤 함께 버려진다(Windows: kill-on-close Job 핸들을 닫는다).
        if let Some((mut child, _job)) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[derive(Default)]
struct State {
    gateways: Vec<Gateway>,
    sessions: HashMap<String, Sticky>,
    blocks: Vec<Block>,
    synced_at: i64,
    /// 서비스가 마지막으로 broker를 띄운 시각과 그 프로세스(Windows 작업 스케줄러 대체 경로).
    #[cfg(windows)]
    broker_spawned_at: i64,
    #[cfg(windows)]
    broker: Option<Child>,
    error: Option<String>,
    /// 마지막으로 읽은 계정 사용량·안전 여유량·공급자 수동 배정. 저장소를 읽지 못하면 빈 값 대신 이것을 쓴다.
    known: Option<(Vec<Account>, f64, BTreeMap<String, String>, PaidPolicy)>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayStatus {
    pub provider: String,
    /// 화면 표시용 이메일 또는 `account:<id>`. 로그에는 넣지 않는다.
    pub email: String,
    /// `logs/bridge.log`의 계정 칸과 같은 값. identity sha256의 앞 12 hex.
    pub account_key: String,
    pub port: u16,
    pub running: bool,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatus {
    pub session: String,
    pub provider: String,
    pub email: String,
    pub model: String,
    pub cwd: Option<String>,
    pub last_used_at: i64,
    pub requests: u64,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockStatus {
    pub provider: String,
    pub email: String,
    pub scope: Option<String>,
    pub until: i64,
    pub reason: String,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub enabled: bool,
    pub listening: bool,
    pub port: u16,
    pub error: Option<String>,
    pub gateways: Vec<GatewayStatus>,
    pub sessions: Vec<SessionStatus>,
    pub blocks: Vec<BlockStatus>,
}
impl Status {
    pub fn inactive() -> Self {
        Self { enabled: false, listening: false, port: PORT, error: None, gateways: vec![], sessions: vec![], blocks: vec![] }
    }
}

pub struct Bridge {
    service: Arc<Service>,
    paths: Paths,
    state: Mutex<State>,
    enabled: AtomicBool,
    listening: AtomicBool,
    connections: std::sync::atomic::AtomicUsize,
}

impl Bridge {
    pub fn start(service: &Arc<Service>) -> Arc<Self> {
        let bridge = Arc::new(Self {
            service: Arc::clone(service),
            paths: service.paths.clone(),
            state: Mutex::new(State::default()),
            enabled: AtomicBool::new(false),
            listening: AtomicBool::new(false),
            connections: std::sync::atomic::AtomicUsize::new(0),
        });
        let worker = Arc::clone(&bridge);
        std::thread::spawn(move || loop {
            worker.tick();
            std::thread::sleep(Duration::from_secs(2));
        });
        bridge
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Windows에서는 서비스가 omp broker를 띄운다(`omp-broker-service.json` 표시). 작업 스케줄러·`conhost --headless`는
    /// 서명 없는 프로그램에서 백신 행위 탐지를 부르므로 쓰지 않는다.
    /// 포트가 응답하거나 띄운 broker가 아직 살아 있으면(첫 기동은 수십 초 걸린다) 아무것도 하지 않는다.
    /// 띄운 broker가 끝났을 때만 30초 간격으로 다시 띄운다. 서비스가 끝나도 broker는 남겨 두어(실행 중인 omp가
    /// 쓰고 있음) 서비스 재시작이 omp 인증을 끊지 않게 한다.
    #[cfg(windows)]
    fn supervise_broker(&self) {
        // launcher가 검증해 기록한 omp 경로만 띄운다. 기록은 링크를 따라가지 않고, 내 소유·작은 파일만 읽는다.
        let Some(omp) = broker_marker_runtime(&self.paths.home.join("omp-broker-service.json")) else { return };
        if !port_free(BROKER_PORT) {
            return;
        }
        let now = now_ms();
        let mut state = self.state();
        if let Some(child) = state.broker.as_mut() {
            if matches!(child.try_wait(), Ok(None)) {
                return;
            }
            state.broker = None;
        }
        if now - state.broker_spawned_at < 30_000 {
            return;
        }
        state.broker_spawned_at = now;
        let log = open_bounded_log(&self.paths.home.join("logs").join("omp-broker.log"));
        let mut command = Command::new(omp);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("PI_") {
                command.env_remove(&key);
            }
        }
        command.args(["auth-broker", "serve"]).stdin(Stdio::null());
        match log {
            Ok(log) => { command.stdout(log.try_clone().map(Stdio::from).unwrap_or_else(|_| Stdio::null())).stderr(log); }
            Err(_) => { command.stdout(Stdio::null()).stderr(Stdio::null()); }
        }
        // 창 없이, 서비스 Job 밖에서 독립 프로세스로 띄운다(DETACHED + NEW_GROUP + NO_WINDOW, 가능하면 BREAKAWAY).
        use std::os::windows::process::CommandExt;
        const FLAGS: u32 = 0x0000_0008 | 0x0000_0200 | 0x0800_0000;
        state.broker = command.creation_flags(FLAGS | 0x0100_0000).spawn()
            .or_else(|_| command.creation_flags(FLAGS).spawn())
            .ok();
    }

    fn tick(self: &Arc<Self>) {
        // broker 감독은 브릿지가 꺼져 있어도 돈다. 첫 연결은 broker가 떠야 브릿지 설정까지 진행되기 때문이다.
        #[cfg(windows)]
        self.supervise_broker();
        let enabled = read_settings(&self.paths).enabled;
        self.enabled.store(enabled, Ordering::SeqCst);
        if !enabled {
            let mut state = self.state();
            for gateway in &mut state.gateways {
                gateway.stop();
            }
            state.gateways.clear();
            state.sessions.clear();
            state.blocks.clear();
            return;
        }
        if let Err(error) = ensure_token(&self.paths) {
            self.state().error = Some(format!("Ojak 연결 토큰을 만들지 못했어요: {error}"));
            return;
        }
        if !self.listening.load(Ordering::SeqCst) {
            match TcpListener::bind(("127.0.0.1", PORT)) {
                Ok(listener) => {
                    self.listening.store(true, Ordering::SeqCst);
                    let bridge = Arc::clone(self);
                    std::thread::spawn(move || {
                        for mut stream in listener.incoming().flatten() {
                            // 인증 전 자원 소모를 막는다: 동시 연결 상한, 요청 전체 데드라인(읽기·쓰기 타임아웃),
                            // 스레드 생성 실패 시 panic으로 리스너를 잃지 않고 그 연결만 거절.
                            if bridge.connections.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
                                respond(&mut stream, 503, &json!({ "error": "Ojak 연결이 너무 많아요." }));
                                continue;
                            }
                            let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
                            let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
                            bridge.connections.fetch_add(1, Ordering::SeqCst);
                            let worker = Arc::clone(&bridge);
                            let spawned = std::thread::Builder::new().name("bridge-conn".into()).spawn(move || {
                                worker.handle(stream);
                                worker.connections.fetch_sub(1, Ordering::SeqCst);
                            });
                            if spawned.is_err() {
                                bridge.connections.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                        bridge.listening.store(false, Ordering::SeqCst);
                    });
                }
                Err(error) => {
                    self.state().error = Some(format!("Ojak 연결 포트 {PORT}를 열지 못했어요: {error}"));
                    return;
                }
            }
        }
        let now = now_ms();
        let due = {
            let mut state = self.state();
            state.blocks.retain(|block| block.until > now);
            retain_sessions(&mut state.sessions, now);
            now - state.synced_at >= SYNC_MS || state.gateways.iter_mut().any(|gateway| !gateway.running())
        };
        if due {
            self.sync(now);
        }
    }

    /// broker snapshot에 맞춰 계정별 gateway를 띄우거나 정리한다.
    fn sync(&self, now: i64) {
        let snapshot = match broker_identities() {
            Ok(value) => value,
            Err(message) => {
                let mut state = self.state();
                state.error = Some(message);
                state.synced_at = now;
                return;
            }
        };
        let desired: BTreeSet<(String, String)> =
            snapshot.oauth.iter().filter(|(provider, _)| ALIASES.iter().any(|(_, original)| original == provider)).cloned().collect();
        let mut state = self.state();
        state.synced_at = now;
        state.error = None;
        state.gateways.retain_mut(|gateway| {
            let keep = desired.contains(&(gateway.provider.clone(), gateway.identity.clone()));
            if !keep {
                gateway.stop();
            }
            keep
        });
        for (provider, identity) in &desired {
            if state.gateways.iter().any(|gateway| &gateway.provider == provider && &gateway.identity == identity) {
                continue;
            }
            let used: Vec<u16> = state.gateways.iter().map(|gateway| gateway.port).collect();
            let Some(port) = (GATEWAY_FIRST_PORT..=GATEWAY_LAST_PORT).find(|port| !used.contains(port) && port_free(*port)) else {
                state.error = Some("계정 연결에 쓸 빈 포트가 없어요.".into());
                break;
            };
            let (email, org) = parse_identity(identity);
            state.gateways.push(Gateway {
                provider: provider.clone(),
                identity: identity.clone(),
                email,
                org,
                port,
                child: None,
                spawned_at: 0,
                unhealthy_until: 0,
            });
        }
        let providers = snapshot.providers.clone();
        let mut failures = Vec::new();
        for gateway in &mut state.gateways {
            if gateway.running() || now - gateway.spawned_at < RESPAWN_BACKOFF_MS {
                continue;
            }
            // 다른 프로세스가 이미 포트를 쓰면 계정이 다른 gateway일 수 있으므로 연결하지 않는다.
            if !port_free(gateway.port) {
                failures.push(format!("연결 포트 {}가 사용 중이에요.", gateway.port));
                continue;
            }
            gateway.spawned_at = now;
            match spawn_gateway(&self.paths, gateway, &providers) {
                Ok(child) => gateway.child = Some(child),
                Err(message) => failures.push(message),
            }
        }
        if !failures.is_empty() {
            state.error = Some(failures.join(" "));
        }
        // 방금 띄운 gateway도 이 동기화에서 실행 중이면 바로 올린다. 다음 60초를 기다리지 않는다.
        let running: Vec<String> = state.gateways.iter_mut().filter_map(|gateway| gateway.running().then(|| gateway.provider.clone())).collect();
        let logged_in: Vec<String> = snapshot.providers.iter().cloned().collect();
        let missing = missing_ojak_logins(&logged_in, &running);
        drop(state);
        if let Err(message) = upload_ojak_logins(&self.paths, &missing) {
            let mut state = self.state();
            state.error = Some(match state.error.take() {
                Some(existing) => format!("{existing} {message}"),
                None => message,
            });
        }
    }

    pub fn status(&self) -> Status {
        let enabled = self.enabled.load(Ordering::SeqCst);
        let mut state = self.state();
        let now = now_ms();
        let owner = |gateways: &[Gateway], key: &str| {
            gateways
                .iter()
                .find(|gateway| gateway.key() == key)
                .map(|gateway| (gateway.provider.clone(), gateway.email.clone()))
                .unwrap_or_default()
        };
        let gateways = state
            .gateways
            .iter_mut()
            .map(|gateway| GatewayStatus {
                provider: gateway.provider.clone(),
                email: gateway.email.clone(),
                account_key: account_key(&gateway.identity),
                port: gateway.port,
                running: gateway.running(),
            })
            .collect();
        let mut sessions: Vec<SessionStatus> = state
            .sessions
            .iter()
            .filter(|(_, sticky)| now - sticky.last_used < STICKY_MS)
            .map(|(id, sticky)| {
                let (provider, email) = owner(&state.gateways, &sticky.key);
                SessionStatus {
                    // omp 세션 ID는 UUIDv7이라 앞부분이 시각이다. 구분되는 끝 8자리를 보여 준다.
                    session: tail(id).to_owned(),
                    provider,
                    email,
                    model: sticky.model.clone(),
                    cwd: sticky.cwd.clone(),
                    last_used_at: sticky.last_used,
                    requests: sticky.requests,
                }
            })
            .collect();
        sessions.sort_by_key(|session| std::cmp::Reverse(session.last_used_at));
        let blocks = state
            .blocks
            .iter()
            .filter(|block| block.until > now)
            .map(|block| {
                let (provider, email) = owner(&state.gateways, &block.key);
                BlockStatus { provider, email, scope: block.scope.clone(), until: block.until, reason: block.reason.clone() }
            })
            .collect();
        Status {
            enabled,
            listening: self.listening.load(Ordering::SeqCst),
            port: PORT,
            error: state.error.clone(),
            gateways,
            sessions,
            blocks,
        }
    }

    // ───────────────────────────── HTTP 처리 ─────────────────────────────

    fn handle(&self, mut client: TcpStream) {
        // 헤더만 먼저 읽고 토큰을 확인한 뒤에 본문(최대 64 MiB)을 읽는다. 인증 없는 연결이 메모리를 잡지 못하게.
        let (request, rest) = match read_head(&mut client) {
            Ok(head) => head,
            Err(message) => return respond(&mut client, 400, &json!({ "error": message })),
        };
        if request.method == "GET" && request.path == "/healthz" {
            return respond(&mut client, 200, &json!({ "ok": true, "enabled": self.enabled.load(Ordering::SeqCst) }));
        }
        if !self.enabled.load(Ordering::SeqCst) {
            return respond(&mut client, 503, &json!({ "error": "Ojak 계정 연결이 꺼져 있어요." }));
        }
        let expected = read_secret(self.paths.bridge_token());
        let presented = request.header("authorization").and_then(|value| value.strip_prefix("Bearer ")).map(str::trim);
        if expected.is_none() || !constant_eq(presented.unwrap_or(""), expected.as_deref().unwrap_or("")) {
            return respond(&mut client, 401, &json!({ "error": "Ojak 연결 인증에 실패했어요." }));
        }
        let request = match read_body(&mut client, request, rest) {
            Ok(request) => request,
            Err(message) => return respond(&mut client, 400, &json!({ "error": message })),
        };
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/v1/models") => self.models(&mut client),
            ("GET", "/v1/providers") => self.providers(&mut client),
            ("POST", "/v1/pi/stream") => self.stream(&mut client, &request),
            _ => respond(&mut client, 404, &json!({ "error": "Ojak은 pi-native 요청만 처리해요." })),
        }
    }

    fn models(&self, client: &mut TcpStream) {
        let ports: Vec<u16> = self.state().gateways.iter_mut().filter_map(|gateway| gateway.running().then_some(gateway.port)).collect();
        let token = gateway_token().unwrap_or_default();
        let mut seen = BTreeSet::new();
        let mut data = Vec::new();
        for port in ports {
            let Ok(body) = get_json(port, "/v1/models", &token) else { continue };
            for model in body.get("data").and_then(Value::as_array).into_iter().flatten() {
                if let Some(id) = model.get("id").and_then(Value::as_str) {
                    if seen.insert(id.to_owned()) {
                        data.push(model.clone());
                    }
                }
            }
        }
        respond(client, 200, &json!({ "object": "list", "data": data }));
    }

    /// omp 확장이 로그인할 때 확인하는 공급자별 실행 중인 계정 수.
    fn providers(&self, client: &mut TcpStream) {
        let mut counts: Vec<(&str, usize)> = ALIASES.iter().map(|(_, original)| (*original, 0)).collect();
        for gateway in &mut self.state().gateways {
            if gateway.running() {
                if let Some(entry) = counts.iter_mut().find(|(provider, _)| *provider == gateway.provider) {
                    entry.1 += 1;
                }
            }
        }
        let providers: Vec<Value> = counts.into_iter().map(|(provider, accounts)| json!({ "provider": provider, "accounts": accounts })).collect();
        respond(client, 200, &json!({ "providers": providers }));
    }

    fn candidates(&self, provider: &str, accounts: &[Account], pins: &BTreeMap<String, String>, paid: PaidPolicy, now: i64) -> Vec<(Candidate, u16)> {
        let pin = pins
            .get(aam_protocol::pin_provider(provider))
            .and_then(|id| accounts.iter().find(|account| &account.id == id));
        let mut state = self.state();
        state
            .gateways
            .iter_mut()
            .filter(|gateway| gateway.provider == provider)
            .map(|gateway| {
                let healthy = gateway.running() && gateway.unhealthy_until <= now;
                let (buckets, enabled, pinned, paid_fallback) = account_view(gateway, accounts, pin, paid, now);
                (Candidate { key: gateway.key(), buckets, healthy, enabled, pinned, paid_fallback }, gateway.port)
            })
            .collect()
    }

    fn stream(&self, client: &mut TcpStream, request: &Request) {
        let body: Value = match serde_json::from_slice(&request.body) {
            Ok(value) => value,
            Err(_) => return respond(client, 400, &json!({ "error": "요청 본문이 JSON이 아니에요." })),
        };
        let Some(Target { provider, model, session, turn }) = request_target(&body) else {
            let named = body.get("modelId").and_then(Value::as_str).unwrap_or_default();
            return respond(client, 400, &json!({ "error": format!("Ojak이 처리하지 않는 모델이에요: {named}") }));
        };
        let tools = body.pointer("/context/tools").and_then(Value::as_array).map_or(0, Vec::len);
        let cwd = body.pointer("/options/cwd").and_then(Value::as_str).map(str::to_owned);
        let upstream_body = for_gateway(body, &provider, &model);
        // 저장소를 못 읽었을 때 빈 계정 목록으로 고르면 모든 계정이 여유 있어 보인다. 마지막 값을 쓰고, 없으면 거절한다.
        let fresh = self
            .service
            .lock()
            .and_then(|store| {
                let policy = policy(&store.connection)?;
                Ok((accounts(&store.connection)?, policy.safety_reserve_percent, policy.provider_pins.clone(), PaidPolicy::of(&policy)))
            });
        let (accounts, reserve, pins, paid) = match fresh {
            Ok(view) => {
                self.state().known = Some(view.clone());
                view
            }
            Err(_) => match self.state().known.clone() {
                Some(view) => view,
                None => return respond(client, 503, &json!({ "error": "Ojak이 계정 한도를 아직 읽지 못했어요. 잠시 뒤 다시 시도해 주세요." })),
            },
        };
        let Some(token) = gateway_token() else {
            return respond(client, 503, &json!({ "error": "omp 로그인 연결 토큰이 없어요." }));
        };
        let mut tried = Vec::new();
        let mut last_limited: Option<Vec<u8>> = None;
        loop {
            let now = now_ms();
            let mut candidates = self.candidates(&provider, &accounts, &pins, paid, now);
            // 서비스 재시작 직후에는 계정 gateway가 아직 뜨는 중일 수 있다. 전부 준비될 때까지 최대 10초 기다린다.
            for _ in 0..20 {
                if tried.is_empty() && candidates.iter().any(|(c, _)| !c.healthy) && self.starting_up(now) {
                    std::thread::sleep(Duration::from_millis(500));
                    candidates = self.candidates(&provider, &accounts, &pins, paid, now_ms());
                } else {
                    break;
                }
            }
            let pool: Vec<Candidate> = candidates.iter().map(|(candidate, _)| candidate.clone()).collect();
            // 고른 계정을 같은 잠금 안에서 세션에 기록한다. 응답을 기다리는 동안 들어온 다른 세션이
            // 이 배정을 보고 다른 계정으로 분산되게 하기 위함이다. 한도 응답이면 다음 반복에서 덮어쓴다.
            let chosen = {
                let mut state = self.state();
                let chosen = choose(&pool, &provider, &model, session.as_deref(), &state.sessions, &state.blocks, &tried, reserve, now);
                if let (Some(index), Some(id), true) = (chosen, &session, turn) {
                    if id.len() <= 128 {
                        let sticky = state.sessions.entry(id.clone()).or_insert_with(|| Sticky {
                            key: String::new(),
                            model: String::new(),
                            cwd: None,
                            last_used: now,
                            requests: 0,
                        });
                        sticky.key = pool[index].key.clone();
                        sticky.model = sanitize_log_model(&model);
                        if let Some(cwd) = &cwd {
                            sticky.cwd = Some(bounded_text(cwd, 1024));
                        }
                        sticky.last_used = now;
                        retain_sessions(&mut state.sessions, now);
                    }
                }
                chosen
            };
            let Some(index) = chosen else {
                // 왜 아무도 못 받는지 이 시점의 상태로 분류해 한 줄 남긴다. 사고 뒤에 원인을 다시 맞출 수 있어야 한다.
                let causes = {
                    let state = self.state();
                    classify_all(&pool, &provider, &model, &state.blocks, now)
                };
                let reset = summarize(&causes);
                self.write_log_line(&format_reject_line(now, &provider, &model, session.as_deref().map_or("-", tail), &causes, reset.as_ref()));
                if last_limited.is_none() {
                    if let Some(reset) = reset {
                        let alternatives = self.running_alternatives(&provider);
                        return respond_quota(client, &provider, &model, &reset, &alternatives, now);
                    }
                }
                break;
            };
            let (candidate, port) = &candidates[index];
            tried.push(candidate.key.clone());
            let mut upstream = match forward(*port, &token, request, &upstream_body) {
                Ok(stream) => stream,
                Err(_) => {
                    self.mark_unhealthy(&candidate.key, now);
                    continue;
                }
            };
            let (head, peek) = match peek_response(&mut upstream) {
                Ok(value) => value,
                Err(_) => {
                    self.mark_unhealthy(&candidate.key, now);
                    continue;
                }
            };
            if let Some(Limit { quota, retry_ms }) = limit_signal(&head, &peek) {
                // 응답이 알려 준 대기가 관측 리셋보다 길면 그 시각까지 막는다(최대 8일).
                let hinted = retry_ms.map(|ms| now + ms.min(8 * 24 * 60 * 60_000));
                let (until, estimate) = block_until(&candidate.buckets, &model, quota, hinted, now);
                let scope = if quota { block_scope(&provider, &model) } else { None };
                self.state().blocks.push(Block {
                    key: candidate.key.clone(),
                    scope: scope.clone(),
                    until,
                    reason: if quota { "사용량 한도 소진".into() } else { "요청 속도 제한".into() },
                    quota,
                    estimate,
                });
                self.write_log_line(&format_block_line(
                    now,
                    &provider,
                    &model,
                    session.as_deref().map_or("-", tail),
                    &account_key(identity_from_key(&candidate.key)),
                    quota,
                    scope.as_deref(),
                    until,
                    estimate,
                ));
                let mut bytes = head;
                bytes.extend_from_slice(&peek);
                let _ = upstream.take(PEEK_LIMIT as u64).read_to_end(&mut bytes);
                last_limited = Some(bytes);
                continue;
            }
            if let (Some(id), true) = (&session, turn) {
                if let Some(sticky) = self.state().sessions.get_mut(id) {
                    sticky.requests += 1;
                }
            }
            let reserve_fallback = in_reserve(&candidate.buckets, &model, reserve);
            let alias = account_key(identity_from_key(&candidate.key));
            self.write_log_line(&format_usage_line(
                now_ms(),
                &provider,
                &model,
                session.as_deref().map_or("-", tail),
                turn,
                tools,
                &alias,
                reserve_fallback,
            ));
            if client.write_all(&head).and_then(|_| client.write_all(&peek)).is_ok() {
                let _ = io::copy(&mut upstream, client);
            }
            let _ = client.shutdown(Shutdown::Both);
            return;
        }
        match last_limited {
            Some(bytes) => {
                let _ = client.write_all(&bytes);
                let _ = client.shutdown(Shutdown::Both);
            }
            None => respond(client, 503, &json!({ "error": format!("Ojak: {model}에 쓸 수 있는 {provider} 계정이 없어요.") })),
        }
    }


    /// 요청 요약 한 줄. 이메일·조직·프롬프트·토큰은 기록하지 않는다.
    fn write_log_line(&self, line: &str) {
        let path = self.paths.home.join("logs/bridge.log");
        rotate_log(&path);
        if let Ok(mut file) = aam_protocol::secure::private_options(fs::OpenOptions::new().create(true).append(true)).open(&path) {
            // 줄과 줄바꿈을 한 번에 쓴다. `writeln!`은 두 번에 나눠 써서 동시 요청의 줄이 한 줄로 붙었다.
            let _ = file.write_all(format!("{line}\n").as_bytes());
        }
    }

    /// 최근 10초 안에 띄운 gateway가 있으면 참.
    fn starting_up(&self, now: i64) -> bool {
        self.state().gateways.iter().any(|gateway| now - gateway.spawned_at < 10_000)
    }

    /// 지금 gateway가 떠 있는 다른 공급자의 Ojak 공급자 ID. 429 안내에서 "이쪽을 써 보세요"로 이름을 댈 수 있는 곳만 담는다.
    fn running_alternatives(&self, provider: &str) -> Vec<&'static str> {
        let mut state = self.state();
        ALIASES
            .iter()
            .filter(|(_, original)| *original != provider && state.gateways.iter_mut().any(|gateway| gateway.provider == *original && gateway.running()))
            .map(|(alias, _)| *alias)
            .collect()
    }

    fn mark_unhealthy(&self, key: &str, now: i64) {
        if let Some(gateway) = self.state().gateways.iter_mut().find(|gateway| gateway.key() == key) {
            gateway.unhealthy_until = now + UNHEALTHY_MS;
        }
    }
}

/// pi-native 본문에서 (공급자, 모델 ID, 세션 ID)를 꺼낸다.
/// 대화 요청은 `options.model`을 싣고, judge 같은 보조 요청은 `modelId: "<공급자>/<모델>"`만 싣는다.
/// 보조 요청이 대화와 같은 `options.sessionId`를 쓰면 대화와 같은 계정으로 간다.
pub(crate) fn request_target(body: &Value) -> Option<Target> {
    let options = body.get("options");
    let qualified = body.get("modelId").and_then(Value::as_str).and_then(|id| id.split_once('/'));
    let provider = options
        .and_then(|options| options.pointer("/model/provider"))
        .and_then(Value::as_str)
        .or(qualified.map(|(provider, _)| provider))?;
    let model = options
        .and_then(|options| options.pointer("/model/id"))
        .and_then(Value::as_str)
        .or(qualified.map(|(_, id)| id))
        .filter(|id| !id.is_empty())?;
    let provider = upstream(provider)?;
    let session = options.and_then(|options| options.get("sessionId")).and_then(Value::as_str).map(str::to_owned);
    let turn = options.and_then(|options| options.get("model")).is_some();
    Some(Target { provider: provider.to_owned(), model: model.to_owned(), session, turn })
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub provider: String,
    pub model: String,
    pub session: Option<String>,
    /// 대화 요청이면 참. judge 같은 보조 요청은 세션 고정을 따르기만 하고 새로 만들지 않는다.
    pub turn: bool,
}

/// gateway는 `modelId`로 자기 모델 목록에서 모델을 찾는다. Ojak 공급자 이름(`ojak-claude`)으로 온 요청도
/// 원래 공급자 모델(`anthropic/<모델>`)을 쓰도록 공급자를 붙이고, `options.model.provider`도 원래 이름으로 바꾼다.
pub(crate) fn for_gateway(mut body: Value, provider: &str, model: &str) -> Vec<u8> {
    body["modelId"] = Value::String(format!("{provider}/{model}"));
    if let Some(options_model) = body.pointer_mut("/options/model").and_then(Value::as_object_mut) {
        options_model.insert("provider".into(), Value::String(provider.to_owned()));
    }
    serde_json::to_vec(&body).unwrap_or_default()
}

fn tail(id: &str) -> &str {
    id.char_indices().rev().nth(7).map_or(id, |(index, _)| &id[index..])
}

fn port_free(port: u16) -> bool {
    TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200)).is_err()
}

/// broker identityKey `email:<email>|org:<org>`에서 이메일과 조직을 꺼낸다.
fn parse_identity(identity: &str) -> (String, String) {
    let field = |name: &str| {
        identity
            .split('|')
            .find_map(|part| part.strip_prefix(name))
            .unwrap_or_default()
            .to_ascii_lowercase()
    };
    let email = field("email:");
    // 이메일 없이 계정 ID로만 식별되는 공급자(xAI 등)는 계정 ID를 표시 이름으로 쓴다.
    let label = if email.is_empty() { identity.split('|').next().unwrap_or_default().to_owned() } else { email };
    (label, field("org:"))
}

/// AAM이 관측한 같은 계정의 사용량 버킷과 배정 포함 여부.
/// 도구별 연결이 여러 개여도 같은 버킷은 최신 관측 하나만 쓰고, 연결 중 하나라도 배정에서 빼면 omp에서도 뺀다.
/// 세 번째 값은 이 gateway 계정이 공급자 수동 배정 계정(`pin`)과 같은 실제 계정인지다.
fn account_view(gateway: &Gateway, accounts: &[Account], pin: Option<&Account>, paid: PaidPolicy, now: i64) -> (Vec<QuotaBucket>, bool, bool, Option<i64>) {
    let provider = match gateway.provider.as_str() {
        "openai-codex" => "openai",
        "google-antigravity" => "google",
        "xai-oauth" => "xai",
        "zai" => "other",
        other => other,
    };
    let mut merged: HashMap<String, QuotaBucket> = HashMap::new();
    let mut enabled = true;
    let mut pinned = false;
    let mut paid_ok = false;
    for account in accounts {
        if account.provider != provider || account.email.as_deref().map(str::to_ascii_lowercase).as_deref() != Some(gateway.email.as_str()) {
            continue;
        }
        let workspace = account
            .identity_key
            .as_deref()
            .and_then(|key| key.split('|').find_map(|part| part.strip_prefix("workspace:")))
            .filter(|value| !value.is_empty())
            .map(str::to_ascii_lowercase);
        if workspace.is_some_and(|workspace| !gateway.org.is_empty() && workspace != gateway.org) {
            continue;
        }
        enabled &= account.enabled;
        pinned |= pin.is_some_and(|pin| scheduler::same_identity(pin, account));
        // Codex 크레딧(openai)과 Claude 추가 사용량(anthropic)은 단위가 달라 각 공급자에서만 본다.
        paid_ok |= (paid.credits && provider == "openai" && account.credits.as_ref().is_some_and(|credits| credits.usable(now, paid.stale_seconds)))
            || (paid.extra && provider == "anthropic" && account.extra_usage.as_ref().is_some_and(|extra| extra.usable(now, paid.stale_seconds)));
        for bucket in &account.buckets {
            let newer = merged.get(&bucket.id).is_none_or(|current| bucket.observed_at > current.observed_at);
            if newer {
                merged.insert(bucket.id.clone(), bucket.clone());
            }
        }
    }
    // 추가 과금 경로는 이 gateway와 같은 계정으로 확인된 연결 중 하나라도 신선한 관측으로 명시할 때만 쓴다.
    let paid_fallback = paid.fresh_ms.filter(|_| paid_ok);
    (merged.into_values().collect(), enabled, pinned, paid_fallback)
}

struct BrokerIdentities {
    oauth: Vec<(String, String)>,
    providers: BTreeSet<String>,
}

/// broker snapshot에서 공급자와 identityKey만 꺼낸다. 토큰 등 나머지 값은 보관하지 않는다.
fn broker_identities() -> Result<BrokerIdentities, String> {
    let token = broker_token().ok_or("omp 로그인 연결 토큰을 읽지 못했어요. `aam omp-broker connect`를 먼저 실행해 주세요.")?;
    let body = http_get(BROKER_PORT, "/v1/snapshot", &token).map_err(|_| "omp 로그인 연결에 닿지 못했어요.".to_owned())?;
    let mut oauth = Vec::new();
    let mut providers = BTreeSet::new();
    fn walk(value: &Value, oauth: &mut Vec<(String, String)>, providers: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                if let (Some(provider), Some(identity)) =
                    (map.get("provider").and_then(Value::as_str), map.get("identityKey").and_then(Value::as_str))
                {
                    providers.insert(provider.to_owned());
                    let oauth_row = map
                        .get("type")
                        .or_else(|| map.get("credential").and_then(|credential| credential.get("type")))
                        .and_then(Value::as_str)
                        .is_none_or(|kind| kind == "oauth");
                    if oauth_row && !identity.is_empty() {
                        oauth.push((provider.to_owned(), identity.to_owned()));
                    }
                }
                for child in map.values() {
                    walk(child, oauth, providers);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| walk(item, oauth, providers)),
            _ => {}
        }
    }
    walk(&body, &mut oauth, &mut providers);
    oauth.sort();
    oauth.dedup();
    Ok(BrokerIdentities { oauth, providers })
}

fn spawn_gateway(paths: &Paths, gateway: &Gateway, providers: &BTreeSet<String>) -> Result<(Child, GatewayJob), String> {
    let omp = native_omp().ok_or("원래 omp 실행 파일 경로를 확인하지 못했어요.")?;
    let broker = broker_token().ok_or("omp 로그인 연결 토큰을 읽지 못했어요.")?;
    let root = paths.home.join("bridge");
    let agent = root.join("agent");
    let pools = root.join("pools");
    let logs = paths.home.join("logs");
    for dir in [&agent, &pools, &logs] {
        fs::create_dir_all(dir).map_err(|_| "브릿지 작업 폴더를 만들지 못했습니다.".to_owned())?;
    }
    let _ = aam_protocol::secure::restrict_dir(&root);
    // 계정 풀: 이 gateway가 다룰 계정 하나만 보이고 나머지 공급자는 모두 숨긴다.
    let mut pool = serde_json::Map::new();
    for provider in providers.iter().map(String::as_str).chain(ALIASES.iter().flat_map(|(alias, original)| [*alias, *original])) {
        pool.insert(provider.to_owned(), json!([]));
    }
    pool.insert(gateway.provider.clone(), json!([gateway.identity]));
    let pool_path = pools.join(format!("{}.json", gateway.port));
    fs::write(&pool_path, serde_json::to_vec(&Value::Object(pool)).unwrap_or_default())
        .and_then(|_| aam_protocol::secure::restrict_file(&pool_path))
        .map_err(|_| "계정 풀 파일을 쓰지 못했습니다.".to_owned())?;
    let log = open_bounded_log(&logs.join(format!("bridge-gateway-{}.log", gateway.port)))
        .map_err(|_| "gateway 로그 파일을 열지 못했습니다.".to_owned())?;
    let mut command = Command::new(omp);
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("OMP_") || name.starts_with("PI_") {
            command.env_remove(&key);
        }
    }
    command
        .args(["auth-gateway", "serve", &format!("--bind=127.0.0.1:{}", gateway.port)])
        // 사용자 models.yml을 읽으면 anthropic 요청이 다시 브릿지로 돌아오므로 전용 설정 폴더를 쓴다.
        .env("PI_CODING_AGENT_DIR", &agent)
        .env("OMP_AUTH_BROKER_URL", BROKER_URL)
        .env("OMP_AUTH_BROKER_TOKEN", broker)
        .env("OMP_AUTH_BROKER_ACCOUNT_POOL_FILE", &pool_path)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|_| "gateway 로그를 준비하지 못했습니다.".to_owned())?)
        .stderr(log);
    let failed = |_| format!("포트 {} gateway를 시작하지 못했습니다.", gateway.port);
    // 창 없는 서비스가 콘솔 프로그램을 띄우면 Windows는 새 콘솔 창을 연다. 출력은 로그 파일로 가므로 창을 만들지 않는다.
    #[cfg(windows)]
    return aam_protocol::spawn_in_job(&mut command, true, true, false).map_err(failed);
    #[cfg(unix)]
    return command.spawn().map(|child| (child, ())).map_err(failed);
}

/// gateway identity의 안정적인 비식별 별칭. 이메일·조직 ID를 로그에 남기지 않기 위함이다.
pub(crate) fn account_key(identity: &str) -> String {
    let digest = Sha256::digest(identity.as_bytes());
    digest.iter().take(ACCOUNT_KEY_HEX / 2).fold(String::new(), |mut out, byte| {
        out.push_str(&format!("{byte:02x}"));
        out
    })
}

fn identity_from_key(key: &str) -> &str {
    key.split_once('|').map(|(_, identity)| identity).unwrap_or(key)
}

fn bounded_text(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

/// 로그 칸을 깨는 문자와 이메일로 읽힐 문자를 빼고 길이를 제한한다.
pub(crate) fn sanitize_log_model(model: &str) -> String {
    let mut out = String::new();
    for ch in model.chars() {
        if out.len() >= LOG_MODEL_LIMIT {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '+' | '/' | '-') {
            out.push(ch);
        }
    }
    if out.is_empty() { "-".into() } else { out }
}

fn sanitize_log_token(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if out.len() >= 32 {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        }
    }
    if out.is_empty() { "-".into() } else { out }
}

/// `<ms> <provider>/<model> session=<id> turn=<bool> tools=<n> -> <accountKey> [reserve-fallback]`
/// 계정 칸은 identity sha256의 앞 12 hex다. 데스크톱 파서와 함께 바꾼다.
pub(crate) fn format_usage_line(
    at: i64,
    provider: &str,
    model: &str,
    session: &str,
    turn: bool,
    tools: usize,
    account_key: &str,
    reserve_fallback: bool,
) -> String {
    format!(
        "{at} {provider}/{} session={} turn={turn} tools={tools} -> {account_key}{}",
        sanitize_log_model(model),
        sanitize_log_token(session),
        if reserve_fallback { " reserve-fallback" } else { "" }
    )
}

/// 위 형식의 한 줄을 (시각, 공급자, 모델, 계정 별칭, 세션, 대화 요청 여부)로 나눈다.
/// 실제 파서는 데스크톱(`apps/desktop/src-tauri/src/main.rs`)에 있고, 이것은 쓰기 형식과 맞는지 확인하는 테스트용이다.
#[cfg(test)]
fn parse_usage_line(line: &str) -> Option<(i64, &str, &str, &str, &str, bool)> {
    let mut parts = line.split(' ').filter(|part| !part.is_empty());
    let at = parts.next()?.parse::<i64>().ok()?;
    let (provider, model) = parts.next()?.split_once('/')?;
    let mut session = "-";
    let mut turn = false;
    let mut key = None;
    let mut after_arrow = false;
    for part in parts {
        if after_arrow {
            key = Some(part);
            break;
        }
        if let Some(value) = part.strip_prefix("session=") {
            session = value;
        } else if let Some(value) = part.strip_prefix("turn=") {
            turn = value == "true";
        } else if part == "->" {
            after_arrow = true;
        }
    }
    let account = key?;
    if provider.is_empty() || model.is_empty() || account.is_empty() || account.contains('@') {
        return None;
    }
    Some((at, provider, model, account, session, turn))
}

fn retain_sessions(sessions: &mut HashMap<String, Sticky>, now: i64) {
    sessions.retain(|_, sticky| now.saturating_sub(sticky.last_used) < 24 * 60 * 60_000);
    if sessions.len() <= MAX_STICKY_SESSIONS {
        return;
    }
    let mut ranked: Vec<(String, i64)> = sessions.iter().map(|(id, sticky)| (id.clone(), sticky.last_used)).collect();
    ranked.sort_by(|left, right| left.1.cmp(&right.1).then_with(|| left.0.cmp(&right.0)));
    let drop_count = ranked.len() - MAX_STICKY_SESSIONS;
    for (id, _) in ranked.into_iter().take(drop_count) {
        sessions.remove(&id);
    }
}

fn rotate_log(path: &Path) {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink() && meta.len() > LOG_ROTATE_BYTES) {
        let _ = fs::rename(path, path.with_extension("log.1"));
    }
}

/// gateway stdout/stderr. 0600으로 만들고 bridge.log와 같이 크기를 제한한다. symlink는 열지 않는다.
fn open_bounded_log(path: &Path) -> io::Result<fs::File> {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "symlink"));
    }
    rotate_log(path);
    let file = aam_protocol::secure::private_options(fs::OpenOptions::new().create(true).append(true)).open(path)?;
    aam_protocol::secure::restrict_file(path)?;
    Ok(file)
}

// ───────────────────────────── HTTP 도우미 ─────────────────────────────

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}
impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
    }
}

fn head_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n").map(|index| index + 4)
}

/// 요청 줄과 헤더만 읽는다. 헤더 뒤에 딸려 온 본문 바이트는 그대로 돌려준다.
fn read_head(stream: &mut TcpStream) -> Result<(Request, Vec<u8>), String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 16 * 1024];
    let end = loop {
        if let Some(end) = head_end(&buffer) {
            break end;
        }
        if buffer.len() > MAX_HEAD {
            return Err("요청 헤더가 너무 큽니다.".into());
        }
        let read = stream.read(&mut chunk).map_err(|_| "요청을 읽지 못했습니다.".to_owned())?;
        if read == 0 {
            return Err("요청이 끝나기 전에 연결이 닫혔습니다.".into());
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let method = first.next().unwrap_or_default().to_owned();
    let path = first.next().unwrap_or_default().split('?').next().unwrap_or_default().to_owned();
    // 헤더 값의 제어 문자는 upstream 요청에 줄을 끼워 넣을 수 있으므로 여기서 제거한다.
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().chars().filter(|c| !c.is_control()).collect()))
        .collect();
    let rest = buffer[end..].to_vec();
    Ok((Request { method, path, headers, body: Vec::new() }, rest))
}

/// 인증이 끝난 뒤 본문을 읽는다.
fn read_body(stream: &mut TcpStream, request: Request, mut body: Vec<u8>) -> Result<Request, String> {
    if request.header("transfer-encoding").is_some_and(|value| value.to_ascii_lowercase().contains("chunked")) {
        return Err("chunked 요청 본문은 지원하지 않습니다.".into());
    }
    let length: usize = request.header("content-length").and_then(|value| value.parse().ok()).unwrap_or(0);
    if length > MAX_BODY {
        return Err("요청 본문이 너무 큽니다.".into());
    }
    let mut chunk = [0u8; 16 * 1024];
    body.reserve(length.saturating_sub(body.len()));
    while body.len() < length {
        let read = stream.read(&mut chunk).map_err(|_| "요청 본문을 읽지 못했습니다.".to_owned())?;
        if read == 0 {
            return Err("요청 본문이 끝나기 전에 연결이 닫혔습니다.".into());
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(length);
    Ok(Request { body, ..request })
}

fn respond(stream: &mut TcpStream, status: u16, body: &Value) {
    let text = body.to_string();
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Service Unavailable",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
        text.len()
    );
    let _ = stream.shutdown(Shutdown::Both);
}

/// 후보 한 개가 지금 요청을 못 받는 이유. 선택(`choose`)이 후보를 거르는 조건과 같은 순서로 판정한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cause {
    /// 배정에서 뺀 계정. 쓸 수 없으므로 "한도를 다 썼다"는 판단에 넣지 않는다.
    Disabled,
    /// gateway가 꺼졌거나 최근 연결에 실패했다. 한도가 아니므로 재시도 시각을 단정하지 않는다.
    Down,
    /// `until`까지 기다려야 한다. `quota`: 사용량 한도(참) / 요청 속도 제한(거짓).
    /// `known`: 공급자가 알려 준 시각(참) / Ojak의 추정(거짓. 리셋 시각 없이 한도 응답을 받아 건 기본 대기 포함).
    Wait { until: i64, quota: bool, known: bool },
    /// 못 받을 이유가 없다(이 요청에서 이미 시도한 계정 등).
    Ready,
}

impl Cause {
    /// 로그의 원인 코드. 데스크톱은 읽지 않고 사람이 사고를 맞춰 볼 때 쓴다.
    fn code(self) -> &'static str {
        match self {
            Cause::Disabled => "disabled",
            Cause::Down => "down",
            Cause::Wait { quota: true, known: true, .. } => "quota",
            Cause::Wait { quota: true, known: false, .. } => "cooldown",
            Cause::Wait { quota: false, .. } => "rate",
            Cause::Ready => "ready",
        }
    }
}

/// 모든 후보가 기다려야 할 때 가장 먼저 풀리는 시각과 그 시각의 성격.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Recovery {
    pub at: i64,
    /// `at`이 공급자가 알려 준 리셋 시각이면 참, Ojak의 추정이면 거짓.
    pub known: bool,
    /// 기다리는 후보가 전부 사용량 한도 때문이면 참. 요청 속도 제한이 섞였으면 거짓.
    pub all_quota: bool,
}

/// 후보 한 개의 지금 상태. 관측한 한도 소진과 브릿지가 받은 한도 응답(`blocks`)을 함께 본다.
fn classify(candidate: &Candidate, scope: Option<&str>, model: &str, blocks: &[Block], now: i64) -> Cause {
    if !candidate.enabled {
        return Cause::Disabled;
    }
    if !candidate.healthy {
        return Cause::Down;
    }
    let observed = candidate
        .buckets
        .iter()
        .filter(|bucket| scheduler::applies(bucket, model) && exhausted(bucket, now))
        .map(|bucket| match bucket.resets_at {
            Some(reset) => (reset, true),
            None => (now + QUOTA_BLOCK_MS, false),
        })
        .map(|(until, known)| Cause::Wait { until, quota: true, known });
    let responded = blocks
        .iter()
        .filter(|block| block.key == candidate.key && block.until > now && (block.scope.is_none() || block.scope.as_deref() == scope))
        .map(|block| Cause::Wait { until: block.until, quota: block.quota, known: !block.estimate });
    // 후보는 모든 대기가 끝나야 쓸 수 있으므로 가장 늦은 것이 결정한다. 같은 시각이면 공급자가 알려 준 쪽을 쓴다.
    observed
        .chain(responded)
        .max_by_key(|cause| match cause {
            Cause::Wait { until, known, .. } => (*until, *known),
            _ => (i64::MIN, false),
        })
        .unwrap_or(Cause::Ready)
}

/// 후보마다 (로그용 계정 별칭, 원인). 별칭은 identity sha256 앞 12 hex라 이메일이 나오지 않는다.
pub(crate) fn classify_all(candidates: &[Candidate], provider: &str, model: &str, blocks: &[Block], now: i64) -> Vec<(String, Cause)> {
    let scope = block_scope(provider, model);
    candidates
        .iter()
        .map(|candidate| (account_key(identity_from_key(&candidate.key)), classify(candidate, scope.as_deref(), model, blocks, now)))
        .collect()
}

/// 쓸 수 있는 후보가 전부 기다리는 중이면 가장 먼저 풀리는 시각. 장애·시도 안 한 후보 등 한도가 아닌 이유로 막힌 후보가
/// 있으면 `None`이라 호출자가 "한도를 다 썼다"고 말하지 않는다. 배정에서 뺀 계정은 어차피 못 쓰므로 세지 않는다.
pub(crate) fn summarize(causes: &[(String, Cause)]) -> Option<Recovery> {
    let mut earliest: Option<(i64, bool)> = None;
    let mut all_quota = true;
    for (_, cause) in causes {
        match cause {
            Cause::Disabled => {}
            Cause::Down | Cause::Ready => return None,
            Cause::Wait { until, quota, known } => {
                all_quota &= quota;
                // 같은 시각이면 공급자가 알려 준 쪽을 앞세운다.
                if earliest.is_none_or(|(at, was_known)| (*until, !*known) < (at, !was_known)) {
                    earliest = Some((*until, *known));
                }
            }
        }
    }
    earliest.map(|(at, known)| Recovery { at, known, all_quota })
}

/// 모든 후보가 한도 때문에 막혔으면 가장 먼저 풀리는 시각과 그 성격. 요청 경로는 로그에 원인도 남기려고
/// `classify_all`과 `summarize`를 직접 부르고, 이 함수는 둘을 합친 형태로 테스트가 쓴다.
#[cfg(test)]
pub(crate) fn quota_reset(candidates: &[Candidate], provider: &str, model: &str, blocks: &[Block], now: i64) -> Option<Recovery> {
    summarize(&classify_all(candidates, provider, model, blocks, now))
}

/// 시각을 로그에 남길 때 쓰는 꼬리표. 공급자가 알려 준 시각인지 Ojak의 추정인지.
fn reset_kind(known: bool) -> &'static str {
    if known { "provider" } else { "estimate" }
}

/// 모든 후보가 못 받아 요청을 거절한 한 줄. 데스크톱 사용량 파서는 `->`가 없는 이 줄을 건너뛴다.
/// `<ms> <provider>/<model> session=<tail> outcome=rejected reset=<ms>:<provider|estimate>|- causes=<alias>:<code>[:<until>],…`
pub(crate) fn format_reject_line(at: i64, provider: &str, model: &str, session: &str, causes: &[(String, Cause)], reset: Option<&Recovery>) -> String {
    let reset = reset.map_or_else(|| "-".to_owned(), |recovery| format!("{}:{}", recovery.at, reset_kind(recovery.known)));
    let causes = causes
        .iter()
        .map(|(alias, cause)| match cause {
            Cause::Wait { until, .. } => format!("{alias}:{}:{until}", cause.code()),
            _ => format!("{alias}:{}", cause.code()),
        })
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{at} {provider}/{} session={} outcome=rejected reset={reset} causes={}",
        sanitize_log_model(model),
        sanitize_log_token(session),
        if causes.is_empty() { "-" } else { causes.as_str() }
    )
}

/// 한도 응답을 받아 계정을 막은 한 줄. 사용량 파서는 건너뛴다.
/// `<ms> <provider>/<model> session=<tail> outcome=blocked account=<alias> kind=<quota|rate> scope=<scope|-> until=<ms>:<provider|estimate>`
pub(crate) fn format_block_line(
    at: i64,
    provider: &str,
    model: &str,
    session: &str,
    alias: &str,
    quota: bool,
    scope: Option<&str>,
    until: i64,
    estimate: bool,
) -> String {
    format!(
        "{at} {provider}/{} session={} outcome=blocked account={alias} kind={} scope={} until={until}:{}",
        sanitize_log_model(model),
        sanitize_log_token(session),
        if quota { "quota" } else { "rate" },
        scope.map_or_else(|| "-".to_owned(), sanitize_log_token),
        reset_kind(!estimate)
    )
}

/// 지역 시각의 월·일·시·분. 메시지에 "몇 시부터"를 적을 때만 쓴다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Clock {
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

fn local_clock(ms: i64) -> Option<Clock> {
    let secs = ms.div_euclid(1000) as libc::time_t;
    // SAFETY: `tm`은 0으로 채운 순수 정수 구조체이고, 두 함수 모두 스레드 안전한 변환 함수(`localtime_r`/`localtime_s`)다.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    #[cfg(unix)]
    let ok = !unsafe { libc::localtime_r(&secs, &mut tm) }.is_null();
    #[cfg(windows)]
    let ok = unsafe { libc::localtime_s(&mut tm, &secs) } == 0;
    ok.then(|| Clock { month: (tm.tm_mon + 1) as u32, day: tm.tm_mday as u32, hour: tm.tm_hour as u32, minute: tm.tm_min as u32 })
}

/// 남은 시간을 사람이 읽는 말로. 정확히 아는 값이 아니므로 "약"을 붙인다.
fn wait_phrase(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 60_000 {
        format!("약 {}초", (ms + 999) / 1000)
    } else if ms < 120 * 60_000 {
        format!("약 {}분", (ms + 59_999) / 60_000)
    } else {
        format!("약 {}시간", (ms + 3_599_999) / 3_600_000)
    }
}

fn provider_label(provider: &str) -> &str {
    match provider {
        "anthropic" => "Claude",
        "openai-codex" => "Codex",
        "google-antigravity" => "Antigravity",
        "xai-oauth" => "Grok",
        "zai" => "Z.AI",
        other => other,
    }
}

/// omp가 429를 사용량 한도로 분류하려면 본문에 한도 문구가 있어야 한다(omp의 `usage.?limit` 판별).
/// 사용자가 읽는 한국어 문장 끝에 이 영어 꼬리표를 붙인다.
const OMP_LIMIT_TAG: &str = "(usage limit reached)";

/// 429 본문 메시지. 아는 것만 말한다: 공급자가 알려 준 시각이면 그 시각, Ojak의 추정이면 추정이라고 밝힌다.
/// `alternatives`는 지금 gateway가 떠 있는 다른 공급자의 Ojak 공급자 ID다(없는 공급자는 넣지 않는다).
pub(crate) fn quota_message(
    provider: &str,
    model: &str,
    recovery: &Recovery,
    now: i64,
    alternatives: &[&str],
    clock: impl Fn(i64) -> Option<Clock>,
) -> String {
    let label = provider_label(provider);
    let model = sanitize_log_model(model);
    let wait = wait_phrase(recovery.at - now);
    let hint = if alternatives.is_empty() {
        "지금은 /model로 다른 모델을 골라 보세요.".to_owned()
    } else {
        format!("지금은 /model로 다른 모델을 고르거나 {} 같은 다른 공급자를 써 보세요.", alternatives.join(", "))
    };
    if !recovery.all_quota {
        // 요청 속도 제한이 섞였으면 사용량 한도로 분류되지 않게 태그를 붙이지 않는다. omp가 짧은 Retry-After만큼 기다린다.
        return format!("Ojak: 쓸 수 있는 {label} 계정이 모두 한도나 요청 제한 때문에 잠시 쉬고 있어요. {wait} 뒤 다시 확인해 볼게요. {hint}");
    }
    if !recovery.known {
        return format!(
            "Ojak: 쓸 수 있는 {label} 계정이 모두 {model} 한도를 다 쓴 것 같아요. 리셋 시각을 몰라서 {wait} 뒤 다시 확인해 볼게요. {hint} {OMP_LIMIT_TAG}"
        );
    }
    let when = match (clock(recovery.at), clock(now)) {
        (Some(at), Some(today)) if (at.month, at.day) == (today.month, today.day) => format!("{:02}:{:02}부터", at.hour, at.minute),
        (Some(at), Some(_)) => format!("{}월 {}일 {:02}:{:02}부터", at.month, at.day, at.hour, at.minute),
        _ => format!("{wait} 뒤부터"),
    };
    format!("Ojak: 쓸 수 있는 {label} 계정이 모두 {model} 한도를 다 썼어요. {when} 다시 쓸 수 있어요. {hint} {OMP_LIMIT_TAG}")
}

/// omp가 사용량 한도로 분류하도록 429와 재시도 시각을 돌려준다. 503을 주면 일시 장애로 보고 재시도를 반복한다.
/// `Retry-After`는 아는 시각까지의 초다. 추정이어도 그 이상으로 늘리지 않는다.
fn respond_quota(stream: &mut TcpStream, provider: &str, model: &str, recovery: &Recovery, alternatives: &[&str], now: i64) {
    let seconds = ((recovery.at - now).max(0) + 999) / 1000;
    let body = json!({
        "error": {
            "type": "rate_limit_error",
            "message": quota_message(provider, model, recovery, now, alternatives, local_clock),
        }
    })
    .to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: {seconds}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.shutdown(Shutdown::Both);
}

fn forward(port: u16, token: &str, request: &Request, body: &[u8]) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2))?;
    // gateway가 멈춰도 이 연결의 스레드가 무기한 잠기지 않게 한다. 스트리밍 응답은 토큰 사이 간격 기준이다.
    stream.set_read_timeout(Some(UPSTREAM_READ_TIMEOUT))?;
    stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
    let mut head = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {token}\r\nContent-Length: {}\r\nConnection: close\r\n",
        request.method,
        request.path,
        body.len()
    );
    for (key, value) in &request.headers {
        let lower = key.to_ascii_lowercase();
        if lower == "content-type" || lower == "accept" || lower.starts_with("x-omp-") || lower == "user-agent" {
            head.push_str(&format!("{key}: {value}\r\n"));
        }
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    Ok(stream)
}

/// 응답 헤더와 첫 이벤트까지만 읽는다. 한도 응답이면 다른 계정으로 다시 보낼 수 있다.
fn peek_response(stream: &mut TcpStream) -> io::Result<(Vec<u8>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8 * 1024];
    let end = loop {
        if let Some(end) = head_end(&buffer) {
            break end;
        }
        if buffer.len() > MAX_HEAD {
            return Err(io::Error::other("upstream head too large"));
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            return Err(io::Error::other("upstream closed"));
        }
        buffer.extend_from_slice(&chunk[..read]);
    };
    let mut peek = buffer.split_off(end);
    while !peek.windows(2).any(|window| window == b"\n\n") && peek.len() < PEEK_LIMIT {
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        peek.extend_from_slice(&chunk[..read]);
    }
    Ok((buffer, peek))
}

/// 한도 응답 판정 결과. `quota`가 참이면 사용량 한도(모델 범위로 오래 막음), 거짓이면 일시적 속도 제한.
/// `retry_ms`는 응답이 알려 준 재시도 대기(ms).
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Limit {
    pub quota: bool,
    pub retry_ms: Option<i64>,
}

/// 재시도 대기가 이보다 길면 속도 제한이 아니라 주간·모델 한도 소진으로 본다.
const LONG_RETRY_MS: i64 = 60 * 60_000;

/// 응답 헤더 `Retry-After`(초)나 본문의 `retry-after-ms=`/`"retry-after-ms":` 표기에서 재시도 대기(ms)를 읽는다.
fn retry_hint(head: &str, text: &str) -> Option<i64> {
    let header = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        match name.trim().to_ascii_lowercase().as_str() {
            "retry-after-ms" => value.trim().parse::<i64>().ok(),
            "retry-after" => value.trim().parse::<i64>().ok().map(|seconds| seconds.saturating_mul(1000)),
            _ => None,
        }
    });
    let body = text.find("retry-after-ms").and_then(|index| {
        let digits: String = text[index + "retry-after-ms".len()..]
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(char::is_ascii_digit)
            .collect();
        digits.parse::<i64>().ok()
    });
    header.or(body).filter(|ms| *ms > 0)
}

/// 한도 응답이면 `Some`. 상태 코드가 429이거나, 오류 응답(스트림 오류 이벤트 포함)에 사용량·속도 제한 문구가 있을 때.
/// Anthropic은 주간·모델 한도 소진도 `rate_limit_error`로 보내고 긴 재시도 대기로만 구분되므로 대기 길이로 판별한다.
fn limit_signal(head: &[u8], peek: &[u8]) -> Option<Limit> {
    let head = String::from_utf8_lossy(head);
    let status: u16 = head.split(' ').nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
    let text = String::from_utf8_lossy(peek).to_ascii_lowercase();
    let usage = ["usage_limit", "usage limit", "quota", "limit has been reached", "hit your limit", "limit reached"]
        .iter()
        .any(|marker| text.contains(marker));
    let rate = text.contains("rate_limit") || text.contains("rate limit");
    let error = status >= 400 || text.contains("\"type\":\"error\"") || text.contains("event: error");
    if status != 429 && !(error && (usage || rate)) {
        return None;
    }
    let retry_ms = retry_hint(&head, &text);
    Some(Limit { quota: usage || retry_ms.is_some_and(|ms| ms > LONG_RETRY_MS), retry_ms })
}

fn http_get(port: u16, path: &str, token: &str) -> io::Result<Value> {
    let mut stream = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n")?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response)?;
    let end = head_end(&response).ok_or_else(|| io::Error::other("bad response"))?;
    let head = String::from_utf8_lossy(&response[..end]).to_ascii_lowercase();
    if !head.starts_with("http/1.1 200") {
        return Err(io::Error::other("non-200"));
    }
    let body = if head.contains("transfer-encoding: chunked") { dechunk(&response[end..]) } else { response[end..].to_vec() };
    serde_json::from_slice(&body).map_err(io::Error::other)
}
fn get_json(port: u16, path: &str, token: &str) -> io::Result<Value> {
    http_get(port, path, token)
}

/// broker에 Ojak 로그인만 올린다. 요청 본문과 토큰은 오류에 넣지 않는다. 기존 자격 증명은 지우지 않는다.
fn upload_ojak_logins(paths: &Paths, providers: &[&str]) -> Result<(), String> {
    if providers.is_empty() {
        return Ok(());
    }
    let token = read_secret(paths.bridge_token()).ok_or_else(|| "브릿지 토큰을 읽지 못해 Ojak 로그인을 맞추지 못했습니다.".to_owned())?;
    let broker = broker_token().ok_or_else(|| "OMP broker 토큰을 읽지 못해 Ojak 로그인을 맞추지 못했습니다.".to_owned())?;
    let expires = (now_ms().max(0) as u128).saturating_add(LOGIN_LIFETIME_MS) as u64;
    let mut failed = Vec::new();
    for provider in providers {
        let body = json!({
            "provider": provider,
            "credential": { "type": "oauth", "access": &token, "refresh": &token, "expires": expires }
        })
        .to_string();
        if broker_post("/v1/credential", &broker, &body).is_err() {
            failed.push(*provider);
        }
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("Ojak 로그인({})을 broker에 올리지 못했습니다.", failed.join(", ")))
    }
}

fn broker_post(path: &str, token: &str, body: &str) -> Result<(), ()> {
    let mut stream = TcpStream::connect_timeout(&([127, 0, 0, 1], BROKER_PORT).into(), Duration::from_secs(2)).map_err(|_| ())?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).map_err(|_| ())?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).map_err(|_| ())?;
    let end = head_end(&response).ok_or(())?;
    let head = String::from_utf8_lossy(&response[..end]);
    if head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200") { Ok(()) } else { Err(()) }
}


fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = body.windows(2).position(|window| window == b"\r\n") {
        let size = usize::from_str_radix(String::from_utf8_lossy(&body[..line_end]).split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        body = &body[line_end + 2..];
        if size == 0 || body.len() < size {
            break;
        }
        out.extend_from_slice(&body[..size]);
        body = body.get(size + 2..).unwrap_or_default();
    }
    out
}

fn constant_eq(left: &str, right: &str) -> bool {
    left.len() == right.len() && left.bytes().zip(right.bytes()).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

pub(crate) fn status_or_inactive(bridge: Option<&Arc<Bridge>>) -> Result<Status, ApiError> {
    Ok(bridge.map(|bridge| bridge.status()).unwrap_or_else(Status::inactive))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn bucket(id: &str, model: Option<&str>, used: f64, reset_in: i64, now: i64) -> QuotaBucket {
        QuotaBucket {
            id: id.into(),
            label: id.into(),
            model: model.map(str::to_owned),
            used_percent: Some(used),
            resets_at: Some(now + reset_in),
            observed_at: now,
            source: "test".into(),
            status: if used >= 100.0 { "exhausted".into() } else { "known".into() },
        }
    }
    fn candidate(key: &str, buckets: Vec<QuotaBucket>) -> Candidate {
        Candidate { key: key.into(), buckets, healthy: true, enabled: true, pinned: false, paid_fallback: None }
    }
    const DAY: i64 = 86_400_000;

    #[test]
    fn exhausted_fable_tier_skips_account_only_for_fable() {
        let now = 1_000_000_000_000;
        let fable_spent = candidate(
            "a",
            vec![bucket("5h", None, 10.0, DAY / 5, now), bucket("7d", None, 40.0, 2 * DAY, now), bucket("fable", Some("fable"), 100.0, 2 * DAY, now)],
        );
        let other = candidate("b", vec![bucket("5h", None, 10.0, DAY / 5, now), bucket("7d", None, 40.0, 5 * DAY, now)]);
        let pool = [fable_spent, other];
        let none = HashMap::new();
        assert_eq!(choose(&pool, "anthropic", "claude-fable-5-1", None, &none, &[], &[], 0.0, now), Some(1));
        // Opus는 Fable 전용 한도의 영향을 받지 않으며, 리셋이 더 가까운 계정을 먼저 쓴다.
        assert_eq!(choose(&pool, "anthropic", "claude-opus-5-5", None, &none, &[], &[], 0.0, now), Some(0));
    }

    #[test]
    fn paid_fallback_is_last_resort_and_never_sticks_after_recovery() {
        let now = 1_000_000_000_000;
        let fresh = 900_000;
        let spent = |key: &str, paid: Option<i64>| Candidate { paid_fallback: paid, ..candidate(key, vec![bucket("7d", None, 100.0, DAY, now)]) };
        let roomy = candidate("roomy", vec![bucket("7d", None, 40.0, 5 * DAY, now)]);
        let none = HashMap::new();
        // 옵트인이 없으면(paid_fallback 없음) 소진 계정은 그대로 제외된다.
        assert_eq!(choose(&[spent("paid", None)], "openai-codex", "gpt-6-astra", None, &none, &[], &[], 0.0, now), None);
        // 구독 한도가 남은 계정이 있으면 크레딧·추가 사용량 계정은 고르지 않는다.
        let pool = [spent("paid", Some(fresh)), roomy.clone()];
        assert_eq!(choose(&pool, "openai-codex", "gpt-6-astra", None, &none, &[], &[], 0.0, now), Some(1));
        // 구독 계정이 하나도 없으면 마지막 수단으로 고른다.
        assert_eq!(choose(&[spent("paid", Some(fresh))], "openai-codex", "gpt-6-astra", None, &none, &[], &[], 0.0, now), Some(0));
        // 그 계정에 세션이 붙은 뒤 구독 한도가 회복되면 다음 요청부터 구독 계정으로 옮긴다.
        let mut stickies = HashMap::new();
        stickies.insert("s".to_owned(), Sticky { key: "paid".into(), model: "gpt-6-astra".into(), cwd: None, last_used: now, requests: 3 });
        assert_eq!(choose(&[spent("paid", Some(fresh))], "openai-codex", "gpt-6-astra", Some("s"), &stickies, &[], &[], 0.0, now), Some(0));
        assert_eq!(choose(&pool, "openai-codex", "gpt-6-astra", Some("s"), &stickies, &[], &[], 0.0, now), Some(1));
        // 같은 계정의 한도가 리셋되면(새 관측) 크레딧 계정이어도 구독 계정으로 돌아온다.
        let recovered = Candidate { paid_fallback: Some(fresh), ..candidate("paid", vec![bucket("7d", None, 20.0, 5 * DAY, now)]) };
        assert_eq!(choose(&[recovered], "openai-codex", "gpt-6-astra", Some("s"), &stickies, &[], &[], 0.0, now), Some(0));
        // 소진 관측이 낡았거나(관측 시각이 오래됨) 리셋 시각이 지났으면 크레딧 fallback을 고르지 않는다.
        let mut old = bucket("7d", None, 100.0, DAY, now - 2 * fresh);
        old.observed_at = now - 2 * fresh;
        let stale = Candidate { paid_fallback: Some(fresh), ..candidate("paid", vec![old]) };
        assert_eq!(choose(&[stale], "openai-codex", "gpt-6-astra", None, &none, &[], &[], 0.0, now), None);
        // 한도가 남았지만 일시 차단된 구독 계정이 있으면 기다리지 크레딧으로 넘어가지 않는다.
        let blocks = [Block { key: "roomy".into(), scope: None, until: now + 60_000, reason: "test".into(), quota: false, estimate: false }];
        assert_eq!(choose(&pool, "openai-codex", "gpt-6-astra", None, &none, &blocks, &[], 0.0, now), None);
        // 모델 전용 한도 소진은 공용 크레딧이 대신 받지 않는다.
        let model_only = Candidate { paid_fallback: Some(fresh), ..candidate("paid", vec![bucket("7d", None, 20.0, 5 * DAY, now), bucket("spark", Some("fable"), 100.0, DAY, now)]) };
        assert_eq!(choose(&[model_only], "anthropic", "claude-fable-5-1", None, &none, &[], &[], 0.0, now), None);
    }

    #[test]
    fn disabled_accounts_are_never_billed_through_paid_fallback() {
        let now = 1_000_000_000_000;
        let fresh = 900_000;
        let none = HashMap::new();
        let spent = |key: &str, enabled: bool| Candidate {
            enabled,
            paid_fallback: Some(fresh),
            ..candidate(key, vec![bucket("7d", None, 100.0, DAY, now)])
        };
        // 사용자가 제외한 계정 하나뿐이면 크레딧이 있어도 고르지 않는다.
        assert_eq!(choose(&[spent("off", false)], "openai-codex", "gpt-6-astra", None, &none, &[], &[], 0.0, now), None);
        // 제외한 계정과 쓸 수 있는 소진 계정이 섞여 있으면 활성 계정만 고른다(순서와 무관하게).
        let pool = [spent("off", false), spent("on", true)];
        assert_eq!(choose(&pool, "openai-codex", "gpt-6-astra", None, &none, &[], &[], 0.0, now), Some(1));
        let reversed = [spent("on", true), spent("off", false)];
        assert_eq!(choose(&reversed, "openai-codex", "gpt-6-astra", None, &none, &[], &[], 0.0, now), Some(0));
        // 제외한 계정에 세션이 붙어 있어도 그 계정으로 계속 보내지 않는다.
        let mut stickies = HashMap::new();
        stickies.insert("s".to_owned(), Sticky { key: "off".into(), model: "gpt-6-astra".into(), cwd: None, last_used: now, requests: 2 });
        assert_eq!(choose(&pool, "openai-codex", "gpt-6-astra", Some("s"), &stickies, &[], &[], 0.0, now), Some(1));
        assert_eq!(choose(&[spent("off", false)], "openai-codex", "gpt-6-astra", Some("s"), &stickies, &[], &[], 0.0, now), None);
        // 추가 사용량(Claude)도 같다.
        let claude_off = Candidate { enabled: false, paid_fallback: Some(fresh), ..candidate("off", vec![bucket("7d", None, 100.0, DAY, now)]) };
        assert_eq!(choose(&[claude_off], "anthropic", "claude-opus-5-5", None, &none, &[], &[], 0.0, now), None);
    }

    #[test]
    fn session_stays_on_its_account_until_that_account_is_unusable() {
        let now = 1_000_000_000_000;
        let pool = [
            candidate("a", vec![bucket("7d", None, 90.0, 5 * DAY, now)]),
            candidate("b", vec![bucket("7d", None, 10.0, DAY, now)]),
        ];
        let mut stickies = HashMap::new();
        stickies.insert("s1".into(), Sticky { key: "a".into(), model: "claude-opus-5-5".into(), cwd: None, last_used: now - 60_000, requests: 3 });
        assert_eq!(choose(&pool, "anthropic", "claude-opus-5-5", Some("s1"), &stickies, &[], &[], 0.0, now), Some(0));
        let blocks = [Block { key: "a".into(), scope: None, until: now + 60_000, reason: "test".into(), quota: false, estimate: false }];
        assert_eq!(choose(&pool, "anthropic", "claude-opus-5-5", Some("s1"), &stickies, &blocks, &[], 0.0, now), Some(1));
        // 고정이 오래되면 다시 순위를 매긴다.
        stickies.get_mut("s1").unwrap().last_used = now - STICKY_MS - 1;
        assert_eq!(choose(&pool, "anthropic", "claude-opus-5-5", Some("s1"), &stickies, &[], &[], 0.0, now), Some(1));
    }

    #[test]
    fn scoped_block_does_not_leak_to_other_models_but_account_block_does() {
        let now = 1_000_000_000_000;
        let pool = [candidate("a", vec![]), candidate("b", vec![])];
        let fable_block = [Block { key: "a".into(), scope: Some("fable".into()), until: now + 60_000, reason: "q".into(), quota: true, estimate: false }];
        let none = HashMap::new();
        assert_eq!(choose(&pool, "anthropic", "claude-fable-5-1", None, &none, &fable_block, &[], 0.0, now), Some(1));
        assert_eq!(choose(&pool, "anthropic", "claude-opus-5-5", None, &none, &fable_block, &[], 0.0, now), Some(0));
        // Codex는 chat 한도와 spark 한도가 따로다.
        let chat_block = [Block { key: "a".into(), scope: block_scope("openai-codex", "gpt-6-astra"), until: now + 60_000, reason: "q".into(), quota: true, estimate: false }];
        assert_eq!(choose(&pool, "openai-codex", "gpt-6-astra", None, &none, &chat_block, &[], 0.0, now), Some(1));
        // 공급자 수동 배정: 새 대화는 고정 계정이 받을 수 있으면 그 계정. 소진·차단이면 다른 계정으로 넘어간다.
        let mut pinned_pool = vec![candidate("a", vec![]), candidate("b", vec![])];
        pinned_pool[1].pinned = true;
        assert_eq!(choose(&pinned_pool, "anthropic", "claude-opus-5-5", None, &none, &[], &[], 0.0, now), Some(1));
        let pinned_block = [Block { key: "b".into(), scope: None, until: now + 60_000, reason: "q".into(), quota: false, estimate: false }];
        assert_eq!(choose(&pinned_pool, "anthropic", "claude-opus-5-5", None, &none, &pinned_block, &[], 0.0, now), Some(0));
        // 진행 중인 대화(sticky)는 고정을 바꿔도 원래 계정을 유지한다.
        let mut kept = HashMap::new();
        kept.insert("s1".to_owned(), Sticky { key: "a".into(), model: String::new(), cwd: None, last_used: now, requests: 1 });
        assert_eq!(choose(&pinned_pool, "anthropic", "claude-opus-5-5", Some("s1"), &kept, &[], &[], 0.0, now), Some(0));
        assert_eq!(choose(&pool, "openai-codex", "gpt-5.3-codex-spark", None, &none, &chat_block, &[], 0.0, now), Some(0));
    }

    #[test]
    fn concurrent_sessions_spread_and_hot_five_hour_account_goes_last() {
        let now = 1_000_000_000_000;
        let pool = [
            candidate("a", vec![bucket("5h", None, 20.0, DAY / 5, now), bucket("7d", None, 30.0, DAY, now)]),
            candidate("b", vec![bucket("5h", None, 20.0, DAY / 5, now), bucket("7d", None, 30.0, 3 * DAY, now)]),
        ];
        let mut stickies = HashMap::new();
        stickies.insert("other".into(), Sticky { key: "a".into(), model: "m".into(), cwd: None, last_used: now, requests: 1 });
        // a가 소진 급한 계정이지만 이미 다른 세션이 쓰고 있어 새 세션은 b로 간다.
        assert_eq!(choose(&pool, "anthropic", "claude-opus-5-5", Some("new"), &stickies, &[], &[], 0.0, now), Some(1));
        let hot = [
            candidate("a", vec![bucket("5h", None, 90.0, DAY / 5, now), bucket("7d", None, 30.0, DAY, now)]),
            candidate("b", vec![bucket("5h", None, 20.0, DAY / 5, now), bucket("7d", None, 30.0, 3 * DAY, now)]),
        ];
        assert_eq!(choose(&hot, "anthropic", "claude-opus-5-5", None, &HashMap::new(), &[], &[], 0.0, now), Some(1));
    }

    fn block(key: &str, scope: Option<String>, until: i64, quota: bool, estimate: bool) -> Block {
        Block { key: key.into(), scope, until, reason: "test".into(), quota, estimate }
    }

    #[test]
    fn quota_reset_reports_earliest_recovery_only_when_every_account_is_quota_blocked() {
        let now = 1_000_000_000_000;
        let spent = |key: &str, reset_in: i64| candidate(key, vec![bucket("7d", None, 100.0, reset_in, now)]);
        let pool = [spent("a", 3 * DAY), spent("b", DAY)];
        // 둘 다 관측한 공급자 리셋이 있으면 가장 빠른 시각이고, 공급자가 알려 준 시각이다.
        assert_eq!(
            quota_reset(&pool, "openai-codex", "gpt-6-astra", &[], now),
            Some(Recovery { at: now + DAY, known: true, all_quota: true })
        );
        // 한 계정은 한도가 아니라 장애로 빠졌다면 재시도 시각을 단정하지 않는다.
        let mixed = [spent("a", 3 * DAY), candidate("b", vec![bucket("7d", None, 10.0, DAY, now)])];
        assert_eq!(quota_reset(&mixed, "openai-codex", "gpt-6-astra", &[], now), None);
        // 브릿지가 직접 본 한도 응답도 막힘 근거가 된다.
        let scope = block_scope("openai-codex", "gpt-6-astra");
        let blocks = [block("b", scope, now + 60_000, true, false)];
        assert_eq!(
            quota_reset(&mixed, "openai-codex", "gpt-6-astra", &blocks, now),
            Some(Recovery { at: now + 60_000, known: true, all_quota: true })
        );
    }

    #[test]
    fn unavailable_causes_are_classified_with_known_or_estimated_times() {
        let now = 1_000_000_000_000;
        let model = "claude-opus-5-5";
        let scope = block_scope("anthropic", model);
        let classify_one = |candidate: &Candidate, blocks: &[Block]| classify(candidate, scope.as_deref(), model, blocks, now);
        // 관측한 소진 + 공급자 리셋 시각.
        let spent = candidate("a", vec![bucket("7d", None, 100.0, DAY, now)]);
        assert_eq!(classify_one(&spent, &[]), Cause::Wait { until: now + DAY, quota: true, known: true });
        // 소진됐다고만 알고 리셋 시각이 없으면 Ojak의 30분 대기를 쓰는 추정이다.
        let mut unknown_reset = bucket("7d", None, 100.0, DAY, now);
        unknown_reset.resets_at = None;
        assert_eq!(
            classify_one(&candidate("a", vec![unknown_reset]), &[]),
            Cause::Wait { until: now + QUOTA_BLOCK_MS, quota: true, known: false }
        );
        // 한도 응답을 받았지만 리셋 시각을 모른 채 건 대기(2026-10-07 사고의 1618초)도 추정이다.
        let cooldown = block("a", scope.clone(), now + 1_618_000, true, true);
        let cause = classify_one(&candidate("a", vec![]), std::slice::from_ref(&cooldown));
        assert_eq!(cause, Cause::Wait { until: now + 1_618_000, quota: true, known: false });
        assert_eq!(cause.code(), "cooldown");
        // 속도 제한 차단은 한도가 아니다. 계정 전체를 막고 모델 범위 밖에도 적용된다.
        let rate = block("a", None, now + 20_000, false, false);
        let cause = classify_one(&candidate("a", vec![]), std::slice::from_ref(&rate));
        assert_eq!(cause, Cause::Wait { until: now + 20_000, quota: false, known: true });
        assert_eq!(cause.code(), "rate");
        // 다른 계정·다른 모델 범위·이미 끝난 차단은 영향이 없다.
        let other = [block("b", None, now + 60_000, false, false), block("a", Some("fable".into()), now + 60_000, true, false), block("a", None, now - 1, true, true)];
        assert_eq!(classify_one(&candidate("a", vec![]), &other), Cause::Ready);
        // 관측 소진과 차단이 겹치면 더 늦게 풀리는 쪽이 결정한다.
        let long_block = block("a", scope.clone(), now + 2 * DAY, true, false);
        assert_eq!(classify_one(&spent, std::slice::from_ref(&long_block)), Cause::Wait { until: now + 2 * DAY, quota: true, known: true });
        // 한도가 아닌 이유.
        assert_eq!(classify_one(&Candidate { healthy: false, ..candidate("a", vec![]) }, &[]), Cause::Down);
        assert_eq!(classify_one(&Candidate { enabled: false, ..spent.clone() }, &[]), Cause::Disabled);
        assert_eq!(Cause::Down.code(), "down");
        assert_eq!(Cause::Disabled.code(), "disabled");
    }

    #[test]
    fn summary_claims_quota_exhaustion_only_when_every_usable_account_is_waiting() {
        let now = 1_000_000_000_000;
        let wait = |until: i64, quota: bool, known: bool| Cause::Wait { until, quota, known };
        let causes = |items: &[Cause]| items.iter().enumerate().map(|(index, cause)| (format!("k{index}"), *cause)).collect::<Vec<_>>();
        // 가장 빠른 시각과 그 성격. 같은 시각이면 공급자가 알려 준 쪽을 앞세운다.
        let two = causes(&[wait(now + 3 * DAY, true, true), wait(now + 1_618_000, true, false)]);
        assert_eq!(summarize(&two), Some(Recovery { at: now + 1_618_000, known: false, all_quota: true }));
        let tie = causes(&[wait(now + 1000, true, false), wait(now + 1000, true, true)]);
        assert_eq!(summarize(&tie).map(|recovery| recovery.known), Some(true));
        // 속도 제한이 섞이면 사용량 한도라고 단정하지 않는다.
        let mixed_rate = causes(&[wait(now + 60_000, false, false), wait(now + DAY, true, true)]);
        assert_eq!(summarize(&mixed_rate), Some(Recovery { at: now + 60_000, known: false, all_quota: false }));
        // 장애·미시도 후보가 있으면 시각을 말하지 않는다.
        assert_eq!(summarize(&causes(&[wait(now + DAY, true, true), Cause::Down])), None);
        assert_eq!(summarize(&causes(&[wait(now + DAY, true, true), Cause::Ready])), None);
        // 배정에서 뺀 계정은 판단에서 빠진다. 전부 뺐으면 말할 시각이 없다.
        assert_eq!(summarize(&causes(&[Cause::Disabled, wait(now + DAY, true, true)])).map(|recovery| recovery.at), Some(now + DAY));
        assert_eq!(summarize(&causes(&[Cause::Disabled])), None);
        assert_eq!(summarize(&[]), None);
        // 장애 하나가 섞인 실제 풀: 한도 소진 + gateway 중단.
        let pool = [
            candidate("a", vec![bucket("7d", None, 100.0, DAY, now)]),
            Candidate { healthy: false, ..candidate("b", vec![]) },
        ];
        assert_eq!(quota_reset(&pool, "anthropic", "claude-opus-5-5", &[], now), None);
    }

    #[test]
    fn block_until_marks_only_the_default_cooldown_as_an_estimate() {
        let now = 1_000_000_000_000;
        let model = "claude-opus-5-5";
        let none: Vec<QuotaBucket> = Vec::new();
        // 리셋도 대기 힌트도 없으면 Ojak의 기본 30분/60초이고 추정이다.
        assert_eq!(block_until(&none, model, true, None, now), (now + QUOTA_BLOCK_MS, true));
        assert_eq!(block_until(&none, model, false, None, now), (now + RATE_BLOCK_MS, true));
        // 기본값보다 짧은 힌트는 기본값을 못 줄인다. 그래도 공급자가 정한 시각이 아니다.
        assert_eq!(block_until(&none, model, true, Some(now + 5 * 60_000), now), (now + QUOTA_BLOCK_MS, true));
        assert_eq!(block_until(&none, model, false, Some(now + 20_000), now), (now + RATE_BLOCK_MS, true));
        // 기본값보다 긴 힌트는 공급자가 알려 준 시각이다.
        assert_eq!(block_until(&none, model, true, Some(now + DAY), now), (now + DAY, false));
        // 관측한 리셋 시각이 있으면 그것이 기준이고, 더 늦은 힌트가 이긴다.
        let spent = [bucket("7d", None, 100.0, 3 * DAY, now)];
        assert_eq!(block_until(&spent, model, true, None, now), (now + 3 * DAY, false));
        assert_eq!(block_until(&spent, model, true, Some(now + 5 * DAY), now), (now + 5 * DAY, false));
        assert_eq!(block_until(&spent, model, true, Some(now + DAY), now), (now + 3 * DAY, false));
    }

    fn clock_for(now: i64, reset: Clock, today: Clock) -> impl Fn(i64) -> Option<Clock> {
        move |ms| Some(if ms == now { today } else { reset })
    }

    #[test]
    fn quota_message_is_honest_actionable_and_never_names_aam() {
        let now = 1_000_000_000_000;
        let today = Clock { month: 10, day: 7, hour: 3, minute: 53 };
        let same_day = Clock { month: 10, day: 7, hour: 14, minute: 5 };
        let later = Clock { month: 10, day: 9, hour: 9, minute: 0 };
        let recovery = |at: i64, known: bool, all_quota: bool| Recovery { at, known, all_quota };
        let all = ["ojak-codex", "ojak-antigravity"];
        // 공급자가 알려 준 리셋: 시각과 다음 행동.
        let message = quota_message("anthropic", "claude-opus-5-5", &recovery(now + 3_600_000, true, true), now, &all, clock_for(now, same_day, today));
        assert!(message.starts_with("Ojak:"), "{message}");
        assert!(message.contains("Claude 계정이 모두 claude-opus-5-5 한도를 다 썼어요"), "{message}");
        assert!(message.contains("14:05부터 다시 쓸 수 있어요"), "{message}");
        assert!(message.contains("/model") && message.contains("ojak-codex, ojak-antigravity"), "{message}");
        assert!(!message.contains("AAM") && !message.contains("브릿지"), "{message}");
        // omp가 사용량 한도로 분류하는 문구(`usage.?limit`)를 남긴다.
        assert!(message.contains("usage limit"), "{message}");
        assert!(!message.contains("약 "), "{message}");
        // 날짜가 다르면 월·일을 붙인다.
        let message = quota_message("anthropic", "claude-opus-5-5", &recovery(now + 2 * DAY, true, true), now, &[], clock_for(now, later, today));
        assert!(message.contains("10월 9일 09:00부터"), "{message}");
        // 로컬 시각을 못 구하면 남은 시간으로 말한다.
        let message = quota_message("openai-codex", "gpt-6-astra", &recovery(now + 2 * 3_600_000, true, true), now, &[], |_| None);
        assert!(message.contains("Codex 계정이") && message.contains("약 2시간 뒤부터 다시 쓸 수 있어요"), "{message}");
        // Ojak의 추정: 공급자 리셋이라고 말하지 않고 다시 확인한다고 밝힌다(2026-10-07 사고의 1618초).
        let message = quota_message("anthropic", "claude-opus-5-5", &recovery(now + 1_618_000, false, true), now, &all, clock_for(now, same_day, today));
        assert!(message.contains("약 27분 뒤 다시 확인해 볼게요"), "{message}");
        assert!(message.contains("리셋 시각을 몰라서"), "{message}");
        assert!(!message.contains("부터 다시 쓸 수 있어요") && !message.contains("AAM"), "{message}");
        assert!(message.contains("/model"), "{message}");
        assert!(message.contains("usage limit"), "{message}");
        // 속도 제한이 섞이면 한도를 다 썼다고 말하지 않고 사용량 한도 문구도 붙이지 않는다.
        let message = quota_message("anthropic", "claude-opus-5-5", &recovery(now + 20_000, true, false), now, &all, clock_for(now, same_day, today));
        assert!(message.contains("약 20초 뒤 다시 확인해 볼게요"), "{message}");
        assert!(!message.contains("한도를 다 썼") && !message.contains("usage limit"), "{message}");
        // 대안 공급자가 없으면 이름을 대지 않는다.
        let message = quota_message("anthropic", "claude-opus-5-5", &recovery(now + 60_000, false, true), now, &[], clock_for(now, same_day, today));
        assert!(message.contains("/model로 다른 모델을 골라 보세요") && !message.contains("ojak-"), "{message}");
    }

    #[test]
    fn reject_and_block_log_lines_are_single_lines_without_email() {
        let now = 1_700_000_000_000;
        let model = "claude-opus-5-5";
        let email_key = "anthropic|email:person@example.com|org:org-1";
        let other_key = "anthropic|email:second@example.com|org:org-2";
        let pool = [
            candidate(email_key, vec![bucket("7d", None, 100.0, DAY, now)]),
            candidate(other_key, vec![]),
        ];
        let blocks = [block(other_key, None, now + 1_618_000, true, true)];
        let causes = classify_all(&pool, "anthropic", model, &blocks, now);
        let reset = summarize(&causes);
        let line = format_reject_line(now, "anthropic", model, "abcd\n1234", &causes, reset.as_ref());
        let first = account_key(identity_from_key(email_key));
        let second = account_key(identity_from_key(other_key));
        assert!(line.starts_with(&format!("{now} anthropic/{model} session=abcd1234 outcome=rejected ")), "{line}");
        assert!(line.contains(&format!("reset={}:estimate", now + 1_618_000)), "{line}");
        assert!(line.contains(&format!("{first}:quota:{}", now + DAY)) && line.contains(&format!("{second}:cooldown:{}", now + 1_618_000)), "{line}");
        for secret in ["@", "email:", "org:", "person", "example.com", "\n"] {
            assert!(!line.contains(secret), "{secret}: {line}");
        }
        // 한도가 아닌 이유가 섞이면 시각 칸은 비고 원인만 남는다.
        let down = [candidate(email_key, vec![bucket("7d", None, 100.0, DAY, now)]), Candidate { healthy: false, ..candidate(other_key, vec![]) }];
        let causes = classify_all(&down, "anthropic", model, &[], now);
        let line = format_reject_line(now, "anthropic", model, "-", &causes, summarize(&causes).as_ref());
        assert!(line.contains(" reset=- ") && line.contains(&format!("{second}:down")) && !line.contains('@'), "{line}");
        // 차단 한 줄.
        let line = format_block_line(now, "anthropic", "claude-fable-5-1", "abcd1234", &first, true, Some("fable"), now + 1_618_000, true);
        assert_eq!(
            line,
            format!("{now} anthropic/claude-fable-5-1 session=abcd1234 outcome=blocked account={first} kind=quota scope=fable until={}:estimate", now + 1_618_000)
        );
        let line = format_block_line(now, "openai-codex", "gpt-6-astra", "-", &first, false, None, now + 60_000, false);
        assert!(line.contains("kind=rate scope=- until=") && line.ends_with(":provider"), "{line}");
        // 사용량 파서는 이 줄들을 요청으로 세지 않는다.
        assert!(parse_usage_line(&line).is_none());
        let rejected = format_reject_line(now, "anthropic", model, "-", &classify_all(&pool, "anthropic", model, &blocks, now), reset.as_ref());
        assert!(parse_usage_line(&rejected).is_none());
    }

    #[test]
    fn local_clock_returns_a_valid_wall_time() {
        let clock = local_clock(1_700_000_000_000).unwrap();
        assert!((1..=12).contains(&clock.month) && (1..=31).contains(&clock.day) && clock.hour < 24 && clock.minute < 60);
    }

    #[test]
    fn auxiliary_requests_route_by_model_id_in_the_same_session() {
        // 2026-09-26 omp 18.3.2 캡처: 대화 요청과 judge 요청의 실제 형태.
        let turn = json!({"modelId": "claude-opus-5-5", "options": {"model": {"id": "claude-opus-5-5", "provider": "anthropic"}, "sessionId": "s-1"}});
        let judge = json!({"modelId": "anthropic/claude-opus-5", "options": {"sessionId": "s-1", "maxTokens": 64}});
        let target = |provider: &str, model: &str, turn: bool| Target { provider: provider.into(), model: model.into(), session: Some("s-1".into()), turn };
        assert_eq!(request_target(&turn), Some(target("anthropic", "claude-opus-5-5", true)));
        // 보조 요청은 같은 세션의 고정을 따르지만 세션을 새로 만들거나 활성 세션으로 세지 않는다.
        assert_eq!(request_target(&judge), Some(target("anthropic", "claude-opus-5", false)));
        // 브릿지가 맡지 않는 공급자는 받지 않는다.
        assert_eq!(request_target(&json!({"modelId": "openrouter/some-model", "options": {}})), None);
    }

    #[test]
    fn ojak_login_providers_route_to_their_original_provider_models() {
        let body = json!({"modelId": "claude-fable-5-1", "options": {"model": {"id": "claude-fable-5-1", "provider": "ojak-claude", "baseUrl": "http://127.0.0.1:4020"}, "sessionId": "s-2"}});
        let target = request_target(&body).unwrap();
        assert_eq!((target.provider.as_str(), target.model.as_str(), target.turn), ("anthropic", "claude-fable-5-1", true));
        // gateway는 원래 공급자 모델로 찾는다. Ojak 이름이 그대로 가면 gateway가 모델을 찾지 못한다.
        let sent: Value = serde_json::from_slice(&for_gateway(body, &target.provider, &target.model)).unwrap();
        assert_eq!(sent["modelId"], "anthropic/claude-fable-5-1");
        assert_eq!(sent["options"]["model"]["provider"], "anthropic");
        assert_eq!(sent["options"]["sessionId"], "s-2");
        let codex = json!({"modelId": "gpt-6-astra", "options": {"model": {"id": "gpt-6-astra", "provider": "ojak-codex"}}});
        assert_eq!(request_target(&codex).unwrap().provider, "openai-codex");
        // 이름을 바꾸기 전에 시작한 omp 세션의 요청도 같은 공급자로 보낸다.
        let legacy = json!({"modelId": "claude-opus-5-5", "options": {"model": {"id": "claude-opus-5-5", "provider": "aam-claude"}}});
        assert_eq!(request_target(&legacy).unwrap().provider, "anthropic");
    }

    #[test]
    fn reserve_keeps_nearly_full_model_limits_for_last_resort_and_disabled_accounts_out() {
        let now = 1_000_000_000_000;
        // 실측 상황: Fable 91%. 여유량 10%면 Fable 새 세션은 다른 계정으로, Opus는 그대로 받는다.
        let tight = candidate("tight", vec![bucket("7d", None, 78.0, DAY, now), bucket("fable", Some("fable"), 91.0, DAY, now)]);
        let roomy = candidate("roomy", vec![bucket("7d", None, 40.0, 5 * DAY, now), bucket("fable", Some("fable"), 60.0, 5 * DAY, now)]);
        let pool = [tight.clone(), roomy.clone()];
        let none = HashMap::new();
        assert_eq!(choose(&pool, "anthropic", "claude-fable-5-1", None, &none, &[], &[], 10.0, now), Some(1));
        assert_eq!(choose(&pool, "anthropic", "claude-opus-5-5", None, &none, &[], &[], 10.0, now), Some(0));
        // 여유 있는 계정이 없으면 여유량 안쪽 계정이라도 쓴다.
        assert_eq!(choose(&[tight.clone()], "anthropic", "claude-fable-5-1", None, &none, &[], &[], 10.0, now), Some(0));
        // 진행 중인 대화는 캐시를 지키려고 여유량 안쪽이어도 같은 계정에 둔다.
        let mut stickies = HashMap::new();
        stickies.insert("s".into(), Sticky { key: "tight".into(), model: "claude-fable-5-1".into(), cwd: None, last_used: now, requests: 3 });
        assert_eq!(choose(&pool, "anthropic", "claude-fable-5-1", Some("s"), &stickies, &[], &[], 10.0, now), Some(0));
        // 고정 계정의 해당 모델 한도가 소진되면 그때 옮긴다.
        let spent = candidate("tight", vec![bucket("7d", None, 78.0, DAY, now), bucket("fable", Some("fable"), 100.0, DAY, now)]);
        assert_eq!(choose(&[spent, roomy.clone()], "anthropic", "claude-fable-5-1", Some("s"), &stickies, &[], &[], 10.0, now), Some(1));
        // 고정이 끝난 대화는 새 대화처럼 여유 있는 계정을 고른다.
        stickies.get_mut("s").unwrap().last_used = now - STICKY_MS - 1;
        assert_eq!(choose(&pool, "anthropic", "claude-fable-5-1", Some("s"), &stickies, &[], &[], 10.0, now), Some(1));
        stickies.get_mut("s").unwrap().last_used = now;
        // 여유 있는 계정이 없으면 고정을 유지한다.
        assert_eq!(choose(&[tight.clone()], "anthropic", "claude-fable-5-1", Some("s"), &stickies, &[], &[], 10.0, now), Some(0));
        // 배정에서 뺀 계정은 omp에서도 쓰지 않는다.
        let off = Candidate { enabled: false, ..roomy };
        assert_eq!(choose(&[off], "anthropic", "claude-opus-5-5", None, &none, &[], &[], 10.0, now), None);
    }

    #[test]
    fn limit_detection_distinguishes_quota_from_rate_limit_and_success() {
        assert_eq!(limit_signal(b"HTTP/1.1 429 Too Many Requests\r\n\r\n", b"{\"error\":\"slow down\"}"), Some(Limit { quota: false, retry_ms: None }));
        // 2026-09-27 실측: Fable 주간 소진이 200 스트림의 오류 이벤트 + rate_limit_error + 5일 대기로 왔다.
        let fable = b"event: error\ndata: {\"message\":\"429 {\\\"type\\\":\\\"error\\\",\\\"error\\\":{\\\"type\\\":\\\"rate_limit_error\\\",\\\"message\\\":\\\"This request would exceed your account's rate limit.\\\"}} retry-after-ms=461826000\"}\n\n";
        assert_eq!(limit_signal(b"HTTP/1.1 200 OK\r\n\r\n", fable), Some(Limit { quota: true, retry_ms: Some(461_826_000) }));
        // 짧은 대기의 속도 제한은 잠깐만 막는다.
        assert_eq!(limit_signal(b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 20\r\n\r\n", b"{}"), Some(Limit { quota: false, retry_ms: Some(20_000) }));
        // 정상 본문에 "rate limit"이라는 단어가 있어도 오류가 아니면 한도가 아니다.
        assert_eq!(limit_signal(b"HTTP/1.1 200 OK\r\n\r\n", b"event: text\ndata: {\"delta\":\"about rate limit design\"}\n\n"), None);
        assert_eq!(
            limit_signal(b"HTTP/1.1 200 OK\r\n\r\n", b"event: error\ndata: {\"message\":\"The usage limit has been reached\"}\n\n"),
            Some(Limit { quota: true, retry_ms: None })
        );
        assert_eq!(limit_signal(b"HTTP/1.1 200 OK\r\n\r\n", b"event: start\ndata: {}\n\n"), None);
    }

    #[test]
    fn usage_line_round_trip_omits_email_and_bounds_model() {
        let identity = "email:person@example.com|org:org-1";
        let key = account_key(identity);
        assert_eq!(key.len(), 12);
        assert!(key.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(account_key("email:other@example.com|org:org-1"), key);
        let model = "claude-opus\n5 injected -> anthropic|email:evil@x.io|org:z";
        let line = format_usage_line(1_700_000_000_000, "anthropic", model, "abcd\n1234", true, 3, &key, true);
        assert!(!line.contains('@'));
        assert!(!line.contains("email:"));
        assert!(!line.contains("org:"));
        assert!(!line.contains('\n'));
        assert!(line.len() < 400);
        let (at, provider, parsed_model, account, session, turn) = parse_usage_line(&line).unwrap();
        assert_eq!(at, 1_700_000_000_000);
        assert_eq!(provider, "anthropic");
        assert!(!parsed_model.contains(' '));
        assert!(parsed_model.len() <= 128);
        assert_eq!(account, key.as_str());
        assert_eq!(session, "abcd1234");
        assert!(turn);
        assert!(line.ends_with(" reserve-fallback"));
        assert!(parse_usage_line("1 anthropic/m session=a turn=true tools=1 -> anthropic|email:a@x.io|org:o").is_none());
    }

    #[test]
    fn sticky_map_evicts_oldest_past_cap() {
        let now = 1_000_000;
        let mut sessions = HashMap::new();
        for index in 0..=MAX_STICKY_SESSIONS {
            sessions.insert(
                format!("s{index}"),
                Sticky { key: "a".into(), model: "m".into(), cwd: None, last_used: now + index as i64, requests: 1 },
            );
        }
        retain_sessions(&mut sessions, now + 10_000);
        assert_eq!(sessions.len(), MAX_STICKY_SESSIONS);
        assert!(!sessions.contains_key("s0"));
        assert!(sessions.contains_key(&format!("s{MAX_STICKY_SESSIONS}")));
        sessions.insert(
            "expired".into(),
            Sticky { key: "a".into(), model: "m".into(), cwd: None, last_used: now - 25 * 60 * 60_000, requests: 1 },
        );
        retain_sessions(&mut sessions, now + 10_000);
        assert!(!sessions.contains_key("expired"));
        assert!(sessions.len() <= MAX_STICKY_SESSIONS);
    }

    #[cfg(unix)]
    #[test]
    fn gateway_log_is_private_and_rotates() {
        let dir = std::env::temp_dir().join(format!("aam-gwlog-{}", aam_protocol::new_id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bridge-gateway-4101.log");
        {
            let mut file = open_bounded_log(&path).unwrap();
            use std::io::Write;
            writeln!(file, "ok").unwrap();
        }
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        fs::write(&path, vec![b'x'; (LOG_ROTATE_BYTES as usize) + 8]).unwrap();
        let _ = open_bounded_log(&path).unwrap();
        assert!(path.with_extension("log.1").exists());
        assert!(fs::metadata(&path).unwrap().len() < LOG_ROTATE_BYTES);
        let link = dir.join("link.log");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(open_bounded_log(&link).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_ojak_logins_follow_running_upstreams_and_skip_existing() {
        let owned = |items: &[&str]| items.iter().map(|item| (*item).to_owned()).collect::<Vec<_>>();
        let logged_in = owned(&["anthropic", "openai-codex", "google-antigravity", "xai-oauth", "ojak-codex"]);
        let running = owned(&["anthropic", "openai-codex", "google-antigravity", "xai-oauth"]);
        assert_eq!(missing_ojak_logins(&logged_in, &running), vec!["ojak-claude", "ojak-antigravity", "ojak-grok"]);
        // 이미 로그인된 공급자와, gateway가 아직 없는 공급자는 올리지 않는다.
        assert!(missing_ojak_logins(&owned(&["ojak-claude"]), &owned(&["anthropic"])).is_empty());
        assert!(missing_ojak_logins(&owned(&["anthropic"]), &[]).is_empty());
        // 이전 ID 로그인은 gateway가 없어도 새 ID로 맞춘다. 사용자 항목은 지우지 않는다.
        assert_eq!(missing_ojak_logins(&owned(&["aam-zai"]), &[]), vec!["ojak-zai"]);
    }


}

/// `omp-broker-service.json`(launcher가 검증한 omp 경로 기록)에서 omp 실행 경로를 읽는다.
/// 링크·남의 파일·큰 파일·형식이 다른 기록·존재하지 않는 `.exe`는 무시한다.
#[cfg(windows)]
fn broker_marker_runtime(path: &Path) -> Option<PathBuf> {
    #[derive(Deserialize)]
    struct Marker { owner: String, runtime: PathBuf }
    let mut file = aam_protocol::secure::open_read_no_follow(path).ok()?;
    if !aam_protocol::secure::file_owned_by_me(&file).ok()? || file.metadata().ok()?.len() > 4096 {
        return None;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let marker: Marker = serde_json::from_slice(&bytes).ok()?;
    let runtime = marker.runtime;
    (marker.owner == "ai-account-manager.omp-broker"
        && runtime.is_absolute()
        && runtime.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        && runtime.is_file())
    .then_some(runtime)
}
