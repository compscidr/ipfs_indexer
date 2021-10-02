# ipfs_indexer
An ipfs indexer / search engine

## Build notes
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

## CI
I setup a workflow that should run a build at least on push, but doesnt run any tests because I have no idea how test
suites work yet for rust.