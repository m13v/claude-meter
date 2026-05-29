---
name: sentry-stats
description: Use when the user asks "check Sentry", "Sentry analytics", "Sentry stats", "how many users yesterday", "DAU", "WAU", "who's on what version", "version adoption", "release health", "release adoption", "crash free", or anything about Codex-meter usage from Sentry. Queries the mediar-n5/Codex-meter Sentry project for unique hosts, version breakdown, release adoption, and (post v0.3.1) session-based DAU + crash-free metrics.
user_invocable: true
---

# Sentry Stats (Codex-meter)

Pulls usage data from Sentry for the Codex-meter desktop app. Two query lanes:

1. **Events lane (always works)**: counts unique `tags[server_name]` (hostname) and `release` from events. Hostname is a proxy for "user" because the app does not set `sentry.User`. Only counts hosts that emitted at least one captured event (warn / error / capture_message).
2. **Sessions lane (works for v0.3.1+ only)**: real Release Health data from `auto_session_tracking`. Session tracking was added in v0.3.1; older versions emit zero sessions, so this lane is partial until all users update.

Prefer the sessions lane once `count_unique(user)` on a 24h window returns > 0; until then use the events lane.

## Constants

| Field | Value |
|---|---|
| Sentry org slug | `mediar-n5` |
| Sentry project slug | `Codex-meter` |
| Sentry project ID | `4511372322209792` |
| Auth token (keychain) | service `sentry-auth-token` (read-only, mediar-n5 org) |
| DSN (in source) | `src/bin/menubar.rs` `SENTRY_DSN` constant |

Pull the token at the start of every run:

```bash
TOKEN=$(security find-generic-password -s "sentry-auth-token" -w 2>/dev/null)
```

If that returns empty, fall through to the `auth` skill to recover it.

## Steps

### 1. Decide the time window

- "yesterday" -> compute UTC midnight bounds for yesterday's date
- "last N days" -> use `statsPeriod=Nd` (Sentry accepts `1h`, `24h`, `7d`, `14d`, `30d`, `90d`)
- specific dates -> use `start=YYYY-MM-DDTHH:MM:SS&end=...` in UTC

Today's date is in the user's AGENTS.md system reminder; always convert relative ("yesterday", "this week") to absolute UTC before querying.

### 2. Pick the right query

| Question | Query | Lane |
|---|---|---|
| How many unique users in window? | A | events |
| Who is on which version? | B | events |
| How many events total + which hosts noisiest? | C | events |
| Release adoption (real users per version) | D | sessions |
| Crash-free sessions / crash-free users | D | sessions |

### 3. Run the query and present a tight markdown table

Always include the caveat that hostname is a proxy, not a real user ID, when using the events lane.

---

## Query A: Unique hosts in window (DAU / WAU proxy)

```bash
TOKEN=$(security find-generic-password -s "sentry-auth-token" -w 2>/dev/null)
# Replace start/end as needed; example shown is yesterday in UTC.
curl -s -H "Authorization: Bearer $TOKEN" \
  "https://sentry.io/api/0/organizations/mediar-n5/events/?project=4511372322209792&start=2026-05-14T00:00:00&end=2026-05-15T00:00:00&field=tags%5Bserver_name%5D&field=count%28%29&sort=-count&per_page=100&dataset=errors" \
  | python3 -c "
import json, sys
d = json.load(sys.stdin)
rows = d.get('data', [])
print(f'Unique hosts: {len(rows)}')
print(f'Total events: {sum(r[\"count()\"] for r in rows)}')
print()
for r in rows:
    print(f'  {r[\"count()\"]:>5}  {r.get(\"tags[server_name]\") or \"(empty)\"}')"
```

## Query B: Host x version matrix

```bash
TOKEN=$(security find-generic-password -s "sentry-auth-token" -w 2>/dev/null)
# Use statsPeriod=7d for the last week; swap to start/end for a specific range.
curl -s -H "Authorization: Bearer $TOKEN" \
  "https://sentry.io/api/0/organizations/mediar-n5/events/?project=4511372322209792&statsPeriod=7d&field=release&field=tags%5Bserver_name%5D&field=count%28%29&sort=-count&per_page=100&dataset=errors" \
  | python3 -c "
import json, sys, collections
d = json.load(sys.stdin)
rows = d.get('data', [])
print(f'{len(rows)} (release, host) pairs, {sum(r[\"count()\"] for r in rows)} events')

by_release = collections.defaultdict(lambda: {'events': 0, 'hosts': set()})
for r in rows:
    rel = r.get('release') or '(unknown)'
    host = r.get('tags[server_name]') or '(empty)'
    by_release[rel]['events'] += r['count()']
    by_release[rel]['hosts'].add(host)
print()
print('By version:')
for rel in sorted(by_release):
    info = by_release[rel]
    print(f'  {rel:<28}  hosts={len(info[\"hosts\"]):>3}  events={info[\"events\"]:>5}')

hosts = collections.defaultdict(dict)
for r in rows:
    rel = r.get('release') or '(unknown)'
    host = r.get('tags[server_name]') or '(empty)'
    hosts[host][rel] = hosts[host].get(rel, 0) + r['count()']
print()
print('Host x version:')
for host in sorted(hosts, key=lambda h: -sum(hosts[h].values())):
    versions = ', '.join(f'{rel}={cnt}' for rel, cnt in sorted(hosts[host].items()))
    print(f'  {host:<40}  {versions}')"
```

## Query C: Top events / noisy hosts

```bash
TOKEN=$(security find-generic-password -s "sentry-auth-token" -w 2>/dev/null)
curl -s -H "Authorization: Bearer $TOKEN" \
  "https://sentry.io/api/0/organizations/mediar-n5/events/?project=4511372322209792&statsPeriod=24h&field=title&field=tags%5Bserver_name%5D&field=release&field=count%28%29&sort=-count&per_page=20&dataset=errors" \
  | python3 -m json.tool
```

## Query D: Release Health / Sessions (post v0.3.1 only)

```bash
TOKEN=$(security find-generic-password -s "sentry-auth-token" -w 2>/dev/null)
curl -s -H "Authorization: Bearer $TOKEN" \
  "https://sentry.io/api/0/organizations/mediar-n5/sessions/?project=4511372322209792&statsPeriod=7d&interval=1d&field=sum%28session%29&field=count_unique%28user%29&field=crash_free_rate%28session%29&field=crash_free_rate%28user%29&groupBy=release" \
  | python3 -m json.tool
```

If every group returns `sum(session) = 0`, no users on a session-enabled build (v0.3.1+) have launched in the window yet; fall back to events lane.

## Caveats to mention to the user

- **Events lane** counts hostnames as a proxy for users. Generic names (`MacBook-Air.local`, `Mac.lan`) can collide between different people, so the unique-host count is a *minimum*. Silent installs that never trigger a captured event are invisible.
- **Releases** are reported as `Codex-meter@<version>` because that is how `release:` is set in `src/bin/menubar.rs`. The leading prefix is meaningful for Sentry; do not strip it when filtering.
- **Environment** splits between `release` (production builds) and `debug` (cargo run from source). Add `&environment=release` to either query to exclude dev-machine traffic if needed.
- **One developer machine usually dominates the volume.** As of 2026-05-15 `MacBook-Pro-87.local` accounts for ~92% of events. Mention this in the summary so the user can mentally subtract it.

## Future improvements

- If the app starts setting `sentry::User` with a stable anonymous install ID (e.g., a UUID persisted to `~/Library/Application Support/Codex-meter/install_id`), swap the events lane to `count_unique(user)` and stop using hostname as a proxy.
- Releases data only becomes accurate once enough users upgrade past v0.3.1.

## Do NOT publish this skill

This skill hardcodes the `mediar-n5` org slug and the `Codex-meter` project ID. It is project-internal and lives in this workspace's `.Codex/skills/`. Do not run `publish-skill` against it; it is not useful to anyone outside this repo.
