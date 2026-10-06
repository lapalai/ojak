mod ipc;
mod process;
pub mod secure;
#[cfg(windows)]
pub mod winutil;
pub use ipc::{connect, peer_is_self, LocalListener, LocalStream};
pub use process::{process_alive, process_identity};
#[cfg(windows)]
pub use winutil::{job_name, process_parent, spawn_in_job, ProcessJob};

/// Windows: 이 경로의 서비스 프로세스 ID(같은 사용자 확인 후). 서비스 중지에 쓴다.
#[cfg(windows)]
pub fn service_pid(paths: &Paths) -> io::Result<u32> {
    winutil::pipe_server_pid(&winutil::pipe_name_for(&paths.socket))
}

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    io::{self, Read, Write},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const MAX_FRAME: usize = 1024 * 1024;
// 모델을 덮어쓰지 않고 공식 CLI가 기존 설정으로 결정하게 합니다.
pub const NATIVE_DEFAULT_MODEL: &str = "native-default";
/// integration.json 형식 판. 2부터 원본 CLI의 진입 경로(symlink)를 저장하고 실행할 때마다 해석합니다.
/// 판이 없는 기록은 해석된 버전별 경로를 고정했으므로 원본 CLI 탐색에 사용하지 않습니다.
pub const INTEGRATION_VERSION: u32 = 2;
/// omp 계정 브릿지가 듣는 loopback 포트.
pub const BRIDGE_PORT: u16 = 4020;
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Paths {
    pub home: PathBuf,
    pub socket: PathBuf,
    pub database: PathBuf,
    pub profiles: PathBuf,
}
/// 사용자 홈 절대 경로. Windows는 `USERPROFILE`(일반 로그온에는 `HOME`이 없다), 그 밖은 `HOME`.
/// 도구·gateway 공급자 이름을 수동 배정 키(공급자)로 바꾼다.
pub fn pin_provider(name: &str) -> &str {
    match name {
        "claude" | "anthropic" => "anthropic",
        "codex" | "openai" | "openai-codex" => "openai",
        "google" | "google-antigravity" => "google",
        "xai" | "xai-oauth" => "xai",
        other => other,
    }
}

pub fn user_home() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from).filter(|path| path.is_absolute())
}
impl Paths {
    pub fn discover() -> io::Result<Self> {
        #[cfg(windows)]
        let user_home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .ok_or_else(|| io::Error::other("사용자 홈 경로가 없습니다"))?;
        #[cfg(not(windows))]
        let user_home = std::env::var_os("HOME")
            .ok_or_else(|| io::Error::other("HOME 경로가 없습니다"))?;
        let home = std::env::var_os("AAM_HOME").map(PathBuf::from).unwrap_or_else(|| {
            #[cfg(windows)]
            {
                std::env::var_os("APPDATA")
                    .map(|dir| PathBuf::from(dir).join("AI Account Manager"))
                    .unwrap_or_else(|| {
                        PathBuf::from(&user_home)
                            .join("AppData")
                            .join("Roaming")
                            .join("AI Account Manager")
                    })
            }
            #[cfg(not(windows))]
            {
                PathBuf::from(user_home).join("Library/Application Support/AI Account Manager")
            }
        });
        let suffix = home.to_string_lossy().bytes().fold(14695981039346656037u64, |h, b| {
            (h ^ b as u64).wrapping_mul(1099511628211)
        });
        #[cfg(windows)]
        let socket = PathBuf::from(format!(r"\\.\pipe\aam-{suffix:x}"));
        #[cfg(not(windows))]
        let socket = home.join("run").join(format!("{suffix:x}.sock"));
        Ok(Self {
            database: home.join("state.sqlite3"),
            profiles: home.join("profiles"),
            home,
            socket,
        })
    }
    pub fn prepare(&self) -> io::Result<()> {
        secure::restrict_dir(&self.home)?;
        secure::restrict_dir(&self.profiles)?;
        #[cfg(not(windows))]
        if let Some(parent) = self.socket.parent() {
            secure::restrict_dir(parent)?;
        }
        Ok(())
    }
    /// omp 계정 브릿지 켜기/끄기 설정. 서비스가 주기적으로 읽는다.
    pub fn bridge_settings(&self) -> PathBuf {
        self.home.join("bridge.json")
    }
    /// omp가 `models.yml`의 `!cat` 명령으로 읽는 브릿지 인증 토큰.
    pub fn bridge_token(&self) -> PathBuf {
        self.home.join("bridge.token")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct QuotaBucket {
    pub id: String,
    pub label: String,
    pub model: Option<String>,
    pub used_percent: Option<f64>,
    pub resets_at: Option<i64>,
    pub observed_at: i64,
    pub source: String,
    pub status: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct OmpCredentialPin {
    pub provider: String,
    pub hash: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub id: String,
    pub provider: String,
    pub tool: String,
    pub label: String,
    pub email: Option<String>,
    pub organization: Option<String>,
    pub plan: Option<String>,
    pub profile_path: Option<String>,
    pub binary_path: Option<String>,
    pub identity_key: Option<String>,
    pub auth_status: String,
    pub verification: String,
    pub can_launch: bool,
    pub reason: Option<String>,
    pub enabled: bool,
    pub max_concurrency: u32,
    pub buckets: Vec<QuotaBucket>,
    pub last_checked_at: i64,
    /// 원본 OMP usage identity의 scope/case를 보존한 관측용 digest입니다.
    #[serde(default)]
    pub omp_credential_pins: Vec<OmpCredentialPin>,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ToolStatus {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub binary_path: Option<String>,
    pub version: Option<String>,
    pub installed: bool,
    pub isolation: String,
    pub reason: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RouteScope {
    Directory,
    Repository,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RouteMode {
    Pinned,
    Automatic,
    Unmanaged,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProjectRoute {
    pub path: String,
    pub scope: RouteScope,
    pub tool: String,
    pub mode: RouteMode,
    pub account_id: Option<String>,
    pub model: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteResolution {
    pub mode: RouteMode,
    pub account_id: Option<String>,
    pub model: Option<String>,
    pub source: String,
    pub policy_revision: u64,
    /// 이어가기에서 배정하면 안 되는 원래 계정.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_account_id: Option<String>,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AllocationMode {
    #[default]
    Smart,
    Priority,
}
/// 구버전 저장 정책에는 이 필드가 없습니다. 자동 인계를 기본값으로 읽습니다.
fn enabled() -> bool {
    true
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    pub revision: u64,
    pub automatic: bool,
    #[serde(default)]
    pub allocation_mode: AllocationMode,
    #[serde(default = "enabled")]
    pub auto_takeover: bool,
    #[serde(default)]
    pub account_priority: Vec<String>,
    /// 공급자(`anthropic`·`openai`·`google`·`xai`)별 수동 배정 계정. 없으면 그 공급자는 자동 배정이다.
    /// 고정 계정을 쓸 수 없으면(소진·여유량 부족·로그인 필요 등) 새 작업은 자동 배정으로 넘어간다.
    #[serde(default)]
    pub provider_pins: std::collections::BTreeMap<String, String>,
    pub safety_reserve_percent: f64,
    pub stale_after_seconds: u64,
    #[serde(default)]
    pub project_allowlist: std::collections::BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub project_routes: Vec<ProjectRoute>,
    /// 리셋 전에 쓰지 않으면 사라질 주간 한도가 있는 계정을 스마트 배정에서 먼저 고른다.
    #[serde(default)]
    pub expiring_boost: bool,
    /// 리셋까지 이 시간 이내인 긴 주기 한도만 "곧 리셋"으로 본다.
    #[serde(default = "default_expiring_window_hours")]
    pub expiring_window_hours: u32,
    /// 안전 여유량을 뺀 남은 한도가 이 % 이상일 때만 알린다.
    #[serde(default = "default_expiring_min_percent")]
    pub expiring_min_percent: f64,
}
fn default_expiring_window_hours() -> u32 {
    48
}
fn default_expiring_min_percent() -> f64 {
    30.0
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            revision: 1,
            automatic: true,
            allocation_mode: AllocationMode::default(),
            auto_takeover: true,
            account_priority: Vec::new(),
            provider_pins: Default::default(),
            safety_reserve_percent: 5.0,
            stale_after_seconds: 900,
            project_allowlist: Default::default(),
            project_routes: Vec::new(),
            expiring_boost: false,
            expiring_window_hours: default_expiring_window_hours(),
            expiring_min_percent: default_expiring_min_percent(),
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    pub pid: u32,
    pub started_at: String,
    pub boot_id: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub request_id: String,
    pub account_id: String,
    pub tool: String,
    pub model: String,
    pub cwd: String,
    pub state: String,
    pub verification: String,
    pub started_at: i64,
    pub updated_at: i64,
    pub process: Option<ProcessIdentity>,
    pub supervisor: Option<ProcessIdentity>,
    pub spawn_attempt_id: Option<String>,
    pub generation: String,
    pub exit_code: Option<i32>,
    pub reason: Option<String>,
    #[serde(default)]
    pub native_session_id: Option<String>,
    #[serde(default)]
    pub background_processes: Option<Vec<ProcessIdentity>>,
    #[serde(default)]
    pub parent_session_id: Option<String>,
    /// 다른 계정에서 이어 간 세션이면 원래 관리 세션 ID. 같은 native 대화가 계정을 옮긴 근거다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continued_from: Option<String>,
}
/// 외부 실행의 읽기 전용 관측이며 lease·계정 예약·제어 권한이 아닙니다.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedSession {
    pub id: String,
    pub tool: String,
    pub process: ProcessIdentity,
    pub parent_process: Option<ProcessIdentity>,
    pub host: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub account_id: Option<String>,
    pub verification: String,
    pub reason: Option<String>,
    #[serde(default)]
    pub attributions: Vec<ObservedAttribution>,
    /// 인계 대상으로 사용할 수 있는 실제 대화 파일 경로입니다. 확정할 수 없으면 비웁니다.
    #[serde(default)]
    pub native_session_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservedAttribution {
    pub session_id: String,
    pub role: String,
    pub provider: String,
    pub model: Option<String>,
    pub account_id: Option<String>,
    pub verification: String,
    pub recorded_at: i64,
    pub stop_reason: Option<String>,
    pub source: String,
    /// 요청별 hook에서 확인한 경로. 응답 완료·계정 identity의 증거가 아니며, 과거 기록에는 없을 수 있습니다.
    #[serde(default)]
    pub route: Option<String>,
    pub reason: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub id: String,
    pub level: String,
    pub title: String,
    pub message: String,
}
/// 외부에서 시작한 대화를 확인된 소유 계정으로 관리 실행에 인계하는 등록입니다.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Takeover {
    pub tool: String,
    pub native_session_id: String,
    pub account_id: String,
    pub cwd: Option<String>,
    pub adopted_at: i64,
    /// `manual`은 사용자가 지정한 인계, `automatic`은 정책에 따른 인계입니다.
    pub source: String,
    pub evidence: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub version: u32,
    pub generated_at: i64,
    pub service_started_at: i64,
    pub accounts: Vec<Account>,
    pub tools: Vec<ToolStatus>,
    pub sessions: Vec<Session>,
    #[serde(default)]
    pub observed_sessions: Vec<ObservedSession>,
    pub policy: Policy,
    pub notices: Vec<Notice>,
    pub refreshing: bool,
    pub last_refresh_at: Option<i64>,
    #[serde(default)]
    pub takeovers: Vec<Takeover>,
    /// 서비스가 계산한 계정 그룹별 한도 요약. 특정 실행의 허용 여부는 route.explain으로 확인한다.
    #[serde(default)]
    pub quota_summaries: Vec<AccountQuotaSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountQuotaSummary {
    pub account_ids: Vec<String>,
    pub kind: String,
    pub until: Option<i64>,
    pub models: Vec<String>,
    pub label: Option<String>,
    pub rate: bool,
    /// 곧 리셋되는데 많이 남은 긴 주기 한도. 리셋 전에 쓰면 한도를 아낄 수 있다.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiring: Option<ExpiringQuota>,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExpiringQuota {
    pub label: String,
    pub resets_at: i64,
    /// 안전 여유량을 뺀, 리셋 전에 쓸 수 있는 남은 %.
    pub usable_percent: f64,
    /// 리셋 전에 다 쓰려면 시간당 필요한 %.
    pub per_hour: f64,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct LaunchIntent {
    pub tool: String,
    pub model: String,
    pub cwd: String,
    pub account_id: Option<String>,
    pub parent_session_id: Option<String>,
    pub resume_session_id: Option<String>,
    #[serde(default)]
    pub native_session_id: Option<String>,
    /// 서비스가 인계 대상으로 확인한 재개입니다. 클라이언트 지정값은 무시하고 매 요청에서 다시 확인합니다.
    #[serde(default)]
    pub adopted: bool,
    /// `resume_session_id`의 대화를 원래 계정이 아닌 다른 계정에서 이어 간다(한도 소진 뒤 이어가기).
    /// 원래 계정과 같은 실제 계정은 배정에서 뺀다. 대화 기록 복사는 실행기가 한다.
    #[serde(default)]
    pub continue_elsewhere: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IdentityEvidence {
    pub identity_key: String,
    pub tier: String,
    pub observed_at: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchPlan {
    pub program: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub account: Account,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseGrant {
    pub session: Session,
    pub capability: String,
    pub account: Account,
    /// 공급자 수동 배정 계정을 지금 쓸 수 없어 다른 계정을 배정했는지.
    #[serde(default)]
    pub pin_unavailable: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub account_id: String,
    pub eligible: bool,
    pub score: Option<f64>,
    pub reasons: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub selected_account_id: Option<String>,
    /// 공급자 수동 배정 계정이 있었지만 지금 쓸 수 없어 다른 계정을 고른 경우 그 고정 계정 ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_unavailable: Option<String>,
    pub candidates: Vec<Candidate>,
    pub policy_revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcRequest {
    pub version: u32,
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}
impl RpcRequest {
    pub fn new(method: &str, params: Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id: new_id(),
            method: method.into(),
            params,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
impl ApiError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpcResponse {
    pub version: u32,
    pub id: String,
    pub result: Option<Value>,
    pub error: Option<ApiError>,
}
impl RpcResponse {
    pub fn ok(id: String, result: Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn err(id: String, error: ApiError) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            result: None,
            error: Some(error),
        }
    }
}
pub fn read_frame<R: Read, T: serde::de::DeserializeOwned>(reader: &mut R) -> io::Result<T> {
    let mut size = [0; 4];
    reader.read_exact(&mut size)?;
    let len = u32::from_be_bytes(size) as usize;
    if len == 0 || len > MAX_FRAME {
        return Err(io::Error::other(
            "IPC 프레임 크기가 허용 범위를 벗어났습니다",
        ));
    }
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map_err(|_| io::Error::other("IPC JSON 형식이 올바르지 않습니다"))
}
pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::other("IPC 응답 크기가 너무 큽니다"));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}
pub fn call(paths: &Paths, method: &str, params: Value) -> Result<Value, ApiError> {
    let stream = connect(&paths.socket).map_err(|_| {
        ApiError::new(
            "DAEMON_UNAVAILABLE",
            "관리 서비스에 연결할 수 없습니다. 연결 설정에서 서비스를 시작하세요.",
        )
    })?;
    // 서비스가 검증하듯 클라이언트도 상대가 같은 사용자인지 확인한다. 다른 사용자가 경로를
    // 선점해 가짜 응답(실행 파일 경로 등)을 주는 것을 막는다.
    if !peer_is_self(&stream) {
        return Err(ApiError::new(
            "DAEMON_UNTRUSTED",
            "관리 서비스 연결의 상대가 현재 사용자가 아닙니다. 연결하지 않았습니다.",
        ));
    }
    let mut stream = stream;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(60)))
        .map_err(|e| ApiError::new("IPC_ERROR", e.to_string()))?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|e| ApiError::new("IPC_ERROR", e.to_string()))?;
    let request = RpcRequest::new(method, params);
    write_frame(&mut stream, &request).map_err(|e| ApiError::new("IPC_ERROR", e.to_string()))?;
    let response: RpcResponse =
        read_frame(&mut stream).map_err(|e| ApiError::new("IPC_ERROR", e.to_string()))?;
    if response.version != PROTOCOL_VERSION || response.id != request.id {
        return Err(ApiError::new(
            "PROTOCOL_MISMATCH",
            "관리 서비스 응답이 요청과 일치하지 않습니다",
        ));
    }
    if let Some(error) = response.error {
        Err(error)
    } else {
        response
            .result
            .ok_or_else(|| ApiError::new("IPC_ERROR", "관리 서비스 응답이 비어 있습니다"))
    }
}

#[cfg(test)]
mod attribution_compatibility_tests {
    use super::*;

    #[test]
    fn persisted_accounts_and_observations_without_attribution_fields_still_load() {
        let mut legacy_account = serde_json::to_value(Account::default()).unwrap();
        legacy_account
            .as_object_mut()
            .unwrap()
            .remove("ompCredentialPins");
        let account: Account = serde_json::from_value(legacy_account).unwrap();
        assert!(account.omp_credential_pins.is_empty());
        let observed: ObservedSession = serde_json::from_value(serde_json::json!({
            "id":"external","tool":"omp","process":{"pid":12,"startedAt":"100:000000","bootId":"boot"},
            "host":"terminal","verification":"observed"
        })).unwrap();
        assert!(observed.attributions.is_empty());
        assert!(observed.account_id.is_none());
    }
}
