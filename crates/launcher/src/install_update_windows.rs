//! NSIS upgrade transaction. Never treats installer success as proof of daemon replacement.
use super::*;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

#[path = "install_update_blockers_windows.rs"]
mod blockers;

const RECEIPT: &str = "installer-update.json";
const BACKUP: &str = "installer-backup";
const FILES: [&str; 2] = ["aam.exe", "aam-service.exe"];
// 0.9.2 finish는 새 payload 증거를 저장하기 전에 daemon을 시작했다. 이 특정 migration의
// 증거는 공개·minisign 검증된 v0.9.2 setup에서 추출한 서비스다. 임의의 현재 파일은 승인하지 않는다.
const LEGACY_092_SERVICE_HASH: &str = "79aef33a5725b7727b097d750aee90cc261c18980ba77186124a6e38b498225c";
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
    /// shim map과 구분한다. legacy의 필드 없음은 서비스 등록 부재를 뜻하지 않는다.
    #[serde(default)]
    service_record: Option<ServiceRecord>,
    /// 교체 payload 증거는 원본 복구 hash와 분리하고 daemon 시작 전에 저장한다.
    #[serde(default)]
    replacement_files: BTreeMap<String, String>,
    /// 무기록 daemon의 신규 배정 차단 승인. 종료 전 저장해 복구 재시도에도 같은 승인만 취소한다.
    #[serde(default)]
    rollback_permit: Option<String>,
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
    hash_file(&mut file)
}
fn hash_file(file: &mut fs::File) -> Result<String, ApiError> {
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
/// 대상이 없거나 읽지 못하거나 기대 hash와 다르면 true. 읽지 못하면 보수적으로 되돌려야 하는 것으로 본다.
fn needs_restore(path: &Path, expected: &str) -> bool {
    hash(path).map_or(true, |current| current != expected)
}
fn copy_io(source: &Path, destination: &Path) -> std::io::Result<()> {
    let mut input = aam_protocol::secure::open_read_no_follow(source)?;
    let temporary = destination.with_extension(format!("update-{}", aam_protocol::new_id()));
    let result = (|| {
        let mut output = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        std::io::copy(&mut input, &mut output)?;
        output.flush()?;
        output.sync_all()?;
        drop(output);
        fs::rename(&temporary, destination)
    })();
    if result.is_err() { let _ = fs::remove_file(&temporary); }
    result
}
fn copy(source: &Path, destination: &Path) -> Result<(), ApiError> {
    copy_io(source, destination).map_err(io_error)
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
    if update.owner != OWNER || update.directory != directory
        || update.files.keys().chain(update.replacement_files.keys()).any(|name| !FILES.contains(&name.as_str()))
    {
        return Err(error("FOREIGN_INSTALL", "다른 설치의 업데이트 기록은 사용하지 않습니다."));
    }
    if let Some(state) = &update.state {
        let known = state_files();
        if state.keys().any(|name| !known.contains(name)) {
            return Err(error("FOREIGN_INSTALL", "확인되지 않은 shim 경로의 복구 기록은 사용하지 않습니다."));
        }
    }
    if let Some(record) = &update.service_record {
        if !update.service_installed || record.owner != OWNER
            || record.binary_path.file_name().and_then(|name| name.to_str()) != Some("aam-service.exe")
            || record.binary_path.parent().and_then(|path| path.canonicalize().ok()).as_deref() != Some(directory)
        {
            return Err(error("FOREIGN_INSTALL", "서비스 복구 기록이 이 설치의 소유 증거와 일치하지 않습니다."));
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
    // 먼저 소유를 검사한다. 승인 metadata가 없으면 검증한 peer에서 새 lease 검사를 받는다.
    let mut record = owned_service(paths, &update.directory)?
        .ok_or_else(|| error("UNMANAGED_SERVICE", "서비스 설치 기록을 확인하지 못해 시작하지 않았습니다."))?;
    if record.uninstall_permit.as_ref().is_some_and(|permit| permit.is_empty()) {
        return Err(error("INVALID_UNINSTALL_PERMIT", "비어 있는 안전 종료 승인은 사용하지 않습니다."));
    }
    let binary = update.directory.join("aam-service.exe");
    // 새 판은 감독 프로세스를 시작하고 서비스는 그 자식이다. 승인과 RPC는 pipe 서버(서비스) 프로세스 객체에 대해서만 한다.
    let process = match ServiceProcess::connect(paths, &binary)? {
        Some(process) => process,
        None => {
            let started = start_detached(&binary, paths)?;
            started_worker(paths, &binary, &started)?
        }
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if !process.alive() { return Err(error("SERVICE_START_FAILED", "승인한 서비스가 준비되기 전에 종료됐습니다. 복구 기록을 보존했습니다.")); }
        match process.call(paths, "status.read", json!({})) {
            Ok(_) => break,
            Err(failure) if failure.code == "DAEMON_UNAVAILABLE" => {
                if Instant::now() >= deadline { return Err(error("SERVICE_START_TIMEOUT", "파일 교체 후 서비스 시작을 확인하지 못했습니다. 복구 기록을 보존했습니다.")); }
                thread::sleep(Duration::from_millis(100));
            }
            Err(failure) => return Err(failure),
        }
    }
    if record.uninstall_permit.is_none() {
        record.uninstall_permit = Some(process.prepare(paths)?);
        save(&paths.home.join("service-install.json"), &record)?;
    }
    let permit = record.uninstall_permit.as_ref().ok_or_else(|| error("INVALID_UNINSTALL_PERMIT", "안전 승인 기록이 없습니다."))?;
    process.call(paths, "service.cancelUninstall", json!({ "permit": permit }))?;
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
            // 바뀌지 않은 shim 복사본은 다시 쓰지 않는다. 실행 중인 복사본을 덮어쓰다 복구가 실패하는 일을 막는다.
            if backup.try_exists().map_err(io_error)? && state.get(&name).is_none_or(|expected| needs_restore(&target, expected)) {
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
/// 복구 중의 점유는 "파일을 바꾸지 않았다"고 말할 수 없다. 코드는 그대로 두고 설명만 정확히 한다.
fn recovery_busy(failure: ApiError) -> ApiError {
    if failure.code != "INSTALLER_FILES_BUSY" { return failure; }
    error("INSTALLER_FILES_BUSY", "이전 버전으로 되돌리는 중인데 Ojak 실행 파일을 쓰는 프로그램이 남아 있어요. 일부 Ojak 파일이 이미 새 버전일 수 있어요. 복구 기록과 백업은 보존했고, 해당 프로그램을 종료한 뒤 설치 파일을 다시 실행하면 이어서 복구해요.")
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
/// `prepare-interactive`·`recover-interactive` 전용. 조용한 단계는 이 경로를 거치지 않는다. 사용자 동의 없이는 프로세스를 닫지 않으며,
/// 동의가 끝난 뒤에도 같은 쓰기 잠금 확인으로 다시 검증한다.
fn release_launcher_interactively(install_dir: &Path, purpose: blockers::Purpose) -> Result<(), ApiError> {
    match ensure_launcher_released(install_dir) {
        Err(failure) if failure.code == "INSTALLER_FILES_BUSY" => {
            blockers::negotiate(&mut blockers::System::new(install_dir)?, &mut blockers::Native(purpose))?;
            ensure_launcher_released(install_dir)
        }
        other => other,
    }
}

fn owned_service(paths: &Paths, install_dir: &Path) -> Result<Option<ServiceRecord>, ApiError> {
    let service: Option<ServiceRecord> = read_owned(&paths.home.join("service-install.json"))?;
    if let Some(record) = &service {
        if record.owner != OWNER
            || record.binary_path.file_name().and_then(|name| name.to_str()) != Some("aam-service.exe")
            || record.binary_path.parent().and_then(|path| path.canonicalize().ok()).as_deref() != Some(install_dir)
        {
            return Err(error("FOREIGN_INSTALL", "다른 설치가 관리하는 서비스는 중지하지 않습니다."));
        }
    }
    Ok(service)
}

pub(super) fn run(paths: &Paths, action: &str, install_dir: &Path) -> Result<(), ApiError> {
    let install_dir = super::installation_directory(install_dir)?;
    match action {
        "prepare" | "prepare-interactive" => {
            if paths.home.join(RECEIPT).exists() || paths.home.join(BACKUP).exists() {
                return Err(error("UPDATE_RECOVERY_REQUIRED", "이전 업데이트 복구 기록이 있습니다. installer recover로 복구한 뒤 다시 설치하세요."));
            }
            let service = owned_service(paths, &install_dir)?;
            // 서비스가 충돌 뒤 재시작 대기 중이어도 감독 프로세스가 있으면 실행 중이던 서비스다. 업데이트 뒤 다시 시작해야 한다.
            let was_running = running(paths) || aam_service::supervisor::running(paths);
            if was_running && service.is_none() { return Err(error("UNMANAGED_SERVICE", "등록되지 않은 실행 중 서비스가 있어 업데이트하지 않았습니다.")); }
            // 조용한 prepare는 닫지 않고 실패한다. 동의 UI와 종료는 prepare-interactive에서만, 백업·영수증을 만들기 전에 일어난다.
            if action == "prepare-interactive" { release_launcher_interactively(&install_dir, blockers::Purpose::Update)?; } else { ensure_launcher_released(&install_dir)?; }
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
            let mut update = Update { owner: OWNER.into(), directory: install_dir, was_running, service_installed: service.is_some(), files, state: Some(state), committed: false, service_record: service, replacement_files: BTreeMap::new(), rollback_permit: None };
            save(&paths.home.join(RECEIPT), &update)?;
            if update.service_installed {
                if let Err(failure) = service_stop(paths) {
                    // No replacement has occurred. Preserve the original shutdown error.
                    let _ = clear(paths, &update);
                    return Err(failure);
                }
                update.service_record = owned_service(paths, &update.directory)?;
                save(&paths.home.join(RECEIPT), &update)?;
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
                update.replacement_files.insert(name.to_owned(), expected);
            }
            save(&paths.home.join(RECEIPT), &update)?;
            refresh_shims(paths, &install_dir.join("aam.exe"))?;
            resume(paths, &update)?;
            update.committed = true;
            save(&paths.home.join(RECEIPT), &update)?;
            clear(paths, &update)
        }
        "recover" | "recover-interactive" => {
            if !paths.home.join(RECEIPT).try_exists().map_err(io_error)? { return clear_orphan(paths, &install_dir); }
            let mut update = load(paths, &install_dir)?;
            if update.committed {
                let committed_files = if update.replacement_files.is_empty() { &update.files } else { &update.replacement_files };
                for (name, expected) in committed_files {
                    if hash(&install_dir.join(name))? != *expected {
                        return Err(error("UPDATE_FILE_MISMATCH", "완료된 업데이트의 파일이 달라 복구 기록을 보존했습니다."));
                    }
                }
                return clear(paths, &update);
            }
            let mut service = owned_service(paths, &install_dir)?;
            // 백업 증거부터 확인한다. 증거가 깨졌으면 프로그램을 닫도록 묻지 않고 바로 멈춘다.
            for (name, expected) in &update.files {
                verify_restore(&paths.home.join(BACKUP).join(name), &install_dir.join(name), expected)?;
            }
            for (name, expected) in update.state.iter().flatten() {
                verify_restore(&state_backup(paths, name), &paths.home.join(name), expected)?;
            }
            // launcher를 실제로 덮어써야 할 때만 잠금을 요구한다. 새 파일을 쓰지 못한 채 실패한 설치는 launcher가 원본 그대로라
            // omp 같은 사용 중 프로그램이 있어도 복구된다. 닫기 동의는 화면 있는 `recover-interactive`에서만 받는다.
            if let Some(expected) = update.files.get("aam.exe") {
                let backup_present = paths.home.join(BACKUP).join("aam.exe").try_exists().map_err(io_error)?;
                if backup_present && needs_restore(&install_dir.join("aam.exe"), expected) {
                    let released = if action == "recover-interactive" { release_launcher_interactively(&install_dir, blockers::Purpose::Recovery) } else { ensure_launcher_released(&install_dir) };
                    released.map_err(recovery_busy)?;
                }
            }
            // 먼저 실제 pipe peer를 검증한다. 다른 설치가 연결했으면 소유 기록도 바꾸지 않는다.
            let binary = install_dir.join("aam-service.exe");
            let process = ServiceProcess::connect(paths, &binary)?;
            // 새 transaction의 실제 snapshot만 복원한다. legacy boolean으로 등록하지 않는다.
            if service.is_none() {
                if let Some(record) = &update.service_record {
                    save(&paths.home.join("service-install.json"), record)?;
                    service = owned_service(paths, &install_dir)?;
                }
            }
            // 파일을 되돌리는 동안 쥐는 관문. 새 감독 프로세스나 작업자가 실행 파일을 다시 열지 못하게 한다.
            let mut fence: Option<aam_service::supervisor::Fence> = None;
            if service.is_some() {
                // 최초 연결의 image 핸들도 복원 전에 놓는다. 중지 경로에서 같은 소유 서비스를 다시 검증한다.
                drop(process);
                // 등록된 서비스는 pipe가 있든 없든 독립적으로 끝낸다. 충돌한 교체 서비스의 감독 프로세스가 재시작 대기 중이면 pipe가 없지만
                // 같은 실행 파일을 쥐고 새 서비스를 만들 수 있다. `service_stop_for_rollback`이 자기 관문을 쥐고 서비스(있으면)와 감독 프로세스를 끝낸다.
                fence = Some(service_stop_for_rollback(paths)?);
            } else if let Some(mut process) = process {
                // 0.9.2가 시작 후 소유 검사에 실패해 남긴 daemon만 installer 복구에서 다룬다.
                let originals_proved = update.service_installed && FILES.iter().all(|name| update.files.contains_key(*name));
                let image = process.image.as_mut().ok_or_else(|| error("UNMANAGED_SERVICE", "서비스 실행 파일 객체를 확인하지 못했습니다."))?;
                let current_hash = hash_file(image)?;
                let known_hash = update.files.get("aam-service.exe").is_some_and(|hash| hash == &current_hash)
                    || update.replacement_files.get("aam-service.exe").is_some_and(|hash| hash == &current_hash)
                    || current_hash == LEGACY_092_SERVICE_HASH;
                if !originals_proved || !known_hash {
                    return Err(error("UNMANAGED_SERVICE", "기존 설치의 실행 파일 증거가 없어 무기록 서비스를 중지하지 않았습니다."));
                }
                // 감독 프로세스가 (있다면) 이 서비스를 되살리지 못하게 막은 뒤에 승인하고 끈다. 파일을 되돌리기 전에 감독 프로세스가
                // 끝난 것까지 확인하고 관문은 파일 복구가 끝날 때까지 쥔다. 확정 전에 거절되면 관문을 놓아 감독이 이어진다.
                let held = fence.insert(aam_service::supervisor::engage(paths)?);
                let permit = process.prepare(paths)?;
                update.rollback_permit = Some(permit.clone());
                save(&paths.home.join(RECEIPT), &update)?;
                if !process.stop() || running(paths) {
                    let _ = process.call(paths, "service.cancelUninstall", json!({ "permit": permit }));
                    return Err(error("SERVICE_STOP_FAILED", "승인한 서비스의 종료를 확인하지 못해 복구 기록을 보존했습니다."));
                }
                held.commit_retirement();
                held.wait_supervisor_exit(Duration::from_secs(15))?;
            } else {
                // 등록도 pipe도 없다. 감독 프로세스만 살아 있으면 누가 소유한 것인지 알 수 없는 감독이 같은 실행 파일을 쥐고 있다.
                // 건드리지 않고 파일 복구 전에 거절한다. 없다고 확인되면 파일 복구가 끝날 때까지 그 잠금을 쥐어 새 감독 프로세스를 막는다.
                if aam_service::supervisor::running(paths) {
                    return Err(error("UNMANAGED_SERVICE", "등록되지 않은 감독 프로세스가 실행 중이라 파일을 되돌리지 않았습니다."));
                }
                let mut held = aam_service::supervisor::engage(paths)?;
                if held.supervisor_alive() {
                    return Err(error("UNMANAGED_SERVICE", "등록되지 않은 감독 프로세스가 실행 중이라 파일을 되돌리지 않았습니다."));
                }
                fence = Some(held);
            }
            // 실행 중일 수 있는 launcher를 먼저 되돌린다. 실패해도 서비스 파일은 아직 그대로라 한 세대가 섞이지 않는다.
            // 이미 원본과 같은 파일은 다시 쓰지 않는다: 실행 중인 `aam.exe`는 덮어쓸 수 없고, 바뀌지 않은 파일을 쓸 이유도 없다.
            for name in FILES {
                let Some(expected) = update.files.get(name) else { continue };
                let backup = paths.home.join(BACKUP).join(name);
                if !backup.try_exists().map_err(io_error)? || !needs_restore(&install_dir.join(name), expected) { continue; }
                copy_io(&backup, &install_dir.join(name)).map_err(|failure| {
                    // 검사 뒤 새로 뜬 프로그램이 파일을 잡았다. 복구 기록과 백업은 그대로 둔다(다음 복구가 이어서 한다).
                    if name == "aam.exe" && matches!(failure.raw_os_error(), Some(5 | 32 | 33)) && !released(&install_dir, std::time::Duration::ZERO).unwrap_or(false) { recovery_busy(files_busy()) } else { io_error(failure) }
                })?;
            }
            // shim은 복구한 launcher와 같은 세대로 되돌린다. 새 launcher로 다시 만들면 이전 launcher가 모르는 형식이 된다.
            match &update.state {
                Some(state) => restore_state(paths, state)?,
                None if update.files.contains_key("aam.exe") => refresh_shims(paths, &install_dir.join("aam.exe"))?,
                None => {}
            }
            // legacy 소유 기록이 사라졌으면 검증된 파일만 되돌린다. 다음 NSIS 설치가 새 등록을 담당한다.
            if service.is_some() {
                // 서비스를 다시 시작하기 전에 관문을 푼다. 쥔 채로는 새 감독 프로세스가 작업자를 만들지 못한다.
                drop(fence.take());
                resume(paths, &update)?;
            } else if paths.database.try_exists().map_err(io_error)? {
                aam_service::cancel_stopped_uninstall(paths, update.rollback_permit.as_deref())?;
            }
            update.replacement_files.clear();
            update.rollback_permit = None;
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
    fn interrupted_rollback_cleanup_verifies_restored_original_generation_on_retry() {
        let fixture = Fixture::new();
        for name in FILES { fs::write(fixture.app.join(name), b"original").unwrap(); }
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        let mut update = load(&fixture.paths, &directory(&fixture.app).unwrap()).unwrap();
        for name in FILES {
            fs::write(fixture.app.join(name), b"replacement").unwrap();
            update.replacement_files.insert(name.into(), hash(&fixture.app.join(name)).unwrap());
        }
        save(&fixture.paths.home.join(RECEIPT), &update).unwrap();
        let unknown = fixture.paths.home.join(BACKUP).join("user-notes.txt");
        fs::write(&unknown, b"preserve unknown data").unwrap();
        assert!(run(&fixture.paths, "recover", &fixture.app).is_err());
        for name in FILES { assert_eq!(fs::read(fixture.app.join(name)).unwrap(), b"original"); }
        assert_eq!(fs::read(&unknown).unwrap(), b"preserve unknown data");
        fs::remove_file(&unknown).unwrap();
        run(&fixture.paths, "recover", &fixture.app).unwrap();
        for name in FILES { assert_eq!(fs::read(fixture.app.join(name)).unwrap(), b"original"); }
        assert!(!fixture.paths.home.join(RECEIPT).exists());
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        run(&fixture.paths, "recover", &fixture.app).unwrap();
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

    #[test]
    fn legacy_missing_enrollment_rolls_back_without_starting_or_registering_service() {
        let fixture = Fixture::new();
        for name in FILES { fs::write(fixture.app.join(name), b"original").unwrap(); }
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        let mut update = load(&fixture.paths, &directory(&fixture.app).unwrap()).unwrap();
        update.service_installed = true;
        update.was_running = true;
        save(&fixture.paths.home.join(RECEIPT), &update).unwrap();
        for name in FILES { fs::write(fixture.app.join(name), b"incomplete replacement").unwrap(); }
        run(&fixture.paths, "recover", &fixture.app).unwrap();
        for name in FILES { assert_eq!(fs::read(fixture.app.join(name)).unwrap(), b"original"); }
        assert!(!fixture.paths.home.join("service-install.json").exists());
        assert!(!fixture.paths.home.join(RECEIPT).exists());
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        run(&fixture.paths, "recover", &fixture.app).unwrap();
    }

    #[test]
    fn resume_rejects_missing_ownership_before_attempting_to_start_service() {
        let fixture = Fixture::new();
        for name in FILES { fs::write(fixture.app.join(name), b"not an executable").unwrap(); }
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        let mut update = load(&fixture.paths, &directory(&fixture.app).unwrap()).unwrap();
        update.service_installed = true;
        update.was_running = true;
        assert_eq!(resume(&fixture.paths, &update).unwrap_err().code, "UNMANAGED_SERVICE");
        assert!(!fixture.paths.home.join("service-install.json").exists());
    }
    #[test]
    fn prepare_counts_a_live_supervisor_as_a_running_service() {
        // 서비스가 충돌 뒤 재시작을 기다리는 중이라 pipe가 없어도 감독 프로세스가 살아 있으면 등록 없는 서비스로 보고 업데이트하지 않는다.
        let fixture = Fixture::new();
        for name in FILES { fs::write(fixture.app.join(name), b"original").unwrap(); }
        let supervisor = aam_protocol::secure::open_owned_file(&fixture.paths.home.join("service-supervisor.lock")).unwrap();
        supervisor.try_lock().unwrap();
        assert!(aam_service::supervisor::running(&fixture.paths));
        assert_eq!(run(&fixture.paths, "prepare", &fixture.app).unwrap_err().code, "UNMANAGED_SERVICE");
        assert!(!fixture.paths.home.join(BACKUP).exists() && !fixture.paths.home.join(RECEIPT).exists());
        drop(supervisor);
        assert!(!aam_service::supervisor::running(&fixture.paths));
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
    }

    #[test]
    fn recover_refuses_an_unmanaged_supervisor_before_restoring_files_and_leaves_it_running() {
        // 등록도 pipe도 없는데 감독 프로세스만 살아 있으면(재시작 대기) 같은 실행 파일을 쥔 소유 불명의 감독이다. 파일을 되돌리기 전에 거절하고
        // 건드리지 않아야 한다(관문도 남기지 않아 감독이 계속 작업자를 만든다).
        let fixture = Fixture::new();
        for name in FILES { fs::write(fixture.app.join(name), b"original").unwrap(); }
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        for name in FILES { fs::write(fixture.app.join(name), b"replacement").unwrap(); }
        let supervisor = aam_protocol::secure::open_owned_file(&fixture.paths.home.join("service-supervisor.lock")).unwrap();
        supervisor.try_lock().unwrap();
        assert_eq!(run(&fixture.paths, "recover", &fixture.app).unwrap_err().code, "UNMANAGED_SERVICE");
        for name in FILES { assert_eq!(fs::read(fixture.app.join(name)).unwrap(), b"replacement"); }
        assert!(fixture.paths.home.join(RECEIPT).exists());
        assert!(aam_service::supervisor::running(&fixture.paths), "a refused recovery must not retire the supervisor");
        // 관문이 남지 않았다: 다른 작업이 즉시 관문을 얻을 수 있다.
        drop(aam_service::supervisor::engage(&fixture.paths).unwrap());
        drop(supervisor);
        run(&fixture.paths, "recover", &fixture.app).unwrap();
        for name in FILES { assert_eq!(fs::read(fixture.app.join(name)).unwrap(), b"original"); }
        assert!(!fixture.paths.home.join(RECEIPT).exists());
    }

    /// 실행 중인 이미지처럼 읽기는 허용하고 쓰기·교체는 막는 점유(FILE_SHARE_READ).
    fn hold(path: &Path) -> fs::File {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new().read(true).share_mode(1).open(path).unwrap()
    }

    #[test]
    fn late_launcher_holder_after_prepare_rolls_back_without_rewriting_the_unchanged_launcher_and_blocks_a_new_transaction() {
        // prepare 뒤에 omp가 새 `aam.exe` 자식을 띄웠고, NSIS는 서비스 파일만 바꾸고 launcher를 쓰지 못했다.
        let fixture = Fixture::new();
        fs::write(fixture.app.join("aam.exe"), b"original launcher").unwrap();
        fs::write(fixture.app.join("aam-service.exe"), b"original service").unwrap();
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        let held = hold(&fixture.app.join("aam.exe"));
        fs::write(fixture.app.join("aam-service.exe"), b"new service").unwrap();
        // 조용한 복구도 닫지 않고 끝낸다: 되돌릴 launcher가 원본과 같아 쓸 필요가 없다.
        run(&fixture.paths, "recover", &fixture.app).unwrap();
        assert_eq!(fs::read(fixture.app.join("aam.exe")).unwrap(), b"original launcher");
        assert_eq!(fs::read(fixture.app.join("aam-service.exe")).unwrap(), b"original service");
        assert!(!fixture.paths.home.join(RECEIPT).exists() && !fixture.paths.home.join(BACKUP).exists());
        // 점유가 남아 있는 동안 새 트랜잭션은 만들어지지 않는다(복구 기록도 남지 않는다).
        assert_eq!(run(&fixture.paths, "prepare", &fixture.app).unwrap_err().code, "INSTALLER_FILES_BUSY");
        assert!(!fixture.paths.home.join(RECEIPT).exists() && !fixture.paths.home.join(BACKUP).exists());
        drop(held);
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        run(&fixture.paths, "recover", &fixture.app).unwrap();
    }

    #[test]
    fn existing_receipt_reports_recovery_required_before_any_blocker_check_and_recover_then_unblocks_the_retry() {
        // 이전 시도가 영수증을 남긴 채 끝났고 새 `aam.exe`가 떠 있다. prepare가 점유 오류로 가로채면 복구 동의 순서가 꼬인다.
        let fixture = Fixture::new();
        for name in FILES { fs::write(fixture.app.join(name), b"original").unwrap(); }
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        let held = hold(&fixture.app.join("aam.exe"));
        for action in ["prepare", "prepare-interactive"] {
            assert_eq!(run(&fixture.paths, action, &fixture.app).unwrap_err().code, "UPDATE_RECOVERY_REQUIRED");
        }
        run(&fixture.paths, "recover", &fixture.app).unwrap();
        assert_eq!(run(&fixture.paths, "prepare", &fixture.app).unwrap_err().code, "INSTALLER_FILES_BUSY");
        assert!(!fixture.paths.home.join(RECEIPT).exists() && !fixture.paths.home.join(BACKUP).exists());
        drop(held);
    }

    #[test]
    fn partially_replaced_launcher_held_by_a_new_process_preserves_receipt_backup_and_changed_files_until_it_is_released() {
        let fixture = Fixture::new();
        for name in FILES { fs::write(fixture.app.join(name), b"original").unwrap(); }
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        // NSIS가 launcher를 먼저 새 파일로 바꿨고, 그 새 launcher를 omp 자식이 곧바로 실행해 잡고 있다.
        for name in FILES { fs::write(fixture.app.join(name), b"new").unwrap(); }
        let held = hold(&fixture.app.join("aam.exe"));
        // 조용한 복구는 닫지 않고 멈춘다. 파일·영수증·백업은 그대로다.
        let failure = run(&fixture.paths, "recover", &fixture.app).unwrap_err();
        assert_eq!(failure.code, "INSTALLER_FILES_BUSY");
        assert!(failure.message.contains("새 버전"), "복구 실패는 파일이 바뀌지 않았다고 말하면 안 된다: {}", failure.message);
        assert!(!failure.message.contains("바꾸지 않았어요"));
        for name in FILES { assert_eq!(fs::read(fixture.app.join(name)).unwrap(), b"new"); }
        assert!(fixture.paths.home.join(RECEIPT).exists());
        assert_eq!(fs::read(fixture.paths.home.join(BACKUP).join("aam.exe")).unwrap(), b"original");
        assert_eq!(fs::read(fixture.paths.home.join(BACKUP).join("aam-service.exe")).unwrap(), b"original");
        // 같은 영수증으로 이어서 복구한다. 점유가 사라진 뒤에는 두 파일 모두 원본이다.
        drop(held);
        run(&fixture.paths, "recover", &fixture.app).unwrap();
        for name in FILES { assert_eq!(fs::read(fixture.app.join(name)).unwrap(), b"original"); }
        assert!(!fixture.paths.home.join(RECEIPT).exists() && !fixture.paths.home.join(BACKUP).exists());
    }

    #[test]
    fn tampered_backup_stops_recovery_before_any_launcher_lock_decision() {
        // 백업 증거가 깨졌으면 "프로그램을 닫으면 복구된다"고 안내하지 않고 증거 오류로 멈춘다.
        let fixture = Fixture::new();
        for name in FILES { fs::write(fixture.app.join(name), b"original").unwrap(); }
        run(&fixture.paths, "prepare", &fixture.app).unwrap();
        for name in FILES { fs::write(fixture.app.join(name), b"new").unwrap(); }
        fs::write(fixture.paths.home.join(BACKUP).join("aam.exe"), b"tampered").unwrap();
        let held = hold(&fixture.app.join("aam.exe"));
        assert_eq!(run(&fixture.paths, "recover", &fixture.app).unwrap_err().code, "UPDATE_BACKUP_FAILED");
        for name in FILES { assert_eq!(fs::read(fixture.app.join(name)).unwrap(), b"new"); }
        assert!(fixture.paths.home.join(RECEIPT).exists());
        drop(held);
    }

}
