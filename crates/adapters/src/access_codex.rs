use super::NativeAccess;
use aam_protocol::{now_ms, Account, ApiError};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::Deserialize;
use serde_json::json;
use std::{io::Read, path::Path};

#[derive(Deserialize)]
struct Store {
    auth_mode: Option<String>,
    #[serde(rename = "OPENAI_API_KEY")]
    api_key: Option<String>,
    tokens: Option<Tokens>,
}
#[derive(Deserialize)]
struct Tokens {
    access_token: String,
    account_id: Option<String>,
}
#[derive(Deserialize)]
struct Claims {
    exp: i64,
    #[serde(rename = "https://api.openai.com/auth")]
    auth: Option<AuthClaims>,
}
#[derive(Deserialize)]
struct AuthClaims {
    chatgpt_account_id: Option<String>,
}

fn parse(bytes: &[u8]) -> Result<NativeAccess, ApiError> {
    let store: Store = serde_json::from_slice(bytes).map_err(|_| format_error())?;
    if store.api_key.is_some_and(|key| !key.is_empty())
        || store
            .auth_mode
            .as_deref()
            .is_some_and(|mode| mode != "chatgpt")
    {
        return Err(ApiError::new(
            "AUTH_OVERRIDE_CONFLICT",
            "Codex가 ChatGPT 구독이 아닌 인증을 사용하고 있어요.",
        ));
    }
    let tokens = store
        .tokens
        .ok_or_else(|| ApiError::new("AUTH_REQUIRED", "Codex 구독 로그인이 없어요."))?;
    let mut parts = tokens.access_token.split('.');
    let _header = parts.next().ok_or_else(format_error)?;
    let payload = parts.next().ok_or_else(format_error)?;
    if parts.next().is_none() || parts.next().is_some() {
        return Err(format_error());
    }
    let claims: Claims = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| format_error())?,
    )
    .map_err(|_| format_error())?;
    let claimed = claims.auth.and_then(|auth| auth.chatgpt_account_id);
    if let (Some(stored), Some(claimed)) = (&tokens.account_id, &claimed) {
        if stored != claimed {
            return Err(ApiError::new(
                "PROFILE_IDENTITY_MISMATCH",
                "Codex 인증의 워크스페이스가 일치하지 않아요.",
            ));
        }
    }
    let account_id = tokens
        .account_id
        .or(claimed)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                "PROFILE_UNVERIFIED",
                "Codex 구독 워크스페이스를 확인하지 못했어요.",
            )
        })?;
    let expires = claims.exp.checked_mul(1000).ok_or_else(format_error)?;
    Ok(NativeAccess {
        access: tokens.access_token,
        expires,
        account_id: Some(account_id),
    })
}
fn format_error() -> ApiError {
    ApiError::new(
        "NATIVE_CREDENTIAL_FORMAT",
        "Codex 인증 저장소 형식을 확인하지 못했어요.",
    )
}
fn bytes(path: &Path) -> Result<Vec<u8>, ApiError> {
    let file = aam_protocol::secure::open_read_no_follow(path)
        .map_err(|_| ApiError::new("NATIVE_CREDENTIAL_READ", "Codex 인증 저장소를 읽지 못했어요. 원래 프로필의 파일 권한과 로그인 상태를 확인해 주세요."))?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| format_error())?;
    if bytes.len() > 1024 * 1024 {
        return Err(format_error());
    }
    Ok(bytes)
}
fn read(profile: &Path) -> Result<NativeAccess, ApiError> {
    let config_path = profile.join("config.toml");
    if config_path.exists() {
        let config: toml::Value =
            toml::from_str(std::str::from_utf8(&bytes(&config_path)?).map_err(|_| format_error())?)
                .map_err(|_| format_error())?;
        // The currently registered Codex profiles are file-backed. Never read a
        // stale auth.json when the CLI is explicitly using another backend.
        if config
            .get("cli_auth_credentials_store")
            .and_then(toml::Value::as_str)
            .is_some_and(|mode| mode != "file")
        {
            return Err(ApiError::new("NATIVE_STORAGE_UNSUPPORTED", "이 Codex 프로필은 파일이 아닌 인증 저장소를 사용해요. 저장 방식을 자동 변경하거나 다른 인증 파일로 대체하지 않아요."));
        }
    }
    parse(&bytes(&profile.join("auth.json"))?)
}

pub(super) fn load(account: &Account, refresh: bool) -> Result<NativeAccess, ApiError> {
    let profile = Path::new(
        account
            .profile_path
            .as_deref()
            .ok_or_else(|| ApiError::new("PROFILE_UNVERIFIED", "Codex 프로필 경로가 없어요."))?,
    )
    .canonicalize()
    .map_err(|_| ApiError::new("PROFILE_UNVERIFIED", "Codex 프로필을 확인하지 못했어요."))?;
    let access = read(&profile)?;
    let bound = account
        .identity_key
        .as_deref()
        .and_then(|key| {
            key.split('|')
                .find_map(|part| part.strip_prefix("workspace:"))
        })
        .filter(|s| !s.is_empty());
    if bound.is_some_and(|id| access.account_id.as_deref() != Some(id)) {
        return Err(ApiError::new(
            "PROFILE_IDENTITY_MISMATCH",
            "Codex 로그인 워크스페이스가 등록된 계정과 달라요.",
        ));
    }
    if !refresh && access.expires > now_ms() + 60_000 {
        return Ok(access);
    }
    let binary = crate::process::executable(Path::new(
        account
            .binary_path
            .as_deref()
            .ok_or_else(|| ApiError::new("CLI_NOT_FOUND", "Codex 실행 경로가 없어요."))?,
    ))?;
    let mut server = crate::process::Probe::spawn(
        &binary,
        &["app-server", "--listen", "stdio://"],
        &crate::native::profile_env("codex", &profile),
        &profile,
    )?;
    let initialized = server.request(1, "initialize", json!({"clientInfo":{"name":"aam_auth","title":"Ojak","version":env!("CARGO_PKG_VERSION")},"capabilities":{"experimentalApi":true}}))?;
    let reported = initialized["codexHome"]
        .as_str()
        .and_then(|home| Path::new(home).canonicalize().ok());
    if reported.as_deref() != Some(profile.as_path()) {
        return Err(ApiError::new(
            "PROFILE_IDENTITY_MISMATCH",
            "Codex가 다른 인증 프로필을 사용하고 있어요.",
        ));
    }
    server.send(&json!({"method":"initialized"}))?;
    let result = server.request(2, "account/read", json!({"refreshToken":true}))?;
    if result["account"]["type"] != "chatgpt" {
        return Err(ApiError::new(
            "AUTH_REQUIRED",
            "Codex 공식 CLI에서 구독 로그인을 갱신하지 못했어요.",
        ));
    }
    let renewed = read(&profile)?;
    if renewed.account_id != access.account_id {
        return Err(ApiError::new(
            "PROFILE_IDENTITY_MISMATCH",
            "Codex 인증 갱신 중 워크스페이스가 바뀌었어요.",
        ));
    }
    if renewed.expires <= now_ms() {
        return Err(ApiError::new(
            "AUTH_REQUIRED",
            "Codex 공식 CLI가 인증을 갱신하지 못했어요.",
        ));
    }
    Ok(renewed)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn credential(exp: i64, stored: &str, claimed: &str) -> Vec<u8> {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(
                &json!({"exp":exp,"https://api.openai.com/auth":{"chatgpt_account_id":claimed}}),
            )
            .unwrap(),
        );
        serde_json::to_vec(&json!({"auth_mode":"chatgpt","tokens":{"access_token":format!("header.{payload}.signature"),"account_id":stored}})).unwrap()
    }
    #[test]
    fn token_workspace_mismatch_and_expiry_overflow_are_rejected() {
        assert!(parse(&credential(100, "a", "b")).is_err());
        assert!(parse(&credential(i64::MAX, "a", "a")).is_err());
        assert_eq!(parse(&credential(123, "a", "a")).unwrap().expires, 123_000);
    }
}
