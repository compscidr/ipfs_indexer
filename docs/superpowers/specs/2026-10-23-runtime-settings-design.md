# Runtime settings from the admin (#304)

## Why
Today every indexer setting is an environment variable set by iac, so changing one means an iac
PR, a merge and a redeploy. The #300 spike showed why that hurts: the Chinese Wikipedia crawl is
limited by how long each page block takes to arrive (about 13 s), so the worker count is the
lever, and it needs tuning while watching the crawl rate, not one redeploy per guess.

The aim: every setting an operator might tune can be seen and changed from darksearch's admin.
Each one shows where its value came from, and an admin change can be reset. Along the way the
admin should finally show what the local denylist contains; today it can add and remove
takedowns from a report, but nothing lists them.

## Goals
- See and change the indexer's tunable settings from darksearch's admin, and the values that are
  fixed in the code today but are operational knobs (listed below).
- Set `ETH_RPC_URL` from the admin without the page ever showing its value.
- See and edit the discovery seeds from the admin, on top of iac's list.
- See every local denylist entry in the admin, and undo one from there.
- A setting that only takes effect at startup restarts the indexer when it's saved.

## Non-goals
- `DATA_DIR`, `BIND_ADDR`, and the paths `SEEDS_FILE` and `LOCAL_DENYLIST`: they're tied to the
  Docker volume, port and mounted files iac sets up. They stay iac-only.
- `ADMIN_TOKEN`: darksearch holds the same token from its own iac config, so changing it on the
  page would lock the admin out. It stays iac-only.
- Index internals (segment flush sizes, merge fan-out, checksum constants), search scoring (BM25
  weights, the top-1000 cutoff), and the badbits URL. A bad value buys nothing and could hurt the
  index, or quietly switch off the takedown list.
- Editing the badbits list. The admin may show its entry count; it's an external list.

## Anti-goals (a failure even if it works)
- **A secret is readable anywhere:** on the page, in `/admin/*` responses, or in logs. `ETH_RPC_URL` contains the Alchemy key.
- **An iac deploy silently undoes an admin change, or an admin change silently fights iac.**
  Where each value comes from must always be visible.
- **A bad value takes the indexer down or into a restart loop.** Values are validated before
  they're saved, and a value that fails at startup must not keep it from starting.
- **A takedown becomes weaker than today.** Showing and editing the denylist must not open a way
  to serve or crawl something on it.

## Precedence (decided)
For each setting: **admin override, then the environment (iac), then the built-in default.**
- Overrides are stored in `index.sqlite` and survive restarts and iac deploys.
- Each override records who set it (darksearch's signed-in tailnet login, from
  `Tailscale-User-Login`, passed through by darksearch) and when.
- Any override can be reset, which falls back to the env value, or to the default.
- The page shows, for each setting: the effective value, its source (`admin`, `iac` or
  `default`), what iac and the default would give, and for an override who set it and when.

## The settings
Existing environment variables, now changeable from the admin:

| Setting | Env var | Default | iac sets |
|---|---|---|---|
| Crawl workers | `CRAWL_WORKERS` | 10 | 60 |
| Gateway requests per minute | `GATEWAY_REQUESTS_PER_MIN` | 0 (no limit) on our own node, 100 on a public gateway | — |
| New sites per hour | `DISCOVERY_PER_HOUR` | 500 (0 = no limit) | — |
| Health minimum crawl rate | `HEALTH_MIN_RATE` | 500 (0 = off) | — |
| Page max age, days | `INDEX_MAX_AGE_DAYS` | 30 | — |
| Index cache, MB | `INDEX_CACHE_MB` | 1024 | — |
| ENS: blocks per log request | `ETH_LOGS_MAX_BLOCKS` | 10,000 | 10 |
| ENS: request timeout, s | `ETH_REQUEST_SECS` | 1 | 6 |
| ENS RPC URL (secret) | `ETH_RPC_URL` | none | the Alchemy URL |

Ranges must include every default and every value iac sets today (above), including the 0s that
mean "no limit" or "off", so the new validation can't fail an existing deploy.

Fixed in the code today, now settings (the env var is new, and the default is today's value):

| Setting | Today | Where |
|---|---|---|
| Pause: recent fetches looked at | 20 | `breaker.rs` `WINDOW` |
| Pause: failures among them that pause a site | 16 | `breaker.rs` `TRIP` |
| Pause: shortest | 60 s | `breaker.rs` `MIN_PAUSE` |
| Pause: longest | 15 min | `breaker.rs` `MAX_PAUSE` |
| Tries before a page is dropped | 3 | `index_queue.rs` `MAX_ATTEMPTS` |
| Page fetch timeout | 90 s | `index_queue.rs` client |
| Largest page read | 2 MB | `index_queue.rs` `MAX_BODY_BYTES` |
| Site name lookup timeout | 30 s | `discovery.rs` client |
| Health grace after startup | 15 min | `main.rs` `HEALTH_GRACE` |

Each new one also gets an env var, so iac can set it like the others (the names are the builder's
call; follow the existing style, for example `PAUSE_WINDOW`, `PAGE_ATTEMPTS`, `PAGE_TIMEOUT_SECS`).

### Validation
Every setting has a type and an allowed range. A value outside it is refused with a 400 that
says why, and nothing changes. Cross-field rules are checked too: failures to pause ≤ fetches
looked at, and shortest pause ≤ longest. The same rules apply to env values at startup. Today a
bad env value is fatal, and it stays fatal. `INDEX_MAX_AGE_DAYS` is the exception today: it falls back
to 30 on a bad value. It becomes fatal like the others (my call: iac sets nothing for it, so no
deploy changes). But a stored admin override that fails validation at
startup (a range tightened by a later version, say) is logged, ignored, and shown on the page as
"ignored: …". It must not stop the indexer from starting. The exact ranges are the builder's
call: wide enough not to get in the way, narrow enough that a typo can't hurt (no 0 workers, no
1 s page timeout, no 100,000 workers).

### Live or restart
- **Live:** workers, gateway rate, discovery per hour, health minimum and grace, max age, pause
  rule, tries, page timeout, max page size, and name lookup timeout. These take effect on the
  next use, with no restart.
  - Workers grow or shrink without a restart. A worker that's no longer needed finishes its
    current page and stops.
  - A pause rule change applies to sites as their pause next opens or closes. Breakers already
  running needn't be rebuilt.
- **Restart:** the index cache size (SQLite's cache is set at open), and the ENS settings and RPC
  URL (the ENS thread starts once). The builder may make any of these live instead if it's
  simple.
- The page marks restart settings before you save. Saving one returns once the override is
  stored, then the indexer exits cleanly and Docker's `restart_policy: unless-stopped` starts it
  again. Startup is about 3 s since v0.5.6. The page says it's restarting, and offers a reload.

## Seeds (decided)
- iac's list (`SEEDS_FILE`, rewritten on every deploy) is the base.
- Seeds added or removed in the admin are stored in `index.sqlite` on top of it and survive
  deploys.
- The page lists the effective seeds, each marked "from iac", "added by admin" or "removed by
  admin", with who and when for admin changes. Reset undoes the admin change for that seed.
- An admin-added seed is validated like a seeds-file line.
- The effective list feeds discovery exactly as the file does today: added and forgotten as
  today's hourly re-read does (`add_seeds`, `forget_seeds`). An admin change applies straight
  away, without waiting for the hourly re-read.
- With no `SEEDS_FILE`, the iac base is empty and admin seeds still work, so the seeds thread runs
  whenever there's a store (my call). Without `DATA_DIR`, the seeds endpoints return the same 400 as
  the settings endpoints.
- Removing a seed means the same as taking it out of the file today: it's forgotten. A removal
  that iac's list later drops too stays recorded but has no effect.

## Local denylist (decided)
- The file is already admin-owned. iac creates it empty once (`force: false`) and never
  overwrites it. Its contents stay in that file, and admin takedowns keep appending to it as
  today. No second store.
- New: the admin lists every line of the file, in file order, as written. A line whose item
  `/admin/undeny` accepts (a CID or cid/path) gets an "undo" that calls `/admin/undeny` exactly as
  today, which removes every line naming that item. Other lines (comments, `//<hex>` hash entries,
  anything `parse` skips) are shown read-only with no undo (my call: no new removal path, so a
  takedown can't get weaker).
- The badbits list is not listed. The page shows its entry count and when it was loaded.
- No new way to edit the file: adding stays the existing `/admin/deny`, from a report or a
  CID typed into the admin.

## No change history (my call)
The page shows only the current state: one row per override, with who set it and when. There's
no history table or history page. Each set or reset is logged (`Setting <name> set to <value> by
<actor>`, or `reset by <actor>`; for the secret, only `ETH RPC URL changed by <actor>`), so the
indexer's log is the record.

## Secrets
- `ETH_RPC_URL` is write-only. It can be set, replaced, or reset to iac's value.
- Every response shows only whether it's set and where it came from (`set (admin, by …, …)` or
  `set (iac)`), never the value or any part of it.
- The value is stored in the settings table like any other override. That is acceptable: it is
  readable only on the server, where iac's env value already lives. Logs record only "ETH RPC URL
  changed by …".

## The indexer's admin API
All are behind the existing `ADMIN_TOKEN` check and absent without it. Without `DATA_DIR`
there's nowhere to persist overrides: the settings endpoints return 400 "settings need DATA_DIR",
and the read endpoint still shows env and default values. The shapes are the builder's call. The
existing endpoints use `text/plain` for simplicity and darksearch parses them, but JSON is fine
for these. They need:
- **Read all settings:** each with its effective value (secret: set or not), source, the env and
  default values, live or restart, the allowed range, and who/when for an override.
- **Set a setting:** value plus actor. Validated. Returns whether a restart follows.
- **Reset a setting:** plus actor.
- **Read seeds:** effective list with marks. **Add** and **remove** a seed, **reset** one.
- **Read the local denylist:** entries as written. Undo uses the existing `/admin/undeny`.
- The actor is passed by darksearch, for example in an `X-Admin-Actor` header. The indexer
  trusts it because the request already carries the admin token.

`/status` stays as it is.

## darksearch
- A new **Settings** page in the admin, listing every setting from the read endpoint. For each:
  the effective value and its source, the iac and default values, edit, reset, a restart marker,
  and who/when.
  - A secret shows "set" or "not set" with a replace field that's never pre-filled, and a reset.
- A **Seeds** section, on the same page or its own: the list with marks, add, remove, and reset.
- A **Takedowns** section listing the local denylist with an undo for each, plus the badbits
  count.
- It follows the existing admin pages' layout, `adminActor()` guard and `adminIndexer()` calls.
- After a save that restarts, it says so. The indexer may be unreachable for a few seconds, which
  is the existing "Indexer unreachable" case.

## iac
- No change is required: env values become the "iac" layer.
- Any new env var can be set later only if wanted.

## Constraints
- Never log, return or show `ETH_RPC_URL`'s value. Log redaction already exists on the server; don't rely on it.
- Settings live in `index.sqlite`. No WAL mode and no backups, as with the rest of the store.
- Follow the repo's patterns: rusqlite schema in `store.rs` (with a migration for existing
  stores), admin routes in `admin.rs` behind `authorized`, and tests next to the code.
- A version bump, and a README section on the settings and their precedence.
- Ship the indexer and darksearch changes as separate PRs. The indexer goes first and is
  backwards compatible: the old admin keeps working.

## Left to the builder
The exact env var names for the new settings, the ranges, the endpoint shapes, the table layout,
how workers are resized, and whether any restart setting can reasonably be live.
