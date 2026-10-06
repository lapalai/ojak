use aam_protocol::{ApiError, Paths};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
#[cfg(unix)]
use std::os::{
    fd::AsRawFd,
    unix::{fs::PermissionsExt, process::CommandExt},
};

const OUTPUT_LIMIT: usize = 2 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);
/// `omp usage`는 로그인한 모든 공급자의 한도 API를 차례로 부른다. Windows 실측 27초(계정 5개)라 일반 제한으로는 매번 시간 초과가 나
/// 계정이 `QUOTA_STALE`로 배정에서 빠진다. 서비스 주기(60초) 안에서 끝나도록 이 조회만 길게 기다린다.
const USAGE_PROBE_TIMEOUT: Duration = Duration::from_secs(50);

pub(crate) fn search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|v| {
            std::env::split_paths(&v)
                .filter(|p| p.is_absolute())
                .collect()
        })
        .unwrap_or_default();
    #[cfg(windows)]
    {
        // 공식 설치 위치(실측): claude.exe는 `.local\bin`, codex는 npm 전역(`%APPDATA%\npm`), omp는 `%LOCALAPPDATA%\omp`.
        if let Some(home) = std::env::var_os("USERPROFILE").map(PathBuf::from) {
            dirs.push(home.join(".local").join("bin"));
        }
        if let Some(roaming) = std::env::var_os("APPDATA").map(PathBuf::from) {
            dirs.push(roaming.join("npm"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
            dirs.push(local.join("omp"));
        }
        return dirs;
    }
    #[cfg(not(windows))]
    if let Some(home) = aam_protocol::user_home() {
        dirs.extend([
            home.join(".local/bin"),
            home.join(".bun/bin"),
            home.join(".cargo/bin"),
            home.join("bin"),
        ]);
        if let Ok(entries) = fs::read_dir(home.join(".nvm/versions/node")) {
            let mut versions: Vec<_> = entries.flatten().map(|e| e.path().join("bin")).collect();
            versions.sort();
            versions.reverse();
            dirs.extend(versions);
        }
    }
    #[cfg(not(windows))]
    dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"].map(PathBuf::from));
    #[cfg(not(windows))]
    return dirs;
}

/// 실행 가능한 파일인지. Unix는 실행 비트, Windows는 확장자(PATHEXT의 실행 형식)로 판단한다.
fn runnable(path: &Path, metadata: &fs::Metadata) -> bool {
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        let _ = path;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        path.extension()
            .and_then(|v| v.to_str())
            .is_some_and(|ext| ["exe", "cmd", "bat", "com"].contains(&ext.to_ascii_lowercase().as_str()))
    }
}

/// 디렉터리 안에서 도구 이름에 맞는 실행 파일 후보. Windows는 `claude.exe`·`codex.cmd`처럼 확장자를 붙인다.
fn tool_candidates(dir: &Path, tool: &str) -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        ["exe", "cmd", "bat", "com"]
            .iter()
            .map(|ext| dir.join(format!("{tool}.{ext}")))
            .collect()
    }
    #[cfg(not(windows))]
    {
        vec![dir.join(tool)]
    }
}

pub(crate) fn executable(path: &Path) -> Result<PathBuf, ApiError> {
    if !path.is_absolute() {
        return Err(ApiError::new(
            "BINARY_INVALID",
            "공식 CLI의 절대 경로가 필요합니다.",
        ));
    }
    let canonical = path.canonicalize().map_err(|_| {
        ApiError::new(
            "BINARY_MISSING",
            "연결된 공식 CLI를 찾지 못했습니다. 연결을 다시 확인해 주세요.",
        )
    })?;
    let metadata = fs::metadata(&canonical)
        .map_err(|_| ApiError::new("BINARY_MISSING", "공식 CLI 정보를 읽지 못했습니다."))?;
    if !runnable(&canonical, &metadata) {
        return Err(ApiError::new(
            "BINARY_INVALID",
            "공식 CLI 경로가 실행 파일이 아닙니다.",
        ));
    }
    let name = canonical.file_name().and_then(|v| v.to_str()).unwrap_or("");
    if name == "aam"
        || name.starts_with("aam-")
        || canonical.components().any(|c| c.as_os_str() == "shims")
    {
        return Err(ApiError::new(
            "SHIM_RECURSION",
            "관리자 shim 대신 공식 CLI 원본을 연결해 주세요.",
        ));
    }
    #[cfg(windows)]
    {
        // Windows shims are batch files or (older) executable copies; canonicalization cannot expose aam.exe.
        // A different AAM_HOME's owned manifest still proves that this bin entry is a shim.
        let owned_shim = canonical.parent().filter(|dir| dir.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.eq_ignore_ascii_case("bin")))
            .and_then(Path::parent)
            .and_then(|home| aam_protocol::secure::open_read_no_follow(&home.join("integration.json")).ok())
            .and_then(|file| {
                let mut bytes = Vec::new();
                file.take(64 * 1024 + 1).read_to_end(&mut bytes).ok()?;
                (bytes.len() <= 64 * 1024).then(|| serde_json::from_slice::<Value>(&bytes).ok()).flatten()
            })
            .is_some_and(|manifest| manifest["owner"] == "ai-account-manager"
                && manifest["shims"].as_array().is_some_and(|shims| shims.iter().any(|shim| {
                    shim.as_str().is_some_and(|tool| canonical.file_stem().and_then(|name| name.to_str()).is_some_and(|name| name.eq_ignore_ascii_case(tool)))
                })));
        if owned_shim {
            return Err(ApiError::new("SHIM_RECURSION", "다른 설치의 관리자 shim 대신 공식 CLI 원본을 연결해 주세요."));
        }
    }
    let mut prefix = [0u8; 8192];
    let count = fs::File::open(&canonical)
        .and_then(|mut f| f.read(&mut prefix))
        .unwrap_or(0);
    // 스크립트 shim(shebang, Windows 배치)에 관리자 흔적이 있으면 원본으로 쓰지 않는다.
    let batch = canonical
        .extension()
        .and_then(|v| v.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"));
    if prefix.starts_with(b"#!") || batch {
        let script = String::from_utf8_lossy(&prefix[..count]);
        if script.contains("AAM_")
            || script.contains("aam run")
            || script.contains("/aam\"")
            || script.contains("aam shim")
            || script.contains("aam.exe\" --shim ")
        {
            return Err(ApiError::new(
                "SHIM_RECURSION",
                "관리자 shim을 공식 CLI로 사용할 수 없습니다.",
            ));
        }
    }
    Ok(canonical)
}

/// 원본 CLI의 진입 경로(symlink 포함)를 반환합니다. 실행 대상은 쓸 때마다 다시 해석하므로
/// 공식 CLI의 자체 업데이트가 버전별 경로를 바꿔도 그대로 따라갑니다.
pub(crate) fn discover(paths: &Paths, tool: &str) -> Option<PathBuf> {
    let manifest = fs::File::open(paths.home.join("integration.json"))
        .ok()
        .and_then(|file| {
            let mut bytes = Vec::new();
            file.take(64 * 1024 + 1).read_to_end(&mut bytes).ok()?;
            if bytes.len() > 64 * 1024 {
                return None;
            }
            serde_json::from_slice::<Value>(&bytes).ok()
        });
    let launcher = manifest
        .as_ref()
        .and_then(|v| v.get("launcherPath"))
        .and_then(Value::as_str)
        .and_then(|p| Path::new(p).canonicalize().ok());
    let own = std::env::current_exe()
        .ok()
        .and_then(|p| p.canonicalize().ok());
    let native = |path: &Path| {
        executable(path)
            .ok()
            .filter(|p| Some(p) != launcher.as_ref() && Some(p) != own.as_ref())
            .map(|_| path.to_path_buf())
    };
    // 판이 없는 이전 기록은 해석된 버전별 경로라 업데이트 뒤에도 옛 버전을 가리킵니다.
    if let Some(path) = manifest
        .as_ref()
        .filter(|v| {
            v.get("version").and_then(Value::as_u64)
                == Some(u64::from(aam_protocol::INTEGRATION_VERSION))
        })
        .and_then(|v| v.get("nativeBinaries"))
        .and_then(|v| v.get(tool))
        .and_then(Value::as_str)
    {
        if let Some(path) = native(Path::new(path)) {
            return Some(path);
        }
    }
    search_dirs()
        .into_iter()
        .filter(|p| !p.starts_with(paths.home.join("shims")) && !p.starts_with(paths.home.join("bin")))
        .find_map(|dir| tool_candidates(&dir, tool).into_iter().find_map(|path| native(&path)))
}

pub(crate) fn base_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Ok(path) = std::env::join_paths(search_dirs()) {
        env.insert("PATH".into(), path.to_string_lossy().into_owned());
    }
    env
}

pub(crate) struct Probe {
    child: Child,
    output: Output,
    buffer: Vec<u8>,
    received: usize,
    deadline: Instant,
    /// Windows: 조회 자식과 손자를 한 Job에 묶어, 닫을 때 모두 종료한다(Unix의 프로세스 그룹 SIGKILL에 해당).
    #[cfg(windows)]
    _job: aam_protocol::ProcessJob,
}

/// 조회 출력 채널. Unix는 non-blocking fd를 직접 읽고, Windows 익명 파이프는 non-blocking이 없어
/// 별도 스레드가 읽어 채널로 넘긴다. 어느 쪽이든 `pump`가 전체 시간 제한을 지킨다.
enum Output {
    #[cfg_attr(windows, allow(dead_code))]
    Direct(ChildStdout),
    #[cfg_attr(unix, allow(dead_code))]
    Thread(std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>),
}

impl Probe {
    pub(crate) fn spawn(
        program: &Path,
        args: &[&str],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Result<Self, ApiError> {
        let mut command = Command::new(program);
        command
            .args(args)
            .env_remove("CLAUDE_CONFIG_DIR")
            .envs(env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let start_failed = || {
            ApiError::new(
                "PROBE_START_FAILED",
                "공식 CLI 상태 조회를 시작하지 못했습니다. 설치 및 실행 권한을 확인해 주세요.",
            )
        };
        #[cfg(unix)]
        let mut child = command.process_group(0).spawn().map_err(|_| start_failed())?;
        #[cfg(windows)]
        let (mut child, job) =
            aam_protocol::spawn_in_job(&mut command, true, true, false).map_err(|_| start_failed())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ApiError::new("PROBE_IO", "공식 CLI 조회 채널을 열지 못했습니다."))?;
        #[cfg(unix)]
        let output = {
            let fd = stdout.as_raw_fd();
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                let _ = child.kill();
                let _ = child.wait();
                return Err(ApiError::new(
                    "PROBE_IO",
                    "공식 CLI 조회 시간 제한을 설정하지 못했습니다.",
                ));
            }
            Output::Direct(stdout)
        };
        #[cfg(windows)]
        let output = {
            let (sender, receiver) = std::sync::mpsc::channel();
            let mut stdout = stdout;
            thread::spawn(move || {
                let mut bytes = [0u8; 8192];
                loop {
                    let result = stdout.read(&mut bytes).map(|n| bytes[..n].to_vec());
                    let done = !matches!(&result, Ok(chunk) if !chunk.is_empty());
                    if sender.send(result).is_err() || done {
                        break;
                    }
                }
            });
            Output::Thread(receiver)
        };
        Ok(Self {
            child,
            output,
            buffer: Vec::new(),
            received: 0,
            deadline: Instant::now() + PROBE_TIMEOUT,
            #[cfg(windows)]
            _job: job,
        })
    }

    /// 한 번 읽는다. 아직 데이터가 없으면 Ok(None).
    fn read_chunk(&mut self, bytes: &mut [u8; 8192]) -> std::io::Result<Option<usize>> {
        match &mut self.output {
            Output::Direct(stdout) => match stdout.read(bytes) {
                Ok(n) => Ok(Some(n)),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
                Err(e) => Err(e),
            },
            Output::Thread(receiver) => {
                let wait = self.deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(50));
                match receiver.recv_timeout(wait) {
                    Ok(Ok(chunk)) => {
                        let n = chunk.len().min(bytes.len());
                        bytes[..n].copy_from_slice(&chunk[..n]);
                        Ok(Some(n))
                    }
                    Ok(Err(e)) => Err(e),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Ok(None),
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Ok(Some(0)),
                }
            }
        }
    }

    fn pump(&mut self) -> Result<bool, ApiError> {
        if Instant::now() >= self.deadline {
            return Err(ApiError::new("PROBE_TIMEOUT", "공식 CLI 상태 조회 시간이 초과되었습니다. 네트워크와 native 로그인을 확인한 뒤 새로고침해 주세요."));
        }
        let mut bytes = [0u8; 8192];
        match self.read_chunk(&mut bytes) {
            Ok(Some(0)) => Ok(false),
            Ok(Some(n)) => {
                self.received += n;
                if self.received > OUTPUT_LIMIT {
                    return Err(ApiError::new(
                        "PROBE_OUTPUT_LIMIT",
                        "공식 CLI 상태 응답이 허용 크기를 초과했습니다.",
                    ));
                }
                self.buffer.extend_from_slice(&bytes[..n]);
                Ok(true)
            }
            Ok(None) => {
                if matches!(self.output, Output::Direct(_)) {
                    thread::sleep(Duration::from_millis(15));
                }
                Ok(true)
            }
            Err(_) => Err(ApiError::new(
                "PROBE_IO",
                "공식 CLI 상태 응답을 읽지 못했습니다.",
            )),
        }
    }

    pub(crate) fn send(&mut self, value: &Value) -> Result<(), ApiError> {
        let input = self
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| ApiError::new("PROBE_IO", "공식 CLI 조회 채널이 닫혔습니다."))?;
        serde_json::to_writer(&mut *input, value)
            .map_err(|_| ApiError::new("PROBE_IO", "상태 조회 요청을 전달하지 못했습니다."))?;
        input
            .write_all(b"\n")
            .and_then(|_| input.flush())
            .map_err(|_| ApiError::new("PROBE_IO", "상태 조회 요청을 전달하지 못했습니다."))
    }

    pub(crate) fn request(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value, ApiError> {
        self.send(&json!({"id": id, "method": method, "params": params}))?;
        loop {
            while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
                let line: Vec<_> = self.buffer.drain(..=end).collect();
                let Ok(value) = serde_json::from_slice::<Value>(&line) else {
                    continue;
                };
                if value.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if value.get("error").is_some() {
                    return Err(ApiError::new("NATIVE_METADATA_UNAVAILABLE", "공식 CLI가 계정 메타데이터 조회를 거부했습니다. native 로그인 또는 CLI 버전을 확인해 주세요."));
                }
                return value.get("result").cloned().ok_or_else(|| {
                    ApiError::new(
                        "PROBE_SCHEMA",
                        "공식 CLI 메타데이터 형식을 확인할 수 없습니다.",
                    )
                });
            }
            if !self.pump()? {
                return Err(ApiError::new(
                    "PROBE_CLOSED",
                    "공식 CLI가 메타데이터 응답 전에 종료되었습니다.",
                ));
            }
        }
    }

    pub(crate) fn output(mut self) -> Result<Value, ApiError> {
        self.child.stdin.take();
        while self.pump()? {}
        // 미로그인 상태도 JSON으로 제공하는 CLI가 있어 exit code 대신 구조화 응답을 해석합니다.
        serde_json::from_slice(&self.buffer).map_err(|_| {
            ApiError::new(
                "PROBE_SCHEMA",
                "공식 CLI가 예상한 JSON 상태를 제공하지 않았습니다. CLI 버전을 확인해 주세요.",
            )
        })
    }

    fn version(mut self) -> Result<String, ApiError> {
        self.child.stdin.take();
        while self.pump()? {}
        let text = std::str::from_utf8(&self.buffer)
            .map_err(|_| ApiError::new("PROBE_SCHEMA", "버전 응답이 UTF-8이 아닙니다."))?;
        let line = text.lines().next().unwrap_or("").trim();
        if line.is_empty()
            || line.len() > 160
            || !line.chars().any(|c| c.is_ascii_digit())
            || !line
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || " .-_()[]".contains(c))
        {
            return Err(ApiError::new(
                "PROBE_SCHEMA",
                "공식 CLI 버전 형식을 확인하지 못했습니다.",
            ));
        }
        Ok(line.to_string())
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.child.stdin.take();
        // 조회 전용 자식 그룹만 정리합니다. app-server나 node wrapper의 자식을 남기지 않습니다.
        // Windows는 Probe가 버려질 때 Job 핸들이 닫히며 Job 안의 모든 프로세스가 종료됩니다.
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) fn run_json(
    program: &Path,
    args: &[&str],
    env: &BTreeMap<String, String>,
    cwd: &Path,
) -> Result<Value, ApiError> {
    Probe::spawn(program, args, env, cwd)?.output()
}

/// `omp usage --json`처럼 여러 원격 API를 순회하는 느린 조회. 시간 제한만 `USAGE_PROBE_TIMEOUT`으로 늘린다.
pub(crate) fn run_json_slow(
    program: &Path,
    args: &[&str],
    env: &BTreeMap<String, String>,
    cwd: &Path,
) -> Result<Value, ApiError> {
    let mut probe = Probe::spawn(program, args, env, cwd)?;
    probe.deadline = Instant::now() + USAGE_PROBE_TIMEOUT;
    probe.output()
}

pub(crate) fn version(program: &Path, cwd: &Path) -> Option<String> {
    Probe::spawn(program, &["--version"], &base_env(), cwd)
        .and_then(Probe::version)
        .ok()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn native(path: &Path) {
        fs::write(path, b"\x7fELF").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn discover_follows_self_update_of_registered_entry() {
        let dir = std::env::temp_dir().join(format!("aam-discover-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(dir.join("versions")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let (old, new) = (dir.join("versions/1.0.0"), dir.join("versions/1.0.1"));
        native(&old);
        native(&new);
        // 실제 PATH의 설치본과 섞이지 않도록 존재하지 않는 도구 이름을 씁니다.
        let tool = "aam-test-tool";
        let entry = dir.join(tool);
        symlink(&old, &entry).unwrap();
        let paths = Paths {
            home: dir.clone(),
            profiles: dir.join("profiles"),
            socket: dir.join("socket"),
            database: dir.join("unused.sqlite"),
        };
        let manifest = |version: Option<u32>, binary: &Path| {
            let mut value = json!({"launcherPath": "/nonexistent/aam", "nativeBinaries": {tool: binary}});
            if let Some(version) = version {
                value["version"] = json!(version);
            }
            fs::write(dir.join("integration.json"), value.to_string()).unwrap();
        };

        manifest(Some(aam_protocol::INTEGRATION_VERSION), &entry);
        let found = discover(&paths, tool).unwrap();
        assert_eq!(found, entry);
        assert_eq!(executable(&found).unwrap(), old);
        // 공식 설치 프로그램이 진입 링크를 새 버전으로 바꾼 상황입니다.
        fs::remove_file(&entry).unwrap();
        symlink(&new, &entry).unwrap();
        assert_eq!(executable(&discover(&paths, tool).unwrap()).unwrap(), new);

        // 판 없는 이전 기록의 버전별 경로는 원본 탐색에 쓰지 않습니다.
        manifest(None, &old);
        assert_eq!(discover(&paths, tool), None);
        fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn another_home_owned_executable_copy_is_not_a_native_cli() {
        let root = std::env::temp_dir().join(format!("aam-shim-detection-{}", uuid::Uuid::new_v4()));
        let bin = root.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let executable_path = bin.join("codex.exe");
        fs::copy(std::env::current_exe().unwrap(), &executable_path).unwrap();
        // An executable's name or bin directory alone is not evidence of a manager shim.
        assert!(executable(&executable_path).is_ok());
        fs::write(root.join("integration.json"), serde_json::to_vec(&json!({
            "owner": "ai-account-manager", "version": 2, "shims": ["codex"]
        })).unwrap()).unwrap();
        assert_eq!(executable(&executable_path).unwrap_err().code, "SHIM_RECURSION");
        fs::remove_dir_all(root).unwrap();
    }
}
