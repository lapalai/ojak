import { errorCodeText } from "./i18n.ts";
import type { ApiError } from "./types.ts";

/// 오류 한 건을 화면에 보여 줄 모양. `text`가 주 문장이고 `details`가 있으면 접힌 '자세히'에 둔다.
export interface ErrorView {
  text: string;
  /// 표시 언어 문장으로 바꿨을 때만 값이 있다. 서비스·CLI가 보낸 원문 그대로이며 지원 문의에 쓴다.
  details: string | null;
}

/// 코드에 표시 언어 문장이 있으면 그것을 주 문장으로, 서비스 원문은 details로 옮긴다.
/// 코드가 사전에 없으면 지금까지처럼 원문이 주 문장이다. 원문이 비었거나 주 문장과 같으면 details를 만들지 않는다.
export function describeError(error: ApiError): ErrorView {
  const localized = errorCodeText(error.code);
  if (localized === null) return { text: error.message, details: null };
  return { text: localized, details: error.message.trim() && error.message !== localized ? error.message : null };
}
