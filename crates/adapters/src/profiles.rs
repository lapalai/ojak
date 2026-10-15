//! Ojak이 `enrollment_plan`으로 만든 독립 native 프로필 디렉터리 열거.
//!
//! 이름 규칙은 `enrollment_plan`과 같다: `paths.profiles` 바로 아래 `claude-<UUIDv4>` / `codex-<UUIDv4>`.
//! 파일 시스템 메타데이터(이름·종류·symlink 여부·canonical 경로)만 본다.
//! 디렉터리 안의 파일(자격 증명 포함)은 열거나 읽지 않는다. 디렉터리가 있다는 사실은
//! 로그인 완료를 뜻하지 않으며, 인증 여부는 호출자가 공식 CLI 검사로 확인한다.

use std::fs;
use std::path::PathBuf;

const TOOLS: [&str; 2] = ["claude", "codex"];

/// `(tool, canonical 프로필 경로)` 목록. tool·UUID 문자열 순으로 정렬해 결정적이다.
/// 루트가 없거나 symlink·디렉터리가 아니면 빈 목록이다.
pub(crate) fn managed_profiles(paths: &aam_protocol::Paths) -> Vec<(String, PathBuf)> {
    let root = &paths.profiles;
    match fs::symlink_metadata(root) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        _ => return Vec::new(),
    }
    let Ok(canonical_root) = root.canonicalize() else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(&canonical_root) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let (tool, _) = parse_name(&name)?;
            let child = entry.path();
            // file_type은 symlink를 따라가지 않는다. 링크·파일은 여기서 제외된다.
            let kind = entry.file_type().ok()?;
            if kind.is_symlink() || !kind.is_dir() {
                return None;
            }
            let canonical = child.canonicalize().ok()?;
            if canonical.parent() != Some(canonical_root.as_path()) || !canonical.is_dir() {
                return None;
            }
            Some((tool.to_string(), canonical))
        })
        .collect();
    found.sort();
    found
}

/// `claude-<uuid>` / `codex-<uuid>`만 받는다. UUID는 `new_id()`가 만드는 소문자 하이픈 v4 형식이다.
fn parse_name(name: &str) -> Option<(&'static str, &str)> {
    TOOLS.iter().find_map(|tool| {
        let id = name.strip_prefix(*tool)?.strip_prefix('-')?;
        let parsed = uuid::Uuid::parse_str(id).ok()?;
        let mut buffer = uuid::Uuid::encode_buffer();
        (parsed.get_version_num() == 4 && parsed.hyphenated().encode_lower(&mut buffer) == id)
            .then_some((*tool, id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths() -> (PathBuf, aam_protocol::Paths) {
        let base = std::env::temp_dir().join(format!("aam-profiles-{}", aam_protocol::new_id()));
        let home = base.join("home");
        let paths = aam_protocol::Paths {
            socket: home.join("aam.sock"),
            database: home.join("aam.db"),
            profiles: home.join("profiles"),
            home,
        };
        (base, paths)
    }

    fn make(paths: &aam_protocol::Paths, tool: &str) -> PathBuf {
        let dir = paths
            .profiles
            .join(format!("{tool}-{}", aam_protocol::new_id()));
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn missing_root_is_empty() {
        let (base, paths) = temp_paths();
        assert!(managed_profiles(&paths).is_empty());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn lists_every_claude_and_codex_profile_deterministically() {
        let (base, paths) = temp_paths();
        let mut expected = Vec::new();
        for tool in ["codex", "claude", "claude", "codex", "claude"] {
            expected.push((tool.to_string(), make(&paths, tool)));
        }
        let listed = managed_profiles(&paths);
        assert_eq!(listed.len(), 5);
        for item in &expected {
            assert!(listed.contains(item), "lost profile {item:?}");
        }
        assert_eq!(listed, managed_profiles(&paths));
        let tools: Vec<_> = listed.iter().map(|(t, _)| t.as_str()).collect();
        assert_eq!(tools, ["claude", "claude", "claude", "codex", "codex"]);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn does_not_read_directory_contents() {
        let (base, paths) = temp_paths();
        let dir = make(&paths, "claude");
        // 내용이 없어도(로그인 전) 열거된다 — 인증 여부는 추론하지 않는다.
        assert_eq!(managed_profiles(&paths), vec![("claude".into(), dir)]);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn foreign_names_and_files_are_excluded() {
        let (base, paths) = temp_paths();
        let good = make(&paths, "codex");
        let id = aam_protocol::new_id();
        fs::create_dir_all(&paths.profiles).unwrap();
        for name in [
            "claude".to_string(),
            "claude-".to_string(),
            "claude-notes".to_string(),
            format!("gemini-{id}"),
            format!("Claude-{id}"),
            format!("claude_{id}"),
            format!("claude-{id}.bak"),
            format!("claude-{}", id.to_uppercase()),
            format!("claude-{}", id.replace('-', "")),
            format!("claude-{id}-extra"),
            "claude-00000000-0000-0000-0000-000000000000".to_string(),
        ] {
            fs::create_dir_all(paths.profiles.join(name)).unwrap();
        }
        // 이름은 맞지만 일반 파일
        fs::write(
            paths.profiles.join(format!("codex-{}", aam_protocol::new_id())),
            b"x",
        )
        .unwrap();
        assert_eq!(managed_profiles(&paths), vec![("codex".into(), good)]);
        let _ = fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_children_are_excluded() {
        use std::os::unix::fs::symlink;
        let (base, paths) = temp_paths();
        let good = make(&paths, "claude");
        let outside = base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        symlink(
            &outside,
            paths.profiles.join(format!("codex-{}", aam_protocol::new_id())),
        )
        .unwrap();
        symlink(
            &good,
            paths.profiles.join(format!("claude-{}", aam_protocol::new_id())),
        )
        .unwrap();
        symlink(
            base.join("missing"),
            paths.profiles.join(format!("claude-{}", aam_protocol::new_id())),
        )
        .unwrap();
        assert_eq!(managed_profiles(&paths), vec![("claude".into(), good)]);
        let _ = fs::remove_dir_all(base);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_root_is_rejected() {
        use std::os::unix::fs::symlink;
        let (base, paths) = temp_paths();
        let real = base.join("real-profiles");
        let id = aam_protocol::new_id();
        fs::create_dir_all(real.join(format!("claude-{id}"))).unwrap();
        fs::create_dir_all(&paths.home).unwrap();
        symlink(&real, &paths.profiles).unwrap();
        assert!(managed_profiles(&paths).is_empty());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn root_that_is_a_file_is_empty() {
        let (base, paths) = temp_paths();
        fs::create_dir_all(&paths.home).unwrap();
        fs::write(&paths.profiles, b"x").unwrap();
        assert!(managed_profiles(&paths).is_empty());
        let _ = fs::remove_dir_all(base);
    }
}
