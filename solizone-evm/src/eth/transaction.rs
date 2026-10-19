//! Signed Ethereum transaction ingestion.
//!
//! Raw EIP-2718 bytes produced by standard tooling (Foundry, viem, ethers,
//! wallets) are decoded, checked for replay protection, and sender-recovered
//! here, then converted into the internal `SolizoneTransaction` that REVM
//! executes. The original signed bytes travel with the transaction so the
//! block can carry them (SZT2 / SZB2) and any node can re-verify them.

use alloy_consensus::{Transaction as _, TxEnvelope, transaction::SignerRecoverable};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use revm::primitives::{Address, B256, Bytes, TxKind};

use crate::execution::transaction::{SolizoneTransaction, TransactionKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EthTransactionError {
    /// Bytes are not a valid EIP-2718 transaction envelope.
    Decode(String),
    /// Bytes decode but are not in canonical form (would change the tx hash).
    NonCanonical,
    /// EIP-4844 blob and EIP-7702 set-code transactions are not supported yet.
    UnsupportedType(u8),
    /// Legacy transaction without EIP-155 replay protection.
    MissingChainId,
    WrongChainId {
        expected: u64,
        actual: u64,
    },
    InvalidSignature(String),
}

impl std::fmt::Display for EthTransactionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode(reason) => write!(f, "failed to decode transaction: {reason}"),
            Self::NonCanonical => write!(f, "transaction encoding is not canonical"),
            Self::UnsupportedType(ty) => write!(f, "transaction type {ty} is not supported"),
            Self::MissingChainId => {
                write!(f, "legacy transactions must be EIP-155 replay protected")
            }
            Self::WrongChainId { expected, actual } => {
                write!(f, "invalid chain id: expected {expected}, got {actual}")
            }
            Self::InvalidSignature(reason) => write!(f, "invalid signature: {reason}"),
        }
    }
}

impl std::error::Error for EthTransactionError {}

/// A decoded, signature-verified Ethereum transaction.
#[derive(Debug, Clone)]
pub struct SignedEthTransaction {
    pub envelope: TxEnvelope,
    pub sender: Address,
    pub raw: Bytes,
}

impl SignedEthTransaction {
    /// Decodes and sender-recovers raw signed bytes without checking the
    /// chain id (used when decoding stored blocks; block validation checks
    /// the chain id against the header).
    pub fn decode(raw: &[u8]) -> Result<Self, EthTransactionError> {
        let mut buf = raw;

        let envelope = TxEnvelope::decode_2718(&mut buf)
            .map_err(|error| EthTransactionError::Decode(error.to_string()))?;

        if !buf.is_empty() {
            return Err(EthTransactionError::Decode("trailing bytes".to_string()));
        }

        if envelope.encoded_2718().as_slice() != raw {
            return Err(EthTransactionError::NonCanonical);
        }

        match &envelope {
            TxEnvelope::Legacy(_) | TxEnvelope::Eip2930(_) | TxEnvelope::Eip1559(_) => {}
            TxEnvelope::Eip4844(_) => return Err(EthTransactionError::UnsupportedType(3)),
            TxEnvelope::Eip7702(_) => return Err(EthTransactionError::UnsupportedType(4)),
        }

        if envelope.chain_id().is_none() {
            return Err(EthTransactionError::MissingChainId);
        }

        let sender = envelope
            .recover_signer()
            .map_err(|error| EthTransactionError::InvalidSignature(error.to_string()))?;

        Ok(Self {
            envelope,
            sender,
            raw: Bytes::copy_from_slice(raw),
        })
    }

    /// Decodes raw bytes submitted to this chain (`eth_sendRawTransaction`).
    pub fn decode_for_chain(raw: &[u8], chain_id: u64) -> Result<Self, EthTransactionError> {
        let tx = Self::decode(raw)?;

        let actual = tx.chain_id();

        if actual != chain_id {
            return Err(EthTransactionError::WrongChainId {
                expected: chain_id,
                actual,
            });
        }

        Ok(tx)
    }

    /// Standard Ethereum transaction hash (keccak256 of the EIP-2718 bytes).
    pub fn hash(&self) -> B256 {
        *self.envelope.tx_hash()
    }

    pub fn chain_id(&self) -> u64 {
        self.envelope
            .chain_id()
            .expect("chain id presence checked at decode")
    }

    /// Execution form of this transaction. Fee fields are not charged on
    /// Solizone today (zero gas price), so they are not carried over.
    pub fn to_solizone(&self) -> SolizoneTransaction {
        let kind = match self.envelope.kind() {
            TxKind::Create => TransactionKind::Create,
            TxKind::Call(to) => TransactionKind::Call(to),
        };

        SolizoneTransaction {
            sender: self.sender,
            nonce: self.envelope.nonce(),
            kind,
            value: self.envelope.value(),
            data: self.envelope.input().clone(),
            gas_limit: self.envelope.gas_limit(),
            signed: Some(self.raw.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::eth::test_utils::{TxParams, dev_signer, sign_eip1559, sign_legacy};
    use revm::primitives::{address, keccak256};

    const CHAIN_ID: u64 = 9001;
    const BOB: Address = address!("2222222222222222222222222222222222222222");

    #[test]
    fn decodes_and_recovers_eip1559() {
        let signer = dev_signer(0);
        let raw = sign_eip1559(&signer, TxParams::transfer(CHAIN_ID, 3, BOB, 500));

        let tx = SignedEthTransaction::decode_for_chain(&raw, CHAIN_ID).unwrap();

        assert_eq!(tx.sender, signer.address());
        assert_eq!(tx.hash(), keccak256(&raw));

        let internal = tx.to_solizone();
        assert_eq!(internal.sender, signer.address());
        assert_eq!(internal.nonce, 3);
        assert_eq!(internal.kind, TransactionKind::Call(BOB));
        assert_eq!(internal.value, revm::primitives::U256::from(500));
        assert_eq!(internal.gas_limit, 21_000);
        assert_eq!(internal.eth_hash(), Some(tx.hash()));
    }

    #[test]
    fn decodes_eip155_legacy() {
        let signer = dev_signer(1);
        let raw = sign_legacy(
            &signer,
            Some(CHAIN_ID),
            TxParams::transfer(CHAIN_ID, 0, BOB, 1),
        );

        let tx = SignedEthTransaction::decode_for_chain(&raw, CHAIN_ID).unwrap();
        assert_eq!(tx.sender, signer.address());
    }

    #[test]
    fn rejects_unprotected_legacy() {
        let raw = sign_legacy(
            &dev_signer(0),
            None,
            TxParams::transfer(CHAIN_ID, 0, BOB, 1),
        );

        assert_eq!(
            SignedEthTransaction::decode(&raw).unwrap_err(),
            EthTransactionError::MissingChainId
        );
    }

    #[test]
    fn rejects_wrong_chain() {
        let raw = sign_eip1559(&dev_signer(0), TxParams::transfer(1, 0, BOB, 1));

        assert_eq!(
            SignedEthTransaction::decode_for_chain(&raw, CHAIN_ID).unwrap_err(),
            EthTransactionError::WrongChainId {
                expected: CHAIN_ID,
                actual: 1
            }
        );
    }

    #[test]
    fn tampered_bytes_change_sender_or_fail() {
        let signer = dev_signer(0);
        let raw = sign_eip1559(&signer, TxParams::transfer(CHAIN_ID, 0, BOB, 1));

        let mut tampered = raw.to_vec();
        let last = tampered.len() - 40; // inside the signature r value
        tampered[last] ^= 0x01;

        if let Ok(tx) = SignedEthTransaction::decode(&tampered) {
            assert_ne!(tx.sender, signer.address());
        }
    }

    #[test]
    fn rejects_garbage_and_trailing_bytes() {
        assert!(matches!(
            SignedEthTransaction::decode(&[0x02, 0xff, 0x00]),
            Err(EthTransactionError::Decode(_))
        ));

        let mut raw =
            sign_eip1559(&dev_signer(0), TxParams::transfer(CHAIN_ID, 0, BOB, 1)).to_vec();
        raw.push(0x00);
        assert!(SignedEthTransaction::decode(&raw).is_err());
    }

    #[test]
    fn signed_transaction_round_trips_through_szt2() {
        let raw = sign_eip1559(&dev_signer(2), TxParams::transfer(CHAIN_ID, 7, BOB, 42));
        let tx = SignedEthTransaction::decode(&raw).unwrap().to_solizone();

        let encoded = tx.encode_canonical();
        assert_eq!(&encoded[..4], b"SZT2");
        assert_eq!(&encoded[4..], raw.as_ref());

        let decoded = SolizoneTransaction::decode_canonical(&encoded).unwrap();
        assert_eq!(decoded, tx);
    }
}
