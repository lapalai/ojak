//! Access-only handoff. Official CLIs retain storage and refresh-token ownership.
#[path = "access_claude.rs"]
mod claude;
#[path = "access_codex.rs"]
mod codex;

use aam_protocol::{Account, ApiError};
use std::time::Instant;

// Deliberately neither Debug nor Serialize: callers must explicitly select wire fields.
pub struct NativeAccess {
    pub access: String,
    pub expires: i64,
    pub account_id: Option<String>,
}

/// 각 단계의 이름은 고정된 허용 목록이다(토큰·경로·CLI 출력은 담지 않는다). 오류가 나면 마지막으로 알린 단계가 실패한 단계다.
/// `binding`(프로필·바이너리 연결 해석) → `identity-check`(실행 전 공식 CLI 신원 확인) → `auth-context`(Claude만: 저장소·OAuth 대상을 바꾸는 환경 변수 검사) → `credential-read`(공식 저장소 읽기)
/// → `cli-refresh`(공식 CLI로 갱신, 필요할 때만) → `credential-reread`(갱신 뒤 저장소 다시 읽기)
/// → `identity-recheck`(실행 뒤 신원 재확인) → `result-check`(결과 검증).
pub const STAGES: [&str; 8] = [
    "binding",
    "identity-check",
    "auth-context",
    "credential-read",
    "cli-refresh",
    "credential-reread",
    "identity-recheck",
    "result-check",
];

pub fn native_access(account: &Account, refresh: bool, deadline: Instant) -> Result<NativeAccess, ApiError> {
    native_access_staged(account, refresh, deadline, &mut |_| {})
}

/// `native_access`와 같지만 실제로 시작하는 단계마다 `stage`로 알린다. 단계 이름은 `STAGES`의 값뿐이다.
pub fn native_access_staged(
    account: &Account,
    refresh: bool,
    deadline: Instant,
    stage: &mut dyn FnMut(&'static str),
) -> Result<NativeAccess, ApiError> {
    stage("binding");
    if !account.enabled {
        return Err(ApiError::new(
            "ACCOUNT_DISABLED",
            "이 계정의 배정이 꺼져 있어요.",
        ));
    }
    if account.auth_status != "authenticated" || account.identity_key.is_none() {
        return Err(ApiError::new(
            "AUTH_REQUIRED",
            "사용할 수 있는 공식 CLI 로그인이 없어요.",
        ));
    }
    ensure_deadline(deadline)?;
    let (profile, binary) = crate::native::bound_paths(account)?;
    let verify = |name: &'static str, stage: &mut dyn FnMut(&'static str)| -> Result<(), ApiError> {
        stage(name);
        ensure_deadline(deadline)?;
        let actual = crate::native::inspect_identity_until(&account.tool, &profile, &binary, &account.label, deadline)?;
        if actual.auth_status != "authenticated" {
            return Err(ApiError::new(
                "AUTH_REQUIRED",
                "공식 CLI의 로그인이 더 이상 유효하지 않아요.",
            ));
        }
        if actual.identity_key != account.identity_key {
            return Err(ApiError::new(
                "PROFILE_IDENTITY_MISMATCH",
                "공식 CLI의 로그인 계정이 바뀌었어요. Ojak에서 계정을 새로고침해 주세요.",
            ));
        }
        Ok(())
    };
    verify("identity-check", stage)?;
    let result = match account.tool.as_str() {
        "claude" => claude::load(account, refresh, deadline, stage),
        "codex" => codex::load(account, refresh, deadline, stage),
        _ => Err(ApiError::new(
            "TOOL_UNSUPPORTED",
            "이 공급자는 omp의 원래 공급자 로그인으로 연결해 주세요.",
        )),
    }?;
    verify("identity-recheck", stage)?;
    stage("result-check");
    ensure_deadline(deadline)?;
    if result.access.is_empty() || result.expires <= aam_protocol::now_ms() {
        return Err(ApiError::new(
            "AUTH_REQUIRED",
            "공식 CLI에서 유효한 구독 인증을 확인하지 못했어요.",
        ));
    }
    Ok(result)
}

fn ensure_deadline(deadline: Instant) -> Result<(), ApiError> {
    if Instant::now() >= deadline {
        Err(ApiError::new("AUTH_REFRESH_TIMEOUT", "공식 CLI 인증 확인·갱신 작업의 제한 시간이 지났어요."))
    } else {
        Ok(())
    }
}
