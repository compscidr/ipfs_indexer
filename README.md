# ipfs_indexer
[![.github/workflows/build-and-test.yml](https://github.com/compscidr/ipfs_indexer/actions/workflows/build-and-test.yml/badge.svg)](https://github.com/compscidr/ipfs_indexer/actions/workflows/build-and-test.yml)

A search indexer for web pages published on [IPFS](https://ipfs.tech), written in Rust. It crawls IPFS content through an HTTP gateway, builds a keyword index of page titles and text, and serves it over a small HTTP API. It's the backend for [darksearch.xyz](https://www.darksearch.xyz).

## How it works
- **Crawling:** starts from a seed CID (the IPFS Wikipedia mirror) and follows links to other IPFS pages. Ten worker threads fetch pages through a gateway; the default is the public `ipfs.filebase.io`, which allows about 200 requests a minute, so the indexer limits itself to 100 a minute there (`GATEWAY_REQUESTS_PER_MIN`).
- **Only HTML is indexed.** Responses with any other content type, or bodies over 2 MB, are skipped without being downloaded. No images, video or other media are fetched or stored.
- **Indexing:** for each page it keeps the CID/path, title, a 128-character excerpt and keyword counts.
- **Denylists:** the [badbits](https://badbits.dwebops.pub/) list and an optional local takedown list are checked before anything is queued or fetched, and matching pages are removed from the index.
- **Persistence (optional):** with `DATA_DIR` set, pages and the crawl queue are kept in SQLite and reloaded at startup.

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
| `INDEX_CACHE_MB` | `1024` | With `DATA_DIR`, the most of the index (pages and keyword index) SQLite keeps in memory; the rest is read from disk. Lower it to cap memory, raise it if searches or crawling get slow. |
| `INDEX_MAX_AGE_DAYS` | `30` | With `DATA_DIR`, pages older than this are dropped. |
| `GATEWAY_REQUESTS_PER_MIN` | `100` on the default public gateway, no limit with your own gateway | Spaces out gateway requests across all workers (`0` = no limit). Filebase allows 200 a minute. |
| `RUST_LOG` | unset | Log level, e.g. `info`. |

The indexer **exits at startup** if it can't download the badbits list, read a configured `LOCAL_DENYLIST`, or open a configured `DATA_DIR`, rather than run without them.

## HTTP API
| Endpoint | Returns |
|---|---|
| `GET /search/{query}?limit=n&offset=m` | Indexed pages containing every word of the query, best first (TF-IDF). Case and punctuation are ignored, and common words like "the" are skipped. `limit` and `offset` are optional and page through the results (all of them by default). The body starts `Results for {query} ({total} total): `. |
| `GET /enqueue/{cid}` or `/enqueue/{cid}/{path}` | Queues a page: `200` queued, `400` not a valid CID, `403` blocked by a denylist. |
| `GET /status` | Queue length, index size, keyword count, gateway health and version on one line. |
| `GET /resources` | `Memory: <bytes> Index file: <bytes> Disk free: <bytes>`: process RSS, the size of `index.sqlite` and free space on its filesystem (`-` without `DATA_DIR`). |
| `GET /version` | e.g. `0.1.0 (abc1234)`. The commit comes from `GIT_SHA` at build time. |
| `GET /keywords` | The most common keywords. |
| `POST /admin/deny`, `POST /admin/undeny` | Only with `ADMIN_TOKEN` set (otherwise 404); needs `Authorization: Bearer <token>`. The body is one `cid`, `cid/path` or `/ipfs/cid/path`: added to or removed from `LOCAL_DENYLIST` and applied at once. Returns `Blocked; <n> pages removed` or `Unblocked; <n> lines removed`. Badbits is never changed. |
| `POST /admin/recrawl` | Same token. Queues every indexed page to be fetched again, so their links to pages not indexed yet are followed. Pages stay searchable meanwhile; a restart drops what is left of it. Returns `Queued <n> pages to crawl again`. |

## Denylists
The badbits denylist is downloaded at startup and refreshed daily.

For your own takedowns, set `LOCAL_DENYLIST=/path/to/local.deny`:
- **Format:** one entry per line, `cid`, `cid/path`, `/ipfs/cid/path` or badbits-style `//<sha256 hex>`. A CID blocks everything under it, and other lines are ignored.
- **Effect:** the file is re-read every 5 minutes. Listed items are removed from the index and search results, and never fetched or queued again.

## Persistence
Set `DATA_DIR` to keep the index across restarts in `$DATA_DIR/index.sqlite`.

- **Stored per page:** the CID/path, title, 128-character excerpt, keyword counts and when it was indexed. Nothing else.
- **The keyword index** (which pages each word is on, and how often) lives in the file, not in memory (#231); SQLite caches up to `INDEX_CACHE_MB` of it. An index file from before this is converted on the first start, which can take a few minutes for a large index.
- **The crawl queue** (CID/paths waiting to be fetched, no content) is stored too, so a restart resumes the crawl. An item leaves it once it has been fetched.
- **Startup:** both denylists are loaded first. Stored pages that are now denylisted, or older than `INDEX_MAX_AGE_DAYS`, are deleted. Only then are the remaining pages loaded, and the saved queue (minus anything now denylisted or already indexed); then crawling and serving start.
- **Takedowns:**
  - Whenever a denylist refresh removes pages, their rows and keyword entries are deleted from the file, and `secure_delete` overwrites them with zeros inside `index.sqlite`.
  - The rollback journal is kept in memory, so plaintext copies of pages are never written to a separate journal file.
  - Erasure at the filesystem or SSD level (old blocks, wear levelling) is outside what the indexer can guarantee. Use full-disk encryption on the data volume for that.
- **Expiry:** expired pages are dropped daily, and the seed is queued again so the crawl reaches them again.
- **No backups:** the index is deliberately not backed up. It can be rebuilt by crawling, and a backup is a copy takedowns can't reach.
- **Wipe:** stop the indexer, `rm $DATA_DIR/index.sqlite*`, and start it again.
- **After a crash:** with the journal in memory, a crash mid-write can corrupt `index.sqlite`. The indexer then refuses to start ("Couldn't use DATA_DIR"). Wipe the file as above and it rebuilds by crawling.

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

## Development
CI runs `cargo fmt --all -- --check && cargo test`, then a release build. Run the same before opening a PR. Ideas and planned work live in the [issues](https://github.com/compscidr/ipfs_indexer/issues).
