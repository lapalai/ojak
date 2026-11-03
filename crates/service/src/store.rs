use aam_protocol::{Account, ApiError, DismissedProfile, Notice, Policy, Session, Takeover, ToolStatus};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

pub(crate) fn db_error(_: rusqlite::Error) -> ApiError {
    ApiError::new(
        "STORAGE_ERROR",
        "관리 상태 저장소를 읽거나 저장할 수 없습니다.",
    )
}
pub(crate) fn encode<T: Serialize>(value: &T) -> Result<String, ApiError> {
    serde_json::to_string(value)
        .map_err(|_| ApiError::new("STATE_INVALID", "관리 상태를 직렬화할 수 없습니다."))
}
pub(crate) fn decode<T: DeserializeOwned>(value: &str) -> Result<T, ApiError> {
    serde_json::from_str(value).map_err(|_| {
        ApiError::new(
            "STATE_INVALID",
            "저장된 관리 상태의 형식이 올바르지 않습니다.",
        )
    })
}
pub(crate) fn fingerprint<T: Serialize>(value: &T) -> Result<String, ApiError> {
    Ok(format!("{:x}", Sha256::digest(encode(value)?.as_bytes())))
}
pub(crate) fn binding_fingerprint(account: &Account) -> Result<String, ApiError> {
    let binding = (
        &account.tool,
        &account.identity_key,
        &account.profile_path,
        &account.binary_path,
        &account.auth_status,
        &account.verification,
        account.can_launch,
        account.enabled,
        account.max_concurrency,
    );
    fingerprint(&binding)
}

pub(crate) struct Store {
    pub connection: Connection,
}

#[derive(Clone)]
pub(crate) struct LeaseRecord {
    pub session: Session,
    pub capability: String,
    pub payload_hash: String,
    pub policy_revision: u64,
    pub binding_hash: String,
    pub account: Account,
    pub expires_at: i64,
    pub heartbeat_at: i64,
    pub manual: bool,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, ApiError> {
        let mut connection = Connection::open(path).map_err(db_error)?;
        connection
            .busy_timeout(std::time::Duration::from_secs(5))
            .map_err(db_error)?;
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(db_error)?;
        if version > 1 {
            return Err(ApiError::new(
                "SCHEMA_VERSION_UNSUPPORTED",
                "이 저장소는 더 새로운 관리 서비스가 필요합니다.",
            ));
        }
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA synchronous=FULL; PRAGMA trusted_schema=OFF;").map_err(db_error)?;
        if version == 0 {
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                CREATE TABLE accounts (id TEXT PRIMARY KEY, body TEXT NOT NULL);
                CREATE TABLE metadata (key TEXT PRIMARY KEY, body TEXT NOT NULL);
                CREATE TABLE leases (
                    id TEXT PRIMARY KEY,
                    account_id TEXT NOT NULL,
                    request_id TEXT NOT NULL UNIQUE,
                    client_id TEXT NOT NULL,
                    payload_hash TEXT NOT NULL,
                    capability TEXT NOT NULL,
                    policy_revision INTEGER NOT NULL,
                    binding_hash TEXT NOT NULL,
                    account_body TEXT NOT NULL,
                    expires_at INTEGER NOT NULL,
                    heartbeat_at INTEGER NOT NULL,
                    manual INTEGER NOT NULL,
                    body TEXT NOT NULL
                );
                PRAGMA user_version=1;
                COMMIT;",
                )
                .map_err(db_error)?;
        }
        drop_lease_account_reference(&connection)?;
        // 계정별 lease 참조 조회(계정 삭제의 외래키·이력 보존 검사)용 색인. 컬럼과 스키마 버전은 그대로다.
        connection
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS leases_account_id ON leases(account_id);
                 CREATE INDEX IF NOT EXISTS leases_capacity_snapshot ON leases(json_extract(body,'$.updatedAt') DESC,id)
                   WHERE COALESCE(json_extract(body,'$.state'),'') IN ('PREPARED','STARTING','ACTIVE','SUSPECT','ORPHANED');
                 CREATE INDEX IF NOT EXISTS leases_recent_history ON leases(json_extract(body,'$.updatedAt') DESC,id)
                   WHERE COALESCE(json_extract(body,'$.state'),'') NOT IN ('PREPARED','STARTING','ACTIVE','SUSPECT','ORPHANED');",
            )
            .map_err(db_error)?;
        // API 키(월 API 크레딧) 사용액 원장. 계정 행(`accounts.body`)과 따로 보관하므로 키 계정을 지워도 이번 달 합계에서 빠지지 않는다.
        // 계정당·UTC 달력월당 한 행이며 행은 계정이 사라진 뒤에도 남는다. `provider`는 키의 공급자(`anthropic`·`openai`)라 지운 키의 사용액이
        // 자기 공급자의 월 합계에만 들어간다. 컬럼·스키마 버전(user_version)은 그대로다.
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS api_credit_usage (
                    account_id TEXT NOT NULL,
                    period_start INTEGER NOT NULL,
                    used_usd REAL NOT NULL DEFAULT 0,
                    unmetered_requests INTEGER NOT NULL DEFAULT 0,
                    updated_at INTEGER NOT NULL DEFAULT 0,
                    provider TEXT NOT NULL DEFAULT 'anthropic',
                    PRIMARY KEY (account_id, period_start)
                ) WITHOUT ROWID;
                -- 사용자가 확인한 API 크레딧 허용 기간(`Policy.api_credit_grant.id`)별 합산 추정 사용액. 키 계정과 무관하게 쌓으므로 키를 지워도 남는다.
                CREATE TABLE IF NOT EXISTS api_credit_grant_usage (
                    grant_id TEXT NOT NULL PRIMARY KEY,
                    used_usd REAL NOT NULL DEFAULT 0,
                    unmetered_requests INTEGER NOT NULL DEFAULT 0,
                    updated_at INTEGER NOT NULL DEFAULT 0
                ) WITHOUT ROWID;",
            )
            .map_err(db_error)?;
        // 공급자 열이 생기기 전 원장: 그때 키는 Claude 키뿐이라 기본값 `anthropic`으로 채워진다.
        let has_provider: bool = connection
            .query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('api_credit_usage') WHERE name='provider')", [], |row| row.get(0))
            .map_err(db_error)?;
        if !has_provider {
            connection
                .execute_batch("ALTER TABLE api_credit_usage ADD COLUMN provider TEXT NOT NULL DEFAULT 'anthropic';")
                .map_err(db_error)?;
        }
        migrate_api_credit_bodies(&mut connection)?;
        if metadata::<Policy>(&connection, "policy")?.is_none() {
            set_metadata(&connection, "policy", &Policy::default())?;
        }
        Ok(Self { connection })
    }
}

/// 0.18.0이 계정 본문(`Account.api_credit`)에 쌓아 둔 사용액을 원장으로 옮기고 본문에서는 지운다. 같은 (계정, 기간) 행이 이미 있으면
/// 더한 값이 아니라 더 큰 쪽을 둔다(이전 판으로 되돌아가 본문에 다시 쌓인 값도 두 번 세지 않는다). 한 트랜잭션이라 중간에 끊겨도 다음 시작에서 이어서 처리하고, 옮길 것이 없으면 아무것도 쓰지 않는다.
fn migrate_api_credit_bodies(connection: &mut Connection) -> Result<(), ApiError> {
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(db_error)?;
    let legacy: Vec<Account> = {
        let mut statement = tx.prepare(
            "SELECT body FROM accounts WHERE json_extract(body,'$.tool')='api' AND json_type(body,'$.apiCredit')='object'",
        ).map_err(db_error)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0)).map_err(db_error)?;
        rows.map(|row| decode(&row.map_err(db_error)?)).collect::<Result<_, ApiError>>()?
    };
    for account in &legacy {
        let state = account.api_credit.as_ref().expect("filtered");
        tx.execute(
            "INSERT INTO api_credit_usage(account_id,period_start,used_usd,unmetered_requests,updated_at) VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(account_id,period_start) DO UPDATE SET
               used_usd=MAX(used_usd,excluded.used_usd),
               unmetered_requests=MAX(unmetered_requests,excluded.unmetered_requests),
               updated_at=MAX(updated_at,excluded.updated_at)",
            params![account.id, state.period_start, state.used_usd, state.unmetered_requests as i64, state.updated_at],
        )
        .map_err(db_error)?;
        let mut stripped = account.clone();
        stripped.api_credit = None;
        tx.execute("UPDATE accounts SET body=?2 WHERE id=?1", params![account.id, encode(&stripped)?]).map_err(db_error)?;
    }
    tx.commit().map_err(db_error)
}

/// 계정을 끊어도 세션 이력(`leases`)은 남긴다. 이전 판이 만든 `leases.account_id → accounts(id)` 외래키는
/// 이력이 있는 계정의 삭제를 막으므로, 한 번만 같은 열·색인의 표로 다시 만들어 외래키만 뺀다. 이력 본문은 계정 사본을 함께 들고 있다.
fn drop_lease_account_reference(connection: &Connection) -> Result<(), ApiError> {
    let references: i64 = connection
        .query_row("SELECT COUNT(*) FROM pragma_foreign_key_list('leases')", [], |row| row.get(0))
        .map_err(db_error)?;
    if references == 0 {
        return Ok(());
    }
    connection
        .execute_batch(
            "BEGIN IMMEDIATE;
            CREATE TABLE leases_rebuilt (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                request_id TEXT NOT NULL UNIQUE,
                client_id TEXT NOT NULL,
                payload_hash TEXT NOT NULL,
                capability TEXT NOT NULL,
                policy_revision INTEGER NOT NULL,
                binding_hash TEXT NOT NULL,
                account_body TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                heartbeat_at INTEGER NOT NULL,
                manual INTEGER NOT NULL,
                body TEXT NOT NULL
            );
            INSERT INTO leases_rebuilt SELECT id,account_id,request_id,client_id,payload_hash,capability,policy_revision,binding_hash,account_body,expires_at,heartbeat_at,manual,body FROM leases;
            DROP TABLE leases;
            ALTER TABLE leases_rebuilt RENAME TO leases;
            COMMIT;",
        )
        .map_err(db_error)
}

pub(crate) fn metadata<T: DeserializeOwned>(
    connection: &Connection,
    key: &str,
) -> Result<Option<T>, ApiError> {
    let value: Option<String> = connection
        .query_row("SELECT body FROM metadata WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(db_error)?;
    value.map(|body| decode(&body)).transpose()
}
pub(crate) fn set_metadata<T: Serialize>(
    connection: &Connection,
    key: &str,
    value: &T,
) -> Result<(), ApiError> {
    connection.execute("INSERT INTO metadata(key, body) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET body=excluded.body", params![key, encode(value)?]).map_err(db_error)?;
    Ok(())
}
pub(crate) fn policy(connection: &Connection) -> Result<Policy, ApiError> {
    let Some(mut raw) = metadata::<serde_json::Value>(connection, "policy")? else {
        return Ok(Policy::default());
    };
    // 이전 판의 도구별 기본 계정(`preferredAccounts`, claude·codex)을 공급자별 수동 배정으로 옮긴다.
    if let Some(object) = raw.as_object_mut() {
        if let Some(legacy) = object.remove("preferredAccounts") {
            if !object.contains_key("providerPins") {
                let pins: serde_json::Map<String, serde_json::Value> = legacy
                    .as_object()
                    .into_iter()
                    .flatten()
                    .map(|(tool, id)| (aam_protocol::pin_provider(tool).to_owned(), id.clone()))
                    .collect();
                object.insert("providerPins".into(), serde_json::Value::Object(pins));
            }
        }
    }
    serde_json::from_value(raw).map_err(|_| {
        ApiError::new("STATE_CORRUPT", "저장된 정책을 해석하지 못했습니다.")
    })
}
pub(crate) fn takeovers(connection: &Connection) -> Result<Vec<Takeover>, ApiError> {
    Ok(metadata(connection, "takeovers")?.unwrap_or_default())
}
/// 사용자가 연결을 끊어 자동 탐색에서 뺀 native 프로필 목록.
pub(crate) fn dismissed_profiles(connection: &Connection) -> Result<Vec<DismissedProfile>, ApiError> {
    Ok(metadata(connection, "dismissedProfiles")?.unwrap_or_default())
}
pub(crate) fn set_dismissed_profiles(
    connection: &Connection,
    records: &[DismissedProfile],
) -> Result<(), ApiError> {
    set_metadata(connection, "dismissedProfiles", &records)
}
pub(crate) fn set_takeovers(
    connection: &Connection,
    records: &[Takeover],
) -> Result<(), ApiError> {
    set_metadata(connection, "takeovers", &records)
}
fn raw_accounts(connection: &Connection) -> Result<Vec<Account>, ApiError> {
    let mut statement = connection
        .prepare("SELECT body FROM accounts ORDER BY id")
        .map_err(db_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_error)?;
    rows.map(|row| decode(&row.map_err(db_error)?)).collect()
}
/// 계정 목록. API 키 계정의 `api_credit`은 원장에서 가장 최근 기간의 행으로 채운다(기간이 지났는지는 소비자가 `now`로 본다).
pub(crate) fn accounts(connection: &Connection) -> Result<Vec<Account>, ApiError> {
    let mut accounts = raw_accounts(connection)?;
    if accounts.iter().any(|account| account.tool == aam_adapters::api_credit::API_TOOL) {
        let mut statement = connection
            .prepare(
                "SELECT account_id,period_start,used_usd,unmetered_requests,updated_at FROM api_credit_usage u
                 WHERE period_start=(SELECT MAX(period_start) FROM api_credit_usage WHERE account_id=u.account_id)",
            )
            .map_err(db_error)?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, ledger_state(row, 1)?)))
            .map_err(db_error)?;
        let mut latest = std::collections::HashMap::new();
        for row in rows {
            let (id, state) = row.map_err(db_error)?;
            latest.insert(id, state);
        }
        for account in accounts.iter_mut().filter(|account| account.tool == aam_adapters::api_credit::API_TOOL) {
            account.api_credit = latest.remove(&account.id);
        }
    }
    Ok(accounts)
}
fn ledger_state(row: &rusqlite::Row<'_>, first: usize) -> rusqlite::Result<aam_protocol::ApiCreditState> {
    Ok(aam_protocol::ApiCreditState {
        period_start: row.get(first)?,
        used_usd: row.get(first + 1)?,
        unmetered_requests: row.get::<_, i64>(first + 2)?.max(0) as u64,
        updated_at: row.get(first + 3)?,
    })
}
pub(crate) fn account(connection: &Connection, id: &str) -> Result<Account, ApiError> {
    let body: Option<String> = connection
        .query_row("SELECT body FROM accounts WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(db_error)?;
    let mut account: Account = body
        .map(|body| decode(&body))
        .transpose()?
        .ok_or_else(|| ApiError::new("ACCOUNT_NOT_FOUND", "계정을 찾을 수 없습니다."))?;
    if account.tool == aam_adapters::api_credit::API_TOOL {
        account.api_credit = connection
            .query_row(
                "SELECT period_start,used_usd,unmetered_requests,updated_at FROM api_credit_usage WHERE account_id=?1 ORDER BY period_start DESC LIMIT 1",
                [id],
                |row| ledger_state(row, 0),
            )
            .optional()
            .map_err(db_error)?;
    }
    Ok(account)
}
/// 계정 본문을 저장한다. `api_credit`은 원장(`api_credit_usage`)이 정본이므로 본문에는 쓰지 않는다. 저장된 계정 본문을 다시 저장해도 사용액이 되돌아가지 않는다.
pub(crate) fn save_account(connection: &Connection, account: &Account) -> Result<(), ApiError> {
    let body = if account.api_credit.is_some() {
        let mut stored = account.clone();
        stored.api_credit = None;
        encode(&stored)?
    } else {
        encode(account)?
    };
    connection.execute("INSERT INTO accounts(id,body) VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body", params![account.id, body]).map_err(db_error)?;
    Ok(())
}
pub(crate) fn ledger_knows(connection: &Connection, account_id: &str) -> Result<bool, ApiError> {
    connection
        .query_row("SELECT EXISTS(SELECT 1 FROM api_credit_usage WHERE account_id=?1)", [account_id], |row| row.get(0))
        .map_err(db_error)
}
/// 원장이 기록해 둔 키 계정의 공급자. 행이 없으면 `None`. 지운 키의 공급자를 계정 행 없이도 알 수 있다.
pub(crate) fn ledger_provider(connection: &Connection, account_id: &str) -> Result<Option<String>, ApiError> {
    connection
        .query_row("SELECT provider FROM api_credit_usage WHERE account_id=?1 LIMIT 1", [account_id], |row| row.get(0))
        .optional()
        .map_err(db_error)
}
/// API 키 계정을 지우기 전에 원장에 그 계정의 행이 있게 한다. 지운 뒤에 끝나는 요청의 사용액도 같은 계정 몫으로 이어서 기록할 수 있다.
/// 행에는 계정의 공급자를 함께 적는다(지운 뒤에도 그 공급자의 월 합계에만 들어가게).
pub(crate) fn retain_api_credit_ledger(connection: &Connection, account: &Account, now: i64) -> Result<(), ApiError> {
    if !ledger_knows(connection, &account.id)? {
        connection
            .execute(
                "INSERT OR IGNORE INTO api_credit_usage(account_id,period_start,used_usd,unmetered_requests,updated_at,provider) VALUES (?1,?2,0,0,?3,?4)",
                params![account.id, aam_adapters::api_credit::period_start(now), now, account.provider],
            )
            .map_err(db_error)?;
    }
    Ok(())
}
/// API 키 계정의 이번 달 추정 사용액에 한 요청을 더한다. `cost`가 `None`이면 usage를 못 읽은 요청으로 센다.
/// 사용액은 계정 행이 아니라 원장에 쌓는다. 기간(UTC 달력월)이 바뀌면 새 행에서 0부터 다시 센다. 같은 트랜잭션 안에서 더해 동시 요청의 덧셈이 사라지지 않게 한다.
/// 이미 지운 키 계정(원장에 행이 있는 ID)의 늦은 기록도 받는다. 모르는 ID는 `ACCOUNT_NOT_FOUND`, API 키가 아닌 계정은 기록하지 않는다.
/// 허용 기간(grant) 합산 원장은 Claude 크레딧의 개념이라 Anthropic 키만 더한다.
pub(crate) fn record_api_credit(connection: &mut Connection, account_id: &str, cost: Option<f64>, now: i64) -> Result<(), ApiError> {
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(db_error)?;
    let provider = match account(&tx, account_id) {
        Ok(found) if found.tool != aam_adapters::api_credit::API_TOOL => return Ok(()),
        Ok(found) => found.provider,
        Err(error) if error.code == "ACCOUNT_NOT_FOUND" => match ledger_provider(&tx, account_id)? {
            Some(provider) => provider,
            None => return Err(error),
        },
        Err(error) => return Err(error),
    };
    let (spent, unmetered) = match cost.filter(|value| value.is_finite() && *value >= 0.0) {
        Some(value) => (value, 0_i64),
        None => (0.0, 1_i64),
    };
    tx.execute(
        "INSERT INTO api_credit_usage(account_id,period_start,used_usd,unmetered_requests,updated_at,provider) VALUES (?1,?2,?3,?4,?5,?6)
         ON CONFLICT(account_id,period_start) DO UPDATE SET
           used_usd=used_usd+excluded.used_usd,
           unmetered_requests=unmetered_requests+excluded.unmetered_requests,
           updated_at=excluded.updated_at",
        params![account_id, aam_adapters::api_credit::period_start(now), spent, unmetered, now, provider],
    )
    .map_err(db_error)?;
    // 현재 허용 기간이 있고 요청이 그 기간 안에 끝났으면 같은 금액을 그 기간의 합산 원장에도 더한다(키 계정과 무관, 같은 트랜잭션).
    // 기간 밖(시작 전·만료 뒤) 사용은 허용 기간 예산에 넣지 않는다. 월 상한 합계에는 위에서 이미 들어갔다.
    if provider == aam_adapters::api_credit::ApiProvider::Anthropic.id() {
        if let Some(grant) = policy(&tx)?.api_credit_grant.filter(|grant| (grant.starts_at..grant.expires_at).contains(&now)) {
            tx.execute(
                "INSERT INTO api_credit_grant_usage(grant_id,used_usd,unmetered_requests,updated_at) VALUES (?1,?2,?3,?4)
                 ON CONFLICT(grant_id) DO UPDATE SET
                   used_usd=used_usd+excluded.used_usd,
                   unmetered_requests=unmetered_requests+excluded.unmetered_requests,
                   updated_at=excluded.updated_at",
                params![grant.id, spent, unmetered, now],
            )
            .map_err(db_error)?;
        }
    }
    tx.commit().map_err(db_error)
}
/// 판정에 쓰는 API 크레딧 원장 값: 지운 키의 이번 달 사용액(공급자별)과 현재 허용 기간(Claude)의 합산 사용액·미확인 요청 수.
pub(crate) fn api_ledger(connection: &Connection, policy: &Policy, now: i64) -> Result<crate::scheduler::ApiLedger, ApiError> {
    use aam_adapters::api_credit::ApiProvider;
    let removed_usd = api_credit_removed(connection, ApiProvider::Anthropic, now)?.used_usd;
    let openai_removed = api_credit_removed(connection, ApiProvider::OpenAi, now)?;
    let (grant_used_usd, grant_unmetered) = match &policy.api_credit_grant {
        Some(grant) => connection
            .query_row("SELECT used_usd,unmetered_requests FROM api_credit_grant_usage WHERE grant_id=?1", [&grant.id], |row| {
                Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?.max(0) as u64))
            })
            .optional()
            .map_err(db_error)?
            .unwrap_or((0.0, 0)),
        None => (0.0, 0),
    };
    Ok(crate::scheduler::ApiLedger { removed_usd, openai_removed_usd: openai_removed.used_usd, openai_removed_unmetered: openai_removed.unmetered_requests, grant_used_usd, grant_unmetered })
}
/// 지운 API 키 계정(계정 행이 없는 원장 행)이 `now`가 속한 달에 쓴 추정 사용액의 합. 해당 공급자의 월 상한 합계와 화면 합계는 남아 있는 키의 합계에 이 값을 더한다.
pub(crate) fn api_credit_removed(connection: &Connection, provider: aam_adapters::api_credit::ApiProvider, now: i64) -> Result<aam_protocol::ApiCreditState, ApiError> {
    let period = aam_adapters::api_credit::period_start(now);
    connection
        .query_row(
            "SELECT COALESCE(SUM(used_usd),0.0),COALESCE(SUM(unmetered_requests),0),COALESCE(MAX(updated_at),0) FROM api_credit_usage
             WHERE period_start=?1 AND provider=?2 AND account_id NOT IN (SELECT id FROM accounts)",
            params![period, provider.id()],
            |row| {
                Ok(aam_protocol::ApiCreditState {
                    period_start: period,
                    used_usd: row.get(0)?,
                    unmetered_requests: row.get::<_, i64>(1)?.max(0) as u64,
                    updated_at: row.get(2)?,
                })
            },
        )
        .map_err(db_error)
}
/// 테스트 전용: 계정 상태를 그대로 원장에 심는다(같은 계정·기간 행은 덮어쓴다).
#[cfg(test)]
pub(crate) fn seed_api_credit(connection: &Connection, account_id: &str, provider: &str, state: &aam_protocol::ApiCreditState) {
    connection
        .execute(
            "INSERT OR REPLACE INTO api_credit_usage(account_id,period_start,used_usd,unmetered_requests,updated_at,provider) VALUES (?1,?2,?3,?4,?5,?6)",
            params![account_id, state.period_start, state.used_usd, state.unmetered_requests as i64, state.updated_at, provider],
        )
        .unwrap();
}
type LeaseRow = (String, String, String, u64, String, String, i64, i64, bool);

fn row_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<LeaseRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
    ))
}
fn unpack(row: LeaseRow) -> Result<LeaseRecord, ApiError> {
    Ok(LeaseRecord {
        session: decode(&row.0)?,
        capability: row.1,
        payload_hash: row.2,
        policy_revision: row.3,
        binding_hash: row.4,
        account: decode(&row.5)?,
        expires_at: row.6,
        heartbeat_at: row.7,
        manual: row.8,
    })
}
pub(crate) fn lease(connection: &Connection, id: &str) -> Result<LeaseRecord, ApiError> {
    connection.query_row("SELECT body,capability,payload_hash,policy_revision,binding_hash,account_body,expires_at,heartbeat_at,manual FROM leases WHERE id=?1", [id], row_record).optional().map_err(db_error)?
        .map(unpack).transpose()?.ok_or_else(|| ApiError::new("SESSION_NOT_FOUND", "관리 세션을 찾을 수 없습니다."))
}
pub(crate) fn by_request(
    connection: &Connection,
    id: &str,
) -> Result<Option<LeaseRecord>, ApiError> {
    connection.query_row("SELECT body,capability,payload_hash,policy_revision,binding_hash,account_body,expires_at,heartbeat_at,manual FROM leases WHERE request_id=?1", [id], row_record).optional().map_err(db_error)?.map(unpack).transpose()
}
pub(crate) fn leases(connection: &Connection) -> Result<Vec<LeaseRecord>, ApiError> {
    let mut statement = connection.prepare("SELECT body,capability,payload_hash,policy_revision,binding_hash,account_body,expires_at,heartbeat_at,manual FROM leases ORDER BY id").map_err(db_error)?;
    let rows = statement.query_map([], row_record).map_err(db_error)?;
    rows.map(|row| unpack(row.map_err(db_error)?)).collect()
}

/// 생존 확인에 필요한 용량 보유 lease만 읽는다. 종료 이력은 이 경로에 들어오지 않는다.
pub(crate) fn capacity_leases(connection: &Connection) -> Result<Vec<LeaseRecord>, ApiError> {
    let mut statement = connection.prepare(
        "SELECT body,capability,payload_hash,policy_revision,binding_hash,account_body,expires_at,heartbeat_at,manual
         FROM leases WHERE COALESCE(json_extract(body,'$.state'),'') IN ('PREPARED','STARTING','ACTIVE','SUSPECT','ORPHANED')
         ORDER BY id",
    ).map_err(db_error)?;
    let rows = statement.query_map([], row_record).map_err(db_error)?;
    rows.map(|row| unpack(row.map_err(db_error)?)).collect()
}

/// 미실행 예약의 만료 확인만 한다. 실행 중·불확실 세션은 시간 초과만으로 반환하지 않는다.
pub(crate) fn prepared_leases(connection: &Connection) -> Result<Vec<LeaseRecord>, ApiError> {
    let mut statement = connection.prepare(
        "SELECT body,capability,payload_hash,policy_revision,binding_hash,account_body,expires_at,heartbeat_at,manual
         FROM leases WHERE COALESCE(json_extract(body,'$.state'),'') IN ('PREPARED','STARTING','ACTIVE','SUSPECT','ORPHANED')
           AND json_extract(body,'$.state')='PREPARED' ORDER BY id",
    ).map_err(db_error)?;
    let rows = statement.query_map([], row_record).map_err(db_error)?;
    rows.map(|row| unpack(row.map_err(db_error)?)).collect()
}

/// 상태 조회용 세션 목록. 용량을 쥔 세션은 모두, 종료 이력은 최근 `history`개만 DB에서 고른다.
/// 세션 본문(`body`)만 읽으므로 계정 사본·capability 등 조회에 쓰지 않는 열은 해석하지 않는다.
/// 용량 상태 목록은 `scheduler::holds_capacity`와 같아야 하며, 알 수 없는 상태는 종료 이력으로 취급한다.
/// 정렬은 기존과 같이 `updatedAt` 내림차순, 같으면 id 오름차순이다.
pub(crate) fn snapshot_sessions(connection: &Connection, history: usize) -> Result<Vec<Session>, ApiError> {
    let mut statement = connection
        .prepare(
            "SELECT body FROM (
                SELECT body,id,json_extract(body,'$.updatedAt') AS u FROM leases
                  WHERE COALESCE(json_extract(body,'$.state'),'') IN ('PREPARED','STARTING','ACTIVE','SUSPECT','ORPHANED')
                UNION ALL
                SELECT body,id,json_extract(body,'$.updatedAt') AS u FROM (
                  SELECT body,id FROM leases
                    WHERE COALESCE(json_extract(body,'$.state'),'') NOT IN ('PREPARED','STARTING','ACTIVE','SUSPECT','ORPHANED')
                    ORDER BY json_extract(body,'$.updatedAt') DESC,id ASC LIMIT ?1))
             ORDER BY u DESC,id ASC",
        )
        .map_err(db_error)?;
    let rows = statement
        .query_map([history as i64], |row| row.get::<_, String>(0))
        .map_err(db_error)?;
    rows.map(|row| decode(&row.map_err(db_error)?)).collect()
}
pub(crate) fn save_lease(connection: &Connection, record: &LeaseRecord) -> Result<(), ApiError> {
    connection
        .execute(
            "UPDATE leases SET body=?2,heartbeat_at=?3 WHERE id=?1",
            params![
                record.session.id,
                encode(&record.session)?,
                record.heartbeat_at
            ],
        )
        .map_err(db_error)?;
    Ok(())
}
pub(crate) fn insert_lease(
    connection: &Connection,
    record: &LeaseRecord,
    client_id: &str,
) -> Result<(), ApiError> {
    connection.execute("INSERT INTO leases(id,request_id,client_id,payload_hash,capability,policy_revision,binding_hash,account_body,expires_at,heartbeat_at,body,manual,account_id) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)", params![record.session.id, record.session.request_id, client_id, record.payload_hash, record.capability, record.policy_revision, record.binding_hash, encode(&record.account)?, record.expires_at, record.heartbeat_at, encode(&record.session)?, record.manual, record.session.account_id]).map_err(db_error)?;
    Ok(())
}
pub(crate) fn tools(connection: &Connection) -> Result<Vec<ToolStatus>, ApiError> {
    Ok(metadata(connection, "tools")?.unwrap_or_default())
}
pub(crate) fn notices(connection: &Connection) -> Result<Vec<Notice>, ApiError> {
    Ok(metadata(connection, "notices")?.unwrap_or_default())
}
