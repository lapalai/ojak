use super::NativeAccess;
use aam_protocol::{now_ms, Account, ApiError};
use serde::Deserialize;
#[cfg(target_os = "macos")]
use sha2::{Digest, Sha256};
use std::{io::Read, path::Path};
#[cfg(target_os = "macos")]
use unicode_normalization::UnicodeNormalization;

#[derive(Deserialize)]
struct Store {
    #[serde(rename = "claudeAiOauth")]
    oauth: Option<OAuth>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OAuth {
    access_token: String,
    expires_at: i64,
}

fn parse(bytes: &[u8]) -> Result<NativeAccess, ApiError> {
    let store: Store = serde_json::from_slice(bytes).map_err(|_| {
        ApiError::new(
            "NATIVE_CREDENTIAL_FORMAT",
            "Claude 인증 저장소 형식을 확인하지 못했어요.",
        )
    })?;
    let oauth = store
        .oauth
        .filter(|v| !v.access_token.is_empty())
        .ok_or_else(|| ApiError::new("AUTH_REQUIRED", "Claude 구독 로그인이 없어요."))?;
    Ok(NativeAccess {
        access: oauth.access_token,
        expires: oauth.expires_at,
        account_id: None,
    })
}

fn file(profile: &Path) -> Result<NativeAccess, ApiError> {
    let file = aam_protocol::secure::open_read_no_follow(&profile.join(".credentials.json"))
        .map_err(|_| {
            ApiError::new(
                "NATIVE_CREDENTIAL_READ",
                "Claude 인증 파일을 읽지 못했어요. 파일 권한과 공식 CLI 로그인을 확인해 주세요.",
            )
        })?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            ApiError::new(
                "NATIVE_CREDENTIAL_READ",
                "Claude 인증 파일을 읽지 못했어요.",
            )
        })?;
    if bytes.len() > 1024 * 1024 {
        return Err(ApiError::new(
            "NATIVE_CREDENTIAL_FORMAT",
            "Claude 인증 파일이 예상한 크기를 넘었어요.",
        ));
    }
    parse(&bytes)
}

#[cfg(target_os = "macos")]
fn service_name(config_dir: Option<&str>) -> String {
    match config_dir.filter(|s| !s.is_empty()) {
        None => "Claude Code-credentials".into(),
        Some(dir) => {
            let normalized: String = dir.nfc().collect();
            let digest = Sha256::digest(normalized.as_bytes());
            format!(
                "Claude Code-credentials-{:02x}{:02x}{:02x}{:02x}",
                digest[0], digest[1], digest[2], digest[3]
            )
        }
    }
}

fn read(profile: &Path) -> Result<NativeAccess, ApiError> {
    #[cfg(target_os = "macos")]
    {
        let env = crate::native::profile_env("claude", profile);
        let service = service_name(env.get("CLAUDE_CONFIG_DIR").map(String::as_str));
        let user = match std::env::var("USER") {
            Ok(user) => user,
            Err(_) => {
                let (status, bytes) = crate::process::Probe::spawn(
                    Path::new("/usr/bin/id"),
                    &["-un"],
                    &env,
                    profile,
                )?
                .output_bytes()?;
                if !status.success() {
                    return Err(ApiError::new(
                        "NATIVE_CREDENTIAL_READ",
                        "키체인 사용자 계정을 확인하지 못했어요.",
                    ));
                }
                String::from_utf8(bytes)
                    .map_err(|_| {
                        ApiError::new(
                            "NATIVE_CREDENTIAL_READ",
                            "키체인 사용자 계정을 확인하지 못했어요.",
                        )
                    })?
                    .trim()
                    .to_owned()
            }
        };
        let user = if !user.is_empty()
            && user
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        {
            user.as_str()
        } else {
            "claude-code-user"
        };
        let (status, bytes) = crate::process::Probe::spawn(
            Path::new("/usr/bin/security"),
            &["find-generic-password", "-a", user, "-w", "-s", &service],
            &env,
            profile,
        )?
        .output_bytes()?;
        if status.success() {
            return parse(&bytes);
        }
        // errSecItemNotFound only. A locked/denied keychain must not silently
        // select an older plaintext fallback belonging to a different login.
        if status.code() != Some(44) {
            return Err(ApiError::new(
                "NATIVE_KEYCHAIN_LOCKED",
                "Claude 키체인 인증을 읽지 못했어요. macOS 키체인 접근 상태를 확인해 주세요.",
            ));
        }
    }
    file(profile)
}

pub(super) fn load(account: &Account, refresh: bool) -> Result<NativeAccess, ApiError> {
    if [
        "CLAUDE_SECURESTORAGE_CONFIG_DIR",
        "CLAUDE_CODE_CUSTOM_OAUTH_URL",
        "CLAUDE_LOCAL_OAUTH_API_BASE",
        "CLAUDE_CODE_OAUTH_CLIENT_ID",
    ]
    .iter()
    .any(|key| std::env::var_os(key).is_some())
    {
        return Err(ApiError::new(
            "AUTH_OVERRIDE_CONFLICT",
            "Claude 인증 저장소나 OAuth 대상을 바꾸는 환경 설정이 있어 연결을 멈췄어요.",
        ));
    }
    let profile = Path::new(
        account
            .profile_path
            .as_deref()
            .ok_or_else(|| ApiError::new("PROFILE_UNVERIFIED", "Claude 프로필 경로가 없어요."))?,
    )
    .canonicalize()
    .map_err(|_| ApiError::new("PROFILE_UNVERIFIED", "Claude 프로필을 확인하지 못했어요."))?;
    let access = read(&profile)?;
    if !refresh && access.expires > now_ms() + 60_000 {
        return Ok(access);
    }
    let binary = crate::process::executable(Path::new(
        account
            .binary_path
            .as_deref()
            .ok_or_else(|| ApiError::new("CLI_NOT_FOUND", "Claude 실행 경로가 없어요."))?,
    ))?;
    // The native /usage command calls the authenticated usage API with
    // refreshOAuth enabled and the CLI's cross-process OAuth refresh lock.
    // No inference, tools, hooks, or third-party refresh-token writer.
    let value = crate::process::run_json(
        &binary,
        &[
            "-p",
            "/usage",
            "--safe-mode",
            "--tools",
            "",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--output-format",
            "json",
        ],
        &crate::native::profile_env("claude", &profile),
        &profile,
    )?;
    if value["is_error"] == true || value["num_turns"].as_u64() != Some(0) {
        return Err(ApiError::new(
            "NATIVE_AUTH_UNAVAILABLE",
            "Claude의 공식 인증 갱신을 확인하지 못했어요.",
        ));
    }
    let renewed = read(&profile)?;
    if renewed.expires <= now_ms() {
        return Err(ApiError::new(
            "AUTH_REQUIRED",
            "Claude 공식 CLI가 인증을 갱신하지 못했어요.",
        ));
    }
    Ok(renewed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parser_rejects_api_key_only_and_missing_expiry() {
        assert!(parse(br#"{"apiKey":"not-a-subscription"}"#).is_err());
        assert!(parse(br#"{"claudeAiOauth":{"accessToken":"access"}}"#).is_err());
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_profiles_use_native_nfc_normalization() {
        assert_eq!(
            service_name(Some("/tmp/caf\u{e9}")),
            service_name(Some("/tmp/cafe\u{301}"))
        );
        assert_ne!(service_name(Some("/tmp/a")), service_name(Some("/tmp/b")));
        assert_ne!(service_name(None), service_name(Some("/tmp/a")));
    }
}
