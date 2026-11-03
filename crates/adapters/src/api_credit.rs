//! Console API 키(Claude · OpenAI) fallback의 공용 부품.
//!
//! - 키 저장: macOS Keychain generic password(`ai.aam.api-key`) 또는 Windows 자격 증명 관리자 generic credential
//!   (`ai.aam.api-key:<account_id>`, `CRED_PERSIST_LOCAL_MACHINE`). 키 값은 argv에 싣지 않는다(macOS는 `security -i`의
//!   stdin, Windows는 `CredWriteW` 버퍼). 파일·로그·오류에 키를 남기지 않는다. 공급자는 계정(`Account.provider`)이 정하고
//!   저장소 항목은 계정 ID로만 구분한다.
//! - 단가: 공급자 공개 단가표(Anthropic 2026-10-30, OpenAI 2026-11-01 확인)로 응답 usage를 USD로 추정한다.
//!   모르는 모델은 `None`이고, 호출자는 그 모델을 fallback 대상에서 뺀다(추정할 수 없으면 쓰지 않는다).
//! - 기간: UTC 달력월. 실제 크레딧 지급 주기와 다를 수 있어 화면에 근사임을 표시한다.

use aam_protocol::ApiError;
use sha2::{Digest, Sha256};

/// Keychain service 이름. account 칸에는 Ojak 계정 ID를 쓴다.
pub const KEYCHAIN_SERVICE: &str = "ai.aam.api-key";
/// API 키 계정의 `Account.tool`.
pub const API_TOOL: &str = "api";

/// 이 API 키가 속한 공급자. 계정의 `Account.provider`(`anthropic`·`openai`)와 같은 값이며, 키·단가·검증 서버·fallback 대상 도구가 공급자마다 다르다.
/// 한 공급자의 키를 다른 공급자 경로에 쓰지 않는다(키 값은 상대 서버로 보내지도 않는다).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApiProvider {
    /// Claude Console 키(`claude -p`, omp `anthropic`).
    Anthropic,
    /// OpenAI Platform 키(`codex exec`·`codex review`, omp `openai`). Codex 구독이 아니라 실제 API 요금으로 청구된다.
    OpenAi,
}

impl ApiProvider {
    /// `Account.provider` 값에서. 모르는 값이면 `None`.
    pub fn of(provider: &str) -> Option<Self> {
        match provider {
            "anthropic" => Some(Self::Anthropic),
            "openai" => Some(Self::OpenAi),
            _ => None,
        }
    }

    /// `Account.provider`와 omp 공급자 ID에 쓰는 값. 두 공급자 모두 같다.
    pub fn id(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }

    /// 이 키를 쓸 수 있는 공식 CLI 도구.
    pub fn tool(self) -> &'static str {
        match self {
            Self::Anthropic => "claude",
            Self::OpenAi => "codex",
        }
    }

    /// 이 공급자의 키로 만든 API 키 계정(`tool == "api"`)인지. 다른 공급자의 키 계정은 합계·후보·표시에서 섞지 않는다.
    pub fn owns(self, account: &aam_protocol::Account) -> bool {
        account.tool == API_TOOL && account.provider == self.id()
    }

    /// 실행하려는 공식 CLI 도구(`claude`·`codex`)에 쓰는 API 키 공급자. 다른 도구는 `None`.
    pub fn of_tool(tool: &str) -> Option<Self> {
        match tool {
            "claude" => Some(Self::Anthropic),
            "codex" => Some(Self::OpenAi),
            _ => None,
        }
    }

    /// omp 브릿지 요청의 upstream 공급자(`anthropic`·`openai-codex`)에서, 구독이 모두 소진됐을 때 이어 쓸 API 키 공급자.
    /// `openai-codex`는 ChatGPT 구독 백엔드이고 API 키는 omp의 `openai` 공급자(같은 모델 ID)로 나간다.
    pub fn of_request(provider: &str) -> Option<Self> {
        match provider {
            "anthropic" => Some(Self::Anthropic),
            "openai-codex" => Some(Self::OpenAi),
            _ => None,
        }
    }
}

/// 키로 쓸 수 있는 문자인지(비어 있지 않고 인쇄 가능한 ASCII만). 공급자를 모르는 저장소 읽기에도 쓴다.
fn well_formed(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_graphic())
}

fn format_error() -> ApiError {
    ApiError::new(
        "API_KEY_FORMAT",
        "키가 비어 있거나 공백·제어 문자 등이 들어 있어요. OAuth 토큰·Admin 키·다른 서비스의 키도 이 경로에는 쓸 수 없어요. 입력한 값은 보여 주지 않았어요.",
    )
}

/// HTTP 헤더로 안전하게 전달할 입력인지 확인한다. 키의 접두사·길이·문자 조합은 공급자의 검증에 맡긴다.
/// 별도 인증 경로인 OAuth 토큰·Admin 키와, 다른 공급자의 키로 보이는 값(엉뚱한 서버로 키가 나가지 않게)은 받지 않는다.
pub fn validate_key_format(provider: ApiProvider, key: &str) -> Result<(), ApiError> {
    let foreign: &[&str] = match provider {
        ApiProvider::Anthropic => &["sk-ant-oat", "sk-ant-admin", "sk-proj-", "sk-svcacct-", "sk-admin-"],
        ApiProvider::OpenAi => &["sk-ant-", "sk-admin-"],
    };
    if well_formed(key) && !foreign.iter().any(|prefix| key.starts_with(prefix)) {
        Ok(())
    } else {
        Err(format_error())
    }
}

/// 저장소에 넣거나 꺼낼 때의 검사. 공급자는 등록 때(`validate_key_format`) 정했으므로 여기서는 문자와 인증 경로가 다른 값(OAuth·Admin 키)만 거른다.
fn stored_key_format(key: &str) -> Result<(), ApiError> {
    if well_formed(key) && !["sk-ant-oat", "sk-ant-admin", "sk-admin-"].iter().any(|prefix| key.starts_with(prefix)) {
        Ok(())
    } else {
        Err(format_error())
    }
}

/// 키를 가리킬 때 쓰는 비밀 없는 식별자. Anthropic은 `apikey:<sha256 앞 12 hex>`(기존 형식), OpenAI는 `openai-apikey:<…>`이라
/// 공급자가 달라도 같은 값이 나오지 않는다(gateway 키·계정 중복 검사가 공급자를 가린다).
pub fn identity_key(provider: ApiProvider, key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    match provider {
        ApiProvider::Anthropic => format!("apikey:{hex}"),
        ApiProvider::OpenAi => format!("openai-apikey:{hex}"),
    }
}

#[cfg(not(any(target_os = "macos", windows)))]
fn unsupported() -> ApiError {
    ApiError::new(
        "PLATFORM_UNSUPPORTED",
        "API 키 저장은 지금 macOS Keychain과 Windows 자격 증명 관리자만 지원해요. 이 OS에서는 API 크레딧 연결을 쓸 수 없어요.",
    )
}

#[cfg(target_os = "macos")]
fn keychain_failure() -> ApiError {
    ApiError::new(
        "NATIVE_KEYCHAIN_LOCKED",
        "macOS 키체인에 API 키를 읽거나 쓰지 못했어요. 키체인 잠금과 접근 권한을 확인해 주세요.",
    )
}

/// Windows 자격 증명 관리자 입출력의 순수 부분(대상 이름·blob 한계·blob 해석). 플랫폼과 무관해서 모든 OS에서 테스트한다.
#[cfg(any(windows, test))]
mod credential_codec {
    use super::{stored_key_format, KEYCHAIN_SERVICE};
    use aam_protocol::ApiError;

    /// `CRED_MAX_CREDENTIAL_BLOB_SIZE`(5*512바이트). 키는 ASCII라 바이트 수가 곧 길이다.
    pub(super) const MAX_BLOB_BYTES: usize = 2560;
    /// `CRED_MAX_GENERIC_TARGET_NAME_LENGTH`(32767). 문서가 종료 NUL 포함 여부를 밝히지 않아 NUL까지 한계 안에 넣는다.
    pub(super) const MAX_TARGET_UNITS: usize = 32767;

    pub(super) fn store_failure() -> ApiError {
        ApiError::new(
            "NATIVE_CREDENTIAL_STORE_FAILED",
            "Windows 자격 증명 관리자에 API 키를 읽거나 쓰지 못했어요. 현재 사용자 로그인과 접근 권한을 확인해 주세요.",
        )
    }

    /// `ai.aam.api-key:<account_id>`를 NUL 종료 UTF-16으로. 빈 ID·NUL·길이 초과는 거부한다.
    pub(super) fn target_wide(account_id: &str) -> Result<Vec<u16>, ApiError> {
        if account_id.is_empty() || account_id.contains('\0') {
            return Err(store_failure());
        }
        let mut wide: Vec<u16> = KEYCHAIN_SERVICE
            .encode_utf16()
            .chain(std::iter::once(u16::from(b':')))
            .chain(account_id.encode_utf16())
            .collect();
        if wide.len() >= MAX_TARGET_UNITS {
            return Err(store_failure());
        }
        wide.push(0);
        Ok(wide)
    }

    /// 저장할 blob. 형식 검증 뒤 길이 한계를 확인한다. 한계 초과는 저장소를 건드리기 전에 막는다.
    pub(super) fn blob_for_key(key: &str) -> Result<&[u8], ApiError> {
        stored_key_format(key)?;
        if key.len() > MAX_BLOB_BYTES {
            return Err(ApiError::new(
                "API_KEY_FORMAT",
                "키가 Windows 자격 증명 관리자에 저장할 수 있는 길이(2560바이트)를 넘어요. 입력한 값은 보여 주지 않았어요.",
            ));
        }
        Ok(key.as_bytes())
    }

    /// 읽은 blob을 키로. 비었거나 한계를 넘거나 UTF-8·키 형식이 아니면 오류다(손상된 항목을 다른 값으로 대체하지 않는다).
    pub(super) fn key_from_blob(bytes: &[u8]) -> Result<String, ApiError> {
        if bytes.is_empty() || bytes.len() > MAX_BLOB_BYTES {
            return Err(store_failure());
        }
        let key = std::str::from_utf8(bytes).map_err(|_| store_failure())?;
        stored_key_format(key).map_err(|_| store_failure())?;
        Ok(key.to_owned())
    }
}

/// `CredWriteW`/`CredReadW`/`CredDeleteW` 직접 호출. 항목은 현재 사용자 자격 증명 집합의 generic credential이다.
#[cfg(windows)]
mod windows_store {
    use super::credential_codec::{blob_for_key, key_from_blob, store_failure, target_wide, MAX_BLOB_BYTES};
    use aam_protocol::ApiError;
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_NOT_FOUND, FILETIME};
    use windows_sys::Win32::Security::Credentials::{
        CredDeleteW, CredFree, CredReadW, CredWriteW, CREDENTIALW, CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC,
    };

    /// `CredReadW`가 돌려준 단일 블록. 어떤 경로로 벗어나든(오류·panic 포함) `CredFree`로 돌려준다.
    struct ReadGuard(*mut CREDENTIALW);
    impl Drop for ReadGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: `CredReadW`가 성공으로 돌려준 포인터이고 한 번만 해제한다.
                unsafe { CredFree(self.0.cast::<c_void>().cast_const()) };
            }
        }
    }


    pub(super) fn write(account_id: &str, key: &str) -> Result<(), ApiError> {
        let target = target_wide(account_id)?;
        let blob = blob_for_key(key)?;
        let size = u32::try_from(blob.len()).map_err(|_| store_failure())?;
        let credential = CREDENTIALW {
            Flags: 0,
            Type: CRED_TYPE_GENERIC,
            TargetName: target.as_ptr().cast_mut(),
            Comment: null_mut(),
            LastWritten: FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 },
            CredentialBlobSize: size,
            CredentialBlob: blob.as_ptr().cast_mut(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            AttributeCount: 0,
            Attributes: null_mut(),
            TargetAlias: null_mut(),
            UserName: null_mut(),
        };
        // SAFETY: `credential`은 이 호출 동안 살아 있고, `target`(NUL 종료)·`blob`도 호출이 끝날 때까지 이 스택에 묶여 있다.
        // `CredWriteW`는 입력을 읽기만 하고 포인터를 보관하지 않는다.
        let ok = unsafe { CredWriteW(&credential, 0) };
        if ok == 0 {
            return Err(store_failure());
        }
        Ok(())
    }

    pub(super) fn read(account_id: &str) -> Result<Option<String>, ApiError> {
        let target = target_wide(account_id)?;
        let mut raw: *mut CREDENTIALW = null_mut();
        // SAFETY: `target`은 NUL 종료 UTF-16이고 호출 동안 살아 있다. `raw`는 유효한 출력 슬롯이다.
        let ok = unsafe { CredReadW(target.as_ptr(), CRED_TYPE_GENERIC, 0, &mut raw) };
        if ok == 0 {
            // 실패 직후 바로 읽어야 다른 호출이 오류 코드를 덮지 않는다. 없음만 `None`이고 나머지(접근·세션 등)는 오류다.
            // SAFETY: 인자 없는 스레드-로컬 조회다.
            let code = unsafe { GetLastError() };
            return if code == ERROR_NOT_FOUND { Ok(None) } else { Err(store_failure()) };
        }
        let guard = ReadGuard(raw);
        if guard.0.is_null() {
            return Err(store_failure());
        }
        // SAFETY: 성공한 `CredReadW`가 돌려준 단일 블록이고 `guard`가 살아 있는 동안 유효하다.
        let credential = unsafe { &*guard.0 };
        let size = credential.CredentialBlobSize as usize;
        if credential.Type != CRED_TYPE_GENERIC || credential.CredentialBlob.is_null() || size == 0 || size > MAX_BLOB_BYTES {
            return Err(store_failure());
        }
        // SAFETY: 포인터가 null이 아니고 크기가 0 초과 `MAX_BLOB_BYTES` 이하임을 위에서 확인했다. 블록은 `guard`가 해제하기 전까지 유효하며,
        // 슬라이스는 `key_from_blob`이 `String`으로 복사하는 동안만 쓴다.
        let bytes = unsafe { std::slice::from_raw_parts(credential.CredentialBlob.cast_const(), size) };
        key_from_blob(bytes).map(Some)
    }

    pub(super) fn delete(account_id: &str) -> Result<(), ApiError> {
        let target = target_wide(account_id)?;
        // SAFETY: `target`은 NUL 종료 UTF-16이고 호출 동안 살아 있다.
        let ok = unsafe { CredDeleteW(target.as_ptr(), CRED_TYPE_GENERIC, 0) };
        if ok != 0 {
            return Ok(());
        }
        // SAFETY: 인자 없는 스레드-로컬 조회다. 실패 직후 바로 읽는다.
        let code = unsafe { GetLastError() };
        if code == ERROR_NOT_FOUND {
            Ok(())
        } else {
            Err(store_failure())
        }
    }
}

/// `security -i`에 넘길 한 줄. 인용 부호 안의 `"`와 `\`를 이스케이프한다(키·ID 형식이 이미 막지만 방어적으로).
#[cfg(target_os = "macos")]
fn quoted(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(target_os = "macos")]
fn security_stdin(line: &str) -> Result<std::process::Output, ApiError> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("/usr/bin/security")
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| keychain_failure())?;
    {
        let mut stdin = child.stdin.take().ok_or_else(keychain_failure)?;
        stdin.write_all(line.as_bytes()).map_err(|_| keychain_failure())?;
        stdin.write_all(b"\n").map_err(|_| keychain_failure())?;
    }
    child.wait_with_output().map_err(|_| keychain_failure())
}

/// 키를 OS 자격 저장소에 저장한다(같은 ID면 덮어쓴다).
pub fn store_key(account_id: &str, key: &str) -> Result<(), ApiError> {
    stored_key_format(key)?;
    #[cfg(target_os = "macos")]
    {
        let line = format!(
            "add-generic-password -U -a {} -s {} -w {}",
            quoted(account_id),
            quoted(KEYCHAIN_SERVICE),
            quoted(key)
        );
        let output = security_stdin(&line)?;
        // `security -i`는 명령 실패도 exit 0으로 끝날 수 있어 다시 읽어 확인한다.
        if !output.status.success() || load_key(account_id)?.as_deref() != Some(key) {
            return Err(keychain_failure());
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        // 기존 값을 읽지 못하면 덮어쓰지 않는다.
        let previous = load_key(account_id)?;
        windows_store::write(account_id, key)?;
        // 쓰기 성공 응답만 믿지 않고 다시 읽어 같은 값인지 확인한다.
        let stored = load_key(account_id);
        if !matches!(&stored, Ok(Some(value)) if value == key) {
            // 새 항목이면 지우고, 덮어쓴 것이면 이전 값을 되돌린다.
            match previous {
                None => {
                    let _ = windows_store::delete(account_id);
                }
                Some(old) => {
                    let _ = windows_store::write(account_id, &old);
                }
            }
            return Err(credential_codec::store_failure());
        }
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = account_id;
        Err(unsupported())
    }
}

/// 저장된 키. 없으면 `Ok(None)`. 잠김·거부·손상은 오류다(다른 키로 대체하지 않는다).
pub fn load_key(account_id: &str) -> Result<Option<String>, ApiError> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/bin/security")
            .args(["find-generic-password", "-a", account_id, "-s", KEYCHAIN_SERVICE, "-w"])
            .output()
            .map_err(|_| keychain_failure())?;
        if output.status.code() == Some(44) {
            return Ok(None);
        }
        if !output.status.success() {
            return Err(keychain_failure());
        }
        let key = String::from_utf8(output.stdout).map_err(|_| keychain_failure())?;
        let key = key.trim_end_matches(['\r', '\n']).to_owned();
        stored_key_format(&key).map_err(|_| keychain_failure())?;
        Ok(Some(key))
    }
    #[cfg(windows)]
    {
        windows_store::read(account_id)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = account_id;
        Err(unsupported())
    }
}

/// 키를 OS 자격 저장소에서 지운다. 이미 없으면 성공이다.
pub fn delete_key(account_id: &str) -> Result<(), ApiError> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/usr/bin/security")
            .args(["delete-generic-password", "-a", account_id, "-s", KEYCHAIN_SERVICE])
            .output()
            .map_err(|_| keychain_failure())?;
        if output.status.success() || output.status.code() == Some(44) {
            Ok(())
        } else {
            Err(keychain_failure())
        }
    }
    #[cfg(windows)]
    {
        windows_store::delete(account_id)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        let _ = account_id;
        Ok(())
    }
}

/// 응답 하나의 토큰 사용량. 공급자 응답(omp `usage`, `claude -p --output-format json`)에서 읽는다.
/// `input`은 캐시를 뺀 입력 토큰이다(omp가 OpenAI의 `input_tokens`에서 `cached_tokens`와 캐시 기록분을 뺀 값을 준다).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

/// 모델 ID를 공개 단가표의 이름과 정확히 맞춘다. `anthropic/` 접두와 공식 날짜 접미(`-YYYYMMDD`)만 허용하고,
/// 비슷한 미등록 버전(`claude-opus-5-6`)이나 다른 접두·접미는 모르는 모델로 본다(추정할 수 없으면 쓰지 않는다).
fn base_model(model: &str) -> Option<&'static str> {
    const KNOWN: &[&str] = &[
        "claude-fable-5-1", "claude-mythos-5-1", "claude-fable-5", "claude-mythos-5",
        "claude-opus-5-5", "claude-opus-5", "claude-opus-4-8", "claude-opus-4-7", "claude-opus-4-6", "claude-opus-4-5",
        "claude-sonnet-5-5", "claude-sonnet-5", "claude-sonnet-4-6", "claude-sonnet-4-5",
        "claude-haiku-5-5", "claude-haiku-4-5",
    ];
    let m = model.strip_prefix("anthropic/").unwrap_or(model);
    KNOWN.iter().copied().find(|known| {
        m.strip_prefix(known).is_some_and(|rest| {
            rest.is_empty() || (rest.len() == 9 && rest.starts_with('-') && rest[1..].bytes().all(|b| b.is_ascii_digit()))
        })
    })
}

/// OpenAI 모델 ID를 공개 단가표의 이름과 정확히 맞춘다. 정확한 ID와 `openai/` 접두만 허용한다. 날짜 스냅샷·`-pro`·`-codex` 같은
/// 변형은 단가표에 따로 있거나 없으므로 모르는 모델로 본다(추정할 수 없으면 쓰지 않는다).
fn openai_base_model(model: &str) -> Option<&'static str> {
    const KNOWN: &[&str] = &[
        "gpt-6-astra", "gpt-6.1-sol", "gpt-6.1-terra", "gpt-6-luna", "gpt-6-sol",
        "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5", "gpt-5.4",
    ];
    let m = model.strip_prefix("openai/").unwrap_or(model);
    KNOWN.iter().copied().find(|known| *known == m)
}

/// 한 요청의 단가(USD per MTok). 캐시 기록 단가는 TTL(30분·12시간)을 usage만으로 알 수 없어 비싼 쪽을 쓴다(추정이 실제보다 작아지지 않게).
#[derive(Clone, Copy)]
struct Rates {
    input: f64,
    cache_write: f64,
    cache_read: f64,
    output: f64,
}

/// Anthropic: (input, cache write 1h, cache read, output).
fn rates(model: &str, prompt_tokens: u64) -> Option<Rates> {
    let (input, cache_write, cache_read, output) = match base_model(model)? {
        "claude-fable-5-1" | "claude-mythos-5-1" => (10.0, 20.0, 0.25, 50.0),
        "claude-fable-5" | "claude-mythos-5" => (10.0, 20.0, 1.0, 50.0),
        "claude-opus-5-5" => (4.0, 8.0, 0.20, 20.0),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6" | "claude-opus-4-5" => (5.0, 10.0, 0.50, 25.0),
        "claude-sonnet-5-5" => (2.0, 4.0, 0.10, 10.0),
        "claude-sonnet-5" => (2.0, 4.0, 0.20, 10.0),
        "claude-sonnet-4-6" | "claude-sonnet-4-5" => (3.0, 6.0, 0.30, 15.0),
        "claude-haiku-5-5" if prompt_tokens > 100_000 => (0.50, 1.0, 0.05, 2.50),
        "claude-haiku-5-5" => (0.10, 0.20, 0.01, 0.50),
        "claude-haiku-4-5" => (1.0, 2.0, 0.10, 5.0),
        _ => return None,
    };
    Some(Rates { input, cache_write, cache_read, output })
}

/// OpenAI 표준(Standard) 단가(2026-11-01 확인, developers.openai.com/api/docs/pricing). 입력 토큰이 272K 이하면 짧은 문맥, 넘으면 긴 문맥 단가를
/// 요청 전체에 적용한다. 캐시 기록은 12시간 기록 단가(짧은 문맥 입력의 2.5배·gpt-5.5/5.4는 기록 단가가 따로 없어 입력 단가).
/// Fast·Flex·Batch·Priority 같은 다른 처리 등급과 지역 처리 할증(10%)은 반영하지 않는다. 호출자는 기본 등급이 아닌 요청에 이 키를 쓰지 않는다.
fn openai_rates(model: &str, prompt_tokens: u64) -> Option<Rates> {
    const LONG_CONTEXT_TOKENS: u64 = 272_000;
    let long = prompt_tokens > LONG_CONTEXT_TOKENS;
    // (short input, short output, short cached, short 12h write), (long input, long output, long cached, long 12h write)
    let (short, long_rates): ((f64, f64, f64, f64), (f64, f64, f64, f64)) = match openai_base_model(model)? {
        "gpt-6-astra" => ((10.0, 50.0, 1.00, 25.0), (20.0, 75.0, 2.00, 50.0)),
        "gpt-6.1-sol" => ((2.0, 10.0, 0.10, 5.0), (4.0, 15.0, 0.20, 10.0)),
        "gpt-6.1-terra" => ((1.0, 5.0, 0.05, 2.5), (2.0, 7.5, 0.10, 5.0)),
        "gpt-6-luna" => ((0.10, 0.50, 0.01, 0.25), (0.20, 0.75, 0.02, 0.50)),
        "gpt-6-sol" => ((2.0, 10.0, 0.20, 5.0), (4.0, 15.0, 0.40, 10.0)),
        "gpt-5.6-sol" => ((4.0, 20.0, 0.40, 10.0), (8.0, 30.0, 0.80, 20.0)),
        "gpt-5.6-terra" => ((2.0, 12.0, 0.20, 5.0), (4.0, 18.0, 0.40, 10.0)),
        "gpt-5.6-luna" => ((0.20, 1.20, 0.02, 0.50), (0.40, 1.80, 0.04, 1.00)),
        // 기록 단가가 표에 없다: 입력 단가로 센다.
        "gpt-5.5" => ((5.0, 30.0, 0.50, 5.0), (10.0, 45.0, 1.00, 10.0)),
        "gpt-5.4" => ((2.5, 15.0, 0.25, 2.5), (5.0, 22.5, 0.50, 5.0)),
        _ => return None,
    };
    let (input, output, cache_read, cache_write) = if long { long_rates } else { short };
    Some(Rates { input, cache_write, cache_read, output })
}

fn rates_for(provider: ApiProvider, model: &str, prompt_tokens: u64) -> Option<Rates> {
    match provider {
        ApiProvider::Anthropic => rates(model, prompt_tokens),
        ApiProvider::OpenAi => openai_rates(model, prompt_tokens),
    }
}

/// 단가를 아는 모델인지. 모르면 API 키 fallback에 쓰지 않는다. 한 공급자의 단가표에 있는 모델이 다른 공급자 키에서는 모르는 모델이다.
pub fn priced(provider: ApiProvider, model: &str) -> bool {
    rates_for(provider, model, 0).is_some()
}

/// 응답 하나의 추정 금액(USD). 모르는 모델이면 `None`.
pub fn cost_usd(provider: ApiProvider, model: &str, usage: Usage) -> Option<f64> {
    let prompt = usage.input + usage.cache_read + usage.cache_write;
    let rates = rates_for(provider, model, prompt)?;
    let per = |tokens: u64, rate: f64| tokens as f64 * rate / 1_000_000.0;
    Some(per(usage.input, rates.input) + per(usage.cache_write, rates.cache_write) + per(usage.cache_read, rates.cache_read) + per(usage.output, rates.output))
}

/// `now`(ms)가 속한 UTC 달력월의 시작 시각(ms).
pub fn period_start(now_ms: i64) -> i64 {
    let days = now_ms.div_euclid(86_400_000);
    // civil-from-days (Howard Hinnant). 1970-01-01 기준 일수를 연·월로 바꾼다.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    (days - (day - 1)) * 86_400_000
}

#[cfg(test)]
mod tests {
    use super::*;

    use ApiProvider::{Anthropic, OpenAi};

    #[test]
    fn key_format_rejects_oauth_and_admin_and_hides_value() {
        let good = format!("sk-ant-api03-{}", "a".repeat(40));
        assert!(validate_key_format(Anthropic, &good).is_ok());
        for bad in [format!("sk-ant-oat01-{}", "a".repeat(40)), format!("sk-ant-admin01-{}", "a".repeat(40)), "key\ninjected".into(), "key\rheader".into(), "key\0nul".into(), "key\ttab".into(), "unicode-secret-가나다".into(), format!("sk-ant-api03-{} x", "a".repeat(40))] {
            let error = validate_key_format(Anthropic, &bad).unwrap_err();
            assert_eq!(error.code, "API_KEY_FORMAT");
            assert!(!error.message.contains(&bad));
        }
        assert!(validate_key_format(Anthropic, "").is_err());
    }

    #[test]
    fn key_validation_does_not_guess_provider_key_shape() {
        for key in ["opaque-key.v2:+/=".to_owned(), format!("sk-ant-api03-{}", "a".repeat(300)), "short".to_owned()] {
            assert!(validate_key_format(Anthropic, &key).is_ok());
        }
        for key in ["opaque-key.v2:+/=".to_owned(), format!("sk-proj-{}", "a".repeat(100)), "short".to_owned()] {
            assert!(validate_key_format(OpenAi, &key).is_ok());
        }
    }

    /// 한 공급자의 키를 다른 공급자 경로에 등록할 수 없다(키가 엉뚱한 서버로 나가지 않게). OpenAI Admin 키도 받지 않는다.
    #[test]
    fn key_format_never_crosses_providers_and_rejects_admin_keys() {
        let openai = format!("sk-proj-{}", "a".repeat(60));
        let claude = format!("sk-ant-api03-{}", "a".repeat(60));
        assert!(validate_key_format(Anthropic, &openai).is_err());
        assert!(validate_key_format(OpenAi, &claude).is_err());
        let admin = format!("sk-admin-{}", "a".repeat(60));
        for provider in [Anthropic, OpenAi] {
            let error = validate_key_format(provider, &admin).unwrap_err();
            assert_eq!(error.code, "API_KEY_FORMAT");
            assert!(!error.message.contains("sk-admin"));
        }
        for bad in ["key\ninjected", "key with space", "", "한글키"] {
            assert!(validate_key_format(OpenAi, bad).is_err(), "{bad:?}");
        }
        // 저장소 입출력은 공급자를 모르므로 문자와 OAuth·Admin 키만 거른다.
        assert!(stored_key_format(&openai).is_ok() && stored_key_format(&claude).is_ok());
        assert!(stored_key_format(&admin).is_err());
    }

    #[test]
    fn identity_is_stable_secret_free_and_never_collides_across_providers() {
        let key = format!("sk-ant-api03-{}", "b".repeat(40));
        let id = identity_key(Anthropic, &key);
        assert_eq!(id, identity_key(Anthropic, &key));
        assert!(id.starts_with("apikey:") && id.len() == "apikey:".len() + 12);
        assert!(!id.contains("sk-ant"));
        // 같은 값을 두 공급자에 등록해도 gateway 키·중복 검사가 서로 가리지 않는다.
        let other = identity_key(OpenAi, &key);
        assert!(other.starts_with("openai-apikey:") && other != id);
        assert!(!other.contains(&key));
    }

    #[test]
    fn cost_uses_per_model_cache_rates_and_rejects_unknown_models() {
        let usage = Usage { input: 1_000_000, output: 1_000_000, cache_read: 1_000_000, cache_write: 1_000_000 };
        // Opus 5.5: 4 + 20 + 0.20 + 8 (1h write)
        assert!((cost_usd(Anthropic, "claude-opus-5-5", usage).unwrap() - 32.20).abs() < 1e-9);
        assert!((cost_usd(Anthropic, "anthropic/claude-sonnet-5-5", usage).unwrap() - 16.10).abs() < 1e-9);
        // Haiku 5.5는 프롬프트 100k 초과 시 높은 구간 단가.
        let small = Usage { input: 1_000, output: 0, cache_read: 0, cache_write: 0 };
        assert!((cost_usd(Anthropic, "claude-haiku-5-5", small).unwrap() - 0.0001).abs() < 1e-12);
        assert_eq!(cost_usd(Anthropic, "claude-future-9", usage), None);
        assert!(!priced(Anthropic, "gpt-6-astra"));
        // 비슷한 미등록 버전·다른 접두/접미는 모르는 모델이다. 공식 날짜 접미와 `anthropic/` 접두만 허용한다.
        for unknown in ["claude-opus-5-6", "claude-fable-5-10", "claude-sonnet-5-5-preview", "my-claude-opus-5-5", "claude-opus-5-5-2026", "Claude-Opus-5-5", "anthropic/claude-opus-5-5x"] {
            assert!(!priced(Anthropic, unknown), "{unknown}");
        }
        for known in ["claude-opus-5-5", "anthropic/claude-opus-5-5", "claude-sonnet-4-5-20250929", "claude-fable-5-1"] {
            assert!(priced(Anthropic, known), "{known}");
        }
    }

    /// OpenAI 표준 단가표(2026-11-01): 캐시 읽기·쓰기 단가, 272K 경계의 긴 문맥 전환, 단가 없는 모델, 공급자 격리.
    #[test]
    fn openai_cost_uses_cached_write_and_long_context_rates_and_stays_in_its_provider() {
        let near = |left: f64, right: f64| (left - right).abs() < 1e-9;
        // gpt-6.1-sol 짧은 문맥: input $2 + output $10 + cached $0.10 + 12h write $5 per MTok.
        let usage = Usage { input: 1_000_000, output: 1_000_000, cache_read: 1_000_000, cache_write: 1_000_000 };
        // 프롬프트 합계가 272K를 넘으므로 긴 문맥 단가: 4 + 15 + 0.20 + 10.
        assert!(near(cost_usd(OpenAi, "gpt-6.1-sol", usage).unwrap(), 29.20));
        // 272,000토큰 정확히는 짧은 문맥, 272,001부터 긴 문맥(요청 전체에 적용).
        let edge = |input: u64| Usage { input, output: 1_000_000, cache_read: 0, cache_write: 0 };
        assert!(near(cost_usd(OpenAi, "gpt-6.1-sol", edge(272_000)).unwrap(), 272_000.0 * 2.0 / 1e6 + 10.0));
        assert!(near(cost_usd(OpenAi, "gpt-6.1-sol", edge(272_001)).unwrap(), 272_001.0 * 4.0 / 1e6 + 15.0));
        // 캐시 읽기 토큰도 문맥 길이에 들어간다: input 100K + cached 200K = 300K → 긴 문맥, cached $0.20.
        let cached = Usage { input: 100_000, output: 0, cache_read: 200_000, cache_write: 0 };
        assert!(near(cost_usd(OpenAi, "gpt-6.1-sol", cached).unwrap(), 100_000.0 * 4.0 / 1e6 + 200_000.0 * 0.20 / 1e6));
        let short = Usage { input: 100_000, output: 0, cache_read: 100_000, cache_write: 50_000 };
        assert!(near(cost_usd(OpenAi, "gpt-6.1-sol", short).unwrap(), 100_000.0 * 2.0 / 1e6 + 100_000.0 * 0.10 / 1e6 + 50_000.0 * 5.0 / 1e6));
        // gpt-5.5·5.4는 캐시 기록 단가가 표에 없어 입력 단가로 센다.
        let write = Usage { input: 0, output: 0, cache_read: 0, cache_write: 100_000 };
        assert!(near(cost_usd(OpenAi, "gpt-5.5", write).unwrap(), 100_000.0 * 5.0 / 1e6));
        assert!(near(cost_usd(OpenAi, "openai/gpt-5.4", write).unwrap(), 100_000.0 * 2.5 / 1e6));
        // 단가표에 없는 모델·변형은 추정하지 않는다(미계량으로 다룬다).
        for unknown in ["gpt-6-astra-pro", "gpt-6-astra-20261001", "gpt-5.5-pro", "gpt-5.6", "GPT-6-ASTRA", "gpt-9", "openai/gpt-6-astra-x", "codex-mini-latest", "gpt-image-2"] {
            assert!(!priced(OpenAi, unknown), "{unknown}");
            assert_eq!(cost_usd(OpenAi, unknown, usage), None, "{unknown}");
        }
        for known in ["gpt-6-astra", "gpt-6.1-sol", "gpt-6.1-terra", "gpt-6-luna", "gpt-6-sol", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5", "gpt-5.4", "openai/gpt-5.5"] {
            assert!(priced(OpenAi, known), "{known}");
        }
        // 공급자 격리: Claude 모델은 OpenAI 키로, OpenAI 모델은 Claude 키로 단가를 구하지 않는다.
        assert!(!priced(OpenAi, "claude-opus-5-5") && !priced(Anthropic, "gpt-6.1-sol"));
        assert_eq!(cost_usd(OpenAi, "claude-opus-5-5", usage), None);
        assert_eq!(cost_usd(Anthropic, "gpt-6.1-sol", usage), None);
    }

    #[test]
    fn period_start_is_utc_month_boundary() {
        // 2026-10-30T12:00:00Z → 2026-10-01T00:00:00Z
        assert_eq!(period_start(1_793_361_600_000), 1_790_812_800_000);
        // 2024-02-29T23:59:59.999Z (윤년) → 2024-02-01
        assert_eq!(period_start(1_709_251_199_999), 1_706_745_600_000);
        // 경계 자체는 그 달의 시작이다.
        assert_eq!(period_start(1_790_812_800_000), 1_790_812_800_000);
    }

    #[test]
    fn credential_target_is_prefixed_nul_terminated_and_bounded() {
        use credential_codec::{target_wide, MAX_TARGET_UNITS};
        let wide = target_wide("acc-1").unwrap();
        assert_eq!(wide.last(), Some(&0));
        assert_eq!(String::from_utf16(&wide[..wide.len() - 1]).unwrap(), "ai.aam.api-key:acc-1");
        let prefix = KEYCHAIN_SERVICE.len() + 1;
        // NUL을 포함해 32767 유닛까지만 받고 하나 넘으면 거부한다. 빈 ID·NUL은 거부한다.
        assert_eq!(target_wide(&"a".repeat(MAX_TARGET_UNITS - 1 - prefix)).unwrap().len(), MAX_TARGET_UNITS);
        assert_eq!(target_wide(&"a".repeat(MAX_TARGET_UNITS - prefix)).unwrap_err().code, "NATIVE_CREDENTIAL_STORE_FAILED");
        assert!(target_wide("").is_err());
        assert!(target_wide("a\0b").is_err());
    }

    #[test]
    fn credential_blob_length_boundary() {
        use credential_codec::{blob_for_key, key_from_blob, MAX_BLOB_BYTES};
        let max = "k".repeat(MAX_BLOB_BYTES);
        assert_eq!(blob_for_key(&max).unwrap().len(), MAX_BLOB_BYTES);
        assert_eq!(key_from_blob(max.as_bytes()).unwrap(), max);
        let over = "k".repeat(MAX_BLOB_BYTES + 1);
        let error = blob_for_key(&over).unwrap_err();
        assert_eq!(error.code, "API_KEY_FORMAT");
        assert!(!error.message.contains(&over));
        assert_eq!(key_from_blob(over.as_bytes()).unwrap_err().code, "NATIVE_CREDENTIAL_STORE_FAILED");
        // 형식이 틀린 키는 길이 검사 전에 형식 오류다.
        assert_eq!(blob_for_key("bad key").unwrap_err().code, "API_KEY_FORMAT");
        assert_eq!(blob_for_key("").unwrap_err().code, "API_KEY_FORMAT");
    }

    #[test]
    fn malformed_credential_blob_is_an_error_not_a_panic_or_a_key() {
        use credential_codec::key_from_blob;
        let oauth = format!("sk-ant-oat01-{}", "a".repeat(20));
        let admin = format!("sk-ant-admin01-{}", "a".repeat(20));
        let cases: [&[u8]; 10] = [
            b"",
            b"\xff\xfe\xfd",
            b"valid\xc3",
            "가나다".as_bytes(),
            b"key\n",
            b"key with space",
            b"key\0",
            // UTF-16 LE로 저장된 값(다른 도구가 쓴 항목)은 NUL이 섞여 거부된다.
            b"k\0e\0y\0",
            oauth.as_bytes(),
            admin.as_bytes(),
        ];
        for bytes in cases {
            let error = key_from_blob(bytes).unwrap_err();
            assert_eq!(error.code, "NATIVE_CREDENTIAL_STORE_FAILED", "{bytes:?}");
            assert!(!error.message.contains("key"), "{bytes:?}");
        }
        assert_eq!(key_from_blob(b"opaque-key.v2:+/=").unwrap(), "opaque-key.v2:+/=");
    }
}

/// 실제 macOS Keychain 왕복. 사용자 키체인을 건드리므로 기본 실행에서는 빠진다.
/// `cargo test -p aam-adapters keychain_roundtrip -- --ignored`로만 실행한다. 고유 임시 ID만 쓰고 끝에 그 항목만 지운다.
#[cfg(all(test, target_os = "macos"))]
mod keychain {
    use super::*;

    struct Cleanup(String);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = delete_key(&self.0);
        }
    }

    #[test]
    #[ignore = "touches the real login keychain"]
    fn keychain_roundtrip_store_overwrite_delete() {
        let id = format!("ojak-test-{}", uuid::Uuid::new_v4());
        let _cleanup = Cleanup(id.clone());
        let first = format!("sk-ant-api03-{}", "K".repeat(40));
        let second = format!("sk-ant-api03-{}", "L".repeat(40));
        assert_eq!(load_key(&id).unwrap(), None);
        store_key(&id, &first).unwrap();
        assert_eq!(load_key(&id).unwrap().as_deref(), Some(first.as_str()));
        store_key(&id, &second).unwrap();
        assert_eq!(load_key(&id).unwrap().as_deref(), Some(second.as_str()));
        delete_key(&id).unwrap();
        assert_eq!(load_key(&id).unwrap(), None);
        delete_key(&id).unwrap();
    }
}

/// 실제 Windows 자격 증명 관리자 왕복. 현재 사용자 자격 증명 집합을 건드리므로 기본 실행에서는 빠진다.
/// `cargo test -p aam-adapters windows_credential_roundtrip -- --ignored`로만 실행한다. 고유 UUID 대상 이름만 쓰고,
/// 실패·panic으로 빠져나가도 `Drop`에서 그 항목만 지운다.
#[cfg(all(test, windows))]
mod windows_credential {
    use super::credential_codec::MAX_BLOB_BYTES;
    use super::*;

    struct Cleanup(Vec<String>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for id in &self.0 {
                let _ = delete_key(id);
            }
        }
    }

    #[test]
    #[ignore = "touches the real Windows Credential Manager"]
    fn windows_credential_roundtrip_overwrite_absent_delete_isolated() {
        let a = format!("ojak-test-{}", uuid::Uuid::new_v4());
        let b = format!("ojak-test-{}", uuid::Uuid::new_v4());
        // 가드를 먼저 만들어 이후 단언이 panic해도 두 항목이 지워진다.
        let _cleanup = Cleanup(vec![a.clone(), b.clone()]);
        let first = format!("sk-ant-api03-{}", "K".repeat(40));
        let second = format!("sk-ant-api03-{}", "L".repeat(40));
        let other = format!("sk-ant-api03-{}", "M".repeat(40));

        // 없음은 오류가 아니라 None이고, 없는 항목 삭제는 성공이다.
        assert_eq!(load_key(&a).unwrap(), None);
        delete_key(&a).unwrap();

        store_key(&a, &first).unwrap();
        store_key(&b, &other).unwrap();
        assert_eq!(load_key(&a).unwrap().as_deref(), Some(first.as_str()));
        assert_eq!(load_key(&b).unwrap().as_deref(), Some(other.as_str()));

        // 덮어쓰기는 대상 항목만 바꾼다.
        store_key(&a, &second).unwrap();
        assert_eq!(load_key(&a).unwrap().as_deref(), Some(second.as_str()));
        assert_eq!(load_key(&b).unwrap().as_deref(), Some(other.as_str()));

        // 삭제도 대상 항목만 지운다.
        delete_key(&a).unwrap();
        assert_eq!(load_key(&a).unwrap(), None);
        assert_eq!(load_key(&b).unwrap().as_deref(), Some(other.as_str()));
        delete_key(&a).unwrap();

        // blob 한계(2560바이트)는 저장·재읽기가 되고, 한계 초과는 저장소를 건드리지 않고 거부된다.
        let max = "k".repeat(MAX_BLOB_BYTES);
        store_key(&a, &max).unwrap();
        assert_eq!(load_key(&a).unwrap().as_deref(), Some(max.as_str()));
        let over = "k".repeat(MAX_BLOB_BYTES + 1);
        assert_eq!(store_key(&a, &over).unwrap_err().code, "API_KEY_FORMAT");
        assert_eq!(load_key(&a).unwrap().as_deref(), Some(max.as_str()));
        delete_key(&a).unwrap();
        delete_key(&b).unwrap();
        assert_eq!(load_key(&b).unwrap(), None);
    }
}
