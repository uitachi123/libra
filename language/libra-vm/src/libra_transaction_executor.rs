// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    counters::*,
    data_cache::StateViewCache,
    libra_vm::{
        get_transaction_output, txn_effects_to_writeset_and_events_cached, LibraVMImpl,
        LibraVMInternals,
    },
    system_module_names::*,
    transaction_metadata::TransactionMetadata,
    VMExecutor,
};
use debug_interface::prelude::*;
use libra_crypto::HashValue;
use libra_logger::prelude::*;
use libra_state_view::StateView;
use libra_types::{
    account_config,
    block_metadata::BlockMetadata,
    transaction::{
        ChangeSet, Module, Script, SignatureCheckedTransaction, SignedTransaction, Transaction,
        TransactionArgument, TransactionOutput, TransactionPayload, TransactionStatus,
    },
    vm_status::{StatusCode, VMStatus},
    write_set::{WriteSet, WriteSetMut},
};
use move_core_types::{
    gas_schedule::{CostTable, GasAlgebra, GasCarrier, GasUnits},
    identifier::IdentStr,
};
use move_vm_runtime::{data_cache::RemoteCache, session::Session};

use move_vm_types::{
    gas_schedule::{zero_cost_schedule, CostStrategy},
    values::Value,
};
use rayon::prelude::*;
use std::{
    collections::HashSet,
    convert::{AsMut, AsRef, TryFrom},
};

pub struct LibraVM(LibraVMImpl);

impl LibraVM {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(LibraVMImpl::new())
    }

    pub fn load_configs<S: StateView>(&mut self, state: &S) {
        self.0.load_configs(state)
    }

    pub fn internals(&self) -> LibraVMInternals {
        LibraVMInternals::new(&self.0)
    }

    /// Generates a transaction output for a transaction that encountered errors during the
    /// execution process. This is public for now only for tests.
    pub fn failed_transaction_cleanup(
        &self,
        error_code: VMStatus,
        gas_schedule: &CostTable,
        gas_left: GasUnits<GasCarrier>,
        txn_data: &TransactionMetadata,
        remote_cache: &StateViewCache<'_>,
        account_currency_symbol: &IdentStr,
    ) -> TransactionOutput {
        let mut cost_strategy = CostStrategy::system(gas_schedule, gas_left);
        let mut session = self.0.new_session(remote_cache);
        match TransactionStatus::from(error_code) {
            TransactionStatus::Keep(status) => {
                if let Err(e) = self.0.run_failure_epilogue(
                    &mut session,
                    &mut cost_strategy,
                    txn_data,
                    account_currency_symbol,
                ) {
                    return discard_error_output(e);
                }
                get_transaction_output(&mut (), session, &cost_strategy, txn_data, status)
                    .unwrap_or_else(discard_error_output)
            }
            TransactionStatus::Discard(status) => discard_error_output(status),
            TransactionStatus::Retry => unreachable!(),
        }
    }

    fn success_transaction_cleanup<R: RemoteCache>(
        &self,
        mut session: Session<R>,
        gas_schedule: &CostTable,
        gas_left: GasUnits<GasCarrier>,
        txn_data: &TransactionMetadata,
        account_currency_symbol: &IdentStr,
    ) -> Result<TransactionOutput, VMStatus> {
        let mut cost_strategy = CostStrategy::system(gas_schedule, gas_left);
        self.0.run_success_epilogue(
            &mut session,
            &mut cost_strategy,
            txn_data,
            account_currency_symbol,
        )?;

        Ok(get_transaction_output(
            &mut (),
            session,
            &cost_strategy,
            txn_data,
            VMStatus::executed(),
        )?)
    }

    fn execute_script(
        &self,
        remote_cache: &StateViewCache<'_>,
        cost_strategy: &mut CostStrategy,
        txn_data: &TransactionMetadata,
        script: &Script,
        account_currency_symbol: &IdentStr,
    ) -> Result<TransactionOutput, VMStatus> {
        let gas_schedule = self.0.get_gas_schedule()?;
        let mut session = self.0.new_session(remote_cache);
        // TODO: The logic for handling falied transaction fee is pretty ugly right now. Fix it later.

        // Run the validation logic
        {
            cost_strategy.disable_metering();
            let _timer = TXN_VERIFICATION_SECONDS.start_timer();
            self.0.check_gas(txn_data)?;
            self.0.is_allowed_script(script)?;
            self.0.run_prologue(
                &mut session,
                cost_strategy,
                &txn_data,
                account_currency_symbol,
            )?;
        }

        // Run the execution logic
        {
            let _timer = TXN_EXECUTION_SECONDS.start_timer();
            cost_strategy.enable_metering();
            cost_strategy
                .charge_intrinsic_gas(txn_data.transaction_size())
                .map_err(|e| e.into_vm_status())?;
            session
                .execute_script(
                    script.code().to_vec(),
                    script.ty_args().to_vec(),
                    convert_txn_args(script.args()),
                    txn_data.sender(),
                    cost_strategy,
                )
                .map_err(|e| e.into_vm_status())?;

            let gas_usage = txn_data
                .max_gas_amount()
                .sub(cost_strategy.remaining_gas())
                .get();
            TXN_EXECUTION_GAS_USAGE.observe(gas_usage as f64);

            cost_strategy.disable_metering();
            self.success_transaction_cleanup(
                session,
                gas_schedule,
                cost_strategy.remaining_gas(),
                txn_data,
                account_currency_symbol,
            )
        }
    }

    fn execute_module(
        &self,
        remote_cache: &StateViewCache<'_>,
        cost_strategy: &mut CostStrategy,
        txn_data: &TransactionMetadata,
        module: &Module,
        account_currency_symbol: &IdentStr,
    ) -> Result<TransactionOutput, VMStatus> {
        let gas_schedule = self.0.get_gas_schedule()?;
        let mut session = self.0.new_session(remote_cache);

        // Run validation logic
        cost_strategy.disable_metering();
        self.0.check_gas(txn_data)?;
        self.0.is_allowed_module(txn_data, remote_cache)?;
        self.0.run_prologue(
            &mut session,
            cost_strategy,
            txn_data,
            account_currency_symbol,
        )?;

        // Publish the module
        let module_address = if self.0.on_chain_config()?.publishing_option.is_open() {
            txn_data.sender()
        } else {
            account_config::CORE_CODE_ADDRESS
        };

        cost_strategy.enable_metering();
        cost_strategy
            .charge_intrinsic_gas(txn_data.transaction_size())
            .map_err(|e| e.into_vm_status())?;
        session
            .publish_module(module.code().to_vec(), module_address, cost_strategy)
            .map_err(|e| e.into_vm_status())?;

        self.success_transaction_cleanup(
            session,
            gas_schedule,
            cost_strategy.remaining_gas(),
            txn_data,
            account_currency_symbol,
        )
    }

    fn execute_user_transaction(
        &mut self,
        _state_view: &dyn StateView,
        remote_cache: &StateViewCache<'_>,
        txn: &SignatureCheckedTransaction,
    ) -> TransactionOutput {
        macro_rules! unwrap_or_discard {
            ($res: expr) => {
                match $res {
                    Ok(s) => s,
                    Err(e) => return discard_error_output(e),
                }
            };
        }

        let gas_schedule = unwrap_or_discard!(self.0.get_gas_schedule());
        let txn_data = TransactionMetadata::new(txn);
        let mut cost_strategy = CostStrategy::system(gas_schedule, txn_data.max_gas_amount());
        let account_currency_symbol = unwrap_or_discard!(
            account_config::from_currency_code_string(txn.gas_currency_code())
                .map_err(|_| VMStatus::new(StatusCode::INVALID_GAS_SPECIFIER, None, None))
        );
        let result = match txn.payload() {
            TransactionPayload::Script(s) => self.execute_script(
                remote_cache,
                &mut cost_strategy,
                &txn_data,
                s,
                account_currency_symbol.as_ident_str(),
            ),
            TransactionPayload::Module(m) => self.execute_module(
                remote_cache,
                &mut cost_strategy,
                &txn_data,
                m,
                account_currency_symbol.as_ident_str(),
            ),
            TransactionPayload::WriteSet(_) => {
                return discard_error_output(VMStatus::new(StatusCode::UNREACHABLE, None, None))
            }
        };

        match result {
            Ok(output) => output,
            Err(err) => {
                let txn_status = TransactionStatus::from(err.clone());
                if txn_status.is_discarded() {
                    discard_error_output(err)
                } else {
                    self.failed_transaction_cleanup(
                        err,
                        gas_schedule,
                        cost_strategy.remaining_gas(),
                        &txn_data,
                        remote_cache,
                        account_currency_symbol.as_ident_str(),
                    )
                }
            }
        }
    }

    fn read_writeset(
        &self,
        remote_cache: &StateViewCache<'_>,
        write_set: &WriteSet,
    ) -> Result<(), VMStatus> {
        // All Move executions satisfy the read-before-write property. Thus we need to read each
        // access path that the write set is going to update.
        for (ap, _) in write_set.iter() {
            remote_cache
                .get(ap)
                .map_err(|_| VMStatus::new(StatusCode::STORAGE_ERROR, None, None))?;
        }
        Ok(())
    }

    fn process_waypoint_change_set(
        &mut self,
        remote_cache: &mut StateViewCache<'_>,
        change_set: ChangeSet,
    ) -> Result<TransactionOutput, VMStatus> {
        let (write_set, events) = change_set.into_inner();
        self.read_writeset(remote_cache, &write_set)?;
        remote_cache.push_write_set(&write_set);
        self.0.load_configs_impl(remote_cache);
        Ok(TransactionOutput::new(
            write_set,
            events,
            0,
            VMStatus::new(StatusCode::EXECUTED, None, None).into(),
        ))
    }

    fn process_block_prologue(
        &mut self,
        remote_cache: &mut StateViewCache<'_>,
        block_metadata: BlockMetadata,
    ) -> Result<TransactionOutput, VMStatus> {
        // TODO: How should we setup the metadata here? A couple of thoughts here:
        // 1. We might make the txn_data to be poisoned so that reading anything will result in a panic.
        // 2. The most important consideration is figuring out the sender address.  Having a notion of a
        //    "null address" (probably 0x0...0) that is prohibited from containing modules or resources
        //    might be useful here.
        // 3. We set the max gas to a big number just to get rid of the potential out of gas error.
        let mut txn_data = TransactionMetadata::default();
        txn_data.sender = account_config::reserved_vm_address();
        txn_data.max_gas_amount = GasUnits::new(std::u64::MAX);

        let gas_schedule = zero_cost_schedule();
        let mut cost_strategy = CostStrategy::transaction(&gas_schedule, txn_data.max_gas_amount());
        cost_strategy
            .charge_intrinsic_gas(txn_data.transaction_size())
            .map_err(|e| e.into_vm_status())?;
        let mut session = self.0.new_session(remote_cache);

        if let Ok((round, timestamp, previous_vote, proposer)) = block_metadata.into_inner() {
            let args = vec![
                Value::transaction_argument_signer_reference(txn_data.sender),
                Value::u64(round),
                Value::u64(timestamp),
                Value::vector_address(previous_vote),
                Value::address(proposer),
            ];
            session
                .execute_function(
                    &LIBRA_BLOCK_MODULE,
                    &BLOCK_PROLOGUE,
                    vec![],
                    args,
                    txn_data.sender,
                    &mut cost_strategy,
                )
                .map_err(|e| e.into_vm_status())?
        } else {
            return Err(VMStatus::new(StatusCode::MALFORMED, None, None));
        };

        get_transaction_output(
            &mut (),
            session,
            &cost_strategy,
            &txn_data,
            VMStatus::executed(),
        )
        .map(|output| {
            remote_cache.push_write_set(output.write_set());
            output
        })
    }

    fn process_writeset_transaction(
        &mut self,
        remote_cache: &mut StateViewCache<'_>,
        txn: SignedTransaction,
    ) -> Result<TransactionOutput, VMStatus> {
        let txn = match txn.check_signature() {
            Ok(t) => t,
            _ => {
                return Ok(discard_error_output(VMStatus::new(
                    StatusCode::INVALID_SIGNATURE,
                    None,
                    None,
                )))
            }
        };

        let change_set = if let TransactionPayload::WriteSet(change_set) = txn.payload() {
            change_set
        } else {
            error!("[libra_vm] UNREACHABLE");
            return Ok(discard_error_output(VMStatus::new(
                StatusCode::UNREACHABLE,
                None,
                None,
            )));
        };

        let txn_data = TransactionMetadata::new(&txn);

        let mut session = self.0.new_session(remote_cache);

        if let Err(e) = self.0.run_writeset_prologue(&mut session, &txn_data) {
            return Ok(discard_error_output(e));
        };

        // Bump the sequence number of sender.
        let gas_schedule = zero_cost_schedule();
        let mut cost_strategy = CostStrategy::system(&gas_schedule, GasUnits::new(0));

        session
            .execute_function(
                &account_config::ACCOUNT_MODULE,
                &BUMP_SEQUENCE_NUMBER_NAME,
                vec![],
                vec![Value::transaction_argument_signer_reference(
                    txn_data.sender,
                )],
                txn_data.sender,
                &mut cost_strategy,
            )
            .map_err(|e| e.into_vm_status())?;

        // Emit the reconfiguration event
        self.0
            .run_writeset_epilogue(&mut session, change_set, &txn_data)?;

        if let Err(e) = self.read_writeset(remote_cache, &change_set.write_set()) {
            return Ok(discard_error_output(e));
        };

        let effects = session.finish().map_err(|e| e.into_vm_status())?;
        let (epilogue_writeset, epilogue_events) =
            txn_effects_to_writeset_and_events_cached(&mut (), effects)?;

        // Make sure epilogue WriteSet doesn't intersect with the writeset in TransactionPayload.
        if !epilogue_writeset
            .iter()
            .map(|(ap, _)| ap)
            .collect::<HashSet<_>>()
            .is_disjoint(
                &change_set
                    .write_set()
                    .iter()
                    .map(|(ap, _)| ap)
                    .collect::<HashSet<_>>(),
            )
        {
            return Ok(discard_error_output(VMStatus::new(
                StatusCode::INVALID_WRITE_SET,
                None,
                None,
            )));
        }
        if !epilogue_events
            .iter()
            .map(|event| event.key())
            .collect::<HashSet<_>>()
            .is_disjoint(
                &change_set
                    .events()
                    .iter()
                    .map(|event| event.key())
                    .collect::<HashSet<_>>(),
            )
        {
            return Ok(discard_error_output(VMStatus::new(
                StatusCode::INVALID_WRITE_SET,
                None,
                None,
            )));
        }

        let write_set = WriteSetMut::new(
            epilogue_writeset
                .iter()
                .chain(change_set.write_set().iter())
                .cloned()
                .collect(),
        )
        .freeze()
        .map_err(|_| VMStatus::new(StatusCode::INVALID_WRITE_SET, None, None))?;
        let events = change_set
            .events()
            .iter()
            .chain(epilogue_events.iter())
            .cloned()
            .collect();

        Ok(TransactionOutput::new(
            write_set,
            events,
            0,
            TransactionStatus::Keep(VMStatus::executed()),
        ))
    }

    fn execute_block_impl(
        &mut self,
        transactions: Vec<Transaction>,
        state_view: &dyn StateView,
    ) -> Result<Vec<TransactionOutput>, VMStatus> {
        let count = transactions.len();
        let mut result = vec![];
        let blocks = chunk_block_transactions(transactions);
        let mut data_cache = StateViewCache::new(state_view);
        let mut execute_block_trace_guard = vec![];
        let mut current_block_id = HashValue::zero();
        for block in blocks {
            match block {
                TransactionBlock::UserTransaction(txns) => {
                    let mut outs = self.execute_user_transactions(
                        current_block_id,
                        txns,
                        &mut data_cache,
                        state_view,
                    )?;
                    result.append(&mut outs);
                }
                TransactionBlock::BlockPrologue(block_metadata) => {
                    execute_block_trace_guard.clear();
                    current_block_id = block_metadata.id();
                    trace_code_block!("libra_vm::execute_block_impl", {"block", current_block_id}, execute_block_trace_guard);
                    result.push(self.process_block_prologue(&mut data_cache, block_metadata)?)
                }
                TransactionBlock::WaypointWriteSet(change_set) => result.push(
                    self.process_waypoint_change_set(&mut data_cache, change_set)
                        .unwrap_or_else(discard_error_output),
                ),
                TransactionBlock::WriteSet(txn) => {
                    result.push(self.process_writeset_transaction(&mut data_cache, *txn)?)
                }
            }
        }

        // Record the histogram count for transactions per block.
        match i64::try_from(count) {
            Ok(val) => BLOCK_TRANSACTION_COUNT.set(val),
            Err(_) => BLOCK_TRANSACTION_COUNT.set(std::i64::MAX),
        }

        Ok(result)
    }

    fn execute_user_transactions(
        &mut self,
        block_id: HashValue,
        txn_block: Vec<SignedTransaction>,
        data_cache: &mut StateViewCache<'_>,
        state_view: &dyn StateView,
    ) -> Result<Vec<TransactionOutput>, VMStatus> {
        self.0.load_configs_impl(data_cache);
        let signature_verified_block: Vec<Result<SignatureCheckedTransaction, VMStatus>>;
        {
            trace_code_block!("libra_vm::verify_signatures", {"block", block_id});
            signature_verified_block = txn_block
                .into_par_iter()
                .map(|txn| {
                    txn.check_signature()
                        .map_err(|_| VMStatus::new(StatusCode::INVALID_SIGNATURE, None, None))
                })
                .collect();
        }
        let mut result = vec![];
        trace_code_block!("libra_vm::execute_transactions", {"block", block_id});
        for transaction in signature_verified_block {
            let output = match transaction {
                Ok(txn) => {
                    let _timer = TXN_TOTAL_SECONDS.start_timer();
                    self.execute_user_transaction(state_view, data_cache, &txn)
                }
                Err(e) => discard_error_output(e),
            };

            if !output.status().is_discarded() {
                data_cache.push_write_set(output.write_set());
            }

            // Increment the counter for transactions executed.
            let counter_label = match output.status() {
                TransactionStatus::Keep(_) => Some("success"),
                TransactionStatus::Discard(_) => Some("discarded"),
                TransactionStatus::Retry => None,
            };
            if let Some(label) = counter_label {
                TRANSACTIONS_EXECUTED.with_label_values(&[label]).inc();
            }

            // `result` is initially empty, a single element is pushed per loop iteration and
            // the number of iterations is bound to the max size of `signature_verified_block`
            assume!(result.len() < usize::max_value());
            result.push(output);
        }
        Ok(result)
    }
}

/// Transactions divided by transaction flow.
/// Transaction flows are different across different types of transactions.
pub enum TransactionBlock {
    UserTransaction(Vec<SignedTransaction>),
    WaypointWriteSet(ChangeSet),
    BlockPrologue(BlockMetadata),
    WriteSet(Box<SignedTransaction>),
}

pub fn chunk_block_transactions(txns: Vec<Transaction>) -> Vec<TransactionBlock> {
    let mut blocks = vec![];
    let mut buf = vec![];
    for txn in txns {
        match txn {
            Transaction::BlockMetadata(data) => {
                if !buf.is_empty() {
                    blocks.push(TransactionBlock::UserTransaction(buf));
                    buf = vec![];
                }
                blocks.push(TransactionBlock::BlockPrologue(data));
            }
            Transaction::WaypointWriteSet(cs) => {
                if !buf.is_empty() {
                    blocks.push(TransactionBlock::UserTransaction(buf));
                    buf = vec![];
                }
                blocks.push(TransactionBlock::WaypointWriteSet(cs));
            }
            Transaction::UserTransaction(txn) => {
                if let TransactionPayload::WriteSet(_) = txn.payload() {
                    if !buf.is_empty() {
                        blocks.push(TransactionBlock::UserTransaction(buf));
                        buf = vec![];
                    }
                    blocks.push(TransactionBlock::WriteSet(Box::new(txn)));
                } else {
                    buf.push(txn);
                }
            }
        }
    }
    if !buf.is_empty() {
        blocks.push(TransactionBlock::UserTransaction(buf));
    }
    blocks
}

// Executor external API
impl VMExecutor for LibraVM {
    /// Execute a block of `transactions`. The output vector will have the exact same length as the
    /// input vector. The discarded transactions will be marked as `TransactionStatus::Discard` and
    /// have an empty `WriteSet`. Also `state_view` is immutable, and does not have interior
    /// mutability. Writes to be applied to the data view are encoded in the write set part of a
    /// transaction output.
    fn execute_block(
        transactions: Vec<Transaction>,
        state_view: &dyn StateView,
    ) -> Result<Vec<TransactionOutput>, VMStatus> {
        let mut vm = LibraVM::new();
        vm.execute_block_impl(transactions, state_view)
    }
}

pub(crate) fn discard_error_output(err: VMStatus) -> TransactionOutput {
    // Since this transaction will be discarded, no writeset will be included.
    TransactionOutput::new(
        WriteSet::default(),
        vec![],
        0,
        TransactionStatus::Discard(err),
    )
}

/// Convert the transaction arguments into move values.
fn convert_txn_args(args: &[TransactionArgument]) -> Vec<Value> {
    args.iter()
        .map(|arg| match arg {
            TransactionArgument::U8(i) => Value::u8(*i),
            TransactionArgument::U64(i) => Value::u64(*i),
            TransactionArgument::U128(i) => Value::u128(*i),
            TransactionArgument::Address(a) => Value::address(*a),
            TransactionArgument::Bool(b) => Value::bool(*b),
            TransactionArgument::U8Vector(v) => Value::vector_u8(v.clone()),
        })
        .collect()
}

impl AsRef<LibraVMImpl> for LibraVM {
    fn as_ref(&self) -> &LibraVMImpl {
        &self.0
    }
}

impl AsMut<LibraVMImpl> for LibraVM {
    fn as_mut(&mut self) -> &mut LibraVMImpl {
        &mut self.0
    }
}
