const BASE = "https://claude.ai";
const BRIDGE = "http://127.0.0.1:63762/snapshots";
const POLL_MINUTES = 1;

async function fetchJSON(url) {
  const r = await fetch(url, {
    credentials: "include",
    headers: { "accept": "application/json" },
  });
  if (!r.ok) throw new Error(`${r.status} ${r.statusText} @ ${url}`);
  return r.json();
}

async function fetchSnapshots() {
  const account = await fetchJSON(`${BASE}/api/account`);
  const email = account.email_address || null;
  const memberships = account.memberships || [];
  const results = [];
  for (const m of memberships) {
    const org = m.organization?.uuid || m.uuid;
    if (!org) continue;
    const errors = [];
    let usage = null, overage = null, subscription = null;
    try { usage = await fetchJSON(`${BASE}/api/organizations/${org}/usage`); }
    catch (e) { errors.push(`usage: ${e}`); }
    try { overage = await fetchJSON(`${BASE}/api/organizations/${org}/overage_spend_limit`); }
    catch (e) { /* overage may not exist for all orgs */ }
    try { subscription = await fetchJSON(`${BASE}/api/organizations/${org}/subscription_details`); }
    catch (e) { /* sub details may not exist */ }

    if (!usage) continue;
    results.push({
      org_uuid: org,
      browser: "extension",
      account_email: email,
      fetched_at: new Date().toISOString(),
      usage,
      overage,
      subscription,
      errors,
    });
  }
  return results;
}

async function postToBridge(snapshots) {
  try {
    await fetch(BRIDGE, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(snapshots),
    });
  } catch (_e) {
    // bridge may not be running; that's fine — popup still works
  }
}

function pctFromWindow(w) {
  if (!w) return null;
  const u = typeof w.utilization === "number" ? w.utilization : null;
  if (u == null) return null;
  return u <= 1 ? u * 100 : u;
}

function worstPct(snaps, key) {
  let worst = null;
  for (const s of snaps) {
    if (!s.usage) continue;
    const v = pctFromWindow(s.usage[key]);
    if (v != null && (worst == null || v > worst)) worst = v;
  }
  return worst;
}

async function refresh() {
  try {
    const snaps = await fetchSnapshots();
    await chrome.storage.local.set({ snapshots: snaps, error: null, updated_at: Date.now() });
    postToBridge(snaps);
    const five = worstPct(snaps, "five_hour");
    const seven = worstPct(snaps, "seven_day");
    const badge =
      five == null && seven == null ? "?" :
      `${Math.round(five ?? 0)}`;
    chrome.action.setBadgeText({ text: badge });
    chrome.action.setBadgeBackgroundColor({
      color: (five ?? 0) >= 100 ? "#b00020" : (five ?? 0) >= 80 ? "#b26a00" : "#2c6e2f",
    });
    chrome.action.setTitle({
      title: `ClaudeMeter\n5h: ${fmt(five)}\n7d: ${fmt(seven)}`,
    });
  } catch (e) {
    await chrome.storage.local.set({ error: String(e), updated_at: Date.now() });
    chrome.action.setBadgeText({ text: "!" });
    chrome.action.setBadgeBackgroundColor({ color: "#666" });
    chrome.action.setTitle({ title: `ClaudeMeter: ${e}` });
  }
}

function fmt(pct) {
  return pct == null ? "n/a" : `${Math.round(pct)}%`;
}

chrome.runtime.onInstalled.addListener(() => {
  chrome.alarms.create("refresh", { periodInMinutes: POLL_MINUTES });
  refresh();
});
chrome.runtime.onStartup.addListener(() => {
  chrome.alarms.create("refresh", { periodInMinutes: POLL_MINUTES });
  refresh();
});
chrome.alarms.onAlarm.addListener((a) => {
  if (a.name === "refresh") refresh();
});
chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg?.type === "refresh") {
    refresh().then(() => sendResponse({ ok: true })).catch((e) => sendResponse({ ok: false, error: String(e) }));
    return true;
  }
});
