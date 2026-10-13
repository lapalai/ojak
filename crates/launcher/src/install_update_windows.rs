//! NSIS upgrade transaction. Never treats installer success as proof of daemon replacement.
use super::*;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

#[path = "install_update_blockers_windows.rs"]
mod blockers;

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
    /// 파일 검증·서비스 재개가 끝났다. 중단된 백업 정리는 되돌리지 않고 이어서 끝낸다.
    #[serde(default)]
    committed: bool,
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
    if let Some(state) = &update.state {
        let known = state_files();
        if state.keys().any(|name| !known.contains(name)) {
            return Err(error("FOREIGN_INSTALL", "확인되지 않은 shim 경로의 복구 기록은 사용하지 않습니다."));
        }
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
fn remove_if_present(path: &Path) -> Result<(), ApiError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(failure) => Err(io_error(failure)),
    }
}
fn clear(paths: &Paths, update: &Update) -> Result<(), ApiError> {
    let backup = paths.home.join(BACKUP);
    if backup.try_exists().map_err(io_error)? {
        directory(&backup)?;
        let state_dir = backup.join("state");
        if state_dir.try_exists().map_err(io_error)? { directory(&state_dir)?; }
        for name in update.files.keys() { remove_if_present(&backup.join(name))?; }
        for name in update.state.iter().flat_map(|state| state.keys()) { remove_if_present(&state_backup(paths, name))?; }
        if state_dir.try_exists().map_err(io_error)? { fs::remove_dir(&state_dir).map_err(io_error)?; }
        fs::remove_dir(&backup).map_err(io_error)?;
    }
    remove_if_present(&paths.home.join(RECEIPT))
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
    for name in state_files() {
        let target = paths.home.join(&name);
        // 평소 저장과 같은 경로(atomic_write: 현재 사용자 전용 ACL)로 되돌린다. 승격된 설치기가 복구해도
        // 일반 권한 launcher의 소유권 검사(read_owned)를 통과해야 한다.
        let backup = state_backup(paths, &name);
        if state.contains_key(&name) {
            if backup.try_exists().map_err(io_error)? {
                let mut bytes = Vec::new();
                aam_protocol::secure::open_read_no_follow(&backup).map_err(io_error)?.read_to_end(&mut bytes).map_err(io_error)?;
                atomic_write(&target, &bytes)?;
            }
        } else if target.exists() { remove_shim_file(&target)?; }
    }
    Ok(())
}

fn verify_restore(backup: &Path, original: &Path, expected: &str) -> Result<(), ApiError> {
    let source = if backup.try_exists().map_err(io_error)? { backup } else { original };
    if !source.try_exists().map_err(io_error)? || hash(source)? != expected {
        return Err(error("UPDATE_BACKUP_FAILED", "복구 파일이나 이미 복구된 원본을 확인하지 못했습니다. 파일과 복구 기록을 보존했습니다."));
    }
    Ok(())
}

/// receipt를 쓰기 전에 중단된 prepare만 정리한다. 현재 파일과 같은 백업만 지우며 알 수 없는 파일은 보존한다.
fn clear_orphan(paths: &Paths, install_dir: &Path) -> Result<(), ApiError> {
    let backup = paths.home.join(BACKUP);
    if !backup.try_exists().map_err(io_error)? { return Ok(()); }
    directory(&backup)?;
    let known_state = state_files();
    let mut removable = Vec::new();
    let state_dir = backup.join("state");
    for entry in fs::read_dir(&backup).map_err(io_error)? {
        let entry = entry.map_err(io_error)?;
        if entry.file_name() == "state" {
            directory(&state_dir)?;
            for entry in fs::read_dir(&state_dir).map_err(io_error)? {
                let entry = entry.map_err(io_error)?;
                let path = entry.path();
                let encoded = path.file_name().and_then(|name| name.to_str())
                    .ok_or_else(|| error("UPDATE_BACKUP_FAILED", "확인되지 않은 백업 파일이 있어 복구 기록을 보존했습니다."))?;
                let name = known_state.iter().find(|name| name.split('/').eq(encoded.split("__")))
                    .ok_or_else(|| error("UPDATE_BACKUP_FAILED", "확인되지 않은 백업 파일이 있어 복구 기록을 보존했습니다."))?;
                let original = paths.home.join(name);
                if !original.try_exists().map_err(io_error)? || hash(&path)? != hash(&original)? {
                    return Err(error("UPDATE_BACKUP_FAILED", "영수증 없는 shim 백업이 현재 파일과 달라 보존했습니다."));
                }
                removable.push(path);
            }
        } else {
            let name = entry.file_name();
            let name = name.to_str().filter(|name| FILES.contains(name))
                .ok_or_else(|| error("UPDATE_BACKUP_FAILED", "확인되지 않은 백업 파일이 있어 복구 기록을 보존했습니다."))?;
            let original = install_dir.join(name);
            if !original.try_exists().map_err(io_error)? || hash(&entry.path())? != hash(&original)? {
                return Err(error("UPDATE_BACKUP_FAILED", "영수증 없는 실행 파일 백업이 현재 파일과 달라 보존했습니다."));
            }
            removable.push(entry.path());
        }
    }
    for path in removable { fs::remove_file(path).map_err(io_error)?; }
    if state_dir.try_exists().map_err(io_error)? { fs::remove_dir(state_dir).map_err(io_error)?; }
    fs::remove_dir(backup).map_err(io_error)
}
fn files_busy() -> ApiError {
    error("INSTALLER_FILES_BUSY", "Ojak 실행 파일을 쓰는 프로그램이 남아 있어요. 열려 있는 omp와 Ojak 명령을 종료한 뒤 다시 시도해 주세요. 앱 실행 파일은 바꾸지 않았어요.")
}
/// `aam.exe`를 쓰기로 열 수 있으면 true. 사용 중(32·33)이면 `wait` 동안 기다린 뒤 false. 그 밖의 오류는 그대로 낸다.
fn released(install_dir: &Path, wait: std::time::Duration) -> Result<bool, ApiError> {
    let path = install_dir.join("aam.exe");
    let deadline = std::time::Instant::now() + wait;
    loop {
        match fs::OpenOptions::new().write(true).open(&path) {
            Ok(_) => return Ok(true),
            Err(failure) if failure.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(failure) if matches!(failure.raw_os_error(), Some(32 | 33)) => {
                if std::time::Instant::now() >= deadline { return Ok(false); }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(failure) => return Err(io_error(failure)),
        }
    }
}
fn ensure_launcher_released(install_dir: &Path) -> Result<(), ApiError> {
    // NSIS runs a temporary helper. A direct internal CLI call cannot wait for itself.
    if launcher()? == install_dir.join("aam.exe") { return Ok(()); }
    if released(install_dir, std::time::Duration::from_secs(5))? { Ok(()) } else { Err(files_busy()) }
}
/// `prepare-interactive` 전용. 조용한 `prepare`는 이 경로를 거치지 않는다. 사용자 동의 없이는 프로세스를 닫지 않으며,
/// 동의가 끝난 뒤에도 같은 쓰기 잠금 확인으로 다시 검증한다.
fn release_launcher_interactively(install_dir: &Path) -> Result<(), ApiError> {
    match ensure_launcher_released(install_dir) {
        Err(failure) if failure.code == "INSTALLER_FILES_BUSY" => {
            blockers::negotiate(&mut blockers::System::new(install_dir)?, &mut blockers::Native)?;
            ensure_launcher_released(install_dir)
        }
        other => other,
    }
}

fn owned_service(paths: &Paths, install_dir: &Path) -> Result<Option<ServiceRecord>, ApiError> {
    let service: Option<ServiceRecord> = read_owned(&paths.home.join("service-install.json"))?;
    if let Some(record) = &service {
        if record.owner != OWNER || record.binary_path.parent().and_then(|path| path.canonicalize().ok()).as_deref() != Some(install_dir) {
            return Err(error("FOREIGN_INSTALL", "다른 설치가 관리하는 서비스는 중지하지 않습니다."));
        }
    }
    Ok(service)
}

pub(super) fn run(paths: &Paths, action: &str, install_dir: &Path) -> Result<(), ApiError> {
    let install_dir = directory(install_dir)?;
    match action {
        "prepare" | "prepare-interactive" => {
            if paths.home.join(RECEIPT).exists() || paths.home.join(BACKUP).exists() {
                return Err(error("UPDATE_RECOVERY_REQUIRED", "이전 업데이트 복구 기록이 있습니다. installer recover로 복구한 뒤 다시 설치하세요."));
            }
            let service = owned_service(paths, &install_dir)?;
            let was_running = running(paths);
            if was_running && service.is_none() { return Err(error("UNMANAGED_SERVICE", "등록되지 않은 실행 중 서비스가 있어 업데이트하지 않았습니다.")); }
            // 조용한 prepare는 닫지 않고 실패한다. 동의 UI와 종료는 prepare-interactive에서만, 백업·영수증을 만들기 전에 일어난다.
            if action == "prepare-interactive" { release_launcher_interactively(&install_dir)?; } else { ensure_launcher_released(&install_dir)?; }
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
            let update = Update { owner: OWNER.into(), directory: install_dir, was_running, service_installed: service.is_some(), files, state: Some(state), committed: false };
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
            let mut update = load(paths, &install_dir)?;
            let staged = launcher()?.parent().ok_or_else(|| error("BINARY_MISSING", "설치 도우미 경로가 없습니다."))?.to_owned();
            for name in FILES {
                let expected = hash(&staged.join(name))?;
                if hash(&install_dir.join(name))? != expected {
                    return Err(error("UPDATE_FILE_MISMATCH", "설치 파일이 새 빌드와 일치하지 않습니다. 서비스를 시작하지 않았습니다."));
                }
                update.files.insert(name.to_owned(), expected);
            }
            refresh_shims(paths, &install_dir.join("aam.exe"))?;
            resume(paths, &update)?;
            update.committed = true;
            save(&paths.home.join(RECEIPT), &update)?;
            clear(paths, &update)
        }
        "recover" => {
            if !paths.home.join(RECEIPT).try_exists().map_err(io_error)? { return clear_orphan(paths, &install_dir); }
            let mut update = load(paths, &install_dir)?;
            if update.committed {
                for (name, expected) in &update.files {
                    if hash(&install_dir.join(name))? != *expected {
                        return Err(error("UPDATE_FILE_MISMATCH", "완료된 업데이트의 파일이 달라 복구 기록을 보존했습니다."));
                    }
                }
                return clear(paths, &update);
            }
            owned_service(paths, &install_dir)?;
            ensure_launcher_released(&install_dir)?;
            for (name, expected) in &update.files {
                verify_restore(&paths.home.join(BACKUP).join(name), &install_dir.join(name), expected)?;
            }
            for (name, expected) in update.state.iter().flatten() {
                verify_restore(&state_backup(paths, name), &paths.home.join(name), expected)?;
            }
            if running(paths) { service_stop(paths)?; }
            for name in update.files.keys() {
                let backup = paths.home.join(BACKUP).join(name);
                if backup.try_exists().map_err(io_error)? { copy(&backup, &install_dir.join(name))?; }
            }
            // shim은 복구한 launcher와 같은 세대로 되돌린다. 새 launcher로 다시 만들면 이전 launcher가 모르는 형식이 된다.
            match &update.state {
                Some(state) => restore_state(paths, state)?,
                None if update.files.contains_key("aam.exe") => refresh_shims(paths, &install_dir.join("aam.exe"))?,
                None => {}
            }
            resume(paths, &update)?;
            update.committed = true;
            save(&paths.home.join(RECEIPT), &update)?;
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

    #[test]
    fn orphan_backup_requires_matching_current_files_before_cleanup() {
        let fixture = Fixture::new();
        for name in FILES { fs::write(fixture.app.join(name), b"original").unwrap(); }
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        fs::remove_file(fixture.paths.home.join(RECEIPT)).unwrap();
        fs::write(fixture.app.join("aam-service.exe"), b"unverified replacement").unwrap();
        assert_eq!(run(&fixture.paths, "recover", &fixture.app).unwrap_err().code, "UPDATE_BACKUP_FAILED");
        assert_eq!(fs::read(fixture.paths.home.join(BACKUP).join("aam.exe")).unwrap(), b"original");
        fs::write(fixture.app.join("aam-service.exe"), b"original").unwrap();
        run(&fixture.paths, "recover", &fixture.app).unwrap();
        assert!(!fixture.paths.home.join(BACKUP).exists());
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        run(&fixture.paths, "recover", &fixture.app).unwrap();
        for name in FILES { assert_eq!(fs::read(fixture.app.join(name)).unwrap(), b"original"); }
    }

    #[test]
    fn unknown_orphan_files_are_preserved() {
        let fixture = Fixture::new();
        let backup = fixture.paths.home.join(BACKUP);
        private_dir(&backup).unwrap();
        let unknown = backup.join("user-notes.txt");
        fs::write(&unknown, b"not an Ojak backup").unwrap();
        assert_eq!(run(&fixture.paths, "recover", &fixture.app).unwrap_err().code, "UPDATE_BACKUP_FAILED");
        assert_eq!(fs::read(unknown).unwrap(), b"not an Ojak backup");
    }

    #[test]
    fn committed_cleanup_is_resumable_without_rolling_back_new_files() {
        let fixture = Fixture::new();
        for name in FILES { fs::write(fixture.app.join(name), b"original").unwrap(); }
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        let mut update = load(&fixture.paths, &directory(&fixture.app).unwrap()).unwrap();
        for name in FILES {
            fs::write(fixture.app.join(name), b"verified new payload").unwrap();
            update.files.insert(name.into(), hash(&fixture.app.join(name)).unwrap());
        }
        update.committed = true;
        save(&fixture.paths.home.join(RECEIPT), &update).unwrap();
        fs::remove_file(fixture.paths.home.join(BACKUP).join("aam.exe")).unwrap();
        run(&fixture.paths, "recover", &fixture.app).unwrap();
        for name in FILES { assert_eq!(fs::read(fixture.app.join(name)).unwrap(), b"verified new payload"); }
        assert!(!fixture.paths.home.join(RECEIPT).exists());
        assert!(!fixture.paths.home.join(BACKUP).exists());
    }

    #[test]
    fn missing_legacy_backup_does_not_authorize_unverified_replacement() {
        let fixture = Fixture::new();
        for name in FILES { fs::write(fixture.app.join(name), b"original").unwrap(); }
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        fs::remove_dir_all(fixture.paths.home.join(BACKUP)).unwrap();
        fs::write(fixture.app.join("aam.exe"), b"unverified replacement").unwrap();
        assert_eq!(run(&fixture.paths, "recover", &fixture.app).unwrap_err().code, "UPDATE_BACKUP_FAILED");
        assert_eq!(fs::read(fixture.app.join("aam.exe")).unwrap(), b"unverified replacement");
        assert!(fixture.paths.home.join(RECEIPT).exists());
        fs::write(fixture.app.join("aam.exe"), b"original").unwrap();
        run(&fixture.paths, "recover", &fixture.app).unwrap();
        assert!(!fixture.paths.home.join(RECEIPT).exists());
    }

}
