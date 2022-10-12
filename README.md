# ipfs_indexer
[![.github/workflows/build-and-test.yml](https://github.com/compscidr/ipfs_indexer/actions/workflows/build-and-test.yml/badge.svg)](https://github.com/compscidr/ipfs_indexer/actions/workflows/build-and-test.yml)

An ipfs indexer / search engine built in rust.

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

## Build notes
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
- Run `RUST_LOG=info ./target/debug/ipfs_indexer 127.0.0.1:8080` to use your own ipfs gateway instead of ipfs.io

By default runs an endpoing on `0.0.0.0:9090` so you can go to 
- http://localhost:9090/status
- http://localhost:9090/enqueue/somecid
- http://localhost:9090/search/somequery

## Running with docker
From the docker directory, run `docker-compose up`. Currently image is only ~26MB.

## CI
I setup a workflow that should run a build at least on push, but doesnt run any tests because I have no idea how test
suites work yet for rust.
