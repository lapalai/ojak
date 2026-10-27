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
        let connection = Connection::open(path).map_err(db_error)?;
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
        if metadata::<Policy>(&connection, "policy")?.is_none() {
            set_metadata(&connection, "policy", &Policy::default())?;
        }
        Ok(Self { connection })
    }
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
pub(crate) fn accounts(connection: &Connection) -> Result<Vec<Account>, ApiError> {
    let mut statement = connection
        .prepare("SELECT body FROM accounts ORDER BY id")
        .map_err(db_error)?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(db_error)?;
    rows.map(|row| decode(&row.map_err(db_error)?)).collect()
}
pub(crate) fn account(connection: &Connection, id: &str) -> Result<Account, ApiError> {
    let body: Option<String> = connection
        .query_row("SELECT body FROM accounts WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .optional()
        .map_err(db_error)?;
    body.map(|body| decode(&body))
        .transpose()?
        .ok_or_else(|| ApiError::new("ACCOUNT_NOT_FOUND", "계정을 찾을 수 없습니다."))
}
pub(crate) fn save_account(connection: &Connection, account: &Account) -> Result<(), ApiError> {
    connection.execute("INSERT INTO accounts(id,body) VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET body=excluded.body", params![account.id, encode(account)?]).map_err(db_error)?;
    Ok(())
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
