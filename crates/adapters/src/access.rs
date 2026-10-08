//! Access-only handoff. Official CLIs retain storage and refresh-token ownership.
#[path = "access_claude.rs"]
mod claude;
#[path = "access_codex.rs"]
mod codex;

use aam_protocol::{Account, ApiError};
use std::path::Path;

// Deliberately neither Debug nor Serialize: callers must explicitly select wire fields.
pub struct NativeAccess {
    pub access: String,
    pub expires: i64,
    pub account_id: Option<String>,
}

pub fn native_access(account: &Account, refresh: bool) -> Result<NativeAccess, ApiError> {
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
    let profile = account
        .profile_path
        .as_deref()
        .map(Path::new)
        .and_then(|path| path.canonicalize().ok())
        .ok_or_else(|| {
            ApiError::new(
                "PROFILE_UNVERIFIED",
                "공식 CLI 인증 폴더를 확인하지 못했어요.",
            )
        })?;
    let binary = account
        .binary_path
        .as_deref()
        .map(Path::new)
        .and_then(|path| path.canonicalize().ok())
        .ok_or_else(|| {
            ApiError::new(
                "CLI_NOT_FOUND",
                "원래 공식 CLI 실행 파일을 확인하지 못했어요.",
            )
        })?;
    let verify = || -> Result<(), ApiError> {
        let actual = crate::native::inspect(&account.tool, &profile, &binary, &account.label)?;
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
    verify()?;
    let result = match account.tool.as_str() {
        "claude" => claude::load(account, refresh),
        "codex" => codex::load(account, refresh),
        _ => Err(ApiError::new(
            "TOOL_UNSUPPORTED",
            "이 공급자는 omp의 원래 공급자 로그인으로 연결해 주세요.",
        )),
    }?;
    verify()?;
    if result.access.is_empty() || result.expires <= aam_protocol::now_ms() {
        return Err(ApiError::new(
            "AUTH_REQUIRED",
            "공식 CLI에서 유효한 구독 인증을 확인하지 못했어요.",
        ));
    }
    Ok(result)
}
