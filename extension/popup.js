const $status = document.getElementById("status");
const $accounts = document.getElementById("accounts");
const $updated = document.getElementById("updated");
const $refresh = document.getElementById("refresh");

function pctFromWindow(w) {
  if (!w) return null;
  const u = typeof w.utilization === "number" ? w.utilization : null;
  if (u == null) return null;
  return u <= 1 ? u * 100 : u;
}

function fmtPct(v) {
  return v == null ? "—" : `${Math.round(v)}%`;
}

function fmtResets(iso) {
  if (!iso) return "";
  const d = new Date(iso);
  const now = Date.now();
  const diff = d - now;
  if (diff <= 0) return "now";
  const h = diff / 3_600_000;
  if (h < 1) return `${Math.round(diff / 60_000)}m`;
  if (h < 48) return `${Math.round(h)}h`;
  return `${Math.round(h / 24)}d`;
}

function row(label, win) {
  const v = pctFromWindow(win);
  const cls = v == null ? "" : v >= 100 ? "hot" : v >= 80 ? "warn" : "";
  const resets = fmtResets(win?.resets_at);
  const lab = resets ? `${label} · ${resets}` : label;
  return `
    <div class="row">
      <span class="label">${lab}</span>
      <span class="bar ${cls}"><span style="width:${Math.min(100, v ?? 0)}%"></span></span>
      <span class="pct">${fmtPct(v)}</span>
    </div>`;
}

function render(state) {
  $status.textContent = state.error || "";
  $accounts.innerHTML = "";
  const snaps = state.snapshots || [];
  if (!snaps.length && !state.error) {
    $accounts.innerHTML = `<div style="padding:8px;color:#8a8a8e">No data yet. Make sure you're logged into claude.ai.</div>`;
  }
  for (const s of snaps) {
    const name = s.email || s.org_name || s.org_uuid;
    if (s.error) {
      $accounts.insertAdjacentHTML("beforeend",
        `<div class="account"><div class="email">${name}</div><div style="color:#b00020;font-size:11px">${s.error}</div></div>`);
      continue;
    }
    const u = s.usage || {};
    $accounts.insertAdjacentHTML("beforeend", `
      <div class="account">
        <div class="email">${name}</div>
        ${row("5-hour", u.five_hour)}
        ${row("7-day", u.seven_day)}
        ${u.seven_day_sonnet ? row("7d Sonnet", u.seven_day_sonnet) : ""}
        ${u.seven_day_opus ? row("7d Opus", u.seven_day_opus) : ""}
      </div>`);
  }
  if (state.updated_at) {
    const ago = Math.max(0, Math.round((Date.now() - state.updated_at) / 1000));
    $updated.textContent = `updated ${ago}s ago`;
  } else {
    $updated.textContent = "";
  }
}

async function load() {
  const state = await chrome.storage.local.get(["snapshots", "error", "updated_at"]);
  render(state);
}

$refresh.addEventListener("click", async () => {
  $refresh.disabled = true;
  try {
    await chrome.runtime.sendMessage({ type: "refresh" });
    await load();
  } finally {
    $refresh.disabled = false;
  }
});

load();
