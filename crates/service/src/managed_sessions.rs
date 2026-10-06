use crate::{
    authorize, process,
    store::{lease, LeaseRecord},
    Service, ValidateChild,
};
use aam_protocol::{Account, ApiError, LaunchIntent, ProcessIdentity, Session, Takeover};
use serde_json::Value;

impl Service {
    pub(super) fn check_child(
        &self,
        parent: &LeaseRecord,
        child: &ProcessIdentity,
    ) -> Result<(), ApiError> {
        if parent.session.state != "ACTIVE"
            || !parent
                .session
                .process
                .as_ref()
                .is_some_and(|native| process::is_descendant(child, native))
        {
            return Err(ApiError::new(
                "PARENT_PROCESS_UNVERIFIED",
                "실행 중인 부모 native 프로세스의 자손인지 확인할 수 없습니다.",
            ));
        }
        Ok(())
    }
    pub(super) fn validate_child(&self, request: ValidateChild) -> Result<Value, ApiError> {
        let store = self.lock()?;
        let parent = lease(&store.connection, &request.session_id)?;
        authorize(&parent, &request.capability)?;
        if request.account_id != parent.session.account_id {
            return Err(ApiError::new(
                "IDENTITY_MISMATCH",
                "자식 실행의 계정이 원래 세션과 다릅니다.",
            ));
        }
        if request.root_spawn {
            if !matches!(parent.session.state.as_str(), "STARTING" | "ACTIVE")
                || !parent
                    .session
                    .supervisor
                    .as_ref()
                    .is_some_and(|supervisor| process::is_child(&request.process, supervisor))
                || parent
                    .session
                    .process
                    .as_ref()
                    .is_some_and(|native| native != &request.process)
            {
                return Err(ApiError::new(
                    "PARENT_PROCESS_UNVERIFIED",
                    "승인된 실행기의 직접 자식 프로세스가 아닙니다.",
                ));
            }
        } else {
            self.check_child(&parent, &request.process)?;
        }
        Ok(serde_json::json!({"session": parent.session, "account": parent.account}))
    }
    /// 외부 대화를 확인된 소유 계정으로 인계할 수 있는지 판단합니다. 근거가 없으면 인계하지 않습니다.
    fn adopt_external(
        &self,
        intent: &mut LaunchIntent,
        accounts: &[Account],
        takeovers: &[Takeover],
        auto: bool,
    ) -> Result<(), ApiError> {
        let Some(native) = intent.resume_session_id.clone() else {
            return Ok(());
        };
        let registered = takeovers
            .iter()
            .any(|record| record.tool == intent.tool && record.native_session_id == native);
        if !registered && !auto {
            return Ok(());
        }
        // 저장된 인계도 매번 실제 파일 근거로 다시 확인합니다.
        let owner = match aam_adapters::session_owner(accounts, &intent.tool, &native) {
            Ok(owner) => owner,
            Err(error) if registered => return Err(error),
            Err(_) => return Ok(()),
        };
        if intent
            .account_id
            .as_deref()
            .is_some_and(|id| id != owner.account_id)
        {
            return Err(ApiError::new(
                "SWITCH_UNSUPPORTED",
                "이 대화의 소유 계정과 다른 계정으로는 재개할 수 없습니다. 계정 배분은 새 세션에만 적용됩니다.",
            ));
        }
        if let Some(cwd) = owner.cwd {
            intent.cwd = cwd;
        }
        intent.account_id = Some(owner.account_id);
        intent.adopted = true;
        Ok(())
    }
    pub(super) fn normalize_resume(
        &self,
        intent: &mut LaunchIntent,
        sessions: &[Session],
        accounts: &[Account],
        takeovers: &[Takeover],
        auto_takeover: bool,
    ) -> Result<(), ApiError> {
        if intent.native_session_id.is_some()
            && (intent.tool != "claude" || intent.resume_session_id.is_some())
        {
            return Err(ApiError::new(
                "INVALID_INTENT",
                "native 세션 ID 지정은 새 Claude 세션에서만 사용할 수 있습니다.",
            ));
        }
        // 클라이언트가 보낸 인계 표시는 신뢰하지 않고 매 요청에서 실제 근거로 다시 확인합니다.
        intent.adopted = false;
        let Some(id) = intent.resume_session_id.as_deref() else {
            return Ok(());
        };
        if let Ok(uuid) = uuid::Uuid::parse_str(id) {
            intent.resume_session_id = Some(uuid.to_string());
        }
        let mapped = {
            let id = intent.resume_session_id.as_deref().unwrap();
            sessions.iter().any(|session| {
                session.tool == intent.tool
                    && (session.id == id || session.native_session_id.as_deref() == Some(id))
            })
        };
        if !mapped && intent.continue_elsewhere {
            return Err(ApiError::new(
                "CONTINUE_UNKNOWN",
                "다른 계정에서 이어 가기는 Ojak이 관리한 대화에서만 할 수 있습니다.",
            ));
        }
        if !mapped {
            self.adopt_external(intent, accounts, takeovers, auto_takeover)?;
        }
        Ok(())
    }
}
