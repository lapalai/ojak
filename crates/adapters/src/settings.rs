use aam_protocol::ApiError;
use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsPreview {
    pub tool: String,
    pub source: String,
    pub digest: String,
    pub can_import: bool,
    pub changes: Vec<SettingChange>,
    pub omitted: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SettingChange {
    pub key: String,
    pub value: Value,
}

pub struct SettingsImport {
    filename: &'static str,
    content: Vec<u8>,
}

fn source_path(tool: &str) -> Result<PathBuf, ApiError> {
    let home = crate::user_home()?;
    match tool {
        "claude" => Ok(home.join(".claude/settings.json")),
        "codex" => Ok(home.join(".codex/config.toml")),
        _ => Err(ApiError::new(
            "TOOL_UNSUPPORTED",
            "지원하지 않는 도구입니다.",
        )),
    }
}

fn sensitive_string(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "bearer ",
        "api_key=",
        "api-key=",
        "apikey=",
        "access_token=",
        "token=",
        "password=",
        "secret=",
        "sk-",
        "ghp_",
        "github_pat_",
        "-----begin",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        || value.split("://").skip(1).any(|part| {
            part.split('/')
                .next()
                .is_some_and(|authority| authority.contains('@'))
        })
}

fn plain_string(value: &Value) -> bool {
    value.as_str().is_some_and(|text| {
        text.len() <= 8192
            && !text.chars().any(|character| character.is_control())
            && !sensitive_string(text)
    })
}
fn strings(value: &Value) -> bool {
    value
        .as_array()
        .is_some_and(|values| values.len() <= 512 && values.iter().all(plain_string))
}

fn permissions(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.iter().all(|(key, value)| match key.as_str() {
            "allow" | "deny" | "ask" | "additionalDirectories" => strings(value),
            "defaultMode" | "disableBypassPermissionsMode" => plain_string(value),
            _ => false,
        })
    })
}
fn sandbox(value: &Value) -> bool {
    value.as_object().is_some_and(|object| {
        object.iter().all(|(key, value)| match key.as_str() {
            "enabled"
            | "autoAllowBashIfSandboxed"
            | "allowUnsandboxedCommands"
            | "failIfUnavailable"
            | "enableWeakerNestedSandbox"
            | "enableWeakerNetworkIsolation" => value.is_boolean(),
            "excludedCommands" => strings(value),
            "filesystem" => value.as_object().is_some_and(|fields| {
                fields.iter().all(|(key, value)| {
                    matches!(
                        key.as_str(),
                        "denyRead" | "denyWrite" | "allowRead" | "allowWrite"
                    ) && strings(value)
                })
            }),
            "network" => value.as_object().is_some_and(|fields| {
                fields.iter().all(|(key, value)| match key.as_str() {
                    "allowedDomains" | "deniedDomains" | "allowUnixSockets" => strings(value),
                    "allowAllUnixSockets" | "allowLocalBinding" | "allowManagedDomainsOnly" => {
                        value.is_boolean()
                    }
                    _ => false,
                })
            }),
            _ => false,
        })
    })
}

fn allowed(tool: &str, key: &str, value: &Value) -> bool {
    match (tool, key) {
        ("claude", "permissions") => permissions(value),
        ("claude", "sandbox") => sandbox(value),
        ("claude", "availableModels") => strings(value),
        (
            "claude",
            "model"
            | "language"
            | "outputStyle"
            | "effortLevel"
            | "editorMode"
            | "theme"
            | "preferredNotifChannel"
            | "autoUpdatesChannel",
        ) => plain_string(value),
        (
            "claude",
            "alwaysThinkingEnabled"
            | "spinnerTipsEnabled"
            | "showTurnDuration"
            | "terminalProgressBarEnabled"
            | "autoMemoryEnabled"
            | "enforceAvailableModels",
        ) => value.is_boolean(),
        ("claude", "cleanupPeriodDays") => value.as_u64().is_some_and(|days| days <= 3650),
        (
            "codex",
            "model"
            | "model_reasoning_effort"
            | "model_reasoning_summary"
            | "model_verbosity"
            | "approval_policy"
            | "sandbox_mode",
        ) => plain_string(value),
        ("codex", "sandbox_workspace_write") => value.as_object().is_some_and(|fields| {
            fields.iter().all(|(key, value)| match key.as_str() {
                "writable_roots" => strings(value),
                "network_access" | "exclude_tmpdir_env_var" | "exclude_slash_tmp" => {
                    value.is_boolean()
                }
                _ => false,
            })
        }),
        _ => false,
    }
}

fn parse_preview(tool: &str, source: &Path, bytes: &[u8]) -> Result<SettingsPreview, ApiError> {
    let source_value: Value = if tool == "codex" {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| ApiError::new("SETTINGS_INVALID", "설정 파일이 UTF-8이 아닙니다."))?;
        let value: toml::Value = toml::from_str(text).map_err(|_| {
            ApiError::new(
                "SETTINGS_INVALID",
                "기존 설정 TOML을 해석할 수 없습니다. 원문은 출력하지 않았습니다.",
            )
        })?;
        serde_json::to_value(value).map_err(|_| {
            ApiError::new("SETTINGS_INVALID", "기존 설정 구조를 확인할 수 없습니다.")
        })?
    } else {
        serde_json::from_slice(bytes).map_err(|_| {
            ApiError::new(
                "SETTINGS_INVALID",
                "기존 설정 JSON을 해석할 수 없습니다. 원문은 출력하지 않았습니다.",
            )
        })?
    };
    let object = source_value.as_object().ok_or_else(|| {
        ApiError::new("SETTINGS_INVALID", "기존 설정은 object 형식이어야 합니다.")
    })?;
    let mut digest = Sha256::new();
    digest.update(tool.as_bytes());
    digest.update([0]);
    digest.update(source.to_string_lossy().as_bytes());
    digest.update([0]);
    digest.update(bytes);
    let mut preview = SettingsPreview {
        tool: tool.into(), source: source.to_string_lossy().into_owned(), digest: format!("{:x}", digest.finalize()), can_import: true,
        changes: Vec::new(), omitted: Vec::new(), warnings: vec!["시스템·조직의 managed policy는 공식 CLI가 계속 적용합니다. 기존 프로필 파일은 변경하지 않습니다.".into()],
    };
    let mut extra = 0usize;
    for (key, value) in object {
        if allowed(tool, key, value) {
            preview.changes.push(SettingChange {
                key: key.clone(),
                value: value.clone(),
            });
        } else {
            extra += 1;
            if matches!(
                key.as_str(),
                "permissions"
                    | "sandbox"
                    | "sandbox_workspace_write"
                    | "availableModels"
                    | "enforceAvailableModels"
            ) {
                preview.can_import = false;
                preview.warnings.push("보안 설정에 미지원 항목 또는 민감한 값이 있어 일부만 복제하지 않습니다. 기존 프로필 연결을 사용하거나 해당 설정을 공식 CLI에서 별도로 검토해 주세요.".into());
            }
        }
    }
    if extra > 0 {
        preview.omitted.push(format!(
            "인증·환경변수·실행 코드·미지원 설정 {extra}개 항목"
        ));
    }
    preview.omitted.push("MCP 연결, hooks, statusline 명령, skills·plugins·commands, 인증·세션·메모리 파일은 자동 복제하지 않습니다. 확장 환경을 그대로 유지하려면 기존 프로필을 연결해 주세요.".into());
    preview.can_import &= !preview.changes.is_empty();
    Ok(preview)
}

pub fn preview_settings(tool: &str) -> Result<SettingsPreview, ApiError> {
    let source = source_path(tool)?;
    let source = match source.canonicalize() {
        Ok(source) => source,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SettingsPreview {
                tool: tool.into(),
                source: source.to_string_lossy().into_owned(),
                digest: String::new(),
                can_import: false,
                changes: Vec::new(),
                omitted: Vec::new(),
                warnings: vec![
                    "가져올 기본 설정 파일이 없습니다. 새 프로필의 기본 설정으로 시작합니다."
                        .into(),
                ],
            })
        }
        Err(_) => {
            return Err(ApiError::new(
                "SETTINGS_UNREADABLE",
                "기존 설정 경로를 확인할 수 없습니다.",
            ))
        }
    };
    let file = aam_protocol::secure::open_read_no_follow(&source)
        .map_err(|_| ApiError::new("SETTINGS_UNREADABLE", "기존 설정 파일을 읽을 수 없습니다."))?;
    let metadata = file.metadata().map_err(|_| {
        ApiError::new(
            "SETTINGS_UNREADABLE",
            "기존 설정 파일 정보를 확인할 수 없습니다.",
        )
    })?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(ApiError::new(
            "SETTINGS_INVALID",
            "기존 설정은 1MiB 이하의 일반 파일이어야 합니다.",
        ));
    }
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ApiError::new("SETTINGS_UNREADABLE", "기존 설정 파일을 읽을 수 없습니다."))?;
    if bytes.len() > 1024 * 1024 {
        return Err(ApiError::new(
            "SETTINGS_INVALID",
            "기존 설정 파일이 읽는 동안 허용 크기를 초과했습니다.",
        ));
    }
    parse_preview(tool, &source, &bytes)
}

fn checked_import(
    preview: SettingsPreview,
    expected_digest: &str,
) -> Result<SettingsImport, ApiError> {
    if expected_digest.len() != 64
        || !expected_digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        || preview.digest != expected_digest
    {
        return Err(ApiError::new(
            "SETTINGS_CHANGED",
            "미리보기 이후 설정이 바뀌었습니다. 변경 내용을 다시 확인한 뒤 로그인해 주세요.",
        ));
    }
    if !preview.can_import {
        return Err(ApiError::new(
            "SETTINGS_IMPORT_UNAVAILABLE",
            "안전하게 가져올 수 있는 설정이 없거나 보안 설정 검토가 필요합니다.",
        ));
    }
    let values: Map<String, Value> = preview
        .changes
        .into_iter()
        .map(|change| (change.key, change.value))
        .collect();
    if preview.tool == "codex" {
        let content = toml::to_string_pretty(&values)
            .map_err(|_| ApiError::new("SETTINGS_INVALID", "설정 TOML을 만들 수 없습니다."))?;
        Ok(SettingsImport {
            filename: "config.toml",
            content: content.into_bytes(),
        })
    } else {
        let content = serde_json::to_vec_pretty(&values)
            .map_err(|_| ApiError::new("SETTINGS_INVALID", "설정 JSON을 만들 수 없습니다."))?;
        Ok(SettingsImport {
            filename: "settings.json",
            content,
        })
    }
}

pub fn prepare_settings_import(tool: &str, digest: &str) -> Result<SettingsImport, ApiError> {
    checked_import(preview_settings(tool)?, digest)
}

pub fn apply_settings_import(profile: &Path, import: SettingsImport) -> Result<(), ApiError> {
    let target = profile.join(import.filename);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    // Windows는 프로필 폴더의 소유자 전용 상속 ACL을 그대로 받는다.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(&target)
        .map_err(|_| {
            ApiError::new(
                "SETTINGS_WRITE_FAILED",
                "새 프로필에 설정을 만들지 못했습니다. 기존 파일은 덮어쓰지 않았습니다.",
            )
        })?;
    file.write_all(&import.content)
        .and_then(|_| file.sync_all())
        .map_err(|_| {
            ApiError::new(
                "SETTINGS_WRITE_FAILED",
                "새 프로필 설정을 저장하지 못해 로그인을 시작하지 않았습니다.",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn preview_omits_auth_and_executable_config_but_keeps_security_rules() {
        let source = json!({"model":"claude-haiku-4-5","permissions":{"deny":["Read(**/secrets/**)"]},"env":{"ANTHROPIC_API_KEY":"private-canary-credential"},"hooks":{"command":"private-canary-hook"}});
        let preview = parse_preview(
            "claude",
            Path::new("/settings.json"),
            &serde_json::to_vec(&source).unwrap(),
        )
        .unwrap();
        let json = serde_json::to_string(&preview).unwrap();
        assert!(!json.contains("private-canary"));
        let digest = preview.digest.clone();
        let import = checked_import(preview, &digest).unwrap();
        let imported: Value = serde_json::from_slice(&import.content).unwrap();
        assert_eq!(
            imported["permissions"]["deny"],
            source["permissions"]["deny"]
        );
        assert!(imported.get("env").is_none());
        assert!(imported.get("hooks").is_none());
    }

    #[test]
    fn changed_settings_and_partially_unsupported_permissions_cannot_be_imported() {
        let first = parse_preview(
            "claude",
            Path::new("/settings.json"),
            br#"{"model":"haiku"}"#,
        )
        .unwrap();
        let changed = parse_preview(
            "claude",
            Path::new("/settings.json"),
            br#"{"model":"sonnet"}"#,
        )
        .unwrap();
        assert_eq!(
            checked_import(changed, &first.digest).err().unwrap().code,
            "SETTINGS_CHANGED"
        );
        let unsafe_settings = parse_preview("claude", Path::new("/settings.json"), br#"{"permissions":{"deny":["Read(.env)"],"futureSecurityPolicy":true},"language":"Korean"}"#).unwrap();
        let digest = unsafe_settings.digest.clone();
        assert_eq!(
            checked_import(unsafe_settings, &digest).err().unwrap().code,
            "SETTINGS_IMPORT_UNAVAILABLE"
        );
    }

    #[test]
    fn codex_sandbox_survives_without_copying_provider_credentials() {
        let preview = parse_preview("codex", Path::new("/config.toml"), b"model = 'gpt-5'\nmodel_provider = 'private'\n[sandbox_workspace_write]\nnetwork_access = false\n[model_providers.private]\napi_key = 'private-canary'\n").unwrap();
        let digest = preview.digest.clone();
        let import = checked_import(preview, &digest).unwrap();
        let text = std::str::from_utf8(&import.content).unwrap();
        let imported: toml::Value = toml::from_str(text).unwrap();
        assert_eq!(
            imported["sandbox_workspace_write"]["network_access"].as_bool(),
            Some(false)
        );
        assert!(imported.get("model_providers").is_none());
        assert!(imported.get("model_provider").is_none());
        assert!(!text.contains("private-canary"));
    }

    #[test]
    fn settings_import_never_overwrites_an_existing_profile_file() {
        let folder =
            std::env::temp_dir().join(format!("aam-settings-test-{}", aam_protocol::new_id()));
        fs::create_dir(&folder).unwrap();
        let target = folder.join("settings.json");
        let original = br#"{"permissions":{"deny":["Read(.env)"]}}"#;
        fs::write(&target, original).unwrap();
        let imported = SettingsImport {
            filename: "settings.json",
            content: b"{}".to_vec(),
        };
        assert_eq!(
            apply_settings_import(&folder, imported).unwrap_err().code,
            "SETTINGS_WRITE_FAILED"
        );
        assert_eq!(fs::read(&target).unwrap(), original);
        fs::remove_dir_all(folder).unwrap();
    }
}
