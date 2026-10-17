# Discover sites beyond links: seeds, DNSLink, ENS and submitted names (#213, part 1): design

**Status:** agreed · **Date:** 2026-10-17

## Goal

Until now the crawler has found pages in two ways: by following links from one seed, the English Wikipedia mirror, and from `/enqueue` submissions. That mirror is almost fully crawled, and the queue empties within a day. Wikipedia holds 7.97M pages, and every other root has one or two.

**Outcome:** the index keeps growing with new IPFS websites, so darksearch has more worth searching.

**Constraints (from #213):**
- every discovered root goes through `enqueue()`: CID validation, the badbits and local denylists, and HTML-only pages up to 2 MB;
- discovered roots are rate-limited so they can't flood the queue;
- every source is off until it's configured;
- kubo stays hardened: no providing, DHT client only, Bitswap server off.

**Success:**
1. Within a week of enabling it, a measurable stream of new sites appears in `/discovered` and `/roots`, with their pages indexed.
2. The crawl stays inside the same safety limits.
3. Normal crawling, search and the API are otherwise unchanged.

## Decisions (agreed)

- **#213 is split into three.** This spec is part 1: a discovery framework plus a curated seeds file, DNSLink, ENS `.eth` names and submitted names.
  - Part 2 is a spike on cid.contact (#291).
  - Part 3 is a small DHT server that listens for announcements without storing content (#292).
  - Alternatives for listing ENS sites are in #293: the ENS subgraph, or published lists.
- **It runs inside the indexer,** one background thread per source. The rate limit, `enqueue()`, the denylists and `roots` all live there, and it's one binary and one deploy.
- **Names are resolved through the existing gateway.** kubo already resolves DNSLink and `.eth`: `/ipns/docs.ipfs.tech/` answers 200 with `X-Ipfs-Roots`, and `vitalik.eth` resolves through eth.limo. So the indexer needs no DNS or ENS client of its own.
- **ENS listing reads `ContenthashChanged` events with `eth_getLogs`, through an RPC provider's free API key** (`ETH_RPC_URL`, which carries the key). A live check on 2026-10-17 showed why a key is needed: no keyless public endpoint serves old logs. publicnode needs a token for archive ranges, Flashbots has pruned its history, and 1rpc and Blast allow 10–50 blocks per request. Recent blocks did work: 153 events across 74 nodes in the last 10,000 blocks (146 `ipfs-ns`, 1 `ipns-ns`, 5 Swarm, 1 cleared).
- **Submitted names are accepted by `/enqueue`,** so darksearch's Submit page can add them.
- **There is no master switch.** A source runs only when it's configured.

## 1. Entries and sources

An **entry** is anything a source produces:
- a CID, or a `cid/path`;
- a DNSLink domain, such as `docs.ipfs.tech`;
- a `.eth` name, such as `vitalik.eth`;
- an IPNS key, a CID with the libp2p-key codec, such as `k51…`.

Names may also be written as `/ipns/<name>` or `ipns://<name>`. CIDs may be written as `/ipfs/<cid>` or `ipfs://<cid>`.

**Valid names:**
- a domain is 2 or more labels, each 1–63 characters of `[a-z0-9-]`, not starting or ending with `-`, at most 253 characters in all;
- an IP address isn't a name, and neither is a single label;
- names are lowercased.

**What happens to an entry:**
- **A CID entry** goes straight to `enqueue()`. Nothing more is tracked.
- **A name entry** is recorded in `discovered` and resolved by the resolver (section 2). Its root goes to `enqueue()`.

**The sources:**

- **The seeds file** (`SEEDS_FILE`):
  - One entry per line. `#` starts a comment and blank lines are ignored.
  - It's read at start and again every hour.
  - A line that isn't a valid entry is skipped with a warning.
  - An unreadable file keeps the entries read last time, with a warning. Unlike `LOCAL_DENYLIST`, a missing seeds file is never fatal, since it only adds pages.
  - Its CIDs are queued each time it's read. `enqueue()` skips CIDs already indexed or queued.
  - Its names are added to `discovered` with source `seeds`. Entries later removed from the file are kept and still re-checked.
  - The repo ships `seeds.example.txt`:
    - the other Wikipedia mirrors: `tr.`, `ar.`, `zh.`, `uk.`, `ru.`, `fa.` and `my.wikipedia-on-ipfs.org`;
    - IPFS project sites: `docs.ipfs.tech`, `specs.ipfs.tech`, `blog.ipfs.tech`, `ipfs.tech` and `libp2p.io`;
    - a few known `.eth` sites.

    iac deploys the real file.
- **ENS** (`ETH_RPC_URL`; off when unset):
  - **Calls:** JSON-RPC `eth_getLogs` with `topics: [ContenthashChanged(bytes32,bytes)]` and no address, so the filter covers every resolver contract.
  - **Range:** it walks from ENS's first registry deployment (block 3,327,417) to the latest block, with 1 s between requests. Requests start at 10,000 blocks. Providers cap ranges and result counts differently, so when the provider answers with a JSON-RPC error, the range is halved, down to 1 block, and the request retried after 1 s. Each successful request doubles it again, back up to 10,000. The first pass is about 2,300 requests at full size. After that, each pass reads only new blocks, every 10 minutes.
  - **The key stays out of logs.** `ETH_RPC_URL` is never logged; only its host is, at start. Transport errors are logged without the URL (`reqwest::Error::without_url`).
  - **Decoding:** each event's `topics[1]` is the ENS node, and its `data` is ABI-encoded bytes holding the EIP-1577 contenthash:
    - `0xe3 0x01 …` (`ipfs-ns`): the rest is a CID, so it's a CID entry;
    - `0xe5 0x01 …` (`ipns-ns`): the rest is a CID with the libp2p-key codec (an IPNS key), or the identity multihash of a DNS name (a domain). Either way it's a name entry;
    - empty: the record was cleared, so the node is removed;
    - anything else (Swarm, Onion, junk) is skipped and counted.
  - **Bookkeeping:** only the latest record per node counts, and it's stored with source `ens` keyed by node. A changed record replaces the node's entry. The scan position (the last block read) is saved after each request.
  - JSON-RPC needs `serde_json` (new dependency). Decoding uses the `cid` crate already used.
- **Submitted names** (`/enqueue`, section 2) are added with source `submitted`.

## 2. Resolving, submissions, limits

**The resolver:** one thread, always running, idle when nothing is due.
- **Pace:** at most 1 name a second.
- **Each resolve:** a `GET http://<gateway>/ipns/<name>/` with a 30 s timeout, reading the root from `X-Ipfs-Roots`. Only the first CID is used, and the body isn't read.
- **When it succeeds:**
  - if the root differs from the entry's last root, it goes to `enqueue()` and is stored;
  - the next check is 1 day later.
- **When it fails** (an error status, no header, a timeout, or a root `enqueue()` refuses as blocked or invalid):
  - the error is stored on the entry, with a warning;
  - the next try is 1 hour later, doubling up to 1 day.
- **During a #284 pause** (`crawl_paused()` is `Some`), the resolver waits too.
- **Changed roots:** a new root is a different CID, so all its pages are new items. The old root's pages expire with `INDEX_MAX_AGE_DAYS`.

**Rate limit:** at most `DISCOVERY_PER_HOUR` new roots go to `enqueue()` per hour (default 500; `0` means no limit).
- The limit covers roots from every source: seeds CIDs, ENS CIDs and resolved names, submitted ones included. A CID submitted to `/enqueue` isn't limited, as now. Links found while crawling aren't limited either.
- A CID entry over the limit stays pending in `discovered` (source, root, and next check now) and is queued in a later hour. So CID entries are tracked too once the limit holds them back.
- A resolve whose root is over the limit doesn't count as checked: it's retried in the next hour.
- An invalid value stops the indexer at start, as `CRAWL_WORKERS` does.

**`/enqueue/{item}`:**
- **CIDs:** unchanged: `200` queued, `400` not a valid CID, `403` blocked.
- **Names:** recorded with source `submitted` and resolved by the resolver. It returns `200 Queued <name> to resolve`, or `200 Already known: <name>` when the name was already recorded.
- **Anything else** returns `400`. The message says it's neither a CID nor a name.
- **Denylists:** a name can't be checked against them until it's resolved. A blocked root is refused when it's queued, and the entry stores "blocked".

**Store** (`index.sqlite`):
- `discovered (entry TEXT PRIMARY KEY, source TEXT NOT NULL, node TEXT, root TEXT, checked_at INTEGER, next_at INTEGER NOT NULL, failures INTEGER NOT NULL DEFAULT 0, error TEXT)`:
  - `node` is the ENS node, for `ens` entries;
  - `root` is the last root queued;
  - `next_at` is when it's due.
- `discovery_state (key TEXT PRIMARY KEY, value TEXT NOT NULL)` holds the ENS scan position.
- Both are created on open if missing, so a v0.4.6 store simply gains them.

**HTTP:**
- `/status` gains `Discovered: <n>`, the number of entries in `discovered`.
- `GET /discovered` lists one line per entry, tab-separated like `/roots`, most recently checked first: entry, source, root (or `-`), checked (unix time or `-`), error (or `-`). No token needed: entries are public names and CIDs.

**Errors:**
- **ENS endpoint:**
  - A JSON-RPC error halves the range and retries, as above.
  - A transport failure, or an error at a 1-block range, is logged. The scan keeps its position and retries after 1 minute, doubling up to 1 hour. Nothing is lost.
- **Store errors:** logged and noted with `note_error`, as other store writes do. That source's pass ends and runs again at its next interval.

**Safety:** nothing here fetches page content except through the gateway, and every root passes through `enqueue()`. The resolver only reads headers. The ENS source talks only to `ETH_RPC_URL`.

## Testing

- **Entries:**
  - CIDs and `cid/path`, with and without `/ipfs/` or `ipfs://`;
  - domains, `.eth` names and `k51…` keys, with and without `/ipns/` or `ipns://`;
  - lowercasing;
  - rejected: a single label, spaces, IPv4 and IPv6 addresses, an overlong label or name, a label with a leading `-`.
- **Contenthash:**
  - `ipfs-ns` with a CIDv0 and a CIDv1;
  - `ipns-ns` with a libp2p-key, and with an identity-encoded domain;
  - empty;
  - Swarm and garbage, skipped.
- **ENS scan,** against saved `eth_getLogs` JSON, with no network in CI:
  - events decode, and the latest per node wins;
  - an empty record removes the node;
  - the position is saved and survives a restart;
  - 10,000-block ranges, halved on a JSON-RPC error and doubled back after successes;
  - a transport error backs off and keeps the position;
  - the URL never appears in an error message.
- **Resolver,** against a local test gateway sending `X-Ipfs-Roots`:
  - a name's root is queued;
  - an unchanged root isn't queued again, and a changed one is;
  - a failure stores the error and backs off, doubling;
  - it waits while `crawl_paused()`;
  - the hourly limit holds roots back to the next hour.
- **Seeds file:** comments and blank lines; a bad line is skipped; re-reading it adds new entries; an unreadable file keeps the last entries.
- **HTTP:**
  - `/enqueue` returns 200 for a new name and for a known one, 400 for junk, and is unchanged for CIDs;
  - `/discovered` lines;
  - `Discovered:` in `/status`.
- **Store:** a v0.4.6 store opens and gains both tables.
- **End to end:** a seeds file names a domain; a fake gateway resolves it and serves the site; its pages are indexed.
- **Manual live check, not in CI:** one `eth_getLogs` over a small recent block range through a public endpoint. It confirms the event signature hash and decoding against real ENS data.

## Release

v0.5.0.
- **iac PR:**
  - deploy the seeds file, starting from `seeds.example.txt`;
  - set `SEEDS_FILE`;
  - set `ETH_RPC_URL` from a secret: a free API key from an RPC provider, which you create;
  - leave `DISCOVERY_PER_HOUR` at its default.
- **darksearch PR:** the Submit page accepts names and says so.
- **One week after deploy:** count discovered entries, resolved roots and their pages from `/discovered` and `/roots`.

Closes #275 (IPNS resolution and polling): the resolver and its daily checks cover it.

## Out of scope

- cid.contact (#291), the DHT server (#292), and other ENS listing sources (#293).
- A pause per site (#286). The queue is first in, first out, so a large new site crawls ahead of smaller ones found after it. A fairer order belongs with #286.
- IPNS over pubsub.
