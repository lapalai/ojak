import { useEffect, useId, useState } from "react";
import { rpc } from "./api";
import { ActionFeedback, Badge, Busy, Modal } from "./components";
import { billingAccounts, billingFormFrom, billingPatch, creditSignature, draftRevision, estimateView, keyGrantForm, keyGrantOf, keyGrantPatch, proactiveBlockKey, sharedConsentNeeded } from "./api-credit-grant";
import type { BillingFormError, CreditDraftBase, KeyGrantError } from "./api-credit-grant";
import { apiAccounts, apiKeyProvider } from "./api-credit";
import { fullTime, privacyText, timeZoneLabel, useAction } from "./state";
import { t } from "./i18n";
import { planLabel } from "./plan";
import type { MessageKey } from "./i18n";
import { usdLimit, usdUsed } from "./limits";
import type { BillingCycle, Policy, Snapshot } from "./types";

const ERROR_KEYS: Record<KeyGrantError, MessageKey> = {
  keys: "grant.error.keys", budget: "grant.error.budget", start: "grant.error.start", expiry: "grant.error.expiry", range: "grant.error.range",
  past: "grant.error.past", tooLong: "grant.error.tooLong", confirm: "grant.error.confirm",
  estimateNone: "grant.error.estimateNone", estimateAnnual: "grant.error.estimateAnnual", estimatePassed: "grant.error.estimatePassed",
  estimateAccount: "grant.error.estimateAccount", estimateConfirm: "grant.error.estimateConfirm", shared: "apiKey.credit.error.shared",
};
const BILLING_ERROR_KEYS: Record<BillingFormError, MessageKey> = {
  account: "billing.error.account", date: "billing.error.date", past: "billing.error.past", tooFar: "billing.error.tooFar",
};
const ESTIMATE_BLOCK_KEYS = { NONE: "grant.estimate.none", ANNUAL: "grant.estimate.annual", DATE_PASSED: "grant.estimate.passed", ACCOUNT_INVALID: "grant.estimate.account" } as const;
const STATE_TONE = { upcoming: "neutral", active: "good", exhausted: "warning", expired: "warning" } as const;

/// 연결 → API 키 한 줄의 '금액·기간' 대화상자. 선택한 Claude API 키 **하나**의 크레딧 금액·시작·만료를 직접 입력해 저장한다(Console에서 읽은 값이 아니라 사용자가 입력한 값이고, 사용액은 Ojak이 보낸 요청의 추정치다).
/// 새 입력은 선택한 키의 id만 담아 id 없이 보내 0부터 센다. 이미 있는 입력은 같은 id·저장된 예산으로 보내 사용액을 이어 가며, 다른 키는 자동으로 고르지 않는다.
/// 여러 키가 함께 쓰는 이전 입력은 구성원을 그대로 보여 주고, 금액·기간을 바꾸려면 다른 키에도 적용된다는 동의를 받는다. 나누거나 사용액을 초기화하거나 구성원을 줄이는 변환은 하지 않는다.
/// 상태 판단(미리 쓰기 가능 여부 포함)은 서비스의 `snapshot.apiCreditGrants`만 믿고 여기서 다시 계산하지 않는다. 미리 쓰기 스위치와 월 상한은 크레딧 설정이 맡는다.
/// 만료는 정확한 날짜를 직접 입력하거나, 직접 저장한 월 결제일에서 서비스가 계산한 추정을 별도 동의로 쓴다. 연 결제·지난 결제일은 추정하지 않고 이유를 보여 준다.
/// 입력 중인 값은 3초 조회로 지우지 않는다. 저장 요청의 `expectedRevision`은 폼을 연 시점의 값이고, 이 키의 입력·결제 정보가 그대로일 때만 최신 revision을 이어 받는다.
/// 그 밖의 변경(다른 창에서 수정·삭제·교체)은 충돌로 보고 최신 내용을 불러오기 전에는 저장하지 못하게 한다. 이 대화상자의 결제 정보 저장은 입력 기준을 안전하게 이어 간다.
export function ApiKeyCreditDialog({ accountId, snapshot, online, masked, onReload, onClose }: { accountId: string; snapshot: Snapshot; online: boolean; masked: boolean; onReload: () => Promise<void>; onClose: () => void }) {
  const { policy } = snapshot;
  const action = useAction();
  const formId = useId();
  const grants = policy.apiCreditGrants ?? [];
  const billings = policy.apiCreditBillings ?? [];
  const grantStatuses = snapshot.apiCreditGrants ?? [];
  const billingStatuses = snapshot.apiCreditBillings ?? [];
  // 한 풀은 한 공급자의 키만 담는다. 구성원·저장 대상은 선택한 키와 같은 공급자의 키다.
  const account = apiAccounts(snapshot.accounts).find(key => key.id === accountId) ?? null;
  const openai = account !== null && apiKeyProvider(account) === "openai";
  const keys = apiAccounts(snapshot.accounts, openai ? "openai" : "anthropic");
  const keyIds = keys.map(key => key.id);
  const eligible = billingAccounts(snapshot.accounts);
  const freeEligible = eligible.filter(item => !billings.some(billing => billing.accountId === item.id));
  const currentGrant = keyGrantOf(grants, accountId);
  const current: CreditDraftBase = { revision: policy.revision, signature: creditSignature(currentGrant, billings) };
  // 폼을 만든 시점의 크레딧 입력(수정 대상·공유 동의의 기준)과 revision 기준.
  const [origin, setOrigin] = useState(currentGrant);
  const [base, setBase] = useState<CreditDraftBase>(current);
  const [form, setForm] = useState(() => keyGrantForm(currentGrant, accountId, billingStatuses, snapshot.generatedAt, keyIds));
  const [formError, setFormError] = useState<KeyGrantError | null>(null);
  const [sharedConsent, setSharedConsent] = useState(false);
  const [deleting, setDeleting] = useState(false);
  const [billingOpen, setBillingOpen] = useState(false);
  const [billingForm, setBillingForm] = useState(() => billingFormFrom(null, freeEligible));
  const [editingBilling, setEditingBilling] = useState<string | null>(null);
  const [billingError, setBillingError] = useState<BillingFormError | null>(null);
  const draft = draftRevision(base, current);
  const billingEditingExists = editingBilling !== null && billings.some(billing => billing.accountId === editingBilling);
  // 수정하던 결제 정보가 다른 곳에서 지워졌으면 새 입력으로 되돌린다. 그 밖에는 3초 조회로 입력 중인 값을 건드리지 않는다.
  useEffect(() => { if (editingBilling !== null && !billingEditingExists) { setEditingBilling(null); setBillingForm(billingFormFrom(null, freeEligible)); setBillingError(null); } }, [editingBilling, billingEditingExists]);
  const busy = !online || action.pending;
  const blocked = account === null || draft.conflict;
  const keyName = (id: string) => { const found = snapshot.accounts.find(item => item.id === id); return found ? privacyText(found.label, masked) : t("grant.keys.removed"); };
  const subscriptionName = (id: string) => { const found = snapshot.accounts.find(item => item.id === id); return found ? privacyText(found.email ?? found.label, masked) : t("billing.accountMissing"); };
  const name = account ? privacyText(account.label, masked) : "";
  const status = currentGrant ? grantStatuses.find(item => item.id === currentGrant.id) : undefined;
  const estimate = estimateView(billingStatuses.find(billing => billing.accountId === form.billingAccountId), snapshot.generatedAt);
  const estimating = form.expiryMode === "billing-estimate";
  const editing = form.id !== null;
  const needsConsent = sharedConsentNeeded(origin?.accountIds, form.accountIds);
  const members = (origin ?? currentGrant)?.accountIds ?? [];

  /// 저장 요청: 정책 변경은 모두 이 키 폼의 기준 revision(`draft.revision`)으로 보낸다. 성공하면 서비스가 돌려준 정책으로 기준을 이어 받는다(내 저장 때문에 생긴 revision 변화가 충돌로 보이지 않게).
  function send(patch: Record<string, unknown>) {
    const expectedRevision = draft.revision;
    return async () => {
      try {
        const saved = await rpc<Policy>("policy.update", { expectedRevision, ...patch });
        setBase({ revision: saved.revision, signature: creditSignature(keyGrantOf(saved.apiCreditGrants, accountId), saved.apiCreditBillings) });
        return saved;
      } finally { await onReload(); }
    };
  }
  function reloadLatest() {
    setOrigin(currentGrant);
    setBase(current);
    setForm(keyGrantForm(currentGrant, accountId, billingStatuses, snapshot.generatedAt, keyIds));
    setSharedConsent(false);
    setFormError(null);
    setDeleting(false);
    setEditingBilling(null);
    setBillingForm(billingFormFrom(null, freeEligible));
    setBillingError(null);
    action.clear();
  }
  function save() {
    if (blocked) return;
    const result = keyGrantPatch({ form, accountId, original: origin, sharedConsent }, Date.now(), billingStatuses);
    if (!result.ok) { setFormError(result.error); return; }
    setFormError(null);
    void action.run(send({ apiCreditGrant: result.patch }), () => t(origin ? "grant.saved.edit" : "grant.saved")).then(ok => { if (ok) onClose(); });
  }
  function remove() {
    if (blocked || !origin) return;
    void action.run(send({ apiCreditGrantRemove: origin.id }), () => t("apiKey.credit.delete.done")).then(ok => { if (ok) onClose(); });
  }
  function saveBilling() {
    if (blocked) return;
    const result = billingPatch(billingForm, eligible, Date.now());
    if (!result.ok) { setBillingError(result.error); return; }
    setBillingError(null);
    void action.run(send({ apiCreditBilling: result.patch }), () => t("billing.saved")).then(ok => {
      if (!ok) return;
      setEditingBilling(null);
      setBillingForm(billingFormFrom(null, freeEligible.filter(item => item.id !== result.patch.accountId)));
    });
  }

  return <Modal title={t("apiKey.credit.dialog.title", { name })} subtitle={t(openai ? "apiKey.credit.dialog.subtitle.openai" : "apiKey.credit.dialog.subtitle")} busy={action.pending} onClose={onClose} wide>
    <div className="modal-body key-credit-body">
      <fieldset className="key-credit-lock" disabled={busy}>
        {account === null && <p className="gate-reason" role="alert">{t("apiKey.credit.accountGone")}</p>}
        {draft.conflict && <div className="key-credit-conflict" role="alert">
          <p className="gate-reason">{t("apiKey.credit.conflict")}</p>
          <button type="button" onClick={reloadLatest}>{t("apiKey.credit.reload")}</button>
        </div>}
        {currentGrant ? <>
          <div className="key-credit-status">
            {status && <Badge tone={STATE_TONE[status.state]}>{t(`grant.state.${status.state}`)}</Badge>}
            {members.length > 1 && <Badge tone="accent">{t("apiKey.credit.shared")}</Badge>}
          </div>
          <ul className="grant-facts" aria-label={t("grant.factsAria")}>
            {status ? <li>{t("grant.remaining", { remaining: usdUsed(status.remainingUsd), budget: usdLimit(status.budgetUsd) })}</li> : <li>{t("apiKey.credit.statusUnknown")}</li>}
            {status?.state === "active" && status.perDayUsd !== null && <li>{t("grant.perDay", { amount: usdUsed(status.perDayUsd) })}</li>}
            {(status ? status.state === "upcoming" : currentGrant.startsAt > snapshot.generatedAt) && <li>{t("grant.starts", { time: fullTime(currentGrant.startsAt) })}</li>}
            <li>{t((status ? status.state === "expired" : currentGrant.expiresAt <= snapshot.generatedAt) ? "grant.expired" : "grant.expires", { time: fullTime(currentGrant.expiresAt) })}{currentGrant.expirySource === "billing-estimate" && <> <span className="tag warning">{t("grant.estimateTag")}</span></>}</li>
            {currentGrant.expirySource === "billing-estimate" && <li>{t("grant.estimateNote", { account: subscriptionName(status?.billingEstimate?.accountId ?? currentGrant.billingEstimate?.accountId ?? "") })}</li>}
            {currentGrant.expirySource === "billing-estimate" && status?.estimateDrift && <li className="warn">{t("grant.estimateDrift")}</li>}
            {status?.state === "exhausted" && <li>{t("grant.exhausted")}</li>}
            {status?.state === "expired" && <li>{t("grant.expiredNote")}</li>}
            {status?.unmetered && <li className="warn">{t("grant.unmetered", { count: status.unmeteredRequests })}</li>}
            {policy.balanceApiCredit && status && <li className={status.proactive.allowed ? undefined : "warn"}>{status.proactive.allowed ? t("balance.allowed") : t("balance.blocked", { reason: (() => { const key = proactiveBlockKey(status.proactive.blockedBy); return key ? t(key) : t("balance.block.unknown"); })() })}</li>}
          </ul>
        </> : <p className="field-help">{t("grant.none")}</p>}
        {members.length > 1 && <div className="key-credit-members" role="note">
          <p>{t("apiKey.credit.sharedWarn", { count: members.length })}</p>
          <ul>{members.map(id => <li key={id}>{keyIds.includes(id) ? keyName(id) : t("grant.keys.removed")}{id === accountId && <> <span className="tag">{t("apiKey.credit.thisKey")}</span></>}</li>)}</ul>
        </div>}
        <form id={formId} className="grant-form" aria-label={t("apiKey.credit.manage")} onSubmit={event => { event.preventDefault(); if (!busy) save(); }}>
          <label>{t(editing ? "grant.budget.edit" : "grant.budget")}<span className="grant-input"><input type="number" min={0.01} step={0.01} inputMode="decimal" value={form.budget} aria-label={t("grant.budgetAria")} aria-invalid={formError === "budget"} onChange={event => setForm({ ...form, budget: event.target.value })} />USD</span></label>
          <label>{t("grant.starts.label")}<input type="datetime-local" value={form.startsAt} aria-label={t("grant.startsAria")} aria-invalid={formError === "start" || formError === "range"} onChange={event => setForm({ ...form, startsAt: event.target.value })} /></label>
          {editing && <p className="field-help">{t("grant.budget.editHelp")}</p>}
          {/* OpenAI 크레딧은 Platform의 Credit grants에 만료일이 나오므로 결제일로 추정하지 않는다(서비스도 거절한다). */}
          {!openai && <fieldset className="grant-expiry">
            <legend>{t("grant.expiry.legend")}</legend>
            <label className="grant-radio"><input type="radio" name={`${formId}-expiry`} checked={!estimating} onChange={() => setForm({ ...form, expiryMode: "exact", estimateConfirmedFor: null })} /><span>{t("grant.expiry.exact")}</span></label>
            <label className="grant-radio"><input type="radio" name={`${formId}-expiry`} checked={estimating} disabled={billingStatuses.length === 0 && !estimating} onChange={() => setForm({ ...form, expiryMode: "billing-estimate" })} /><span>{t("grant.expiry.estimate")}</span></label>
          </fieldset>}
          {estimating ? <>
            <label>{t("grant.billingAccount")}<select value={form.billingAccountId ?? ""} aria-label={t("grant.billingAccountAria")} onChange={event => setForm({ ...form, billingAccountId: event.target.value || null, estimateConfirmedFor: null })}>
              <option value="">{t("billing.accountPick")}</option>
              {billingStatuses.map(billing => <option key={billing.accountId} value={billing.accountId}>{subscriptionName(billing.accountId)}</option>)}
            </select></label>
            {estimate.blockedBy === null
              ? <p className="grant-estimate" role="status"><strong>{fullTime(estimate.expiresAt)}</strong> <span className="tag warning">{t("grant.estimateTag")}</span><br />{t("grant.estimate.basis", { zone: timeZoneLabel() })}</p>
              : <p className="gate-reason" role="status">{t(ESTIMATE_BLOCK_KEYS[estimate.blockedBy])}</p>}
            {estimate.blockedBy === null && <label className="grant-confirm"><input type="checkbox" checked={form.estimateConfirmedFor === estimate.expiresAt} onChange={event => setForm({ ...form, estimateConfirmedFor: event.target.checked ? estimate.expiresAt : null })} /><span>{t("grant.estimate.confirm")}</span></label>}
          </> : <label>{t("grant.expires.label")}<input type="datetime-local" value={form.expiresAt} aria-label={t("grant.expiresAria")} aria-invalid={formError === "expiry" || formError === "range" || formError === "past" || formError === "tooLong"} onChange={event => setForm({ ...form, expiresAt: event.target.value })} /></label>}
          <p className="field-help">{t("usage.timezone", { zone: timeZoneLabel() })}</p>
          <label className="grant-confirm"><input type="checkbox" checked={form.confirmed} onChange={event => setForm({ ...form, confirmed: event.target.checked })} /><span>{t(estimating ? "grant.confirm.balance" : openai ? "grant.confirm.openai" : "grant.confirm")}</span></label>
          {needsConsent && <label className="grant-confirm"><input type="checkbox" checked={sharedConsent} aria-invalid={formError === "shared"} onChange={event => setSharedConsent(event.target.checked)} /><span>{t("apiKey.credit.sharedConsent", { count: members.length })}</span></label>}
          {formError && <p className="gate-reason" role="alert">{t(ERROR_KEYS[formError])}</p>}
          <p className="field-help">{t(openai ? "grant.safety.openai" : "grant.safety")}</p>
        </form>
        {!openai && <details className="key-credit-billing" open={billingOpen} onToggle={event => setBillingOpen(event.currentTarget.open)}>
          <summary>{t("apiKey.credit.billing.summary")}</summary>
          <h3 id={`${formId}-billing`} className="grant-subtitle">{t("billing.title")}</h3>
          <p className="field-help">{t("billing.tip")}</p>
          {billings.length === 0 ? <p className="field-help">{t("billing.none")}</p> : <ul className="grant-pools" aria-labelledby={`${formId}-billing`}>
            {billings.map(billing => <li key={billing.accountId} className="grant-pool grant-billing-row">
              <span>{t("billing.saved.line", { account: subscriptionName(billing.accountId), cycle: t(billing.cycle === "annual" ? "billing.cycle.annual" : "billing.cycle.monthly"), time: fullTime(billing.nextBillingAt) })}</span>
              <span className="grant-actions">
                <button type="button" disabled={busy || blocked} onClick={() => { setEditingBilling(billing.accountId); setBillingForm(billingFormFrom(billing, eligible)); setBillingError(null); }}>{t("grant.edit")}</button>
                <button type="button" disabled={busy || blocked} onClick={() => { void action.run(send({ apiCreditBillingRemove: billing.accountId }), () => t("billing.cleared")); }}>{t("billing.clear")}</button>
              </span>
            </li>)}
          </ul>}
          <form className="grant-form billing-form" aria-labelledby={`${formId}-billing`} onSubmit={event => { event.preventDefault(); if (!busy) saveBilling(); }}>
            <p className="field-help">{t("billing.manual")}</p>
            {eligible.length === 0 && <p className="gate-reason" role="status">{t("billing.noAccount")}</p>}
            <label>{t("billing.account")}<select value={billingForm.accountId} aria-label={t("billing.accountAria")} aria-invalid={billingError === "account"} disabled={eligible.length === 0 || editingBilling !== null} onChange={event => setBillingForm({ ...billingForm, accountId: event.target.value })}>
              <option value="">{t("billing.accountPick")}</option>
              {eligible.map(item => <option key={item.id} value={item.id}>{privacyText(item.email ?? item.label, masked)}{item.plan ? ` · ${planLabel(item.provider, item.plan)}` : ""}</option>)}
            </select></label>
            <label>{t("billing.cycle")}<select value={billingForm.cycle} aria-label={t("billing.cycleAria")} onChange={event => setBillingForm({ ...billingForm, cycle: event.target.value as BillingCycle })}>
              <option value="monthly">{t("billing.cycle.monthly")}</option>
              <option value="annual">{t("billing.cycle.annual")}</option>
            </select></label>
            <label>{t("billing.next")}<input type="datetime-local" value={billingForm.nextBillingAt} aria-label={t("billing.nextAria")} aria-invalid={billingError === "date" || billingError === "past" || billingError === "tooFar"} onChange={event => setBillingForm({ ...billingForm, nextBillingAt: event.target.value })} /></label>
            <p className="field-help">{t("billing.zone", { zone: timeZoneLabel() })}</p>
            {billingError && <p className="gate-reason" role="alert">{t(BILLING_ERROR_KEYS[billingError])}</p>}
            <div className="grant-actions">
              <button type="submit" className="primary" disabled={busy || blocked || eligible.length === 0}>{action.pending ? <Busy /> : t("billing.save")}</button>
              {editingBilling !== null && <button type="button" onClick={() => { setEditingBilling(null); setBillingForm(billingFormFrom(null, freeEligible)); setBillingError(null); }}>{t("grant.form.cancel")}</button>}
            </div>
          </form>
        </details>}
        {origin && (deleting
          ? <div className="key-credit-danger" role="alertdialog" aria-label={t("apiKey.credit.delete.title")}>
            <strong>{t("apiKey.credit.delete.title")}</strong>
            <p>{t("apiKey.credit.delete.body", { name })}</p>
            {origin.accountIds.length > 1 && <p>{t("apiKey.credit.delete.bodyShared", { count: origin.accountIds.length, keys: origin.accountIds.map(id => keyIds.includes(id) ? keyName(id) : t("grant.keys.removed")).join(", ") })}</p>}
            <div className="grant-actions">
              <button type="button" onClick={() => setDeleting(false)}>{t("common.cancel")}</button>
              <button type="button" className="danger-button" disabled={blocked} onClick={remove}>{action.pending ? <Busy /> : t("apiKey.credit.delete.confirm")}</button>
            </div>
          </div>
          : <div className="key-credit-danger-entry"><button type="button" className="danger-button" disabled={blocked} onClick={() => setDeleting(true)}>{t("apiKey.credit.delete")}</button></div>)}
      </fieldset>
      <ActionFeedback error={action.error} message={action.message} />
    </div>
    <div className="modal-footer">
      <button type="button" disabled={action.pending} onClick={onClose}>{t("common.cancel")}</button>
      <button type="submit" form={formId} className="primary" disabled={busy || blocked}>{action.pending ? <Busy /> : t(editing ? "grant.save.edit" : "grant.save")}</button>
    </div>
  </Modal>;
}
