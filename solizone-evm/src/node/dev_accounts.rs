//! Pre-funded development accounts.
//!
//! These are the well-known Foundry / Anvil / Hardhat test accounts derived
//! from the public mnemonic "test test test test test test test test test
//! test test junk". Their private keys are PUBLIC: use them only on local
//! development nodes, never for anything holding real value.

use revm::primitives::{Address, U256, address};

pub struct DevAccount {
    pub address: Address,
    pub private_key: &'static str,
}

pub const DEV_ACCOUNTS: [DevAccount; 5] = [
    DevAccount {
        address: address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"),
        private_key: "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    },
    DevAccount {
        address: address!("70997970C51812dc3A010C7d01b50e0d17dc79C8"),
        private_key: "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    },
    DevAccount {
        address: address!("3C44CdDdB6a900fa2b585dd299e03d12FA4293BC"),
        private_key: "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
    },
    DevAccount {
        address: address!("90F79bf6EB2c4f870365E785982E1f101E93b906"),
        private_key: "0x7c852118294e51e653712a81e05800f419141751be58f605c371e15141b007a6",
    },
    DevAccount {
        address: address!("15d34AAf54267DB7D7c367839AAf71A00a2C6A65"),
        private_key: "0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a",
    },
];

/// 10,000 ETH (in wei) per dev account.
pub fn dev_account_balance() -> U256 {
    U256::from(10_000u64) * U256::from(10u64).pow(U256::from(18u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_signer_local::PrivateKeySigner;

    #[test]
    fn dev_keys_match_addresses() {
        for account in &DEV_ACCOUNTS {
            let signer: PrivateKeySigner = account.private_key.parse().unwrap();
            assert_eq!(signer.address(), account.address);
        }
    }
}
