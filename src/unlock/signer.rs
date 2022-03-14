use ckb_hash::{blake2b_256, new_blake2b};
use ckb_script::ScriptGroup;
use ckb_types::{
    bytes::Bytes,
    core::TransactionView,
    error::VerificationError,
    packed::{self, WitnessArgs},
    prelude::*,
    H160,
};
use std::collections::HashSet;
use thiserror::Error;

use crate::traits::{
    TransactionDependencyError, TransactionDependencyProvider, Wallet, WalletError,
};

#[derive(Error, Debug)]
pub enum SignError {
    #[error("wallet error: `{0}`")]
    Wallet(#[from] WalletError),

    #[error("transaction dependency error: `{0}`")]
    TxDep(#[from] TransactionDependencyError),

    #[error("witness count in current transaction not enough to cover current script group")]
    WitnessNotEnough,

    #[error("the witness is not empty and not WitnessArgs format: `{0}`")]
    InvalidWitnessArgs(#[from] VerificationError),

    #[error("invalid multisig config: `{0}`")]
    InvalidMultisigConfig(String),

    #[error("there already too many signatures in current WitnessArgs.lock field (old_count + new_count > threshold)")]
    TooManySignatures,

    #[error("other error: `{0}`")]
    Other(#[from] Box<dyn std::error::Error>),
}

/// Script signer logic:
///   * Generate message to sign
///   * Sign the message by wallet
///   * Put the signature into tx.witnesses
pub trait ScriptSigner {
    fn match_args(&self, args: &[u8]) -> bool;

    /// Add signature information to witnesses
    fn sign_tx(
        &self,
        tx: &TransactionView,
        script_group: &ScriptGroup,
        // This argument is for inner wallet to use
        tx_dep_provider: &dyn TransactionDependencyProvider,
    ) -> Result<TransactionView, SignError>;

    /// Common logic of generate message for certain script group. Overwrite
    /// this method to support special use case.
    fn generate_message(
        &self,
        tx: &TransactionView,
        script_group: &ScriptGroup,
        zero_lock: Bytes,
    ) -> Result<Bytes, SignError> {
        if tx.witnesses().item_count() <= script_group.input_indices[0] {
            return Err(SignError::WitnessNotEnough);
        }

        let witnesses: Vec<packed::Bytes> = tx.witnesses().into_iter().collect();
        let witness_data = witnesses[script_group.input_indices[0]].raw_data();
        let mut init_witness = if witness_data.is_empty() {
            WitnessArgs::default()
        } else {
            WitnessArgs::from_slice(witness_data.as_ref())?
        };
        init_witness = init_witness
            .as_builder()
            .lock(Some(zero_lock).pack())
            .build();
        // Other witnesses in current script group
        let other_witnesses: Vec<([u8; 8], Bytes)> = script_group
            .input_indices
            .iter()
            .skip(1)
            .filter_map(|idx| witnesses.get(*idx))
            .map(|witness| {
                (
                    (witness.item_count() as u64).to_le_bytes(),
                    witness.raw_data(),
                )
            })
            .collect();
        // The witnesses not covered by any inputs
        let outter_witnesses: Vec<([u8; 8], Bytes)> = if tx.inputs().len() < witnesses.len() {
            witnesses[tx.inputs().len()..witnesses.len()]
                .iter()
                .map(|witness| {
                    (
                        (witness.item_count() as u64).to_le_bytes(),
                        witness.raw_data(),
                    )
                })
                .collect()
        } else {
            Default::default()
        };

        let mut blake2b = new_blake2b();
        blake2b.update(tx.hash().as_slice());
        blake2b.update(&(init_witness.as_bytes().len() as u64).to_le_bytes());
        blake2b.update(&init_witness.as_bytes());
        for (len_le, data) in other_witnesses {
            blake2b.update(&len_le);
            blake2b.update(&data);
        }
        for (len_le, data) in outter_witnesses {
            blake2b.update(&len_le);
            blake2b.update(&data);
        }
        let mut message = vec![0u8; 32];
        blake2b.finalize(&mut message);
        Ok(Bytes::from(message))
    }
}

/// Signer for secp256k1 sighash all lock script
pub struct Secp256k1SighashSigner {
    // Can be: SecpCkbRawKeyWallet, HardwareWallet
    wallet: Box<dyn Wallet>,
}

impl Secp256k1SighashSigner {
    pub fn new(wallet: Box<dyn Wallet>) -> Secp256k1SighashSigner {
        Secp256k1SighashSigner { wallet }
    }

    pub fn wallet(&self) -> &dyn Wallet {
        self.wallet.as_ref()
    }

    fn sign_tx_with_owner_id(
        &self,
        owner_id: &[u8],
        tx: &TransactionView,
        script_group: &ScriptGroup,
        tx_dep_provider: &dyn TransactionDependencyProvider,
    ) -> Result<TransactionView, SignError> {
        let witness_idx = script_group.input_indices[0];
        let mut witnesses: Vec<packed::Bytes> = tx.witnesses().into_iter().collect();
        while witnesses.len() <= witness_idx {
            witnesses.push(Default::default());
        }
        let tx_new = tx
            .as_advanced_builder()
            .set_witnesses(witnesses.clone())
            .build();

        let zero_lock = Bytes::from(vec![0u8; 65]);
        let message = self.generate_message(&tx_new, script_group, zero_lock)?;

        let signature = self
            .wallet
            .sign(owner_id, message.as_ref(), tx, tx_dep_provider)?;

        // Put signature into witness
        let witness_data = witnesses[witness_idx].raw_data();
        let mut current_witness: WitnessArgs = if witness_data.is_empty() {
            WitnessArgs::default()
        } else {
            WitnessArgs::from_slice(witness_data.as_ref())?
        };
        current_witness = current_witness
            .as_builder()
            .lock(Some(signature).pack())
            .build();
        witnesses[witness_idx] = current_witness.as_bytes().pack();
        Ok(tx.as_advanced_builder().set_witnesses(witnesses).build())
    }
}

impl ScriptSigner for Secp256k1SighashSigner {
    fn match_args(&self, args: &[u8]) -> bool {
        args.len() == 20 && self.wallet.match_id(args)
    }

    fn sign_tx(
        &self,
        tx: &TransactionView,
        script_group: &ScriptGroup,
        tx_dep_provider: &dyn TransactionDependencyProvider,
    ) -> Result<TransactionView, SignError> {
        let args = script_group.script.args().raw_data();
        self.sign_tx_with_owner_id(args.as_ref(), tx, script_group, tx_dep_provider)
    }
}

pub struct MultisigConfig {
    sighash_addresses: Vec<H160>,
    require_first_n: u8,
    threshold: u8,
}
impl MultisigConfig {
    pub fn new_with(
        sighash_addresses: Vec<H160>,
        require_first_n: u8,
        threshold: u8,
    ) -> Result<MultisigConfig, SignError> {
        let mut addr_set: HashSet<&H160> = HashSet::default();
        for addr in &sighash_addresses {
            if !addr_set.insert(addr) {
                return Err(SignError::InvalidMultisigConfig(format!(
                    "Duplicated address: {:?}",
                    addr
                )));
            }
        }
        if threshold as usize > sighash_addresses.len() {
            return Err(SignError::InvalidMultisigConfig(format!(
                "Invalid threshold {} > {}",
                threshold,
                sighash_addresses.len()
            )));
        }
        if require_first_n > threshold {
            return Err(SignError::InvalidMultisigConfig(format!(
                "Invalid require-first-n {} > {}",
                require_first_n, threshold
            )));
        }
        Ok(MultisigConfig {
            sighash_addresses,
            require_first_n,
            threshold,
        })
    }

    pub fn to_witness_data(&self) -> Vec<u8> {
        let reserved_byte = 0u8;
        let mut witness_data = vec![
            reserved_byte,
            self.require_first_n,
            self.threshold,
            self.sighash_addresses.len() as u8,
        ];
        for sighash_address in &self.sighash_addresses {
            witness_data.extend_from_slice(sighash_address.as_bytes());
        }
        witness_data
    }
}
/// Signer for secp256k1 multisig all lock script
pub struct Secp256k1MultisigSigner {
    // Can be: SecpCkbRawKeyWallet, HardwareWallet
    wallet: Box<dyn Wallet>,
    config: MultisigConfig,
    config_hash: [u8; 32],
}
impl Secp256k1MultisigSigner {
    pub fn new(wallet: Box<dyn Wallet>, config: MultisigConfig) -> Secp256k1MultisigSigner {
        let config_hash = blake2b_256(config.to_witness_data());
        Secp256k1MultisigSigner {
            wallet,
            config,
            config_hash,
        }
    }
    pub fn wallet(&self) -> &dyn Wallet {
        self.wallet.as_ref()
    }
}

impl ScriptSigner for Secp256k1MultisigSigner {
    fn match_args(&self, args: &[u8]) -> bool {
        &self.config_hash[0..20] == args
            && self
                .config
                .sighash_addresses
                .iter()
                .any(|id| self.wallet.match_id(id.as_bytes()))
    }

    fn sign_tx(
        &self,
        tx: &TransactionView,
        script_group: &ScriptGroup,
        tx_dep_provider: &dyn TransactionDependencyProvider,
    ) -> Result<TransactionView, SignError> {
        let witness_idx = script_group.input_indices[0];
        let mut witnesses: Vec<packed::Bytes> = tx.witnesses().into_iter().collect();
        while witnesses.len() <= witness_idx {
            witnesses.push(Default::default());
        }
        let tx_new = tx
            .as_advanced_builder()
            .set_witnesses(witnesses.clone())
            .build();

        let config_data = self.config.to_witness_data();
        let mut zero_lock = vec![0u8; config_data.len() + 65 * (self.config.threshold as usize)];
        zero_lock[0..config_data.len()].copy_from_slice(&config_data);
        let message =
            self.generate_message(&tx_new, script_group, Bytes::from(zero_lock.clone()))?;

        let signatures = self
            .config
            .sighash_addresses
            .iter()
            .filter(|id| self.wallet.match_id(id.as_bytes()))
            .map(|id| {
                self.wallet
                    .sign(id.as_bytes(), message.as_ref(), tx, tx_dep_provider)
            })
            .collect::<Result<Vec<_>, WalletError>>()?;
        // Put signature into witness
        let witness_idx = script_group.input_indices[0];
        let witness_data = witnesses[witness_idx].raw_data();
        let mut current_witness: WitnessArgs = if witness_data.is_empty() {
            WitnessArgs::default()
        } else {
            WitnessArgs::from_slice(witness_data.as_ref())?
        };
        let mut lock_field = current_witness
            .lock()
            .to_opt()
            .map(|data| data.raw_data().as_ref().to_vec())
            .unwrap_or(zero_lock);
        for signature in signatures {
            let mut idx = config_data.len();
            while idx < lock_field.len() {
                // Put signature into an empty place.
                if lock_field[idx..idx + 65] == signature {
                    break;
                } else if lock_field[idx..idx + 65] == [0u8; 65] {
                    lock_field[idx..idx + 65].copy_from_slice(signature.as_ref());
                    break;
                }
                idx += 65;
            }
            if idx >= lock_field.len() {
                return Err(SignError::TooManySignatures);
            }
        }

        current_witness = current_witness
            .as_builder()
            .lock(Some(Bytes::from(lock_field)).pack())
            .build();
        witnesses[witness_idx] = current_witness.as_bytes().pack();
        Ok(tx.as_advanced_builder().set_witnesses(witnesses).build())
    }
}

pub struct AnyoneCanPaySigner {
    sighash_signer: Secp256k1SighashSigner,
}

impl ScriptSigner for AnyoneCanPaySigner {
    fn match_args(&self, args: &[u8]) -> bool {
        let id = &args[0..20];
        args.len() >= 20 && args.len() <= 22 && self.sighash_signer.wallet().match_id(id)
    }

    fn sign_tx(
        &self,
        tx: &TransactionView,
        script_group: &ScriptGroup,
        tx_dep_provider: &dyn TransactionDependencyProvider,
    ) -> Result<TransactionView, SignError> {
        let args = script_group.script.args().raw_data();
        let id = &args[0..20];
        self.sighash_signer
            .sign_tx_with_owner_id(id, tx, script_group, tx_dep_provider)
    }
}

#[derive(Clone, Eq, PartialEq, Debug)]
pub enum ChequeAction {
    Claim,
    Withdraw,
}
pub struct ChequeSigner {
    sighash_signer: Secp256k1SighashSigner,
    action: ChequeAction,
}
impl ChequeSigner {
    pub fn new(sighash_signer: Secp256k1SighashSigner, action: ChequeAction) -> ChequeSigner {
        ChequeSigner {
            sighash_signer,
            action,
        }
    }
    pub fn owner_id<'t>(&self, args: &'t [u8]) -> &'t [u8] {
        if args.len() != 40 {
            &args[0..0]
        } else if self.action == ChequeAction::Claim {
            &args[0..20]
        } else {
            &args[20..40]
        }
    }
}

impl ScriptSigner for ChequeSigner {
    fn match_args(&self, args: &[u8]) -> bool {
        // NOTE: Require wallet raw key map as: {script_hash[0..20] -> private key}
        args.len() == 40 && self.sighash_signer.wallet().match_id(self.owner_id(args))
    }

    fn sign_tx(
        &self,
        tx: &TransactionView,
        script_group: &ScriptGroup,
        tx_dep_provider: &dyn TransactionDependencyProvider,
    ) -> Result<TransactionView, SignError> {
        let args = script_group.script.args().raw_data();
        let id = self.owner_id(args.as_ref());
        self.sighash_signer
            .sign_tx_with_owner_id(id, tx, script_group, tx_dep_provider)
    }
}
