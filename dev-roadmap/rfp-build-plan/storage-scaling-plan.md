# Solizone Storage and Scaling Plan

> **Status:** Planned, not started
> **As of:** 2026-11-17
> **Scope:** `solizone-evm` (node storage, state, checkpoints, archive, receipts)
> **Related:** [PriFi Build Plan](./prifi-build-plan.md) (local-first reads depend on a follower that stays light)
>
> Independent project. Not an official Logos project.

---

## Why this plan

Local-first reads only work if every user can run a Solizone follower on an ordinary laptop. Today, block history is handled well: old blocks move to Logos Storage and only the latest few stay on disk. But four things still grow without limit, and the biggest one is **state**, not chain length.

**One-line summary:** the chain can get long cheaply; it can't get big cheaply. Each block rewrites and re-hashes the entire state, so per-block cost grows with every account and storage slot.

---

## What a quick test showed

One local run, before this plan: 100,000 blocks with one transfer each, using the real node core, storage and replayer (`--no-default-features`, no Logos upload).

| Measure | Result per 100,000 blocks |
| --- | --- |
| Data written (blocks + receipts + archive index) | ~66 MB |
| Actual disk used | **~800 MB**, because every block and receipt is its own small file |
| Archive index (`archive-index.json`) | ~18 MB, rewritten in full every block |
| Block production + persistence | ~0.3 ms per block |
| Full replay from genesis (local files) | ~45 s, or ~0.45 ms per block |

That run kept state almost flat (six accounts), so it measures **chain length only**. A growing-state run was not completed; the state findings below come from reading the code.

---

## Problems

### 1. Full state work on every block (biggest)

All state lives in memory (`state/memory.rs`, a revm `InMemoryDB`). For every block, the node does four full passes over it:

| Step | Where | Cost |
| --- | --- | --- |
| Re-hash every account and slot for the state root | `compute_state_root` called in `execution/revm_engine.rs` | Sorts and hashes all state |
| Copy the whole state into a snapshot | `MemoryState::snapshot()` in `node/core.rs::produce_block` | Full copy |
| Serialise the whole state to JSON and write it | `ChainStorage::persist` → checkpoint backend | Full write; in Logos mode also a full upload (`LogosStorageCheckpointBackend::save`) |
| Clone the whole state again for pending transactions | `rebuild_pending_state` in `node/core.rs` | Full copy |

**Effect:** per-block time, disk writes and upload size all grow with state. Rough estimate: around one million simple accounts gives a JSON checkpoint of about 200 MB, written and uploaded every block. Before that point, RAM becomes the limit, because the whole state must fit in memory.

**Analogy:** saving a document by retyping the whole book every time you change one sentence.

### 2. Receipts are never pruned

`service.rs::open_storage` keeps receipts on local disk in both modes (`FileReceiptStore`, one `<height>.szrb` file per block). Blocks are archived and pruned; receipts are not. They grow with every block, forever.

### 3. The archive index is rewritten in full every block

`hybrid_block_store.rs` archives one block at a time: it uploads the block, then loads the entire `archive-index.json`, adds one entry, sorts it and writes the whole file back. The file grows by about 18 MB per 100,000 blocks, and the work per block grows with it.

### 4. One file per block and per receipt

`file_block_store.rs` writes `<height>.szb` and the receipt store writes `<height>.szrb`. Each file is a few hundred bytes but takes at least 4 KB on disk. That is why ~66 MB of data used ~800 MB.

### 5. Replay downloads one block at a time

A new follower that recovers archived history downloads each block by its own CID (`recover_archived` in `hybrid_block_store.rs`). 100,000 blocks means 100,000 separate Logos Storage downloads.

---

## Fixes, in priority order

### Fix 1: Checkpoint less often, and only what changed

- Write a full checkpoint every N blocks (for example 1,000) instead of every block.
- Between checkpoints, persist only the **state diff** of each block (accounts and slots that changed). revm already reports the changed state from `transact`.
- On restart, load the last full checkpoint and apply the diffs after it.
- Upload to Logos Storage only the periodic full checkpoints.

**Brings:** per-block write and upload drop from "whole state" to "what changed". **Size:** medium.

### Fix 2: Incremental state root

- Replace the full sort-and-hash in `compute_state_root` with a Merkle trie (or sparse Merkle tree) that updates only the paths of changed accounts and slots.
- Activate the new root format at a set block height so older blocks still replay with the old rule.

**Brings:** state-root cost grows with the changes per block, not the total state. **Size:** medium to large; changes consensus rules.

### Fix 3: Move state from memory to an on-disk database

- Back `MemoryState` with an embedded database (for example `redb` or RocksDB, which the Logos feature already pulls in), with an in-memory cache for hot accounts.
- Remove the full `pending_state` clone; build pending state as a thin overlay on top of the canonical state.

**Brings:** state size limited by disk, not RAM. **Size:** large.

### Fix 4: Append-only storage for blocks and receipts

- Store blocks and receipts in one append-only segment file (or a small database) per range, with an offset index, instead of one file each.
- Prune receipts older than a retention window, like blocks. Rebuild old receipts on demand by replaying, or fetch them from an archive.

**Brings:** disk use close to actual data size; receipts stop growing forever. **Size:** small to medium.

### Fix 5: Bundle archives and make the index append-only

- Archive blocks in bundles (for example 1,000 blocks per Logos Storage upload, one CID per bundle).
- Replace `archive-index.json` with an append-only index (one line per bundle), so archiving no longer rewrites the whole file.
- Followers download bundles, not single blocks.

**Brings:** about 1,000× fewer uploads, downloads and index entries. **Size:** small to medium.

---

## Order of work

1. **Fix 4 and Fix 5.** Small, contained to storage, and they stop the disk waste and the index rewrite right away.
2. **Fix 1.** Removes the largest per-block write and upload with no consensus change.
3. **Fix 2.** The incremental state root; needs an activation height.
4. **Fix 3.** On-disk state; the biggest change, needed once state outgrows RAM.

---

## Done when

- [ ] 100,000 blocks use disk close to their actual data size (no small-file overhead).
- [ ] Receipts and the archive index stay bounded as the chain grows.
- [ ] Per-block time and disk writes stay flat as state grows from 10,000 to 1,000,000 accounts.
- [ ] A new follower syncs 100,000 archived blocks with about 100 downloads, not 100,000.
- [ ] A follower runs with state larger than available RAM.

---

## Benchmarks to add

A repeatable benchmark (for example `examples/scale_bench.rs`) that reports, every N blocks:

- disk use (actual, not logical) for blocks, receipts, checkpoint and index;
- block production and persistence time;
- full replay time from genesis;

for two workloads: a **quiet chain** (one transfer per block, flat state) and a **growing chain** (many new accounts per block). Run it before and after each fix.