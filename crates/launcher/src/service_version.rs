//! 앱과 실행 중인 서비스의 버전 비교, 그리고 둘을 맞추는 안전한 서비스 재시작.
//!
//! 앱을 DMG로 덮어쓰면 화면은 새 버전인데 `aam-service`는 예전 프로세스로 남을 수 있다. 서비스가 알려 주는
//! 버전(`Snapshot.service_version`)을 앱에 묶인 `aam`의 버전과 비교해 알려 준다. 이 필드가 없던 이전 서비스는
//! '알 수 없음'이고 재시작 대상이다. 조용히 재시작하지 않는다. 사용자가 누를 때만 아래 순서로 돌린다.
//!
//! 1. 서비스의 버전·시작 시각을 읽는다. 이미 같은 버전이면 아무것도 하지 않는다.
//! 2. 새 배정을 닫고 점유 lease가 없는지 한 번에 확인한다(`service.prepareUninstall`). 불확실하거나 쓰는 중이면
//!    아무것도 바꾸지 않고 멈춘다. 시간 초과만으로 lease를 풀지 않는다.
//! 3. 서비스를 다시 시작한다. 시작이 실패하면 배정을 다시 연다.
//! 4. 새 프로세스(시작 시각이 바뀜)가 응답하면 배정을 다시 열고, 버전이 앱과 같은지 확인한다.
use aam_protocol::{ApiError, Paths, Snapshot};
use serde::Serialize;
use serde_json::json;
use std::{
    cmp::Ordering,
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
    /// 새 배정을 닫고 점유 lease가 없음을 확인한다. 쓰는 중이거나 불확실하면 오류(아무것도 바꾸지 않음).
    fn close_admission(&mut self) -> Result<String, ApiError>;
    /// 닫은 배정을 다시 연다(재시작이 시작되지 않았을 때). 실패해도 오류로 알리지 않는다.
    fn reopen(&mut self, permit: &str);
    fn restart(&mut self) -> Result<(), ApiError>;
    /// 새 서비스가 응답한 뒤 닫아 둔 배정을 다시 연다.
    fn resume(&mut self) -> Result<(), ApiError>;
}

pub fn restart_checked(host: &mut dyn RestartHost, app_version: &str, wait: Duration) -> Result<String, ApiError> {
    let before = host.info()?;
    let state = compare(app_version, before.version.as_deref());
    if state == ServiceVersion::Current {
        return Ok(format!("서비스가 이미 앱과 같은 버전({app_version})이라 다시 시작하지 않았어요."));
    }
    if state == ServiceVersion::Newer {
        return Ok("서비스가 앱보다 새 버전이라 다시 시작하지 않았어요. 앱을 최신 버전으로 설치해 주세요.".into());
    }
    let permit = host.close_admission().map_err(|error| {
        if error.code == "SESSION_BUSY" {
            ApiError::new(
                "SESSION_BUSY",
                "실행 중이거나 시작 결과가 불확실한 대화가 있어서 서비스를 다시 시작하지 않았어요. 대화를 끝낸 뒤 다시 시도해 주세요.",
            )
        } else {
            error
        }
    })?;
    if let Err(error) = host.restart() {
        host.reopen(&permit);
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
    Ok(format!("서비스를 안전하게 다시 시작했어요. 이제 버전 {app_version}이에요."))
}

/// 실제 서비스를 대상으로 하는 단계. 점유 lease 검사는 서비스의 `service.prepareUninstall`이 한 트랜잭션으로 한다.
struct SystemHost<'a>(&'a Paths);

impl RestartHost for SystemHost<'_> {
    fn info(&mut self) -> Result<ServiceInfo, ApiError> {
        Ok(ServiceInfo::from_snapshot(&crate::read_snapshot(self.0)?))
    }
    fn close_admission(&mut self) -> Result<String, ApiError> {
        prepare_permit(self.0)
    }
    fn reopen(&mut self, permit: &str) {
        let _ = aam_protocol::call(self.0, "service.cancelUninstall", json!({ "permit": permit }));
    }
    fn restart(&mut self) -> Result<(), ApiError> {
        #[cfg(unix)]
        return crate::install::service_kickstart(self.0);
        // Windows: 안전 종료(같은 lease 검사) 뒤 기록된 실행 파일로 다시 시작한다. 앱이 등록한 서비스가 아니면 아무것도
        // 하지 않으므로 아래 확인 단계가 시작 시각이 안 바뀐 것을 `SERVICE_NOT_READY`로 알린다.
        #[cfg(windows)]
        return crate::install::service_restart(self.0);
    }
    fn resume(&mut self) -> Result<(), ApiError> {
        aam_protocol::call(self.0, "service.resumeAdmission", json!({})).map(|_| ())
    }
}

/// 앱 버전과 다른 서비스를 안전하게 다시 시작한다. 사용자가 명령·버튼으로 요청했을 때만 부른다.
pub fn restart(paths: &Paths) -> Result<String, ApiError> {
    restart_checked(&mut SystemHost(paths), APP_VERSION, Duration::from_secs(20))
}

/// 점유 lease가 없을 때만 permit을 돌려주는 `service.prepareUninstall` 호출.
fn prepare_permit(paths: &Paths) -> Result<String, ApiError> {
    let prepared = aam_protocol::call(paths, "service.prepareUninstall", json!({}))?;
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

    struct Fake {
        calls: Vec<&'static str>,
        busy: bool,
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
            Self { calls: Vec::new(), busy: false, restart_fails: false, new_version: Some("0.2.0"), before, restarted: false, stuck: false }
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
        fn close_admission(&mut self) -> Result<String, ApiError> {
            self.calls.push("close");
            if self.busy { Err(ApiError::new("SESSION_BUSY", "점유 lease")) } else { Ok("permit".into()) }
        }
        fn reopen(&mut self, permit: &str) {
            assert_eq!(permit, "permit");
            self.calls.push("reopen");
        }
        fn restart(&mut self) -> Result<(), ApiError> {
            self.calls.push("restart");
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

    #[test]
    fn restart_is_refused_without_touching_the_service_while_leases_are_active() {
        let mut host = Fake::new(Some("0.1.1"));
        host.busy = true;
        let error = restart_checked(&mut host, "0.2.0", SHORT).unwrap_err();
        assert_eq!(error.code, "SESSION_BUSY");
        assert!(error.message.contains("다시 시도"), "{}", error.message);
        assert_eq!(host.calls, ["close"], "배정을 닫는 검사 뒤에는 아무 단계도 실행하지 않는다");
    }

    #[test]
    fn restart_runs_gate_then_restart_then_resume_and_verifies_the_version() {
        let mut host = Fake::new(None);
        let message = restart_checked(&mut host, "0.2.0", SHORT).unwrap();
        assert_eq!(host.calls, ["close", "restart", "resume"]);
        assert!(message.contains("0.2.0"), "{message}");
    }

    #[test]
    fn restart_leaves_a_newer_service_alone() {
        let mut host = Fake::new(Some("0.3.0"));
        let message = restart_checked(&mut host, "0.2.0", SHORT).unwrap();
        assert!(host.calls.is_empty(), "{:?}", host.calls);
        assert!(message.contains("새 버전"), "{message}");
    }

    #[test]
    fn restart_does_nothing_when_the_service_already_matches() {
        let mut host = Fake::new(Some("0.2.0"));
        let message = restart_checked(&mut host, "0.2.0", SHORT).unwrap();
        assert!(host.calls.is_empty(), "{:?}", host.calls);
        assert!(message.contains("다시 시작하지 않았"), "{message}");
    }

    #[test]
    fn failed_restart_reopens_admission_and_keeps_the_old_service() {
        let mut host = Fake::new(Some("0.1.1"));
        host.restart_fails = true;
        let error = restart_checked(&mut host, "0.2.0", SHORT).unwrap_err();
        assert_eq!(error.code, "SERVICE_RESTART_FAILED");
        assert_eq!(host.calls, ["close", "restart", "reopen"]);
    }

    #[test]
    fn restarted_service_with_the_wrong_version_is_reported_not_called_success() {
        let mut host = Fake::new(Some("0.1.1"));
        host.new_version = Some("0.1.1");
        let error = restart_checked(&mut host, "0.2.0", SHORT).unwrap_err();
        assert_eq!(error.code, "SERVICE_VERSION_STALE");
        assert_eq!(host.calls, ["close", "restart", "resume"]);
    }

    #[test]
    fn unconfirmed_new_process_is_not_success() {
        let mut host = Fake::new(Some("0.1.1"));
        host.stuck = true;
        let error = restart_checked(&mut host, "0.2.0", SHORT).unwrap_err();
        assert_eq!(error.code, "SERVICE_NOT_READY");
        assert_eq!(host.calls, ["close", "restart", "resume"]);
    }
}
