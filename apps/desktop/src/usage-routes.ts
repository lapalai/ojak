import type { ObservedSession } from "./types";

export interface ObservedRouteModel {
  key: string;
  provider: string;
  model: string;
  route: "direct" | "unknown";
  projects: string[];
  roles: string[];
  observations: { project: string | null; lastRecordedAt: number }[];
}

/** Only explicit per-request evidence belongs in this panel; completion and selection rows carry no route. */
export function observedRouteModels(
  sessions: Pick<ObservedSession, "cwd" | "attributions">[],
  from: number,
  to: number,
  canonicalProvider: (provider: string) => string,
): ObservedRouteModel[] {
  const models = new Map<string, ObservedRouteModel>();
  for (const session of sessions) {
    for (const attribution of session.attributions ?? []) {
      if (!attribution.model || attribution.verification === "configured" || !Number.isFinite(attribution.recordedAt)
        || attribution.recordedAt < from || attribution.recordedAt > to
        || (attribution.route !== "direct" && attribution.route !== "unknown")) continue;
      const route = attribution.route;
      const provider = canonicalProvider(attribution.provider);
      const key = JSON.stringify([provider, attribution.model, route]);
      const cell: ObservedRouteModel = models.get(key) ?? { key, provider, model: attribution.model, route, projects: [], roles: [], observations: [] };
      const project = session.cwd || null;
      const observation = cell.observations.find(item => item.project === project);
      if (observation) observation.lastRecordedAt = Math.max(observation.lastRecordedAt, attribution.recordedAt);
      else cell.observations.push({ project, lastRecordedAt: attribution.recordedAt });
      if (session.cwd && !cell.projects.includes(session.cwd)) cell.projects.push(session.cwd);
      if (["main", "subagent", "auxiliary"].includes(attribution.role) && !cell.roles.includes(attribution.role)) cell.roles.push(attribution.role);
      models.set(key, cell);
    }
  }
  return [...models.values()];
}
