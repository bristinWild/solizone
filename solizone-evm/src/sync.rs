//! Rebuilding a node's storage from the chain published on Logos.
//!
//! [`ChainImporter`] takes channel payloads in on-chain order, picks out the
//! Solizone blocks, *replays* each one against the genesis state (a block
//! that does not reproduce itself is refused), and persists it exactly like a
//! block the node produced itself: receipts, block, then checkpoint. A node
//! started on that storage resumes at the imported head.
//!
//! [`sync_from_logos`] feeds the importer from the channel's finalized
//! history.

use revm::primitives::{Address, B256, U256};

use crate::{
    node::{core::NodeConfig, storage::NodeStorage},
    publication::reader::{self, BlockAssembler, Pushed},
    replay::ChainReplayer,
};

/// What an import found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    /// Blocks verified and stored.
    pub imported: u64,
    /// Height and hash of the last stored block, if any.
    pub head: Option<(u64, B256)>,
    /// Channel payloads that were not Solizone blocks.
    pub foreign: u64,
    pub malformed: u64,
    /// Re-published copies skipped.
    pub duplicates: u64,
    /// Blocks received but not stored because an earlier block never showed
    /// up. Non-zero means the channel history has a gap.
    pub buffered: usize,
    /// The next height the importer is waiting for.
    pub next_height: u64,
    /// Inscriptions seen on the channel (all kinds).
    pub inscriptions: u64,
}

pub struct ChainImporter<'a> {
    storage: &'a mut dyn NodeStorage,
    assembler: BlockAssembler,
    replayer: ChainReplayer,
    imported: u64,
    head: Option<(u64, B256)>,
}

impl<'a> ChainImporter<'a> {
    /// Imports onto empty `storage`, replaying from `genesis`.
    pub fn new(
        storage: &'a mut dyn NodeStorage,
        config: &NodeConfig,
        genesis: &[(Address, U256)],
    ) -> Result<Self, String> {
        if storage.load_checkpoint()?.is_some() {
            return Err(
                "storage already contains a chain; sync from Logos needs an empty data directory"
                    .into(),
            );
        }

        Ok(Self {
            storage,
            assembler: BlockAssembler::new(),
            replayer: ChainReplayer::from_genesis(config, genesis),
            imported: 0,
            head: None,
        })
    }

    /// Handles one channel payload. Errors mean a Solizone block failed
    /// verification or storage failed; both stop the import.
    pub fn push(&mut self, payload: &[u8]) -> Result<Pushed, String> {
        let pushed = self.assembler.push(payload);

        while let Some(block) = self.assembler.pop_ready() {
            let height = block.header.height;

            let receipts = self
                .replayer
                .apply(&block)
                .map_err(|error| format!("refusing published chain: {error}"))?;

            let hash = block.hash();

            self.storage
                .persist(block, &receipts, &self.replayer.checkpoint())?;

            self.imported += 1;
            self.head = Some((height, hash));
        }

        Ok(pushed)
    }

    pub fn imported(&self) -> u64 {
        self.imported
    }

    pub fn finish(self, inscriptions: u64) -> ImportReport {
        ImportReport {
            imported: self.imported,
            head: self.head,
            foreign: self.assembler.foreign,
            malformed: self.assembler.malformed,
            duplicates: self.assembler.duplicates,
            buffered: self.assembler.buffered(),
            next_height: self.assembler.next_height(),
            inscriptions,
        }
    }
}

/// Reads the channel's finalized history from `node_url` and rebuilds the
/// chain into `storage`. Prints progress as blocks are verified.
pub async fn sync_from_logos(
    storage: &mut dyn NodeStorage,
    config: &NodeConfig,
    genesis: &[(Address, U256)],
    node_url: &str,
    channel_id: [u8; 32],
) -> Result<ImportReport, String> {
    let mut importer = ChainImporter::new(storage, config, genesis)?;

    let summary = reader::read_finalized(node_url, channel_id, |payload| {
        let before = importer.imported();

        importer.push(payload)?;

        if importer.imported() != before && importer.imported().is_multiple_of(100) {
            println!("verified {} block(s) from Logos", importer.imported());
        }

        Ok(())
    })
    .await?;

    Ok(importer.finish(summary.inscriptions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        eth::test_utils::{TxParams, dev_signer, sign_eip1559},
        node::{
            core::{BlockCommit, NodeCore},
            dev_accounts::{DEV_ACCOUNTS, dev_account_balance},
            storage::MemoryNodeStorage,
        },
    };
    use revm::primitives::address;

    const NOW: u64 = 1_800_000_000;
    const BOB: Address = address!("2222222222222222222222222222222222222222");

    fn genesis() -> Vec<(Address, U256)> {
        DEV_ACCOUNTS
            .iter()
            .map(|account| (account.address, dev_account_balance()))
            .collect()
    }

    fn chain(transfers: u64) -> Vec<BlockCommit> {
        let (mut node, genesis_commit) = NodeCore::new(NodeConfig::default(), &genesis(), NOW);
        let mut commits = vec![genesis_commit];

        for nonce in 0..transfers {
            let raw = sign_eip1559(&dev_signer(0), TxParams::transfer(9001, nonce, BOB, 5));
            node.submit_raw_transaction(&raw, NOW).unwrap();
            commits.push(node.produce_block(NOW + nonce + 1).unwrap());
        }

        commits
    }

    fn importer(storage: &mut MemoryNodeStorage) -> ChainImporter<'_> {
        ChainImporter::new(storage, &NodeConfig::default(), &genesis()).unwrap()
    }

    #[test]
    fn imports_a_published_chain_into_storage() {
        let commits = chain(3);
        let mut storage = MemoryNodeStorage::default();

        {
            let mut importer = importer(&mut storage);

            for commit in &commits {
                assert_eq!(
                    importer.push(&commit.block.encode()).unwrap(),
                    Pushed::Stored
                );
            }

            let report = importer.finish(4);
            let last = commits.last().unwrap();

            assert_eq!(report.imported, 4);
            assert_eq!(report.head, Some((3, last.block.hash())));
            assert_eq!(report.next_height, 4);
            assert_eq!(report.buffered, 0);
        }

        // Stored like a block the node produced itself.
        assert_eq!(
            storage.load_checkpoint().unwrap(),
            Some(commits[3].checkpoint.clone())
        );

        for commit in &commits {
            let height = commit.block.header.height;

            assert_eq!(
                storage.block_by_height(height).unwrap(),
                Some(commit.block.clone())
            );

            let stored = storage.block_receipts(height).unwrap().unwrap();
            let receipts: Vec<_> = stored.entries.iter().map(|(_, r)| r.clone()).collect();
            assert_eq!(receipts, commit.receipts);
        }
    }

    #[test]
    fn a_node_resumes_from_the_imported_chain() {
        let commits = chain(2);
        let mut storage = MemoryNodeStorage::default();

        {
            let mut importer = importer(&mut storage);
            for commit in &commits {
                importer.push(&commit.block.encode()).unwrap();
            }
        }

        let checkpoint = storage.load_checkpoint().unwrap().unwrap();
        let head = storage.block_by_height(2).unwrap();

        let mut node = NodeCore::resume(NodeConfig::default(), checkpoint, head);

        assert_eq!(node.head_height(), Some(2));

        // It carries on producing valid blocks on top.
        let raw = sign_eip1559(&dev_signer(0), TxParams::transfer(9001, 2, BOB, 5));
        node.submit_raw_transaction(&raw, NOW).unwrap();
        let next = node.produce_block(NOW + 10).unwrap();

        assert_eq!(next.block.header.height, 3);
        assert_eq!(next.block.header.parent_hash, commits[2].block.hash());
    }

    #[test]
    fn mixed_channel_traffic_is_tolerated() {
        let commits = chain(1);
        let mut storage = MemoryNodeStorage::default();
        let mut importer = importer(&mut storage);

        assert_eq!(importer.push(b"not a block").unwrap(), Pushed::Foreign);
        importer.push(&commits[0].block.encode()).unwrap();
        // A republished copy.
        assert_eq!(
            importer.push(&commits[0].block.encode()).unwrap(),
            Pushed::Duplicate
        );
        importer.push(&commits[1].block.encode()).unwrap();

        let report = importer.finish(4);

        assert_eq!(report.imported, 2);
        assert_eq!((report.foreign, report.duplicates), (1, 1));
    }

    #[test]
    fn a_forged_block_stops_the_import_and_keeps_the_good_prefix() {
        let commits = chain(2);
        let mut storage = MemoryNodeStorage::default();

        {
            let mut importer = importer(&mut storage);
            importer.push(&commits[0].block.encode()).unwrap();
            importer.push(&commits[1].block.encode()).unwrap();

            let mut forged = commits[2].block.clone();
            forged.header.state_root = B256::repeat_byte(3);

            let error = importer.push(&forged.encode()).unwrap_err();
            assert!(error.contains("refusing published chain"), "{error}");
        }

        // Blocks #0 and #1 are stored; the forged #2 is not.
        assert_eq!(
            storage.load_checkpoint().unwrap(),
            Some(commits[1].checkpoint.clone())
        );
        assert!(storage.block_by_height(2).unwrap().is_none());
    }

    #[test]
    fn a_gap_is_reported_not_papered_over() {
        let commits = chain(3);
        let mut storage = MemoryNodeStorage::default();
        let mut importer = importer(&mut storage);

        importer.push(&commits[0].block.encode()).unwrap();
        // #1 never arrives.
        importer.push(&commits[2].block.encode()).unwrap();
        importer.push(&commits[3].block.encode()).unwrap();

        let report = importer.finish(3);

        assert_eq!(report.imported, 1);
        assert_eq!(report.next_height, 1);
        assert_eq!(report.buffered, 2);
    }

    #[test]
    fn refuses_storage_that_already_has_a_chain() {
        let commits = chain(0);
        let mut storage = MemoryNodeStorage::default();

        importer(&mut storage)
            .push(&commits[0].block.encode())
            .unwrap();

        assert!(ChainImporter::new(&mut storage, &NodeConfig::default(), &genesis()).is_err());
    }
}
