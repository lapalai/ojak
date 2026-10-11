//! Minimal access-only broker for one registered native CLI profile per gateway.
//! Refresh tokens never leave the official CLI store. Unsupported broker operations
//! return 404 rather than claiming to have persisted data.
use super::*;
use aam_adapters::NativeAccess;

pub(super) struct Source {
    pub account: Account,
    pub provider: &'static str,
    pub identity: String,
    state: Mutex<SourceState>,
    /// 상태(`generation`)가 바뀌면 깨운다. long-poll이 바쁘게 다시 확인하지 않고 변경·만료·마감까지 잠든다.
    changed: std::sync::Condvar,
}
#[derive(Default)]
struct SourceState {
    access: Option<NativeAccess>,
    checked_at: i64,
    generation: i64,
    disabled: Option<[u8; 32]>,
    blocks: Vec<Value>,
}
impl SourceState {
    fn rejected(&self) -> bool {
        self.disabled.is_some_and(|disabled| {
            self.access.as_ref().is_some_and(|access| {
                let fingerprint: [u8; 32] = Sha256::digest(access.access.as_bytes()).into();
                disabled == fingerprint
            })
        })
    }
}

pub(super) fn identity(account: &Account) -> Option<(&'static str, String)> {
    let provider = match account.tool.as_str() {
        "claude" => "anthropic",
        "codex" => "openai-codex",
        _ => return None,
    };
    if !account.enabled || account.auth_status != "authenticated" {
        return None;
    }
    let key = account.identity_key.as_deref()?;
    let email = account
        .email
        .as_deref()
        .filter(|s| !s.is_empty())?
        .to_lowercase();
    let org = key
        .split('|')
        .find_map(|part| part.strip_prefix("workspace:"))
        .filter(|s| !s.is_empty())?;
    Some((
        provider,
        format!("email:{email}|org:{}", org.to_lowercase()),
    ))
}

impl Source {
    pub fn new(account: Account, provider: &'static str, identity: String) -> Self {
        Self {
            account,
            provider,
            identity,
            state: Mutex::new(SourceState::default()),
            changed: std::sync::Condvar::new(),
        }
    }
    fn state(&self) -> MutexGuard<'_, SourceState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn load(&self, state: &mut SourceState, force: bool) -> Result<(), ApiError> {
        let now = now_ms();
        if !force
            && now - state.checked_at < 30_000
            && state
                .access
                .as_ref()
                .is_some_and(|a| a.expires > now + 60_000)
        {
            return Ok(());
        }
        // The per-profile mutex serializes broker callers; the native CLI owns
        // its cross-process refresh lock and canonical credential writes.
        let access = match aam_adapters::native_access(&self.account, force) {
            Ok(access) => access,
            Err(error) => {
                state.access = None;
                state.checked_at = 0;
                state.generation += 1;
                return Err(error);
            }
        };
        if state
            .access
            .as_ref()
            .is_none_or(|old| old.access != access.access || old.expires != access.expires)
        {
            state.generation += 1;
        }
        state.checked_at = now_ms();
        state.access = Some(access);
        Ok(())
    }
    pub fn ready(&self) -> Result<(), ApiError> {
        let mut state = self.state();
        self.load(&mut state, false)?;
        if state.rejected() {
            return Err(ApiError::new(
                "AUTH_REQUIRED",
                "공급자가 기존 인증을 거절했어요.",
            ));
        }
        Ok(())
    }
    fn entry(&self, state: &SourceState) -> Option<Value> {
        let access = state.access.as_ref()?;
        if state.rejected() {
            return None;
        }
        let mut credential = json!({
            "type": "oauth", "access": access.access, "refresh": "__remote__", "expires": access.expires,
        });
        if let Some(email) = &self.account.email {
            credential["email"] = json!(email);
        }
        if let Some(id) = &access.account_id {
            credential["accountId"] = json!(id);
        }
        if let Some(org) = self
            .identity
            .split('|')
            .find_map(|s| s.strip_prefix("org:"))
        {
            credential["orgId"] = json!(org);
        }
        Some(
            json!({ "id": 1, "provider": self.provider, "identityKey": self.identity, "credential": credential }),
        )
    }
    pub fn handle(&self, client: &mut TcpStream, request: &Request, path: &str) {
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        if request.method == "GET" && route == "/v1/healthz" {
            return respond(client, 200, &json!({"ok":true}));
        }
        // omp 18.8.3 explicitly falls back to long polling on a stream 404.
        if route == "/v1/snapshot/stream" {
            return respond(
                client,
                404,
                &json!({"error":"Snapshot streaming is not supported."}),
            );
        }
        if request.method == "GET" && route == "/v1/snapshot" {
            let wait = query
                .split('&')
                .find_map(|v| v.strip_prefix("wait="))
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0)
                .min(25_000);
            let previous = request
                .header("if-none-match")
                .and_then(|v| v.trim_matches('"').parse::<i64>().ok());
            let started = std::time::Instant::now();
            let deadline = started + Duration::from_millis(wait);
            let mut state = self.state();
            loop {
                if let Err(error) = self.load(&mut state, false) {
                    return auth_error(client, error);
                }
                let now = now_ms();
                let count = state.blocks.len();
                state
                    .blocks
                    .retain(|b| b["blockedUntilMs"].as_i64().is_some_and(|t| t > now));
                if state.blocks.len() != count {
                    state.generation += 1;
                }
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                if previous != Some(state.generation) || left.is_zero() {
                    let entries: Vec<Value> = self
                        .entry(&state)
                        .into_iter()
                        .map(|mut entry| {
                            entry["rotatesInMs"] =
                                json!(state.access.as_ref().map(|a| (a.expires - now).max(0)));
                            entry["blocks"] = json!(state.blocks);
                            entry
                        })
                        .collect();
                    let generation = state.generation;
                    let body = json!({"generation":generation,"generatedAt":now,"serverNowMs":now,
                        "refresher":{"enabled":false,"intervalMs":60_000,"skewMs":0,"nextSweepInMs":0},"credentials":entries});
                    drop(state);
                    let text = body.to_string();
                    let _ = write!(client,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\nETag: \"{}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",generation,text.len(),text);
                    let _ = client.shutdown(Shutdown::Both);
                    return;
                }
                // 바뀔 때까지 잠근 채 기다리지 않고 잠금을 풀고 잔다. 깨우는 시점: 상태 변경, 가장 이른 차단 만료,
                // 접근 토큰 재확인 시점(30초), 요청 마감 중 가장 이른 것.
                let next_block = state.blocks.iter().filter_map(|b| b["blockedUntilMs"].as_i64()).min()
                    .map(|t| Duration::from_millis((t - now).max(1) as u64));
                let recheck = Duration::from_millis((30_000 - (now - state.checked_at)).clamp(1, 30_000) as u64);
                let sleep = [Some(left), next_block, Some(recheck)].into_iter().flatten().min().unwrap_or(left);
                state = self.changed.wait_timeout(state, sleep).map(|(guard, _)| guard).unwrap_or_else(|e| e.into_inner().0);
            }
        }
        // 선언 순서의 반대로 해제되므로 `_wake`가 `state` 잠금보다 나중에 해제된다(잠금을 다시 잡아도 교착이 없다).
        let before = self.state().generation;
        let _wake = WakeOnChange { source: self, before };
        let mut state = self.state();
        match (request.method.as_str(), route) {
            ("POST", "/v1/credential/1/refresh") => {
                if let Err(error) = self.load(&mut state, true) {
                    return auth_error(client, error);
                }
                match self.entry(&state) {
                    Some(entry) => respond(client, 200, &json!({"entry":entry})),
                    None => respond(
                        client,
                        401,
                        &json!({"error":"Native credential was rejected; sign in through its official CLI."}),
                    ),
                }
            }
            ("POST", "/v1/credential/1/disable") => {
                // Disable only this rejected access token in the bridge. Do not
                // revoke or delete the user's canonical CLI login.
                if let Some(access) = &state.access {
                    state.disabled = Some(Sha256::digest(access.access.as_bytes()).into());
                    state.generation += 1;
                }
                respond(client, 200, &json!({"ok":true}));
            }
            ("POST", "/v1/credential/1/block") => {
                let Ok(value) = serde_json::from_slice::<Value>(&request.body) else {
                    return respond(client, 400, &json!({"error":"Invalid block."}));
                };
                let Some(provider) = value["providerKey"]
                    .as_str()
                    .filter(|v| *v == self.provider)
                else {
                    return respond(client, 400, &json!({"error":"Invalid block provider."}));
                };
                let Some(scope) = value["blockScope"].as_str() else {
                    return respond(client, 400, &json!({"error":"Invalid block scope."}));
                };
                let Some(until) = value["blockedUntilMs"].as_i64() else {
                    return respond(client, 400, &json!({"error":"Invalid block expiry."}));
                };
                state
                    .blocks
                    .retain(|b| b["providerKey"] != provider || b["blockScope"] != scope);
                state.blocks.push(json!({"providerKey":provider,"blockScope":scope,"blockedUntilMs":until,"updatedAtMs":now_ms()}));
                state.generation += 1;
                respond(client, 200, &json!({"ok":true}));
            }
            ("DELETE", "/v1/credential/1/block") => {
                let Ok(value) = serde_json::from_slice::<Value>(&request.body) else {
                    return respond(client, 400, &json!({"error":"Invalid block."}));
                };
                if value["providerKey"].as_str() != Some(self.provider)
                    || !value["blockScope"].is_string()
                {
                    return respond(client, 400, &json!({"error":"Invalid block selector."}));
                }
                state.blocks.retain(|b| {
                    b["providerKey"] != value["providerKey"]
                        || b["blockScope"] != value["blockScope"]
                });
                state.generation += 1;
                respond(client, 200, &json!({"ok":true}));
            }
            ("DELETE", "/v1/credential/1/blocks") => {
                state.blocks.clear();
                state.generation += 1;
                respond(client, 200, &json!({"ok":true}));
            }
            _ => respond(
                client,
                404,
                &json!({"error":"This native credential source does not support that broker operation."}),
            ),
        }
    }
}

/// 변경 요청이 끝나면(잠금이 풀린 뒤) `generation`이 바뀌었는지 보고 long-poll을 깨운다.
struct WakeOnChange<'a> {
    source: &'a Source,
    before: i64,
}
impl Drop for WakeOnChange<'_> {
    fn drop(&mut self) {
        if self.source.state().generation != self.before {
            self.source.changed.notify_all();
        }
    }
}
fn auth_error(client: &mut TcpStream, error: ApiError) {
    // Never forward native CLI stderr or credential payloads.
    let (status, message) = failure(&error.code);
    respond(client, status, &json!({"code":error.code,"error":message}));
}

pub(super) fn failure(code: &str) -> (u16, &'static str) {
    match code {
        "AUTH_REQUIRED" => (401, "공식 CLI의 로그인이 유효하지 않아요. Ojak → 연결에서 해당 계정에 다시 로그인해 주세요. omp에 별도로 로그인할 필요는 없어요."),
        "ACCOUNT_DISABLED" => (409, "계정이 배정에서 제외되어 있어요. Ojak → 사용 현황 → 계정 고르기에서 해당 계정을 포함해 주세요. 다시 로그인할 필요는 없어요."),
        "PROFILE_IDENTITY_MISMATCH" => (409, "공식 CLI의 로그인 계정이 바뀌었어요. Ojak에서 계정을 새로고침한 뒤 확인해 주세요."),
        "AUTH_OVERRIDE_CONFLICT" => (409, "API 키·다른 공급자·인증 도우미 설정이 구독 로그인과 충돌해 연결을 멈췄어요. Ojak의 계정 오류 상세에서 해당 설정을 확인해 주세요."),
        "CLI_NOT_FOUND" => (503, "등록된 공식 CLI를 찾지 못했어요. Ojak → 연결에서 원본 CLI 설치 상태를 확인해 주세요."),
        "NATIVE_KEYCHAIN_LOCKED" => (503, "Claude 키체인에 접근하지 못했어요. macOS 키체인 잠금과 접근 권한을 확인해 주세요. 다시 로그인할 필요는 없어요."),
        "NATIVE_STORAGE_UNSUPPORTED" => (409, "이 Codex 프로필의 인증 저장소는 직접 연결할 수 없어요. 저장소 설정은 바꾸지 않았어요. Ojak → 연결에서 이 계정의 저장소 설정을 확인해 주세요."),
        "NATIVE_CREDENTIAL_READ" => (503, "공식 CLI의 인증 파일을 읽지 못했어요. Ojak → 연결에서 원래 프로필 경로와 파일 접근 권한을 확인해 주세요."),
        "PROFILE_UNVERIFIED" => (409, "공식 CLI의 계정 또는 워크스페이스를 확인하지 못했어요. Ojak → 연결에서 해당 계정을 새로고침해 주세요."),
        _ => (503, "기존 공식 CLI 로그인을 읽거나 갱신하지 못했어요. Ojak → 연결에서 해당 계정 상태를 확인해 주세요. 이 오류만으로 omp에 다시 로그인할 필요는 없어요."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_identity_keeps_workspaces_separate_and_requires_verified_login() {
        let mut account = Account {
            tool: "claude".into(),
            enabled: true,
            auth_status: "authenticated".into(),
            email: Some("A@example.test".into()),
            identity_key: Some("anthropic|email:a@example.test|workspace:org-a".into()),
            ..Account::default()
        };
        assert_eq!(
            identity(&account),
            Some(("anthropic", "email:a@example.test|org:org-a".into()))
        );
        account.identity_key = Some("anthropic|email:a@example.test|workspace:org-b".into());
        assert_eq!(
            identity(&account),
            Some(("anthropic", "email:a@example.test|org:org-b".into()))
        );
        account.enabled = false;
        assert!(identity(&account).is_none());
        account.enabled = true;
        account.auth_status = "auth-required".into();
        assert!(identity(&account).is_none());
    }
    #[test]
    fn rejected_access_stays_disabled_until_native_token_changes() {
        let source = Source::new(
            Account::default(),
            "anthropic",
            "email:a@example.test|org:a".into(),
        );
        let mut state = SourceState {
            access: Some(NativeAccess {
                access: "old".into(),
                expires: i64::MAX,
                account_id: None,
            }),
            ..SourceState::default()
        };
        assert!(source.entry(&state).is_some());
        state.disabled = Some(Sha256::digest(b"old").into());
        assert!(source.entry(&state).is_none());
        state.access.as_mut().unwrap().access = "new".into();
        let entry = source.entry(&state).unwrap();
        assert_eq!(entry["credential"]["refresh"], "__remote__");
        assert_eq!(entry["credential"]["access"], "new");
    }

    /// omp gateways re-request `/v1/snapshot?wait=…` with the generation they already have. If the server answers
    /// at once, every gateway spins and the service burns a CPU core. The poll must hold until the deadline,
    /// and a block added meanwhile must wake it promptly.
    #[test]
    fn snapshot_long_poll_waits_for_its_deadline_and_wakes_on_change() {
        use std::net::TcpListener;
        let source = Arc::new(Source::new(Account::default(), "anthropic", "email:a@example.test|org:a".into()));
        {
            let mut state = source.state();
            // A fresh token keeps `load` from calling the native CLI during the test.
            state.access = Some(NativeAccess { access: "token".into(), expires: i64::MAX, account_id: None });
            state.checked_at = now_ms();
            state.generation = 7;
        }
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = Arc::clone(&source);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let source = Arc::clone(&server);
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let (request, _) = super::super::read_head(&mut stream).unwrap();
                    let route = if request.query.is_empty() { request.path.clone() } else { format!("{}?{}", request.path, request.query) };
                    let body_request = Request { body: Vec::new(), ..request };
                    source.handle(&mut stream, &body_request, &route);
                });
            }
        });
        let poll = |wait_ms: u64| {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(stream, "GET /v1/snapshot?wait={wait_ms} HTTP/1.1\r\nHost: x\r\nIf-None-Match: \"7\"\r\n\r\n").unwrap();
            let started = std::time::Instant::now();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            (started.elapsed(), response)
        };
        let (elapsed, response) = poll(400);
        assert!(elapsed >= Duration::from_millis(350), "answered after {elapsed:?} instead of waiting");
        assert!(response.contains("ETag: \"7\""));

        let waker = Arc::clone(&source);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let before = waker.state().generation;
            let _wake = WakeOnChange { source: &waker, before };
            waker.state().generation += 1;
        });
        let (elapsed, response) = poll(5_000);
        assert!(elapsed < Duration::from_secs(2), "change did not wake the poll ({elapsed:?})");
        assert!(response.contains("ETag: \"8\""));
    }
}
