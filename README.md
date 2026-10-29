<div align="center">

# Solizone

**Write Solidity. Run it on Logos.**

An EVM execution layer for the Logos network, with the developer tools to go with it.

`Rust` · `REVM` · `Logos Bedrock / Zone SDK` · `Logos Storage` · `Chain ID 9001`

</div>

> Solizone is an independent project. It is **not** an official Logos project.

---

## In one minute

Logos is a privacy-first network. Most developers, though, already know Solidity, Foundry and the Ethereum toolchain. Solizone lets them use those same tools on Logos.

- You write normal Solidity and deploy with `forge`, `cast`, ethers or viem.
- Solizone runs it on its own EVM (REVM), keeps its own state, and turns every transaction into a canonical block.
- Those blocks are **published to Logos**, which orders them and gives them finality. The history is also stored on **Logos Storage**, so anyone can rebuild the chain from Logos and check every block.

**Solizone has three parts:**

| Part | What it is | Who it is for |
|---|---|---|
| **Solizone EVM** | The node. It runs the EVM, produces blocks and publishes them to Logos. | The core of the project |
| **Zoner** | A command-line toolkit (a thin companion to Foundry) to create a project, run a node, deploy and check status. | Solidity developers |
| **SoliCamp** | A Remix-style Solidity playground that lives inside Logos Basecamp. It is a local test chain with a "Publish to Bedrock" button. | Anyone who wants to try it with zero setup |

---

## Why this matters

**The problem.** Logos has strong privacy and infrastructure foundations, but a new chain only grows if developers can build on it quickly. Learning a new language and new tools stops most of them.

**Our answer.** Meet developers where they already are.

```text
Ethereum developer experience   ->   Solizone EVM   ->   Logos
(Solidity, Foundry, MetaMask)        (execution)          (ordering, finality, storage)
```

**What we split, and why.**

| Solizone is responsible for | Logos is responsible for |
|---|---|
| Running EVM transactions, state, receipts | Publishing blocks |
| Producing canonical blocks | Ordering and consensus |
| Checkpoints and recovery | Finality |
| Checking every block it reads back | Content-addressed storage of checkpoints and old blocks |

This split means Solizone does not need to build its own consensus network or its own storage. It reuses what Logos already provides.

**The point for the ecosystem:** every contract deployed on Solizone produces blocks that other people can download from Logos and replay. Developer activity adds usage to Logos.

---

## How it works

```text
Ethereum tooling (forge, cast, ethers, viem)
        |
        v
Ethereum JSON-RPC            http://127.0.0.1:8545
        |
        v
Transaction pool  ->  Block producer  ->  REVM (executes Solidity)
                                              |
                                              v
                                    Solizone EVM state
                                              |
                                              v
                                   Canonical Solizone block
                       ______________________|______________________
                      |                      |                      |
                      v                      v                      v
               Local block store      Checkpoint              Block publisher
             (recent blocks, fast)   (Logos Storage)         (Logos Zone SDK)
                      |                                             |
                older blocks move                                   v
                to Logos Storage                          Logos blockchain
                                                   (ordering, consensus, finality)
```

### Key ideas

- **Ethereum-compatible RPC.** `forge create`, `forge script`, `cast send` and `cast call` work. Signed transactions (legacy EIP-155, EIP-2930, EIP-1559) are accepted, checked and kept in blocks as raw bytes.
- **Deterministic blocks.** Each block carries commitments to state, transactions and receipts. The same input always gives the same block hash.
- **Restart-safe.** The node stores a checkpoint. After a full restart it restores state and chain head and carries on from where it stopped.
- **Hybrid storage.** Recent blocks stay on local disk. Older blocks move to Logos Storage and can still be fetched and verified (height to hash to content ID).
- **Trust-minimised reading.** Logos Storage is not the source of truth. Every block pulled back is hash-checked and re-executed.
- **Publish without blocking.** Publication runs behind block production. If Logos is slow or offline, the node keeps working and catches up later. It keeps a cursor, so nothing is published twice or skipped after a restart.

---

## What works today

| Area | Status |
|---|---|
| Solidity / EVM execution with REVM | Done |
| Persistent state, receipts and transaction index | Done |
| Canonical blocks, parent-linked chain | Done |
| Signed-transaction mempool and Ethereum JSON-RPC | Done |
| Works with Foundry (`forge create`, `forge script`, `cast`) | Done |
| Checkpoints and recovery through Logos Storage | Done |
| Hybrid block store with archive and pruning | Done |
| Publishing blocks to Logos through the Zone SDK, with finality tracking | Done. Tested against a live Logos node; blocks appear on the channel. |
| Rebuilding a chain from Logos and replaying every block (`zoner sync`) | Done. Tested with in-memory chains; testing on a live channel is pending. |
| Zoner CLI | Done (v1) |
| SoliCamp in Basecamp (compile, deploy, call, files and folders, optional publish) | Working. Still being polished. |

Plain-English note: Logos finalises a block about **12 hours** after it is included, so a block you just published will show as "submitted" for a long time before it shows as "finalized". That is how Logos works, not a Solizone bug.

---

## Try it

### 1. Run a node (about 2 minutes)

```bash
git clone https://github.com/bristinWild/solizone
cd solizone/solizone-evm

# local disk only, no Logos needed
cargo run --bin solizone_node -- --storage local
```

The node serves JSON-RPC at `http://127.0.0.1:8545` (chain id `9001`) and prints funded dev accounts.

### 2. Deploy a contract with Foundry

```bash
forge create contracts/Counter.sol:Counter --broadcast \
  --rpc-url http://127.0.0.1:8545 \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
```

(That is a well-known public dev key. Never use it for real funds.)

### 3. Publish blocks to Logos

```bash
cargo run --bin solizone_node -- --storage local --publish
```

Check progress:

```bash
curl -s -X POST -H 'content-type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"solizone_publicationStatus","params":[]}' \
  http://127.0.0.1:8545
```

Publishing needs a running Logos node and a funded wallet key. An inscription costs about 3,000 base units.

### 4. Or use Zoner

```bash
cargo install --path .          # from the zoner repo

zoner init my-project           # new Foundry project preconfigured for Solizone
zoner node                      # run a local node
zoner deploy Counter            # deploy
zoner status                    # chain id, head, publication status
zoner sync                      # rebuild and verify the chain from Logos
```

### 5. Or use SoliCamp (no terminal)

Install the SoliCamp package in Logos Basecamp. Then:

1. Write or open Solidity in the editor. Files and folders are supported.
2. Press **Compile**, then **Deploy** and call functions from the **Interact** panel.
3. When you want it on the network, press **Publish to Bedrock**.

SoliCamp is a sandbox like Anvil. **Nothing is published and no fees are charged until you press Publish.**

---

## SoliCamp in more detail

- Native Qt QML interface running inside Logos Basecamp (no web view).
- A Rust core module embeds the Solizone node and a pinned `solc` compiler.
- Each "room" has its own chain and its own workspace of `.sol` files (autosaved, drag to organise).
- Publishing is opt-in. Each room gets its own channel and sequencer key, paid from your Basecamp wallet. Only the host of a shared room publishes.
- Repos: [`solicamp`](https://github.com/bristinWild/solicamp) (`solizone-module/` and `solicamp-ui/`).

---

## Repositories

| Repo | Contents |
|---|---|
| [`bristinWild/solizone`](https://github.com/bristinWild/solizone) | The node, `solizone-evm/` (Rust), plus research and roadmap |
| `Zoner` (own repository) | Developer CLI |
| [`bristinWild/solicamp`](https://github.com/bristinWild/solicamp) | Basecamp module (`solizone-module/`) and QML UI (`solicamp-ui/`) |
| [`bristinWild/solizone-website`](https://github.com/bristinWild/solizone-website) | Project website |

```text
solizone/
├── solizone-evm/      # the node (Rust): execution, blocks, storage, publishing, RPC
├── research/          # protocol and design research
├── experiments/       # Logos / Zone SDK experiments
└── DEVELOPMENT_ROADMAP.md
```

---

## Honest status and limits

We would rather be clear than oversell.

- This is an **early research implementation**, not a production network.
- Fees are zero (no gas market yet). Mempool replacement by fee is not built.
- Only the latest state is kept; there are no historical state queries yet.
- State roots are Solizone commitments, not Ethereum Merkle-Patricia roots.
- A single sequencer produces blocks. That is fine for a demo, and decentralising it is future work.
- Crash-safe commits, graceful shutdown and a production publisher service are still to come.
- Rebuilding a chain from a live Logos channel still needs a full test (blocks only become readable after finality).

---

## Roadmap

| Phase | Goal | State |
|---|---|---|
| 1 | Raw transactions, mempool, JSON-RPC, Foundry support | Done |
| 2 | Persistent receipts and transaction index | Done |
| 3 | Publish blocks to Logos from inside the node | Done |
| 4 | Read blocks back from Logos and verify them by replay | Done |
| 5 | Zoner CLI v1 | Done |
| 6 | SoliCamp playground in Basecamp | In progress |
| 7 | Live end-to-end test against a Logos channel; stand-alone validator that follows a live channel | Next |
| 8 | Fee market, historical state queries, WebSocket subscriptions | Planned |
| 9 | Production hardening, decentralised sequencing | Planned |

---

## For judges: what to look at

1. **Run the node and deploy with Foundry.** This shows Ethereum compatibility. (5 minutes)
2. **Restart the node.** It recovers state, receipts and its chain head.
3. **Run with `--publish`** and check `solizone_publicationStatus`. This shows the Logos connection.
4. **Run `cargo test`** in `solizone-evm/` for the automated tests.
5. **Open SoliCamp in Basecamp** for the no-terminal experience.

## For investors: the short version

- **Wedge.** Solidity developers can build on Logos without relearning anything.
- **Leverage.** We reuse Logos for consensus, finality and storage instead of rebuilding them. The team stays small and the surface area stays small.
- **Distribution.** SoliCamp ships inside Basecamp, so every Basecamp user is a possible first-time builder.
- **Tools around the chain.** Zoner and SoliCamp are the part developers touch every day.
- **Where we are.** The core pipeline (execute, block, store, publish, recover, replay) works. The remaining work is a live end-to-end test, a fee model, and hardening. We have not claimed users, revenue or market numbers here, because we have none to show yet.

---

## Team and contact

**Bristin Borah**, founder and lead engineer. Full-stack Web3 engineer with 4+ years on EVM chains and Solana, working in Rust and Solidity. [bristinborah.com](https://bristinborah.com) · [github.com/bristinWild](https://github.com/bristinWild)

<!-- Add: demo video link, contact email, any co-builders -->

---

<sub>Solizone is an independent research project and is not affiliated with or endorsed by Logos.</sub>