# Discovery: seeds, DNSLink, ENS and submitted names (#213, part 1): implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The indexer finds new IPFS sites on its own, from a seeds file, ENS `.eth` records and names submitted to `/enqueue`. It resolves names to root CIDs through the gateway, queues them under an hourly cap, and checks them again daily.

**Architecture:**
- A new `discovered` table holds every entry: a CID or a name, its source, the last root queued, when it's next due, and its failures.
- Three background threads feed or drain it:
  - **seeds:** reads `SEEDS_FILE` hourly;
  - **ENS:** scans `ContenthashChanged` logs through `ETH_RPC_URL`, with an adaptive block range;
  - **resolver:** always runs. It takes due entries one a second, resolves names via `GET /ipns/<name>/` and its `X-Ipfs-Roots` header, and queues roots through `enqueue()`.
- `/enqueue` accepts names, and `/discovered` lists entries.

**Tech Stack:** Rust 2021, rusqlite, reqwest (blocking), `cid` 0.11 (with `multibase` and `multihash`), and `serde_json`, newly listed: it's already in `Cargo.lock` through actix. actix-web tests.

**Spec:** `docs/superpowers/specs/2026-10-17-discovery-design.md`

## Global Constraints

- **Every root goes through `IndexQueue::enqueue()`:** CID checks, both denylists and the HTML-only crawl. Nothing new fetches page content. The resolver reads only headers.
- **Sources are off until configured:** no `SEEDS_FILE` means no seeds, and no `ETH_RPC_URL` means no ENS. The resolver always runs.
- **`ETH_RPC_URL` carries an API key.** Never log it or put it in an error: log only its host, and use `reqwest::Error::without_url()`.
- **Values:**
  - ENS starts at block 3,327,417, with topic `0xe379c1624ed7e714cc0937528a32359d69d5281337765313dba4e081b72d7578`;
  - the block range starts at 10,000, halves on a JSON-RPC error down to 1, and doubles on success up to 10,000;
  - 1 s between ENS requests, and a 10-minute wait once caught up;
  - after an ENS error, wait 1 minute, doubling up to 1 hour;
  - the resolver checks at most 1 entry a second, with a 30 s timeout per name;
  - an entry that worked is due again after 1 day; after a failure, 1 hour doubling up to 1 day;
  - `DISCOVERY_PER_HOUR` defaults to 500, and `0` means no limit.
- **The hourly cap counts new roots only:** a root different from the entry's last one. An unchanged root is queued again without counting, which re-crawls a site once its pages expire. A capped entry is postponed to the next hour and isn't counted as checked.
- **`/status`** gains `Discovered: <n> ` just before `Gateway:`. darksearch finds fields by label.
- **`/discovered`** lists one line per entry: entry, source, root, checked, error, tab-separated, with `-` for none, most recently checked first.
- **`/enqueue` with a name** returns `200 Queued <name> to resolve` or `200 Already known: <name>`. An invalid item returns `400 Not a valid CID or name: <item>`. CIDs behave as before.
- **Release:** v0.5.0.
- **Quality bar:** `cargo fmt` clean, all tests pass. The only new dependency is `serde_json = "1"`.
- **Commit messages** end with `Co-Authored-By: <the model that wrote the commit> <noreply@anthropic.com>`.

## Review Focus

1. **An ENS record pointing at an entry another source already has,** e.g. a seeded `.eth` site whose node later changes. The seeds entry must not be deleted, only lose its node. Test: Task 1, `an_ens_node_keeps_only_its_latest_record`.
2. **A provider that rejects big ranges,** and keeps rejecting at 1 block. The first case shrinks the range and carries on. The second backs off and keeps the position, without spinning. Tests: Task 3, `the_range_halves_on_an_rpc_error_and_doubles_back` and `the_range_never_goes_below_one_block`; Task 4, `an_ens_pass_halves_and_carries_on_and_saves_its_position` and `a_failing_ens_pass_keeps_its_position`.
3. **The API key leaking into logs or `/discovered`** through a reqwest error. Test: Task 3, `http_errors_never_include_the_url`.
4. **The hourly cap used up by new roots** while known sites come due. Known sites still get queued; only new roots wait. Test: Task 4, `an_unchanged_root_is_queued_again_even_when_capped`.
5. **A name submitted twice, or in another case.** One entry, lowercased, with "Already known" the second time. Test: Task 5, `enqueue_records_names_for_the_resolver`.

---

### Task 1: The `discovered` and `discovery_state` tables (`src/store.rs`)

**Files:**
- Modify: `src/store.rs`:
  - `SCHEMA`: the queue table ends with `attempts INTEGER NOT NULL DEFAULT 0\n    );";`;
  - a new `Discovered` struct and helpers near `root_of`;
  - new `Store` methods after `queue_retry`;
  - tests in `mod tests`.

**Interfaces:**
- Produces:
  - `pub struct Discovered { pub entry: String, pub source: String, pub root: Option<String>, pub checked_at: Option<i64>, pub next_at: i64, pub failures: u32, pub error: Option<String> }`, deriving `Debug, Clone, PartialEq`;
  - `Store::discover(&self, entry: &str, source: &str, now: i64) -> rusqlite::Result<bool>`;
  - `Store::discover_ens(&self, node: &str, entry: Option<&str>, now: i64) -> rusqlite::Result<()>`;
  - `Store::discovered_due(&self, now: i64, limit: usize) -> rusqlite::Result<Vec<Discovered>>`;
  - `Store::discovered_ok(&self, entry: &str, root: &str, now: i64, next_at: i64) -> rusqlite::Result<()>`;
  - `Store::discovered_failed(&self, entry: &str, error: &str, now: i64, next_at: i64) -> rusqlite::Result<()>`;
  - `Store::discovered_postpone(&self, entry: &str, next_at: i64) -> rusqlite::Result<()>`;
  - `Store::discovered_all(&self) -> rusqlite::Result<Vec<Discovered>>`;
  - `Store::discovered_count(&self) -> usize`;
  - `Store::state(&self, key: &str) -> rusqlite::Result<Option<String>>`;
  - `Store::set_state(&self, key: &str, value: &str) -> rusqlite::Result<()>`.

- [ ] **Step 1: Write the failing tests** (in `src/store.rs` `mod tests`, after `a_queue_from_before_attempts_gains_the_column`)

```rust
    #[test]
    fn discovered_entries_round_trip_and_come_due() {
        let dir = temp_dir("discovered");
        let store = Store::open(&dir, 0).unwrap();
        assert!(store.discover("docs.ipfs.tech", "seeds", 100).unwrap());
        assert!(!store.discover("docs.ipfs.tech", "submitted", 50).unwrap(), "known already");
        assert!(store.discover("vitalik.eth", "submitted", 200).unwrap());
        let due = |store: &Store, now| -> Vec<String> {
            store
                .discovered_due(now, 10)
                .unwrap()
                .into_iter()
                .map(|d| d.entry)
                .collect()
        };
        assert_eq!(due(&store, 150), vec!["docs.ipfs.tech"]);
        store.discovered_failed("docs.ipfs.tech", "504", 150, 3750).unwrap();
        store.discovered_failed("docs.ipfs.tech", "504", 3750, 11000).unwrap();
        let docs = |store: &Store| {
            store
                .discovered_all()
                .unwrap()
                .into_iter()
                .find(|d| d.entry == "docs.ipfs.tech")
                .unwrap()
        };
        let row = docs(&store);
        assert_eq!(
            (row.failures, row.error.as_deref(), row.checked_at, row.next_at),
            (2, Some("504"), Some(3750), 11000)
        );
        store
            .discovered_ok("docs.ipfs.tech", "bafyroot", 11000, 97400)
            .unwrap();
        store.discovered_postpone("vitalik.eth", 14400).unwrap();
        assert!(due(&store, 14000).is_empty());
        assert_eq!(due(&store, 14400), vec!["vitalik.eth"]);
        drop(store);
        let store = Store::open(&dir, 0).unwrap();
        let all = store.discovered_all().unwrap();
        assert_eq!(
            all[0],
            Discovered {
                entry: "docs.ipfs.tech".into(),
                source: "seeds".into(),
                root: Some("bafyroot".into()),
                checked_at: Some(11000),
                next_at: 97400,
                failures: 0,
                error: None,
            },
            "checked first, the unchecked after"
        );
        assert_eq!(all[1].entry, "vitalik.eth");
        assert_eq!(store.discovered_count(), 2);
    }

    #[test]
    fn an_ens_node_keeps_only_its_latest_record() {
        let store = Store::open_in_memory().unwrap();
        let entries = |s: &Store| -> Vec<(String, String)> {
            s.discovered_all()
                .unwrap()
                .into_iter()
                .map(|d| (d.entry, d.source))
                .collect()
        };
        let pair = |e: &str, s: &str| (e.to_string(), s.to_string());
        store.discover_ens("0xnode", Some("bafyone"), 1).unwrap();
        store.discover_ens("0xnode", Some("bafytwo"), 2).unwrap();
        assert_eq!(entries(&store), vec![pair("bafytwo", "ens")]);
        store.discover_ens("0xnode", None, 3).unwrap(); // cleared
        assert!(entries(&store).is_empty());
        // a record pointing at an entry another source found keeps that entry's source, and
        // moving the record away leaves the entry in place
        store.discover("seeded.eth", "seeds", 4).unwrap();
        store.discover_ens("0xother", Some("seeded.eth"), 5).unwrap();
        assert_eq!(entries(&store), vec![pair("seeded.eth", "seeds")]);
        store.discover_ens("0xother", Some("bafythree"), 6).unwrap();
        assert_eq!(
            entries(&store),
            vec![pair("bafythree", "ens"), pair("seeded.eth", "seeds")]
        );
        store.discover_ens("0xother", None, 7).unwrap();
        assert_eq!(entries(&store), vec![pair("seeded.eth", "seeds")]);
    }

    #[test]
    fn a_store_from_before_discovery_gains_its_tables() {
        let dir = temp_dir("discovery-tables");
        drop(Store::open(&dir, 0).unwrap());
        Connection::open(dir.join("index.sqlite"))
            .unwrap()
            .execute_batch("DROP TABLE discovered; DROP TABLE discovery_state;")
            .unwrap();
        let store = Store::open(&dir, 0).unwrap();
        assert!(store.discover("docs.ipfs.tech", "seeds", 1).unwrap());
        assert_eq!(store.state("ens_next_block").unwrap(), None);
        store.set_state("ens_next_block", "3327417").unwrap();
        store.set_state("ens_next_block", "3337417").unwrap();
        drop(store);
        assert_eq!(
            Store::open(&dir, 0)
                .unwrap()
                .state("ens_next_block")
                .unwrap()
                .as_deref(),
            Some("3337417")
        );
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --quiet discover 2>&1 | grep -E "^error" | sort | uniq -c`
Expected: compile errors. `discover`, `Discovered` and the others don't exist yet.

- [ ] **Step 3: Implement**

In `SCHEMA`, replace the queue table's ending `        attempts INTEGER NOT NULL DEFAULT 0\n    );";` with:

```rust
        attempts INTEGER NOT NULL DEFAULT 0
    );
    -- names and CIDs found by discovery sources, resolved to roots and checked again (#213)
    CREATE TABLE IF NOT EXISTS discovered (
        entry      TEXT PRIMARY KEY,
        source     TEXT NOT NULL,
        -- the ENS node, for an entry an ENS record points at
        node       TEXT UNIQUE,
        -- the last root queued
        root       TEXT,
        checked_at INTEGER,
        next_at    INTEGER NOT NULL,
        failures   INTEGER NOT NULL DEFAULT 0,
        error      TEXT
    );
    CREATE INDEX IF NOT EXISTS discovered_next_at ON discovered (next_at);
    -- where background scans are, e.g. the ENS scan's next block (#213)
    CREATE TABLE IF NOT EXISTS discovery_state (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );";
```

Next to `fn root_of`, add:

```rust
/// One row of `discovered` (#213): an entry, where it came from, its last root and when it's
/// next checked
#[derive(Debug, Clone, PartialEq)]
pub struct Discovered {
    pub entry: String,
    pub source: String,
    pub root: Option<String>,
    pub checked_at: Option<i64>,
    pub next_at: i64,
    pub failures: u32,
    pub error: Option<String>,
}

const DISCOVERED_COLUMNS: &str = "entry, source, root, checked_at, next_at, failures, error";

fn discovered_row(r: &rusqlite::Row) -> rusqlite::Result<Discovered> {
    Ok(Discovered {
        entry: r.get(0)?,
        source: r.get(1)?,
        root: r.get(2)?,
        checked_at: r.get(3)?,
        next_at: r.get(4)?,
        failures: r.get(5)?,
        error: r.get(6)?,
    })
}
```

After `queue_retry`, add:

```rust
    /// Records an entry from a discovery source, due now; false if it's known already, which
    /// leaves it as it is (#213)
    pub fn discover(&self, entry: &str, source: &str, now: i64) -> rusqlite::Result<bool> {
        let conn = self.conn.lock().unwrap();
        let n = conn
            .prepare_cached(
                "INSERT OR IGNORE INTO discovered (entry, source, next_at) VALUES (?1, ?2, ?3)",
            )?
            .execute(params![entry, source, now])?;
        Ok(n == 1)
    }

    /// An ENS node's latest record (#213): its entry replaces the one the node had; None (the
    /// record was cleared) drops it. Entries another source found stay, losing only the node.
    pub fn discover_ens(&self, node: &str, entry: Option<&str>, now: i64) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        tx.prepare_cached(
            "DELETE FROM discovered WHERE node = ?1 AND entry IS NOT ?2 AND source = 'ens'",
        )?
        .execute(params![node, entry])?;
        tx.prepare_cached("UPDATE discovered SET node = NULL WHERE node = ?1 AND entry IS NOT ?2")?
            .execute(params![node, entry])?;
        if let Some(entry) = entry {
            tx.prepare_cached(
                "INSERT INTO discovered (entry, source, node, next_at) VALUES (?1, 'ens', ?2, ?3)
                 ON CONFLICT (entry) DO UPDATE SET node = ?2",
            )?
            .execute(params![entry, node, now])?;
        }
        tx.commit()
    }

    /// Entries due by `now`, soonest first, at most `limit`
    pub fn discovered_due(&self, now: i64, limit: usize) -> rusqlite::Result<Vec<Discovered>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {} FROM discovered WHERE next_at <= ?1 ORDER BY next_at LIMIT ?2",
            DISCOVERED_COLUMNS
        ))?;
        let rows = stmt.query_map(params![now, limit as i64], discovered_row)?;
        rows.collect()
    }

    /// A check that worked: the root it queued, and when it's due again
    pub fn discovered_ok(
        &self,
        entry: &str,
        root: &str,
        now: i64,
        next_at: i64,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.prepare_cached(
            "UPDATE discovered SET root = ?2, checked_at = ?3, next_at = ?4, failures = 0,
             error = NULL WHERE entry = ?1",
        )?
        .execute(params![entry, root, now, next_at])?;
        Ok(())
    }

    /// A check that failed: its error, one more failure, and when it's due again
    pub fn discovered_failed(
        &self,
        entry: &str,
        error: &str,
        now: i64,
        next_at: i64,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.prepare_cached(
            "UPDATE discovered SET checked_at = ?3, next_at = ?4, failures = failures + 1,
             error = ?2 WHERE entry = ?1",
        )?
        .execute(params![entry, error, now, next_at])?;
        Ok(())
    }

    /// Puts off an entry's check without counting one, e.g. while the hourly cap is used up
    pub fn discovered_postpone(&self, entry: &str, next_at: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.prepare_cached("UPDATE discovered SET next_at = ?2 WHERE entry = ?1")?
            .execute(params![entry, next_at])?;
        Ok(())
    }

    /// Every entry, most recently checked first, then those never checked, for /discovered
    pub fn discovered_all(&self) -> rusqlite::Result<Vec<Discovered>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {} FROM discovered ORDER BY checked_at IS NULL, checked_at DESC, entry",
            DISCOVERED_COLUMNS
        ))?;
        let rows = stmt.query_map([], discovered_row)?;
        rows.collect()
    }

    /// How many entries discovery has, for /status (0 if it can't be read)
    pub fn discovered_count(&self) -> usize {
        self.read_conn()
            .query_row("SELECT count(*) FROM discovered", [], |r| r.get::<_, i64>(0))
            .map(|n| n as usize)
            .unwrap_or(0)
    }

    /// A value a background scan saved, e.g. the ENS scan's next block
    pub fn state(&self, key: &str) -> rusqlite::Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        conn.prepare_cached("SELECT value FROM discovery_state WHERE key = ?1")?
            .query_row([key], |r| r.get(0))
            .optional()
    }

    pub fn set_state(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.prepare_cached(
            "INSERT INTO discovery_state (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = ?2",
        )?
        .execute(params![key, value])?;
        Ok(())
    }
```

- [ ] **Step 4: Run all tests**

Run: `cargo fmt && cargo test --quiet 2>&1 | grep "test result"`
Expected: all pass, the 223 already there plus 3 new. Dead-code warnings for the new methods are expected until Task 4.

- [ ] **Step 5: Commit**

```bash
git add src/store.rs
git commit -m "Store: discovered entries and discovery state (#213)"
```

---

### Task 2: Entries and ENS contenthash records (`src/entry.rs`)

**Files:**
- Create: `src/entry.rs`
- Modify: `src/main.rs`: add `mod entry;` to the module list, alphabetically after `mod denylist;`.

**Interfaces:**
- Produces:
  - `pub enum Entry { Cid(String), Name(String) }`, deriving `Debug, Clone, PartialEq`, with `Entry::as_str(&self) -> &str`;
  - `pub fn parse(input: &str) -> Option<Entry>`;
  - `pub fn domain(s: &str) -> Option<String>`;
  - `pub enum Contenthash { Set(Entry), Cleared, Other }`, deriving `Debug, PartialEq`;
  - `pub fn contenthash(bytes: &[u8]) -> Contenthash`.

- [ ] **Step 1: Write the module's tests, with the module stubbed so they fail**

Create `src/entry.rs` with the tests and a stub `parse`, `domain` and `contenthash` that return `None` / `Contenthash::Other`. Step 3 replaces the stubs.

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use cid::multihash::Multihash;

    const V1: &str = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";
    const V0: &str = "QmXoypizjW3WknFiJnKLwHCnL72vedxjQkDDP1mXWo6uco";

    // an IPNS key (an ed25519 public key as an identity multihash), and its CID
    fn key_cid() -> CidGeneric<64> {
        let mut key = vec![8, 1, 18, 32];
        key.extend([7u8; 32]);
        CidGeneric::new_v1(LIBP2P_KEY, Multihash::wrap(IDENTITY, &key).unwrap())
    }

    fn key() -> String {
        key_cid().to_string_of_base(Base::Base36Lower).unwrap()
    }

    fn cid(s: &str) -> Option<Entry> {
        Some(Entry::Cid(s.to_string()))
    }

    fn name(s: &str) -> Option<Entry> {
        Some(Entry::Name(s.to_string()))
    }

    #[test]
    fn cids_with_or_without_a_prefix_or_a_path() {
        assert_eq!(parse(V1), cid(V1));
        assert_eq!(parse(&format!(" /ipfs/{}/wiki/A ", V1)), cid(&format!("{}/wiki/A", V1)));
        assert_eq!(parse(&format!("ipfs://{}/", V0)), cid(V0));
        assert_eq!(parse(&format!("{}/index.html", V1)), cid(&format!("{}/index.html", V1)));
    }

    #[test]
    fn names_are_domains_eth_names_and_ipns_keys() {
        assert_eq!(parse("Docs.IPFS.tech"), name("docs.ipfs.tech"));
        assert_eq!(parse("/ipns/vitalik.eth/"), name("vitalik.eth"));
        assert_eq!(parse(&format!("ipns://{}", key())), name(&key()));
        assert_eq!(parse(&key()), name(&key()));
        assert_eq!(parse(&format!("/ipfs/{}", key())), None, "a key isn't content");
    }

    #[test]
    fn anything_else_is_not_an_entry() {
        let long_label = format!("{}.com", "a".repeat(64));
        let long_name = format!("{}com", "a.".repeat(127));
        for bad in [
            "",
            "wikipedia",
            "two words.com",
            "192.168.1.1",
            "::1",
            "[::1]",
            "-bad.com",
            "bad-.com",
            "a..com",
            "ftp://x.com",
            "notacid/wiki",
            "/ipns/docs.ipfs.tech/page",
            &long_label,
            &long_name,
        ] {
            assert_eq!(parse(bad), None, "{:?}", bad);
        }
    }

    fn ipfs_ns(cid_bytes: Vec<u8>) -> Vec<u8> {
        [vec![0xe3, 0x01], cid_bytes].concat()
    }

    fn ipns_ns(cid_bytes: Vec<u8>) -> Vec<u8> {
        [vec![0xe5, 0x01], cid_bytes].concat()
    }

    #[test]
    fn ipfs_records_are_cids() {
        let v1 = Cid::try_from(V1).unwrap().to_bytes();
        let v0 = Cid::try_from(V0).unwrap().to_bytes();
        assert_eq!(contenthash(&ipfs_ns(v1)), Contenthash::Set(Entry::Cid(V1.into())));
        assert_eq!(contenthash(&ipfs_ns(v0)), Contenthash::Set(Entry::Cid(V0.into())));
    }

    #[test]
    fn ipns_records_are_keys_or_inlined_domains() {
        assert_eq!(
            contenthash(&ipns_ns(key_cid().to_bytes())),
            Contenthash::Set(Entry::Name(key()))
        );
        // older records inline a DNSLink name as an identity multihash
        let inlined = CidGeneric::<256>::new_v1(
            0x70,
            Multihash::<256>::wrap(IDENTITY, b"app.uniswap.org").unwrap(),
        );
        assert_eq!(
            contenthash(&ipns_ns(inlined.to_bytes())),
            Contenthash::Set(Entry::Name("app.uniswap.org".into()))
        );
    }

    #[test]
    fn cleared_swarm_and_garbage_records() {
        assert_eq!(contenthash(&[]), Contenthash::Cleared);
        assert_eq!(contenthash(&[0xe4, 0x01, 0xfa, 0x01]), Contenthash::Other, "swarm");
        assert_eq!(contenthash(&[0xe3, 0x01, 0xff, 0xff]), Contenthash::Other);
        assert_eq!(contenthash(&[0x00]), Contenthash::Other);
        assert_eq!(
            contenthash(&ipfs_ns(key_cid().to_bytes())),
            Contenthash::Other,
            "an IPNS key in an ipfs-ns record"
        );
    }
}
```

Stub, at the top of the file for now:

```rust
use cid::multibase::Base;
use cid::{Cid, CidGeneric};

const LIBP2P_KEY: u64 = 0x72;
const IDENTITY: u64 = 0x00;

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Cid(String),
    Name(String),
}

#[derive(Debug, PartialEq)]
pub enum Contenthash {
    Set(Entry),
    Cleared,
    Other,
}

pub fn parse(_input: &str) -> Option<Entry> {
    None
}

pub fn contenthash(_bytes: &[u8]) -> Contenthash {
    Contenthash::Other
}
```

Add `mod entry;` to `src/main.rs`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --quiet entry:: 2>&1 | grep -E "test result|FAILED" | head`
Expected: `anything_else_is_not_an_entry` passes, since the stub always returns `None`. The other 5 fail.

- [ ] **Step 3: Implement.** Replace everything above `#[cfg(test)]` with:

```rust
/*!
 * What discovery sources produce (#213): a CID (or cid/path) to queue, or a name to resolve
 * through the gateway's /ipns/ path (a DNSLink domain, a .eth name or an IPNS key). Also decodes
 * ENS contenthash records (EIP-1577) into entries.
 */
use cid::multibase::Base;
use cid::{Cid, CidGeneric};

// the multicodecs of an IPNS key and of the identity multihash
const LIBP2P_KEY: u64 = 0x72;
const IDENTITY: u64 = 0x00;

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    /// "cid" or "cid/path", as enqueue() takes it
    Cid(String),
    /// resolved through /ipns/<name>/
    Name(String),
}

impl Entry {
    /// The text it's stored and shown as
    pub fn as_str(&self) -> &str {
        match self {
            Entry::Cid(s) | Entry::Name(s) => s,
        }
    }
}

/// An entry from a seeds file line, a submission or a stored row: "cid", "cid/path",
/// "/ipfs/..." or "ipfs://..."; or a domain, a .eth name or an IPNS key, bare or as "/ipns/..."
/// or "ipns://...". None for anything else.
pub fn parse(input: &str) -> Option<Entry> {
    let s = input.trim();
    if let Some(rest) = s.strip_prefix("/ipfs/").or_else(|| s.strip_prefix("ipfs://")) {
        return cid_entry(rest);
    }
    if let Some(rest) = s.strip_prefix("/ipns/").or_else(|| s.strip_prefix("ipns://")) {
        return name(rest.trim_end_matches('/'));
    }
    cid_entry(s).or_else(|| name(s))
}

// a CID, maybe with a path; an IPNS key isn't content, it's a name
fn cid_entry(s: &str) -> Option<Entry> {
    let s = s.trim_end_matches('/');
    let cid = Cid::try_from(s.split('/').next()?).ok()?;
    (cid.codec() != LIBP2P_KEY).then(|| Entry::Cid(s.to_string()))
}

// a domain (it has dots, which no CID's base does) or an IPNS key
fn name(s: &str) -> Option<Entry> {
    if s.contains('.') {
        return domain(s).map(Entry::Name);
    }
    let cid = Cid::try_from(s).ok()?;
    (cid.codec() == LIBP2P_KEY).then(|| Entry::Name(s.to_string()))
}

/// A DNS name, lowercased (DNSLink or .eth): 2 or more labels of [a-z0-9-], each 1-63 long and
/// not starting or ending with "-", at most 253 in all, and not an IP address
pub fn domain(s: &str) -> Option<String> {
    let s = s.to_ascii_lowercase();
    let labels: Vec<&str> = s.split('.').collect();
    let label_ok = |label: &&str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    };
    // a numeric top-level label: an IPv4 address (IPv6 has ':' and fails the labels)
    let ip = labels
        .last()
        .is_some_and(|tld| tld.chars().all(|c| c.is_ascii_digit()));
    (s.len() <= 253 && labels.len() >= 2 && labels.iter().all(label_ok) && !ip).then_some(s)
}

/// What an ENS contenthash record (EIP-1577) points at
#[derive(Debug, PartialEq)]
pub enum Contenthash {
    Set(Entry),
    /// the record was cleared
    Cleared,
    /// not IPFS or IPNS (Swarm, Onion...), or not decodable
    Other,
}

pub fn contenthash(bytes: &[u8]) -> Contenthash {
    match bytes {
        [] => Contenthash::Cleared,
        // ipfs-ns (0xe3, as a varint), then a CID
        [0xe3, 0x01, cid @ ..] => match Cid::try_from(cid) {
            Ok(cid) if cid.codec() != LIBP2P_KEY => Contenthash::Set(Entry::Cid(cid.to_string())),
            _ => Contenthash::Other,
        },
        // ipns-ns (0xe5): an IPNS key, or a DNS name inlined as an identity multihash (which can
        // be longer than a hash, hence the larger CID)
        [0xe5, 0x01, cid @ ..] => match CidGeneric::<256>::try_from(cid) {
            Ok(cid) if cid.codec() == LIBP2P_KEY => match cid.to_string_of_base(Base::Base36Lower) {
                Ok(key) => Contenthash::Set(Entry::Name(key)),
                Err(_) => Contenthash::Other,
            },
            Ok(cid) if cid.hash().code() == IDENTITY => {
                match std::str::from_utf8(cid.hash().digest()).ok().and_then(domain) {
                    Some(name) => Contenthash::Set(Entry::Name(name)),
                    None => Contenthash::Other,
                }
            }
            _ => Contenthash::Other,
        },
        _ => Contenthash::Other,
    }
}
```

- [ ] **Step 4: Run all tests**

Run: `cargo fmt && cargo test --quiet 2>&1 | grep "test result"`
Expected: all pass, with 6 new. Dead-code warnings for `entry` items are fine until Task 4.

- [ ] **Step 5: Commit**

```bash
git add src/entry.rs src/main.rs
git commit -m "Entries: CIDs and names for discovery; ENS contenthash records (#213)"
```

---

### Task 3: The ENS log scan (`src/ens.rs`)

**Files:**
- Create: `src/ens.rs`
- Modify: `Cargo.toml`: add `serde_json = "1"` under `[dependencies]`, alphabetically. `src/main.rs`: add `mod ens;` after `mod denylist;`, keeping alphabetical order.

**Interfaces:**
- Consumes: `crate::entry::{contenthash, Contenthash}` (Task 2).
- Produces:
  - `pub const TOPIC: &str`, `pub const START_BLOCK: u64 = 3_327_417`, `pub const MAX_RANGE: u64 = 10_000`;
  - `pub enum RpcError { Transport(String), Rpc(String) }`, deriving `Debug, PartialEq`, with `Display`;
  - `pub trait Rpc { fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, RpcError>; }`;
  - `pub struct HttpRpc`, with `HttpRpc::new(url: String) -> Result<HttpRpc, String>` and `HttpRpc::host(&self) -> String`, implementing `Rpc`;
  - `pub struct Event { pub node: String, pub record: Contenthash }`, deriving `Debug, PartialEq`;
  - `pub struct Scan { pub next: u64, pub range: u64 }`, deriving `Debug, Clone, Copy, PartialEq`, with `Scan::starting(next: u64) -> Scan`;
  - `pub fn latest_block(rpc: &dyn Rpc) -> Result<u64, RpcError>`;
  - `pub fn step(rpc: &dyn Rpc, scan: &mut Scan, latest: u64) -> Result<Vec<Event>, RpcError>`;
  - for tests, `#[cfg(test)] pub(crate) mod fake`, with `FakeRpc::new(answers: Vec<Result<Value, RpcError>>)`, `FakeRpc::calls(&self) -> Vec<(String, Value)>` and `pub fn log(node: &str, hash: &[u8]) -> Value`.

- [ ] **Step 1: Write the failing tests.** Create `src/ens.rs` containing only the `fake` module and the tests below, plus `use` lines, so it fails to compile until Step 3.

```rust
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Answers calls from a list, in order, and records them
    pub struct FakeRpc {
        answers: RefCell<VecDeque<Result<Value, RpcError>>>,
        calls: RefCell<Vec<(String, Value)>>,
    }

    impl FakeRpc {
        pub fn new(answers: Vec<Result<Value, RpcError>>) -> FakeRpc {
            FakeRpc {
                answers: RefCell::new(answers.into()),
                calls: RefCell::new(Vec::new()),
            }
        }

        pub fn calls(&self) -> Vec<(String, Value)> {
            self.calls.borrow().clone()
        }
    }

    impl Rpc for FakeRpc {
        fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
            self.calls.borrow_mut().push((method.to_string(), params));
            self.answers
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err(RpcError::Transport("no more answers".into())))
        }
    }

    /// A ContenthashChanged log as a provider returns it, the contenthash ABI-encoded
    pub fn log(node: &str, hash: &[u8]) -> Value {
        let mut data = vec![0u8; 31];
        data.push(32);
        data.extend([0u8; 24]);
        data.extend((hash.len() as u64).to_be_bytes());
        data.extend(hash);
        data.resize(64 + hash.len().div_ceil(32) * 32, 0);
        let hex: String = data.iter().map(|b| format!("{:02x}", b)).collect();
        json!({
            "address": "0x231b0ee14048e9dccd1d247744d114a4eb5e8e63",
            "topics": [TOPIC, node],
            "data": format!("0x{}", hex),
            "blockNumber": "0x18feaf6",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fake::{log, FakeRpc};
    use super::*;
    use crate::entry::Entry;

    const V1: &str = "bafybeiaysi4s6lnjev27ln5icwm6tueaw2vdykrtjkwiphwekaywqhcjze";

    fn ipfs_record() -> Vec<u8> {
        [vec![0xe3, 0x01], cid::Cid::try_from(V1).unwrap().to_bytes()].concat()
    }

    fn range_of(call: &(String, Value)) -> (u64, u64) {
        let p = &call.1[0];
        let n = |k: &str| u64::from_str_radix(p[k].as_str().unwrap().trim_start_matches("0x"), 16).unwrap();
        (n("fromBlock"), n("toBlock"))
    }

    #[test]
    fn logs_decode_to_records_and_unreadable_ones_are_skipped() {
        let rpc = FakeRpc::new(vec![Ok(json!([
            log("0xaa", &ipfs_record()),
            log("0xbb", &[]),
            log("0xcc", &[0xe4, 0x01, 0xfa]),
            {"topics": [TOPIC], "data": "0x"},
            {"topics": [TOPIC, "0xdd"], "data": "0x1234"},
        ]))]);
        let mut scan = Scan::starting(START_BLOCK);
        let events = step(&rpc, &mut scan, START_BLOCK + 50_000).unwrap();
        assert_eq!(
            events,
            vec![
                Event { node: "0xaa".into(), record: Contenthash::Set(Entry::Cid(V1.into())) },
                Event { node: "0xbb".into(), record: Contenthash::Cleared },
                Event { node: "0xcc".into(), record: Contenthash::Other },
            ]
        );
        let calls = rpc.calls();
        assert_eq!(calls[0].0, "eth_getLogs");
        assert_eq!(calls[0].1[0]["topics"], json!([TOPIC]));
        assert!(calls[0].1[0].get("address").is_none(), "every resolver");
        assert_eq!(range_of(&calls[0]), (START_BLOCK, START_BLOCK + 9_999));
        assert_eq!(scan, Scan { next: START_BLOCK + 10_000, range: MAX_RANGE });
    }

    #[test]
    fn the_range_halves_on_an_rpc_error_and_doubles_back() {
        let too_big = || Err(RpcError::Rpc("ranges over 10000 blocks are not supported".into()));
        let rpc = FakeRpc::new(vec![too_big(), too_big(), Ok(json!([])), Ok(json!([]))]);
        let mut scan = Scan::starting(100);
        assert!(step(&rpc, &mut scan, 1_000_000).is_err());
        assert_eq!(scan, Scan { next: 100, range: 5_000 });
        assert!(step(&rpc, &mut scan, 1_000_000).is_err());
        assert_eq!(scan, Scan { next: 100, range: 2_500 });
        step(&rpc, &mut scan, 1_000_000).unwrap();
        assert_eq!(scan, Scan { next: 2_600, range: 5_000 });
        step(&rpc, &mut scan, 1_000_000).unwrap();
        assert_eq!(scan, Scan { next: 7_600, range: 10_000 });
        let ranges: Vec<_> = rpc.calls().iter().map(range_of).collect();
        assert_eq!(ranges, vec![(100, 10_099), (100, 5_099), (100, 2_599), (2_600, 7_599)]);
    }

    #[test]
    fn the_range_never_goes_below_one_block() {
        let rpc = FakeRpc::new(vec![Err(RpcError::Rpc("no".into()))]);
        let mut scan = Scan { next: 100, range: 1 };
        assert!(step(&rpc, &mut scan, 1_000).is_err());
        assert_eq!(scan, Scan { next: 100, range: 1 });
    }

    #[test]
    fn a_transport_error_keeps_the_range_and_position() {
        let rpc = FakeRpc::new(vec![Err(RpcError::Transport("timed out".into()))]);
        let mut scan = Scan::starting(100);
        assert_eq!(
            step(&rpc, &mut scan, 1_000_000),
            Err(RpcError::Transport("timed out".into()))
        );
        assert_eq!(scan, Scan::starting(100));
    }

    #[test]
    fn the_last_request_stops_at_the_latest_block_and_then_there_are_none() {
        let rpc = FakeRpc::new(vec![Ok(json!([]))]);
        let mut scan = Scan::starting(100);
        step(&rpc, &mut scan, 150).unwrap();
        assert_eq!(range_of(&rpc.calls()[0]), (100, 150));
        assert_eq!(scan.next, 151);
        assert_eq!(step(&rpc, &mut scan, 150), Ok(vec![]));
        assert_eq!(rpc.calls().len(), 1, "caught up: no request");
    }

    #[test]
    fn latest_block_reads_the_hex_number() {
        let rpc = FakeRpc::new(vec![Ok(json!("0x18fec3e"))]);
        assert_eq!(latest_block(&rpc), Ok(26_209_342));
        assert_eq!(rpc.calls()[0].0, "eth_blockNumber");
    }

    #[test]
    fn answers_are_results_or_rpc_errors() {
        assert_eq!(
            answer(r#"{"jsonrpc":"2.0","id":1,"error":{"code":35,"message":"ranges over 10000 blocks are not supported on free plan"}}"#),
            Err(RpcError::Rpc("ranges over 10000 blocks are not supported on free plan".into()))
        );
        assert!(matches!(answer("<html>busy</html>"), Err(RpcError::Transport(_))));
        assert_eq!(answer(r#"{"jsonrpc":"2.0","id":1,"result":[1]}"#), Ok(json!([1])));
    }

    #[test]
    fn http_errors_never_include_the_url() {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let rpc = HttpRpc::new(format!("http://127.0.0.1:{}/v2/SECRETKEY", port)).unwrap();
        assert_eq!(rpc.host(), "127.0.0.1");
        match rpc.call("eth_blockNumber", json!([])) {
            Err(RpcError::Transport(msg)) => assert!(!msg.contains("SECRETKEY"), "{}", msg),
            other => panic!("{:?}", other),
        }
    }
}
```

At the top of the file, so the tests can name things:

```rust
use crate::entry::{contenthash, Contenthash};
use serde_json::{json, Value};
```

Add `serde_json = "1"` to `Cargo.toml` and `mod ens;` to `src/main.rs`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --quiet ens:: 2>&1 | grep -E "^error" | sort | uniq -c`
Expected: compile errors. `Rpc`, `Scan`, `step` and the rest don't exist.

- [ ] **Step 3: Implement.** Put this above the `fake` module, replacing the two `use` lines:

```rust
/*!
 * Lists .eth sites from Ethereum (#213): ENS resolvers emit ContenthashChanged(node, hash) when a
 * name's content record changes. eth_getLogs over block ranges finds them all, and only the
 * latest per node counts. Providers cap ranges differently, so the range adapts: a JSON-RPC
 * error halves it, a success doubles it back.
 */
use crate::entry::{contenthash, Contenthash};
use log::warn;
use serde_json::{json, Value};
use std::time::Duration;

/// keccak256("ContenthashChanged(bytes32,bytes)")
pub const TOPIC: &str = "0xe379c1624ed7e714cc0937528a32359d69d5281337765313dba4e081b72d7578";
/// The first ENS registry's deployment (May 2017): no record is older
pub const START_BLOCK: u64 = 3_327_417;
pub const MAX_RANGE: u64 = 10_000;

#[derive(Debug, PartialEq)]
pub enum RpcError {
    /// no usable answer: no connection, a timeout, a rate limit, a body that isn't JSON-RPC
    Transport(String),
    /// the provider answered with a JSON-RPC error, e.g. a range or result-count limit
    Rpc(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RpcError::Transport(msg) => write!(f, "{}", msg),
            RpcError::Rpc(msg) => write!(f, "provider error: {}", msg),
        }
    }
}

pub trait Rpc {
    fn call(&self, method: &str, params: Value) -> Result<Value, RpcError>;
}

/// JSON-RPC over HTTP. The URL carries the provider's API key, so it's never logged or put in an
/// error.
pub struct HttpRpc {
    url: String,
    client: reqwest::blocking::Client,
}

impl HttpRpc {
    pub fn new(url: String) -> Result<HttpRpc, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| e.without_url().to_string())?;
        Ok(HttpRpc { url, client })
    }

    /// The URL's host: the one part of it that's safe to log
    pub fn host(&self) -> String {
        reqwest::Url::parse(&self.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "?".to_string())
    }
}

impl Rpc for HttpRpc {
    fn call(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let transport = |e: reqwest::Error| RpcError::Transport(e.without_url().to_string());
        let response = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .map_err(transport)?;
        // a rate limit is a reason to wait, not to ask for fewer blocks
        if response.status().as_u16() == 429 {
            return Err(RpcError::Transport("rate limited (429)".to_string()));
        }
        let status = response.status();
        let text = response.text().map_err(transport)?;
        answer(&text).map_err(|err| match err {
            RpcError::Transport(msg) => RpcError::Transport(format!("HTTP {}: {}", status, msg)),
            err => err,
        })
    }
}

// a JSON-RPC response body: its result, or its error
fn answer(text: &str) -> Result<Value, RpcError> {
    let mut v: Value = serde_json::from_str(text).map_err(|_| {
        RpcError::Transport(format!("not a JSON-RPC answer: {:.200}", text))
    })?;
    if let Some(err) = v.get("error") {
        let msg = err.get("message").and_then(Value::as_str).map(str::to_string);
        return Err(RpcError::Rpc(msg.unwrap_or_else(|| err.to_string())));
    }
    v.get_mut("result")
        .map(Value::take)
        .ok_or_else(|| RpcError::Transport("no result in the answer".to_string()))
}

/// One ContenthashChanged log: the ENS node and what its record points at now
#[derive(Debug, PartialEq)]
pub struct Event {
    pub node: String,
    pub record: Contenthash,
}

/// Where the scan is: the next block to read, and how many blocks a request asks for
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scan {
    pub next: u64,
    pub range: u64,
}

impl Scan {
    pub fn starting(next: u64) -> Scan {
        Scan {
            next,
            range: MAX_RANGE,
        }
    }
}

/// The latest block number
pub fn latest_block(rpc: &dyn Rpc) -> Result<u64, RpcError> {
    let v = rpc.call("eth_blockNumber", json!([]))?;
    v.as_str()
        .and_then(hex_u64)
        .ok_or_else(|| RpcError::Transport(format!("not a block number: {:.100}", v)))
}

/**
 * One eth_getLogs request, from scan.next to at most `latest`. On success the scan moves past
 * the range and the range doubles (up to MAX_RANGE); a JSON-RPC error halves it (down to 1) so
 * the next request asks for less; a transport error leaves the scan alone. Logs that can't be
 * read are skipped with a warning.
 */
pub fn step(rpc: &dyn Rpc, scan: &mut Scan, latest: u64) -> Result<Vec<Event>, RpcError> {
    if scan.next > latest {
        return Ok(Vec::new());
    }
    let to = latest.min(scan.next + scan.range - 1);
    let params = json!([{
        "fromBlock": format!("{:#x}", scan.next),
        "toBlock": format!("{:#x}", to),
        "topics": [TOPIC],
    }]);
    let logs = match rpc.call("eth_getLogs", params) {
        Ok(logs) => logs,
        Err(RpcError::Rpc(msg)) => {
            scan.range = (scan.range / 2).max(1);
            return Err(RpcError::Rpc(msg));
        }
        Err(err) => return Err(err),
    };
    let logs = logs
        .as_array()
        .ok_or_else(|| RpcError::Transport("eth_getLogs didn't return a list".to_string()))?;
    let events = logs
        .iter()
        .filter_map(|log| {
            let read = event(log);
            if read.is_none() {
                warn!("ENS scan: skipping a log I couldn't read: {:.300}", log);
            }
            read
        })
        .collect();
    scan.next = to + 1;
    scan.range = (scan.range * 2).min(MAX_RANGE);
    Ok(events)
}

// a log's node (topics[1]) and its contenthash (data: ABI-encoded bytes)
fn event(log: &Value) -> Option<Event> {
    let node = log.get("topics")?.get(1)?.as_str()?.to_string();
    let data = hex_bytes(log.get("data")?.as_str()?)?;
    Some(Event {
        node,
        record: contenthash(abi_bytes(&data)?),
    })
}

fn hex_u64(s: &str) -> Option<u64> {
    u64::from_str_radix(s.strip_prefix("0x")?, 16).ok()
}

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x")?;
    if !s.is_ascii() || s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

// ABI-encoded `bytes`: a 32-byte offset, and there a 32-byte length and the bytes themselves
fn abi_bytes(data: &[u8]) -> Option<&[u8]> {
    let word = |at: usize| -> Option<usize> {
        let w = data.get(at..at.checked_add(32)?)?;
        if w[..24].iter().any(|b| *b != 0) {
            return None;
        }
        Some(u64::from_be_bytes(w[24..].try_into().ok()?) as usize)
    };
    let offset = word(0)?;
    let len = word(offset)?;
    let start = offset.checked_add(32)?;
    data.get(start..start.checked_add(len)?)
}
```

- [ ] **Step 4: Run all tests**

Run: `cargo fmt && cargo test --quiet 2>&1 | grep "test result"`
Expected: all pass, with 8 new.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/ens.rs src/main.rs
git commit -m "ENS scan: ContenthashChanged logs with an adaptive block range, key kept out of errors (#213)"
```

---

### Task 4: Discovery: the cap, the resolver, seeds and ENS passes (`src/discovery.rs`)

**Files:**
- Create: `src/discovery.rs`
- Modify:
  - `src/main.rs`: add `mod discovery;` after `mod denylist;`, alphabetically.
  - `src/index_queue.rs`: make `fn store`, `fn wait_for_gateway` and `fn breaker` `pub(crate)`. Add `IndexQueue::discover`, `IndexQueue::discovered` and `IndexQueue::discovered_count` after `crawl_paused`.

**Interfaces:**
- Consumes:
  - Task 1's `Store` discovery methods and `Discovered`;
  - Task 2's `entry::{parse, Entry, Contenthash}`;
  - Task 3's `ens::{Rpc, RpcError, Scan, Event, step, latest_block, START_BLOCK}` and `ens::fake`.
- Produces:
  - `pub struct HourlyCap`, with `HourlyCap::new(per_hour: u32)` and `HourlyCap::take(&self, now: i64) -> bool`;
  - `pub enum Checked { Queued(String), Unchanged(String), Capped, Failed(String) }`, deriving `Debug, PartialEq`;
  - `pub fn resolve(gateway: &str, name: &str) -> Result<String, String>`;
  - `pub fn check(queue: &IndexQueue, gateway: &str, cap: &HourlyCap, row: &Discovered, now: i64) -> Checked`;
  - `pub fn resolve_next(queue: &IndexQueue, gateway: &str, cap: &HourlyCap) -> Option<Checked>`;
  - `pub fn run_resolver(queue: &IndexQueue, gateway: &str, cap: &HourlyCap)`, which loops forever;
  - `pub fn parse_seeds(text: &str) -> Vec<Entry>`;
  - `pub fn add_seeds(queue: &IndexQueue, entries: &[Entry], now: i64) -> usize`;
  - `pub fn run_seeds(queue: &IndexQueue, path: &std::path::Path)`, which loops forever;
  - `pub fn apply_ens(queue: &IndexQueue, events: &[ens::Event], now: i64) -> usize`;
  - `pub fn ens_start(queue: &IndexQueue) -> Scan`;
  - `pub fn ens_pass(queue: &IndexQueue, rpc: &dyn Rpc, scan: &mut Scan, pause: Duration) -> Result<(), RpcError>`;
  - `pub fn run_ens(queue: &IndexQueue, rpc: &dyn Rpc)`, which loops forever.
- Also, in `IndexQueue`:
  - `pub fn discover(&self, entry: &str, source: &str) -> rusqlite::Result<bool>`;
  - `pub fn discovered(&self) -> Vec<Discovered>`;
  - `pub fn discovered_count(&self) -> usize`.

- [ ] **Step 1: Write the failing tests.** Create `src/discovery.rs` with only this test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::breaker::Breaker;
    use crate::denylist::Denylist;
    use crate::ens::fake::{log, FakeRpc};
    use serde_json::json;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const A: &str = "bafybeier6ud42ptljtfaxrdlknktw54b7ohfpg5h33mosxpxjndxs7xzvm";
    const B: &str = "bafybeiesmy6rsaglneqxbzkbdj5degzp2izydfadhl2vu3oyrp7k52fypu";
    const NOW: i64 = 1_800_000_000;

    // a gateway answering /ipns/<name>/ with X-Ipfs-Roots: <root>, or 504 if root is None, and
    // /ipfs/<path> with a page naming the path; returns its host:port
    fn gateway(root: Option<&'static str>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                let reply = match (path.starts_with("/ipns/"), root) {
                    (true, Some(root)) => format!(
                        "HTTP/1.1 200 OK\r\nX-Ipfs-Roots: {},bafkreiextra\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        root
                    ),
                    (true, None) => "HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                    (false, _) => {
                        let body = format!("<title>{0}</title><body>words for {0}</body>", path);
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    }
                };
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        host
    }

    fn row(queue: &IndexQueue, entry: &str) -> Discovered {
        queue
            .discovered()
            .into_iter()
            .find(|d| d.entry == entry)
            .unwrap()
    }

    #[test]
    fn the_hourly_cap_counts_per_hour_and_zero_means_none() {
        let cap = HourlyCap::new(2);
        assert!(cap.take(NOW));
        assert!(cap.take(NOW + 1));
        assert!(!cap.take(NOW + 2));
        assert!(cap.take(next_hour(NOW)), "a new hour");
        let none = HourlyCap::new(0);
        assert!((0..1000).all(|i| none.take(NOW + i)));
    }

    #[test]
    fn failures_wait_an_hour_doubling_up_to_a_day() {
        let waits: Vec<i64> = [0, 1, 4, 5, 30].iter().map(|f| retry_after(*f)).collect();
        assert_eq!(waits, vec![HOUR, 2 * HOUR, 16 * HOUR, DAY, DAY]);
    }

    #[test]
    fn a_name_resolves_and_its_root_is_queued() {
        let queue = IndexQueue::new();
        queue.discover("docs.ipfs.tech", "seeds").unwrap();
        let checked = check(&queue, &gateway(Some(A)), &HourlyCap::new(10), &row(&queue, "docs.ipfs.tech"), NOW);
        assert_eq!(checked, Checked::Queued(A.into()));
        assert_eq!(queue.queue_length(), 1);
        let row = row(&queue, "docs.ipfs.tech");
        assert_eq!((row.root.as_deref(), row.checked_at, row.next_at), (Some(A), Some(NOW), NOW + DAY));
    }

    #[test]
    fn a_changed_root_is_queued_and_an_unchanged_one_again() {
        let queue = IndexQueue::new();
        queue.discover("docs.ipfs.tech", "seeds").unwrap();
        let cap = HourlyCap::new(10);
        check(&queue, &gateway(Some(A)), &cap, &row(&queue, "docs.ipfs.tech"), NOW);
        assert_eq!(
            check(&queue, &gateway(Some(A)), &cap, &row(&queue, "docs.ipfs.tech"), NOW + DAY),
            Checked::Unchanged(A.into())
        );
        assert_eq!(
            check(&queue, &gateway(Some(B)), &cap, &row(&queue, "docs.ipfs.tech"), NOW + 2 * DAY),
            Checked::Queued(B.into())
        );
        assert_eq!(row(&queue, "docs.ipfs.tech").root.as_deref(), Some(B));
    }

    #[test]
    fn an_unchanged_root_is_queued_again_even_when_capped() {
        let queue = IndexQueue::new();
        queue.discover("docs.ipfs.tech", "seeds").unwrap();
        let cap = HourlyCap::new(1);
        check(&queue, &gateway(Some(A)), &cap, &row(&queue, "docs.ipfs.tech"), NOW);
        assert_eq!(
            check(&queue, &gateway(Some(A)), &cap, &row(&queue, "docs.ipfs.tech"), NOW + 10),
            Checked::Unchanged(A.into())
        );
    }

    #[test]
    fn a_new_root_over_the_cap_waits_for_the_next_hour() {
        let queue = IndexQueue::new();
        queue.discover("docs.ipfs.tech", "seeds").unwrap();
        let cap = HourlyCap::new(1);
        assert!(cap.take(NOW));
        assert_eq!(
            check(&queue, &gateway(Some(A)), &cap, &row(&queue, "docs.ipfs.tech"), NOW),
            Checked::Capped
        );
        let row = row(&queue, "docs.ipfs.tech");
        assert_eq!((row.next_at, row.checked_at, row.failures), (next_hour(NOW), None, 0));
        assert_eq!(queue.queue_length(), 0);
    }

    #[test]
    fn a_failure_is_stored_and_backs_off() {
        let queue = IndexQueue::new();
        queue.discover("docs.ipfs.tech", "seeds").unwrap();
        let cap = HourlyCap::new(10);
        let gw = gateway(None);
        assert!(matches!(check(&queue, &gw, &cap, &row(&queue, "docs.ipfs.tech"), NOW), Checked::Failed(_)));
        let first = row(&queue, "docs.ipfs.tech");
        assert_eq!((first.failures, first.next_at), (1, NOW + HOUR));
        assert!(first.error.unwrap().contains("504"));
        check(&queue, &gw, &cap, &row(&queue, "docs.ipfs.tech"), NOW + HOUR);
        assert_eq!(row(&queue, "docs.ipfs.tech").next_at, NOW + HOUR + 2 * HOUR);
    }

    #[test]
    fn a_cid_entry_is_its_own_root_and_a_blocked_root_is_refused() {
        let queue = IndexQueue::new();
        queue.discover(A, "seeds").unwrap();
        let cap = HourlyCap::new(10);
        // no gateway needed
        assert_eq!(check(&queue, "127.0.0.1:1", &cap, &row(&queue, A), NOW), Checked::Queued(A.into()));
        queue.set_local_denylist(Denylist::parse(&format!("{}\n", B)));
        queue.discover("blocked.example", "submitted").unwrap();
        match check(&queue, &gateway(Some(B)), &cap, &row(&queue, "blocked.example"), NOW) {
            Checked::Failed(err) => assert!(err.contains("blocked"), "{}", err),
            other => panic!("{:?}", other),
        }
    }

    #[test]
    fn the_resolver_waits_while_the_crawl_is_paused() {
        let queue = IndexQueue::new();
        queue.set_breaker_for_test(Breaker::with(Duration::from_secs(600), Duration::from_secs(900)));
        for _ in 0..16 {
            queue.breaker().record(Some("504"));
        }
        queue.discover(A, "seeds").unwrap();
        assert_eq!(resolve_next(&queue, "127.0.0.1:1", &HourlyCap::new(10)), None);
        assert_eq!(row(&queue, A).checked_at, None, "still due");
    }

    #[test]
    fn seeds_lines_skip_comments_blanks_and_junk() {
        let text = format!(
            "# IPFS project sites\n\ndocs.ipfs.tech  # the docs\n/ipfs/{}\nnot a seed\nVitalik.eth\n",
            A
        );
        assert_eq!(
            parse_seeds(&text),
            vec![
                Entry::Name("docs.ipfs.tech".into()),
                Entry::Cid(A.into()),
                Entry::Name("vitalik.eth".into()),
            ]
        );
    }

    #[test]
    fn seeds_are_added_once_and_stay_when_the_file_goes() {
        let queue = IndexQueue::new();
        let entries = parse_seeds("docs.ipfs.tech\nvitalik.eth\n");
        assert_eq!(add_seeds(&queue, &entries, NOW), 2);
        assert_eq!(add_seeds(&queue, &entries, NOW + HOUR), 0);
        assert_eq!(row(&queue, "vitalik.eth").source, "seeds");
        // an unreadable file adds nothing, and the entries stay
        assert_eq!(add_seeds(&queue, &[], NOW + 2 * HOUR), 0);
        assert_eq!(queue.discovered_count(), 2);
    }

    #[test]
    fn ens_events_keep_the_latest_record_per_node() {
        use crate::ens::Event;
        let queue = IndexQueue::new();
        let set = |node: &str, entry: Entry| Event { node: node.into(), record: Contenthash::Set(entry) };
        let skipped = apply_ens(
            &queue,
            &[
                set("0x01", Entry::Cid(A.into())),
                set("0x02", Entry::Name("app.uniswap.org".into())),
                Event { node: "0x03".into(), record: Contenthash::Other },
                Event { node: "0x01".into(), record: Contenthash::Cleared },
            ],
            NOW,
        );
        assert_eq!(skipped, 1);
        let entries: Vec<(String, String)> = queue.discovered().into_iter().map(|d| (d.entry, d.source)).collect();
        assert_eq!(entries, vec![("app.uniswap.org".to_string(), "ens".to_string())]);
    }

    #[test]
    fn an_ens_pass_halves_and_carries_on_and_saves_its_position() {
        let queue = IndexQueue::new();
        let start = crate::ens::START_BLOCK;
        let ipfs = [vec![0xe3, 0x01], cid::Cid::try_from(A).unwrap().to_bytes()].concat();
        let rpc = FakeRpc::new(vec![
            Ok(json!(format!("{:#x}", start + 15_000))),
            Err(RpcError::Rpc("too many blocks".into())),
            Ok(json!([log("0x01", &ipfs)])),
            Ok(json!([])),
            Ok(json!([])),
        ]);
        let mut scan = ens_start(&queue);
        assert_eq!(scan.next, start);
        ens_pass(&queue, &rpc, &mut scan, Duration::ZERO).unwrap();
        assert_eq!(scan.next, start + 15_001);
        assert_eq!(rpc.calls().len(), 5);
        assert_eq!(row(&queue, A).source, "ens");
        assert_eq!(ens_start(&queue).next, start + 15_001, "saved");
    }

    #[test]
    fn a_failing_ens_pass_keeps_its_position() {
        let queue = IndexQueue::new();
        let start = crate::ens::START_BLOCK;
        let rpc = FakeRpc::new(vec![
            Ok(json!(format!("{:#x}", start + 50_000))),
            Ok(json!([])),
            Err(RpcError::Transport("timed out".into())),
        ]);
        let mut scan = ens_start(&queue);
        assert_eq!(
            ens_pass(&queue, &rpc, &mut scan, Duration::ZERO),
            Err(RpcError::Transport("timed out".into()))
        );
        assert_eq!(ens_start(&queue).next, start + 10_000);
        // a provider refusing even one block: the pass gives up instead of spinning
        let rpc = FakeRpc::new(vec![
            Ok(json!(format!("{:#x}", start + 50_000))),
            Err(RpcError::Rpc("no".into())),
        ]);
        let mut scan = Scan { next: start + 10_000, range: 1 };
        assert!(ens_pass(&queue, &rpc, &mut scan, Duration::ZERO).is_err());
        assert_eq!(rpc.calls().len(), 2);
    }

    #[test]
    fn a_seeded_site_is_resolved_queued_and_indexed() {
        let queue = IndexQueue::new();
        add_seeds(&queue, &parse_seeds("site.example\n"), NOW);
        let gw = gateway(Some(A));
        assert_eq!(resolve_next(&queue, &gw, &HourlyCap::new(10)), Some(Checked::Queued(A.into())));
        assert!(queue.process_next(&gw));
        assert_eq!(queue.index_length(), 1);
    }
}
```

Add `mod discovery;` to `src/main.rs`.

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --quiet discovery:: 2>&1 | grep -E "^error" | sort | uniq -c`
Expected: compile errors. Nothing in `discovery` exists yet.

- [ ] **Step 3: Implement**

In `src/index_queue.rs`:
- change `fn store(&self) -> &Store {` to `pub(crate) fn store(&self) -> &Store {`;
- change `fn wait_for_gateway(&self) {` to `pub(crate) fn wait_for_gateway(&self) {`;
- change `fn breaker(&self) -> &Breaker {` to `pub(crate) fn breaker(&self) -> &Breaker {`;
- after `crawl_paused`, add:

```rust
    /// Records a name or CID for the resolver (#213); false if it's known already
    pub fn discover(&self, entry: &str, source: &str) -> rusqlite::Result<bool> {
        self.store().discover(entry, source, now_secs()).inspect_err(|err| {
            warn!("Couldn't record {}: {}", entry, err);
            self.store().note_error(err);
        })
    }

    /// Every discovered entry, for /discovered (empty if the store can't be read)
    pub fn discovered(&self) -> Vec<store::Discovered> {
        self.store().discovered_all().unwrap_or_else(|err| {
            warn!("Couldn't list discovered entries: {}", err);
            self.store().note_error(&err);
            Vec::new()
        })
    }

    pub fn discovered_count(&self) -> usize {
        self.store().discovered_count()
    }
```

Put the module code above the tests in `src/discovery.rs`:

```rust
/*!
 * Discovery (#213): entries from the seeds file, ENS and submissions live in the store's
 * `discovered` table. The resolver turns each into a root (a name through the gateway's /ipns/),
 * queues it through enqueue() and checks it again daily. New roots are capped per hour.
 */
use crate::ens::{self, Rpc, RpcError, Scan};
use crate::entry::{self, Contenthash, Entry};
use crate::index_queue::{Enqueued, IndexQueue};
use crate::store::{now_secs, Discovered};
use log::{debug, info, warn};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

const HOUR: i64 = 60 * 60;
const DAY: i64 = 24 * HOUR;
// the ENS scan's next block, in discovery_state
const ENS_NEXT_BLOCK: &str = "ens_next_block";

/// At most `per_hour` new roots an hour, across every source (0: no limit)
pub struct HourlyCap {
    per_hour: u32,
    // the hour and how many it has used
    used: Mutex<(i64, u32)>,
}

impl HourlyCap {
    pub fn new(per_hour: u32) -> HourlyCap {
        HourlyCap {
            per_hour,
            used: Mutex::new((-1, 0)),
        }
    }

    /// Takes one from this hour's allowance; false if it's used up
    pub fn take(&self, now: i64) -> bool {
        if self.per_hour == 0 {
            return true;
        }
        let mut used = self.used.lock().unwrap_or_else(|e| e.into_inner());
        if used.0 != now / HOUR {
            *used = (now / HOUR, 0);
        }
        if used.1 >= self.per_hour {
            return false;
        }
        used.1 += 1;
        true
    }
}

// the start of the next hour, when a capped entry is tried again
fn next_hour(now: i64) -> i64 {
    (now / HOUR + 1) * HOUR
}

// how long after a failure an entry is tried again: an hour, doubling, at most a day
fn retry_after(failures: u32) -> i64 {
    (HOUR << failures.min(5)).min(DAY)
}

/// The root CID the gateway resolves a name to: GET /ipns/<name>/ and the first CID in its
/// X-Ipfs-Roots header. The body isn't read.
pub fn resolve(gateway: &str, name: &str) -> Result<String, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?;
    let response = client
        .get(format!("http://{}/ipns/{}/", gateway, name))
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    let roots = response
        .headers()
        .get("x-ipfs-roots")
        .and_then(|v| v.to_str().ok())
        .ok_or("no X-Ipfs-Roots header")?;
    let root = roots.split(',').next().unwrap_or("").trim();
    match entry::parse(root) {
        Some(Entry::Cid(cid)) if !cid.contains('/') => Ok(cid),
        _ => Err(format!("not a CID in X-Ipfs-Roots: {:.100}", roots)),
    }
}

/// What checking a due entry came to
#[derive(Debug, PartialEq)]
pub enum Checked {
    /// a new root, queued
    Queued(String),
    /// the root it had, queued again (a no-op unless its pages expired)
    Unchanged(String),
    /// a new root over the hourly cap: tried again next hour
    Capped,
    Failed(String),
}

/**
 * Checks one due entry: a CID is its own root, a name is resolved through the gateway. A new
 * root is queued if the hourly cap allows; otherwise the entry is put off to the next hour,
 * uncounted. An unchanged root is queued again, uncapped, which re-crawls a site once its pages
 * expire. Then the entry is due again in a day, or after a failure in an hour, doubling up to a
 * day.
 */
pub fn check(queue: &IndexQueue, gateway: &str, cap: &HourlyCap, row: &Discovered, now: i64) -> Checked {
    let store = queue.store();
    let root = match entry::parse(&row.entry) {
        Some(Entry::Cid(cid)) => Ok(cid),
        Some(Entry::Name(name)) => {
            queue.wait_for_gateway();
            resolve(gateway, &name)
        }
        None => Err("not a CID or a name".to_string()),
    };
    let outcome = match root {
        Err(err) => Checked::Failed(err),
        Ok(root) => {
            let new = row.root.as_deref() != Some(root.as_str());
            if new && !cap.take(now) {
                if let Err(err) = store.discovered_postpone(&row.entry, next_hour(now)) {
                    warn!("Couldn't put off {}: {}", row.entry, err);
                    store.note_error(&err);
                }
                return Checked::Capped;
            }
            match queue.enqueue(root.clone()) {
                Enqueued::Queued if new => Checked::Queued(root),
                Enqueued::Queued => Checked::Unchanged(root),
                Enqueued::Blocked => Checked::Failed(format!("{} is blocked", root)),
                Enqueued::InvalidCid => Checked::Failed(format!("{} is not a valid CID", root)),
            }
        }
    };
    let written = match &outcome {
        Checked::Queued(root) | Checked::Unchanged(root) => {
            store.discovered_ok(&row.entry, root, now, now + DAY)
        }
        Checked::Failed(err) => {
            warn!("Discovery: couldn't check {}: {}", row.entry, err);
            store.discovered_failed(&row.entry, err, now, now + retry_after(row.failures))
        }
        Checked::Capped => Ok(()),
    };
    if let Err(err) = written {
        warn!("Couldn't save the check of {}: {}", row.entry, err);
        store.note_error(&err);
    }
    outcome
}

/// Checks the next due entry, unless the crawl is paused (#284) or none is due
pub fn resolve_next(queue: &IndexQueue, gateway: &str, cap: &HourlyCap) -> Option<Checked> {
    if queue.crawl_paused().is_some() {
        return None;
    }
    let now = now_secs();
    let row = match queue.store().discovered_due(now, 1) {
        Ok(mut due) => due.pop()?,
        Err(err) => {
            warn!("Couldn't read due entries: {}", err);
            queue.store().note_error(&err);
            return None;
        }
    };
    let checked = check(queue, gateway, cap, &row, now);
    debug!("Discovery: {} {:?}", row.entry, checked);
    Some(checked)
}

/// The resolver thread: at most one entry a second; with none due, or the crawl paused, it
/// looks again in 10 s
pub fn run_resolver(queue: &IndexQueue, gateway: &str, cap: &HourlyCap) {
    loop {
        let pause = match resolve_next(queue, gateway, cap) {
            Some(_) => 1,
            None => 10,
        };
        std::thread::sleep(Duration::from_secs(pause));
    }
}

/// A seeds file's entries: one a line, "#" comments and blank lines skipped. A line that isn't
/// an entry is skipped with a warning.
pub fn parse_seeds(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|line| {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                return None;
            }
            let parsed = entry::parse(line);
            if parsed.is_none() {
                warn!("Seeds file: skipping {:?}: not a CID or a name", line);
            }
            parsed
        })
        .collect()
}

/// Records the seeds file's entries, new ones due now; known ones stay as they are. Returns how
/// many were new.
pub fn add_seeds(queue: &IndexQueue, entries: &[Entry], now: i64) -> usize {
    entries
        .iter()
        .filter(|e| match queue.store().discover(e.as_str(), "seeds", now) {
            Ok(new) => new,
            Err(err) => {
                warn!("Couldn't record seed {}: {}", e.as_str(), err);
                queue.store().note_error(&err);
                false
            }
        })
        .count()
}

/// The seeds thread: reads the file at start and every hour. An unreadable file is logged; the
/// entries read before are in the store and stay.
pub fn run_seeds(queue: &IndexQueue, path: &Path) {
    loop {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let entries = parse_seeds(&text);
                let new = add_seeds(queue, &entries, now_secs());
                if new > 0 {
                    info!("Seeds file: {} new of {} entries", new, entries.len());
                }
            }
            Err(err) => warn!(
                "Couldn't read seeds file {}: {}; keeping the entries read before",
                path.display(),
                err
            ),
        }
        std::thread::sleep(Duration::from_secs(HOUR as u64));
    }
}

/// Stores ENS events: a node's IPFS or IPNS record becomes its entry (due now), a cleared one
/// drops it, anything else (Swarm...) is skipped. Returns how many were skipped.
pub fn apply_ens(queue: &IndexQueue, events: &[ens::Event], now: i64) -> usize {
    let mut skipped = 0;
    for event in events {
        let entry = match &event.record {
            Contenthash::Set(entry) => Some(entry.as_str()),
            Contenthash::Cleared => None,
            Contenthash::Other => {
                skipped += 1;
                continue;
            }
        };
        if let Err(err) = queue.store().discover_ens(&event.node, entry, now) {
            warn!("Couldn't record ENS node {}: {}", event.node, err);
            queue.store().note_error(&err);
        }
    }
    skipped
}

/// Where the ENS scan resumes: the saved block, or START_BLOCK
pub fn ens_start(queue: &IndexQueue) -> Scan {
    let saved = queue.store().state(ENS_NEXT_BLOCK).ok().flatten();
    Scan::starting(saved.and_then(|v| v.parse().ok()).unwrap_or(ens::START_BLOCK))
}

/**
 * One pass of the ENS scan, from where it is to the latest block, with `pause` between requests.
 * The position is saved after each one. A JSON-RPC error halves the range and the pass carries
 * on; at one block, or on a transport error, it stops with the error.
 */
pub fn ens_pass(queue: &IndexQueue, rpc: &dyn Rpc, scan: &mut Scan, pause: Duration) -> Result<(), RpcError> {
    let latest = ens::latest_block(rpc)?;
    while scan.next <= latest {
        let range = scan.range;
        match ens::step(rpc, scan, latest) {
            Ok(events) => {
                let skipped = apply_ens(queue, &events, now_secs());
                debug!("ENS scan: {} records, {} skipped, next block {}", events.len(), skipped, scan.next);
                if let Err(err) = queue.store().set_state(ENS_NEXT_BLOCK, &scan.next.to_string()) {
                    warn!("Couldn't save the ENS scan's position: {}", err);
                    queue.store().note_error(&err);
                }
            }
            Err(RpcError::Rpc(msg)) if range > 1 => {
                debug!("ENS scan: {}; asking for {} blocks", msg, scan.range);
            }
            Err(err) => return Err(err),
        }
        std::thread::sleep(pause);
    }
    Ok(())
}

/// The ENS thread: a pass every 10 minutes; after an error, 1 minute, doubling up to an hour
pub fn run_ens(queue: &IndexQueue, rpc: &dyn Rpc) {
    let mut scan = ens_start(queue);
    let mut backoff = Duration::from_secs(60);
    loop {
        match ens_pass(queue, rpc, &mut scan, Duration::from_secs(1)) {
            Ok(()) => {
                backoff = Duration::from_secs(60);
                std::thread::sleep(Duration::from_secs(600));
            }
            Err(err) => {
                warn!("ENS scan: {} at block {}; trying again in {:?}", err, scan.next, backoff);
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(3600));
            }
        }
    }
}
```

- [ ] **Step 4: Run all tests**

Run: `cargo fmt && cargo test --quiet 2>&1 | grep "test result"`
Expected: all pass, with 15 new. `run_resolver`, `run_seeds` and `run_ens` stay unused until Task 5, so they give dead-code warnings for now.

- [ ] **Step 5: Commit**

```bash
git add src/discovery.rs src/index_queue.rs src/main.rs
git commit -m "Discovery: hourly cap, resolver, seeds file and ENS passes (#213)"
```

---

### Task 5: Wire it up: settings, threads, `/enqueue`, `/discovered`, `/status`, docs (`src/main.rs`, `README.md`, `seeds.example.txt`)

**Files:**
- Modify: `src/main.rs`:
  - `status` (about line 41);
  - `enqueue` (about line 134);
  - a new `discovered` handler;
  - a new `discovery_per_hour` next to `health_min_rate`;
  - a new `keep_running` helper;
  - `main`, after the crawl workers start (the `for _ in 0..n_workers` loop);
  - the `HttpServer` services;
  - tests.
- Modify: `README.md`: the settings table, the HTTP API table and a new "Discovery" section after "Denylists".
- Create: `seeds.example.txt`

**Interfaces:**
- Consumes:
  - Task 4's `discovery::{HourlyCap, run_resolver, run_seeds, run_ens}`;
  - Task 3's `ens::HttpRpc`;
  - Task 2's `entry::{parse, Entry}`;
  - Task 4's `IndexQueue::{discover, discovered, discovered_count}`.
- Produces: `fn discovery_per_hour(value: Option<String>) -> Result<u32, String>`.

- [ ] **Step 1: Write the failing tests** (in `src/main.rs` `mod tests`, after `health_min_rate_defaults_to_500_and_rejects_non_numbers`)

```rust
    #[actix_web::test]
    async fn discovery_per_hour_defaults_to_500_and_rejects_non_numbers() {
        assert_eq!(discovery_per_hour(None), Ok(500));
        assert_eq!(discovery_per_hour(Some("0".to_string())), Ok(0));
        assert_eq!(discovery_per_hour(Some("2000".to_string())), Ok(2000));
        for bad in ["", "-1", "lots", "1.5"] {
            assert!(discovery_per_hour(Some(bad.to_string())).is_err(), "{:?}", bad);
        }
    }

    #[actix_web::test]
    async fn enqueue_records_names_for_the_resolver() {
        let index_queue = web::Data::new(IndexQueue::new());
        let app = test::init_service(App::new().app_data(index_queue.clone()).service(enqueue)).await;
        for (uri, code, body) in [
            ("/enqueue/docs.ipfs.tech", 200, "Queued docs.ipfs.tech to resolve"),
            ("/enqueue/Docs.IPFS.tech", 200, "Already known: docs.ipfs.tech"),
            ("/enqueue/vitalik.eth", 200, "Queued vitalik.eth to resolve"),
            ("/enqueue/two%20words.com", 400, "Not a valid CID or name: two words.com"),
        ] {
            let resp = test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
            assert_eq!(resp.status().as_u16(), code, "{}", uri);
            let got = test::read_body(resp).await;
            assert_eq!(std::str::from_utf8(&got).unwrap(), body, "{}", uri);
        }
        assert_eq!(index_queue.queue_length(), 0, "names wait for the resolver");
        let sources: Vec<String> = index_queue.discovered().into_iter().map(|d| d.source).collect();
        assert_eq!(sources, vec!["submitted", "submitted"]);
    }

    #[actix_web::test]
    async fn discovered_lists_entries_and_status_counts_them() {
        let index_queue = web::Data::new(IndexQueue::new());
        index_queue.discover("docs.ipfs.tech", "seeds").unwrap();
        let app = test::init_service(
            App::new()
                .app_data(index_queue.clone())
                .app_data(web::Data::new(Gateway("gw.example".to_string())))
                .service(discovered)
                .service(status),
        )
        .await;
        let body = test::call_and_read_body(&app, test::TestRequest::get().uri("/discovered").to_request()).await;
        assert_eq!(std::str::from_utf8(&body).unwrap(), "docs.ipfs.tech\tseeds\t-\t-\t-");
        let body = test::call_and_read_body(&app, test::TestRequest::get().uri("/status").to_request()).await;
        assert!(
            std::str::from_utf8(&body).unwrap().contains(" Discovered: 1 Gateway: gw.example"),
            "{:?}",
            body
        );
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --quiet -- discovery_per_hour enqueue_records discovered_lists 2>&1 | grep -E "^error" | sort | uniq -c`
Expected: compile errors. `discovery_per_hour` and the `discovered` handler don't exist yet.

- [ ] **Step 3: Implement**

Imports, at the top of `src/main.rs` after `use crate::denylist::Denylist;`:

```rust
use crate::entry::Entry;
```

`status`: put `Discovered: {} ` before `Gateway: {}` in the format string, and the value `queue.discovered_count(),` before `gateway.0,`:

```rust
        "Queue length: {} Index size: {} Mirrors skipped: {} Number of Keywords: {} Indexed per minute: {} Discovered: {} Gateway: {} ({}) Version: {}",
        queue.queue_length(),
        queue.index_length(),
        queue.mirror_length(),
        queue.keyword_length(),
        queue.indexed_per_minute(),
        queue.discovered_count(),
        gateway.0,
```

Replace `enqueue` with:

```rust
// {item:.*} so "cid/path" submissions match too. A name (a DNSLink domain, a .eth name or an
// IPNS key) is recorded for the resolver, which queues its root (#213); the rest is a CID as before.
#[get("/enqueue/{item:.*}")]
async fn enqueue(data: web::Data<IndexQueue>, item: web::Path<String>) -> HttpResponse {
    let item = item.into_inner();
    if let Some(Entry::Name(name)) = entry::parse(&item) {
        return match data.discover(&name, "submitted") {
            Ok(true) => HttpResponse::Ok().body(format!("Queued {} to resolve", name)),
            Ok(false) => HttpResponse::Ok().body(format!("Already known: {}", name)),
            Err(err) => HttpResponse::InternalServerError().body(format!("Couldn't record {}: {}", name, err)),
        };
    }
    match data.enqueue(item.clone()) {
        Enqueued::Queued => HttpResponse::Ok().body(format!("Enqueued {}", item)),
        Enqueued::InvalidCid => {
            HttpResponse::BadRequest().body(format!("Not a valid CID or name: {}", item))
        }
        Enqueued::Blocked => {
            HttpResponse::Forbidden().body(format!("Blocked by denylist: {}", item))
        }
    }
}

/// One line per discovered entry (#213): entry, source, root, last checked (unix time) and the
/// last error, tab-separated ("-" for none), most recently checked first
#[get("/discovered")]
async fn discovered(queue: web::Data<IndexQueue>) -> HttpResponse {
    let dash = |v: Option<String>| v.unwrap_or_else(|| "-".to_string());
    let lines: Vec<String> = queue
        .discovered()
        .into_iter()
        .map(|d| {
            format!(
                "{}\t{}\t{}\t{}\t{}",
                d.entry,
                d.source,
                dash(d.root),
                dash(d.checked_at.map(|t| t.to_string())),
                dash(d.error.map(|e| e.replace(['\t', '\n'], " ")))
            )
        })
        .collect();
    HttpResponse::Ok().body(lines.join("\n"))
}
```

Remove the old comment line `// {item:.*} so "cid/path" submissions match too` above `enqueue`, because the new comment replaces it.

After `health_min_rate`, add:

```rust
/// New discovered roots queued an hour: `DISCOVERY_PER_HOUR`, or 500 when unset; 0 is no limit
/// (#213)
fn discovery_per_hour(value: Option<String>) -> Result<u32, String> {
    match value {
        None => Ok(500),
        Some(value) => value.parse::<u32>().map_err(|_| {
            format!(
                "DISCOVERY_PER_HOUR must be a whole number of roots an hour (0 = no limit), got {:?}",
                value
            )
        }),
    }
}

// runs a background loop on its own thread; one that panics is logged and started again a
// minute later
fn keep_running(name: &'static str, run: impl Fn() + Send + 'static) {
    std::thread::spawn(move || loop {
        if std::panic::catch_unwind(AssertUnwindSafe(&run)).is_err() {
            error!("{} panicked; starting it again in a minute", name);
            std::thread::sleep(Duration::from_secs(60));
        }
    });
}
```

In `main`, right after the `health_config` block, read the cap:

```rust
    let per_hour = discovery_per_hour(std::env::var("DISCOVERY_PER_HOUR").ok()).unwrap_or_else(|err| {
        error!("{}", err);
        std::process::exit(1);
    });
```

After the `for _ in 0..n_workers { ... }` loop that starts the crawl workers, add:

```rust
    // discovery (#213): the resolver always runs, since submitted names need it; each source runs
    // only when configured
    let cap = Arc::new(discovery::HourlyCap::new(per_hour));
    {
        let (queue, gateway, cap) = (Arc::clone(&index_queue), gateway.clone(), Arc::clone(&cap));
        keep_running("Discovery resolver", move || discovery::run_resolver(&queue, &gateway, &cap));
    }
    match std::env::var("SEEDS_FILE").ok().filter(|p| !p.is_empty()) {
        Some(path) => {
            info!("Seeds file: {}", path);
            let queue = Arc::clone(&index_queue);
            keep_running("Seeds file", move || discovery::run_seeds(&queue, std::path::Path::new(&path)));
        }
        None => info!("Seeds file: none (no SEEDS_FILE)"),
    }
    // the URL carries an API key: only its host is ever logged
    match std::env::var("ETH_RPC_URL").ok().filter(|u| !u.is_empty()) {
        Some(url) => match ens::HttpRpc::new(url) {
            Ok(rpc) => {
                info!("ENS scan: through {}", rpc.host());
                let queue = Arc::clone(&index_queue);
                let rpc = Arc::new(rpc);
                keep_running("ENS scan", move || discovery::run_ens(&queue, rpc.as_ref()));
            }
            Err(err) => {
                error!("ETH_RPC_URL: {}", err);
                std::process::exit(1);
            }
        },
        None => info!("ENS scan: off (no ETH_RPC_URL)"),
    }
```

In `HttpServer::new`, add `.service(discovered)` after `.service(roots)`.

Change the existing test `test_status_get`'s expectation only if it breaks. It checks `Gateway: gw.example (no fetches yet)`, which still matches.

Create `seeds.example.txt`:

```text
# Seeds for discovery (#213): one CID, cid/path, DNSLink domain, .eth name or IPNS key a line.
# Set SEEDS_FILE to a copy of this; it's read at start and every hour. Names are resolved through
# the gateway and checked again daily.

# Wikipedia mirrors in other languages (distributed-wikipedia-mirror)
tr.wikipedia-on-ipfs.org
ar.wikipedia-on-ipfs.org
zh.wikipedia-on-ipfs.org
uk.wikipedia-on-ipfs.org
ru.wikipedia-on-ipfs.org
fa.wikipedia-on-ipfs.org
my.wikipedia-on-ipfs.org

# IPFS and libp2p project sites
docs.ipfs.tech
specs.ipfs.tech
blog.ipfs.tech
ipfs.tech
libp2p.io

# .eth sites
vitalik.eth
ens.eth
```

`README.md`:
- Add to the settings table, after the `HEALTH_MIN_RATE` row:

```markdown
| `SEEDS_FILE` | unset | A file of sites to discover (see [Discovery](#discovery)): one CID, `cid/path`, DNSLink domain, `.eth` name or IPNS key a line, `#` comments. Read at start and every hour. `seeds.example.txt` is a starting list. |
| `ETH_RPC_URL` | unset | An Ethereum JSON-RPC endpoint with an API key (Alchemy, Infura, drpc...), for discovering `.eth` sites from ENS records. Keyless public endpoints don't serve old logs. Only its host is logged. |
| `DISCOVERY_PER_HOUR` | `500` | The most new discovered roots queued an hour, across every source (`0` = no limit). |
```

- In the HTTP API table, replace the `/enqueue` row with:

```markdown
| `GET /enqueue/{cid}` or `/enqueue/{cid}/{path}` | Queues a page: `200` queued, `400` not a valid CID or name, `403` blocked by a denylist. A name instead (a DNSLink domain like `docs.ipfs.tech`, a `.eth` name or an IPNS key, optionally as `/ipns/...` or `ipns://...`) is recorded for discovery: `200 Queued <name> to resolve` or `200 Already known: <name>`. |
```

- Add after the `/roots` row:

```markdown
| `GET /discovered` | One line per discovered entry (see [Discovery](#discovery)): entry, source (`seeds`, `ens` or `submitted`), the last root queued, when it was last checked (unix time) and the last error, tab-separated, `-` for none, most recently checked first. |
```

- In the `/status` row, change `gateway health and version on one line.` to `discovered entries (`Discovered`), gateway health and version on one line.`.
- After the "Denylists" section, add:

```markdown
## Discovery
Besides following links from the seed (the Wikipedia mirror) and `/enqueue` submissions, the indexer finds sites from:
- **a seeds file** (`SEEDS_FILE`): CIDs and names you list;
- **ENS** (`ETH_RPC_URL`): every `.eth` name whose content record points at IPFS or IPNS, read from `ContenthashChanged` events. The first scan walks from 2017 in requests of up to 10,000 blocks (fewer if the provider objects), then only new blocks every 10 minutes;
- **submitted names** (`/enqueue/<name>`).

Each entry is kept in `index.sqlite` and listed by `/discovered`. A name is resolved through the gateway (`/ipns/<name>/`, its `X-Ipfs-Roots` header), at most one a second. Its root is queued through the same checks as any page (CID, denylists, HTML only), and checked again daily. When a site publishes a new root, the new one is crawled; an unchanged root is queued again, which re-crawls the site once its pages expire. Failures are retried after an hour, doubling up to a day. At most `DISCOVERY_PER_HOUR` new roots are queued an hour. Each source is off until configured; submitted names always work.
```

- [ ] **Step 4: Run all tests, and try the binary**

Run: `cargo fmt && cargo test --quiet 2>&1 | grep "test result"`
Expected: all pass, with 3 new, and no dead-code warnings from the new modules.

Run: `DISCOVERY_PER_HOUR=lots RUST_LOG=info timeout 60 ./target/debug/ipfs_indexer 127.0.0.1:1 2>&1 | grep -E "DISCOVERY_PER_HOUR"; echo "exit=${PIPESTATUS[0]}"`. Build first with `cargo build --quiet`.
Expected: the error line, and `exit=1`.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs README.md seeds.example.txt
git commit -m "Discovery: SEEDS_FILE, ETH_RPC_URL and DISCOVERY_PER_HOUR; names via /enqueue; /discovered (#213)"
```

---

### Task 6: Release v0.5.0

**Files:**
- Modify: `Cargo.toml` (`version = "0.4.6"` to `"0.5.0"`), `Cargo.lock` (updated by the build)
- Modify: `docs/superpowers/specs/2026-10-17-discovery-design.md`: change `**Status:** agreed` to `**Status:** implemented (v0.5.0)`.

- [ ] **Step 1: Bump and build**

Run: `sed -i 's/^version = "0.4.6"/version = "0.5.0"/' Cargo.toml && cargo build --quiet && grep -A1 'name = "ipfs_indexer"' Cargo.lock`
Expected: `version = "0.5.0"`.

- [ ] **Step 2: Full test run**

Run: `cargo fmt --check && cargo test --quiet 2>&1 | grep "test result"`
Expected: all pass.

- [ ] **Step 3: The manual live check** (no key needed for recent blocks)

Run a throwaway test outside the repo, or a `#[ignore]`d test you delete afterwards. It calls `ens::step` with `HttpRpc::new("https://rpc.flashbots.net".into())` over the last 10,000 blocks (`latest_block` first), and prints how many events decode to `Set`, `Cleared` and `Other`.
Expected: some dozens of events, mostly `Set(Cid(..))`. That confirms the topic hash and decoding against real data. Record the numbers in the ledger.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock docs/superpowers/specs/2026-10-17-discovery-design.md
git commit -m "v0.5.0"
```
