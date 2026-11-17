import { useEffect, useState } from "react";
import { ShieldCheck } from "lucide-react";
import { rpc } from "./api";
import { ActionFeedback, Badge, Busy, Modal, Switch } from "./components";
import { ApiKeyCreditDialog } from "./ApiCreditGrantPanel";
import { API_KEY_PROVIDERS, apiAccounts, apiCreditAmount, apiCreditNotes, apiCreditSpend, apiKeyAddParams, apiKeyCap, apiKeyTotals } from "./api-credit";
import { keyCreditView } from "./credit-view";
import { fullTime, privacyText, timeZoneLabel, useAction } from "./state";
import { t } from "./i18n";
import type { Account, ApiCreditGrantState, ApiKeyProvider, Snapshot } from "./types";
import { usdUsed } from "./limits";

/// 키를 받는 대화상자. 키는 입력 중에만 이 컴포넌트 상태에 있고, 제출하는 순간(성공·실패 모두) 상태에서 지운다. 화면에 다시 보여 주지 않는다.
function AddApiKeyDialog({ initialProvider, online, onReload, onClose }: { initialProvider: ApiKeyProvider; online: boolean; onReload: () => Promise<void>; onClose: () => void }) {
  const action = useAction();
  const [label, setLabel] = useState("");
  const [key, setKey] = useState("");
  const [provider, setProvider] = useState<ApiKeyProvider>(initialProvider);
  const [confirmed, setConfirmed] = useState(false);
  const openai = provider === "openai";
  const ready = online && label.trim() !== "" && key.trim() !== "" && confirmed;
  const submit = () => {
    if (!ready) return;
    const submitted = key.trim();
    setKey("");
    void (async () => {
      if (await action.run(async () => {
        try { return await rpc<Account>("account.apiKeyAdd", apiKeyAddParams({ provider, label, key: submitted, confirmed })); }
        finally { await onReload(); }
      }, () => t("apiKey.added"))) onClose();
    })();
  };
  return <Modal title={t("apiKey.dialog.title")} subtitle={t("apiKey.dialog.subtitle")} busy={action.pending} onClose={onClose}>
    <form onSubmit={event => { event.preventDefault(); submit(); }}>
      <div className="modal-body form-fields">
        <label>{t("apiKey.provider")}<select value={provider} disabled={action.pending} onChange={event => { setProvider(event.target.value === "openai" ? "openai" : "anthropic"); setConfirmed(false); }}>{API_KEY_PROVIDERS.map(item => <option key={item} value={item}>{t(item === "openai" ? "apiKey.provider.openai" : "apiKey.provider.anthropic")}</option>)}</select></label>
        <label>{t("apiKey.label")}<input required maxLength={80} value={label} onChange={event => setLabel(event.target.value)} placeholder={t(openai ? "apiKey.labelPlaceholder.openai" : "apiKey.labelPlaceholder")} autoComplete="off" autoFocus /></label>
        <label>{t("apiKey.key")}<input required type="password" value={key} onChange={event => setKey(event.target.value)} placeholder={t(openai ? "apiKey.keyPlaceholder.openai" : "apiKey.keyPlaceholder")} autoComplete="off" autoCorrect="off" autoCapitalize="off" spellCheck={false} /></label>
        <label className="settings-confirm"><input type="checkbox" checked={confirmed} disabled={action.pending} onChange={event => setConfirmed(event.target.checked)} /><span>{t(openai ? "apiKey.confirm.openai" : "apiKey.confirm")}</span></label>
        <p className="field-help">{t(openai ? "policy.openaiApiGuideReload" : "policy.apiCreditGuideReload")}</p>
        {openai && <p className="field-help">{t("policy.openaiApiWarn")}</p>}
        <ActionFeedback error={action.error} message={null} />
      </div>
      <div className="modal-footer"><button type="button" disabled={action.pending} onClick={onClose}>{t("common.cancel")}</button><button type="submit" className="primary" disabled={!ready || action.pending}>{action.pending ? <Busy /> : t("apiKey.submit")}</button></div>
    </form>
  </Modal>;
}

const STATE_TONE: Record<ApiCreditGrantState, string> = { upcoming: "neutral", active: "good", exhausted: "warning", expired: "warning" };

/// 키 한 줄 아래의 크레딧 요약. 저장한 금액·기간(`policy.apiCreditGrants`)은 서비스 상태가 없어도 보이고, 서비스가 계산한 상태·남은 금액(`snapshot.apiCreditGrants`)이 없으면 0이 아니라 모른다고 적는다.
/// 여러 키가 한 풀을 쓰는 이전 입력은 공유 크레딧으로 표시하고, 남은 금액은 키별 잔액이 아니라 그 풀의 추정치다.
function KeyCredit({ account, snapshot }: { account: Account; snapshot: Snapshot }) {
  const view = keyCreditView(snapshot, account.id, snapshot.generatedAt);
  const grant = view.grant;
  if (!grant) return <div className={`api-key-credit${account.enabled ? " unset" : " muted"}`}>{t("apiKey.credit.unset")}</div>;
  const status = view.status;
  const expired = view.state === "expired";
  const showStart = status ? status.state === "upcoming" : true;
  return <div className="api-key-credit">
    {status ? <Badge tone={STATE_TONE[status.state]}>{t(`grant.state.${status.state}`)}</Badge> : <Badge>{t("apiKey.credit.statusUnknown")}</Badge>}
    {view.shared && <Badge tone="accent">{t("apiKey.credit.shared")}</Badge>}
    {showStart && <span>{t("grant.starts", { time: fullTime(grant.startsAt) })}</span>}
    <span className={expired ? "api-key-credit-expired" : undefined}>{t(expired ? "grant.expired" : "grant.expires", { time: fullTime(grant.expiresAt) })}{grant.expirySource === "billing-estimate" && <> <Badge tone="warning">{t("grant.estimateTag")}</Badge></>}</span>
    {view.remainingUsd !== null && (expired
      ? view.remainingUsd > 0 && <strong className="api-key-credit-expired">{t("apiKey.credit.lost", { amount: usdUsed(view.remainingUsd) })}</strong>
      : <span>{t("apiKey.credit.remaining", { amount: usdUsed(view.remainingUsd) })}</span>)}
  </div>;
}

/// 연결 화면의 API 키 목록. 공급자별로 목록·추가 버튼·이번 달 합계·과금 안내를 묶고 구독 계정과 섞지 않는다.
/// 키마다 배정 포함(켜기·끄기)·이름 바꾸기·제거를 할 수 있다. 키 값은 다시 보여 주지 않는다.
export function ApiKeyPanel({ snapshot, masked, online, onReload }: { snapshot: Snapshot; masked: boolean; online: boolean; onReload: () => Promise<void> }) {
  const [adding, setAdding] = useState<ApiKeyProvider | null>(null);
  const [removing, setRemoving] = useState<Account | null>(null);
  const [renaming, setRenaming] = useState<{ account: Account; label: string } | null>(null);
  const [creditId, setCreditId] = useState<string | null>(null);
  const removeAction = useAction();
  const updateAction = useAction();
  const totals = apiKeyTotals(snapshot);
  // 금액·기간을 보던 키가 다른 곳에서 지워지면 대화상자를 닫는다. 대화상자는 항상 방금 받은 스냅샷의 키로 연다.
  const creditAccount = creditId === null ? null : apiAccounts(snapshot.accounts).find(account => account.id === creditId) ?? null;
  useEffect(() => { if (creditId !== null && creditAccount === null) setCreditId(null); }, [creditId, creditAccount]);
  const remove = (account: Account) => {
    void removeAction.run(
      async () => { const result = await rpc<{ id: string }>("account.remove", { id: account.id }); setRemoving(null); await onReload(); return result; },
      () => t("apiKey.remove.done", { name: privacyText(account.label, masked) }),
    );
  };
  const update = (account: Account, change: { enabled?: boolean; label?: string }, done: string) => {
    void updateAction.run(
      async () => { const result = await rpc<Account>("account.update", { id: account.id, ...change }); setRenaming(null); await onReload(); return result; },
      () => done,
    );
  };
  const renameValid = renaming !== null && renaming.label.trim() !== "" && renaming.label.trim().length <= 120;
  return <div className="panel api-keys">
    <header><ShieldCheck size={15} className="muted" /><h2>{t("apiKey.title")}</h2></header>
    {API_KEY_PROVIDERS.map(provider => {
      const keys = apiAccounts(snapshot.accounts, provider);
      const openai = provider === "openai";
      const providerName = t(openai ? "apiKey.provider.openai" : "apiKey.provider.anthropic");
      return <section key={provider} className="api-key-provider" aria-labelledby={`api-key-${provider}`}>
        <div className="api-key-provider-heading">
          <h3 id={`api-key-${provider}`}>{providerName} <span className="muted">({keys.length})</span></h3>
          <button type="button" disabled={!online} aria-label={`${providerName} · ${t("apiKey.add")}`} onClick={() => setAdding(provider)}>{t("apiKey.add")}</button>
        </div>
        {keys.length ? <ul className="api-key-list">{keys.map(account => {
      const cap = apiKeyCap(account, totals.anthropic.limitUsd, snapshot.generatedAt);
      const rejected = account.authStatus !== "authenticated";
      const name = privacyText(account.label, masked);
      return <li key={account.id} className={`api-key-row${account.enabled ? "" : " muted"}`}>
        <div className="api-key-identity"><strong>{name}</strong><code className="muted">{account.identityKey ?? "apikey:…"}</code>{rejected && <Badge tone="warning">{t("apiKey.rejected")}</Badge>}</div>
        <div className="api-key-usage">{openai ? t("apiKey.usage", { used: usdUsed(cap.usedUsd) }) : <>{apiCreditAmount(cap.usedUsd, cap.limitUsd)}{cap.reached && <> · <strong>{t("apiKey.capReached")}</strong></>}</>}{cap.unmeteredRequests > 0 && <> · {t(openai ? "apiKey.unmetered.openai" : "apiKey.unmetered")}</>}</div>
        <div className="api-key-actions">
          <Switch checked={account.enabled} label={t("apiKey.enable.aria", { name })} disabled={!online || updateAction.pending}
            onChange={next => update(account, { enabled: next }, t(next ? "apiKey.enabled" : "apiKey.disabled", { name }))} />
          <button type="button" className="button" disabled={!online} aria-label={t("apiKey.credit.manageAria", { name })} onClick={() => setCreditId(account.id)}>{t("apiKey.credit.manage")}</button>
          <button type="button" className="button" disabled={!online || updateAction.pending} aria-label={t("apiKey.rename.aria", { name })} onClick={() => { updateAction.clear(); setRenaming({ account, label: account.label }); }}>{t("apiKey.rename")}</button>
          <button type="button" className="button" disabled={!online || removeAction.pending} aria-label={t("apiKey.remove.aria", { name })} onClick={() => { removeAction.clear(); setRemoving(account); }}>{t("common.remove")}</button>
        </div>
        <KeyCredit account={account} snapshot={snapshot} />
      </li>;
    })}</ul> : <div className="empty">{t("apiKey.empty")}</div>}
        {keys.length > 0 && <div className="api-key-provider-notes">
          <p className="api-key-total">{openai ? t("apiKey.total", { provider: providerName, amount: apiCreditAmount(totals[provider].usedUsd, totals[provider].limitUsd) }) : t("apiKey.total.anthropic", { provider: providerName, amount: apiCreditSpend(totals[provider].usedUsd) })}</p>
          <p className="field-help">{t(openai ? "apiKey.openaiNote" : "apiKey.sharedBalance")}</p>
          {(snapshot.policy.apiCreditGrants ?? []).some(grant => keys.some(key => grant.accountIds.includes(key.id))) && <p className="field-help">{t("usage.timezone", { zone: timeZoneLabel() })}</p>}
        </div>}
      </section>;
    })}
    {!removing && !renaming && <ActionFeedback error={removeAction.error ?? updateAction.error} message={removeAction.message ?? updateAction.message} />}
    <div className="row-actions"><span className="hint">{t("apiKey.hint")}</span></div>
    {adding && <AddApiKeyDialog initialProvider={adding} online={online} onReload={onReload} onClose={() => setAdding(null)} />}
    {creditAccount && <ApiKeyCreditDialog key={creditAccount.id} accountId={creditAccount.id} snapshot={snapshot} online={online} masked={masked} onReload={onReload} onClose={() => setCreditId(null)} />}
    {renaming && <Modal title={t("apiKey.rename.title")} onClose={() => setRenaming(null)} busy={updateAction.pending}>
      <form onSubmit={event => { event.preventDefault(); if (renameValid) update(renaming.account, { label: renaming.label.trim() }, t("apiKey.rename.done")); }}>
        <div className="modal-body form-fields">
          <label>{t("apiKey.label")}<input required maxLength={120} value={renaming.label} onChange={event => setRenaming({ ...renaming, label: event.target.value })} autoComplete="off" autoFocus /></label>
          <ActionFeedback error={updateAction.error} message={null} />
        </div>
        <div className="modal-footer"><button type="button" onClick={() => setRenaming(null)} disabled={updateAction.pending}>{t("common.cancel")}</button><button type="submit" className="primary" disabled={!online || !renameValid || updateAction.pending}>{updateAction.pending ? <Busy /> : t("apiKey.rename.submit")}</button></div>
      </form>
    </Modal>}
    {removing && <Modal title={t("apiKey.remove.title", { name: privacyText(removing.label, masked) })} onClose={() => setRemoving(null)} busy={removeAction.pending}>
      <div className="modal-body"><p className="body-copy">{t("apiKey.remove.body")}</p><ActionFeedback error={removeAction.error} message={null} /></div>
      <div className="modal-footer"><button type="button" onClick={() => setRemoving(null)} disabled={removeAction.pending}>{t("common.cancel")}</button><button type="button" className="danger-button" onClick={() => remove(removing)} disabled={!online || removeAction.pending}>{removeAction.pending ? <Busy /> : t("common.remove")}</button></div>
    </Modal>}
  </div>;
}

/// 사용 현황의 API 한 줄. 공급자마다(Claude·OpenAI) 따로, 지금 쓰는 중이거나 상한에 닿았을 때만 보이고 쓰는 중이면 강조한다. 금액은 그 공급자 키의 합계다. 서비스가 매 스냅샷마다 다시 계산한 값이다.
export function ApiCreditStatus({ snapshot }: { snapshot: Snapshot }) {
  return <>{apiCreditNotes(snapshot).map(note => <div key={note.provider} className={`notice api-credit-status${note.active ? " active" : ""}`} role="status">
    <p><b>{note.label}</b>{note.active && <> <span className="muted">{t(note.reasonKey)}</span></>}</p>
  </div>)}</>;
}
