# ipfs_indexer
[![.github/workflows/build-and-test.yml](https://github.com/compscidr/ipfs_indexer/actions/workflows/build-and-test.yml/badge.svg)](https://github.com/compscidr/ipfs_indexer/actions/workflows/build-and-test.yml)

An ipfs indexer / search engine built in rust.

## Build notes
On Ubuntu you may need to apt install: `libssl-dev`.

As of libp2p 0.44.0, it seems to require rust nightly: https://stackoverflow.com/questions/69848319/unable-to-specify-edition2021-in-order-to-use-unstable-packages-in-rust

Did init as a "binary" - not sure if this makes sense, or if other people think we should split this into a library
bit and an application bit. I suppose we can always change it later as it grows.

Following this guide for libp2p:
https://github.com/libp2p/rust-libp2p/blob/master/src/tutorial.rs

Trying to follow best practices from here:
https://doc.rust-lang.org/cargo/commands/cargo-init.html

Adding Cargo.lock to version control - it *seems* like it might be best practice for a binary (app):
https://stackoverflow.com/questions/62861623/should-cargo-lock-be-committed-when-the-crate-is-both-a-rust-library-and-an-exec

## Building on mac

Requirements: `xcode`

Run `xcode-select --install` if you do not have xcode installed, need to update xcode, or run into xcode related build errors

## Running with logging output
- Run `cargo build` to build
- Run `RUST_LOG=info ./target/debug/ipfs_indexer` to see logging output (adjust level accordingly)
- Run `RUST_LOG=info ./target/debug/ipfs_indexer 127.0.0.1:8080` to use your own ipfs gateway instead of the default public gateway (`ipfs.filebase.io`, rate limited to 200 requests/min)

By default runs an endpoint on `127.0.0.1:9090` (set `BIND_ADDR=0.0.0.0:9090` to accept remote connections) so you can go to 
- http://localhost:9090/status (queue, index size, gateway health, version)
- http://localhost:9090/version
- http://localhost:9090/enqueue/somecid
- http://localhost:9090/search/somequery

## Denylists
The [badbits](https://badbits.dwebops.pub/) denylist is downloaded at startup (the indexer exits if it can't be) and refreshed daily.

For your own takedowns, set `LOCAL_DENYLIST=/path/to/local.deny`. One entry per line: `cid`, `cid/path` or `/ipfs/cid/path` (a CID blocks everything under it), or badbits-style `//<sha256 hex>`; other lines are ignored. The file is re-read every 5 minutes; listed items are removed from the index and search results, and never fetched or enqueued again.

## Persistence
Set `DATA_DIR` to keep the index across restarts in `$DATA_DIR/index.sqlite` (SQLite). Without it the index lives in memory only.

- **Stored per page:** the CID/path, title, 128-character excerpt and keyword counts. Nothing else.
- **Startup:** both denylists (badbits and `LOCAL_DENYLIST`) are loaded first. Stored pages that are now denylisted, or older than `INDEX_MAX_AGE_DAYS` (default 30), are deleted. Only then are the remaining pages loaded, and crawling and serving start.
- **Takedowns:** whenever a denylist refresh removes pages, their rows are deleted from the file, and `secure_delete` overwrites them with zeros inside `index.sqlite`. The rollback journal is kept in memory, so plaintext copies of pages are never written to a separate journal file. Erasure at the filesystem or SSD level (old blocks, wear levelling) is outside what the indexer can guarantee; use full-disk encryption on the data volume for that.
- **Expiry:** pages older than the max age are dropped daily. The crawler re-adds them if it reaches them again.
- **No backups:** the index is deliberately not backed up. It can be rebuilt by crawling, and a backup is a copy takedowns can't reach.
- **Wipe:** stop the indexer, `rm $DATA_DIR/index.sqlite*`, and start it again.
- **After a crash:** with the journal in memory, a crash mid-write can corrupt `index.sqlite`. The indexer then refuses to start ("Couldn't use DATA_DIR"); wipe the file as above and it rebuilds by crawling.
- If `DATA_DIR` is set but can't be opened, the indexer exits rather than silently running without persistence.

## Running with docker
- Indexer + a local IPFS node: from the `docker` directory, run `docker compose up`. The node (Kubo) is locked down so it never re-serves what the crawler fetched: no providing, DHT client only, Bitswap server off, GC at 2GB, no inbound swarm port, and the badbits denylist (re-downloaded on every `up`).
- Just the indexer, against the default public gateway: from the repository root (the build context needs `Cargo.toml` and `src`), run `docker build -f docker/Dockerfile -t ipfs-indexer . && docker run -p 9090:9090 ipfs-indexer`.

## CI
I setup a workflow that should run a build at least on push, but doesnt run any tests because I have no idea how test
suites work yet for rust.


## What Needs to be Done
- Discover content to be indexed, add them to the index queue
  - [ ] Listen in on the gossip protocol **Jason working on**
  - [X] Start from some collection of pages on ipfs.io/ipfs
- Implement an index queue processor
  - [X] Fetch the ipfs content
  - [X] Process the page for more ipfs links, Add those links into the index queue
  - Index the pages somehow
    - Ranked keywords by frequency or something?
    - Need to update to support more than just html content (look at header and index files)
    - Update the except to be flexible - for images, it could be a small crop render of the original image, for videos it could be a gif preview render
  - Store the index somehow (start with in-memory, then figure out how to do storage later) - **Conor working on**
    - A hashmap of map[keyword] -> sorted tree where the entries are sorted by keyword frequency and entries contain ipfs hash? - **Conor working on**
    - Will probably want to think of ejection mechanism sooner than later so we can eject to storage (least recently used? oldest? who knows?)
    - Farther out - need to think about how the store will be designed
   - [X] Probably also want to store an excerpt, page title of the page to present to front-end
- [X] Implement a backend API which a future front-end can use, and in short term we can use to debug
  - search -> search result
    - ordered list of <title, link, excerpts>, possibly grouped by text, images, videos, other
  - stats:
    - indexed entries
    - outstanding index queue
    - memory used / free
    - stoage used / free
- Implement a front-end which queries the index storage and displays the page title, ipfs/io/ipfs link to the page and excerpt
  from the browser
- Feedback loop from what people click on more often to rank those higher
- [X] Might a docker container:
  - [X] deploy will auto restart itself on crash (will also make it easy to see consumed memory with docker stats and other tools)
  - [X] will be able to deploy with a local ipfs instance all ready to go within the container
  - can artificially restrict memory so we can test things like ejection mechanisms
- Farther out -> hook into papertrail or some logging service so we can see what's up if it dies
- Tests! - **Conor working on**
