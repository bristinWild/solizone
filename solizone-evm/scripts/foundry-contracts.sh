#!/usr/bin/env bash
# Broader Foundry compatibility test: deploys and exercises several different
# kinds of contracts from contracts/Zoo.sol on a running Solizone node.
#
#   cargo run --bin solizone_node -- --storage local      # in another terminal
#   ./scripts/foundry-contracts.sh [rpc-url]
#
# Covers: ERC20 with constructor args, custom errors, events/log filters,
# CREATE and CREATE2 factories, payable/receive and ETH transfers, external
# library linking, precompiles, block/tx environment, Panic reverts, and
# call/delegatecall/staticcall between contracts.
#
# Extra forge flags can be passed via FORGE_FLAGS (e.g. "--use /path/to/solc").
#
# Uses dev accounts (0) and (1), whose private keys are PUBLIC (Foundry/Anvil
# test keys).
set -euo pipefail

export FOUNDRY_DISABLE_NIGHTLY_WARNING=1

RPC="${1:-http://127.0.0.1:8545}"
KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
A0=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
A1=0x70997970C51812dc3A010C7d01b50e0d17dc79C8
cd "$(dirname "$0")/.."

FAILED=0

# expect <label> <actual> <expected>
expect() {
  if [ "$2" = "$3" ]; then
    printf '  ok    %s\n' "$1"
  else
    printf '  FAIL  %s\n        expected: %s\n        actual:   %s\n' "$1" "$3" "$2"
    FAILED=1
  fi
}

# expect_contains <label> <haystack> <needle>
expect_contains() {
  if grep -qF -- "$3" <<< "$2"; then
    printf '  ok    %s\n' "$1"
  else
    printf '  FAIL  %s\n        expected to contain: %s\n        actual: %s\n' "$1" "$3" "$2"
    FAILED=1
  fi
}

# deploy <Contract> [forge flags...]   (constructor args must come last)
deploy() {
  local name="$1"
  shift
  forge create "contracts/Zoo.sol:$name" --broadcast ${FORGE_FLAGS:-} \
    --rpc-url "$RPC" --private-key "$KEY" --json "$@" | jq -r .deployedTo
}

send() { cast send "$@" --rpc-url "$RPC" --private-key "$KEY" > /dev/null; }
call() { cast call "$@" --rpc-url "$RPC" 2>&1 || true; }

echo "chain id: $(cast chain-id --rpc-url "$RPC")"

echo "ERC20 (constructor args, events, custom errors)"
TOKEN=$(deploy Token --constructor-args "Zone Token" ZNT 1000000000000000000000)
expect "name()" "$(call "$TOKEN" 'name()(string)')" '"Zone Token"'
expect "totalSupply()" "$(call "$TOKEN" 'totalSupply()(uint256)' | cut -d' ' -f1)" 1000000000000000000000
send "$TOKEN" 'transfer(address,uint256)' "$A1" 5000000000000000000
expect "balanceOf(A1)" "$(call "$TOKEN" 'balanceOf(address)(uint256)' "$A1" | cut -d' ' -f1)" 5000000000000000000
send "$TOKEN" 'approve(address,uint256)' "$A1" 7
expect "allowance" "$(call "$TOKEN" 'allowance(address,address)(uint256)' "$A0" "$A1")" 7
INSUFFICIENT=$(call "$TOKEN" 'transfer(address,uint256)' "$A0" 999999999999999999999 --from "$A1")
expect_contains "custom error revert (Insufficient)" "$INSUFFICIENT" "execution reverted"
expect_contains "custom error selector" "$INSUFFICIENT" "0xe8620800"
expect "Transfer logs" "$(cast logs --from-block 0 --address "$TOKEN" 'Transfer(address,address,uint256)' --rpc-url "$RPC" --json | jq length)" 2
expect "Approval logs" "$(cast logs --from-block 0 --address "$TOKEN" 'Approval(address,address,uint256)' --rpc-url "$RPC" --json | jq length)" 1
expect "storage slot 2 (totalSupply)" "$(cast storage "$TOKEN" 2 --rpc-url "$RPC" | cast to-dec)" 1000000000000000000000

echo "Factory (CREATE, CREATE2)"
FACTORY=$(deploy Factory)
send "$FACTORY" 'make(uint256)' 11 --value 3wei
SALT=0x0000000000000000000000000000000000000000000000000000000000000005
PREDICTED=$(call "$FACTORY" 'predict(bytes32,uint256)(address)' "$SALT" 22 --from "$A0")
send "$FACTORY" 'make2(bytes32,uint256)' "$SALT" 22
expect "CREATE2 code exists at predicted address" "$([ "$(cast code "$PREDICTED" --rpc-url "$RPC")" != "0x" ] && echo yes || echo no)" yes
expect "child owner" "$(call "$PREDICTED" 'owner()(address)')" "$A0"
expect "child salt" "$(call "$PREDICTED" 'salt()(uint256)')" 22
expect "Made logs" "$(cast logs --from-block 0 --address "$FACTORY" 'Made(address,bool)' --rpc-url "$RPC" --json | jq length)" 2

echo "Vault (receive, ETH transfer, library, precompiles, environment)"
LIB=$(deploy MathLib)
VAULT=$(deploy Vault --libraries "contracts/Zoo.sol:MathLib:$LIB")
send "$VAULT" --value 1000wei
expect "deposits after receive()" "$(call "$VAULT" 'deposits(address)(uint256)' "$A0")" 1000
send "$VAULT" 'withdraw(uint256)' 400
expect "vault ETH balance after withdraw" "$(cast balance "$VAULT" --rpc-url "$RPC")" 600
expect "internal library sq(9)" "$(call "$VAULT" 'sq(uint256)(uint256)' 9)" 81
expect "linked external library cube(3)" "$(call "$VAULT" 'cube(uint256)(uint256)' 3)" 27
HASHES=$(call "$VAULT" 'hashIt(bytes)(bytes32,bytes32,address)' 0x1234)
expect_contains "keccak256" "$HASHES" 0x56570de287d73cd1cb6092bb8fdee6173974955fdef345ae579ee9f475ea7432
expect "sha256 precompile present" "$(grep -c '^0x' <<< "$HASHES")" 3
ENVIRONMENT=$(call "$VAULT" 'env()(uint256,uint256,uint256,uint256,address,uint256)' --from "$A1")
expect "block.chainid" "$(sed -n 1p <<< "$ENVIRONMENT")" 9001
expect "block.basefee" "$(sed -n 4p <<< "$ENVIRONMENT")" 0
expect "msg.sender" "$(sed -n 5p <<< "$ENVIRONMENT")" "$A1"
expect "tx.gasprice" "$(sed -n 6p <<< "$ENVIRONMENT")" 0
expect "loop(1000)" "$(call "$VAULT" 'loop(uint256)(uint256)' 1000 | cut -d' ' -f1)" 499500
expect_contains "assert(false) is Panic(1)" "$(call "$VAULT" 'boom()')" 4e487b71
expect_contains "divide by zero is Panic(0x12)" "$(call "$VAULT" 'divz(uint256,uint256)' 1 0)" 4e487b71
expect_contains "withdraw over balance reverts" "$(call "$VAULT" 'withdraw(uint256)' 99999)" "execution reverted"

echo "Cross-contract calls"
CALLER=$(deploy Caller)
expect "call" "$(call "$CALLER" 'ping(address,uint256)(bool,bytes)' "$VAULT" 6 | tr '\n' ' ')" "true 0x0000000000000000000000000000000000000000000000000000000000000024 "
expect "delegatecall" "$(call "$CALLER" 'viaDelegate(address,uint256)(bool,bytes)' "$VAULT" 6 | tr '\n' ' ')" "true 0x0000000000000000000000000000000000000000000000000000000000000024 "
expect "staticcall" "$(call "$CALLER" 'staticTo(address)(bool)' "$VAULT")" true

echo "RPC introspection"
expect "eth_getCode (deployed)" "$([ "$(cast code "$VAULT" --rpc-url "$RPC")" != "0x" ] && echo yes || echo no)" yes
expect "eth_estimateGas > 21000" "$([ "$(cast estimate "$TOKEN" 'transfer(address,uint256)' "$A1" 1 --from "$A0" --rpc-url "$RPC")" -gt 21000 ] && echo yes || echo no)" yes
TX=$(cast send "$TOKEN" 'transfer(address,uint256)' "$A1" 1 --rpc-url "$RPC" --private-key "$KEY" --json | jq -r .transactionHash)
expect "receipt status" "$(cast receipt "$TX" --rpc-url "$RPC" --json | jq -r .status)" 0x1

echo
if [ "$FAILED" = 0 ]; then
  echo "OK: Solizone runs a broad range of contracts"
else
  echo "FAILED: see messages above"
  exit 1
fi