//! Console API 키(Claude · OpenAI) 계정의 등록·사용액 기록 RPC.
//! 키 값은 Keychain에만 있고 계정 행·오류·로그에 넣지 않는다. 오류 문장에도 응답 본문이나 키를 싣지 않는다.
use super::*;
use aam_adapters::api_credit::{self, ApiProvider, Usage, API_TOOL};

const MODELS_URL_PATH: &str = "/v1/models";

/// 키 확인 서버. 키는 자기 공급자의 서버로만 보낸다.
fn default_api_base(provider: ApiProvider) -> &'static str {
    match provider {
        ApiProvider::Anthropic => "https://api.anthropic.com",
        ApiProvider::OpenAi => "https://api.openai.com",
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ApiKeyAdd {
    label: String,
    key: String,
    confirmed_no_auto_reload: bool,
    /// 키의 공급자(`anthropic`·`openai`). 없으면 `anthropic`(공급자 선택이 없던 이전 클라이언트).
    #[serde(default)]
    provider: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct UsageParams {
    #[serde(default)]
    input: u64,
    #[serde(default)]
    output: u64,
    #[serde(default)]
    cache_read: u64,
    #[serde(default)]
    cache_write: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ApiCreditRecord {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    capability: Option<String>,
    account_id: String,
    model: String,
    #[serde(default)]
    usage: Option<UsageParams>,
}

fn check_failed() -> ApiError {
    ApiError::new(
        "API_KEY_CHECK_FAILED",
        "API 키를 확인하지 못했어요. 네트워크 연결을 확인하고 다시 시도해 주세요. 입력한 값은 보여 주지 않았어요.",
    )
}

/// `GET /v1/models`(과금 없음)로 키 유효성만 확인한다. 응답 본문은 읽지 않는다. 키는 자기 공급자 서버로만 보내고 리디렉션은 따르지 않는다.
/// Anthropic은 `x-api-key`, OpenAI는 `Authorization: Bearer`다.
pub(crate) fn verify_key(provider: ApiProvider, base: &str, key: &str) -> Result<(), ApiError> {
    let builder = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none());
    #[cfg(test)]
    let builder = builder.no_proxy();
    let client = builder
        .build()
        .map_err(|_| check_failed())?;
    let request = client.get(format!("{base}{MODELS_URL_PATH}"));
    let request = match provider {
        ApiProvider::Anthropic => request.header("x-api-key", key).header("anthropic-version", "2023-06-01"),
        ApiProvider::OpenAi => request.header("Authorization", format!("Bearer {key}")),
    };
    let response = request.send().map_err(|_| check_failed())?;
    match response.status().as_u16() {
        200..=299 => Ok(()),
        401 | 403 => Err(ApiError::new(
            "API_KEY_INVALID",
            match provider {
                ApiProvider::Anthropic => "이 API 키는 Anthropic이 받아 주지 않았어요. 키가 맞는지, 폐기되지 않았는지 확인해 주세요. 입력한 값은 보여 주지 않았어요.",
                ApiProvider::OpenAi => "이 API 키는 OpenAI가 받아 주지 않았어요. 키가 맞는지, 폐기되지 않았는지 확인해 주세요. 입력한 값은 보여 주지 않았어요.",
            },
        )),
        _ => Err(check_failed()),
    }
}

impl Service {
    fn api_base(&self, provider: ApiProvider) -> String {
        #[cfg(test)]
        if let Some(base) = self.api_base_override.lock().get(provider.id()).cloned() {
            return base;
        }
        default_api_base(provider).to_owned()
    }

    #[cfg(test)]
    pub(crate) fn set_api_base_for_test(&self, provider: ApiProvider, base: &str) {
        self.api_base_override.lock().insert(provider.id().to_owned(), base.to_owned());
    }

    pub(crate) fn api_key_add(&self, request: ApiKeyAdd) -> Result<Account, ApiError> {
        let provider = match request.provider.as_deref() {
            None => ApiProvider::Anthropic,
            Some(name) => ApiProvider::of(name).ok_or_else(|| ApiError::new("INVALID_PARAMS", "API 키 공급자는 anthropic 또는 openai여야 해요."))?,
        };
        if !request.confirmed_no_auto_reload {
            return Err(ApiError::new(
                "API_CREDIT_CONFIRM_REQUIRED",
                match provider {
                    ApiProvider::Anthropic => "이 키의 Console 조직에서 auto-reload를 끄고 크레딧을 따로 사지 않았는지 확인해 주세요.",
                    ApiProvider::OpenAi => "이 키의 OpenAI Platform 조직에서 자동 충전(auto recharge)을 끄고, OpenAI API 사용이 실제 요금으로 청구된다는 점을 확인해 주세요.",
                },
            ));
        }
        if request.label.is_empty()
            || request.label.chars().count() > 120
            || request.label.chars().any(char::is_control)
        {
            return Err(ApiError::new(
                "INVALID_PARAMS",
                "계정 이름은 제어 문자 없이 1~120자로 입력하세요.",
            ));
        }
        api_credit::validate_key_format(provider, &request.key)?;
        // 키 추가는 한 번에 하나씩만 한다. 지웠던 키를 다시 등록하면 같은 계정 id를 이어 쓰므로(아래), 같은 키를 동시에 추가하는 두 요청이
        // 같은 id로 Keychain 항목을 쓰고 진 쪽이 이긴 쪽의 키를 지우는 일이 없어야 한다. 이 잠금은 검증 요청·Keychain 쓰기 동안 쥐지만
        // 다른 서비스 작업(DB 잠금)은 막지 않는다. 키 추가는 사용자가 드물게 하는 조작이다.
        let _one_add_at_a_time = self.api_key_add_gate.lock();
        let identity = api_credit::identity_key(provider, &request.key);
        let duplicate = || {
            ApiError::new(
                "ACCOUNT_ALREADY_CONNECTED",
                "이미 등록한 API 키예요.",
            )
        };
        // 지웠던 키를 다시 등록하면 같은 계정 id를 이어 쓴다: 이번 달 사용액(원장)과 풀 소속이 그대로 따라온다(키별 월 상한 우회 방지).
        let reused_id = {
            let store = self.lock()?;
            if accounts(&store.connection)?
                .iter()
                .any(|account| provider.owns(account) && account.identity_key.as_deref() == Some(identity.as_str()))
            {
                return Err(duplicate());
            }
            store::reusable_api_key_id(&store.connection, &identity)?
        };
        // 네트워크 확인에는 DB 잠금을 쥐지 않는다.
        verify_key(provider, &self.api_base(provider), &request.key)?;
        let now = now_ms();
        let account = Account {
            id: reused_id.unwrap_or_else(new_id),
            provider: provider.id().into(),
            tool: API_TOOL.into(),
            label: request.label,
            identity_key: Some(identity.clone()),
            auth_status: "authenticated".into(),
            verification: "api-key".into(),
            can_launch: true,
            enabled: true,
            max_concurrency: 0,
            last_checked_at: now,
            api_credit: Some(ApiCreditState { period_start: api_credit::period_start(now), updated_at: now, ..Default::default() }),
            ..Default::default()
        };
        // Keychain 쓰기(사용자 승인 창이 뜰 수 있음)는 서비스 잠금·DB 트랜잭션 밖에서 한다. 새 id는 계정이 커밋되기 전에는 아무도 찾지 않고,
        // 이어 쓰는 id는 지울 때 Keychain 항목도 지웠으며 같은 키 추가는 위 잠금으로 직렬화되어 있어 이 id를 가진 계정이 아직 없다.
        // 키 저장이 실패하면 계정을 만들지 않고, 계정 저장이 실패하면 방금 넣은 키를 지운다(그 id의 계정이 이미 커밋돼 있으면 그 키는 지우지 않는다).
        api_credit::store_key(&account.id, &request.key)?;
        let saved = (|| {
            let mut store = self.lock()?;
            let tx = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(db_error)?;
            admission_allowed(&tx)?;
            if accounts(&tx)?
                .iter()
                .any(|existing| provider.owns(existing) && existing.identity_key.as_deref() == Some(identity.as_str()))
            {
                return Err(duplicate());
            }
            save_account(&tx, &account)?;
            // 이어 받은 사용액(원장)이 있으면 그 값을 돌려준다. 새 키는 원장 행이 아직 없어 만들어 둔 0 상태를 그대로 쓴다.
            let mut stored = store::account(&tx, &account.id)?;
            stored.api_credit = stored.api_credit.or_else(|| account.api_credit.clone());
            tx.commit().map_err(db_error)?;
            Ok(stored)
        })();
        match saved {
            Ok(stored) => Ok(stored),
            Err(error) => {
                let absent = self
                    .lock()
                    .is_ok_and(|store| store::account(&store.connection, &account.id).is_err_and(|error| error.code == "ACCOUNT_NOT_FOUND"));
                if absent {
                    let _ = api_credit::delete_key(&account.id);
                }
                Err(error)
            }
        }
    }

    pub(crate) fn api_credit_record(&self, request: ApiCreditRecord) -> Result<Value, ApiError> {
        if !safe_id(&request.model) {
            return Err(ApiError::new("INVALID_PARAMS", "모델 이름이 올바르지 않아요."));
        }
        let mut store = self.lock()?;
        // 이미 지운 키(원장에 행이 있는 ID)의 늦은 기록도 받는다. 키를 지워도 이번 달 사용액은 합계에 남아야 한다.
        // 단가는 키의 공급자 기준이다(원장이 지운 키의 공급자를 기억한다).
        let (target_id, provider) = match account(&store.connection, &request.account_id) {
            Ok(target) if target.tool != API_TOOL => return Err(ApiError::new("INVALID_PARAMS", "API 키 계정이 아니에요.")),
            Ok(target) => (target.id, target.provider),
            Err(error) if error.code == "ACCOUNT_NOT_FOUND" => match ledger_provider(&store.connection, &request.account_id)? {
                Some(provider) => (request.account_id.clone(), provider),
                None => return Err(error),
            },
            Err(error) => return Err(error),
        };
        // 세션이 있으면 입장 때 캡처한 풀에 쌓는다(실행 중 풀 이동과 무관). 세션 없는 기록은 지금 그 키가 속한 풀이다.
        let pool = if let Some(session_id) = &request.session_id {
            let record = lease(&store.connection, session_id)?;
            authorize(&record, request.capability.as_deref().unwrap_or(""))?;
            if record.session.account_id != target_id {
                return Err(ApiError::new("INVALID_PARAMS", "이 대화가 쓴 API 키 계정이 아니에요."));
            }
            record.session.api_credit_pool
        } else {
            crate::scheduler::pool_of(&policy(&store.connection)?, &target_id).map(|grant| grant.id.clone())
        };
        // 공급자를 모르는 계정이면 단가를 구하지 않는다(미확인 요청으로 센다).
        let cost = ApiProvider::of(&provider).and_then(|provider| {
            request.usage.as_ref().and_then(|usage| {
                api_credit::cost_usd(
                    provider,
                    &request.model,
                    Usage { input: usage.input, output: usage.output, cache_read: usage.cache_read, cache_write: usage.cache_write },
                )
            })
        });
        record_api_credit(&mut store.connection, &target_id, pool.as_deref(), cost, now_ms())?;
        Ok(serde_json::json!({"accountId": target_id, "costUsd": cost}))
    }
}
