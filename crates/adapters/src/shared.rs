//! 격리 프로필이 사용자 기본 설정 폴더(`~/.claude`, `~/.codex`)의 인증 없는 확장을 함께 쓰게 한다.
//! 스킬·에이전트·지침·플러그인·hooks는 링크로 공유하고, 설정 파일에서는 확장 관련 키만 복사한다.
//! `env`·인증 키·로그인 파일(`.claude.json`, `auth.json`)은 건드리지 않는다.
use aam_protocol::ApiError;
use serde_json::{Map, Value};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

/// 도구별 공유 대상. `linked`는 프로필에서 기본 폴더의 같은 이름을 가리키게 할 항목(Unix만),
/// `keys`는 기본 설정 파일에서 프로필 설정 파일로 옮길 키다. 기본에서 빠진 키는 프로필에서도 지운다.
pub(crate) struct Spec {
    pub linked: &'static [&'static str],
    pub settings: &'static str,
    pub keys: &'static [&'static str],
}
pub(crate) const CLAUDE: Spec = Spec {
    linked: &["skills", "agents", "commands", "plugins", "hooks", "output-styles", "CLAUDE.md"],
    settings: "settings.json",
    keys: &["enabledPlugins", "extraKnownMarketplaces", "hooks", "statusLine"],
};
/// Codex는 `shell_environment_policy`(셸 도구에 넘길 환경)와 모델·프로젝트 신뢰 설정은 옮기지 않는다.
pub(crate) const CODEX: Spec = Spec {
    linked: &["skills", "agents", "rules", "plugins", "prompts", "AGENTS.md", "hooks.json"],
    settings: "config.toml",
    keys: &["plugins", "marketplaces", "mcp_servers", "hooks", "notify", "features"],
};

fn failed() -> ApiError {
    ApiError::new(
        "SHARED_CONFIG_FAILED",
        "기본 설정의 스킬·플러그인을 이 계정 프로필에 연결하지 못했습니다.",
    )
}

/// `base`(`~/.claude`·`~/.codex`)의 확장을 `profile`에 연결한다. 같은 폴더면 아무것도 하지 않는다.
pub(crate) fn share(spec: &Spec, base: &Path, profile: &Path) -> Result<(), ApiError> {
    let (Ok(base), Ok(profile)) = (base.canonicalize(), profile.canonicalize()) else {
        return Ok(());
    };
    if base == profile || !base.is_dir() {
        return Ok(());
    }
    link_entries(spec.linked, &base, &profile)?;
    merge_settings(spec, &base.join(spec.settings), &profile.join(spec.settings))
}

#[cfg(unix)]
fn link_entries(linked: &[&str], base: &Path, profile: &Path) -> Result<(), ApiError> {
    for &name in linked {
        let source = base.join(name);
        if fs::symlink_metadata(&source).is_err() {
            continue;
        }
        let target = profile.join(name);
        match fs::symlink_metadata(&target) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(failed()),
            // 이미 링크가 있으면(공유 중이거나 사용자가 따로 연결) 그대로 둔다.
            Ok(metadata) if metadata.file_type().is_symlink() => continue,
            // 비어 있는 폴더는 CLI가 만든 자리다. 내용이 있으면 지우지 않고 backups로 옮긴다.
            Ok(metadata) => {
                if !(metadata.is_dir() && fs::remove_dir(&target).is_ok()) {
                    let backups = profile.join("backups");
                    fs::create_dir_all(&backups).map_err(|_| failed())?;
                    let moved = backups.join(format!("{name}.before-shared-{}", aam_protocol::now_ms()));
                    fs::rename(&target, moved).map_err(|_| failed())?;
                }
            }
        }
        std::os::unix::fs::symlink(&source, &target).map_err(|_| failed())?;
    }
    Ok(())
}

/// Windows의 symlink는 개발자 모드나 관리자 권한이 필요하다. 폴더 공유는 하지 않고 설정 키만 옮긴다.
#[cfg(not(unix))]
fn link_entries(_linked: &[&str], _base: &Path, _profile: &Path) -> Result<(), ApiError> {
    Ok(())
}

fn read_object(path: &Path) -> Result<Option<Map<String, Value>>, ApiError> {
    let file = match aam_protocol::secure::open_read_no_follow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(failed()),
    };
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes).map_err(|_| failed())?;
    if bytes.len() > 1024 * 1024 {
        return Err(failed());
    }
    let value = if path.extension().is_some_and(|ext| ext == "toml") {
        let text = std::str::from_utf8(&bytes).map_err(|_| failed())?;
        serde_json::to_value(toml::from_str::<toml::Value>(text).map_err(|_| failed())?).map_err(|_| failed())?
    } else {
        serde_json::from_slice::<Value>(&bytes).map_err(|_| failed())?
    };
    match value {
        Value::Object(object) => Ok(Some(object)),
        _ => Err(failed()),
    }
}

fn merge_settings(spec: &Spec, base: &Path, profile: &Path) -> Result<(), ApiError> {
    let source = read_object(base)?.unwrap_or_default();
    let current = read_object(profile)?;
    let mut merged = current.clone().unwrap_or_default();
    for &key in spec.keys {
        match source.get(key) {
            Some(value) => {
                merged.insert(key.into(), value.clone());
            }
            None => {
                merged.remove(key);
            }
        }
    }
    if current.as_ref() == Some(&merged) || (current.is_none() && merged.is_empty()) {
        return Ok(());
    }
    // 같은 폴더의 임시 파일에 쓰고 바꿔 끼운다. 동시에 시작한 다른 실행이 반쯤 쓴 파일을 읽지 않게 한다.
    let temporary = profile.with_file_name(format!(".{}.{}.tmp", spec.settings, uuid::Uuid::new_v4()));
    let content = if spec.settings.ends_with(".toml") {
        toml::to_string_pretty(&merged).map_err(|_| failed())?.into_bytes()
    } else {
        serde_json::to_vec_pretty(&Value::Object(merged.clone())).map_err(|_| failed())?
    };
    let write = || -> std::io::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        aam_protocol::secure::private_options(&mut options);
        let mut file = options.open(&temporary)?;
        file.write_all(&content)?;
        file.sync_all()?;
        fs::rename(&temporary, profile)
    };
    write().map_err(|_| {
        let _ = fs::remove_file(&temporary);
        failed()
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use serde_json::json;

    /// 공유는 확장만 연결하고 인증과 무관한 키만 옮긴다. 프로필 고유 설정·로그인 파일은 유지하고,
    /// 내용이 있던 폴더는 지우지 않고 backups로 옮긴다. 기본에서 빠진 키는 프로필에서도 빠진다.
    #[test]
    fn shares_extensions_and_plugin_keys_without_env_or_login() {
        let root = std::env::temp_dir().join(format!("aam-shared-{}", uuid::Uuid::new_v4()));
        let base = root.join("base");
        let profile = root.join("profile");
        fs::create_dir_all(base.join("skills/demo")).unwrap();
        fs::create_dir_all(base.join("plugins")).unwrap();
        fs::write(base.join("CLAUDE.md"), "global").unwrap();
        fs::write(
            base.join("settings.json"),
            json!({"enabledPlugins":{"a@m":true},"hooks":{"Stop":[]},"env":{"X":"1"},"theme":"dark"}).to_string(),
        )
        .unwrap();
        fs::create_dir_all(profile.join("skills")).unwrap();
        fs::create_dir_all(profile.join("plugins/marketplaces")).unwrap();
        fs::write(profile.join(".claude.json"), "{\"oauthAccount\":{}}").unwrap();
        fs::write(profile.join("settings.json"), json!({"theme":"light","statusLine":{"type":"command"}}).to_string()).unwrap();

        share(&CLAUDE, &base, &profile).unwrap();

        let base = base.canonicalize().unwrap();
        for name in ["skills", "plugins", "CLAUDE.md"] {
            assert_eq!(fs::read_link(profile.join(name)).unwrap(), base.join(name), "{name}");
        }
        assert!(fs::symlink_metadata(profile.join("agents")).is_err());
        let backups: Vec<_> = fs::read_dir(profile.join("backups")).unwrap().flatten().map(|e| e.file_name().into_string().unwrap()).collect();
        assert!(backups.len() == 1 && backups[0].starts_with("plugins.before-shared-"), "{backups:?}");
        assert!(profile.join(format!("backups/{}/marketplaces", backups[0])).is_dir());
        let settings: Value = serde_json::from_str(&fs::read_to_string(profile.join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings, json!({"theme":"light","enabledPlugins":{"a@m":true},"hooks":{"Stop":[]}}));
        assert_eq!(fs::read_to_string(profile.join(".claude.json")).unwrap(), "{\"oauthAccount\":{}}");

        // 두 번째 실행은 아무것도 바꾸지 않는다.
        share(&CLAUDE, &base, &profile).unwrap();
        assert_eq!(fs::read_dir(profile.join("backups")).unwrap().count(), 1);
        fs::remove_dir_all(&root).unwrap();
    }

    /// Codex는 config.toml에서 확장 키만 옮기고 모델·프로젝트 신뢰·셸 환경 정책은 두며, auth.json은 건드리지 않는다.
    #[test]
    fn codex_shares_extension_keys_in_toml() {
        let root = std::env::temp_dir().join(format!("aam-shared-codex-{}", uuid::Uuid::new_v4()));
        let base = root.join("base");
        let profile = root.join("profile");
        fs::create_dir_all(base.join("skills/.system")).unwrap();
        fs::write(base.join("AGENTS.md"), "global").unwrap();
        fs::write(base.join("config.toml"), "model = \"a\"\nnotify = [\"x\"]\n[shell_environment_policy]\ninherit = \"all\"\n[mcp_servers.m]\ncommand = \"m\"\n[plugins.\"p@q\"]\nenabled = true\n").unwrap();
        fs::create_dir_all(&profile).unwrap();
        fs::write(profile.join("auth.json"), "{}").unwrap();
        fs::write(profile.join("config.toml"), "model = \"b\"\n[projects.\"/w\"]\ntrust_level = \"trusted\"\n").unwrap();

        share(&CODEX, &base, &profile).unwrap();

        let base = base.canonicalize().unwrap();
        assert_eq!(fs::read_link(profile.join("AGENTS.md")).unwrap(), base.join("AGENTS.md"));
        assert_eq!(fs::read_link(profile.join("skills")).unwrap(), base.join("skills"));
        let config: toml::Value = toml::from_str(&fs::read_to_string(profile.join("config.toml")).unwrap()).unwrap();
        assert_eq!(config["model"].as_str(), Some("b"));
        assert!(config["projects"]["/w"].is_table());
        assert_eq!(config["mcp_servers"]["m"]["command"].as_str(), Some("m"));
        assert_eq!(config["plugins"]["p@q"]["enabled"].as_bool(), Some(true));
        assert!(config.get("shell_environment_policy").is_none());
        assert_eq!(fs::read_to_string(profile.join("auth.json")).unwrap(), "{}");
        fs::remove_dir_all(&root).unwrap();
    }
}
