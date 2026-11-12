import { useState } from "react";
import { ShieldCheck } from "lucide-react";
import { rpc } from "./api";
import { ActionFeedback, Badge, Busy, Modal, Switch } from "./components";
import { API_KEY_PROVIDERS, apiAccounts, apiCreditAmount, apiCreditNotes, apiCreditSpend, apiKeyAddParams, apiKeyCap, apiKeyProvider, apiKeyTotals } from "./api-credit";
import { privacyText, useAction } from "./state";
import { t } from "./i18n";
import type { Account, ApiKeyProvider, Snapshot } from "./types";
import { usdUsed } from "./limits";

/// 키를 받는 대화상자. 키는 입력 중에만 이 컴포넌트 상태에 있고, 제출하는 순간(성공·실패 모두) 상태에서 지운다. 화면에 다시 보여 주지 않는다.
function AddApiKeyDialog({ online, onReload, onClose }: { online: boolean; onReload: () => Promise<void>; onClose: () => void }) {
  const action = useAction();
  const [label, setLabel] = useState("");
  const [key, setKey] = useState("");
  const [provider, setProvider] = useState<ApiKeyProvider>("anthropic");
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

/// 연결 화면의 API 키 목록(Claude Console·OpenAI Platform). 구독 계정 목록과 섞지 않는다(`tool === "api"`). 키마다 공급자를 보여 주고, 이번 달 합계는 공급자별로 따로 낸다.
/// 키마다 배정 포함(켜기·끄기)·이름 바꾸기·제거를 할 수 있다. 키 값은 다시 보여 주지 않는다.
export function ApiKeyPanel({ snapshot, masked, online, onReload }: { snapshot: Snapshot; masked: boolean; online: boolean; onReload: () => Promise<void> }) {
  const [adding, setAdding] = useState(false);
  const [removing, setRemoving] = useState<Account | null>(null);
  const [renaming, setRenaming] = useState<{ account: Account; label: string } | null>(null);
  const removeAction = useAction();
  const updateAction = useAction();
  const keys = apiAccounts(snapshot.accounts);
  const totals = apiKeyTotals(snapshot);
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
    {keys.length ? <table><tbody>{keys.map(account => {
      const provider = apiKeyProvider(account);
      const cap = apiKeyCap(account, totals.anthropic.limitUsd, snapshot.generatedAt);
      const rejected = account.authStatus !== "authenticated";
      const name = privacyText(account.label, masked);
      return <tr key={account.id} className={account.enabled ? undefined : "muted"}>
        <td>{name} <Badge tone={apiKeyProvider(account) === "openai" ? "accent" : "neutral"}>{t(apiKeyProvider(account) === "openai" ? "apiKey.provider.openai" : "apiKey.provider.anthropic")}</Badge></td>
        <td className="muted"><code>{account.identityKey ?? "apikey:…"}</code></td>
        <td className="muted">{provider === "openai" ? t("apiKey.usage", { used: usdUsed(cap.usedUsd) }) : <>{apiCreditAmount(cap.usedUsd, cap.limitUsd)}{cap.reached && <> · <strong>{t("apiKey.capReached")}</strong></>}</>}{cap.unmeteredRequests > 0 && <> · {t(provider === "openai" ? "apiKey.unmetered.openai" : "apiKey.unmetered")}</>}</td>
        <td className="num">
          {rejected && <Badge tone="warning">{t("apiKey.rejected")}</Badge>}
          <Switch checked={account.enabled} label={t("apiKey.enable.aria", { name })} disabled={!online || updateAction.pending}
            onChange={next => update(account, { enabled: next }, t(next ? "apiKey.enabled" : "apiKey.disabled", { name }))} />
          <button type="button" className="button" disabled={!online || updateAction.pending} aria-label={t("apiKey.rename.aria", { name })} onClick={() => { updateAction.clear(); setRenaming({ account, label: account.label }); }}>{t("apiKey.rename")}</button>
          <button type="button" className="button" disabled={!online || removeAction.pending} aria-label={t("apiKey.remove.aria", { name })} onClick={() => { removeAction.clear(); setRemoving(account); }}>{t("common.remove")}</button>
        </td>
      </tr>;
    })}</tbody></table> : <div className="empty">{t("apiKey.empty")}</div>}
    {!removing && !renaming && <ActionFeedback error={removeAction.error ?? updateAction.error} message={removeAction.message ?? updateAction.message} />}
    {API_KEY_PROVIDERS.filter(item => totals[item].keys > 0).map(item => <p key={item} className="field-help api-key-total">{item === "openai" ? t("apiKey.total", { provider: t("apiKey.provider.openai"), amount: apiCreditAmount(totals[item].usedUsd, totals[item].limitUsd) }) : t("apiKey.total.anthropic", { provider: t("apiKey.provider.anthropic"), amount: apiCreditSpend(totals[item].usedUsd) })}</p>)}
    {totals.anthropic.keys > 0 && <p className="field-help">{t("apiKey.sharedBalance")}</p>}
    {totals.openai.keys > 0 && <p className="field-help">{t("apiKey.openaiNote")}</p>}
    <div className="row-actions"><span className="hint">{t("apiKey.hint")}</span><button type="button" className="button" disabled={!online} onClick={() => setAdding(true)}>{t("apiKey.add")}</button></div>
    {adding && <AddApiKeyDialog online={online} onReload={onReload} onClose={() => setAdding(false)} />}
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
