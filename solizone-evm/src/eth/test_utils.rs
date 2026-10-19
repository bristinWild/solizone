//! Helpers for producing real signed Ethereum transactions in tests.

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope, TxLegacy};
use alloy_eips::eip2718::Encodable2718;
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;
use revm::primitives::{Address, Bytes, TxKind, U256};

use crate::node::dev_accounts::DEV_ACCOUNTS;

pub fn dev_signer(index: usize) -> PrivateKeySigner {
    DEV_ACCOUNTS[index].private_key.parse().unwrap()
}

pub struct TxParams {
    pub chain_id: u64,
    pub nonce: u64,
    pub to: Option<Address>,
    pub value: U256,
    pub input: Bytes,
    pub gas_limit: u64,
}

impl TxParams {
    pub fn transfer(chain_id: u64, nonce: u64, to: Address, value: u64) -> Self {
        Self {
            chain_id,
            nonce,
            to: Some(to),
            value: U256::from(value),
            input: Bytes::new(),
            gas_limit: 21_000,
        }
    }
}

fn kind(to: Option<Address>) -> TxKind {
    to.map_or(TxKind::Create, TxKind::Call)
}

/// Signs an EIP-1559 transaction and returns its raw EIP-2718 bytes.
pub fn sign_eip1559(signer: &PrivateKeySigner, params: TxParams) -> Bytes {
    let tx = TxEip1559 {
        chain_id: params.chain_id,
        nonce: params.nonce,
        gas_limit: params.gas_limit,
        max_fee_per_gas: 2_000_000_000,
        max_priority_fee_per_gas: 1_000_000_000,
        to: kind(params.to),
        value: params.value,
        access_list: Default::default(),
        input: params.input,
    };

    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    let envelope: TxEnvelope = tx.into_signed(signature).into();

    Bytes::from(envelope.encoded_2718())
}

/// Signs a legacy transaction; `chain_id: None` produces a pre-EIP-155 tx.
pub fn sign_legacy(signer: &PrivateKeySigner, chain_id: Option<u64>, params: TxParams) -> Bytes {
    let tx = TxLegacy {
        chain_id,
        nonce: params.nonce,
        gas_price: 1_000_000_000,
        gas_limit: params.gas_limit,
        to: kind(params.to),
        value: params.value,
        input: params.input,
    };

    let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
    let envelope: TxEnvelope = tx.into_signed(signature).into();

    Bytes::from(envelope.encoded_2718())
}
