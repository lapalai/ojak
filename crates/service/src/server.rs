use crate::Service;
use aam_protocol::{ApiError, Paths, RpcRequest, RpcResponse, PROTOCOL_VERSION};
use fs2::FileExt;
#[cfg(unix)]
use std::{
    fs::OpenOptions,
    os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
};
use std::{
    fs::File,
    io,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

fn io_error(_: io::Error) -> ApiError {
    ApiError::new(
        "SERVICE_IO_ERROR",
        "서비스 경로 또는 로컬 연결을 준비할 수 없습니다. 소유권과 권한을 확인하세요.",
    )
}
#[cfg(unix)]
fn private_directory(path: &Path) -> Result<(), ApiError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || !metadata.is_dir()
                || metadata.uid() != unsafe { libc::geteuid() } =>
        {
            return Err(ApiError::new(
                "UNSAFE_SERVICE_PATH",
                "서비스 디렉터리는 현재 사용자 소유의 실제 디렉터리여야 합니다.",
            ))
        }
        Ok(_) => (),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir_all(path).map_err(io_error)?
        }
        Err(error) => return Err(io_error(error)),
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).map_err(io_error)
}
#[cfg(unix)]
fn private_file(path: &Path) -> Result<File, ApiError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        return Err(ApiError::new(
            "UNSAFE_SERVICE_PATH",
            "서비스 파일의 종류 또는 소유권이 올바르지 않습니다.",
        ));
    }
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(io_error)?;
    Ok(file)
}
/// 소유자 전용 ACL을 걸고 재분석 지점(심볼릭 링크·정션)을 거부한다. Unix의 0700·소유자 검사에 해당한다.
#[cfg(windows)]
fn private_directory(path: &Path) -> Result<(), ApiError> {
    aam_protocol::secure::restrict_dir(path).map_err(|_| {
        ApiError::new(
            "UNSAFE_SERVICE_PATH",
            "서비스 디렉터리는 현재 사용자 소유의 실제 디렉터리여야 합니다.",
        )
    })
}
/// 소유자 전용 ACL, 단일 링크 일반 파일, 현재 사용자 소유를 확인한다. Unix의 0600·O_NOFOLLOW·nlink 검사에 해당한다.
#[cfg(windows)]
fn private_file(path: &Path) -> Result<File, ApiError> {
    aam_protocol::secure::open_owned_file(path).map_err(|_| {
        ApiError::new(
            "UNSAFE_SERVICE_PATH",
            "서비스 파일의 종류 또는 소유권이 올바르지 않습니다.",
        )
    })
}
struct ConnectionSlot(Arc<AtomicUsize>);
impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

pub fn run(paths: Paths) -> Result<(), ApiError> {
    if !cfg!(any(target_os = "macos", windows)) {
        return Err(ApiError::new(
            "PLATFORM_UNSUPPORTED",
            "이 서비스 배포는 macOS와 Windows만 지원합니다.",
        ));
    }
    private_directory(&paths.home)?;
    private_directory(&paths.profiles)?;
    #[cfg(unix)]
    {
        let socket_parent = paths.socket.parent().ok_or_else(|| {
            ApiError::new("UNSAFE_SERVICE_PATH", "로컬 socket 상위 경로가 없습니다.")
        })?;
        private_directory(socket_parent)?;
        if paths.socket.as_os_str().len() >= 104 {
            return Err(ApiError::new(
                "SOCKET_PATH_TOO_LONG",
                "로컬 socket 경로가 macOS 제한을 초과합니다.",
            ));
        }
    }
    let lock = private_file(&paths.home.join("service.lock"))?;
    lock.try_lock_exclusive().map_err(|_| {
        ApiError::new(
            "SERVICE_ALREADY_RUNNING",
            "이 사용자의 관리 서비스가 이미 실행 중입니다.",
        )
    })?;
    let _database_file = private_file(&paths.database)?;
    for suffix in ["-wal", "-shm"] {
        let path = std::path::PathBuf::from(format!("{}{suffix}", paths.database.display()));
        if path.try_exists().map_err(io_error)? {
            let _ = private_file(&path)?;
        }
    }
    #[cfg(unix)]
    match std::fs::symlink_metadata(&paths.socket) {
        Ok(metadata)
            if metadata.file_type().is_socket() && metadata.uid() == unsafe { libc::geteuid() } =>
        {
            std::fs::remove_file(&paths.socket).map_err(io_error)?
        }
        Ok(_) => {
            return Err(ApiError::new(
                "UNSAFE_SOCKET_PATH",
                "socket 경로에 다른 파일이 있습니다. 자동으로 삭제하지 않습니다.",
            ))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(io_error(error)),
    }
    let service = Service::open(paths.clone())?;
    // 재시작 시 프로세스 identity를 먼저 조정한 뒤에 신규 배정을 받습니다.
    service.reconcile(true)?;
    // Unix는 0600 socket, Windows는 소유자 전용 보안 설명자의 named pipe(첫 인스턴스 독점)로 연다.
    let listener = aam_protocol::LocalListener::bind(&paths.socket).map_err(io_error)?;
    service.start_background();
    let connections = Arc::new(AtomicUsize::new(0));
    eprintln!("Ojak 서비스 준비 완료");
    for stream in listener.incoming() {
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(error)),
        };
        // Unix는 연결 직후 확인한다. Windows는 파이프 가장이 첫 읽기 뒤에만 되므로 요청 프레임을 읽은 뒤,
        // 처리하기 전에 확인한다(확인 전에는 아무 명령도 실행하거나 응답하지 않는다).
        #[cfg(unix)]
        if !aam_protocol::peer_is_self(&stream) {
            continue;
        }
        if connections.fetch_add(1, Ordering::AcqRel) >= 64 {
            connections.fetch_sub(1, Ordering::AcqRel);
            continue;
        }
        let slot = ConnectionSlot(Arc::clone(&connections));
        let service = Arc::clone(&service);
        std::thread::spawn(move || {
            let _slot = slot;
            if stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .is_err()
                || stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .is_err()
            {
                return;
            }
            let request: RpcRequest = match aam_protocol::read_frame(&mut stream) {
                Ok(request) => request,
                Err(_) => return,
            };
            #[cfg(windows)]
            if !aam_protocol::peer_is_self(&stream) {
                return;
            }
            let response = if request.version != PROTOCOL_VERSION {
                RpcResponse::err(
                    request.id,
                    ApiError::new(
                        "PROTOCOL_MISMATCH",
                        "앱과 관리 서비스의 프로토콜 버전이 다릅니다.",
                    ),
                )
            } else if request.id.is_empty() || request.id.len() > 128 || request.method.len() > 80 {
                RpcResponse::err(
                    String::new(),
                    ApiError::new(
                        "INVALID_REQUEST",
                        "요청 ID 또는 메서드 형식이 올바르지 않습니다.",
                    ),
                )
            } else {
                match service.dispatch(&request.method, request.params) {
                    Ok(result) => RpcResponse::ok(request.id, result),
                    Err(error) => RpcResponse::err(request.id, error),
                }
            };
            let _ = aam_protocol::write_frame(&mut stream, &response);
        });
    }
    Ok(())
}
