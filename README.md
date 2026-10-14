# ipfs_indexer
[![.github/workflows/build-and-test.yml](https://github.com/compscidr/ipfs_indexer/actions/workflows/build-and-test.yml/badge.svg)](https://github.com/compscidr/ipfs_indexer/actions/workflows/build-and-test.yml)

A search indexer for web pages published on [IPFS](https://ipfs.tech), written in Rust. It crawls IPFS content through an HTTP gateway, builds a keyword index of page titles and text, and serves it over a small HTTP API. It's the backend for [darksearch.xyz](https://www.darksearch.xyz).

## How it works
- **Crawling:** starts from a seed CID (the IPFS Wikipedia mirror) and follows links to other IPFS pages. Ten worker threads fetch pages through a gateway; the default is the public `ipfs.filebase.io`, which allows about 200 requests a minute, so the indexer limits itself to 100 a minute there (`GATEWAY_REQUESTS_PER_MIN`).
- **Only HTML is indexed.** Responses with any other content type, or bodies over 2 MB, are skipped without being downloaded. No images, video or other media are fetched or stored.
- **Indexing:** for each page it keeps the CID/path, title, a 128-character excerpt and keyword counts.
- **Denylists:** the [badbits](https://badbits.dwebops.pub/) list and an optional local takedown list are checked before anything is queued or fetched, and matching pages are removed from the index.
- **Persistence (optional):** with `DATA_DIR` set, pages and the crawl queue are kept in SQLite, and the keyword index in segment files beside it; all of it is reloaded at startup.

## Running
```
cargo build
RUST_LOG=info ./target/debug/ipfs_indexer                  # default public gateway
RUST_LOG=info ./target/debug/ipfs_indexer 127.0.0.1:8080   # your own gateway (host:port)
```

| Variable | Default | Purpose |
|---|---|---|
| `BIND_ADDR` | `127.0.0.1:9090` | Where the HTTP API listens. Set `0.0.0.0:9090` to accept remote connections. |
| `LOCAL_DENYLIST` | unset | Path to your takedown list (see [Denylists](#denylists)). |
| `ADMIN_TOKEN` | unset | Enables the admin endpoints (used by darksearch.xyz's admin area). Mount `LOCAL_DENYLIST`'s directory read-write. |
| `DATA_DIR` | unset | Directory for the persisted index (see [Persistence](#persistence)). Unset: in memory only. |
| `INDEX_CACHE_MB` | `1024` | With `DATA_DIR`, the most of `index.sqlite` (pages, queue) SQLite keeps in memory; the rest is read from disk. Lower it to cap memory, raise it if crawling gets slow. Segment files are cached by the OS instead. |
| `INDEX_MAX_AGE_DAYS` | `30` | With `DATA_DIR`, pages older than this are dropped. |
| `GATEWAY_REQUESTS_PER_MIN` | `100` on the default public gateway, no limit with your own gateway | Spaces out gateway requests across all workers (`0` = no limit). Filebase allows 200 a minute. |
| `RUST_LOG` | unset | Log level, e.g. `info`. |

The indexer **exits at startup** if it can't download the badbits list, read a configured `LOCAL_DENYLIST`, or open a configured `DATA_DIR`, rather than run without them.

## HTTP API
| Endpoint | Returns |
|---|---|
| `GET /search/{query}?limit=n&offset=m` | Indexed pages containing every word of the query, best first (BM25, summed over the query words each page has, so a long page that mentions a word in passing ranks below a focused one). `OR` (uppercase) between words matches any of them: `ipfs OR filecoin`. It binds tighter than the implicit AND, so `rust ipfs OR filecoin` is rust and (ipfs or filecoin). Case and punctuation are ignored, and common words like "the" (and lowercase "or") are skipped. `limit` and `offset` are optional and page through the results (all of them by default). The body starts `Results for {query} ({total} total): `. |
| `GET /enqueue/{cid}` or `/enqueue/{cid}/{path}` | Queues a page: `200` queued, `400` not a valid CID, `403` blocked by a denylist. |
| `GET /status` | Queue length, index size, mirrors skipped (pages with the same text as one already indexed), keyword count (approximate, like `/keywords`), gateway health and version on one line. |
| `GET /resources` | `Memory: <bytes> Index file: <bytes> Disk free: <bytes>`: process RSS, the size of the index (`index.sqlite` plus `segments/`) and free space on its filesystem (`-` without `DATA_DIR`). |
| `GET /version` | e.g. `0.1.0 (abc1234)`. The commit comes from `GIT_SHA` at build time. |
| `GET /keywords` | The most common keywords. Approximate: counts include deleted pages until their segments are merged away. |
| `POST /admin/deny`, `POST /admin/undeny` | Only with `ADMIN_TOKEN` set (otherwise 404); needs `Authorization: Bearer <token>`. The body is one `cid`, `cid/path` or `/ipfs/cid/path`: added to or removed from `LOCAL_DENYLIST` and applied at once. Returns `Blocked; <n> pages removed` or `Unblocked; <n> lines removed`. Badbits is never changed. |
| `POST /admin/recrawl` | Same token. Queues every indexed page to be fetched again, so their links to pages not indexed yet are followed. Pages stay searchable meanwhile; a restart drops what is left of it. Returns `Queued <n> pages to crawl again`. |

## Denylists
The badbits denylist is downloaded at startup and refreshed daily.

For your own takedowns, set `LOCAL_DENYLIST=/path/to/local.deny`:
- **Format:** one entry per line, `cid`, `cid/path`, `/ipfs/cid/path` or badbits-style `//<sha256 hex>`. A CID blocks everything under it, and other lines are ignored.
- **Effect:** the file is re-read every 5 minutes. Listed items are removed from the index and search results, and never fetched or queued again.

## Persistence
Set `DATA_DIR` to keep the index across restarts: pages, mirrors and the crawl queue in `$DATA_DIR/index.sqlite`, the keyword index in `$DATA_DIR/segments/`.

- **Stored per page:** the CID/path, title, 128-character excerpt, keyword counts and when it was indexed. Nothing else.
- **The keyword index** (which pages each word is on, and how often) lives on disk, not in memory (#231), in immutable segment files listed by `segments/MANIFEST` (#247). New pages are buffered in memory and written as a new segment every 60 s or 1M keyword entries; a background thread merges small segments into larger ones, up to 1 GiB; segments that size or larger aren't merged further, which bounds how long a merge or a takedown's rewrite takes, at the cost of a few more files per search on a very large index. An index from before this keeps its keyword index inside `index.sqlite`; it's moved into a segment on the first start, which can take a few minutes for a large index.
- **The crawl queue** (CID/paths waiting to be fetched, no content) is stored too, so a restart resumes the crawl. It lives only on disk, not in memory (#231). An item leaves it when a worker takes it, so a crash loses at most the few items being fetched.
- **Startup:** both denylists are loaded first. Stored pages that are now denylisted, or older than `INDEX_MAX_AGE_DAYS`, are deleted. Only then are the remaining pages loaded, and the saved queue (minus anything now denylisted or already indexed); then crawling and serving start.
- **Takedowns:**
  - Whenever a denylist refresh removes pages, they leave search results at once. Their rows are deleted from `index.sqlite`, where `secure_delete` overwrites them with zeros.
  - Within seconds, every segment file holding the page, or any earlier version of it (a re-crawl gets a new entry), is rewritten without it. The old files are overwritten with zeros, synced and removed (#249). Expired pages and expired old versions are erased the same way.
  - The rollback journal is kept in memory, so plaintext copies of pages are never written to a separate journal file.
  - Erasure at the filesystem or SSD level (old blocks, wear levelling, copy-on-write filesystems such as btrfs or ZFS keeping the old blocks) is outside what the indexer can guarantee, for both `index.sqlite` and the segment files. Use full-disk encryption on the data volume for that.
- **Expiry:** expired pages are dropped daily, and the seed is queued again so the crawl reaches them again.
- **No backups:** the index is deliberately not backed up. It can be rebuilt by crawling, and a backup is a copy takedowns can't reach.
- **Wipe:** stop the indexer, `rm -r $DATA_DIR/index.sqlite* $DATA_DIR/segments`, and start it again.
- **After a crash:** with the journal in memory, and `synchronous=OFF` (commits aren't fsynced, so spinning disks keep up), a crash mid-write or a power loss can corrupt `index.sqlite`. The indexer then refuses to start ("Couldn't use DATA_DIR"). Wipe as above and it rebuilds by crawling. Pages indexed since the last segment was written are queued again on the next start, not lost.

## Docker
- **Just the indexer,** against the default public gateway. Run from the repository root (the build context needs `Cargo.toml` and `src`):
  ```
  docker build -f docker/Dockerfile -t ipfs-indexer . && docker run -p 9090:9090 ipfs-indexer
  ```
  The image listens on `0.0.0.0:9090`. Pass `--build-arg GIT_SHA=$(git rev-parse HEAD)` to show the commit in `/version`.
- **Indexer plus a local IPFS node:** from the `docker` directory, run `docker compose up`.
  - The node (Kubo) is locked down so it never re-serves what the crawler fetched: no providing, DHT client only, Bitswap server off, GC at 2 GB, and no inbound swarm port.
  - It also has the badbits denylist, re-downloaded on every `up`.

Production (ubuntu-cube, with the index on its HDD array) is deployed by the `ipfs_indexer` role in [compscidr/iac](https://github.com/compscidr/iac).

To release: bump `version` in `Cargo.toml`, merge, then push a matching tag (`git tag v0.2.0 && git push origin v0.2.0`). That publishes `ghcr.io/compscidr/ipfs_indexer:v0.2.0`; set `ipfs_indexer_version` in the iac role to the tag and deploy.

## Development
CI runs `cargo fmt --all -- --check && cargo test`, then a release build. Run the same before opening a PR. Ideas and planned work live in the [issues](https://github.com/compscidr/ipfs_indexer/issues).
