import type { MessageKey } from "./i18n.ts";
import type { AllocationMode } from "./types.ts";

export const ALLOCATION_MODES: readonly AllocationMode[] = ["smart", "priority", "expiry-balanced"];

/// 모르는 값(이전·이후 서비스)은 기본인 smart로 본다. 서비스도 누락 필드를 smart로 읽는다.
export function allocationModeOf(raw: unknown): AllocationMode {
  return raw === "priority" || raw === "expiry-balanced" ? raw : "smart";
}

/// 모드별 문구 키. 선택 상자·설명·말풍선·저장 안내가 같은 표를 쓴다.
export const MODE_TEXT = {
  smart: { label: "policy.mode.smart", desc: "policy.mode.smartDesc", set: "policy.mode.setSmart", tip: "policy.mode.smartTip", order: "policy.orderHint.smart", saved: "policy.orderSaved.smart", boost: "policy.expiringBoostDesc" },
  priority: { label: "policy.mode.priority", desc: "policy.mode.priorityDesc", set: "policy.mode.setPriority", tip: "policy.mode.priorityTip", order: "policy.orderHint.priority", saved: "policy.orderSaved.priority", boost: "policy.expiringBoostPriority" },
  "expiry-balanced": { label: "policy.mode.balanced", desc: "policy.mode.balancedDesc", set: "policy.mode.setBalanced", tip: "policy.mode.balancedTip", order: "policy.orderHint.balanced", saved: "policy.orderSaved.balanced", boost: "policy.expiringBoostBalanced" },
} as const satisfies Record<AllocationMode, Record<string, MessageKey>>;
