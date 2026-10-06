//! ACL-aware snapshot writer for Windows observers. No credentials or text are logged.
use aam_protocol::Paths;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{fs, io::{self, BufRead, Read, Write}, path::{Path, PathBuf}};
use crate::omp_extension::{owned_directory, reject_symlink_ancestors, Result};

const LIMIT: usize = 65_536;
fn private_directory(path: &Path) -> Result<()> {
    reject_symlink_ancestors(path)?;
    if !path.try_exists()? {
        aam_protocol::secure::restrict_dir(path)?;
    }
    // 같은 omp 프로세스의 세션 둘이 writer를 동시에 띄우면 한쪽이 폴더를 만들고 ACL을 조이는 사이에
    // 다른 쪽이 "존재하지만 아직 안전하지 않은" 폴더를 본다. 잠깐 기다렸다 다시 검사한다. 끝내 안전하지 않으면 거절한다.
    let mut attempt = 0;
    loop {
        match owned_directory(path, true) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < 20 => {
                attempt += 1;
                let _ = error;
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(error) => return Err(error),
        }
    }
}
fn write_snapshot(paths: &Paths, bytes: &[u8], parent_pid: u32) -> Result<()> {
    let snapshot: Value = serde_json::from_slice(bytes)?;
    let id = snapshot.get("sessionId").and_then(Value::as_str).filter(|id| !id.is_empty() && id.len() <= 256)
        .ok_or_else(|| io::Error::other("invalid-session"))?;
    if !snapshot.get("sessionFile").and_then(Value::as_str).is_some_and(|path| PathBuf::from(path).is_absolute()) {
        return Err(io::Error::other("invalid-session-file").into());
    }
    if snapshot["version"] != 1 || snapshot["pid"].as_u64() != Some(u64::from(parent_pid)) {
        return Err(io::Error::other("invalid-producer").into());
    }
    private_directory(&paths.home)?;
    let directory = paths.home.join("omp-observations");
    private_directory(&directory)?;
    let digest = format!("{:x}", Sha256::digest(id.as_bytes()));
    let destination = directory.join(format!("{parent_pid}-{digest}.json"));
    match aam_protocol::secure::open_read_no_follow(&destination) {
        Ok(file) => {
            if !aam_protocol::winutil::single_link_regular_file(&file)? || !aam_protocol::winutil::handle_access_is_safe(&file, true)? {
                return Err(io::Error::other("unsafe-file").into());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let temporary = directory.join(format!(".observer-{}.tmp", aam_protocol::new_id()));
    let result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        if !aam_protocol::winutil::handle_access_is_safe(&file, true)? {
            return Err(io::Error::other("unsafe-new-file"));
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &destination)
    })();
    if result.is_err() { let _ = fs::remove_file(&temporary); }
    result.map_err(Into::into)
}

pub(super) fn serve(paths: &Paths) -> Result<()> {
    let pid = aam_protocol::winutil::process_parent(std::process::id()).map_err(|_| io::Error::other("invalid-producer"))?;
    if !aam_protocol::winutil::same_user_process(pid) { return Err(io::Error::other("invalid-producer").into()); }
    let identity = aam_protocol::process_identity(pid).map_err(|_| io::Error::other("invalid-producer"))?;
    let input = io::stdin();
    let mut reader = input.lock();
    let mut output = io::stdout().lock();
    let mut bytes = Vec::with_capacity(LIMIT + 1);
    loop {
        bytes.clear();
        let count = reader.by_ref().take((LIMIT + 2) as u64).read_until(b'\n', &mut bytes)?;
        if count == 0 { return Ok(()); }
        if count > LIMIT + 1 || bytes.last() != Some(&b'\n') { return Err(io::Error::other("snapshot-limit").into()); }
        bytes.pop();
        if !aam_protocol::process_alive(&identity) { return Err(io::Error::other("producer-exited").into()); }
        let written = write_snapshot(paths, &bytes, pid).is_ok();
        // Never return serde/OS errors: they can include private paths or supplied content.
        output.write_all(if written { b"ok\n" } else { b"error\n" })?;
        output.flush()?;
    }
}
