//! 앱과 실행 중인 서비스의 버전 비교, 그리고 둘을 맞추는 안전한 서비스 재시작.
//!
//! 앱을 DMG로 덮어쓰면 화면은 새 버전인데 `aam-service`는 예전 프로세스로 남을 수 있다. 서비스가 알려 주는
//! 버전(`Snapshot.service_version`)을 앱에 묶인 `aam`의 버전과 비교해 알려 준다. 이 필드가 없던 이전 서비스는
//! '알 수 없음'이고 재시작 대상이다. 조용히 재시작하지 않는다. 사용자가 누를 때만 아래 순서로 돌린다.
//!
//! 1. 서비스의 버전·시작 시각을 읽는다. 이미 같은 버전이면 아무것도 하지 않는다.
//! 2. 재시작이 끊을 작업을 센다(`busy_preview`: 관리 CLI lease는 `status.read`, omp 응답은 `bridge.status`). 이미 있는
//!    RPC만 쓰므로 예전 서비스에도 통한다. 하나라도 있거나(omp 수를 못 읽은 것 포함) 사용자 동의(`force`)가 없으면
//!    아무것도 바꾸지 않고 `Busy`로 돌려준다. 시간 초과만으로 lease를 풀지 않는다.
//! 3. 새 배정을 닫는다(`service.prepareUninstall`). 동의 없는 경로는 lease가 없음을 같은 트랜잭션에서 확인한다.
//!    동의한 경로는 `{"restart": true}`로 lease가 있어도 배정만 닫는다(lease 기록은 지우지도 풀지도 않는다).
//!    이 매개변수를 모르는 예전 서비스는 lease가 있으면 `SESSION_BUSY`로 거절하므로, 동의한 경우에 한해 배정을
//!    닫지 않고 진행한다(그 사이 시작한 배정은 새 서비스의 `reconcile(true)`가 불확실로 유지한다).
//! 4. 서비스를 다시 시작한다. 시작이 실패하면 배정을 다시 연다.
//! 5. 새 프로세스(시작 시각이 바뀜)가 응답하면 배정을 다시 열고, 버전이 앱과 같은지 확인한다.
//!
//! 관리 CLI(claude·codex) 프로세스는 서비스의 자식이 아니라 터미널의 `aam` 실행기가 시작한다. 서비스가 다시 시작돼도
//! 죽지 않고, 실행기가 10초마다 heartbeat를 다시 보내 새 서비스가 lease를 이어받는다(`docs/domains/launcher.md`).
//! omp 모델 스트림은 서비스 안의 브릿지가 중계하므로 서비스가 내려가면 끊긴다.
use aam_protocol::{ApiError, Paths, Session, Snapshot};
use serde::Serialize;
use serde_json::{json, Value};
use std::{
    cmp::Ordering,
    collections::BTreeMap,
    time::{Duration, Instant},
};

/// 이 `aam`(앱에 묶인 launcher)의 버전. `aam-service`와 같은 workspace 버전이다.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ServiceVersion {
    /// 서비스 버전이 앱과 같다.
    Current,
    /// 서비스가 앱보다 예전 버전이다.
    Older,
    /// 서비스가 앱보다 새 버전이다(예전 앱을 다시 설치한 경우). 되돌리지 않으므로 재시작 대상이 아니다.
    Newer,
    /// 서비스가 버전을 알려 주지 않았거나 읽을 수 없다(이 기능 이전 서비스).
    Unknown,
}

impl ServiceVersion {
    pub fn needs_restart(self) -> bool {
        matches!(self, Self::Older | Self::Unknown)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionReport {
    pub app_version: String,
    pub service_version: Option<String>,
    pub state: ServiceVersion,
    /// 다시 시작해야 앱과 맞는다.
    pub mismatch: bool,
}

/// `major.minor.patch`만 읽는다. `-rc.1`·`+build` 꼬리는 버린다.
fn parse(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.trim().split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((major, minor, patch))
}

pub fn compare(app: &str, service: Option<&str>) -> ServiceVersion {
    let Some(service) = service.map(str::trim).filter(|value| !value.is_empty()) else {
        return ServiceVersion::Unknown;
    };
    if service == app.trim() {
        return ServiceVersion::Current;
    }
    match (parse(app), parse(service)) {
        (Some(app), Some(service)) => match service.cmp(&app) {
            Ordering::Less => ServiceVersion::Older,
            Ordering::Greater => ServiceVersion::Newer,
            // 숫자는 같은데 문자열이 다르면(사전 배포 꼬리) 같은 빌드인지 알 수 없다.
            Ordering::Equal => ServiceVersion::Unknown,
        },
        _ => ServiceVersion::Unknown,
    }
}

pub fn report(app: &str, service: Option<&str>) -> VersionReport {
    let state = compare(app, service);
    VersionReport {
        app_version: app.to_owned(),
        service_version: service.map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned),
        state,
        mismatch: state.needs_restart(),
    }
}

/// 서비스가 응답할 때만 보고서를 만든다. 응답이 없으면 그 오류를 그대로 돌려준다.
pub fn read_report(paths: &Paths) -> Result<VersionReport, ApiError> {
    let info = ServiceInfo::from_snapshot(&crate::read_snapshot(paths)?);
    Ok(report(APP_VERSION, info.version.as_deref()))
}

/// 이미 읽은 snapshot으로 만든 보고서.
pub fn report_for(snapshot: &Snapshot) -> VersionReport {
    report(APP_VERSION, snapshot.service_version.as_deref())
}

/// `aam service status`에 덧붙일 줄. 서비스가 응답하지 않으면 `None`.
pub fn status_text(paths: &Paths) -> Option<String> {
    let report = read_report(paths).ok()?;
    let service = report.service_version.as_deref().unwrap_or("알 수 없음");
    let mut text = format!("서비스 버전: {service} (앱 {})", report.app_version);
    if report.mismatch {
        text.push_str("\n서비스가 앱과 달라요. `aam service restart`로 다시 시작해 주세요.");
    }
    Some(text)
}

/// 도구별 관리 CLI 대화 수. `active`는 실행 중, `uncertain`은 시작·종료가 확인되지 않아 슬롯을 쥔 것
/// (PREPARED·STARTING·SUSPECT·ORPHANED)이다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolBusy {
    pub tool: String,
    pub active: usize,
    pub uncertain: usize,
}

/// omp 브릿지의 진행 중 모델 스트림과 몫을 기다리는 요청. `known`이 거짓이면 읽지 못한 것이다(0이 아니다).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OmpBusy {
    pub known: bool,
    pub streams: usize,
    pub queued: usize,
}

/// 서비스를 다시 시작하면 영향을 받는 작업의 개수. 계정·이메일·경로·프롬프트는 담지 않는다.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BusyReport {
    pub tools: Vec<ToolBusy>,
    pub omp: OmpBusy,
}

impl BusyReport {
    /// 아무것도 쓰지 않는 것이 확인된 상태.
    pub fn idle() -> Self {
        Self { tools: Vec::new(), omp: OmpBusy { known: true, streams: 0, queued: 0 } }
    }
    pub fn managed(&self) -> usize {
        self.tools.iter().map(|tool| tool.active + tool.uncertain).sum()
    }
    pub fn omp_in_flight(&self) -> usize {
        self.omp.streams + self.omp.queued
    }
    /// 사용자 동의가 필요한 상태. omp 수를 읽지 못했으면 쉬고 있음을 증명하지 못한 것이라 동의가 필요하다.
    pub fn needs_consent(&self) -> bool {
        self.managed() > 0 || !self.omp.known || self.omp_in_flight() > 0
    }
    /// 사람이 읽는 줄(CLI 출력용).
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for tool in &self.tools {
            let name = match tool.tool.as_str() {
                "claude" => "Claude",
                "codex" => "Codex",
                other => other,
            };
            lines.push(format!("{name} 터미널 대화 {}개 (실행 중 {}, 시작·종료 확인 중 {})", tool.active + tool.uncertain, tool.active, tool.uncertain));
        }
        if !self.omp.known {
            lines.push("omp 응답이 진행 중인지 확인하지 못했어요".into());
        } else {
            if self.omp.streams > 0 {
                lines.push(format!("omp 응답 {}개 진행 중", self.omp.streams));
            }
            if self.omp.queued > 0 {
                lines.push(format!("omp 요청 {}개 자리 대기 중", self.omp.queued));
            }
        }
        lines
    }
    /// 강제 재시작 뒤 사용자가 할 일. 서비스가 끊은 것만 안내한다.
    pub fn guidance(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.omp.known || self.omp_in_flight() > 0 {
            lines.push("아직 보내지 않고 기다리던 omp 작업은 서비스가 돌아오면 이어져요. 대기 한도에 닿아 멈췄다면 그 omp 세션에서 `/ojak-resume`을 실행하세요.".into());
            lines.push("이미 보낸 요청이 끊겼다면 결과를 알 수 없어요. 도구가 이미 실행됐을 수 있으니 같은 메시지를 그대로 다시 보내지 말고 결과부터 확인하세요.".into());
        }
        if self.managed() > 0 {
            lines.push("Claude·Codex 터미널 대화는 끊기지 않고 이어져요. 재시작 동안 서비스 연결 경고가 한 번 뜰 수 있어요.".into());
        }
        lines
    }
}

/// 세션 목록과 `bridge.status` 응답에서 개수만 뽑는다. 브릿지 응답이 없거나 필드가 없으면(이 필드가 없던 서비스) omp는 '알 수 없음'이다.
pub fn busy_from(sessions: &[Session], bridge: Option<&Value>) -> BusyReport {
    let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for session in sessions {
        let active = match session.state.as_str() {
            "ACTIVE" => true,
            "PREPARED" | "STARTING" | "SUSPECT" | "ORPHANED" => false,
            _ => continue,
        };
        let entry = counts.entry(session.tool.as_str()).or_default();
        if active {
            entry.0 += 1;
        } else {
            entry.1 += 1;
        }
    }
    let tools = counts.into_iter().map(|(tool, (active, uncertain))| ToolBusy { tool: tool.to_owned(), active, uncertain }).collect();
    let read = |pointer: &str| bridge.and_then(|status| status.pointer(pointer)).and_then(Value::as_u64);
    let omp = match (read("/connections/public/longActive"), read("/connections/modelQueue/waiting")) {
        (Some(streams), Some(queued)) => OmpBusy { known: true, streams: streams as usize, queued: queued as usize },
        _ => OmpBusy { known: false, streams: 0, queued: 0 },
    };
    BusyReport { tools, omp }
}

/// 지금 서비스에서 읽은 작업 수. 읽기 전용이다(배정을 닫지도 lease를 바꾸지도 않는다).
pub fn busy_preview(paths: &Paths) -> Result<BusyReport, ApiError> {
    let snapshot: Snapshot = crate::decode(aam_protocol::call(paths, "status.read", json!({ "observe": false }))?)?;
    let bridge = aam_protocol::call(paths, "bridge.status", json!({})).ok();
    Ok(busy_from(&snapshot.sessions, bridge.as_ref()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RestartStatus {
    /// 다시 시작했고 버전을 확인했다. `busy`는 그때 끊긴 작업이다.
    Restarted,
    /// 이미 같은 버전이거나 서비스가 더 새 버전이라 아무것도 하지 않았다.
    Unchanged,
    /// 동의 없이 끊길 작업이 있어 아무것도 바꾸지 않았다. `busy`가 그 작업이다.
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestartReport {
    pub status: RestartStatus,
    pub message: String,
    pub busy: BusyReport,
}

impl RestartReport {
    fn unchanged(message: String) -> Self {
        Self { status: RestartStatus::Unchanged, message, busy: BusyReport::idle() }
    }
    fn busy(busy: BusyReport) -> Self {
        Self { status: RestartStatus::Busy, message: "끊길 작업이 있어 서비스를 다시 시작하지 않았어요.".into(), busy }
    }
    /// CLI 출력. 동의 없이 거절한 경우는 `SESSION_BUSY` 오류(요약 포함, 종료 코드 1)다.
    pub fn cli_text(self) -> Result<String, ApiError> {
        if self.status == RestartStatus::Busy {
            let list = self.busy.describe().into_iter().map(|line| format!("\n  - {line}")).collect::<String>();
            return Err(ApiError::new(
                "SESSION_BUSY",
                format!("지금 쓰는 작업이 있어서 서비스를 다시 시작하지 않았어요. 아무것도 바꾸지 않았어요.{list}\n서비스를 다시 시작하면 omp 응답은 끊겨요. Claude·Codex 터미널 대화는 계속돼요. 그래도 다시 시작하려면 `aam service restart --force`를 실행해 주세요."),
            ));
        }
        let mut text = self.message;
        if self.status == RestartStatus::Restarted {
            for line in self.busy.guidance() {
                text.push_str("\n- ");
                text.push_str(&line);
            }
        }
        Ok(text)
    }
}

/// 재시작 전후로 비교하는 서비스 정보. 시작 시각이 바뀌어야 새 프로세스로 본다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    pub version: Option<String>,
    pub started_at: i64,
}

impl ServiceInfo {
    pub fn from_snapshot(snapshot: &Snapshot) -> Self {
        Self { version: snapshot.service_version.clone(), started_at: snapshot.service_started_at }
    }
}

/// 재시작 순서의 플랫폼별 단계. 테스트에서는 기록용 가짜를 쓴다.
pub trait RestartHost {
    fn info(&mut self) -> Result<ServiceInfo, ApiError>;
    /// 재시작이 끊을 작업을 읽는다(읽기 전용).
    fn busy(&mut self) -> Result<BusyReport, ApiError>;
    /// 새 배정을 닫는다. `interrupt`가 아니면 점유 lease가 없음도 함께 확인하고, 있으면 `SESSION_BUSY`(아무것도 바꾸지 않음).
    /// `interrupt`면 lease가 있어도 닫는다(lease 기록은 건드리지 않는다). 돌려준 permit이 `None`이면 닫지 못한 채 진행한다는 뜻이다
    /// (배정 차단 매개변수를 모르는 예전 서비스).
    fn close_admission(&mut self, interrupt: bool) -> Result<Option<String>, ApiError>;
    /// 닫은 배정을 다시 연다(재시작이 시작되지 않았을 때). 실패해도 오류로 알리지 않는다.
    fn reopen(&mut self, permit: Option<&str>);
    fn restart(&mut self, interrupt: bool) -> Result<(), ApiError>;
    /// 새 서비스가 응답한 뒤 닫아 둔 배정을 다시 연다.
    fn resume(&mut self) -> Result<(), ApiError>;
}

pub fn restart_checked(host: &mut dyn RestartHost, app_version: &str, wait: Duration, force: bool) -> Result<RestartReport, ApiError> {
    let before = host.info()?;
    let state = compare(app_version, before.version.as_deref());
    if state == ServiceVersion::Current {
        return Ok(RestartReport::unchanged(format!("서비스가 이미 앱과 같은 버전({app_version})이라 다시 시작하지 않았어요.")));
    }
    if state == ServiceVersion::Newer {
        return Ok(RestartReport::unchanged("서비스가 앱보다 새 버전이라 다시 시작하지 않았어요. 앱을 최신 버전으로 설치해 주세요.".into()));
    }
    let preview = host.busy()?;
    if !force && preview.needs_consent() {
        return Ok(RestartReport::busy(preview));
    }
    let permit = match host.close_admission(force) {
        Ok(permit) => permit,
        // 미리보기 뒤에 CLI 대화가 시작됐다. 같은 트랜잭션 검사가 거절했으므로 아무것도 바뀌지 않았다.
        Err(error) if error.code == "SESSION_BUSY" && !force => {
            let now = host.busy()?;
            if now.needs_consent() {
                return Ok(RestartReport::busy(now));
            }
            return Err(ApiError::new(
                "SESSION_BUSY",
                "실행 중이거나 시작 결과가 불확실한 대화가 있어서 서비스를 다시 시작하지 않았어요. 대화를 끝낸 뒤 다시 시도해 주세요.",
            ));
        }
        Err(error) => return Err(error),
    };
    // 배정을 닫은 뒤 한 번 더 읽는다. 미리보기 뒤에 omp 스트림이 시작됐다면(브릿지는 배정 차단과 무관하다) 동의 없이 끊지 않는다.
    let interrupted = match host.busy() {
        Ok(now) => now,
        Err(error) if !force => {
            host.reopen(permit.as_deref());
            return Err(error);
        }
        Err(_) => preview,
    };
    if !force && interrupted.needs_consent() {
        host.reopen(permit.as_deref());
        return Ok(RestartReport::busy(interrupted));
    }
    if let Err(error) = host.restart(force) {
        host.reopen(permit.as_deref());
        return Err(error);
    }
    let deadline = Instant::now() + wait;
    let after = loop {
        // 이전 프로세스가 아직 응답하는 순간을 새 서비스로 착각하지 않도록 시작 시각이 바뀌길 기다린다.
        if let Ok(info) = host.info() {
            if info.started_at != before.started_at {
                break info;
            }
        }
        let now = Instant::now();
        if now >= deadline {
            // 새 프로세스를 확인하지 못했다. 응답하는 서비스가 있으면 배정만 다시 열고 상태를 알린다.
            let _ = host.resume();
            return Err(ApiError::new(
                "SERVICE_NOT_READY",
                "서비스가 다시 시작됐는지 확인하지 못했어요. 잠시 뒤 `aam service status`로 확인하고, 그대로면 `aam service install`을 실행해 주세요.",
            ));
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(200)));
    };
    host.resume()?;
    if compare(app_version, after.version.as_deref()) != ServiceVersion::Current {
        return Err(ApiError::new(
            "SERVICE_VERSION_STALE",
            "서비스를 다시 시작했지만 아직 예전 버전이에요. 자동 실행이 다른 위치의 앱을 가리킬 수 있어요. `aam service uninstall` 뒤 이 앱의 `aam service install`을 실행해 주세요.",
        ));
    }
    Ok(RestartReport {
        status: RestartStatus::Restarted,
        message: format!("서비스를 안전하게 다시 시작했어요. 이제 버전 {app_version}이에요."),
        busy: interrupted,
    })
}

/// 실제 서비스를 대상으로 하는 단계. 점유 lease 검사는 서비스의 `service.prepareUninstall`이 한 트랜잭션으로 한다.
struct SystemHost<'a>(&'a Paths);

impl RestartHost for SystemHost<'_> {
    fn info(&mut self) -> Result<ServiceInfo, ApiError> {
        Ok(ServiceInfo::from_snapshot(&crate::read_snapshot(self.0)?))
    }
    fn busy(&mut self) -> Result<BusyReport, ApiError> {
        busy_preview(self.0)
    }
    fn close_admission(&mut self, interrupt: bool) -> Result<Option<String>, ApiError> {
        match prepare_permit(self.0, interrupt) {
            Ok(permit) => Ok(Some(permit)),
            // `restart` 매개변수를 아는 서비스는 동의한 재시작을 `SESSION_BUSY`로 거절하지 않는다. 거절했다면 예전 서비스다.
            // 사용자가 끊김에 동의했으므로 배정은 닫지 못한 채 진행한다. lease 기록은 어느 쪽이든 그대로다.
            Err(error) if interrupt && error.code == "SESSION_BUSY" => Ok(None),
            Err(error) => Err(error),
        }
    }
    fn reopen(&mut self, permit: Option<&str>) {
        if let Some(permit) = permit {
            let _ = aam_protocol::call(self.0, "service.cancelUninstall", json!({ "permit": permit }));
        }
    }
    fn restart(&mut self, interrupt: bool) -> Result<(), ApiError> {
        #[cfg(unix)]
        let _ = interrupt;
        #[cfg(unix)]
        return crate::install::service_kickstart(self.0);
        // Windows: 안전 종료 뒤 기록된 실행 파일로 다시 시작한다. `interrupt`가 아니면 같은 lease 검사를 거친다. 앱이 등록한 서비스가
        // 아니면 아무것도 하지 않으므로 아래 확인 단계가 시작 시각이 안 바뀐 것을 `SERVICE_NOT_READY`로 알린다.
        #[cfg(windows)]
        return crate::install::service_restart(self.0, interrupt);
    }
    fn resume(&mut self) -> Result<(), ApiError> {
        aam_protocol::call(self.0, "service.resumeAdmission", json!({})).map(|_| ())
    }
}

/// 앱 버전과 다른 서비스를 안전하게 다시 시작한다. 사용자가 명령·버튼으로 요청했을 때만 부른다.
/// `force`는 끊길 작업(omp 응답, 관리 CLI 대화)이 있어도 진행한다는 사용자의 명시적 동의다.
pub fn restart(paths: &Paths, force: bool) -> Result<RestartReport, ApiError> {
    restart_checked(&mut SystemHost(paths), APP_VERSION, Duration::from_secs(20), force)
}

/// 배정을 닫고 permit을 받는다. `restart`가 아니면 점유 lease가 있을 때 서비스가 `SESSION_BUSY`로 거절한다.
fn prepare_permit(paths: &Paths, restart: bool) -> Result<String, ApiError> {
    let params = if restart { json!({ "restart": true }) } else { json!({}) };
    let prepared = aam_protocol::call(paths, "service.prepareUninstall", params)?;
    prepared
        .get("permit")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty() && prepared.get("safe").and_then(|value| value.as_bool()) == Some(true))
        .map(str::to_owned)
        .ok_or_else(|| ApiError::new("INVALID_UNINSTALL_PERMIT", "새 배정 차단과 사용 중 안전 검사를 확인하지 못해 서비스를 다시 시작하지 않았어요."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_covers_equal_older_newer_and_unknown() {
        assert_eq!(compare("0.2.0", Some("0.2.0")), ServiceVersion::Current);
        assert_eq!(compare("0.2.0", Some("0.1.1")), ServiceVersion::Older);
        assert_eq!(compare("0.2.0", Some("0.10.0")), ServiceVersion::Newer);
        assert_eq!(compare("0.2.0", Some("0.1.9")), ServiceVersion::Older);
        assert_eq!(compare("0.2.0", None), ServiceVersion::Unknown);
        assert_eq!(compare("0.2.0", Some("  ")), ServiceVersion::Unknown);
        assert_eq!(compare("0.2.0", Some("garbage")), ServiceVersion::Unknown);
        // 같은 숫자의 사전 배포 꼬리는 같은 빌드인지 알 수 없다.
        assert_eq!(compare("0.2.0", Some("0.2.0-rc.1")), ServiceVersion::Unknown);
    }

    #[test]
    fn only_older_and_unknown_services_need_a_restart() {
        assert!(report("0.2.0", Some("0.1.1")).mismatch);
        assert!(report("0.2.0", None).mismatch);
        assert!(!report("0.2.0", Some("0.2.0")).mismatch);
        assert!(!report("0.2.0", Some("0.3.0")).mismatch, "더 새 서비스는 예전 버전으로 되돌리지 않는다");
        let json = serde_json::to_value(report("0.2.0", None)).unwrap();
        assert_eq!(json["state"], "unknown");
        assert_eq!(json["serviceVersion"], serde_json::Value::Null);
        assert_eq!(json["mismatch"], true);
    }

    fn session(tool: &str, state: &str) -> Session {
        Session {
            id: format!("{tool}-{state}"),
            request_id: String::new(),
            account_id: "private-account".into(),
            tool: tool.into(),
            model: "model".into(),
            cwd: "/private/project".into(),
            state: state.into(),
            verification: String::new(),
            started_at: 0,
            updated_at: 0,
            process: None,
            supervisor: None,
            spawn_attempt_id: None,
            generation: String::new(),
            exit_code: None,
            reason: None,
            native_session_id: None,
            background_processes: None,
            parent_session_id: None,
            continued_from: None,
            api_credit_pool: None,
        }
    }

    fn bridge(streams: u64, queued: u64) -> Value {
        json!({"connections": {"public": {"longActive": streams, "active": 99}, "native": {"longActive": 7}, "modelQueue": {"waiting": queued}}, "sessions": [{"email": "secret@example.test"}]})
    }

    fn omp(streams: usize, queued: usize) -> OmpBusy {
        OmpBusy { known: true, streams, queued }
    }

    fn busy_with(tools: Vec<ToolBusy>, omp: OmpBusy) -> BusyReport {
        BusyReport { tools, omp }
    }

    #[test]
    fn busy_counts_split_active_from_uncertain_per_tool_and_ignore_finished_sessions() {
        let sessions = [
            session("claude", "ACTIVE"),
            session("claude", "ACTIVE"),
            session("claude", "SUSPECT"),
            session("codex", "PREPARED"),
            session("codex", "ORPHANED"),
            session("codex", "STARTING"),
            session("claude", "EXITED"),
            session("codex", "ABORTED"),
            session("codex", "FAILED"),
        ];
        let report = busy_from(&sessions, Some(&bridge(0, 0)));
        assert_eq!(report.tools, vec![
            ToolBusy { tool: "claude".into(), active: 2, uncertain: 1 },
            ToolBusy { tool: "codex".into(), active: 0, uncertain: 3 },
        ]);
        assert_eq!(report.managed(), 6);
        assert!(report.needs_consent());
        assert_eq!(busy_from(&[session("claude", "EXITED")], Some(&bridge(0, 0))), BusyReport::idle());
    }

    #[test]
    fn omp_counts_come_from_public_streams_and_the_model_queue_only_and_missing_fields_are_unknown() {
        // native long-poll(`native.longActive`)과 `active`는 응답 스트림이 아니다.
        let report = busy_from(&[], Some(&bridge(3, 2)));
        assert_eq!(report.omp, omp(3, 2));
        assert!(report.needs_consent());
        // 이 필드가 없던 서비스나 응답하지 않는 브릿지는 0이 아니라 '알 수 없음'이다.
        for missing in [None, Some(&json!({"enabled": true})), Some(&json!({"connections": {"public": {}}}))] {
            let report = busy_from(&[], missing);
            assert!(!report.omp.known);
            assert!(report.needs_consent(), "쉬고 있음을 증명하지 못하면 동의가 필요하다");
            assert!(report.describe().iter().any(|line| line.contains("확인하지 못했어요")));
        }
        assert!(!busy_from(&[], Some(&bridge(0, 0))).needs_consent());
    }

    #[test]
    fn the_busy_report_carries_counts_only() {
        let text = serde_json::to_string(&busy_from(&[session("claude", "ACTIVE")], Some(&bridge(1, 0)))).unwrap();
        for secret in ["private-account", "/private/project", "secret@example.test"] {
            assert!(!text.contains(secret), "{text}");
        }
    }

    struct Fake {
        calls: Vec<&'static str>,
        /// `busy()`가 차례로 돌려줄 읽기(마지막 값은 계속 반복). 첫 읽기가 미리보기, 이후가 배정을 닫은 뒤의 읽기다.
        reads: Vec<BusyReport>,
        /// 동의 없는 배정 닫기가 `SESSION_BUSY`로 거절한다(미리보기 뒤에 CLI 대화가 시작됨).
        gate_refuses: bool,
        /// 예전 서비스: 동의한 닫기도 permit을 주지 못한다.
        old_service: bool,
        restart_fails: bool,
        /// 재시작 뒤 서비스가 알리는 버전.
        new_version: Option<&'static str>,
        before: Option<&'static str>,
        restarted: bool,
        /// 재시작해도 시작 시각이 안 바뀌는(새 프로세스가 안 뜬) 경우.
        stuck: bool,
    }

    impl Fake {
        fn new(before: Option<&'static str>) -> Self {
            Self {
                calls: Vec::new(),
                reads: vec![BusyReport::idle()],
                gate_refuses: false,
                old_service: false,
                restart_fails: false,
                new_version: Some("0.2.0"),
                before,
                restarted: false,
                stuck: false,
            }
        }
        fn with_busy(before: Option<&'static str>, busy: BusyReport) -> Self {
            Self { reads: vec![busy], ..Self::new(before) }
        }
    }

    impl RestartHost for Fake {
        fn info(&mut self) -> Result<ServiceInfo, ApiError> {
            if self.restarted && !self.stuck {
                Ok(ServiceInfo { version: self.new_version.map(str::to_owned), started_at: 2 })
            } else {
                Ok(ServiceInfo { version: self.before.map(str::to_owned), started_at: 1 })
            }
        }
        fn busy(&mut self) -> Result<BusyReport, ApiError> {
            Ok(if self.reads.len() > 1 { self.reads.remove(0) } else { self.reads[0].clone() })
        }
        fn close_admission(&mut self, interrupt: bool) -> Result<Option<String>, ApiError> {
            self.calls.push(if interrupt { "close:force" } else { "close" });
            if !interrupt && self.gate_refuses {
                return Err(ApiError::new("SESSION_BUSY", "점유 lease"));
            }
            Ok((!(interrupt && self.old_service)).then(|| "permit".to_owned()))
        }
        fn reopen(&mut self, permit: Option<&str>) {
            assert_eq!(permit.is_some(), !self.old_service, "permit이 없으면 되돌릴 것도 없다");
            self.calls.push("reopen");
        }
        fn restart(&mut self, interrupt: bool) -> Result<(), ApiError> {
            self.calls.push(if interrupt { "restart:force" } else { "restart" });
            if self.restart_fails {
                return Err(ApiError::new("SERVICE_RESTART_FAILED", "launchctl 실패"));
            }
            self.restarted = true;
            Ok(())
        }
        fn resume(&mut self) -> Result<(), ApiError> {
            self.calls.push("resume");
            Ok(())
        }
    }

    const SHORT: Duration = Duration::from_millis(30);
    const OLD: Option<&str> = Some("0.1.1");

    #[test]
    fn restart_without_consent_is_refused_untouched_when_only_omp_streams_are_busy() {
        let mut host = Fake::with_busy(OLD, busy_with(vec![], omp(2, 0)));
        let report = restart_checked(&mut host, "0.2.0", SHORT, false).unwrap();
        assert_eq!(report.status, RestartStatus::Busy);
        assert_eq!(report.busy.omp, omp(2, 0));
        assert!(host.calls.is_empty(), "배정도 닫지 않고 재시작도 하지 않는다: {:?}", host.calls);
        // 몫을 기다리는 요청만 있어도 같다.
        let mut host = Fake::with_busy(OLD, busy_with(vec![], omp(0, 1)));
        assert_eq!(restart_checked(&mut host, "0.2.0", SHORT, false).unwrap().status, RestartStatus::Busy);
        assert!(host.calls.is_empty());
    }

    #[test]
    fn restart_without_consent_is_refused_untouched_when_only_cli_leases_are_busy() {
        let busy = busy_with(vec![ToolBusy { tool: "claude".into(), active: 1, uncertain: 0 }], omp(0, 0));
        let mut host = Fake::with_busy(OLD, busy.clone());
        let report = restart_checked(&mut host, "0.2.0", SHORT, false).unwrap();
        assert_eq!((report.status, report.busy), (RestartStatus::Busy, busy));
        assert!(host.calls.is_empty(), "{:?}", host.calls);
    }

    #[test]
    fn consented_restart_runs_close_restart_resume_verifies_the_version_and_reports_what_it_cut() {
        let busy = busy_with(vec![ToolBusy { tool: "codex".into(), active: 1, uncertain: 1 }], omp(1, 1));
        let mut host = Fake::with_busy(OLD, busy.clone());
        let report = restart_checked(&mut host, "0.2.0", SHORT, true).unwrap();
        assert_eq!(host.calls, ["close:force", "restart:force", "resume"]);
        assert_eq!(report.status, RestartStatus::Restarted);
        assert_eq!(report.busy, busy, "끊은 작업을 돌려줘야 화면이 안내할 수 있다");
        assert!(report.message.contains("0.2.0"), "{}", report.message);
    }

    #[test]
    fn consented_restart_reports_the_work_found_after_admission_closed_not_the_stale_preview() {
        let mut host = Fake::new(OLD);
        host.reads = vec![busy_with(vec![], omp(1, 0)), busy_with(vec![], omp(3, 0))];
        let report = restart_checked(&mut host, "0.2.0", SHORT, true).unwrap();
        assert_eq!(report.busy.omp, omp(3, 0));
    }

    #[test]
    fn nothing_busy_restarts_without_asking_through_the_atomic_gate() {
        let mut host = Fake::new(None);
        let report = restart_checked(&mut host, "0.2.0", SHORT, false).unwrap();
        assert_eq!(host.calls, ["close", "restart", "resume"]);
        assert_eq!(report.status, RestartStatus::Restarted);
        assert!(!report.busy.needs_consent());
        assert!(report.message.contains("0.2.0"), "{}", report.message);
    }

    #[test]
    fn unknown_omp_state_needs_consent_and_consent_proceeds() {
        let unknown = OmpBusy { known: false, streams: 0, queued: 0 };
        let mut host = Fake::with_busy(OLD, busy_with(vec![], unknown.clone()));
        let refused = restart_checked(&mut host, "0.2.0", SHORT, false).unwrap();
        assert_eq!((refused.status, refused.busy.omp.known), (RestartStatus::Busy, false));
        assert!(host.calls.is_empty());
        let report = restart_checked(&mut host, "0.2.0", SHORT, true).unwrap();
        assert_eq!(report.status, RestartStatus::Restarted);
        assert!(!report.busy.omp.known, "알 수 없음은 0으로 바꿔 말하지 않는다");
    }

    #[test]
    fn old_service_without_the_restart_parameter_still_restarts_when_consented() {
        let mut host = Fake::with_busy(OLD, busy_with(vec![ToolBusy { tool: "claude".into(), active: 1, uncertain: 0 }], omp(1, 0)));
        host.old_service = true;
        let report = restart_checked(&mut host, "0.2.0", SHORT, true).unwrap();
        assert_eq!(host.calls, ["close:force", "restart:force", "resume"]);
        assert_eq!(report.status, RestartStatus::Restarted);
        // 재시작이 실패해도 permit이 없으니 되돌릴 배정 차단이 없다(Fake가 permit 유무를 확인한다).
        let mut host = Fake::with_busy(OLD, busy_with(vec![], omp(1, 0)));
        host.old_service = true;
        host.restart_fails = true;
        assert_eq!(restart_checked(&mut host, "0.2.0", SHORT, true).unwrap_err().code, "SERVICE_RESTART_FAILED");
        assert_eq!(host.calls, ["close:force", "restart:force", "reopen"]);
    }

    #[test]
    fn a_cli_lease_started_after_the_preview_is_refused_by_the_gate_without_changes() {
        let late = busy_with(vec![ToolBusy { tool: "claude".into(), active: 0, uncertain: 1 }], omp(0, 0));
        let mut host = Fake::new(OLD);
        // 미리보기는 한가했고, 그 뒤 시작된 대화를 같은 트랜잭션 검사가 거절한다.
        host.reads = vec![BusyReport::idle(), late.clone()];
        host.gate_refuses = true;
        let report = restart_checked(&mut host, "0.2.0", SHORT, false).unwrap();
        assert_eq!((report.status, report.busy), (RestartStatus::Busy, late));
        assert_eq!(host.calls, ["close"], "거절된 뒤에는 재시작도 되돌림도 없다");
    }

    #[test]
    fn an_omp_stream_that_starts_after_admission_closed_is_not_cut_without_consent() {
        let mut host = Fake::new(OLD);
        host.reads = vec![BusyReport::idle(), busy_with(vec![], omp(1, 0))];
        let report = restart_checked(&mut host, "0.2.0", SHORT, false).unwrap();
        assert_eq!((report.status, report.busy.omp), (RestartStatus::Busy, omp(1, 0)));
        assert_eq!(host.calls, ["close", "reopen"], "재시작하지 않고 닫은 배정만 다시 연다");
    }

    #[test]
    fn restart_leaves_a_newer_service_alone() {
        let mut host = Fake::new(Some("0.3.0"));
        let report = restart_checked(&mut host, "0.2.0", SHORT, true).unwrap();
        assert!(host.calls.is_empty(), "{:?}", host.calls);
        assert_eq!(report.status, RestartStatus::Unchanged);
        assert!(report.message.contains("새 버전"), "{}", report.message);
    }

    #[test]
    fn restart_does_nothing_when_the_service_already_matches_even_if_forced() {
        let mut host = Fake::with_busy(Some("0.2.0"), busy_with(vec![], omp(5, 0)));
        let report = restart_checked(&mut host, "0.2.0", SHORT, true).unwrap();
        assert!(host.calls.is_empty(), "{:?}", host.calls);
        assert_eq!(report.status, RestartStatus::Unchanged);
        assert!(report.message.contains("다시 시작하지 않았"), "{}", report.message);
    }

    #[test]
    fn failed_restart_reopens_admission_and_keeps_the_old_service() {
        let mut host = Fake::new(OLD);
        host.restart_fails = true;
        let error = restart_checked(&mut host, "0.2.0", SHORT, false).unwrap_err();
        assert_eq!(error.code, "SERVICE_RESTART_FAILED");
        assert_eq!(host.calls, ["close", "restart", "reopen"]);
    }

    #[test]
    fn restarted_service_with_the_wrong_version_is_reported_not_called_success() {
        let mut host = Fake::new(OLD);
        host.new_version = Some("0.1.1");
        let error = restart_checked(&mut host, "0.2.0", SHORT, false).unwrap_err();
        assert_eq!(error.code, "SERVICE_VERSION_STALE");
        assert_eq!(host.calls, ["close", "restart", "resume"]);
    }

    #[test]
    fn unconfirmed_new_process_is_not_success() {
        let mut host = Fake::new(OLD);
        host.stuck = true;
        let error = restart_checked(&mut host, "0.2.0", SHORT, false).unwrap_err();
        assert_eq!(error.code, "SERVICE_NOT_READY");
        assert_eq!(host.calls, ["close", "restart", "resume"]);
    }

    #[test]
    fn cli_refuses_with_a_summary_and_names_the_force_flag_and_guides_after_a_forced_restart() {
        let busy = busy_with(vec![ToolBusy { tool: "claude".into(), active: 1, uncertain: 0 }], omp(2, 0));
        let error = RestartReport::busy(busy.clone()).cli_text().unwrap_err();
        assert_eq!(error.code, "SESSION_BUSY");
        assert!(error.message.contains("omp 응답 2개") && error.message.contains("Claude 터미널 대화 1개"), "{}", error.message);
        assert!(error.message.contains("--force"), "{}", error.message);
        let done = RestartReport { status: RestartStatus::Restarted, message: "끝".into(), busy }.cli_text().unwrap();
        assert!(done.contains("/ojak-resume") && done.contains("다시 보내지 말고"), "{done}");
        // 끊은 것이 없으면 안내를 덧붙이지 않는다.
        let quiet = RestartReport { status: RestartStatus::Restarted, message: "끝".into(), busy: BusyReport::idle() }.cli_text().unwrap();
        assert_eq!(quiet, "끝");
    }
}
