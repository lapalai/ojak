//! NSIS upgrade transaction. Never treats installer success as proof of daemon replacement.
use super::*;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

const RECEIPT: &str = "installer-update.json";
const BACKUP: &str = "installer-backup";
const FILES: [&str; 2] = ["aam.exe", "aam-service.exe"];
#[derive(Serialize, Deserialize)]
struct Update {
    owner: String,
    directory: PathBuf,
    was_running: bool,
    service_installed: bool,
    files: BTreeMap<String, String>,
    /// prepare 시점의 shim 상태(`state_files()` 중 있던 것)와 hash. 복구 때 이 상태로 그대로 되돌린다.
    /// 없으면(이 필드 이전 기록) 복구는 shim을 건드리지 않는다.
    #[serde(default)]
    state: Option<BTreeMap<String, String>>,
}
/// 실행 파일과 함께 되돌려야 하는 shim 상태. shim 형식은 launcher 버전에 묶여 있다
/// (예: `.cmd`의 `--shim`은 새 launcher만 알아듣는다). AAM_HOME 기준 상대 경로.
fn state_files() -> Vec<String> {
    let mut names = vec!["integration.json".to_owned()];
    for tool in std::iter::once("aam").chain(TOOLS) {
        for ext in ["exe", "cmd"] { names.push(format!("bin/{tool}.{ext}")); }
    }
    names
}
fn state_backup(paths: &Paths, name: &str) -> PathBuf { paths.home.join(BACKUP).join("state").join(name.replace('/', "__")) }
fn hash(path: &Path) -> Result<String, ApiError> {
    let mut file = aam_protocol::secure::open_read_no_follow(path).map_err(io_error)?;
    if !file.metadata().map_err(io_error)?.is_file() { return Err(error("UPDATE_UNSAFE_FILE", "업데이트 파일이 일반 파일이 아닙니다.")); }
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut bytes).map_err(io_error)?;
        if n == 0 { break; }
        hash.update(&bytes[..n]);
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn copy(source: &Path, destination: &Path) -> Result<(), ApiError> {
    let mut input = aam_protocol::secure::open_read_no_follow(source).map_err(io_error)?;
    let temporary = destination.with_extension(format!("update-{}", aam_protocol::new_id()));
    let result = (|| {
        let mut output = fs::OpenOptions::new().write(true).create_new(true).open(&temporary).map_err(io_error)?;
        std::io::copy(&mut input, &mut output).map_err(io_error)?;
        output.flush().map_err(io_error)?;
        output.sync_all().map_err(io_error)?;
        drop(output);
        fs::rename(&temporary, destination).map_err(io_error)
    })();
    if result.is_err() { let _ = fs::remove_file(&temporary); }
    result
}
pub(super) fn directory(path: &Path) -> Result<PathBuf, ApiError> {
    use std::os::windows::fs::MetadataExt;
    if !path.is_absolute() { return Err(error("INVALID_INSTALL_PATH", "설치 폴더는 절대 경로여야 합니다.")); }
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(io_error)?;
        if !metadata.is_dir() || metadata.file_attributes() & 0x400 != 0 {
            return Err(error("INVALID_INSTALL_PATH", "재분석 지점을 통과하는 설치 폴더는 변경하지 않습니다."));
        }
    }
    path.canonicalize().map_err(io_error)
}
fn load(paths: &Paths, directory: &Path) -> Result<Update, ApiError> {
    let update: Update = read_owned(&paths.home.join(RECEIPT))?.ok_or_else(|| error("UPDATE_NOT_PREPARED", "안전 종료가 확인된 업데이트 기록이 없습니다."))?;
    if update.owner != OWNER || update.directory != directory || update.files.keys().any(|name| !FILES.contains(&name.as_str())) {
        return Err(error("FOREIGN_INSTALL", "다른 설치의 업데이트 기록은 사용하지 않습니다."));
    }
    Ok(update)
}
fn refresh_shims(paths: &Paths, source: &Path) -> Result<(), ApiError> {
    let Some(mut integration): Option<Integration> = read_owned(&paths.home.join("integration.json"))? else { return Ok(()); };
    if integration.owner != OWNER || integration.shims.iter().any(|tool| tool != "aam" && !TOOLS.contains(&tool.as_str())) {
        return Err(error("FOREIGN_INSTALL", "소유권을 확인하지 못한 shim은 바꾸지 않습니다."));
    }
    let directory = bin(paths);
    // 업데이트만 하고 연결 설치를 다시 하지 않아도 aam이 PATH에 있게 한다. 사용자 파일이 있으면 덮어쓰지 않는다.
    if !integration.shims.iter().any(|name| name == "aam")
        && !shim_file(&directory, "aam").exists()
        && !legacy_shim(&directory, "aam").exists()
    {
        integration.shims.insert(0, "aam".to_owned());
    }
    for tool in &integration.shims {
        place_shim(source, &directory, tool)?;
    }
    integration.launcher_path = source.to_owned();
    save(&paths.home.join("integration.json"), &integration)
}
fn resume(paths: &Paths, update: &Update) -> Result<(), ApiError> {
    if !update.service_installed || !update.was_running { return Ok(()); }
    let binary = update.directory.join("aam-service.exe");
    start_detached(&binary, paths)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while !running(paths) {
        if Instant::now() >= deadline { return Err(error("SERVICE_START_TIMEOUT", "파일 교체 후 서비스 시작을 확인하지 못했습니다. 복구 기록을 보존했습니다.")); }
        thread::sleep(Duration::from_millis(100));
    }
    let mut record: ServiceRecord = read_owned(&paths.home.join("service-install.json"))?.ok_or_else(|| error("UNMANAGED_SERVICE", "서비스 설치 기록을 확인하지 못했습니다."))?;
    if record.owner != OWNER || record.binary_path.parent().and_then(|path| path.canonicalize().ok()).as_ref() != Some(&update.directory) {
        return Err(error("FOREIGN_INSTALL", "서비스 소유 기록이 변경되어 신규 배정을 재개하지 않았습니다."));
    }
    let permit = record.uninstall_permit.as_ref().ok_or_else(|| error("INVALID_UNINSTALL_PERMIT", "안전 종료 승인이 없어 신규 배정을 재개하지 않았습니다."))?;
    call(paths, "service.cancelUninstall", json!({ "permit": permit }))?;
    record.uninstall_permit = None;
    save(&paths.home.join("service-install.json"), &record)
}
fn clear(paths: &Paths, update: &Update) -> Result<(), ApiError> {
    for name in update.files.keys() { fs::remove_file(paths.home.join(BACKUP).join(name)).map_err(io_error)?; }
    for name in update.state.iter().flat_map(|state| state.keys()) { fs::remove_file(state_backup(paths, name)).map_err(io_error)?; }
    let state_dir = paths.home.join(BACKUP).join("state");
    if state_dir.exists() { fs::remove_dir(&state_dir).map_err(io_error)?; }
    fs::remove_dir(paths.home.join(BACKUP)).map_err(io_error)?;
    fs::remove_file(paths.home.join(RECEIPT)).map_err(io_error)
}
fn snapshot_state(paths: &Paths) -> Result<BTreeMap<String, String>, ApiError> {
    let dir = paths.home.join(BACKUP).join("state");
    fs::create_dir(&dir).map_err(io_error)?;
    private_dir(&dir)?;
    let mut state = BTreeMap::new();
    for name in state_files() {
        let original = paths.home.join(&name);
        if original.try_exists().map_err(io_error)? {
            let digest = hash(&original)?;
            copy(&original, &state_backup(paths, &name))?;
            if hash(&state_backup(paths, &name))? != digest { return Err(error("UPDATE_BACKUP_FAILED", "기존 shim 백업을 확인하지 못했습니다.")); }
            state.insert(name, digest);
        }
    }
    Ok(state)
}
/// prepare 때의 shim 상태로 정확히 되돌린다: 있던 파일은 백업으로 덮고, 없던 파일은 치운다.
fn restore_state(paths: &Paths, state: &BTreeMap<String, String>) -> Result<(), ApiError> {
    for (name, expected) in state {
        if hash(&state_backup(paths, name))? != *expected { return Err(error("UPDATE_BACKUP_FAILED", "shim 복구 파일이 변경되어 자동 복구를 중단했습니다.")); }
    }
    for name in state_files() {
        let target = paths.home.join(&name);
        // 평소 저장과 같은 경로(atomic_write: 현재 사용자 전용 ACL)로 되돌린다. 승격된 설치기가 복구해도
        // 일반 권한 launcher의 소유권 검사(read_owned)를 통과해야 한다.
        if state.contains_key(&name) {
            let mut bytes = Vec::new();
            aam_protocol::secure::open_read_no_follow(&state_backup(paths, &name)).map_err(io_error)?.read_to_end(&mut bytes).map_err(io_error)?;
            atomic_write(&target, &bytes)?;
        } else if target.exists() { remove_shim_file(&target)?; }
    }
    Ok(())
}
fn ensure_launcher_released(install_dir: &Path) -> Result<(), ApiError> {
    let path = install_dir.join("aam.exe");
    // NSIS runs a temporary helper. A direct internal CLI call cannot wait for itself.
    if launcher()? == path { return Ok(()); }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match fs::OpenOptions::new().write(true).open(&path) {
            Ok(_) => return Ok(()),
            Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(failure) if matches!(failure.raw_os_error(), Some(32 | 33)) => {
                if std::time::Instant::now() >= deadline {
                    return Err(error("INSTALLER_FILES_BUSY", "Ojak 실행 파일을 쓰는 프로그램이 남아 있어요. 열려 있는 omp와 Ojak 명령을 종료한 뒤 다시 시도해 주세요. 앱 실행 파일은 바꾸지 않았어요."));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(failure) => return Err(io_error(failure)),
        }
    }
}

pub(super) fn run(paths: &Paths, action: &str, install_dir: &Path) -> Result<(), ApiError> {
    let install_dir = directory(install_dir)?;
    match action {
        "prepare" => {
            if paths.home.join(RECEIPT).exists() || paths.home.join(BACKUP).exists() {
                return Err(error("UPDATE_RECOVERY_REQUIRED", "이전 업데이트 복구 기록이 있습니다. installer recover로 복구한 뒤 다시 설치하세요."));
            }
            let service: Option<ServiceRecord> = read_owned(&paths.home.join("service-install.json"))?;
            let was_running = running(paths);
            if was_running && service.is_none() { return Err(error("UNMANAGED_SERVICE", "등록되지 않은 실행 중 서비스가 있어 업데이트하지 않았습니다.")); }
            if let Some(record) = &service {
                if record.owner != OWNER || record.binary_path.parent().and_then(|path| path.canonicalize().ok()).as_ref() != Some(&install_dir) {
                    return Err(error("FOREIGN_INSTALL", "다른 설치가 관리하는 서비스는 중지하지 않습니다."));
                }
            }
            ensure_launcher_released(&install_dir)?;
            private_dir(&paths.home)?;
            let backup = paths.home.join(BACKUP);
            // Exclusive creation prevents two installers from sharing or overwriting a transaction.
            fs::create_dir(&backup).map_err(io_error)?;
            private_dir(&backup)?;
            let backed_up = (|| -> Result<(BTreeMap<String, String>, BTreeMap<String, String>), ApiError> {
                let mut files = BTreeMap::new();
                for name in FILES {
                    let original = install_dir.join(name);
                    if original.try_exists().map_err(io_error)? {
                        let digest = hash(&original)?;
                        copy(&original, &backup.join(name))?;
                        if hash(&backup.join(name))? != digest { return Err(error("UPDATE_BACKUP_FAILED", "기존 실행 파일 백업을 확인하지 못했습니다.")); }
                        files.insert(name.to_owned(), digest);
                    }
                }
                Ok((files, snapshot_state(paths)?))
            })();
            let (files, state) = match backed_up {
                Ok(value) => value,
                Err(failure) => {
                    // The directory was exclusively created above and no installed file was changed.
                    let _ = fs::remove_dir_all(&backup);
                    return Err(failure);
                }
            };
            let update = Update { owner: OWNER.into(), directory: install_dir, was_running, service_installed: service.is_some(), files, state: Some(state) };
            save(&paths.home.join(RECEIPT), &update)?;
            if update.service_installed {
                if let Err(failure) = service_stop(paths) {
                    // No replacement has occurred. Preserve the original shutdown error.
                    let _ = clear(paths, &update);
                    return Err(failure);
                }
            }
            Ok(())
        }
        "finish" => {
            let update = load(paths, &install_dir)?;
            let staged = launcher()?.parent().ok_or_else(|| error("BINARY_MISSING", "설치 도우미 경로가 없습니다."))?.to_owned();
            for name in FILES {
                if hash(&install_dir.join(name))? != hash(&staged.join(name))? {
                    return Err(error("UPDATE_FILE_MISMATCH", "설치 파일이 새 빌드와 일치하지 않습니다. 서비스를 시작하지 않았습니다."));
                }
            }
            refresh_shims(paths, &install_dir.join("aam.exe"))?;
            resume(paths, &update)?;
            clear(paths, &update)
        }
        "recover" => {
            if !paths.home.join(RECEIPT).exists() { return Ok(()); }
            let update = load(paths, &install_dir)?;
            if running(paths) { service_stop(paths)?; }
            for (name, expected) in &update.files {
                if hash(&paths.home.join(BACKUP).join(name))? != *expected { return Err(error("UPDATE_BACKUP_FAILED", "복구 파일이 변경되어 자동 복구를 중단했습니다.")); }
            }
            for (name, expected) in update.state.iter().flatten() {
                if hash(&state_backup(paths, name))? != *expected { return Err(error("UPDATE_BACKUP_FAILED", "shim 복구 파일이 변경되어 자동 복구를 중단했습니다.")); }
            }
            for name in update.files.keys() { copy(&paths.home.join(BACKUP).join(name), &install_dir.join(name))?; }
            // shim은 복구한 launcher와 같은 세대로 되돌린다. 새 launcher로 다시 만들면 이전 launcher가 모르는 형식이 된다.
            match &update.state {
                Some(state) => restore_state(paths, state)?,
                None if update.files.contains_key("aam.exe") => refresh_shims(paths, &install_dir.join("aam.exe"))?,
                None => {}
            }
            resume(paths, &update)?;
            clear(paths, &update)
        }
        "remove" => {
            // NSIS는 `aam installer remove`를 부른다. 바이너리가 deactivate::run으로 broker 블록을
            // 먼저 지운 뒤에 이 단계로 앱 자동 실행만 지운다. 여기서 서비스를 먼저 끄면 omp 로그인이 깨진다.
            ensure_launcher_released(&install_dir)?;
            super::remove_app_autostart(&install_dir)
        }
        _ => Err(error("INVALID_ARGUMENT", "지원하지 않는 설치 단계입니다.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture { root: PathBuf, app: PathBuf, paths: Paths }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("aam-update-{}", aam_protocol::new_id()));
            let app = root.join("app");
            let home = root.join("state");
            private_dir(&app).unwrap();
            private_dir(&home).unwrap();
            let paths = Paths { profiles: home.join("profiles"), socket: home.join("socket"), database: home.join("state.sqlite"), home };
            Self { root, app, paths }
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = fs::remove_dir_all(&self.root); } }
    #[test]
    fn locked_launcher_blocks_update_before_creating_recovery_state() {
        use std::os::windows::fs::OpenOptionsExt;
        let fixture = Fixture::new();
        let executable = fixture.app.join("aam.exe");
        fs::write(&executable, b"original launcher").unwrap();
        let held = fs::OpenOptions::new().read(true).share_mode(0).open(&executable).unwrap();
        let failure = run(&fixture.paths, "prepare", &fixture.app).unwrap_err();
        assert_eq!(failure.code, "INSTALLER_FILES_BUSY");
        assert!(!fixture.paths.home.join(RECEIPT).exists());
        assert!(!fixture.paths.home.join(BACKUP).exists());
        drop(held);
        assert_eq!(fs::read(&executable).unwrap(), b"original launcher");
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        run(&fixture.paths, "recover", &fixture.app).unwrap();
        assert_eq!(fs::read(&executable).unwrap(), b"original launcher");
    }
    #[test]
    fn interrupted_pair_replacement_restores_originals_without_enrolling_service() {
        let fixture = Fixture::new();
        fs::write(fixture.app.join("aam.exe"), b"original launcher").unwrap();
        fs::write(fixture.app.join("aam-service.exe"), b"original service").unwrap();
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        assert_eq!(run(&fixture.paths, "prepare", &fixture.app).unwrap_err().code, "UPDATE_RECOVERY_REQUIRED");
        fs::write(fixture.app.join("aam.exe"), b"new launcher").unwrap();
        fs::write(fixture.app.join("aam-service.exe"), b"incomplete payload").unwrap();
        run(&fixture.paths, "recover", &fixture.app).unwrap();
        assert_eq!(fs::read(fixture.app.join("aam.exe")).unwrap(), b"original launcher");
        assert_eq!(fs::read(fixture.app.join("aam-service.exe")).unwrap(), b"original service");
        assert!(!fixture.paths.home.join("service-install.json").exists());
        assert!(!fixture.paths.home.join(RECEIPT).exists());
    }
    #[test]
    fn altered_backup_is_rejected_before_either_file_is_restored() {
        let fixture = Fixture::new();
        for name in FILES { fs::write(fixture.app.join(name), b"original").unwrap(); }
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        for name in FILES { fs::write(fixture.app.join(name), b"new").unwrap(); }
        fs::write(fixture.paths.home.join(BACKUP).join("aam-service.exe"), b"changed backup").unwrap();
        assert_eq!(run(&fixture.paths, "recover", &fixture.app).unwrap_err().code, "UPDATE_BACKUP_FAILED");
        for name in FILES { assert_eq!(fs::read(fixture.app.join(name)).unwrap(), b"new"); }
        assert!(fixture.paths.home.join(RECEIPT).exists());
    }
    #[test]
    fn failed_upgrade_restores_shims_of_the_restored_launcher_generation() {
        let fixture = Fixture::new();
        for name in FILES { fs::write(fixture.app.join(name), b"old").unwrap(); }
        let bin = fixture.paths.home.join("bin");
        private_dir(&bin).unwrap();
        // 이전 세대: launcher 복사본 shim과 그 기록.
        fs::write(bin.join("claude.exe"), b"old launcher copy").unwrap();
        let record = br#"{"owner":"ai-account-manager","version":2,"launcherPath":"C:\\old\\aam.exe","nativeBinaries":{},"shims":["claude"]}"#;
        fs::write(fixture.paths.home.join("integration.json"), record).unwrap();
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        // 새 설치가 shim을 새 형식으로 바꾼 뒤 실패한 상황.
        fs::remove_file(bin.join("claude.exe")).unwrap();
        fs::write(bin.join("claude.cmd"), b"@\"aam.exe\" --shim claude %*").unwrap();
        fs::write(fixture.paths.home.join("integration.json"), b"{\"changed\":true}").unwrap();
        for name in FILES { fs::write(fixture.app.join(name), b"new").unwrap(); }
        run(&fixture.paths, "recover", &fixture.app).unwrap();
        assert_eq!(fs::read(bin.join("claude.exe")).unwrap(), b"old launcher copy");
        assert!(!bin.join("claude.cmd").exists(), "the restored launcher does not understand --shim");
        assert_eq!(fs::read(fixture.paths.home.join("integration.json")).unwrap(), record);
        // 일반 권한 launcher가 쓰는 소유권 검사를 통과해야 한다.
        assert!(read_owned::<Integration>(&fixture.paths.home.join("integration.json")).unwrap().is_some());
        assert!(!fixture.paths.home.join(BACKUP).exists());
        assert!(!fixture.paths.home.join(RECEIPT).exists());
    }
}
