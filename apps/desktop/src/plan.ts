/// 계정의 플랜 표기. 서비스가 공식 CLI·omp에서 읽은 원래 값(`Account.plan`)을 사람이 읽는 이름으로 바꾼다.
/// 확인된 값만 바꾼다(출처는 docs/domains/adapters.md "플랜 표기"). 확인하지 못한 값은 원래 값을 그대로 보여 준다.
/// - Claude: 공식 `auth status`는 Max와 Ultra를 모두 `max`로 보고한다. 서비스가 프로필의 표시 이름으로 `Ultra`를 가려내 보낸다.
/// - Codex/omp(openai): `prolite`는 ChatGPT Pro $100(Pro 5x)이다. `pro`는 $100 이상의 어느 Pro 등급인지 이 값만으로 알 수 없어 `Pro`로 둔다.
const NAMES: Record<string, Record<string, string>> = {
  anthropic: { max: "Max", ultra: "Ultra", pro: "Pro", free: "Free", team: "Team", enterprise: "Enterprise" },
  openai: { prolite: "Pro 100", pro: "Pro", plus: "Plus", go: "Go", free: "Free", team: "Team", business: "Business", enterprise: "Enterprise", edu: "Edu" },
};

export function planLabel(provider: string, raw: string): string {
  return NAMES[provider]?.[raw.trim().toLowerCase()] ?? raw;
}
