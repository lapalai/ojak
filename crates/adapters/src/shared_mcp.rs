//! 공식 `claude mcp add`로 기본 설정(`~/.claude.json`)에 등록한 MCP 서버를 Ojak 계정 실행에 연결한다.
//!
//! `.claude.json`은 로그인·신뢰·기록이 섞인 파일이라 복사하거나 링크하지 않는다. 대신 실행마다 서버 정의만 뽑아
//! 사용자 전용 임시 파일을 만들고 공식 `--mcp-config=<파일>`로 넘긴다. 이 옵션은 그 실행에만 적용되고
//! `allowedMcpServers`·`deniedMcpServers` 등 관리 정책을 그대로 받는다(2.1.322 실측).
//!
//! 공식 우선순위(local > project > user)는 이름이 겹칠 때 계정 쪽 정의를 지키도록 미리 걸러서 유지한다.
//! - 계정 프로필이 이미 가진 같은 이름(user·local)은 덮어쓰지 않는다.
//! - 프로젝트 `.mcp.json`에 있는 이름은 user 서버로 덮지 않는다. 명시적으로 거부된 project 서버만 user 등록으로 돌아간다.
//! - 기본 설정이나 계정 프로필에서 이 프로젝트에 대해 끈(`disabledMcpServers`) 서버는 연결하지 않는다.
//!
//! 서버 명령·URL·헤더·환경 변수는 로그나 오류에 쓰지 않는다.
use aam_protocol::{secure, Account, ApiError};
use serde_json::{Map, Value};
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use unicode_normalization::UnicodeNormalization;

/// `.claude.json`은 대화 기록 때문에 수 MB가 될 수 있다.
const CONFIG_LIMIT: u64 = 64 * 1024 * 1024;
const PROJECT_FILE_LIMIT: u64 = 4 * 1024 * 1024;

fn failed() -> ApiError {
    ApiError::new(
        "SHARED_MCP_FAILED",
        "기본 설정의 MCP를 이 계정 실행에 연결하지 못했어요. MCP 없이 실행해요.",
    )
}

/// 한 번의 실행이 쓰는 MCP 설정 파일. 실행이 끝나면(또는 이 값이 버려지면) 파일을 지운다.
/// Claude의 자기 재실행은 같은 인수를 다시 쓰므로 파일은 실행 동안 유지된다.
/// launcher가 강제 종료되면 파일이 남을 수 있다. 그 파일은 사용자 전용 권한이고, 살아 있는 Claude가 아직 쓰는지
/// 알 수 없으므로 시간이나 잠금만 보고 지우지 않는다.
pub struct SharedMcp {
    path: PathBuf,
}

impl SharedMcp {
    /// 값을 인수에 이어 붙이지 않고 `=` 형태로 넘긴다. `--mcp-config <configs...>`는 뒤따르는 위치 인수(프롬프트)를 삼킨다.
    pub fn arg(&self) -> String {
        format!("--mcp-config={}", self.path.display())
    }
}

impl Drop for SharedMcp {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// 이 계정의 Claude 실행에 덧붙일 MCP 설정. 연결할 서버가 없거나 이 계정이 공식 기본 프로필이면 `None`이다.
/// `dir`은 Ojak 앱 폴더 안의 전용 폴더다.
pub fn prepare(dir: &Path, account: &Account, cwd: &str) -> Result<Option<SharedMcp>, ApiError> {
    if account.tool != "claude" {
        return Ok(None);
    }
    let home = aam_protocol::user_home().ok_or_else(failed)?;
    let (profile, _) = crate::native::bound_paths(account)?;
    // 공식 기본 프로필은 공식 CLI가 `~/.claude.json`을 직접 읽는다.
    if home.join(".claude").canonicalize().ok().as_deref() == Some(profile.as_path()) {
        return Ok(None);
    }
    prepare_at(dir, &home.join(".claude.json"), &profile, Path::new(cwd), &managed_dir())
}

fn prepare_at(
    dir: &Path,
    base_config: &Path,
    profile: &Path,
    cwd: &Path,
    managed: &Path,
) -> Result<Option<SharedMcp>, ApiError> {
    // 관리 정책이 `--mcp-config`를 거부하게 해 두었으면 옵션을 붙이는 순간 Claude가 시작하지 못한다.
    if sideload_blocked(managed) {
        return Ok(None);
    }
    let Some(base) = read_config(base_config)? else {
        return Ok(None);
    };
    let own = read_config(&profile.join(".claude.json"))?;
    // Windows 공식 CLI는 작업 폴더의 철자(대소문자·8.3 짧은 이름)를 그대로 키로 쓴다(2.1.322 실측). `canonicalize`는 이를 바꾼다.
    let cwd = if cfg!(windows) { cwd.to_path_buf() } else { cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()) };
    let key = project_key(&cwd);
    let mut project_names = project_server_names(&cwd);
    let rejected = rejected_project_names(&cwd, profile, own.as_ref(), &key)?;
    project_names.retain(|name| !rejected.contains(name));
    let servers = merged(&base, own.as_ref(), &key, &project_names);
    if servers.is_empty() {
        return Ok(None);
    }
    let body = serde_json::to_vec(&serde_json::json!({ "mcpServers": servers })).map_err(|_| failed())?;
    write_private(dir, &body).map(Some)
}

fn read_config(path: &Path) -> Result<Option<Value>, ApiError> {
    // 홈 설정 파일을 dotfiles 저장소로 링크해 둔 사용자가 있으니 읽기에서는 링크를 따라간다(쓰지 않는다).
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(failed()),
    };
    let mut bytes = Vec::new();
    file.take(CONFIG_LIMIT + 1).read_to_end(&mut bytes).map_err(|_| failed())?;
    if bytes.len() as u64 > CONFIG_LIMIT {
        return Err(failed());
    }
    match serde_json::from_slice::<Value>(&bytes).map_err(|_| failed())? {
        value @ Value::Object(_) => Ok(Some(value)),
        _ => Err(failed()),
    }
}

fn object<'a>(value: &'a Value, key: &str) -> Option<&'a Map<String, Value>> {
    value.get(key).and_then(Value::as_object)
}

fn project<'a>(config: &'a Value, key: &str) -> Option<&'a Value> {
    object(config, "projects").and_then(|projects| projects.get(key))
}

fn user_servers(config: &Value) -> Map<String, Value> {
    object(config, "mcpServers").cloned().unwrap_or_default()
}

fn local_servers(config: &Value, key: &str) -> Map<String, Value> {
    project(config, key).and_then(|entry| object(entry, "mcpServers")).cloned().unwrap_or_default()
}

fn disabled(config: &Value, key: &str) -> BTreeSet<String> {
    project(config, key)
        .and_then(|entry| entry.get("disabledMcpServers"))
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default()
}

/// 기본 설정의 user·local 서버 중 이 계정에 덧붙일 것. 우선순위는 공식 CLI와 같게 local > project > user.
fn merged(base: &Value, own: Option<&Value>, key: &str, project_names: &BTreeSet<String>) -> Map<String, Value> {
    let empty = Value::Object(Map::new());
    let own = own.unwrap_or(&empty);
    let (own_user, own_local) = (user_servers(own), local_servers(own, key));
    // 기본 설정에서 끈 서버와 이 계정 프로필에서 끈 서버 모두 연결하지 않는다.
    let mut off = disabled(base, key);
    off.extend(disabled(own, key));
    let mut result = Map::new();
    for (name, server) in user_servers(base) {
        // 계정의 user·local과 프로젝트 `.mcp.json`이 먼저다.
        if own_user.contains_key(&name) || own_local.contains_key(&name) || project_names.contains(&name) {
            continue;
        }
        result.insert(name, server);
    }
    for (name, server) in local_servers(base, key) {
        // local은 계정의 user·프로젝트보다 앞서지만 계정의 같은 이름 local은 그 계정이 가진 값이다.
        if own_local.contains_key(&name) {
            continue;
        }
        result.insert(name, server);
    }
    result.retain(|name, server| server.is_object() && !off.contains(name));
    result
}

/// 공식 CLI가 로컬 범위를 저장하는 프로젝트 키. git 저장소(워크트리는 원본 저장소, 서브모듈은 자기 폴더)의 루트,
/// 없으면 작업 폴더다(macOS 2.1.322에서 하위 폴더·워크트리·심볼릭 링크, Windows 2.1.322에서 하위 폴더·워크트리 실측).
/// Windows는 8.3 짧은 이름·대소문자도 작업 폴더 철자 그대로 키가 된다(`ADMINI~1`과 `Administrator`가 서로 다른 키).
/// 짧은 이름 경로의 워크트리는 공식 CLI가 원본 저장소가 아닌 워크트리 자신의 키를 쓰기도 했다. 이런 철자 차이로 키가
/// 어긋나면 local 서버만 빠지고(user 서버는 그대로) 다른 프로젝트의 서버가 섞이지는 않는다.
fn project_key(cwd: &Path) -> String {
    path_key(&cwd.ancestors().find_map(git_root).unwrap_or_else(|| cwd.to_path_buf()))
}

/// 경로를 `projects`의 키 문자열로 바꾼다. Windows 공식 CLI는 `C:/Users/…`처럼 작업 폴더 문자열 그대로 구분자만 `/`로 쓴다.
fn path_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        windows_key(&text)
    } else {
        text.nfc().collect()
    }
}

/// 확장 드라이브·UNC 접두사는 일반 키 형태로 바꾸고, 다른 장치 경로는 그대로 보존한다.
fn windows_key(text: &str) -> String {
    let (prefix, path) = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        ("//", rest)
    } else {
        let path = match text.strip_prefix(r"\\?\") {
            Some(rest) if rest.as_bytes().get(1) == Some(&b':') && rest.as_bytes()[0].is_ascii_alphabetic() => rest,
            _ => text,
        };
        ("", path)
    };
    let mut key = String::with_capacity(prefix.len() + path.len());
    key.push_str(prefix);
    key.extend(path.nfc().map(|ch| if ch == '\\' { '/' } else { ch }));
    key
}

fn git_root(dir: &Path) -> Option<PathBuf> {
    let git = dir.join(".git");
    let metadata = fs::symlink_metadata(&git).ok()?;
    if metadata.is_dir() {
        return Some(dir.to_path_buf());
    }
    // 워크트리·서브모듈은 `gitdir: <경로>` 한 줄이 든 파일이다. 워크트리만 `commondir`로 원본 저장소를 가리킨다.
    let text = fs::read_to_string(&git).ok()?;
    let gitdir = text.lines().find_map(|line| line.strip_prefix("gitdir:"))?.trim();
    let gitdir = dir.join(gitdir);
    if let Ok(common) = fs::read_to_string(gitdir.join("commondir")) {
        let common = gitdir.join(common.trim()).canonicalize().ok()?;
        if common.file_name().is_some_and(|name| name == ".git") {
            return common.parent().map(Path::to_path_buf);
        }
    }
    Some(dir.to_path_buf())
}

/// 작업 폴더부터 위로 올라가며 찾은 `.mcp.json`의 서버 이름. 서버 정의는 읽어도 쓰지 않는다.
fn project_server_names(cwd: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for dir in cwd.ancestors() {
        let Ok(file) = fs::File::open(dir.join(".mcp.json")) else { continue };
        let mut bytes = Vec::new();
        if file.take(PROJECT_FILE_LIMIT).read_to_end(&mut bytes).is_err() {
            continue;
        }
        if let Some(servers) = serde_json::from_slice::<Value>(&bytes).ok().as_ref().and_then(|value| object(value, "mcpServers")) {
            names.extend(servers.keys().cloned());
        }
    }
    names
}

/// 명시적으로 거부한 project 서버는 공식 CLI처럼 같은 이름의 user 서버에 자리를 돌려준다.
/// 2.1.322 실측: disabled는 enabled/enableAll보다 우선한다. 기본 프로필의 거부는 다른 계정에 이식하지 않는다.
fn rejected_project_names(cwd: &Path, profile: &Path, own: Option<&Value>, key: &str) -> Result<BTreeSet<String>, ApiError> {
    fn collect(config: &Value, names: &mut BTreeSet<String>) {
        if let Some(values) = config.get("disabledMcpjsonServers").and_then(Value::as_array) {
            names.extend(values.iter().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    let mut names = BTreeSet::new();
    if let Some(entry) = own.and_then(|config| project(config, key)) {
        collect(entry, &mut names);
    }
    // cwd의 두 설정과 git 루트의 local 설정만 적용된다. 중간·상위 폴더 설정은 적용하지 않는다.
    let root_local = cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .filter(|root| *root != cwd)
        .map(|root| root.join(".claude/settings.local.json"));
    let paths = [
        profile.join("settings.json"),
        cwd.join(".claude/settings.json"),
        cwd.join(".claude/settings.local.json"),
    ];
    for path in paths.into_iter().chain(root_local) {
        if let Some(config) = read_config(&path)? {
            collect(&config, &mut names);
        }
    }
    Ok(names)
}

fn managed_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        PathBuf::from("/Library/Application Support/ClaudeCode")
    }
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var_os("ProgramFiles").unwrap_or_else(|| "C:\\Program Files".into())).join("ClaudeCode")
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        PathBuf::from("/etc/claude-code")
    }
}

/// 관리 설정의 `disableSideloadFlags`는 SDK가 아닌 `--mcp-config`를 시작 때 거부하고, `managed-mcp.json`은 조직이
/// MCP를 독점 관리한다는 뜻이다. 어느 쪽이든 사용자 서버를 덧붙이지 않는다.
/// 읽지 못하는 파일은 정책이 없다고 보지 않고 거부한 것으로 취급해 실행이 막히지 않게 연결을 건너뛴다.
fn sideload_blocked(dir: &Path) -> bool {
    if dir.join("managed-mcp.json").exists() {
        return true;
    }
    let mut files = vec![dir.join("managed-settings.json")];
    if let Ok(entries) = fs::read_dir(dir.join("managed-settings.d")) {
        files.extend(entries.flatten().map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|ext| ext == "json")));
    }
    files.iter().any(|path| match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .map(|value| value.get("disableSideloadFlags").and_then(Value::as_bool) == Some(true))
            .unwrap_or(true),
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    })
}

fn write_private(dir: &Path, body: &[u8]) -> Result<SharedMcp, ApiError> {
    secure::restrict_dir(dir).map_err(|_| failed())?;
    let path = dir.join(format!("{}.json", aam_protocol::new_id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    secure::private_options(&mut options);
    // 파일은 쓰고 바로 닫는다. 열어 둔 핸들이 없으니 Windows에서도 Drop이 지울 수 있고, 공식 CLI가 읽는 데 방해도 없다.
    let written = options.open(&path).and_then(|mut file| {
        file.write_all(body)?;
        file.sync_all()
    });
    if written.is_err() {
        let _ = fs::remove_file(&path);
        return Err(failed());
    }
    Ok(SharedMcp { path })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Windows `canonicalize`의 `\\?\` 경로는 `..`을 해석하지 않아 `commondir`의 상대 경로를 이어 붙일 수 없다.
    fn plain_path(path: &Path) -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(strip_verbatim(&path.to_string_lossy()))
        } else {
            path.to_path_buf()
        }
    }

    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("aam-mcp-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&root).unwrap();
            Self(plain_path(&root.canonicalize().unwrap()))
        }
        fn write(&self, relative: &str, value: Value) -> PathBuf {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, value.to_string()).unwrap();
            path
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn server(tag: &str) -> Value {
        json!({"type":"stdio","command":tag,"args":[],"env":{}})
    }

    fn overlay(file: &SharedMcp) -> Map<String, Value> {
        let value: Value = serde_json::from_slice(&fs::read(&file.path).unwrap()).unwrap();
        value["mcpServers"].as_object().unwrap().clone()
    }

    #[test]
    fn user_and_local_servers_follow_the_project_and_removals_apply_on_next_launch() {
        let root = Root::new();
        let project = root.0.join("work/repo");
        fs::create_dir_all(project.join("sub/deep")).unwrap();
        fs::create_dir_all(project.join(".git")).unwrap();
        let key = path_key(&project);
        let base = root.write(
            "home/.claude.json",
            json!({"oauthAccount":{"emailAddress":"base"},"mcpServers":{"u":server("u")},
                   "projects":{key.clone():{"mcpServers":{"l":server("l")}},"/elsewhere":{"mcpServers":{"other":server("o")}}}}),
        );
        let profile = root.0.join("profile");
        fs::create_dir_all(&profile).unwrap();
        let managed = root.0.join("managed");
        // 실제 앱 폴더처럼 공백이 든 경로여도 `=` 형태 인수가 그 파일을 그대로 가리킨다.
        let dir = root.0.join("Application Support/AI Account Manager/mcp");

        // 하위 폴더에서 시작해도 저장소 루트의 local 서버가 보이고, 다른 프로젝트의 서버는 보이지 않는다.
        let first = prepare_at(&dir, &base, &profile, &project.join("sub/deep"), &managed).unwrap().unwrap();
        assert_eq!(overlay(&first).keys().collect::<Vec<_>>(), ["l", "u"]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(&first.path).unwrap().permissions().mode() & 0o777, 0o600);
        }

        // 새 등록과 제거는 다음 실행에 바로 반영된다. 이전 실행의 파일은 그대로다.
        root.write("home/.claude.json", json!({"mcpServers":{"added":server("a")}}));
        let second = prepare_at(&dir, &base, &profile, &project, &managed).unwrap().unwrap();
        assert_eq!(overlay(&second).keys().collect::<Vec<_>>(), ["added"]);
        assert_eq!(overlay(&first).keys().collect::<Vec<_>>(), ["l", "u"]);

        // 아무것도 없으면 옵션 자체를 붙이지 않고 파일도 만들지 않는다.
        drop(second);
        root.write("home/.claude.json", json!({"mcpServers":{}}));
        assert!(prepare_at(&dir, &base, &profile, &project, &managed).unwrap().is_none());
        fs::remove_file(&base).unwrap();
        assert!(prepare_at(&dir, &base, &profile, &project, &managed).unwrap().is_none());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);

        // 실행이 끝나면 파일이 사라진다.
        let path = first.path.clone();
        drop(first);
        assert!(!path.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn parallel_runs_keep_their_own_files_until_each_one_ends() {
        let root = Root::new();
        let dir = root.0.join("mcp");
        let first = write_private(&dir, br#"{"mcpServers":{"one":{}}}"#).unwrap();
        let second = write_private(&dir, br#"{"mcpServers":{"two":{}}}"#).unwrap();
        assert_ne!(first.path, second.path);

        // 한쪽이 끝나도 다른 실행의 파일은 그 실행이 끝날 때까지 남는다(자기 재실행이 같은 인수를 다시 쓴다).
        let (first_path, second_path) = (first.path.clone(), second.path.clone());
        drop(first);
        assert!(!first_path.exists());
        assert_eq!(overlay(&second).keys().collect::<Vec<_>>(), ["two"]);
        drop(second);
        assert!(!second_path.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn precedence_matches_the_official_cli_and_account_settings_survive() {
        let root = Root::new();
        let project = root.0.join("repo");
        fs::create_dir_all(project.join(".git")).unwrap();
        fs::write(project.join(".mcp.json"), json!({"mcpServers":{"shared":server("project")}}).to_string()).unwrap();
        let key = path_key(&project);
        let base = root.write(
            "home/.claude.json",
            json!({"mcpServers":{"shared":server("user"),"keep":server("user"),"mine":server("base-user"),"off":server("user"),"also":server("user"),"acct-off-user":server("user")},
                   "projects":{key.clone():{"disabledMcpServers":["off"],"mcpServers":{"shared2":server("local"),"mine2":server("base-local"),"also":server("base-local"),"acct-off-local":server("local")}}}}),
        );
        let profile = root.0.join("profile");
        root.write(
            "profile/.claude.json",
            json!({"oauthAccount":{"x":1},"mcpServers":{"mine":server("own-user")},
                   "projects":{key.clone():{"disabledMcpServers":["acct-off-user","acct-off-local"],"mcpServers":{"mine2":server("own-local")}}}}),
        );
        let before = fs::read(profile.join(".claude.json")).unwrap();
        let managed = root.0.join("managed");
        let file = prepare_at(&root.0.join("aam/mcp"), &base, &profile, &project, &managed).unwrap().unwrap();
        let servers = overlay(&file);
        // 프로젝트 .mcp.json의 이름은 user 서버가 덮지 않고, 계정이 가진 이름·기본 설정과 이 계정에서 꺼 둔 서버는 빠진다.
        assert_eq!(servers.keys().collect::<Vec<_>>(), ["also", "keep", "shared2"]);
        // local은 같은 이름의 user보다 앞선다.
        assert_eq!(servers["also"]["command"], "base-local");
        // 계정의 로그인·설정 파일은 읽기만 한다.
        assert_eq!(fs::read(profile.join(".claude.json")).unwrap(), before);
    }

    #[test]
    fn rejected_project_servers_fall_back_to_user_definitions_without_copying_approvals() {
        let root = Root::new();
        let repo = root.0.join("repo");
        let deep = repo.join("a/b");
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&deep).unwrap();
        let key = path_key(&repo);
        root.write("repo/.mcp.json", json!({"mcpServers":{"dup":server("project")}}));
        let base = root.write("home/.claude.json", json!({
            "mcpServers":{"dup":server("user"),"keep":server("keep")},
            "projects":{key.clone():{"disabledMcpjsonServers":["dup"]}}
        }));
        let profile = root.0.join("profile");
        fs::create_dir_all(&profile).unwrap();
        let managed = root.0.join("managed");
        let dir = root.0.join("mcp");
        // 기본 계정에서만 거부한 서버는 다른 계정의 승인 상태를 바꾸지 않는다.
        let file = prepare_at(&dir, &base, &profile, &deep, &managed).unwrap().unwrap();
        assert!(!overlay(&file).contains_key("dup"));
        for (path, cwd, falls_back) in [
            ("profile/settings.json", &deep, true),
            ("repo/.claude/settings.local.json", &deep, true),
            ("repo/.claude/settings.json", &repo, true),
            ("repo/.claude/settings.json", &deep, false),
            ("repo/a/.claude/settings.local.json", &deep, false),
            ("repo/a/.claude/settings.json", &deep, false),
            ("repo/a/b/.claude/settings.local.json", &deep, true),
            ("repo/a/b/.claude/settings.json", &deep, true),
            ("home/.claude/settings.json", &deep, false),
        ] {
            let path = root.write(path, json!({
                "disabledMcpjsonServers":["dup"],
                "enabledMcpjsonServers":["dup"],
                "enableAllProjectMcpServers":true
            }));
            let before = fs::read(&path).unwrap();
            let file = prepare_at(&dir, &base, &profile, cwd, &managed).unwrap().unwrap();
            let servers = overlay(&file);
            assert_eq!(servers.get("dup"), falls_back.then(|| server("user")).as_ref(), "{}", path.display());
            assert_eq!(fs::read(&path).unwrap(), before);
            fs::remove_file(&path).unwrap();
        }
        let own = root.write("profile/.claude.json", json!({
            "oauthAccount":{"id":"kept"},
            "projects":{key.clone():{"disabledMcpjsonServers":["dup"]}}
        }));
        let before = fs::read(&own).unwrap();
        let file = prepare_at(&dir, &base, &profile, &deep, &managed).unwrap().unwrap();
        assert_eq!(overlay(&file)["dup"], server("user"));
        assert_eq!(fs::read(&own).unwrap(), before);
        // MCP 전체 끄기는 project 거부에 따른 user fallback에도 적용된다.
        root.write("profile/.claude.json", json!({
            "projects":{key:{"disabledMcpjsonServers":["dup"],"disabledMcpServers":["dup"]}}
        }));
        let file = prepare_at(&dir, &base, &profile, &deep, &managed).unwrap().unwrap();
        assert!(!overlay(&file).contains_key("dup"));
    }

    #[test]
    fn project_key_follows_git_roots_like_the_official_cli() {
        let root = Root::new();
        let repo = root.0.join("repo");
        fs::create_dir_all(repo.join(".git/worktrees/wt")).unwrap();
        fs::create_dir_all(repo.join("a/b")).unwrap();
        fs::write(repo.join(".git/worktrees/wt/commondir"), "../..\n").unwrap();
        let worktree = root.0.join("wt");
        fs::create_dir_all(&worktree).unwrap();
        fs::write(worktree.join(".git"), format!("gitdir: {}\n", repo.join(".git/worktrees/wt").display())).unwrap();
        let submodule = repo.join("child");
        fs::create_dir_all(&submodule).unwrap();
        fs::write(submodule.join(".git"), "gitdir: ../.git/modules/child\n").unwrap();
        let plain = root.0.join("plain/x");
        fs::create_dir_all(&plain).unwrap();

        assert_eq!(project_key(&repo.join("a/b")), path_key(&repo));
        assert_eq!(project_key(&worktree), path_key(&repo));
        assert_eq!(project_key(&submodule), path_key(&submodule));
        assert_eq!(project_key(&plain), path_key(&plain));
    }

    #[test]
    fn windows_paths_become_the_keys_the_official_cli_writes() {
        // 공식 CLI(Windows 2.1.322 실측)는 작업 폴더 문자열 그대로, 구분자만 `/`로 저장한다.
        assert_eq!(windows_key(r"C:\Users\Administrator\Temp\repo"), "C:/Users/Administrator/Temp/repo");
        assert_eq!(windows_key(r"c:\users\me\repo"), "c:/users/me/repo");
        // `canonicalize`가 붙이는 확장 접두사는 키에 없다.
        assert_eq!(windows_key(r"\\?\C:\Users\Administrator\Temp\repo"), "C:/Users/Administrator/Temp/repo");
        assert_eq!(windows_key(r"\\?\UNC\server\share\repo"), "//server/share/repo");
        assert_eq!(windows_key(r"\\server\share\repo"), "//server/share/repo");
        // 드라이브가 아닌 확장 경로(볼륨 GUID·장치)는 해석할 수 없으니 그대로 둔다.
        assert_eq!(windows_key(r"\\?\Volume{1234}\repo"), "//?/Volume{1234}/repo");
        assert_eq!(windows_key(r"\\?\D:\한글 폴더\repo"), "D:/한글 폴더/repo");
    }

    #[test]
    fn managed_sideload_policy_and_broken_config_never_block_the_launch() {
        let root = Root::new();
        let base = root.write("home/.claude.json", json!({"mcpServers":{"u":server("u")}}));
        let profile = root.0.join("profile");
        fs::create_dir_all(&profile).unwrap();
        let (dir, managed) = (root.0.join("aam/mcp"), root.0.join("managed"));
        assert!(prepare_at(&dir, &base, &profile, &root.0, &managed).unwrap().is_some());

        // `--mcp-config`를 거부하는 관리 정책(드롭인 포함)이 있으면 옵션을 붙이지 않는다.
        root.write("managed/managed-settings.d/10.json", json!({"disableSideloadFlags":true}));
        assert!(prepare_at(&dir, &base, &profile, &root.0, &managed).unwrap().is_none());
        fs::remove_dir_all(&managed).unwrap();
        root.write("managed/managed-settings.json", json!({"disableSideloadFlags":false}));
        assert!(prepare_at(&dir, &base, &profile, &root.0, &managed).unwrap().is_some());

        // 깨진 기본 설정은 오류로 돌려주고(호출자는 경고만 하고 MCP 없이 실행) 내용은 오류에 없다.
        fs::write(&base, "{\"mcpServers\":{\"secret-cmd\":").unwrap();
        let error = prepare_at(&dir, &base, &profile, &root.0, &managed).err().unwrap();
        assert_eq!(error.code, "SHARED_MCP_FAILED");
        assert!(!error.message.contains("secret"));
    }
}
