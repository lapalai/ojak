//! 단일 실행 잠금(`tauri-plugin-single-instance`)을 모르는 예전 Ojak 사본이 이미 떠 있는지 찾는다.
//! 잠금은 새 버전끼리만 통한다. 백업 폴더의 예전 `Ojak.app`처럼 잠금 이전 판이 먼저 떠 있으면 두 창이 같은 서비스를 보게 된다.
//! 같은 사용자의, 다른 경로의 `*.app/Contents/MacOS/ai-account-manager`만 대상으로 한다. 찾은 사본은 앞으로 가져오기만 하고 종료하지 않는다.
use std::path::{Path, PathBuf};

pub struct OtherCopy {
    #[cfg_attr(not(test), allow(dead_code))]
    pub pid: i32,
    pub path: PathBuf,
}

const EXECUTABLE: &str = "ai-account-manager";

/// `path`가 Ojak 앱 번들 안의 실행 파일이고 `current`와 다른 번들이면 true.
pub fn is_other_copy(path: &Path, current: &Path) -> bool {
    let bundle_exec = path.file_name().and_then(|name| name.to_str()) == Some(EXECUTABLE)
        && path.parent().and_then(|dir| dir.file_name()).and_then(|name| name.to_str()) == Some("MacOS")
        && path.parent().and_then(Path::parent).and_then(|dir| dir.file_name()).and_then(|name| name.to_str()) == Some("Contents")
        && path.ancestors().nth(3).and_then(|dir| dir.extension()).and_then(|ext| ext.to_str()) == Some("app");
    // `/tmp`→`/private/tmp`, 심볼릭 링크로 연결된 폴더처럼 같은 파일이 다른 경로로 보일 수 있다. 실제 경로로 비교한다.
    let same = |a: &Path, b: &Path| a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y);
    bundle_exec && !same(path, current)
}

/// 현재 사용자의 다른 Ojak 사본을 찾는다. 열거에 실패하면 빈 목록(막지 않음).
pub fn find(current: &Path) -> Vec<OtherCopy> {
    let me = std::process::id() as i32;
    let uid = unsafe { libc::getuid() };
    let mut pids = vec![0i32; 8192];
    let bytes = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * std::mem::size_of::<i32>()) as i32) };
    if bytes <= 0 {
        return Vec::new();
    }
    pids.truncate(bytes as usize);
    pids.into_iter()
        .filter(|&pid| pid > 0 && pid != me)
        .filter(|&pid| owner(pid) == Some(uid))
        .filter_map(|pid| executable(pid).map(|path| OtherCopy { pid, path }))
        .filter(|copy| is_other_copy(&copy.path, current))
        .collect()
}

fn owner(pid: i32) -> Option<libc::uid_t> {
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    let read = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&mut info as *mut libc::proc_bsdinfo).cast(), size) };
    (read == size).then_some(info.pbi_uid)
}

fn executable(pid: i32) -> Option<PathBuf> {
    let mut buffer = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let length = unsafe { libc::proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    if length <= 0 {
        return None;
    }
    buffer.truncate(length as usize);
    String::from_utf8(buffer).ok().map(PathBuf::from)
}

/// 이미 떠 있는 사본을 앞으로 가져온다. 그 번들을 `open`으로 다시 열면 macOS가 새로 띄우지 않고 실행 중인 앱에 Reopen을 보낸다.
pub fn activate(copy: &OtherCopy) {
    if let Some(bundle) = copy.path.ancestors().nth(3) {
        // 기다리지 않는다. 이쪽은 바로 끝나야 한다.
        let _ = std::process::Command::new("/usr/bin/open").arg(bundle).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_other_ojak_bundles_count() {
        let current = Path::new("/Users/u/Applications/Ojak.app/Contents/MacOS/ai-account-manager");
        assert!(is_other_copy(Path::new("/Users/u/Applications/Ojak.backup-0.13.2-1.app/Contents/MacOS/ai-account-manager"), current));
        assert!(is_other_copy(Path::new("/Applications/Ojak.app/Contents/MacOS/ai-account-manager"), current));
        // 자기 자신, 번들 밖의 같은 이름, 다른 실행 파일은 대상이 아니다.
        assert!(!is_other_copy(current, current));
        assert!(!is_other_copy(Path::new("/tmp/ai-account-manager"), current));
        assert!(!is_other_copy(Path::new("/Users/u/target/release/ai-account-manager"), current));
        assert!(!is_other_copy(Path::new("/Users/u/Applications/Ojak.app/Contents/MacOS/aam-service"), current));
        assert!(!is_other_copy(Path::new("/x/Ojak.bundle/Contents/MacOS/ai-account-manager"), current));
    }

    #[test]
    fn a_running_copy_from_another_bundle_is_found() {
        let root = std::env::temp_dir().join(format!("ojak-dup-{}", std::process::id()));
        let macos = root.join("Old Ojak.app/Contents/MacOS");
        std::fs::create_dir_all(&macos).unwrap();
        let fake = macos.join(EXECUTABLE);
        // 서명된 시스템 바이너리(/bin/sleep)를 복사하면 macOS가 바로 끝내 버린다. 실제 앱처럼 새로 만든 실행 파일을 쓴다.
        let source = root.join("fake.c");
        std::fs::write(&source, "#include <unistd.h>\nint main(void){sleep(30);return 0;}\n").unwrap();
        assert!(std::process::Command::new("/usr/bin/cc").arg("-o").arg(&fake).arg(&source).status().unwrap().success());
        let mut child = std::process::Command::new(&fake).spawn().unwrap();
        let current = root.join("New.app/Contents/MacOS").join(EXECUTABLE);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while find(&current).iter().all(|copy| copy.pid != child.id() as i32) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let found: Vec<_> = find(&current).into_iter().filter(|copy| copy.pid == child.id() as i32).collect();
        assert_eq!(found.len(), 1, "the other bundle copy must be detected");
        let resolved = found[0].path.canonicalize().unwrap();
        assert_eq!(resolved, fake.canonicalize().unwrap());
        child.kill().unwrap();
        let _ = child.wait();
        // 끝난 사본은 더 이상 찾지 않는다.
        assert!(find(&current).iter().all(|copy| copy.pid != child.id() as i32));
        let _ = std::fs::remove_dir_all(root);
    }
}
