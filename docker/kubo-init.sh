#!/bin/sh
# Runs before every daemon start. The node exists only to fetch for the indexer:
# it must never announce or hand out the blocks it cached while crawling.
set -e
ipfs config --json Provide.Enabled false       # don't announce fetched content
ipfs config Routing.Type autoclient            # never a DHT server
ipfs config --json Bitswap.ServerEnabled false # reply "don't have" to block requests
ipfs config Datastore.StorageMax 2GB           # with --enable-gc, cached blocks get dropped
