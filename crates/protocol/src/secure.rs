use std::{io, path::Path};

#[cfg(unix)]
pub fn restrict_dir(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(windows)]
pub fn restrict_dir(path: &Path) -> io::Result<()> {
    if super::winutil::is_reparse_point(path).unwrap_or(false) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "재분석 지점은 앱 디렉터리로 쓰지 않습니다",
        ));
    }
    std::fs::create_dir_all(path)?;
    if super::winutil::is_reparse_point(path)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "재분석 지점은 앱 디렉터리로 쓰지 않습니다",
        ));
    }
    super::winutil::restrict_to_owner(path)?;
    if !super::winutil::owned_by_current_user(path)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "디렉터리 소유자가 현재 사용자가 아닙니다",
        ));
    }
    Ok(())
}

#[cfg(windows)]
pub fn open_owned_file(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    // 보안 설명자를 바꾸려면 READ_CONTROL·WRITE_DAC·WRITE_OWNER 권한으로 열어야 한다.
    const GENERIC_READ: u32 = 0x8000_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const READ_CONTROL: u32 = 0x0002_0000;
    const WRITE_DAC: u32 = 0x0004_0000;
    const WRITE_OWNER: u32 = 0x0008_0000;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .access_mode(GENERIC_READ | GENERIC_WRITE | READ_CONTROL | WRITE_DAC | WRITE_OWNER)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    if !super::winutil::single_link_regular_file(&file)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "서비스 파일이 일반 파일이 아닙니다",
        ));
    }
    super::winutil::lock_down_handle(&file)?;
    if !super::winutil::handle_owned_by_us(&file)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "서비스 파일 소유자가 현재 사용자가 아닙니다",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
pub fn owned_by_current_user(path: &Path) -> io::Result<bool> {
    super::winutil::owned_by_current_user(path)
}

/// 링크(Unix symlink, Windows 재분석 지점)를 따라가지 않고 읽기 전용으로 연다.
/// Unix는 FIFO에 막히지 않도록 non-blocking으로 연다. 종류·소유 검사는 호출자가 한다.
#[cfg(unix)]
pub fn open_read_no_follow(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
}

#[cfg(windows)]
pub fn open_read_no_follow(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    // 재분석 지점 자체를 열었다면 링크이므로 거부한다.
    if !super::winutil::single_link_regular_file(&file)? && file.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "링크 파일은 열지 않습니다"));
    }
    Ok(file)
}

/// 열린 파일의 소유자가 현재 사용자인지.
#[cfg(unix)]
pub fn file_owned_by_me(file: &std::fs::File) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt;
    Ok(file.metadata()?.uid() == unsafe { libc::geteuid() })
}

#[cfg(windows)]
pub fn file_owned_by_me(file: &std::fs::File) -> io::Result<bool> {
    super::winutil::handle_owned_by_us(file)
}

/// 새로 만들 파일을 현재 사용자 전용으로 연다(Unix 0600). Windows는 앱 폴더의 소유자 전용 상속 ACL을 받는다.
pub fn private_options(options: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(options, 0o600);
    options
}

/// 기존 파일을 현재 사용자 전용으로 바꾼다(Unix 0600, Windows 소유자 전용 보호 DACL).
pub fn restrict_file(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
    }
    #[cfg(windows)]
    {
        super::winutil::restrict_to_owner(path)
    }
}
