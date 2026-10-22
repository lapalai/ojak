//! 설치 폴더 해석. 설치기·업데이트가 만지기 전에 대상의 실제(canonical) 경로를 한 번 구한다.
//!
//! 규칙은 이전 `install_update_windows::directory`와 같다: 절대 로컬 디스크 경로, 모든 상위가 일반 폴더, 재분석 지점
//! (설치 폴더 자신·하위 포함) 거부. 단 하나의 예외만 둔다. 예전 Windows가 사용자 프로필 폴더 이름을 바꾸며 남긴
//! `C:\Users\<옛 이름>` junction이 **현재 사용자의 실제 프로필**(OS 알려진 폴더)을 가리키고, 설치 폴더가 그 프로필의
//! LocalAppData 아래일 때다. 환경 변수는 이 예외의 근거로 쓰지 않는다. 부작용(폴더 생성 등)은 없다.
use aam_protocol::ApiError;
use std::{
    ffi::{c_void, OsString},
    fs, io,
    os::windows::{
        ffi::OsStringExt,
        fs::OpenOptionsExt,
        io::AsRawHandle,
    },
    path::{Component, Path, PathBuf, Prefix},
    ptr,
};
use windows_sys::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{
        FileAttributeTagInfo, GetFileInformationByHandleEx, FILE_ATTRIBUTE_TAG_INFO, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
    },
    System::Com::CoTaskMemFree,
    UI::Shell::{FOLDERID_LocalAppData, FOLDERID_Profile, SHGetKnownFolderPath, KF_FLAG_DEFAULT},
};

const ATTRIBUTE_DIRECTORY: u32 = 0x10;
const ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// `IO_REPARSE_TAG_MOUNT_POINT`: junction(와 볼륨 마운트 지점). symlink·클라우드 placeholder 등 다른 태그는 예외 대상이 아니다.
const REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
const SHARE_ALL: u32 = 0x7;

/// 이 사용자의 알려진 폴더. 예외 판정의 유일한 신뢰 근거다.
struct Trusted {
    profile: PathBuf,
    local_app_data: PathBuf,
}
type TrustedSource<'a> = &'a dyn Fn() -> Result<Trusted, ApiError>;

/// 기존 설치 폴더의 실제 경로. 없으면 `INSTALL_IO_ERROR`. 링크·파일·상대·UNC·장치 경로는 `INVALID_INSTALL_PATH`.
pub(super) fn resolve(path: &Path) -> Result<PathBuf, ApiError> {
    resolve_with(path, &known_folders, false)
}

/// 아직 없는 설치 폴더의 실제 경로(설치 전 `$INSTDIR`). 가장 가까운 기존 상위만 검사·해석하고 없는 하위 이름은
/// 일반 이름 검증 뒤 그대로 붙인다. 아무것도 만들지 않는다.
pub(super) fn resolve_for_creation(path: &Path) -> Result<PathBuf, ApiError> {
    resolve_with(path, &known_folders, true)
}

fn invalid(text: &str) -> ApiError {
    ApiError::new("INVALID_INSTALL_PATH", text)
}
fn io_error(_: io::Error) -> ApiError {
    ApiError::new(
        "INSTALL_IO_ERROR",
        "설치 파일을 처리하지 못했어요. 폴더 소유와 쓰기 권한을 확인해 주세요.",
    )
}
fn reparse_rejected() -> ApiError {
    invalid("재분석 지점을 통과하는 설치 폴더는 변경하지 않습니다.")
}

fn resolve_with(path: &Path, trusted: TrustedSource, creation: bool) -> Result<PathBuf, ApiError> {
    let (root, names) = split(path)?;
    if probe(&root).map_err(io_error)? != Kind::Directory {
        return Err(reparse_rejected());
    }
    let mut cursor = root;
    let mut alias: Option<PathBuf> = None;
    let mut present = names.len();
    for (index, name) in names.iter().enumerate() {
        cursor.push(name);
        match probe(&cursor) {
            Ok(Kind::Directory) => {}
            // 설치 폴더 자신(마지막 이름)이 junction이면 예외가 아니라 leaf 리다이렉트다. 예외 junction은 하나뿐이다.
            Ok(Kind::Junction) if alias.is_none() && index + 1 < names.len() => alias = Some(cursor.clone()),
            Ok(_) => return Err(reparse_rejected()),
            Err(failure) if creation && failure.kind() == io::ErrorKind::NotFound => {
                cursor.pop();
                present = index;
                break;
            }
            Err(failure) => return Err(io_error(failure)),
        }
    }
    let mut resolved = fs::canonicalize(&cursor).map_err(io_error)?;
    for name in &names[present..] {
        resolved.push(name);
    }
    if !matches!(
        resolved.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
    ) {
        return Err(invalid("네트워크·장치 경로의 설치 폴더는 사용하지 않습니다."));
    }
    if let Some(alias) = alias {
        verify_alias(&alias, &resolved, trusted)?;
    }
    Ok(resolved)
}

/// 절대 로컬 디스크 경로만 받아 `(루트, 이름들)`로 쪼갠다. `C:\`와 canonical 형태 `\\?\C:\`만 받는다(NSIS는 접두사를
/// 벗긴 경로를, Rust 내부 호출은 다시 canonical 경로를 넘긴다). 드라이브 상대(`C:x`), `..`, UNC, 그 밖의 `\\?\`·`\\.\`,
/// 대체 스트림, 장치 이름, 끝이 점·공백인 이름(Win32가 조용히 바꾼다)은 여기서 거부한다.
fn split(path: &Path) -> Result<(PathBuf, Vec<String>), ApiError> {
    let mut parts = path.components();
    let drive = match parts.next() {
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_)) => prefix.as_os_str().to_owned(),
        _ => {
            return Err(invalid(
                if path.is_absolute() { "로컬 디스크의 절대 경로만 설치 폴더로 쓸 수 있습니다." } else { "설치 폴더는 절대 경로여야 합니다." },
            ))
        }
    };
    if !matches!(parts.next(), Some(Component::RootDir)) {
        return Err(invalid("설치 폴더는 절대 경로여야 합니다."));
    }
    let mut root = drive;
    root.push("\\");
    let mut names = Vec::new();
    for part in parts {
        let Component::Normal(name) = part else {
            return Err(invalid("설치 폴더 경로에 상위 폴더(..)를 쓸 수 없습니다."));
        };
        let name = name.to_str().ok_or_else(|| invalid("설치 폴더 경로가 올바른 문자가 아닙니다."))?;
        if !valid_name(name) {
            return Err(invalid("설치 폴더 경로에 쓸 수 없는 이름이 있습니다."));
        }
        names.push(name.to_owned());
    }
    if names.is_empty() {
        return Err(invalid("드라이브 루트는 설치 폴더로 쓸 수 없습니다."));
    }
    Ok((PathBuf::from(root), names))
}

fn valid_name(name: &str) -> bool {
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || name.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
    {
        return false;
    }
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.as_bytes()[3].is_ascii_digit());
    !reserved
}

#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Directory,
    Junction,
    /// 파일, symlink, 그 밖의 재분석 지점 전부.
    Other,
}

/// 링크를 따라가지 않고 대상 자체의 종류를 본다.
fn probe(path: &Path) -> io::Result<Kind> {
    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut info = FILE_ATTRIBUTE_TAG_INFO { FileAttributes: 0, ReparseTag: 0 };
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileAttributeTagInfo,
            ptr::addr_of_mut!(info).cast::<c_void>(),
            std::mem::size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let directory = info.FileAttributes & ATTRIBUTE_DIRECTORY != 0;
    Ok(if info.FileAttributes & ATTRIBUTE_REPARSE_POINT == 0 {
        if directory { Kind::Directory } else { Kind::Other }
    } else if directory && info.ReparseTag == REPARSE_TAG_MOUNT_POINT {
        Kind::Junction
    } else {
        Kind::Other
    })
}

/// 링크를 따라가 연 폴더의 (볼륨, 파일 ID).
fn identity(path: &Path) -> io::Result<(u64, u64)> {
    let file = fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .share_mode(SHARE_ALL)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)?;
    aam_protocol::winutil::file_identity(&file)
}

fn same_directory(left: &Path, right: &Path) -> bool {
    matches!((identity(left), identity(right)), (Ok(left), Ok(right)) if left == right)
}

/// 예외 junction 검증. junction이 신뢰 프로필의 형제 이름으로 그 프로필과 같은 폴더를 가리키고, 해석된 설치 경로가
/// 신뢰 LocalAppData의 엄격한 하위일 때만 허용한다.
fn verify_alias(alias: &Path, resolved: &Path, trusted: TrustedSource) -> Result<(), ApiError> {
    let denied = || reparse_rejected();
    let trusted = trusted()?;
    if split(&trusted.profile).is_err() {
        return Err(denied());
    }
    // 신뢰 프로필 자체는 링크 없는 실제 경로여야 한다.
    for ancestor in trusted.profile.ancestors() {
        if probe(ancestor).map_err(io_error)? != Kind::Directory {
            return Err(denied());
        }
    }
    let (Some(alias_parent), Some(profile_parent)) = (alias.parent(), trusted.profile.parent()) else { return Err(denied()) };
    if !same_directory(alias, &trusted.profile) || !same_directory(alias_parent, profile_parent) {
        return Err(denied());
    }
    let local = fs::canonicalize(&trusted.local_app_data).map_err(|_| denied())?;
    if !starts_with_ignore_case(resolved, &local) || resolved.components().count() <= local.components().count() {
        return Err(invalid("옛 사용자 폴더 별칭은 현재 사용자의 LocalAppData 아래 설치에만 허용됩니다."));
    }
    Ok(())
}

fn starts_with_ignore_case(path: &Path, base: &Path) -> bool {
    let key = |component: Component| component.as_os_str().to_string_lossy().to_lowercase();
    let mut path = path.components().map(key);
    base.components().map(key).all(|expected| path.next().is_some_and(|actual| actual == expected))
}

fn known_folders() -> Result<Trusted, ApiError> {
    Ok(Trusted { profile: known_folder(&FOLDERID_Profile)?, local_app_data: known_folder(&FOLDERID_LocalAppData)? })
}

/// 현재 프로세스 사용자의 알려진 폴더(리다이렉션 반영, 환경 변수 아님).
fn known_folder(id: &windows_sys::core::GUID) -> Result<PathBuf, ApiError> {
    let mut raw: *mut u16 = ptr::null_mut();
    let status = unsafe { SHGetKnownFolderPath(id, KF_FLAG_DEFAULT as u32, ptr::null_mut(), &mut raw) };
    if raw.is_null() {
        return Err(invalid("현재 사용자 폴더를 확인하지 못했습니다."));
    }
    let path = (status >= 0).then(|| {
        let mut len = 0usize;
        unsafe {
            while *raw.add(len) != 0 {
                len += 1;
            }
            PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(raw, len)))
        }
    });
    unsafe { CoTaskMemFree(raw.cast()) };
    path.ok_or_else(|| invalid("현재 사용자 폴더를 확인하지 못했습니다."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// 임시 루트. 정리는 junction을 먼저 `remove_dir`로 떼어낸 뒤에만 트리를 지운다(대상은 건드리지 않는다).
    struct Fixture {
        root: PathBuf,
        junctions: Vec<PathBuf>,
    }
    impl Fixture {
        fn new() -> Self {
            // 임시 폴더 상위의 8.3 이름·링크가 섞이지 않게 실제 경로의 평범한 형태로 고정한다.
            let base = fs::canonicalize(std::env::temp_dir()).unwrap();
            let base = PathBuf::from(base.to_string_lossy().trim_start_matches(r"\\?\").to_owned());
            let root = base.join(format!("ojak path test {}", aam_protocol::new_id()));
            fs::create_dir_all(&root).unwrap();
            Self { root, junctions: Vec::new() }
        }
        fn dir(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(&path).unwrap();
            path
        }
        /// `cmd /C mklink /J`: 관리자 권한도 PowerShell도 필요 없다.
        fn junction(&mut self, link: &str, target: &Path) -> PathBuf {
            let link = self.root.join(link);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            let output = Command::new("cmd").args(["/D", "/C", "mklink", "/J"]).arg(&link).arg(target).output().unwrap();
            assert!(output.status.success(), "mklink 실패: {}", String::from_utf8_lossy(&output.stdout));
            self.junctions.push(link.clone());
            link
        }
        /// 사용자 프로필 모의: 실제 프로필, 그 LocalAppData, 옛 이름 junction.
        fn profile(&mut self) -> (PathBuf, PathBuf, PathBuf) {
            let profile = self.dir(r"Users\antonio");
            let local = self.dir(r"Users\antonio\AppData\Local");
            let alias = self.junction(r"Users\전민우", &profile);
            (profile, local, alias)
        }
        fn trusted(&self, profile: &Path, local: &Path) -> impl Fn() -> Result<Trusted, ApiError> {
            let (profile, local_app_data) = (profile.to_owned(), local.to_owned());
            move || Ok(Trusted { profile: profile.clone(), local_app_data: local_app_data.clone() })
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            for junction in self.junctions.iter().rev() {
                if fs::remove_dir(junction).is_err() {
                    return; // 떼지 못한 junction이 있으면 대상까지 지우지 않도록 트리를 남긴다.
                }
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn code(result: Result<PathBuf, ApiError>) -> String {
        result.expect_err("거부돼야 합니다").code
    }
    fn never_trusted() -> Result<Trusted, ApiError> {
        panic!("링크 없는 경로는 신뢰 폴더를 조회하면 안 됩니다")
    }
    fn unavailable_trusted() -> Result<Trusted, ApiError> {
        Err(invalid("알려진 폴더 없음"))
    }
    fn plain(path: &Path) -> PathBuf {
        fs::canonicalize(path).unwrap()
    }

    #[test]
    fn ordinary_paths_keep_working_including_korean_and_spaces() {
        let fixture = Fixture::new();
        for relative in [r"Ojak", r"사용자 폴더\오작 설치", r"Program Files Custom\Ojak 앱"] {
            let dir = fixture.dir(relative);
            assert_eq!(resolve_with(&dir, &never_trusted, false).unwrap(), plain(&dir));
            assert_eq!(resolve_with(&dir, &never_trusted, true).unwrap(), plain(&dir));
        }
    }

    #[test]
    fn old_profile_junction_resolves_to_trusted_profile_local_app_data() {
        let mut fixture = Fixture::new();
        let (profile, local, alias) = fixture.profile();
        let install = fixture.dir(r"Users\antonio\AppData\Local\Ojak");
        let trusted = fixture.trusted(&profile, &local);
        let through_alias = alias.join(r"AppData\Local\Ojak");
        assert_eq!(resolve_with(&through_alias, &trusted, false).unwrap(), plain(&install));
        assert_eq!(resolve_with(&through_alias, &trusted, true).unwrap(), plain(&install));
        // 대소문자와 한글 프로필 이름은 NTFS처럼 구분하지 않고 비교한다.
        let lowered = alias.join(r"appdata\local\OJAK");
        assert_eq!(resolve_with(&lowered, &trusted, false).unwrap(), plain(&install));
    }

    #[test]
    fn old_profile_junction_allows_missing_install_without_creating_anything() {
        let mut fixture = Fixture::new();
        let (profile, local, alias) = fixture.profile();
        let trusted = fixture.trusted(&profile, &local);
        let target = alias.join(r"AppData\Local\Ojak\새 폴더");
        let resolved = resolve_with(&target, &trusted, true).unwrap();
        assert_eq!(resolved, plain(&local).join("Ojak").join("새 폴더"));
        assert!(!local.join("Ojak").exists());
        // 기존 대상 해석은 없는 폴더를 만들지 않고 실패한다.
        assert_eq!(code(resolve_with(&target, &trusted, false)), "INSTALL_IO_ERROR");
        assert!(!local.join("Ojak").exists());
    }

    #[test]
    fn alias_outside_local_app_data_or_itself_is_rejected() {
        let mut fixture = Fixture::new();
        let (profile, local, alias) = fixture.profile();
        fixture.dir(r"Users\antonio\AppData\Roaming\Ojak");
        fixture.dir(r"Users\antonio\Documents");
        let trusted = fixture.trusted(&profile, &local);
        for rejected in [
            alias.join(r"AppData\Roaming\Ojak"),
            alias.join("Documents"),
            alias.join(r"AppData\Local"),
            alias.clone(),
        ] {
            assert_eq!(code(resolve_with(&rejected, &trusted, false)), "INVALID_INSTALL_PATH", "{rejected:?}");
        }
        assert_eq!(code(resolve_with(&alias.join(r"Documents\Ojak"), &trusted, true)), "INVALID_INSTALL_PATH");
    }

    #[test]
    fn junction_to_another_profile_is_rejected() {
        let mut fixture = Fixture::new();
        let (profile, local, _) = fixture.profile();
        let other = fixture.dir(r"Users\mallory");
        fixture.dir(r"Users\mallory\AppData\Local\Ojak");
        let bob = fixture.junction(r"Users\bob", &other);
        let trusted = fixture.trusted(&profile, &local);
        let target = bob.join(r"AppData\Local\Ojak");
        assert_eq!(code(resolve_with(&target, &trusted, false)), "INVALID_INSTALL_PATH");
        assert_eq!(code(resolve_with(&target.join("new"), &trusted, true)), "INVALID_INSTALL_PATH");
    }

    #[test]
    fn junction_to_trusted_profile_outside_the_profile_parent_is_rejected() {
        let mut fixture = Fixture::new();
        let (profile, local, _) = fixture.profile();
        let elsewhere = fixture.junction(r"Elsewhere\alias", &profile);
        let trusted = fixture.trusted(&profile, &local);
        fixture.dir(r"Users\antonio\AppData\Local\Ojak");
        assert_eq!(
            code(resolve_with(&elsewhere.join(r"AppData\Local\Ojak"), &trusted, false)),
            "INVALID_INSTALL_PATH"
        );
    }

    #[test]
    fn alias_does_not_match_a_different_trusted_profile() {
        let mut fixture = Fixture::new();
        let (_, _, alias) = fixture.profile();
        let other = fixture.dir(r"Users\other");
        let other_local = fixture.dir(r"Users\other\AppData\Local");
        fixture.dir(r"Users\antonio\AppData\Local\Ojak");
        let trusted = fixture.trusted(&other, &other_local);
        assert_eq!(
            code(resolve_with(&alias.join(r"AppData\Local\Ojak"), &trusted, false)),
            "INVALID_INSTALL_PATH"
        );
    }

    #[test]
    fn unresolvable_trusted_folders_reject_the_alias_but_not_plain_paths() {
        let mut fixture = Fixture::new();
        let (_, _, alias) = fixture.profile();
        fixture.dir(r"Users\antonio\AppData\Local\Ojak");
        let failing = || Err(invalid("알려진 폴더 없음"));
        assert_eq!(code(resolve_with(&alias.join(r"AppData\Local\Ojak"), &failing, false)), "INVALID_INSTALL_PATH");
        let plain_dir = fixture.dir("Plain Ojak");
        assert!(resolve_with(&plain_dir, &failing, false).is_ok());
    }

    #[test]
    fn canonical_verbatim_drive_paths_are_accepted_and_idempotent() {
        let mut fixture = Fixture::new();
        let dir = fixture.dir(r"사용자 폴더\Ojak 앱");
        let first = resolve_with(&dir, &never_trusted, false).unwrap();
        assert!(first.to_string_lossy().starts_with(r"\\?\"), "{first:?}");
        assert_eq!(resolve_with(&first, &never_trusted, false).unwrap(), first);
        assert_eq!(resolve_with(&first, &never_trusted, true).unwrap(), first);
        // NSIS 출력처럼 접두사를 벗긴 경로도 같은 결과로 돌아온다.
        let stripped = PathBuf::from(first.to_string_lossy().trim_start_matches(r"\\?\").to_owned());
        assert_eq!(resolve_with(&stripped, &never_trusted, false).unwrap(), first);

        let (profile, local, alias) = fixture.profile();
        let install = fixture.dir(r"Users\antonio\AppData\Local\Ojak");
        let trusted = fixture.trusted(&profile, &local);
        let resolved = resolve_with(&alias.join(r"AppData\Local\Ojak"), &trusted, false).unwrap();
        assert_eq!(resolved, plain(&install));
        assert_eq!(resolve_with(&resolved, &trusted, false).unwrap(), resolved);
        // 별칭을 verbatim 형태로 넘겨도 예외 판정은 같다. `\\?\`는 `..`·`/` 정규화가 없으므로 그대로 쓴다.
        let verbatim_alias = PathBuf::from(format!(r"\\?\{}", alias.join(r"AppData\Local\Ojak").display()));
        assert_eq!(resolve_with(&verbatim_alias, &trusted, false).unwrap(), resolved);
        // 접두사만 verbatim이어도 UNC·장치는 계속 거부한다.
        for rejected in [r"\\?\UNC\server\share\Ojak", r"\\?\GLOBALROOT\Device\X\Ojak", r"\\?\Volume{00000000-0000-0000-0000-000000000000}\Ojak"] {
            assert_eq!(code(resolve_with(Path::new(rejected), &never_trusted, false)), "INVALID_INSTALL_PATH", "{rejected}");
        }
    }

    #[test]
    fn leaf_and_subtree_redirects_are_rejected_even_through_the_trusted_alias() {
        let mut fixture = Fixture::new();
        let (profile, local, alias) = fixture.profile();
        let outside = fixture.dir("Outside Target");
        let trusted = fixture.trusted(&profile, &local);

        // 설치 폴더 자신이 junction.
        fixture.junction(r"Users\antonio\AppData\Local\Ojak", &outside);
        assert_eq!(code(resolve_with(&alias.join(r"AppData\Local\Ojak"), &trusted, false)), "INVALID_INSTALL_PATH");
        assert_eq!(code(resolve_with(&local.join("Ojak"), &trusted, false)), "INVALID_INSTALL_PATH");
        // 없는 하위를 만들 설치 전 경로도 링크를 통과하면 안 된다.
        assert_eq!(code(resolve_with(&alias.join(r"AppData\Local\Ojak\new"), &trusted, true)), "INVALID_INSTALL_PATH");

        // 예외 junction 아래의 두 번째 링크(하위 리다이렉트).
        fixture.dir(r"Users\antonio\AppData\Local\Real");
        fixture.junction(r"Users\antonio\AppData\Local\Real\sub", &outside);
        assert_eq!(code(resolve_with(&alias.join(r"AppData\Local\Real\sub"), &trusted, false)), "INVALID_INSTALL_PATH");

        // 예외와 무관한 평범한 경로의 leaf junction.
        let link = fixture.junction(r"Plain\Ojak", &outside);
        assert_eq!(code(resolve_with(&link, &never_trusted, false)), "INVALID_INSTALL_PATH");
        // 상위로 지나가는 경우도 신뢰 프로필 별칭이 아니면 거부한다.
        assert_eq!(code(resolve_with(&link.join("child"), &unavailable_trusted, true)), "INVALID_INSTALL_PATH");
        assert_eq!(code(resolve_with(&link.join("child"), &trusted, true)), "INVALID_INSTALL_PATH");
    }

    #[test]
    fn missing_target_fails_for_existing_and_resolves_for_creation_without_side_effects() {
        let fixture = Fixture::new();
        let existing = fixture.dir("Base 폴더");
        let target = existing.join("Ojak").join("하위 폴더");
        assert_eq!(code(resolve_with(&target, &never_trusted, false)), "INSTALL_IO_ERROR");
        let resolved = resolve_with(&target, &never_trusted, true).unwrap();
        assert_eq!(resolved, plain(&existing).join("Ojak").join("하위 폴더"));
        assert!(!existing.join("Ojak").exists());
    }

    #[test]
    fn files_relative_and_unsafe_paths_are_rejected() {
        let fixture = Fixture::new();
        let dir = fixture.dir("Base");
        let file = dir.join("file.txt");
        fs::write(&file, b"x").unwrap();
        for creation in [false, true] {
            for rejected in [
                file.clone(),
                file.join("below"),
                PathBuf::from(r"relative\Ojak"),
                PathBuf::from("Ojak"),
                PathBuf::from(r"C:drive-relative"),
                PathBuf::from(r"\rooted\Ojak"),
                PathBuf::from(r"\\server\share\Ojak"),
                PathBuf::from(r"\\?\UNC\server\share\Ojak"),
                PathBuf::from(r"\\.\C:\Ojak"),
                PathBuf::from(r"\\.\pipe\Ojak"),
                PathBuf::from(r"C:\"),
                dir.join("..").join("Base"),
                dir.join("Ojak:stream"),
                dir.join("NUL"),
                dir.join("com1.txt"),
                dir.join("trailing."),
                dir.join("trailing "),
                dir.join("a*b"),
            ] {
                assert_eq!(code(resolve_with(&rejected, &never_trusted, creation)), "INVALID_INSTALL_PATH", "{rejected:?} creation={creation}");
            }
        }
    }

    #[test]
    fn fixture_cleanup_never_deletes_a_junction_target() {
        let keep = Fixture::new();
        let target = keep.dir("Keep");
        fs::write(target.join("marker.txt"), b"keep").unwrap();
        {
            let mut other = Fixture::new();
            let link = other.junction(r"Users\전민우", &target);
            assert!(link.join("marker.txt").exists());
        }
        assert!(target.join("marker.txt").exists(), "junction 대상이 지워졌습니다");
    }
}
