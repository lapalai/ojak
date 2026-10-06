use aam_protocol::ApiError;
use serde_json::Value;
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    name.len() <= 128 && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn display_key(key: &str) -> String {
    let safe = !key.is_empty()
        && key.len() <= 64
        && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && !key.to_ascii_lowercase().starts_with("sk-");
    if safe { key.to_string() } else { "(이름을 표시할 수 없는 키)".into() }
}

fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    if text.is_empty() || text.len() > 300 || text.chars().any(char::is_control) {
        "(경로를 표시할 수 없는 파일)".into()
    } else {
        text
    }
}

/// 변수 이름만 알린다. 값은 어떤 경우에도 넣지 않는다.
fn env_conflict(name: &str) -> ApiError {
    let (label, fix) = if is_env_name(name) {
        (
            format!("환경변수 {name}"),
            format!("이 터미널에서 {name} 변수를 해제한 뒤 다시 실행하세요."),
        )
    } else {
        (
            "이름을 표시할 수 없는 환경변수".into(),
            "이 터미널의 인증 관련 환경변수를 확인한 뒤 해제한 다음 다시 실행하세요.".into(),
        )
    };
    ApiError::new(
        "AUTH_OVERRIDE_CONFLICT",
        format!("{label}이(가) 선택한 구독 인증을 덮어쓸 수 있습니다. {fix} 값은 출력하지 않았고, 설정은 자동으로 바꾸지 않습니다."),
    )
}

/// 파일 경로와 키 이름만 알린다. 값은 넣지 않는다.
fn config_conflict(path: &Path, key: &str) -> ApiError {
    ApiError::new(
        "AUTH_OVERRIDE_CONFLICT",
        format!(
            "설정 파일 {}의 {} 항목이 선택한 구독 인증을 덮어쓸 수 있습니다. 해당 파일에서 이 키를 제거한 뒤 다시 실행하세요. 값은 출력하지 않았고, 설정은 자동으로 바꾸지 않습니다.",
            display_path(path),
            display_key(key),
        ),
    )
}

fn has_marker(upper: &str, marker: &str) -> bool {
    // TOKEN은 `MAX_OUTPUT_TOKENS` 같은 출력 길이까지 막지 않도록 밑줄 단위로만 맞춘다.
    if marker == "TOKEN" {
        return upper.split('_').any(|part| part == "TOKEN");
    }
    upper.contains(marker)
}

fn inactive(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "" | "0" | "false" | "no"
    )
}

fn auth_env(tool: &str, key: &str, value: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    // 프로세스에 코드를 넣어 인증 호출·토큰·endpoint를 바꿀 수 있는 변수만 막는다.
    // NODE_OPTIONS: `--require`로 fetch·환경변수를 가로챌 수 있다. cmux 복원 파일만 check_env에서 예외.
    // LD_PRELOAD / DYLD_INSERT_LIBRARIES: 로드된 코드가 인증을 바꿀 수 있다.
    // 아래는 구독 계정을 고르지 않으므로 막지 않는다.
    // NODE_EXTRA_CA_CERTS, SSL_CERT_FILE, SSL_CERT_DIR: 추가 신뢰 CA. 계정·API 키·base URL을 바꾸지 않는다.
    // HTTPS_PROXY, HTTP_PROXY, ALL_PROXY, NODE_USE_ENV_PROXY, REQUESTS_CA_BUNDLE, CURL_CA_BUNDLE,
    // NODE_TLS_REJECT_UNAUTHORIZED: 프록시·TLS 검증 경로만 바꾼다. 프록시는 트래픽을 중계할 수 있지만
    // 프로필의 구독 계정을 다른 계정으로 바꾸지는 않는다. ANTHROPIC_BASE_URL·OPENAI_BASE_URL은
    // endpoint 자체를 바꾸므로 아래에서 막는다.
    if matches!(upper.as_str(), "NODE_OPTIONS" | "LD_PRELOAD" | "DYLD_INSERT_LIBRARIES") {
        return !value.is_empty();
    }
    if tool == "claude" {
        if upper.starts_with("CLAUDE_CODE_USE_") {
            return !inactive(value);
        }
        return (upper.starts_with("ANTHROPIC_") || upper.starts_with("CLAUDE_"))
            && [
                "API_KEY",
                "AUTH",
                "TOKEN",
                "BASE_URL",
                "CUSTOM_HEADER",
                "CREDENTIAL",
                "SETTINGS_FILE",
            ]
            .iter()
            .any(|marker| has_marker(&upper, marker))
            && !value.is_empty();
    }
    if tool == "codex" {
        return (upper.starts_with("OPENAI_")
            || upper.starts_with("CODEX_")
            || upper.starts_with("CHATGPT_"))
            && [
                "API_KEY", "AUTH", "TOKEN", "BASE_URL", "ENDPOINT", "CONFIG", "PROVIDER",
            ]
            .iter()
            .any(|marker| has_marker(&upper, marker))
            && !value.is_empty();
    }
    false
}

fn reject_auth_env(tool: &str, key: &str, value: &str) -> Result<(), ApiError> {
    if auth_env(tool, key, value) {
        return Err(env_conflict(key));
    }
    Ok(())
}

pub(crate) fn profile_var(tool: &str) -> Option<&'static str> {
    match tool {
        "claude" => Some("CLAUDE_CONFIG_DIR"),
        "codex" => Some("CODEX_HOME"),
        _ => None,
    }
}

pub(crate) fn check_env(tool: &str) -> Result<(), ApiError> {
    let cmux_node_options = if tool == "claude" {
        checked_cmux_node_options()?
    } else {
        None
    };
    for (key, value) in std::env::vars_os() {
        if key == "NODE_OPTIONS"
            && cmux_node_options
                .as_deref()
                .is_some_and(|allowed| value == allowed)
        {
            continue;
        }
        if let Err(error) = reject_auth_env(tool, &key.to_string_lossy(), &value.to_string_lossy()) {
            return Err(error);
        }
    }
    Ok(())
}

pub(crate) fn check_inherited_profile(tool: &str, selected: &Path) -> Result<(), ApiError> {
    let Some(key) = profile_var(tool) else {
        return Ok(());
    };
    if let Some(inherited) = std::env::var_os(key) {
        if inherited.is_empty()
            || Path::new(&inherited).canonicalize().ok().as_deref() != Some(selected)
        {
            return Err(ApiError::new("HOST_PROFILE_CONFLICT", "호스트가 지정한 native 프로필이 선택 계정과 다릅니다. 호스트의 계정 설정을 변경하지 않고 실행을 차단했습니다."));
        }
    }
    Ok(())
}

// cmux가 공식 wrapper에서 추가하는 복원 모듈만 인정합니다. 사용자 preload는 허용하지 않습니다.
fn checked_cmux_node_options() -> Result<Option<String>, ApiError> {
    let Some(value) = std::env::var("NODE_OPTIONS")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if std::env::var_os("CMUX_CLAUDE_PID").is_none() {
        return Ok(None);
    }
    let path = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("cmux-claude-node-options/restore-node-options.cjs");
    let prefix = format!("--require={} --max-old-space-size=4096", path.display());
    let original = match std::env::var("CMUX_ORIGINAL_NODE_OPTIONS") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => String::new(),
        Err(_) => return Err(env_conflict("NODE_OPTIONS")),
    };
    let present = std::env::var("CMUX_ORIGINAL_NODE_OPTIONS_PRESENT").unwrap_or_default();
    if !matches!(present.as_str(), "0" | "1")
        || (present == "0" && !original.is_empty())
        || !original.split_whitespace().all(|flag| {
            flag.strip_prefix("--max-old-space-size=")
                .and_then(|value| value.parse::<u32>().ok())
                .is_some_and(|size| (128..=65536).contains(&size))
        })
        || value != prefix
    {
        return Ok(None);
    }
    // cmux(macOS 앱)가 넣는 NODE_OPTIONS 복원 파일만 인정한다. 다른 사용자가 쓸 수 있는지 판정하는
    // Windows ACL 검사를 두지 않았으므로, Windows에서는 이 예외를 허용하지 않고 충돌로 멈춘다.
    if cfg!(windows) {
        return Err(env_conflict("NODE_OPTIONS"));
    }
    let metadata = fs::symlink_metadata(&path).map_err(|_| env_conflict("NODE_OPTIONS"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err(env_conflict("NODE_OPTIONS"));
        }
    }
    #[cfg(windows)]
    let _ = metadata;
    let expected = "const hadOriginalNodeOptions = process.env.CMUX_ORIGINAL_NODE_OPTIONS_PRESENT === \"1\";\nif (hadOriginalNodeOptions) {\n  process.env.NODE_OPTIONS = process.env.CMUX_ORIGINAL_NODE_OPTIONS ?? \"\";\n} else {\n  delete process.env.NODE_OPTIONS;\n}\ndelete process.env.CMUX_ORIGINAL_NODE_OPTIONS;\ndelete process.env.CMUX_ORIGINAL_NODE_OPTIONS_PRESENT;\n";
    if config_text(&path)?.as_deref() != Some(expected) {
        return Err(env_conflict("NODE_OPTIONS"));
    }
    Ok(Some(value))
}

fn config_text(path: &Path) -> Result<Option<String>, ApiError> {
    let file =
        match fs::File::open(path) {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(ApiError::new(
                "CONFIG_UNREADABLE",
                "인증 우선순위를 확인할 설정을 읽지 못했습니다. 파일 접근 권한을 확인해 주세요.",
            )),
        };
    let mut text = String::new();
    file.take(1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|_| {
            ApiError::new(
                "CONFIG_UNREADABLE",
                "native 설정을 안전하게 해석하지 못했습니다.",
            )
        })?;
    if text.len() > 1024 * 1024 {
        return Err(ApiError::new(
            "CONFIG_TOO_LARGE",
            "native 설정 파일이 안전한 검사 크기를 초과했습니다.",
        ));
    }
    Ok(Some(text))
}

fn claude_config(path: &Path) -> Result<(), ApiError> {
    let Some(text) = config_text(path)? else {
        return Ok(());
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| ApiError::new("CONFIG_INVALID", "Claude 설정의 JSON 형식을 확인해 주세요."))?;
    let object = value
        .as_object()
        .ok_or_else(|| ApiError::new("CONFIG_INVALID", "Claude 설정은 JSON 객체여야 합니다."))?;
    for key in object.keys() {
        let lower = key.to_ascii_lowercase();
        if lower.contains("apikey")
            || lower.contains("auth") && key != "forceLoginMethod" && key != "forceLoginOrgUUID"
            || lower.contains("baseurl")
            || lower.contains("credential")
            || key == "processWrapper"
        {
            return Err(config_conflict(path, key));
        }
    }
    if let Some(method) = value.get("forceLoginMethod").and_then(Value::as_str) {
        if method != "claudeai" {
            return Err(config_conflict(path, "forceLoginMethod"));
        }
    }
    if let Some(env) = value.get("env") {
        let env = env.as_object().ok_or_else(|| config_conflict(path, "env"))?;
        for (key, value) in env {
            let Some(value) = value.as_str() else {
                return Err(config_conflict(path, key));
            };
            if profile_var("claude") == Some(key.as_str())
                || key == "CLAUDE_CODE_PROCESS_WRAPPER"
                || auth_env("claude", key, value)
            {
                return Err(config_conflict(path, key));
            }
        }
    }
    Ok(())
}

fn codex_config(path: &Path) -> Result<(), ApiError> {
    let Some(text) = config_text(path)? else {
        return Ok(());
    };
    let value: toml::Value = toml::from_str(&text)
        .map_err(|_| ApiError::new("CONFIG_INVALID", "Codex 설정의 TOML 형식을 확인해 주세요."))?;
    let table = value.as_table().ok_or_else(|| config_conflict(path, "config"))?;
    // 임의 profile·custom provider는 실행 시 인증 우선순위를 바꾸므로 보수적으로 차단합니다.
    for key in [
        "model_providers",
        "profiles",
        "profile",
        "chatgpt_base_url",
        "openai_base_url",
        "api_key",
        "auth",
        "auth_provider",
        "credential_provider",
        "experimental_bearer_token",
        "oss_provider",
    ] {
        if table.contains_key(key) {
            return Err(config_conflict(path, key));
        }
    }
    if table
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .is_some_and(|v| v != "openai")
    {
        return Err(config_conflict(path, "model_provider"));
    }
    if table
        .get("forced_login_method")
        .and_then(toml::Value::as_str)
        .is_some_and(|v| v != "chatgpt")
    {
        return Err(config_conflict(path, "forced_login_method"));
    }
    if table
        .get("cli_auth_credentials_store")
        .and_then(toml::Value::as_str)
        .is_some_and(|v| v != "file")
    {
        return Err(ApiError::new("CREDENTIAL_STORE_UNVERIFIED", "이 Codex credential store의 프로필 격리를 확인하지 못했습니다. 공식 file 저장소 프로필로 등록해 주세요. 기존 인증은 복사하지 않습니다."));
    }
    Ok(())
}

pub(crate) fn check_settings(tool: &str, profile: &Path, cwd: &Path) -> Result<(), ApiError> {
    check_env(tool)?;
    match tool {
        "claude" => {
            for name in [
                "settings.json",
                "settings.local.json",
                "managed-settings.json",
            ] {
                claude_config(&profile.join(name))?;
            }
            let managed = Path::new("/Library/Application Support/ClaudeCode");
            claude_config(&managed.join("managed-settings.json"))?;
            if let Ok(entries) = fs::read_dir(managed.join("managed-settings.d")) {
                for entry in entries.flatten() {
                    if entry.path().extension().is_some_and(|x| x == "json") {
                        claude_config(&entry.path())?;
                    }
                }
            }
            for parent in cwd.ancestors() {
                claude_config(&parent.join(".claude/settings.json"))?;
                claude_config(&parent.join(".claude/settings.local.json"))?;
            }
        }
        "codex" => {
            codex_config(&profile.join("config.toml"))?;
            for path in [
                "/etc/codex/managed_config.toml",
                "/etc/codex/requirements.toml",
                "/Library/Application Support/Codex/managed_config.toml",
            ] {
                codex_config(Path::new(path))?;
            }
            for parent in cwd.ancestors() {
                codex_config(&parent.join(".codex/config.toml"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn canonical_profile(path: &Path) -> Result<PathBuf, ApiError> {
    if !path.is_absolute() {
        return Err(ApiError::new(
            "PROFILE_INVALID",
            "기존 native 프로필의 절대 경로를 선택해 주세요.",
        ));
    }
    let canonical = path.canonicalize().map_err(|_| {
        ApiError::new(
            "PROFILE_MISSING",
            "native 프로필 디렉터리를 찾지 못했습니다. 먼저 공식 로그인을 완료해 주세요.",
        )
    })?;
    if !canonical.is_dir() {
        return Err(ApiError::new(
            "PROFILE_INVALID",
            "프로필은 기존 디렉터리여야 합니다.",
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_cloud_flag_does_not_select_paid_backend() {
        assert!(!auth_env("claude", "CLAUDE_CODE_USE_BEDROCK", "0"));
        assert!(auth_env("claude", "CLAUDE_CODE_USE_BEDROCK", "1"));
        assert!(auth_env("claude", "CLAUDE_CODE_OAUTH_TOKEN_FD", "8"));
        assert!(auth_env(
            "codex",
            "OPENAI_BASE_URL",
            "https://example.invalid"
        ));
    }

    #[test]
    fn env_check_names_the_variable_and_hides_the_value() {
        let err = reject_auth_env("claude", "ANTHROPIC_API_KEY", "sk-test-secret").unwrap_err();
        assert_eq!(err.code, "AUTH_OVERRIDE_CONFLICT");
        assert!(err.message.contains("ANTHROPIC_API_KEY"), "{}", err.message);
        assert!(err.message.contains("해제"), "{}", err.message);
        assert!(!err.message.contains("sk-test-secret"), "{}", err.message);
    }

    #[test]
    fn harmless_tls_and_proxy_vars_are_not_auth_overrides() {
        for (tool, key, value) in [
            ("claude", "NODE_EXTRA_CA_CERTS", "/etc/ssl/corp.pem"),
            ("claude", "SSL_CERT_FILE", "/etc/ssl/cert.pem"),
            ("claude", "SSL_CERT_DIR", "/etc/ssl/certs"),
            ("claude", "HTTPS_PROXY", "http://127.0.0.1:8080"),
            ("claude", "HTTP_PROXY", "http://127.0.0.1:8080"),
            ("claude", "ALL_PROXY", "socks5://127.0.0.1:1080"),
            ("claude", "CLAUDE_CODE_MAX_OUTPUT_TOKENS", "8192"),
            ("codex", "NODE_EXTRA_CA_CERTS", "/etc/ssl/corp.pem"),
            ("codex", "SSL_CERT_FILE", "/etc/ssl/cert.pem"),
            ("codex", "HTTPS_PROXY", "http://127.0.0.1:8080"),
            ("codex", "HTTP_PROXY", "http://127.0.0.1:8080"),
        ] {
            assert!(reject_auth_env(tool, key, value).is_ok(), "{key} should not block {tool}");
        }
        assert!(reject_auth_env("claude", "NODE_OPTIONS", "--require /tmp/hook.js").is_err());
        assert!(reject_auth_env("claude", "LD_PRELOAD", "/tmp/lib.so").is_err());
        assert!(reject_auth_env("claude", "DYLD_INSERT_LIBRARIES", "/tmp/lib.dylib").is_err());
        assert!(reject_auth_env("claude", "ANTHROPIC_BASE_URL", "https://example.invalid").is_err());
        assert!(reject_auth_env("claude", "CLAUDE_CODE_OAUTH_TOKEN", "secret-token").is_err());
        assert!(reject_auth_env("codex", "OPENAI_BASE_URL", "https://example.invalid").is_err());
        assert!(reject_auth_env("codex", "NODE_OPTIONS", "--require /tmp/hook.js").is_err());
    }

    #[test]
    fn config_conflict_names_the_file_and_key_not_the_value() {
        let dir = std::env::temp_dir().join(format!(
            "ojak-safety-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        fs::write(&path, r#"{"apiKey":"sk-should-not-appear"}"#).unwrap();
        let err = claude_config(&path).unwrap_err();
        assert_eq!(err.code, "AUTH_OVERRIDE_CONFLICT");
        assert!(err.message.contains("apiKey"), "{}", err.message);
        assert!(err.message.contains("settings.json"), "{}", err.message);
        assert!(err.message.contains("제거"), "{}", err.message);
        assert!(!err.message.contains("sk-should-not-appear"), "{}", err.message);
        let _ = fs::remove_dir_all(&dir);
    }
}
