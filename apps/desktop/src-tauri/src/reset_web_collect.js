// Runs only inside the account's isolated, authenticated claude.ai WebView.
// No credentials leave the browser. Only account binding and reset-program data leave this function.
(organizationId, slot) => {
  if (Object.prototype.hasOwnProperty.call(globalThis, slot)) return;
  const state = { done: false, result: null };
  Object.defineProperty(globalThis, slot, { value: state, configurable: true });
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 12000);
  const fail = code => { throw { code }; };
  const get = async path => {
    const response = await fetch(path, {
      method: "GET", credentials: "same-origin", redirect: "error", cache: "no-store",
      headers: { Accept: "application/json" }, signal: controller.signal,
    });
    if (response.status === 401 || response.status === 403) fail("WEB_LOGIN_REQUIRED");
    if (!response.ok) fail("WEB_FETCH_FAILED");
    if (!response.headers.get("content-type")?.includes("application/json")) fail("WEB_RESPONSE_INVALID");
    const reader = response.body.getReader();
    const chunks = []; let size = 0;
    while (true) {
      const { done, value } = await reader.read();
      if (done) break;
      size += value.byteLength;
      if (size > 1048576) { await reader.cancel(); fail("WEB_RESPONSE_INVALID"); }
      chunks.push(value);
    }
    const bytes = new Uint8Array(size); let offset = 0;
    for (const chunk of chunks) { bytes.set(chunk, offset); offset += chunk.byteLength; }
    try { return JSON.parse(new TextDecoder().decode(bytes)); } catch { fail("WEB_RESPONSE_INVALID"); }
  };
  const pick = (value, keys) => Object.fromEntries(keys.filter(key => Object.prototype.hasOwnProperty.call(value, key)).map(key => [key, value[key]]));
  const programs = value => {
    if (!value || typeof value !== "object" || Array.isArray(value)) fail("WEB_RESPONSE_INVALID");
    const result = {};
    if (Object.prototype.hasOwnProperty.call(value, "cedar_ember")) {
      const cedar = value.cedar_ember;
      if (cedar == null) result.cedar_ember = cedar;
      else {
        if (typeof cedar !== "object" || Array.isArray(cedar)) fail("WEB_RESPONSE_INVALID");
        result.cedar_ember = pick(cedar, ["eligible", "ineligible_reason", "cooldown_until", "next_grant_id"]);
        if (Object.prototype.hasOwnProperty.call(cedar, "grants")) {
          if (!Array.isArray(cedar.grants) || cedar.grants.length > 256) fail("WEB_RESPONSE_INVALID");
          result.cedar_ember.grants = cedar.grants.map(grant => {
            if (!grant || typeof grant !== "object" || Array.isArray(grant)) fail("WEB_RESPONSE_INVALID");
            return pick(grant, ["id", "type", "resets_left", "starts_at", "ends_at", "paused", "usable_now", "use_requires_limit", "gives_back_weekly", "clears", "blocking", "unavailable_reason"]);
          });
        }
      }
    }
    if (Object.prototype.hasOwnProperty.call(value, "juniper_tide")) {
      const juniper = value.juniper_tide;
      if (juniper == null) result.juniper_tide = juniper;
      else {
        if (typeof juniper !== "object" || Array.isArray(juniper)) fail("WEB_RESPONSE_INVALID");
        result.juniper_tide = pick(juniper, ["eligible", "ineligible_reason", "arm", "available", "weekly_resets_at", "next_available_at", "in_experiment"]);
      }
    }
    return result;
  };
  const identity = account => {
    if (!account || typeof account.uuid !== "string" || typeof account.email_address !== "string"
      || !Array.isArray(account.memberships)) fail("WEB_RESPONSE_INVALID");
    if (!account.memberships.some(row => row.organization?.uuid === organizationId)) fail("WEB_ACCOUNT_MISMATCH");
    return { accountUuid: account.uuid, email: account.email_address.trim().toLowerCase() };
  };
  void (async () => {
    try {
      if (location.origin !== "https://claude.ai" || !/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(organizationId)) fail("WEB_ACCOUNT_MISMATCH");
      const before = identity(await get("/api/account"));
      const base = `/api/organizations/${encodeURIComponent(organizationId)}/usage`;
      const values = await Promise.all([
        get(`${base}?cedar_ember=1&skip_spend=1`).then(programs),
        get(`${base}?at_wall=1&skip_spend=1`).then(programs),
      ]);
      const after = identity(await get("/api/account"));
      if (before.accountUuid !== after.accountUuid || before.email !== after.email) fail("WEB_ACCOUNT_MISMATCH");
      state.result = { ...after, organizationId, member: true, values };
      if (JSON.stringify(state.result).length > 131072) fail("WEB_RESPONSE_INVALID");
    } catch (error) {
      state.result = { errorCode: ["WEB_LOGIN_REQUIRED", "WEB_FETCH_FAILED", "WEB_RESPONSE_INVALID", "WEB_ACCOUNT_MISMATCH"].includes(error?.code) ? error.code : "WEB_FETCH_FAILED" };
    } finally {
      clearTimeout(timer); controller.abort(); state.done = true;
    }
  })();
}
