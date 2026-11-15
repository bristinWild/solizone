# Solizone Ecosystem Tools Plan

> **Status:** Planned, not started
> **As of:** 2026-11-15
> **Scope:** `solizone_module`, Zoner, `solizone-evm`, a sample Basecamp app
> **Companion to:** [PriFi Build Plan](./prifi-build-plan.md)
>
> Independent project. Not an official Logos project.

---

## Why this plan

The PriFi build plan makes Solizone safe to sign, private to read and fair to submit. This plan makes it **easy to build on**. It covers the tools a developer needs to go from "I write Solidity" to "my app runs on Logos and other people can use it."

The privacy features themselves (Solizone Signer, `simulate`, pinned interface CIDs, local-first reads) are specified in the PriFi build plan. This plan packages them for developers, and adds what a working ecosystem needs around them.

### Already shipped

- `zoner init` creates a Foundry project preconfigured for Solizone, with a single `Counter.sol` starter.
- `zoner deploy`, `zoner status`, `zoner accounts` and `zoner sync` (rebuild a chain from Logos).
- `solizone_module` already exposes `deploy`, `send`, `read`, `encode`, `decode` and `rpc` to any Basecamp app, through a generated interface.
- The node serves the standard Ethereum JSON-RPC calls an explorer needs: blocks, transactions, receipts, logs and code.
- Blocks are published to Bedrock and can be followed and verified by anyone.

### The whole plan as an analogy

Solizone today is a working engine. This plan adds the dashboard, the manual, a test track anyone can drive on, and a few ready-made cars to start from.

---

## What the developer sees

| | Today | After this plan |
| --- | --- | --- |
| Starting a project | `zoner init` gives an empty Counter contract | `zoner init --template token` (or `multisig`, `escrow`) gives working contracts, tests and a deploy script |
| Connecting a Basecamp app | Read the Rust source to find out what `solizone_module` offers | Documented methods and events, plus a sample app to copy from |
| Sharing a contract | Only on your own machine or in a SoliCamp room | Deploy to the public Solizone testnet and share the address |
| Inspecting the chain | Raw RPC calls | A local explorer inside Basecamp |

---

## 1. App API for Basecamp builders

**What it is:** documentation and a sample app that show any Basecamp team how to connect their app to Solidity contracts through `solizone_module`.

**Builds on:** the module's existing generated interface and event emitters. New signing and reading methods come from the PriFi plan's Phase A.

### What we build

1. **API reference.** Every `solizone_module` method and event, with arguments, return values and one example each. Generated from the module's interface file so it can't drift from the code.
2. **Sample Basecamp app.** A small QML app (for example, a community fund with a "Donate" button) that reads contract state, requests a signature and reacts to the result event.
3. **Integration guide.** A short walkthrough: install Solizone in Basecamp, call the module, handle approval and rejection.

### What the developer sees

```qml
// "Donate" button in a community-fund app
logos.callModuleAsync("solizone_module", "request_signature",
    [JSON.stringify({ to: fundAddress, value: "0.1" })])
// then listen for the "signature_resolved" event → show "Thank you!"
```

### Analogy

A payment provider's developer docs and demo shop: you copy the example, change the details, and your shop takes payments.

### Done when

- [ ] Every public `solizone_module` method and event is documented with an example.
- [ ] The sample app runs in Basecamp, reads state, and completes a signed transaction through Solizone Signer.
- [ ] A developer new to Solizone can connect a fresh Basecamp app by following only the guide.

---

## 2. Zoner templates

**What it is:** ready-made Foundry projects, so developers start from working, tested code instead of a blank folder.

**Builds on:** `zoner init`, which already writes `foundry.toml`, `src/Counter.sol`, `.gitignore` and a README.

### What we build

1. **`--template` flag on `zoner init`.** Keeps today's Counter as the default.
2. **Three templates**, each with contracts, Foundry tests, a deploy script and a README:
   - `token`: an OpenZeppelin-based ERC-20.
   - `multisig`: an M-of-N wallet.
   - `escrow`: settles only when both parties' typed-data signatures match the agreed terms (the optional escrow template from the PriFi plan).
3. **Pinned-interface support built in.** Each template's contract includes `interfaceCid()` and `abiCid()`, so `zoner publish-ui` (PriFi plan, Phase A) works on it with no changes.

### What the developer sees

```
$ zoner init my-token --template token
  created my-token/  (contract, tests, deploy script, UI folder)
$ forge test
$ zoner deploy
  Token deployed at 0x5Fb…
$ zoner publish-ui
  CIDs recorded in Token ✓
```

### Analogy

A recipe kit with the ingredients already measured out: you still cook, but you don't start by guessing quantities.

### Done when

- [ ] `zoner init --template <name>` creates each template, and `forge test` passes out of the box.
- [ ] Each template deploys to a local Solizone node and to the public testnet with `zoner deploy`.
- [ ] Each template works with `zoner publish-ui` without edits.

---

## 3. Public Solizone testnet

**What it is:** one shared Solizone chain, with blocks published to the Logos testnet, that anyone can deploy to and use. It is the step from "works on my laptop" to "share a link to my contract".

**Builds on:** the node, Bedrock publishing and replay, and `zoner sync`, which already lets anyone rebuild and verify the chain from Logos.

### What we build

1. **Hosted sequencer** publishing to the Logos testnet channel, with its own chain ID. Anyone can run a follower and verify it; nobody has to trust the host's RPC.
2. **Public-network settings.** Two local-dev defaults are unsafe on a shared chain and change for the public testnet only:
   - **No pre-funded public dev keys at genesis.** Their private keys are public, so anyone could use their funds.
   - **A small non-zero gas price.** The node charges zero gas today, which would let anyone spam a shared chain for free.
3. **Faucet.** A small service that sends test funds to a requested address, rate-limited per address.
4. **Network preset in Zoner and the module.** `zoner deploy --network testnet` and `follow_network("testnet")` connect without manual configuration.
5. **Status page** showing the block height, the last block published to Logos, and the faucet balance.

### Analogy

A public test track: anyone can bring their car and drive, and every lap is recorded where anyone can check it.

### Done when

- [ ] The public testnet produces blocks and publishes them to the Logos testnet continuously.
- [ ] A new developer gets test funds from the faucet and deploys a template with one command.
- [ ] An independent follower rebuilds the testnet from Logos and matches the sequencer's state.

---

## 4. Local explorer in Basecamp

**What it is:** a block explorer, like Etherscan, that runs inside Basecamp and reads from the user's own follower node. Nobody learns which addresses or transactions the user looks up.

**Builds on:** the node's existing RPC calls (`eth_getBlockByNumber`, `eth_getTransactionByHash`, `eth_getTransactionReceipt`, `eth_getLogs`, `eth_getCode`) and the local follower from the PriFi plan.

### What we build

1. **Explorer screen** in Basecamp: latest blocks, block detail, transaction detail with decoded calls and events, and contract pages.
2. **Address index.** Standard Ethereum RPC has no "transactions for this address" call, so the explorer builds a small local index while following the chain.
3. **Decoding with pinned ABIs.** Contracts that pin their ABI CID show human-readable calls and events instead of raw data.
4. **Logos publication status per block.** Shows where each block was published on Bedrock, so users can see it's anchored.

### Analogy

Looking up a word in your own dictionary instead of asking a librarian who writes down every word you ask about.

### Done when

- [ ] The explorer shows blocks, transactions, addresses and contracts from the local follower, with no external requests.
- [ ] Calls and events are decoded for contracts with a pinned ABI.
- [ ] Each block shows its Logos publication status.

---

## Order of work

1. **Zoner templates.** No dependencies, quick to ship, and useful immediately.
2. **Public testnet.** Everything shared depends on it.
3. **App API docs and sample app.** Existing methods first; the new signing and reading methods are added as PriFi Phase A lands.
4. **Local explorer.** Needs the local follower from PriFi Phase A.

---

## Limits

- **The testnet sequencer is hosted.** Anyone can verify it by following from Logos, but its uptime depends on one operator until sequencing is decentralised.
- **The faucet can be drained.** Rate limits slow abuse but can't stop it entirely; it may need topping up or stricter limits.
- **The address index is local and grows with the chain.** Fine for a testnet; a large chain may need pruning options.
- **The App API reference covers only methods that exist.** Methods from PriFi Phase A are documented as they ship.