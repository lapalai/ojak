import { createContext, useContext, useEffect, useId, useRef } from "react";
import type { CSSProperties, ReactNode } from "react";
import { AlertCircle, CheckCircle2, LoaderCircle, X } from "lucide-react";
import type { ApiError } from "./types";
import { privacyText, providerColor } from "./state";
import { t } from "./i18n";

export const PrivacyContext = createContext(false);

export function Badge({ children, tone = "neutral" }: { children: ReactNode; tone?: string }) {
  return <span className={`tag ${tone}`}>{children}</span>;
}

/// 공급자 색 점. `provider`는 계정 저장소의 provider 값이다.
export function Dot({ provider, color, className }: { provider?: string; color?: string; className?: string }) {
  return <span className={`dot ${className ?? ""}`} style={{ "--c": color ?? providerColor[provider ?? ""] ?? "var(--tertiary)" } as CSSProperties} aria-hidden="true" />;
}

export function Switch({ checked, disabled, label, onChange }: { checked: boolean; disabled?: boolean; label: string; onChange: (next: boolean) => void }) {
  return <button type="button" className="switch" role="switch" aria-checked={checked} aria-label={label} disabled={disabled} onClick={() => onChange(!checked)} />;
}

/// 5분 단위 요청 수를 잇는 작은 선 그래프. `peak`은 화면 전체의 최대값이라 셀끼리 비교할 수 있다.
export function Sparkline({ series, peak, provider, width = 120, height = 24 }: { series: number[]; peak: number; provider: string; width?: number; height?: number }) {
  if (series.length < 2) return null;
  const step = width / (series.length - 1);
  const scale = Math.max(1, peak);
  const points = series.map((value, index) => `${(index * step).toFixed(1)},${(height - 2 - (value / scale) * (height - 4)).toFixed(1)}`).join(" ");
  const color = providerColor[provider] ?? "var(--tertiary)";
  return <svg viewBox={`0 0 ${width} ${height}`} preserveAspectRatio="none" aria-hidden="true">
    <polygon points={`0,${height} ${points} ${width},${height}`} fill={color} opacity=".16" />
    <polyline points={points} fill="none" stroke={color} strokeWidth="1.6" vectorEffect="non-scaling-stroke" />
  </svg>;
}

export function ErrorMessage({ error }: { error: ApiError | null }) {
  const masked = useContext(PrivacyContext);
  if (!error) return null;
  return <div className="feedback feedback-error" role="alert"><AlertCircle size={16} /><div><strong>{privacyText(error.message, masked)}</strong><span className="error-code">{error.code}</span>{error.code === "POLICY_CONFLICT" && <span>{t("error.policyConflict")}</span>}</div></div>;
}

export function ActionFeedback({ error, message }: { error: ApiError | null; message: string | null }) {
  const masked = useContext(PrivacyContext);
  return <><ErrorMessage error={error} />{message && <div className="feedback feedback-success" role="status"><CheckCircle2 size={16} /><span>{privacyText(message, masked)}</span></div>}</>;
}

export function Busy({ label = t("common.checking") }: { label?: string }) {
  return <span className="inline-busy"><LoaderCircle size={14} className="spinner" />{label}</span>;
}

export function Modal({ title, subtitle, children, onClose, busy = false, wide = false }: {
  title: string; subtitle?: string; children: ReactNode; onClose: () => void; busy?: boolean; wide?: boolean;
}) {
  const dialog = useRef<HTMLDialogElement>(null);
  const titleId = useId();
  const descriptionId = useId();
  useEffect(() => {
    const previous = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    const element = dialog.current;
    if (element && !element.open) element.showModal();
    return () => { element?.close(); previous?.focus(); };
  }, []);
  return <dialog ref={dialog} className={`modal ${wide ? "modal-wide" : ""}`} aria-labelledby={titleId} aria-describedby={subtitle ? descriptionId : undefined}
    onCancel={event => { event.preventDefault(); if (!busy) onClose(); }}>
    <div className="modal-heading"><div><h2 id={titleId}>{title}</h2>{subtitle && <p id={descriptionId}>{subtitle}</p>}</div><button type="button" className="icon-button" aria-label={t("modal.close")} onClick={onClose} disabled={busy}><X size={18} /></button></div>
    {children}
  </dialog>;
}
