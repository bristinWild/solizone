#!/usr/bin/env bash
# Smoke test: deploy and use Counter on a running Solizone node with Foundry.
#
#   cargo run --bin solizone_node -- --storage local      # in another terminal
#   ./scripts/foundry-smoke.sh [rpc-url]
#
# Extra forge flags can be passed via FORGE_FLAGS (e.g. "--offline").
#
# Uses dev account (0), whose private key is PUBLIC (Foundry/Anvil test key).
set -euo pipefail

RPC="${1:-http://127.0.0.1:8545}"
KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
cd "$(dirname "$0")/.."

echo "chain id:     $(cast chain-id --rpc-url "$RPC")"
echo "head block:   $(cast block-number --rpc-url "$RPC")"

COUNTER=$(forge create contracts/Counter.sol:Counter --broadcast ${FORGE_FLAGS:-} \
  --rpc-url "$RPC" --private-key "$KEY" --json | jq -r .deployedTo)
echo "Counter:      $COUNTER"

cast send "$COUNTER" "increment()" --rpc-url "$RPC" --private-key "$KEY" > /dev/null
cast send "$COUNTER" "increment()" --rpc-url "$RPC" --private-key "$KEY" --legacy > /dev/null

COUNT=$(cast call "$COUNTER" "count()(uint256)" --rpc-url "$RPC")
echo "count():      $COUNT"
[ "$COUNT" = "2" ] || { echo "expected count 2"; exit 1; }

REVERT=$(cast call "$COUNTER" "fail()" --rpc-url "$RPC" 2>&1 || true)
if grep -q "intentional failure" <<< "$REVERT"; then
  echo "revert:       reason decoded"
else
  echo "expected revert reason"; exit 1
fi

LOGS=$(cast logs --from-block 0 --address "$COUNTER" "Incremented(uint256)" --rpc-url "$RPC" --json | jq length)
echo "logs:         $LOGS"
[ "$LOGS" = "2" ] || { echo "expected 2 logs"; exit 1; }

echo "OK: Solizone works with Foundry"