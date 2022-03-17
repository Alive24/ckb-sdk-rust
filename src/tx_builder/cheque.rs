use std::collections::HashSet;

use ckb_types::{
    bytes::Bytes,
    core::{TransactionBuilder, TransactionView},
    packed::{CellInput, CellOutput, OutPoint, Script},
    prelude::*,
};

use super::{TxBuilder, TxBuilderError};
use crate::constants::CHEQUE_CELL_SINCE;
use crate::traits::{
    CellCollector, CellDepResolver, HeaderDepResolver, TransactionDependencyProvider,
};
use crate::types::ScriptId;

pub struct ChequeClaimBuilder {
    /// The cheque cells to claim, all cells must have same lock script and same
    /// type script and cell data length is equals to 16.
    pub inputs: Vec<CellInput>,

    /// Add all SUDT amount to this cell, the type script must be the same with
    /// `inputs`. The receiver output will keep the lock script, capacity.
    pub receiver_input: CellInput,

    /// Sender's lock script, the script hash must match the cheque cell's lock script args.
    pub sender_lock_script: Script,
}

impl TxBuilder for ChequeClaimBuilder {
    fn build_base(
        &self,
        _cell_collector: &mut dyn CellCollector,
        cell_dep_resolver: &dyn CellDepResolver,
        _header_dep_resolver: &dyn HeaderDepResolver,
        tx_dep_provider: &dyn TransactionDependencyProvider,
    ) -> Result<TransactionView, TxBuilderError> {
        if self.inputs.is_empty() {
            return Err(TxBuilderError::InvalidParameter(
                "empty cheque inputs".to_string().into(),
            ));
        }

        #[allow(clippy::mutable_key_type)]
        let mut cell_deps = HashSet::new();
        let mut inputs = self.inputs.clone();
        inputs.push(self.receiver_input.clone());

        let receiver_input_cell =
            tx_dep_provider.get_cell(&self.receiver_input.previous_output())?;
        let receiver_input_data =
            tx_dep_provider.get_cell_data(&self.receiver_input.previous_output())?;
        let receiver_type_script = receiver_input_cell.type_().to_opt().ok_or_else(|| {
            TxBuilderError::InvalidParameter(
                "receiver input missing type script".to_string().into(),
            )
        })?;

        if receiver_input_data.len() != 16 {
            return Err(TxBuilderError::InvalidParameter(
                format!(
                    "invalid receiver input cell data length, expected: 16, got: {}",
                    receiver_input_data.len()
                )
                .into(),
            ));
        }
        let receiver_input_amount = {
            let mut amount_bytes = [0u8; 16];
            amount_bytes.copy_from_slice(receiver_input_data.as_ref());
            u128::from_le_bytes(amount_bytes)
        };

        let receiver_type_script_id = ScriptId::from(&receiver_type_script);
        let receiver_type_cell_dep = cell_dep_resolver.resolve(&receiver_type_script_id).ok_or(
            TxBuilderError::ResolveCellDepFailed(receiver_type_script_id),
        )?;
        let receiver_lock_script_id = ScriptId::from(&receiver_input_cell.lock());
        let receiver_lock_cell_dep = cell_dep_resolver.resolve(&receiver_lock_script_id).ok_or(
            TxBuilderError::ResolveCellDepFailed(receiver_lock_script_id),
        )?;
        cell_deps.insert(receiver_type_cell_dep);
        cell_deps.insert(receiver_lock_cell_dep);

        let mut cheque_total_amount = 0;
        let mut cheque_total_capacity = 0;
        let mut last_lock_script = None;
        for input in &self.inputs {
            let out_point = input.previous_output();
            let input_cell = tx_dep_provider.get_cell(&out_point)?;
            let input_data = tx_dep_provider.get_cell_data(&out_point)?;
            let type_script = receiver_input_cell.type_().to_opt().ok_or_else(|| {
                TxBuilderError::InvalidParameter(
                    format!("cheque input missing type script: {}", input).into(),
                )
            })?;

            if input_data.len() != 16 {
                return Err(TxBuilderError::InvalidParameter(
                    format!(
                        "invalid cheque input cell data length, expected: 16, got: {}",
                        input_data.len()
                    )
                    .into(),
                ));
            }
            if type_script != receiver_type_script {
                return Err(TxBuilderError::InvalidParameter(
                    format!(
                        "cheque input's type script not same with receiver input's type script: {}",
                        input
                    )
                    .into(),
                ));
            }
            let input_amount = {
                let mut amount_bytes = [0u8; 16];
                amount_bytes.copy_from_slice(input_data.as_ref());
                u128::from_le_bytes(amount_bytes)
            };
            let input_capacity: u64 = input_cell.capacity().unpack();

            let lock_script = input_cell.lock();
            if last_lock_script.is_none() {
                last_lock_script = Some(lock_script.clone());
            } else if last_lock_script.as_ref() != Some(&lock_script) {
                return Err(TxBuilderError::InvalidParameter(
                    "all cheque input lock script must be the same"
                        .to_string()
                        .into(),
                ));
            }
            let lock_script_id = ScriptId::from(&lock_script);
            let lock_cell_dep = cell_dep_resolver
                .resolve(&lock_script_id)
                .ok_or(TxBuilderError::ResolveCellDepFailed(lock_script_id))?;

            cell_deps.insert(lock_cell_dep);
            cheque_total_amount += input_amount;
            cheque_total_capacity += input_capacity;
        }

        let cheque_lock_script = last_lock_script.unwrap();
        let cheque_lock_args = cheque_lock_script.args().raw_data();
        if cheque_lock_args.len() != 40 {
            return Err(TxBuilderError::InvalidParameter(
                format!(
                    "invalid cheque lock args length, expected: 40, got: {}",
                    cheque_lock_args.len()
                )
                .into(),
            ));
        }
        let sender_lock_hash = self.sender_lock_script.calc_script_hash();
        if sender_lock_hash.as_slice()[0..20] != cheque_lock_args.as_ref()[20..40] {
            return Err(TxBuilderError::InvalidParameter(
                "sender lock script is match with cheque lock script args"
                    .to_string()
                    .into(),
            ));
        }

        let receiver_output = receiver_input_cell;
        let receiver_output_data = {
            let receiver_output_amount = receiver_input_amount + cheque_total_amount;
            Bytes::from(receiver_output_amount.to_le_bytes().to_vec())
        };
        let sender_output = CellOutput::new_builder()
            .lock(self.sender_lock_script.clone())
            .capacity(cheque_total_capacity.pack())
            .build();
        let sender_output_data = Bytes::new();

        let outputs = vec![receiver_output, sender_output];
        let outputs_data = vec![receiver_output_data.pack(), sender_output_data.pack()];

        Ok(TransactionBuilder::default()
            .set_cell_deps(cell_deps.into_iter().collect())
            .set_inputs(inputs)
            .set_outputs(outputs)
            .set_outputs_data(outputs_data)
            .build())
    }
}

pub struct ChequeWithdrawBuilder {
    /// The cheque cells to withdraw, all cells must have same lock script and same
    /// type script and cell data length is equals to 16.
    pub out_points: Vec<OutPoint>,

    /// Sender's lock script, the script hash must match the cheque cell's lock script args.
    pub sender_lock_script: Script,
}

impl TxBuilder for ChequeWithdrawBuilder {
    fn build_base(
        &self,
        _cell_collector: &mut dyn CellCollector,
        cell_dep_resolver: &dyn CellDepResolver,
        _header_dep_resolver: &dyn HeaderDepResolver,
        tx_dep_provider: &dyn TransactionDependencyProvider,
    ) -> Result<TransactionView, TxBuilderError> {
        if self.out_points.is_empty() {
            return Err(TxBuilderError::InvalidParameter(
                "empty withdraw inputs".to_string().into(),
            ));
        }

        let mut inputs = Vec::new();
        let mut last_lock_script = None;
        let mut last_type_script = None;
        let mut cheque_total_amount: u128 = 0;
        let mut cheque_total_capacity: u64 = 0;
        for out_point in &self.out_points {
            let input_cell = tx_dep_provider.get_cell(out_point)?;
            let input_data = tx_dep_provider.get_cell_data(out_point)?;
            let lock_script = input_cell.lock();
            let type_script = input_cell.type_().to_opt().ok_or_else(|| {
                TxBuilderError::InvalidParameter(
                    format!("cheque input missing type script: {}", out_point).into(),
                )
            })?;

            if last_lock_script.is_none() {
                last_lock_script = Some(lock_script.clone());
            } else if last_lock_script.as_ref() != Some(&lock_script) {
                return Err(TxBuilderError::InvalidParameter(
                    "all cheque input lock script must be the same"
                        .to_string()
                        .into(),
                ));
            }
            if last_type_script.is_none() {
                last_type_script = Some(type_script.clone());
            } else if last_type_script.as_ref() != Some(&type_script) {
                return Err(TxBuilderError::InvalidParameter(
                    "all cheque input type script must be the same"
                        .to_string()
                        .into(),
                ));
            }

            let input_amount = {
                let mut amount_bytes = [0u8; 16];
                amount_bytes.copy_from_slice(input_data.as_ref());
                u128::from_le_bytes(amount_bytes)
            };
            let input_capacity: u64 = input_cell.capacity().unpack();
            let input = CellInput::new(out_point.clone(), CHEQUE_CELL_SINCE);

            cheque_total_capacity += input_capacity;
            cheque_total_amount += input_amount;
            inputs.push(input);
        }

        let cheque_lock_script = last_lock_script.unwrap();
        let type_script = last_type_script.unwrap();

        let lock_script_id = ScriptId::from(&cheque_lock_script);
        let lock_cell_dep = cell_dep_resolver
            .resolve(&lock_script_id)
            .ok_or(TxBuilderError::ResolveCellDepFailed(lock_script_id))?;
        let type_script_id = ScriptId::from(&type_script);
        let type_cell_dep = cell_dep_resolver
            .resolve(&type_script_id)
            .ok_or(TxBuilderError::ResolveCellDepFailed(type_script_id))?;

        let cheque_lock_args = cheque_lock_script.args().raw_data();
        if cheque_lock_args.len() != 40 {
            return Err(TxBuilderError::InvalidParameter(
                format!(
                    "invalid cheque lock args length, expected: 40, got: {}",
                    cheque_lock_args.len()
                )
                .into(),
            ));
        }
        let sender_lock_hash = self.sender_lock_script.calc_script_hash();
        if sender_lock_hash.as_slice()[0..20] != cheque_lock_args.as_ref()[20..40] {
            return Err(TxBuilderError::InvalidParameter(
                "sender lock script is match with cheque lock script args"
                    .to_string()
                    .into(),
            ));
        }

        let sender_output = CellOutput::new_builder()
            .lock(self.sender_lock_script.clone())
            .type_(Some(type_script).pack())
            .capacity(cheque_total_capacity.pack())
            .build();
        let sender_output_data = Bytes::from(cheque_total_amount.to_le_bytes().to_vec());

        let cell_deps = vec![lock_cell_dep, type_cell_dep];
        let outputs = vec![sender_output];
        let outputs_data = vec![sender_output_data.pack()];

        Ok(TransactionBuilder::default()
            .set_cell_deps(cell_deps.into_iter().collect())
            .set_inputs(inputs)
            .set_outputs(outputs)
            .set_outputs_data(outputs_data)
            .build())
    }
}
