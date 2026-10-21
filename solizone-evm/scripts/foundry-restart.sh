#!/usr/bin/env bash
# Restart persistence test: starts its own local node, sends transactions with
# Foundry, stops the node, starts it again on the same data and checks that
# receipts, transactions, logs and blocks of the earlier run still resolve.
#
#   ./scripts/foundry-restart.sh
#
# Does not touch a node you already run: it uses its own data directory and
# port. Build the node first (`cargo build --bin solizone_node`) or let this
# script build it. Extra forge flags can be passed via FORGE_FLAGS.
#
# Uses dev account (0), whose private key is PUBLIC (Foundry/Anvil test key).
set -euo pipefail

export FOUNDRY_DISABLE_NIGHTLY_WARNING=1

PORT="${PORT:-18545}"
RPC="http://127.0.0.1:$PORT"
KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
A1=0x70997970C51812dc3A010C7d01b50e0d17dc79C8
cd "$(dirname "$0")/.."

DATA="$(mktemp -d)"
LOG1="$DATA/node-1.log"
LOG2="$DATA/node-2.log"
NODE_PID=""
FAILED=0

cleanup() {
  if [ -n "$NODE_PID" ]; then
    kill "$NODE_PID" 2> /dev/null || true
    wait "$NODE_PID" 2> /dev/null || true
  fi
  rm -rf "$DATA"
}
trap cleanup EXIT

expect() {
  if [ "$2" = "$3" ]; then
    printf '  ok    %s\n' "$1"
  else
    printf '  FAIL  %s\n        expected: %s\n        actual:   %s\n' "$1" "$3" "$2"
    FAILED=1
  fi
}

start_node() {
  ./target/debug/solizone_node --storage local --data-dir "$DATA" \
    --rpc "127.0.0.1:$PORT" --block-time 0.3 > "$1" 2>&1 &
  NODE_PID=$!

  for _ in $(seq 1 50); do
    cast chain-id --rpc-url "$RPC" > /dev/null 2>&1 && return 0
    sleep 0.2
  done

  echo "node did not start; log:"
  cat "$1"
  exit 1
}

stop_node() {
  kill "$NODE_PID"
  wait "$NODE_PID" 2> /dev/null || true
  NODE_PID=""
}

cargo build --quiet --bin solizone_node

echo "First run"
start_node "$LOG1"

COUNTER=$(forge create contracts/Counter.sol:Counter --broadcast ${FORGE_FLAGS:-} \
  --rpc-url "$RPC" --private-key "$KEY" --json | jq -r .deployedTo)

INC=$(cast send "$COUNTER" "increment()" --rpc-url "$RPC" --private-key "$KEY" --json)
INC_HASH=$(jq -r .transactionHash <<< "$INC")
INC_BLOCK=$(jq -r .blockNumber <<< "$INC")

PAY=$(cast send "$A1" --value 5wei --rpc-url "$RPC" --private-key "$KEY" --json)
PAY_HASH=$(jq -r .transactionHash <<< "$PAY")

HEAD_BEFORE=$(cast block-number --rpc-url "$RPC")
BLOCK_HASH_BEFORE=$(cast block "$INC_BLOCK" --rpc-url "$RPC" --json | jq -r .hash)
RECEIPT_BEFORE=$(cast receipt "$INC_HASH" --rpc-url "$RPC" --json | jq -S -c '{status,gasUsed,blockNumber,blockHash,transactionIndex,logs}')

echo "  counter $COUNTER, increment tx $INC_HASH in block $INC_BLOCK"
stop_node

echo "Second run (same data)"
start_node "$LOG2"

expect "head block preserved" "$(cast block-number --rpc-url "$RPC")" "$HEAD_BEFORE"
expect "contract code preserved" "$([ "$(cast code "$COUNTER" --rpc-url "$RPC")" != "0x" ] && echo yes || echo no)" yes

RECEIPT_AFTER=$(cast receipt "$INC_HASH" --rpc-url "$RPC" --json | jq -S -c '{status,gasUsed,blockNumber,blockHash,transactionIndex,logs}')
expect "receipt identical after restart" "$RECEIPT_AFTER" "$RECEIPT_BEFORE"
expect "transaction by hash" "$(cast tx "$INC_HASH" --rpc-url "$RPC" --json | jq -r .hash)" "$INC_HASH"
expect "transfer receipt status" "$(cast receipt "$PAY_HASH" --rpc-url "$RPC" --json | jq -r .status)" 0x1
expect "block hash stable" "$(cast block "$INC_BLOCK" --rpc-url "$RPC" --json | jq -r .hash)" "$BLOCK_HASH_BEFORE"
expect "block lists its transaction" "$(cast block "$INC_BLOCK" --rpc-url "$RPC" --json | jq -r '.transactions[0]')" "$INC_HASH"
expect "logs found after restart" "$(cast logs --from-block 0 --address "$COUNTER" 'Incremented(uint256)' --rpc-url "$RPC" --json | jq length)" 1

cast send "$COUNTER" "increment()" --rpc-url "$RPC" --private-key "$KEY" > /dev/null
expect "chain keeps working after restart" "$(cast call "$COUNTER" 'count()(uint256)' --rpc-url "$RPC")" 2
expect "logs include old and new" "$(cast logs --from-block 0 --address "$COUNTER" 'Incremented(uint256)' --rpc-url "$RPC" --json | jq length)" 2

echo
if [ "$FAILED" = 0 ]; then
  echo "OK: receipts and transactions survive restarts"
else
  echo "FAILED: see messages above"
  echo "--- second run log ---"
  cat "$LOG2"
  exit 1
fi