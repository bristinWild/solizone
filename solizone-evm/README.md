# Solizone EVM

**A stateful, restartable EVM execution environment for Logos.**

Solizone executes Solidity / EVM transactions with REVM, maintains recoverable state, produces parent-linked canonical blocks, publishes those blocks through the Logos Zone SDK, and now uses Logos Storage for execution checkpoints and historical block persistence.

> **Status:** active research implementation on `feat/second-phase`  
> **Note:** Solizone is an independent project and is not an official Logos project.

---

## Why Solizone?

```text
Ethereum developer experience
        ↓
Solizone EVM execution
        ↓
canonical Solizone blocks
        ↓
Logos
```

Solizone owns EVM execution and state.

The Logos stack provides shared infrastructure around it:

```text
Logos blockchain
→ publication
→ ordering
→ consensus
→ finality

Logos Storage
→ execution checkpoint persistence
→ historical canonical block storage
→ content-addressed retrieval
```

The long-term goal is to give Ethereum developers a familiar Solidity / EVM environment while remaining native to the Logos architecture.

---

## What works today

### EVM execution

- generic REVM transaction execution
- Solidity contract deployment and calls
- mutable contract storage and read-only calls
- persistent balances, nonces, bytecode, and storage
- ordered transaction-batch execution
- receipts and deterministic transaction / receipt / state commitments

### State and recovery

- deterministic `StateSnapshot`
- snapshot → fresh REVM reconstruction
- JSON and file-backed persistence
- `SolizoneCheckpoint`
- checkpoint contains EVM state + `next_height` + `parent_hash`
- `FileCheckpointBackend`
- `LogosStorageCheckpointBackend`
- full process restart and continued execution
- protocol state-root continuity after restore
- remote checkpoint recovery through Logos Storage

### Canonical chain

- canonical `SZB1` block format
- parent-linked block production
- `BlockProducer`
- producer resume after restart
- `BlockStore`
- `MemoryBlockStore`
- `FileBlockStore`
- `HybridBlockStore`
- lookup by height or hash
- recent canonical history retained locally
- historical canonical blocks archived into Logos Storage
- automatic pruning with a configurable retention window
- recovered historical blocks decoded, validated, and hash-checked

### Long-running node runtime

- continuous block production
- checkpoint persistence after completed blocks
- automatic local retention enforcement
- automatic archival of older blocks to Logos Storage
- automatic startup recovery from the latest checkpoint
- recovered chain-head verification
- continued block production from the recovered height

### Logos publication

- generic publisher boundary
- Logos publisher adapter
- `ZoneSequencer` publication
- canonical Solizone bytes published to Logos
- Logos block inclusion observed
- LIB / finality observed
- sequencer checkpoint and resume flow

---

## Current architecture

```text
Solidity / Solizone tx
        ↓
       REVM
        ↓
   MemoryState
        ↓
receipts + state_root
        ↓
  BlockProducer
        ↓
canonical Solizone block
        │
        ├──────────────── persistence ────────────────┐
        │                                             │
        ↓                                             ↓
SolizoneCheckpoint                             HybridBlockStore
        │                                      │             │
        ↓                                      ↓             ↓
CheckpointBackend                        recent blocks    older blocks
        │                                  local          Logos Storage
        ├── FileCheckpointBackend                             │
        └── LogosStorageCheckpointBackend                    ↓
                                                    height → hash → CID

canonical Solizone block
        ↓
BlockPublisher
        ↓
LogosPublisher
        ↓
ZoneSequencer
        ↓
Logos blockchain
        ↓
ordering / inclusion / finality
```

The execution, persistence, storage, and publication boundaries are intentionally separate.

---

## Hybrid Storage Layer

Solizone now includes a hybrid storage model for canonical block history.

```text
BlockStore
├── MemoryBlockStore
├── FileBlockStore
└── HybridBlockStore
      ├── recent blocks → local FileBlockStore
      └── older blocks  → Logos Storage
                          via height → hash → CID
```

A node can keep only a recent local block window while moving older canonical history into Logos Storage.

Example with a retention window of `3`:

```text
canonical history:
0 1 2 3 4 5 6

local:
4 5 6

Logos Storage:
0 1 2 3
```

When the local window is exceeded:

```text
insert new block
      ↓
persist locally
      ↓
retention exceeded?
      ↓
archive oldest block
      ↓
upload to Logos Storage
      ↓
persist height + hash + CID
      ↓
prune local .szb
```

The archive index is persisted before the local canonical block is removed.

A caller does not need to know where the block physically lives:

```rust
store.get_by_height(8)?;
store.get_by_height(2)?;
```

`HybridBlockStore` checks local storage first and falls back to Logos Storage when the block is archived.

Recovered blocks are:

```text
downloaded
    ↓
decoded
    ↓
validated
    ↓
hash-checked against canonical metadata
    ↓
returned
```

---

## Logos Storage integration

The Storage integration is now part of Solizone persistence rather than a standalone runtime experiment.

### Checkpoints

```text
SolizoneCheckpoint
        ↓
encode
        ↓
LogosStorageCheckpointBackend
        ↓
Logos Storage
        ↓
CID
```

The latest checkpoint CID is recorded locally as discovery metadata.

On restart:

```text
checkpoint CID
      ↓
Logos Storage
      ↓
download checkpoint
      ↓
restore EVM state
      ↓
restore next_height
      ↓
restore parent_hash
```

### Historical blocks

Canonical `.szb` blocks can be archived individually.

```text
canonical .szb
      ↓
Logos Storage
      ↓
CID
      ↓
archive index
height → block hash → CID
```

The Storage layer stores and retrieves content. It does **not** decide which Solizone block is canonical.

---

## Long-running node runtime

Run:

```bash
# Logos Storage backed (needs Basecamp + logosctl)
cargo run --bin solizone_node

# Local disk only: no Basecamp needed (local development, Zoner)
cargo run --bin solizone_node -- --storage local
```

Flags:

| Flag | Default | Meaning |
|---|---|---|
| `--storage logos\|local` | `logos` | where blocks and checkpoints are persisted |
| `--rpc ADDR` | `127.0.0.1:8545` | JSON-RPC listen address |
| `--block-time SECONDS` | `2` | how often pending transactions are sealed into a block |
| `--retention BLOCKS` | `64` | recent blocks kept locally before archiving (logos mode) |
| `--data-dir DIR` | current dir | where node data lives |
| `reset` | | delete the node data for the selected storage mode |

The runtime continuously owns:

```text
NodeCore  (MemoryState + pending state + BlockProducer + mempool + tx index)
+
NodeStorage
  ├── logos: HybridBlockStore + LogosStorageCheckpointBackend
  └── local: FileBlockStore   + FileCheckpointBackend
+
JSON-RPC server (jsonrpsee)
```

Core and storage sit behind separate locks that are never held together,
so a slow Logos Storage upload never blocks RPC reads or transaction
submission. If persistence fails, block production halts instead of letting
in-memory state drift from durable state.

Runtime flow:

```text
eth_sendRawTransaction
      ↓
decode EIP-2718 + recover sender + check chain id
      ↓
dry-run against pending state (nonce, funds, gas)
      ↓
Solizone mempool (FIFO)
      ↓
every --block-time, if transactions are pending:
produce block
      ↓
insert into block storage
      ↓
archive / prune old history if needed
      ↓
snapshot EVM state
      ↓
persist checkpoint to Logos Storage
      ↓
continue
```

If no checkpoint exists, the node starts a fresh chain: it funds the five
standard Foundry/Anvil dev accounts with 10,000 ETH each and persists an
empty genesis block #0 (Ethereum tooling expects a `latest` block). The dev
private keys are public; use them for local development only.

If a previous checkpoint exists:

```text
existing checkpoint detected
        ↓
download from Logos Storage
        ↓
restore EVM state
        ↓
resume BlockProducer
        ↓
verify recovered chain head
        ↓
continue block production
```

A full restart path has been verified with the node stopping at one height and continuing the same parent-linked chain from the next height after recovery.

---

## Ethereum JSON-RPC

Standard Ethereum tooling (Foundry, viem, ethers, wallets) talks to the node
directly:

```bash
cargo run --bin solizone_node -- --storage local

forge create contracts/Counter.sol:Counter --broadcast \
  --rpc-url http://127.0.0.1:8545 \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80

cast send <counter> "increment()" --rpc-url http://127.0.0.1:8545 --private-key 0xac09…ff80
cast call <counter> "count()(uint256)" --rpc-url http://127.0.0.1:8545
```

`scripts/foundry-smoke.sh` runs that whole flow against a running node.

`scripts/foundry-contracts.sh` goes further with `contracts/Zoo.sol`: an ERC20 with
constructor args and custom errors, CREATE/CREATE2 factories, a payable vault,
external library linking, precompiles, environment opcodes, Panic reverts and
call/delegatecall/staticcall between contracts (35 checks).

Chain id is `9001`. Supported methods:

```text
web3_clientVersion  net_version  net_listening
eth_chainId  eth_blockNumber  eth_syncing  eth_accounts
eth_gasPrice  eth_maxPriorityFeePerGas  eth_feeHistory
eth_getBalance  eth_getTransactionCount  eth_getCode  eth_getStorageAt
eth_call  eth_estimateGas
eth_sendRawTransaction  eth_getTransactionByHash  eth_getTransactionReceipt
eth_getBlockByNumber  eth_getBlockByHash  eth_getLogs
```

Behaviour worth knowing:

- Legacy (EIP-155 only), EIP-2930 and EIP-1559 transactions are accepted.
  Blob (EIP-4844) and set-code (EIP-7702) transactions are rejected.
- Fees are not charged yet: gas price, base fee and priority fee are all 0.
- Per-transaction gas is capped at 16,777,216 (EIP-7825, REVM's Osaka spec);
  the block gas limit is 30,000,000.
- Reverts return error code `3` with the revert data and decoded
  `Error(string)` reason, so Foundry shows readable messages.
- Only the latest state is kept: state queries accept `latest`, `pending`
  and the head block; older block numbers return an error.
- Blocks are produced only when transactions are pending, so the chain does
  not grow while idle.

### Signed transactions in blocks (SZT2 / SZB2)

Transactions submitted through JSON-RPC are stored in blocks as their exact
signed EIP-2718 bytes:

```text
SZT1 | internal fields             internal transaction (no signature)
SZT2 | raw signed EIP-2718 bytes   Ethereum transaction
```

On decode, every execution field of an SZT2 transaction (sender, nonce,
target, value, data, gas) is re-derived from the signed bytes, so a stored
block can never carry fields that disagree with its signatures. The
transactions root commits to the SZT2 bytes, i.e. to the signatures.

A block containing any SZT2 transaction is encoded with the `SZB2` magic;
blocks with only internal transactions remain byte-identical `SZB1`, so
existing history keeps decoding and hashing exactly as before. Block
validation also checks that every signed transaction's chain id matches the
block header.

---

## Recovery model

A normal Solizone node does not need to keep the entire chain locally forever.

```text
local node keeps:
├── current reconstructed EVM state
├── latest checkpoint metadata
├── recent block window
└── archive / head metadata

Logos Storage keeps:
├── execution checkpoints
└── older canonical block history
```

After restart:

```text
load latest checkpoint
        ↓
restore state + chain head
        ↓
verify local head
        ↓
continue execution
```

For historical lookups:

```text
request block N
      ↓
check local store
      ↓
not found
      ↓
archive index
      ↓
CID
      ↓
Logos Storage
      ↓
validate + hash-check
```

Longer-term synchronization still requires a network-level canonical discovery mechanism for:

```text
latest checkpoint CID
current canonical head
height → block hash → CID
```

The current implementation uses local metadata for that discovery boundary.

---

## Important distinctions

### Protocol state root

```text
execution state
→ deterministic Solizone state_root
→ committed into block header
```

Used for execution and block correctness.

### Snapshot commitment

```text
serialized deterministic snapshot
→ Keccak256
```

Used as a persistence fingerprint.

It does **not** replace the protocol state root.

### Logos blockchain vs Logos Storage

```text
Logos blockchain
→ canonical publication
→ ordering
→ consensus
→ finality

Logos Storage
→ content storage
→ retrieval by CID
→ checkpoint persistence
→ historical block persistence
```

Storage does not replace blockchain finality.

---

## Checkpoint boundary

```rust
pub trait CheckpointBackend {
    type Error;

    fn save(
        &self,
        checkpoint: &SolizoneCheckpoint,
    ) -> Result<(), Self::Error>;

    fn load(
        &self,
    ) -> Result<Option<SolizoneCheckpoint>, Self::Error>;
}
```

Implemented backends:

```text
CheckpointBackend
├── FileCheckpointBackend
└── LogosStorageCheckpointBackend
```

This keeps REVM independent from the persistence mechanism.

---

## Block storage boundary

```text
BlockStore
├── MemoryBlockStore
├── FileBlockStore
└── HybridBlockStore
```

`HybridBlockStore` adds:

- automatic local retention
- Logos Storage archival
- archive index lookup
- transparent local / remote reads
- historical block validation
- canonical hash verification

---

## Current limitations

Not implemented yet:

- Ethereum fee handling (all fees are zero)
- fee-based transaction replacement in the mempool
- historical state queries (only latest / pending state is kept)
- transaction receipts and the tx-hash index survive only in memory: after
  a restart, blocks are served from storage but receipts for transactions
  from before the restart return null
- publishing produced blocks to Logos from `solizone_node` (still the
  separate `publish` harness)
- WebSocket subscriptions and CORS for browser clients
- full imported-block re-execution validation
- Ethereum MPT state roots
- network-distributed canonical head / CID discovery
- explicit Storage replication policy
- atomic crash-safe checkpoint + block commit protocol
- production-grade graceful shutdown
- production publisher service

These are intentionally deferred to the next development phase.

---

## Next development steps

### Done: raw transactions, mempool, JSON-RPC

Signed Ethereum transactions, the Solizone mempool and the Ethereum JSON-RPC
surface are implemented (see [Ethereum JSON-RPC](#ethereum-json-rpc)).

### 1. Persist receipts and the transaction index

So `eth_getTransactionReceipt` and `eth_getLogs` keep working across
restarts.

### 2. Durable Logos publisher inside the node

Queue produced blocks and publish them through the Zone SDK from
`solizone_node`, tracking inclusion and finality.

### 3. Zoner

Developer toolkit (build, test, deploy) on top of the JSON-RPC surface.

### 4. Independent block validation

A second execution path should be able to consume canonical blocks, re-execute transactions, and independently verify resulting commitments.

### 5. Production hardening

- crash-safe persistence
- graceful shutdown
- archive-index durability
- distributed CID / head discovery
- replication strategy
- durable publisher service

---

## Repository structure

```text
solizone-evm/
├── contracts/
├── contracts-out/
├── src/
│   ├── block.rs
│   ├── block_builder.rs
│   ├── block_producer.rs
│   ├── block_store.rs
│   ├── memory_block_store.rs
│   ├── file_block_store.rs
│   ├── hybrid_block_store.rs
│   ├── logos_storage_client.rs
│   ├── publisher.rs
│   ├── logos_publisher.rs
│   ├── bin/
│   │   ├── solizone_node.rs
│   │   ├── publish.rs
│   │   ├── hybrid_node_restart.rs
│   │   └── ...
│   ├── state/
│   │   ├── memory.rs
│   │   ├── snapshot.rs
│   │   ├── commitment.rs
│   │   ├── backend.rs
│   │   ├── checkpoint.rs
│   │   ├── checkpoint_backend.rs
│   │   ├── file_backend.rs
│   │   ├── file_checkpoint_backend.rs
│   │   └── logos_storage_checkpoint_backend.rs
│   └── execution/
│       ├── revm_engine.rs
│       ├── transaction.rs
│       ├── receipt.rs
│       ├── state_root.rs
│       ├── transactions_root.rs
│       └── receipts_root.rs
└── README.md
```

---

## Useful commands

### Rust

```bash
cargo fmt
cargo check
cargo test
```

Run the long-running Solizone node:

```bash
cargo run --bin solizone_node
```

Reset the local runtime state:

```bash
cargo run --bin solizone_node -- reset
```

Run the Logos publication harness:

```bash
cargo run --bin publish
```

### Logos Storage

```bash
logosctl module load storage_module

logosctl call storage_module init "$(cat storage-config.json)"

logosctl call storage_module start

logosctl call storage_module peerId

logosctl call storage_module manifests
```

---

## Design rule

Keep the boundaries explicit:

```text
execution
   ↓
state / persistence
   ↓
block production
   ↓
canonical history
   ↓
publication
```

REVM should not know how Logos publication works.

REVM should not depend directly on Logos Storage.

Storage and blockchain finality are different layers.

That separation lets Solizone evolve without coupling execution to a single persistence or publication mechanism.

---

**Solizone is an independent research implementation and is not an official Logos project.**