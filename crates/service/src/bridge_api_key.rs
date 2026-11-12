//! API 키(Claude · OpenAI) fallback의 브릿지 쪽 부품.
//!
//! - `ApiKeySource`: Ojak 내부 loopback broker(`/native/<accountId>/...`)가 Keychain의 API 키 하나를 omp `auth-gateway`에
//!   `{"type":"api_key"}` 자격 증명으로 내준다(공급자 ID는 omp의 `anthropic`·`openai`). 키는 메모리에만 두며 로그·오류·응답 본문(snapshot 제외)에 싣지 않는다.
//! - `UsageScanner`: 응답 스트림을 그대로 넘기면서 마지막 `done`/`error` 이벤트의 `usage`만 읽는다(프롬프트·본문은 보관하지 않는다).
//! - 사용 가능 판정(`usable`)과 응답 분류(`rejected`·`credit_exhausted`, 공급자별 문구).
use super::*;
use aam_adapters::api_credit::{self, ApiProvider, Usage};
use parking_lot::Mutex as Lock;
use std::sync::Weak;

const POLL_SLICE: Duration = Duration::from_millis(500);
/// 키는 바뀌지 않으므로 snapshot 세대는 고정이다. 키가 바뀌면 identity가 달라져 gateway가 새로 뜬다.
const GENERATION: i64 = 1;
/// 종료 이벤트 한 줄에서 `usage`를 찾으려고 보관하는 뒤쪽 바이트 수. `usage`는 본문(content)보다 뒤에 있고 그 뒤 필드는 짧다.
const TAIL_CAP: usize = 128 * 1024;
const HEAD_LEN: usize = 24;

/// 이 API 키 계정을 이번 요청에 쓸 수 있는지: 그 공급자의 키이고, 켜져 있고 인증됐고, `used_usd`가 상한 미만.
/// Claude 키는 `used_usd`로 그 키 자신의 이번 달(UTC 달력월) 추정 사용액을 넘겨 키마다 따로 상한을 적용한다.
/// OpenAI 키는 모든 OpenAI 키(지운 키 포함)의 합계를 넘긴다.
pub(crate) fn usable(provider: ApiProvider, account: &Account, used_usd: f64, limit_usd: f64) -> bool {
    provider.owns(account)
        && account.enabled
        && account.auth_status == "authenticated"
        && limit_usd.is_finite()
        && limit_usd > 0.0
        && used_usd < limit_usd
}

/// 공급자가 키 자체를 거절한 응답 문구인지. Anthropic은 `authentication_error`·`invalid x-api-key`, OpenAI는 `invalid_api_key`·`Incorrect API key provided`다.
pub(crate) fn rejection_text(provider: ApiProvider, text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    let markers: &[&str] = match provider {
        ApiProvider::Anthropic => &["authentication_error", "invalid x-api-key", "invalid api key"],
        ApiProvider::OpenAi => &["invalid_api_key", "incorrect api key", "invalid api key"],
    };
    markers.iter().any(|marker| text.contains(marker))
}

fn error_response(head: &str, text: &str) -> bool {
    let status: u16 = head.split(' ').nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
    status >= 400 || text.contains("\"type\":\"error\"") || text.contains("event: error")
}

/// 키를 거절한 응답(401 `authentication_error`·`invalid_api_key`)인지. gateway 자신의 토큰 거절(`{"error":"unauthorized"}`)은 해당하지 않는다.
pub(crate) fn rejected(provider: ApiProvider, head: &[u8], peek: &[u8]) -> bool {
    let (head, text) = (String::from_utf8_lossy(head), String::from_utf8_lossy(peek));
    error_response(&head, &text) && rejection_text(provider, &text)
}

/// 응답 머리의 상태 코드가 2xx인지.
pub(crate) fn succeeded(head: &[u8]) -> bool {
    let head = String::from_utf8_lossy(head);
    head.split(' ').nth(1).and_then(|code| code.parse::<u16>().ok()).is_some_and(|status| (200..300).contains(&status))
}

/// 조직의 크레딧 잔액·한도가 바닥난 응답인지. Anthropic은 400 "credit balance is too low", OpenAI는 429 `insufficient_quota`·`billing_hard_limit_reached`다.
pub(crate) fn credit_exhausted(provider: ApiProvider, head: &[u8], peek: &[u8]) -> bool {
    let (head, text) = (String::from_utf8_lossy(head), String::from_utf8_lossy(peek));
    let lower = text.to_ascii_lowercase();
    error_response(&head, &text)
        && match provider {
            ApiProvider::Anthropic => lower.contains("credit balance"),
            ApiProvider::OpenAi => lower.contains("insufficient_quota") || lower.contains("billing_hard_limit_reached") || lower.contains("exceeded your current quota"),
        }
}

/// 다음 달(UTC) 시작 시각. 잔액 소진 차단을 이 시각까지 건다.
pub(crate) fn next_period_start(now: i64) -> i64 {
    api_credit::period_start(api_credit::period_start(now) + 32 * 86_400_000)
}

/// 키를 거절당했음을 계정에 기록한다. 이후 `sync`가 gateway를 내리고 후보에서 빠진다. 키 값은 다루지 않는다.
pub(super) fn mark_auth_required(service: &Service, account_id: &str) {
    let Ok(store) = service.lock() else { return };
    let Ok(mut account) = crate::store::account(&store.connection, account_id) else { return };
    if account.tool != api_credit::API_TOOL || account.auth_status == "auth-required" {
        return;
    }
    account.auth_status = "auth-required".into();
    account.can_launch = false;
    account.reason = Some(match ApiProvider::of(&account.provider) {
        Some(ApiProvider::OpenAi) => "OpenAI가 이 API 키를 거절했어요. Ojak → 연결에서 키를 다시 등록해 주세요.",
        _ => "Anthropic이 이 API 키를 거절했어요. Ojak → 연결에서 키를 다시 등록해 주세요.",
    }.into());
    account.last_checked_at = now_ms();
    let _ = crate::store::save_account(&store.connection, &account);
}

pub(super) fn failure(code: &str) -> (u16, &'static str) {
    match code {
        "AUTH_REQUIRED" => (401, "API 키를 쓸 수 없어요. 키가 거절됐거나 자격 증명 저장소에 없어요. Ojak → 연결에서 키를 다시 등록해 주세요."),
        "ACCOUNT_DISABLED" => (409, "API 키 계정이 배정에서 제외되어 있어요. Ojak → 연결에서 해당 키를 포함해 주세요."),
        "PROFILE_IDENTITY_MISMATCH" => (409, "자격 증명 저장소의 API 키가 등록된 키와 달라요. Ojak → 연결에서 키를 다시 등록해 주세요."),
        "NATIVE_KEYCHAIN_LOCKED" => (503, "macOS 키체인에서 API 키를 읽지 못했어요. 키체인 잠금과 접근 권한을 확인해 주세요."),
        "NATIVE_CREDENTIAL_STORE_FAILED" => (503, "Windows 자격 증명 관리자에서 API 키를 읽지 못했어요. 현재 사용자 로그인과 접근 권한을 확인해 주세요."),
        "PLATFORM_UNSUPPORTED" => (409, "이 OS에서는 API 키 저장을 지원하지 않아요."),
        "AUTH_OVERRIDE_CONFLICT" => (409, "Ojak 서비스 환경에 API 키·토큰·base URL 같은 인증 재정의가 있어 API 키를 내주지 않았어요. 해당 변수를 지운 뒤 서비스를 다시 시작해 주세요."),
        _ => (503, "API 키 계정 상태를 확인하지 못했어요. 잠시 뒤 다시 시도해 주세요."),
    }
}

fn auth_error(client: &mut TcpStream, error: ApiError) {
    let (status, message) = failure(&error.code);
    respond(client, status, &json!({"code":error.code,"error":message}));
}

pub(super) struct ApiKeySource {
    pub account_id: String,
    pub provider: ApiProvider,
    /// 계정의 `identity_key`(`apikey:<12hex>`·`openai-apikey:<12hex>`). 키가 바뀌면 달라지므로 gateway가 새로 뜬다.
    pub identity: String,
    owner: Weak<Service>,
    /// 메모리 전용 캐시. 매 snapshot마다 Keychain 프로세스를 띄우지 않는다. 로그·진단에 싣지 않는다.
    key: Lock<Option<String>>,
}

impl ApiKeySource {
    pub fn new(account_id: String, provider: ApiProvider, identity: String, owner: Weak<Service>) -> Self {
        Self { account_id, provider, identity, owner, key: Lock::new(None) }
    }

    /// 테스트가 Keychain 없이 키를 메모리에 둔다.
    #[cfg(test)]
    pub fn with_key(self, key: &str) -> Self {
        *self.key.lock() = Some(key.to_owned());
        self
    }

    /// 최신 DB 상태가 이 소스가 만들어진 때와 같은지 본다. 상태 잠금 밖에서만 호출한다.
    pub(super) fn validate(&self) -> Result<(), ApiError> {
        let unavailable = || ApiError::new("AUTH_REFRESH_UNAVAILABLE", "계정 상태를 확인하지 못했어요.");
        let service = self.owner.upgrade().ok_or_else(unavailable)?;
        let current = {
            let store = service.lock()?;
            crate::store::account(&store.connection, &self.account_id)
        };
        let current = match current {
            Ok(account) => account,
            Err(error) if error.code == "ACCOUNT_NOT_FOUND" => {
                return Err(ApiError::new("ACCOUNT_DISABLED", "이 계정은 더 이상 등록되어 있지 않아요."))
            }
            Err(_) => return Err(unavailable()),
        };
        if !current.enabled {
            return Err(ApiError::new("ACCOUNT_DISABLED", "이 계정의 배정이 꺼져 있어요."));
        }
        if current.auth_status == "auth-required" {
            return Err(ApiError::new("AUTH_REQUIRED", "사용할 수 있는 API 키가 없어요."));
        }
        if current.auth_status != "authenticated" {
            return Err(unavailable());
        }
        if !self.provider.owns(&current) || current.identity_key.as_deref() != Some(self.identity.as_str()) {
            return Err(ApiError::new("PROFILE_IDENTITY_MISMATCH", "등록된 API 키가 바뀌었어요."));
        }
        // 키를 내주기 직전에도 서비스 환경의 인증·엔드포인트 재정의를 다시 본다(고치지 않고 멈춘다).
        aam_adapters::check_api_credit_env(self.provider)?;
        Ok(())
    }

    /// Keychain의 키를 읽어 등록된 지문과 맞는지 확인한다. 한 번 읽으면 메모리에 둔다.
    fn key(&self) -> Result<String, ApiError> {
        if let Some(key) = self.key.lock().clone() {
            return Ok(key);
        }
        let key = api_credit::load_key(&self.account_id)?
            .ok_or_else(|| ApiError::new("AUTH_REQUIRED", "시스템 보안 저장소에 API 키가 없어요."))?;
        if api_credit::identity_key(self.provider, &key) != self.identity {
            return Err(ApiError::new("PROFILE_IDENTITY_MISMATCH", "시스템 보안 저장소의 API 키가 등록된 키와 달라요."));
        }
        *self.key.lock() = Some(key.clone());
        Ok(key)
    }

    pub(super) fn ready(&self) -> Result<(), ApiError> {
        self.key().map(|_| ())
    }

    /// omp 자격 증명 항목. `provider`는 omp의 공급자 ID이고, API 키는 `identityKey: null`이라 계정 풀 필터(OAuth 전용)를 받지 않는다.
    /// OpenAI 키는 `openai-codex`(ChatGPT 구독 백엔드)가 아니라 `openai`(공식 API) 공급자에 붙는다.
    fn entry(&self, key: &str) -> Value {
        json!({ "id": 1, "provider": self.provider.id(), "identityKey": null,
            "credential": { "type": "api_key", "key": key }, "rotatesInMs": null, "blocks": [] })
    }

    pub(super) fn handle(&self, client: &mut TcpStream, request: &Request, path: &str, deadline: Instant) {
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        match (request.method.as_str(), route) {
            ("GET", "/v1/healthz") => respond(client, 200, &json!({"ok":true})),
            // omp는 stream 404에서 long-poll로 물러난다.
            (_, "/v1/snapshot/stream") => respond(client, 404, &json!({"error":"Snapshot streaming is not supported."})),
            ("GET", "/v1/snapshot") => self.snapshot(client, request, query, deadline),
            // 키는 갱신되지 않는다. 같은 항목을 돌려준다.
            ("POST", "/v1/credential/1/refresh") => match self.key() {
                Ok(key) => respond(client, 200, &json!({"entry": self.entry(&key)})),
                Err(error) => auth_error(client, error),
            },
            ("POST", "/v1/credential/1/disable") => {
                // omp가 키 거절(401)을 알려 줄 때만 계정을 `auth-required`로 표시한다. 그 밖의 사유는 무시한다.
                let cause = serde_json::from_slice::<Value>(&request.body)
                    .ok()
                    .and_then(|body| body["cause"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                if rejection_text(self.provider, &cause) {
                    if let Some(service) = self.owner.upgrade() {
                        mark_auth_required(&service, &self.account_id);
                    }
                    *self.key.lock() = None;
                }
                respond(client, 200, &json!({"ok":true}));
            }
            ("POST", "/v1/credential/1/block") | ("DELETE", "/v1/credential/1/block") | ("DELETE", "/v1/credential/1/blocks") => {
                respond(client, 200, &json!({"ok":true}))
            }
            _ => respond(client, 404, &json!({"error":"This API key source does not support that broker operation."})),
        }
    }

    fn snapshot(&self, client: &mut TcpStream, request: &Request, query: &str, deadline: Instant) {
        let wait = query
            .split('&')
            .find_map(|value| value.strip_prefix("wait="))
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            .min(25_000);
        let previous = request.header("if-none-match").and_then(|value| value.trim_matches('"').parse::<i64>().ok());
        let poll_deadline = (Instant::now() + Duration::from_millis(wait)).min(deadline);
        loop {
            let left = poll_deadline.saturating_duration_since(Instant::now());
            if previous != Some(GENERATION) || left.is_zero() {
                // 계정이 그 사이 꺼졌거나 키를 거절당했으면 키를 내주지 않는다.
                if let Err(error) = self.validate() {
                    return auth_error(client, error);
                }
                let key = match self.key() {
                    Ok(key) => key,
                    Err(error) => return auth_error(client, error),
                };
                let now = now_ms();
                let text = json!({"generation":GENERATION,"generatedAt":now,"serverNowMs":now,
                    "refresher":{"enabled":false,"intervalMs":60_000,"skewMs":0,"nextSweepInMs":0},
                    "credentials":[self.entry(&key)]}).to_string();
                let _ = write!(client, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nETag: \"{}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", GENERATION, text.len(), text);
                finish(client);
                return;
            }
            std::thread::sleep(left.min(POLL_SLICE));
            if super::native::client_gone(client) {
                return;
            }
        }
    }
}

/// 응답 스트림에서 마지막 종료 이벤트(`done`/`error`)의 `usage`를 읽는다. HTTP chunked 틀과 SSE 줄 경계가 읽기 경계와
/// 어긋나도 된다. 종료 이벤트가 아닌 줄은 보관하지 않고, 종료 이벤트 줄도 뒤쪽 `TAIL_CAP` 바이트만 둔다.
pub(super) struct UsageScanner {
    chunked: bool,
    stage: Stage,
    size: usize,
    extension: bool,
    head: Vec<u8>,
    tail: Vec<u8>,
    /// 현재 줄이 종료 이벤트인지. 머리를 다 읽기 전에는 `None`.
    keep: Option<bool>,
    found: Option<Usage>,
}

#[derive(Clone, Copy)]
enum Stage {
    Size,
    Data(usize),
    DataEnd,
    Done,
}

impl UsageScanner {
    /// `chunked`: 응답 머리에 `Transfer-Encoding: chunked`가 있었는지.
    pub fn new(chunked: bool) -> Self {
        Self {
            chunked,
            stage: Stage::Size,
            size: 0,
            extension: false,
            head: Vec::with_capacity(HEAD_LEN),
            tail: Vec::new(),
            keep: None,
            found: None,
        }
    }

    pub fn is_chunked(head: &[u8]) -> bool {
        String::from_utf8_lossy(head).to_ascii_lowercase().contains("transfer-encoding: chunked")
    }

    pub fn feed(&mut self, mut bytes: &[u8]) {
        if !self.chunked {
            return self.lines(bytes);
        }
        while !bytes.is_empty() {
            match self.stage {
                Stage::Size => {
                    let byte = bytes[0];
                    bytes = &bytes[1..];
                    match byte {
                        b'\n' => {
                            self.stage = if self.size == 0 { Stage::Done } else { Stage::Data(self.size) };
                            self.size = 0;
                            self.extension = false;
                        }
                        b'\r' => {}
                        b';' => self.extension = true,
                        _ if self.extension => {}
                        _ => match (byte as char).to_digit(16) {
                            Some(digit) => self.size = self.size.saturating_mul(16).saturating_add(digit as usize),
                            // 틀이 깨졌다. 더는 해석하지 않는다(이미 읽은 값은 유지).
                            None => self.stage = Stage::Done,
                        },
                    }
                }
                Stage::Data(left) => {
                    let take = left.min(bytes.len());
                    self.lines(&bytes[..take]);
                    bytes = &bytes[take..];
                    self.stage = if take == left { Stage::DataEnd } else { Stage::Data(left - take) };
                }
                Stage::DataEnd => {
                    let byte = bytes[0];
                    bytes = &bytes[1..];
                    if byte == b'\n' {
                        self.stage = Stage::Size;
                    }
                }
                Stage::Done => return,
            }
        }
    }

    fn lines(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match bytes.iter().position(|byte| *byte == b'\n') {
                Some(index) => {
                    self.push(&bytes[..index]);
                    self.end_line();
                    bytes = &bytes[index + 1..];
                }
                None => return self.push(bytes),
            }
        }
    }

    fn push(&mut self, segment: &[u8]) {
        if self.keep == Some(false) {
            return;
        }
        if self.head.len() < HEAD_LEN {
            let take = (HEAD_LEN - self.head.len()).min(segment.len());
            self.head.extend_from_slice(&segment[..take]);
        }
        if self.keep.is_none() && self.head.len() >= HEAD_LEN {
            let keep = terminal_event(&self.head);
            self.keep = Some(keep);
            if !keep {
                self.tail.clear();
                return;
            }
        }
        self.tail.extend_from_slice(segment);
        if self.tail.len() > 2 * TAIL_CAP {
            let cut = self.tail.len() - TAIL_CAP;
            self.tail.drain(..cut);
        }
    }

    fn end_line(&mut self) {
        if self.keep.unwrap_or_else(|| terminal_event(&self.head)) {
            if let Some(usage) = parse_usage(&self.tail) {
                self.found = Some(usage);
            }
        }
        self.head.clear();
        self.tail.clear();
        self.keep = None;
    }

    /// 스트림이 끝난 뒤 마지막 종료 이벤트의 usage. 줄바꿈 없이 끝난 마지막 줄도 본다.
    pub fn finish(mut self) -> Option<Usage> {
        if !self.head.is_empty() {
            self.end_line();
        }
        self.found
    }
}

/// `data: {"type":"done"` 또는 `data: {"type":"error"`로 시작하는 줄.
fn terminal_event(head: &[u8]) -> bool {
    let Some(rest) = head.strip_prefix(b"data:") else { return false };
    let skip = rest.iter().take_while(|byte| **byte == b' ').count();
    let rest = &rest[skip..];
    rest.starts_with(b"{\"type\":\"done\"") || rest.starts_with(b"{\"type\":\"error\"")
}

/// 줄 뒤쪽에서 마지막 `"usage":{…}` 객체를 읽는다. `usage`는 본문 뒤에 있고 그 안에 `usage` 키가 또 들어 있지 않다.
fn parse_usage(tail: &[u8]) -> Option<Usage> {
    const NEEDLE: &[u8] = b"\"usage\":{";
    let at = tail.windows(NEEDLE.len()).rposition(|window| window == NEEDLE)?;
    let object = &tail[at + NEEDLE.len() - 1..];
    let (mut depth, mut in_string, mut escaped, mut end) = (0usize, false, false, None);
    for (index, byte) in object.iter().enumerate() {
        if in_string {
            match (escaped, *byte) {
                (true, _) => escaped = false,
                (false, b'\\') => escaped = true,
                (false, b'"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    end = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let value: Value = serde_json::from_slice(&object[..=end?]).ok()?;
    let count = |name: &str| value.get(name).and_then(Value::as_f64).filter(|number| number.is_finite() && *number >= 0.0).map(|number| number as u64);
    Some(Usage {
        input: count("input")?,
        output: count("output")?,
        cache_read: count("cacheRead").unwrap_or(0),
        cache_write: count("cacheWrite").unwrap_or(0),
    })
}

/// 응답을 그대로 넘기면서 usage만 읽는다. `peeked`는 이미 읽어 둔 첫 이벤트 바이트다(호출자가 클라이언트에 먼저 썼다).
/// 클라이언트가 떠나면 upstream 읽기를 멈춘다(gateway가 생성을 취소한다).
pub(super) fn copy_metered(upstream: &mut TcpStream, client: &mut TcpStream, mut scanner: UsageScanner) -> Option<Usage> {
    let mut buffer = [0u8; 16 * 1024];
    loop {
        match upstream.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                scanner.feed(&buffer[..read]);
                if client.write_all(&buffer[..read]).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    scanner.finish()
}
