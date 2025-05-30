use crate::{
    ports::{
        MaybeCheckedTransaction,
        NewTxWaiterPort,
        PreconfirmationSenderPort,
        RelayerPort,
        TransactionsSource,
    },
    refs::ContractRef,
};
use fuel_core_storage::{
    StorageAsMut,
    StorageAsRef,
    column::Column,
    kv_store::KeyValueInspect,
    tables::{
        Coins,
        ConsensusParametersVersions,
        ContractsLatestUtxo,
        FuelBlocks,
        Messages,
        ProcessedTransactions,
    },
    transactional::{
        Changes,
        ConflictPolicy,
        IntoTransaction,
        Modifiable,
        ReadTransaction,
        StorageTransaction,
        WriteTransaction,
    },
    vm_storage::VmStorage,
};
use fuel_core_types::{
    blockchain::{
        block::{
            Block,
            PartialFuelBlock,
        },
        header::{
            ConsensusParametersVersion,
            PartialBlockHeader,
        },
        primitives::DaBlockHeight,
        transaction::TransactionExt,
    },
    entities::{
        RelayedTransaction,
        coins::coin::{
            CompressedCoin,
            CompressedCoinV1,
        },
        contract::ContractUtxoInfo,
    },
    fuel_asm::{
        PanicInstruction,
        Word,
        op,
    },
    fuel_merkle::binary::root_calculator::MerkleRootCalculator,
    fuel_tx::{
        Address,
        AssetId,
        Bytes32,
        Cacheable,
        Chargeable,
        ConsensusParameters,
        Input,
        Mint,
        Output,
        PanicReason,
        Receipt,
        Transaction,
        TxId,
        TxPointer,
        UniqueIdentifier,
        UtxoId,
        field::{
            InputContract,
            MaxFeeLimit,
            MintAmount,
            MintAssetId,
            MintGasPrice,
            OutputContract,
            TxPointer as TxPointerField,
        },
        input::{
            self,
            coin::{
                CoinPredicate,
                CoinSigned,
            },
            message::{
                MessageCoinPredicate,
                MessageCoinSigned,
                MessageDataPredicate,
                MessageDataSigned,
            },
        },
        output,
    },
    fuel_types::{
        BlockHeight,
        ContractId,
        MessageId,
        canonical::Deserialize,
    },
    fuel_vm::{
        self,
        Interpreter,
        ProgramState,
        checked_transaction::{
            CheckPredicateParams,
            CheckPredicates,
            Checked,
            CheckedTransaction,
            Checks,
            IntoChecked,
        },
        interpreter::{
            CheckedMetadata as CheckedMetadataTrait,
            ExecutableTransaction,
            InterpreterParams,
            MemoryInstance,
            NotSupportedEcal,
        },
        state::StateTransition,
        verification,
    },
    services::{
        block_producer::Components,
        executor::{
            Error as ExecutorError,
            Event as ExecutorEvent,
            ExecutionResult,
            ForcedTransactionFailure,
            Result as ExecutorResult,
            TransactionExecutionResult,
            TransactionExecutionStatus,
            TransactionValidityError,
            UncommittedResult,
            UncommittedValidationResult,
            ValidationResult,
        },
        preconfirmation::{
            Preconfirmation,
            PreconfirmationStatus,
        },
        relayer::Event,
    },
};
use parking_lot::Mutex as ParkingMutex;
use tracing::{
    debug,
    warn,
};

#[cfg(feature = "std")]
use std::borrow::Cow;

#[cfg(not(feature = "std"))]
use alloc::borrow::Cow;

#[cfg(feature = "alloc")]
use alloc::{
    format,
    string::ToString,
    vec,
    vec::Vec,
};

/// The maximum amount of transactions that can be included in a block,
/// excluding the mint transaction.
#[cfg(not(feature = "limited-tx-count"))]
pub const fn max_tx_count() -> u16 {
    u16::MAX.saturating_sub(1)
}
#[cfg(feature = "limited-tx-count")]
pub const fn max_tx_count() -> u16 {
    1024
}

pub struct OnceTransactionsSource {
    transactions: ParkingMutex<Vec<MaybeCheckedTransaction>>,
}

impl OnceTransactionsSource {
    pub fn new(transactions: Vec<Transaction>) -> Self {
        Self {
            transactions: ParkingMutex::new(
                transactions
                    .into_iter()
                    .map(MaybeCheckedTransaction::Transaction)
                    .collect(),
            ),
        }
    }

    pub fn new_maybe_checked(transactions: Vec<MaybeCheckedTransaction>) -> Self {
        Self {
            transactions: ParkingMutex::new(transactions),
        }
    }
}

impl TransactionsSource for OnceTransactionsSource {
    fn next(
        &self,
        _: u64,
        transactions_limit: u16,
        _: u32,
    ) -> Vec<MaybeCheckedTransaction> {
        let mut lock = self.transactions.lock();
        let transactions: &mut Vec<MaybeCheckedTransaction> = lock.as_mut();
        // Avoid panicking if we request more transactions than there are in the vector
        let transactions_limit = (transactions_limit as usize).min(transactions.len());
        transactions.drain(..transactions_limit).collect()
    }
}

/// The result of waiting for new transactions.
pub enum WaitNewTransactionsResult {
    /// We received a new transaction and we can continue the block execution.
    NewTransaction,
    /// We didn't receive any new transaction and we can end the block execution.
    Timeout,
}

pub struct TimeoutOnlyTxWaiter;

impl NewTxWaiterPort for TimeoutOnlyTxWaiter {
    async fn wait_for_new_transactions(&mut self) -> WaitNewTransactionsResult {
        WaitNewTransactionsResult::Timeout
    }
}

pub struct TransparentPreconfirmationSender;

impl PreconfirmationSenderPort for TransparentPreconfirmationSender {
    fn try_send(&self, _: Vec<Preconfirmation>) -> Vec<Preconfirmation> {
        vec![]
    }

    async fn send(&self, _: Vec<Preconfirmation>) {}
}

pub fn convert_tx_execution_result_to_preconfirmation(
    tx: &Transaction,
    tx_id: TxId,
    tx_exec_result: &TransactionExecutionResult,
    block_height: BlockHeight,
    tx_index: u16,
) -> Preconfirmation {
    let tx_pointer = TxPointer::new(block_height, tx_index);
    let dynamic_outputs = tx
        .outputs()
        .iter()
        .enumerate()
        .filter_map(|(i, output)| {
            if output.is_change() || output.is_variable() && output.amount() != Some(0) {
                let output_index = u16::try_from(i).ok()?;
                let utxo_id = UtxoId::new(tx_id, output_index);
                Some((utxo_id, *output))
            } else {
                None
            }
        })
        .collect();

    let status = match tx_exec_result {
        TransactionExecutionResult::Success {
            receipts,
            total_gas,
            total_fee,
            ..
        } => PreconfirmationStatus::Success {
            total_gas: *total_gas,
            total_fee: *total_fee,
            tx_pointer,
            receipts: receipts.clone(),
            outputs: dynamic_outputs,
        },
        TransactionExecutionResult::Failed {
            receipts,
            total_gas,
            total_fee,
            ..
        } => PreconfirmationStatus::Failure {
            total_gas: *total_gas,
            total_fee: *total_fee,
            tx_pointer,
            receipts: receipts.clone(),
            outputs: dynamic_outputs,
        },
    };
    Preconfirmation { tx_id, status }
}

/// Data that is generated after executing all transactions.
#[derive(Default)]
pub struct ExecutionData {
    coinbase: u64,
    used_gas: u64,
    used_size: u32,
    tx_count: u16,
    found_mint: bool,
    message_ids: Vec<MessageId>,
    tx_status: Vec<TransactionExecutionStatus>,
    events: Vec<ExecutorEvent>,
    changes: Changes,
    pub skipped_transactions: Vec<(TxId, ExecutorError)>,
    event_inbox_root: Bytes32,
}

impl ExecutionData {
    pub fn new() -> Self {
        ExecutionData {
            coinbase: 0,
            used_gas: 0,
            used_size: 0,
            tx_count: 0,
            found_mint: false,
            message_ids: Vec::new(),
            tx_status: Vec::new(),
            events: Vec::new(),
            changes: Default::default(),
            skipped_transactions: Vec::new(),
            event_inbox_root: Default::default(),
        }
    }
}

/// Per-block execution options.
/// These are passed to the executor.
#[derive(serde::Serialize, serde::Deserialize, Clone, Default, Debug)]
pub struct ExecutionOptions {
    /// The flag allows the usage of fake coins in the inputs of the transaction.
    /// When `false` the executor skips signature and UTXO existence checks.
    pub forbid_fake_coins: bool,
    /// Print execution backtraces if transaction execution reverts.
    ///
    /// Deprecated field. Do nothing. This fields exists for serialization and
    /// deserialization compatibility.
    pub backtrace: bool,
}

/// Per-block execution options
#[derive(Clone, Default, Debug)]
struct ExecutionOptionsInner {
    /// The flag allows the usage of fake coins in the inputs of the transaction.
    /// When `false` the executor skips signature and UTXO existence checks.
    pub forbid_fake_coins: bool,
    pub dry_run: bool,
}

/// The executor instance performs block production and validation. Given a block, it will execute all
/// the transactions contained in the block and persist changes to the underlying database as needed.
#[derive(Clone, Debug)]
pub struct ExecutionInstance<R, D> {
    pub relayer: R,
    pub database: D,
    pub options: ExecutionOptions,
}

impl<R, D> ExecutionInstance<R, D>
where
    R: RelayerPort,
    D: KeyValueInspect<Column = Column>,
{
    pub fn new(relayer: R, database: D, options: ExecutionOptions) -> Self {
        Self {
            relayer,
            database,
            options,
        }
    }

    #[tracing::instrument(skip_all)]
    pub async fn produce_without_commit<TxSource>(
        self,
        components: Components<TxSource>,
        dry_run: bool,
        new_tx_waiter: impl NewTxWaiterPort,
        preconfirmation_sender: impl PreconfirmationSenderPort,
    ) -> ExecutorResult<UncommittedResult<Changes>>
    where
        TxSource: TransactionsSource,
    {
        let consensus_params_version = components.consensus_parameters_version();

        let (block_executor, storage_tx) = self.into_executor(
            consensus_params_version,
            new_tx_waiter,
            preconfirmation_sender,
            dry_run,
        )?;

        #[cfg(feature = "fault-proving")]
        let chain_id = block_executor.consensus_params.chain_id();

        let (partial_block, execution_data) =
            block_executor.execute(components, storage_tx).await?;

        let ExecutionData {
            message_ids,
            event_inbox_root,
            changes,
            events,
            tx_status,
            skipped_transactions,
            coinbase,
            used_gas,
            used_size,
            ..
        } = execution_data;

        let block = partial_block
            .generate(
                &message_ids[..],
                event_inbox_root,
                #[cfg(feature = "fault-proving")]
                &chain_id,
            )
            .map_err(ExecutorError::BlockHeaderError)?;

        let finalized_block_id = block.id();

        debug!(
            "Block {:#x} fees: {} gas: {} tx_size: {}",
            finalized_block_id, coinbase, used_gas, used_size
        );

        let result = ExecutionResult {
            block,
            skipped_transactions,
            tx_status,
            events,
        };

        Ok(UncommittedResult::new(result, changes))
    }

    pub fn validate_without_commit(
        self,
        block: &Block,
    ) -> ExecutorResult<UncommittedValidationResult<Changes>> {
        let consensus_params_version = block.header().consensus_parameters_version();
        let (block_executor, storage_tx) = self.into_executor(
            consensus_params_version,
            TimeoutOnlyTxWaiter,
            TransparentPreconfirmationSender,
            false,
        )?;

        let ExecutionData {
            coinbase,
            used_gas,
            used_size,
            tx_status,
            events,
            changes,
            ..
        } = block_executor.validate_block(block, storage_tx)?;

        let finalized_block_id = block.id();

        debug!(
            "Block {:#x} fees: {} gas: {} tx_size: {}",
            finalized_block_id, coinbase, used_gas, used_size
        );

        let result = ValidationResult { tx_status, events };

        Ok(UncommittedValidationResult::new(result, changes))
    }

    fn into_executor<N, P>(
        self,
        consensus_params_version: ConsensusParametersVersion,
        new_tx_waiter: N,
        preconfirmation_sender: P,
        dry_run: bool,
    ) -> ExecutorResult<(BlockExecutor<R, N, P>, StorageTransaction<D>)> {
        let storage_tx = self
            .database
            .into_transaction()
            .with_policy(ConflictPolicy::Overwrite);
        let consensus_params = storage_tx
            .storage::<ConsensusParametersVersions>()
            .get(&consensus_params_version)?
            .ok_or(ExecutorError::ConsensusParametersNotFound(
                consensus_params_version,
            ))?
            .into_owned();
        let executor = BlockExecutor::new(
            self.relayer,
            self.options,
            consensus_params,
            new_tx_waiter,
            preconfirmation_sender,
            dry_run,
        )?;
        Ok((executor, storage_tx))
    }
}

type BlockStorageTransaction<T> = StorageTransaction<T>;
type TxStorageTransaction<'a, T> = StorageTransaction<&'a mut BlockStorageTransaction<T>>;

#[derive(Clone, Debug)]
pub struct BlockExecutor<R, TxWaiter, PreconfirmationSender> {
    relayer: R,
    consensus_params: ConsensusParameters,
    options: ExecutionOptionsInner,
    new_tx_waiter: TxWaiter,
    preconfirmation_sender: PreconfirmationSender,
}

impl<R, TxWaiter, PreconfirmationSender>
    BlockExecutor<R, TxWaiter, PreconfirmationSender>
{
    pub fn new(
        relayer: R,
        options: ExecutionOptions,
        consensus_params: ConsensusParameters,
        new_tx_waiter: TxWaiter,
        preconfirmation_sender: PreconfirmationSender,
        dry_run: bool,
    ) -> ExecutorResult<Self> {
        Ok(Self {
            relayer,
            consensus_params,
            options: ExecutionOptionsInner {
                forbid_fake_coins: options.forbid_fake_coins,
                dry_run,
            },
            new_tx_waiter,
            preconfirmation_sender,
        })
    }
}

impl<R, N, P> BlockExecutor<R, N, P>
where
    R: RelayerPort,
    N: NewTxWaiterPort,
    P: PreconfirmationSenderPort,
{
    async fn execute<TxSource, D>(
        self,
        components: Components<TxSource>,
        block_storage_tx: BlockStorageTransaction<D>,
    ) -> ExecutorResult<(PartialFuelBlock, ExecutionData)>
    where
        TxSource: TransactionsSource,
        D: KeyValueInspect<Column = Column>,
    {
        if self.options.dry_run {
            self.dry_run_block(components, block_storage_tx).await
        } else {
            self.produce_block(components, block_storage_tx).await
        }
    }

    #[tracing::instrument(skip_all)]
    /// Produce the fuel block with specified components
    async fn produce_block<TxSource, D>(
        mut self,
        components: Components<TxSource>,
        mut block_storage_tx: BlockStorageTransaction<D>,
    ) -> ExecutorResult<(PartialFuelBlock, ExecutionData)>
    where
        TxSource: TransactionsSource,
        D: KeyValueInspect<Column = Column>,
    {
        let mut partial_block =
            PartialFuelBlock::new(components.header_to_produce, vec![]);
        let mut data = ExecutionData::new();
        let mut memory = MemoryInstance::new();

        self.process_l1_txs(
            &mut partial_block,
            components.coinbase_recipient,
            &mut block_storage_tx,
            &mut data,
            &mut memory,
        )?;

        loop {
            self.process_l2_txs(
                &mut partial_block,
                &components,
                &mut block_storage_tx,
                &mut data,
                &mut memory,
            )
            .await?;
            match self.new_tx_waiter.wait_for_new_transactions().await {
                WaitNewTransactionsResult::Timeout => break,
                WaitNewTransactionsResult::NewTransaction => {
                    continue;
                }
            }
        }

        self.produce_mint_tx(
            &mut partial_block,
            &components,
            &mut block_storage_tx,
            &mut data,
            &mut memory,
        )?;
        debug_assert!(data.found_mint, "Mint transaction is not found");

        data.changes = block_storage_tx.into_changes();
        Ok((partial_block, data))
    }

    fn produce_mint_tx<TxSource, T>(
        &self,
        block: &mut PartialFuelBlock,
        components: &Components<TxSource>,
        storage_tx: &mut BlockStorageTransaction<T>,
        data: &mut ExecutionData,
        memory: &mut MemoryInstance,
    ) -> ExecutorResult<()>
    where
        T: KeyValueInspect<Column = Column>,
    {
        let Components {
            coinbase_recipient: coinbase_contract_id,
            gas_price,
            ..
        } = *components;

        let amount_to_mint = if coinbase_contract_id != ContractId::zeroed() {
            data.coinbase
        } else {
            0
        };
        let block_height = *block.header.height();

        let coinbase_tx = Transaction::mint(
            TxPointer::new(block_height, data.tx_count),
            input::contract::Contract {
                utxo_id: UtxoId::new(Bytes32::zeroed(), 0),
                balance_root: Bytes32::zeroed(),
                state_root: Bytes32::zeroed(),
                tx_pointer: TxPointer::new(BlockHeight::new(0), 0),
                contract_id: coinbase_contract_id,
            },
            output::contract::Contract {
                input_index: 0,
                balance_root: Bytes32::zeroed(),
                state_root: Bytes32::zeroed(),
            },
            amount_to_mint,
            *self.consensus_params.base_asset_id(),
            gas_price,
        );

        self.execute_transaction_and_commit(
            block,
            storage_tx,
            data,
            MaybeCheckedTransaction::Transaction(coinbase_tx.into()),
            gas_price,
            coinbase_contract_id,
            memory,
        )?;

        Ok(())
    }

    /// Execute dry-run of block with specified components
    async fn dry_run_block<TxSource, D>(
        mut self,
        components: Components<TxSource>,
        mut block_storage_tx: BlockStorageTransaction<D>,
    ) -> ExecutorResult<(PartialFuelBlock, ExecutionData)>
    where
        TxSource: TransactionsSource,
        D: KeyValueInspect<Column = Column>,
    {
        let mut partial_block =
            PartialFuelBlock::new(components.header_to_produce, vec![]);
        let mut data = ExecutionData::new();

        self.process_l2_txs(
            &mut partial_block,
            &components,
            &mut block_storage_tx,
            &mut data,
            &mut MemoryInstance::new(),
        )
        .await?;

        data.changes = block_storage_tx.into_changes();
        Ok((partial_block, data))
    }

    fn process_l1_txs<T>(
        &mut self,
        block: &mut PartialFuelBlock,
        coinbase_contract_id: ContractId,
        storage_tx: &mut BlockStorageTransaction<T>,
        data: &mut ExecutionData,
        memory: &mut MemoryInstance,
    ) -> ExecutorResult<()>
    where
        T: KeyValueInspect<Column = Column>,
    {
        let block_height = *block.header.height();
        let da_block_height = block.header.da_height;
        let relayed_txs = self.get_relayed_txs(
            block_height,
            da_block_height,
            data,
            storage_tx,
            memory,
        )?;

        self.process_relayed_txs(
            relayed_txs,
            block,
            storage_tx,
            data,
            coinbase_contract_id,
            memory,
        )?;

        Ok(())
    }

    async fn process_l2_txs<T, TxSource>(
        &mut self,
        block: &mut PartialFuelBlock,
        components: &Components<TxSource>,
        storage_tx: &mut StorageTransaction<T>,
        data: &mut ExecutionData,
        memory: &mut MemoryInstance,
    ) -> ExecutorResult<()>
    where
        T: KeyValueInspect<Column = Column>,
        TxSource: TransactionsSource,
    {
        let Components {
            transactions_source: l2_tx_source,
            coinbase_recipient: coinbase_contract_id,
            gas_price,
            ..
        } = components;
        let block_gas_limit = self.consensus_params.block_gas_limit();
        let block_transaction_size_limit = self
            .consensus_params
            .block_transaction_size_limit()
            .try_into()
            .unwrap_or(u32::MAX);

        let mut remaining_gas_limit = block_gas_limit.saturating_sub(data.used_gas);
        let mut remaining_block_transaction_size_limit =
            block_transaction_size_limit.saturating_sub(data.used_size);

        // We allow at most u16::MAX transactions in a block, including the mint transaction.
        // When processing l2 transactions, we must take into account transactions from the l1
        // that have been included in the block already (stored in `data.tx_count`), as well
        // as the final mint transaction.
        let mut remaining_tx_count = max_tx_count().saturating_sub(data.tx_count);

        let mut regular_tx_iter = l2_tx_source
            .next(
                remaining_gas_limit,
                remaining_tx_count,
                remaining_block_transaction_size_limit,
            )
            .into_iter()
            .take(remaining_tx_count as usize)
            .peekable();
        let mut statuses = vec![];
        while regular_tx_iter.peek().is_some() {
            for transaction in regular_tx_iter {
                let tx_id = transaction.id(&self.consensus_params.chain_id());
                let tx_max_gas = transaction.max_gas(&self.consensus_params)?;
                if tx_max_gas > remaining_gas_limit {
                    data.skipped_transactions.push((
                        tx_id,
                        ExecutorError::GasOverflow(
                            format!("Transaction cannot fit in remaining gas limit: ({remaining_gas_limit})."),
                            tx_max_gas,
                            remaining_gas_limit,
                        ),
                    ));
                    continue;
                }
                match self.execute_transaction_and_commit(
                    block,
                    storage_tx,
                    data,
                    transaction,
                    *gas_price,
                    *coinbase_contract_id,
                    memory,
                ) {
                    Ok(_) => {
                        // SAFETY: Transaction succeed so we know that `tx_status` has been populated
                        let latest_executed_tx = data.tx_status.last().expect(
                            "Shouldn't happens as we just added a transaction; qed",
                        );
                        let tx = block.transactions.last().expect(
                            "Shouldn't happens as we just added a transaction; qed",
                        );
                        let preconfirmation_status =
                            convert_tx_execution_result_to_preconfirmation(
                                tx,
                                tx_id,
                                &latest_executed_tx.result,
                                *block.header.height(),
                                data.tx_count,
                            );
                        statuses.push(preconfirmation_status);
                    }
                    Err(err) => {
                        statuses.push(Preconfirmation {
                            tx_id,
                            status: PreconfirmationStatus::SqueezedOut {
                                reason: err.to_string(),
                            },
                        });
                        data.skipped_transactions.push((tx_id, err));
                    }
                }
                statuses = self.preconfirmation_sender.try_send(statuses);
                remaining_gas_limit = block_gas_limit.saturating_sub(data.used_gas);
                remaining_block_transaction_size_limit =
                    block_transaction_size_limit.saturating_sub(data.used_size);
                remaining_tx_count = max_tx_count().saturating_sub(data.tx_count);
            }

            regular_tx_iter = l2_tx_source
                .next(
                    remaining_gas_limit,
                    remaining_tx_count,
                    remaining_block_transaction_size_limit,
                )
                .into_iter()
                .take(remaining_tx_count as usize)
                .peekable();
        }
        if !statuses.is_empty() {
            self.preconfirmation_sender.send(statuses).await;
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_transaction_and_commit<'a, W>(
        &'a self,
        block: &'a mut PartialFuelBlock,
        storage_tx: &mut BlockStorageTransaction<W>,
        execution_data: &mut ExecutionData,
        tx: MaybeCheckedTransaction,
        gas_price: Word,
        coinbase_contract_id: ContractId,
        memory: &mut MemoryInstance,
    ) -> ExecutorResult<()>
    where
        W: KeyValueInspect<Column = Column>,
    {
        let tx_count = execution_data.tx_count;
        let tx = {
            let mut tx_st_transaction = storage_tx
                .write_transaction()
                .with_policy(ConflictPolicy::Overwrite);
            let tx_id = tx.id(&self.consensus_params.chain_id());
            let tx = self.execute_transaction(
                tx,
                &tx_id,
                &block.header,
                coinbase_contract_id,
                gas_price,
                execution_data,
                &mut tx_st_transaction,
                memory,
            )?;
            tx_st_transaction.commit()?;
            tx
        };

        block.transactions.push(tx);
        execution_data.tx_count = tx_count
            .checked_add(1)
            .ok_or(ExecutorError::TooManyTransactions)?;

        Ok(())
    }

    #[tracing::instrument(skip_all)]
    fn validate_block<D>(
        mut self,
        block: &Block,
        mut block_storage_tx: StorageTransaction<D>,
    ) -> ExecutorResult<ExecutionData>
    where
        D: KeyValueInspect<Column = Column>,
    {
        let mut data = ExecutionData::new();

        let partial_header = PartialBlockHeader::from(block.header());
        let mut partial_block = PartialFuelBlock::new(partial_header, vec![]);
        let transactions = block.transactions();
        let mut memory = MemoryInstance::new();

        let (gas_price, coinbase_contract_id) =
            Self::get_coinbase_info_from_mint_tx(transactions)?;

        self.process_l1_txs(
            &mut partial_block,
            coinbase_contract_id,
            &mut block_storage_tx,
            &mut data,
            &mut memory,
        )?;
        let processed_l1_tx_count = partial_block.transactions.len();

        for transaction in transactions.iter().skip(processed_l1_tx_count) {
            let maybe_checked_tx =
                MaybeCheckedTransaction::Transaction(transaction.clone());
            self.execute_transaction_and_commit(
                &mut partial_block,
                &mut block_storage_tx,
                &mut data,
                maybe_checked_tx,
                gas_price,
                coinbase_contract_id,
                &mut memory,
            )?;
        }

        self.check_block_matches(partial_block, block, &data)?;

        data.changes = block_storage_tx.into_changes();
        Ok(data)
    }

    fn get_coinbase_info_from_mint_tx(
        transactions: &[Transaction],
    ) -> ExecutorResult<(u64, ContractId)> {
        if let Some(Transaction::Mint(mint)) = transactions.last() {
            Ok((*mint.gas_price(), mint.input_contract().contract_id))
        } else {
            Err(ExecutorError::MintMissing)
        }
    }

    const RELAYED_GAS_PRICE: Word = 0;

    fn process_relayed_txs<T>(
        &self,
        forced_transactions: Vec<CheckedTransaction>,
        partial_block: &mut PartialFuelBlock,
        storage_tx: &mut BlockStorageTransaction<T>,
        data: &mut ExecutionData,
        coinbase_contract_id: ContractId,
        memory: &mut MemoryInstance,
    ) -> ExecutorResult<()>
    where
        T: KeyValueInspect<Column = Column>,
    {
        let block_header = partial_block.header;
        let block_height = block_header.height();
        let consensus_parameters_version = block_header.consensus_parameters_version;
        let relayed_tx_iter = forced_transactions.into_iter();
        for checked in relayed_tx_iter {
            let maybe_checked_transaction = MaybeCheckedTransaction::CheckedTransaction(
                checked,
                consensus_parameters_version,
            );
            let tx_id = maybe_checked_transaction.id(&self.consensus_params.chain_id());
            match self.execute_transaction_and_commit(
                partial_block,
                storage_tx,
                data,
                maybe_checked_transaction,
                Self::RELAYED_GAS_PRICE,
                coinbase_contract_id,
                memory,
            ) {
                Ok(_) => {}
                Err(err) => {
                    let event = ExecutorEvent::ForcedTransactionFailed {
                        id: tx_id.into(),
                        block_height: *block_height,
                        failure: err.to_string(),
                    };
                    data.events.push(event);
                }
            }
        }
        Ok(())
    }

    fn get_relayed_txs<D>(
        &mut self,
        block_height: BlockHeight,
        da_block_height: DaBlockHeight,
        data: &mut ExecutionData,
        block_storage_tx: &mut BlockStorageTransaction<D>,
        memory: &mut MemoryInstance,
    ) -> ExecutorResult<Vec<CheckedTransaction>>
    where
        D: KeyValueInspect<Column = Column>,
    {
        let forced_transactions = if self.relayer.enabled() {
            self.process_da(
                block_height,
                da_block_height,
                data,
                block_storage_tx,
                memory,
            )?
        } else {
            Vec::with_capacity(0)
        };
        Ok(forced_transactions)
    }

    fn check_block_matches(
        &self,
        new_partial_block: PartialFuelBlock,
        old_block: &Block,
        data: &ExecutionData,
    ) -> ExecutorResult<()> {
        let ExecutionData {
            message_ids,
            event_inbox_root,
            ..
        } = &data;

        let chain_id = self.consensus_params.chain_id();

        new_partial_block
            .transactions
            .iter()
            .zip(old_block.transactions())
            .try_for_each(|(new_tx, old_tx)| {
                if new_tx != old_tx {
                    let transaction_id = old_tx.id(&chain_id);

                    tracing::info!(
                        "Transaction {:?} does not match: new_tx {:?} and old_tx {:?}",
                        transaction_id,
                        new_tx,
                        old_tx
                    );

                    Err(ExecutorError::InvalidTransactionOutcome { transaction_id })
                } else {
                    Ok(())
                }
            })?;

        let new_block = new_partial_block
            .generate(
                &message_ids[..],
                *event_inbox_root,
                #[cfg(feature = "fault-proving")]
                &chain_id,
            )
            .map_err(ExecutorError::BlockHeaderError)?;
        if new_block.header() != old_block.header() {
            tracing::info!(
                "Headers does not match: new_block {:?} and old_block {:?}",
                new_block,
                old_block
            );

            Err(ExecutorError::BlockMismatch)
        } else {
            Ok(())
        }
    }

    fn process_da<D>(
        &mut self,
        block_height: BlockHeight,
        da_block_height: DaBlockHeight,
        execution_data: &mut ExecutionData,
        block_storage_tx: &mut BlockStorageTransaction<D>,
        memory: &mut MemoryInstance,
    ) -> ExecutorResult<Vec<CheckedTransaction>>
    where
        D: KeyValueInspect<Column = Column>,
    {
        let prev_block_height = block_height
            .pred()
            .ok_or(ExecutorError::ExecutingGenesisBlock)?;

        let prev_block_header = block_storage_tx
            .storage::<FuelBlocks>()
            .get(&prev_block_height)?
            .ok_or(ExecutorError::PreviousBlockIsNotFound)?;
        let previous_da_height = prev_block_header.header().da_height();
        let Some(next_unprocessed_da_height) = previous_da_height.0.checked_add(1) else {
            return Err(ExecutorError::DaHeightExceededItsLimit)
        };

        let mut root_calculator = MerkleRootCalculator::new();

        let mut checked_forced_txs = vec![];

        for da_height in next_unprocessed_da_height..=da_block_height.0 {
            let da_height = da_height.into();
            let events = self
                .relayer
                .get_events(&da_height)
                .map_err(|err| ExecutorError::RelayerError(err.to_string()))?;
            for event in events {
                root_calculator.push(event.hash().as_ref());
                match event {
                    Event::Message(message) => {
                        if message.da_height() != da_height {
                            return Err(ExecutorError::RelayerGivesIncorrectMessages)
                        }
                        block_storage_tx
                            .storage_as_mut::<Messages>()
                            .insert(message.nonce(), &message)?;
                        execution_data
                            .events
                            .push(ExecutorEvent::MessageImported(message));
                    }
                    Event::Transaction(relayed_tx) => {
                        let id = relayed_tx.id();
                        let checked_tx_res = Self::validate_forced_tx(
                            relayed_tx,
                            block_height,
                            &self.consensus_params,
                            memory,
                            block_storage_tx,
                        );
                        match checked_tx_res {
                            Ok(checked_tx) => {
                                checked_forced_txs.push(checked_tx);
                            }
                            Err(err) => {
                                execution_data.events.push(
                                    ExecutorEvent::ForcedTransactionFailed {
                                        id,
                                        block_height,
                                        failure: err.to_string(),
                                    },
                                );
                            }
                        }
                    }
                }
            }
        }

        execution_data.event_inbox_root = root_calculator.root().into();

        Ok(checked_forced_txs)
    }

    /// Parse forced transaction payloads and perform basic checks
    fn validate_forced_tx<D>(
        relayed_tx: RelayedTransaction,
        block_height: BlockHeight,
        consensus_params: &ConsensusParameters,
        memory: &mut MemoryInstance,
        block_storage_tx: &BlockStorageTransaction<D>,
    ) -> Result<CheckedTransaction, ForcedTransactionFailure>
    where
        D: KeyValueInspect<Column = Column>,
    {
        let parsed_tx = Self::parse_tx_bytes(&relayed_tx)?;
        Self::tx_is_valid_variant(&parsed_tx)?;
        Self::relayed_tx_claimed_enough_max_gas(
            &parsed_tx,
            &relayed_tx,
            consensus_params,
        )?;
        let checked_tx = Self::get_checked_tx(
            parsed_tx,
            block_height,
            consensus_params,
            memory,
            block_storage_tx,
        )?;
        Ok(CheckedTransaction::from(checked_tx))
    }

    fn parse_tx_bytes(
        relayed_transaction: &RelayedTransaction,
    ) -> Result<Transaction, ForcedTransactionFailure> {
        let tx_bytes = relayed_transaction.serialized_transaction();
        let tx = Transaction::from_bytes(tx_bytes)
            .map_err(|_| ForcedTransactionFailure::CodecError)?;
        Ok(tx)
    }

    fn get_checked_tx<D>(
        tx: Transaction,
        height: BlockHeight,
        consensus_params: &ConsensusParameters,
        memory: &mut MemoryInstance,
        block_storage_tx: &BlockStorageTransaction<D>,
    ) -> Result<Checked<Transaction>, ForcedTransactionFailure>
    where
        D: KeyValueInspect<Column = Column>,
    {
        let checked_tx = tx
            .into_checked_reusable_memory(
                height,
                consensus_params,
                memory,
                block_storage_tx,
            )
            .map_err(ForcedTransactionFailure::CheckError)?;
        Ok(checked_tx)
    }

    fn tx_is_valid_variant(tx: &Transaction) -> Result<(), ForcedTransactionFailure> {
        match tx {
            Transaction::Mint(_) => Err(ForcedTransactionFailure::InvalidTransactionType),
            Transaction::Script(_)
            | Transaction::Create(_)
            | Transaction::Upgrade(_)
            | Transaction::Upload(_)
            | Transaction::Blob(_) => Ok(()),
        }
    }

    fn relayed_tx_claimed_enough_max_gas(
        tx: &Transaction,
        relayed_tx: &RelayedTransaction,
        consensus_params: &ConsensusParameters,
    ) -> Result<(), ForcedTransactionFailure> {
        let claimed_max_gas = relayed_tx.max_gas();
        let actual_max_gas = tx
            .max_gas(consensus_params)
            .map_err(|_| ForcedTransactionFailure::InvalidTransactionType)?;
        if actual_max_gas > claimed_max_gas {
            return Err(ForcedTransactionFailure::InsufficientMaxGas {
                claimed_max_gas,
                actual_max_gas,
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_transaction<T>(
        &self,
        tx: MaybeCheckedTransaction,
        tx_id: &TxId,
        header: &PartialBlockHeader,
        coinbase_contract_id: ContractId,
        gas_price: Word,
        execution_data: &mut ExecutionData,
        storage_tx: &mut TxStorageTransaction<T>,
        memory: &mut MemoryInstance,
    ) -> ExecutorResult<Transaction>
    where
        T: KeyValueInspect<Column = Column>,
    {
        Self::check_mint_is_not_found(execution_data)?;
        Self::check_tx_is_not_duplicate(tx_id, storage_tx)?;
        let checked_tx = self.convert_maybe_checked_tx_to_checked_tx(tx, header)?;

        match checked_tx {
            CheckedTransaction::Script(tx) => self.execute_chargeable_transaction(
                tx,
                header,
                coinbase_contract_id,
                gas_price,
                execution_data,
                storage_tx,
                memory,
            ),
            CheckedTransaction::Create(tx) => self.execute_chargeable_transaction(
                tx,
                header,
                coinbase_contract_id,
                gas_price,
                execution_data,
                storage_tx,
                memory,
            ),
            CheckedTransaction::Mint(mint) => self.execute_mint(
                mint,
                header,
                coinbase_contract_id,
                gas_price,
                execution_data,
                storage_tx,
            ),
            CheckedTransaction::Upgrade(tx) => self.execute_chargeable_transaction(
                tx,
                header,
                coinbase_contract_id,
                gas_price,
                execution_data,
                storage_tx,
                memory,
            ),
            CheckedTransaction::Upload(tx) => self.execute_chargeable_transaction(
                tx,
                header,
                coinbase_contract_id,
                gas_price,
                execution_data,
                storage_tx,
                memory,
            ),
            CheckedTransaction::Blob(tx) => self.execute_chargeable_transaction(
                tx,
                header,
                coinbase_contract_id,
                gas_price,
                execution_data,
                storage_tx,
                memory,
            ),
        }
    }

    fn check_mint_is_not_found(execution_data: &ExecutionData) -> ExecutorResult<()> {
        if execution_data.found_mint {
            return Err(ExecutorError::MintIsNotLastTransaction)
        }
        Ok(())
    }

    fn check_tx_is_not_duplicate<T>(
        tx_id: &TxId,
        storage_tx: &TxStorageTransaction<T>,
    ) -> ExecutorResult<()>
    where
        T: KeyValueInspect<Column = Column>,
    {
        if storage_tx
            .storage::<ProcessedTransactions>()
            .contains_key(tx_id)?
        {
            return Err(ExecutorError::TransactionIdCollision(*tx_id))
        }
        Ok(())
    }

    fn convert_maybe_checked_tx_to_checked_tx(
        &self,
        tx: MaybeCheckedTransaction,
        header: &PartialBlockHeader,
    ) -> ExecutorResult<CheckedTransaction> {
        let block_height = *header.height();
        let actual_version = header.consensus_parameters_version;
        let expiration = tx.expiration();
        let checked_tx = match tx {
            MaybeCheckedTransaction::Transaction(tx) => tx
                .into_checked_basic(block_height, &self.consensus_params)?
                .into(),
            MaybeCheckedTransaction::CheckedTransaction(checked_tx, checked_version) => {
                // If you plan to add an other check of validity like this one on the checked_tx
                // then probably the `CheckedTransaction` type isn't useful anymore.
                if block_height > expiration {
                    return Err(ExecutorError::TransactionExpired(
                        expiration,
                        block_height,
                    ));
                }
                if actual_version == checked_version {
                    checked_tx
                } else {
                    let checked_tx: Checked<Transaction> = checked_tx.into();
                    let (tx, _) = checked_tx.into();
                    tx.into_checked_basic(block_height, &self.consensus_params)?
                        .into()
                }
            }
        };
        Ok(checked_tx)
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_mint<T>(
        &self,
        checked_mint: Checked<Mint>,
        header: &PartialBlockHeader,
        coinbase_contract_id: ContractId,
        gas_price: Word,
        execution_data: &mut ExecutionData,
        storage_tx: &mut TxStorageTransaction<T>,
    ) -> ExecutorResult<Transaction>
    where
        T: KeyValueInspect<Column = Column>,
    {
        execution_data.found_mint = true;

        Self::check_mint_has_expected_index(&checked_mint, execution_data)?;

        let coinbase_id = checked_mint.id();
        let (mut mint, _) = checked_mint.into();

        Self::check_gas_price(&mint, gas_price)?;
        if mint.input_contract().contract_id == ContractId::zeroed() {
            Self::verify_mint_for_empty_contract(&mint)?;
        } else {
            Self::check_mint_amount(&mint, execution_data.coinbase)?;

            let input = mint.input_contract().clone();
            let mut input = Input::Contract(input);

            if self.options.forbid_fake_coins {
                self.verify_inputs_exist_and_values_match(
                    storage_tx,
                    core::slice::from_ref(&input),
                    header.da_height,
                )?;
            }

            self.compute_inputs(core::slice::from_mut(&mut input), storage_tx)?;

            let (input, output) = self.execute_mint_with_vm(
                header,
                coinbase_contract_id,
                execution_data,
                storage_tx,
                &coinbase_id,
                &mut mint,
                &mut input,
            )?;

            *mint.input_contract_mut() = input;
            *mint.output_contract_mut() = output;
        }

        Self::store_mint_tx(mint, execution_data, coinbase_id, storage_tx)
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_chargeable_transaction<Tx, T>(
        &self,
        mut checked_tx: Checked<Tx>,
        header: &PartialBlockHeader,
        coinbase_contract_id: ContractId,
        gas_price: Word,
        execution_data: &mut ExecutionData,
        storage_tx: &mut TxStorageTransaction<T>,
        memory: &mut MemoryInstance,
    ) -> ExecutorResult<Transaction>
    where
        Tx: ExecutableTransaction + Cacheable + Send + Sync + 'static,
        <Tx as IntoChecked>::Metadata: CheckedMetadataTrait + Send + Sync,
        T: KeyValueInspect<Column = Column>,
    {
        let tx_id = checked_tx.id();

        if self.options.forbid_fake_coins {
            checked_tx = self.extra_tx_checks(checked_tx, header, storage_tx, memory)?;
        }

        let (reverted, state, tx, receipts) = self.attempt_tx_execution_with_vm(
            checked_tx,
            header,
            coinbase_contract_id,
            gas_price,
            storage_tx,
            memory,
        )?;

        self.spend_input_utxos(tx.inputs(), storage_tx, reverted, execution_data)?;

        self.persist_output_utxos(
            *header.height(),
            execution_data,
            &tx_id,
            storage_tx,
            tx.inputs(),
            tx.outputs(),
        )?;

        storage_tx
            .storage::<ProcessedTransactions>()
            .insert(&tx_id, &())?;

        self.update_execution_data(
            &tx,
            execution_data,
            receipts,
            gas_price,
            reverted,
            state,
            tx_id,
        )?;

        Ok(tx.into())
    }

    fn check_mint_amount(mint: &Mint, expected_amount: u64) -> ExecutorResult<()> {
        if *mint.mint_amount() != expected_amount {
            return Err(ExecutorError::CoinbaseAmountMismatch)
        }
        Ok(())
    }

    fn check_gas_price(mint: &Mint, expected_gas_price: Word) -> ExecutorResult<()> {
        if *mint.gas_price() != expected_gas_price {
            return Err(ExecutorError::CoinbaseGasPriceMismatch)
        }
        Ok(())
    }

    fn check_mint_has_expected_index(
        checked_mint: &Checked<Mint>,
        execution_data: &ExecutionData,
    ) -> ExecutorResult<()> {
        if checked_mint.transaction().tx_pointer().tx_index() != execution_data.tx_count {
            return Err(ExecutorError::MintHasUnexpectedIndex)
        }
        Ok(())
    }

    fn verify_mint_for_empty_contract(mint: &Mint) -> ExecutorResult<()> {
        if *mint.mint_amount() != 0 {
            return Err(ExecutorError::CoinbaseAmountMismatch)
        }

        let input = input::contract::Contract {
            utxo_id: UtxoId::new(Bytes32::zeroed(), 0),
            balance_root: Bytes32::zeroed(),
            state_root: Bytes32::zeroed(),
            tx_pointer: TxPointer::new(BlockHeight::new(0), 0),
            contract_id: ContractId::zeroed(),
        };
        let output = output::contract::Contract {
            input_index: 0,
            balance_root: Bytes32::zeroed(),
            state_root: Bytes32::zeroed(),
        };
        if mint.input_contract() != &input || mint.output_contract() != &output {
            return Err(ExecutorError::MintMismatch)
        }
        Ok(())
    }

    fn store_mint_tx<T>(
        mint: Mint,
        execution_data: &mut ExecutionData,
        coinbase_id: TxId,
        storage_tx: &mut TxStorageTransaction<T>,
    ) -> ExecutorResult<Transaction>
    where
        T: KeyValueInspect<Column = Column>,
    {
        let tx = mint.into();

        execution_data.tx_status.push(TransactionExecutionStatus {
            id: coinbase_id,
            result: TransactionExecutionResult::Success {
                result: None,
                receipts: vec![],
                total_gas: 0,
                total_fee: 0,
            },
        });

        if storage_tx
            .storage::<ProcessedTransactions>()
            .replace(&coinbase_id, &())?
            .is_some()
        {
            return Err(ExecutorError::TransactionIdCollision(coinbase_id))
        }
        Ok(tx)
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_mint_with_vm<T>(
        &self,
        header: &PartialBlockHeader,
        coinbase_contract_id: ContractId,
        execution_data: &mut ExecutionData,
        storage_tx: &mut TxStorageTransaction<T>,
        coinbase_id: &TxId,
        mint: &mut Mint,
        input: &mut Input,
    ) -> ExecutorResult<(input::contract::Contract, output::contract::Contract)>
    where
        T: KeyValueInspect<Column = Column>,
    {
        let mut sub_block_db_commit = storage_tx
            .write_transaction()
            .with_policy(ConflictPolicy::Overwrite);

        let mut vm_db = VmStorage::new(
            &mut sub_block_db_commit,
            &header.consensus,
            &header.application,
            coinbase_contract_id,
        );

        fuel_vm::interpreter::contract::balance_increase(
            &mut vm_db,
            &mint.input_contract().contract_id,
            mint.mint_asset_id(),
            *mint.mint_amount(),
        )
        .map_err(|e| format!("{e}"))
        .map_err(ExecutorError::CoinbaseCannotIncreaseBalance)?;
        sub_block_db_commit.commit()?;

        let block_height = *header.height();
        let output = *mint.output_contract();
        let mut outputs = [Output::Contract(output)];
        self.persist_output_utxos(
            block_height,
            execution_data,
            coinbase_id,
            storage_tx,
            core::slice::from_ref(input),
            outputs.as_slice(),
        )?;
        self.compute_state_of_not_utxo_outputs(
            outputs.as_mut_slice(),
            core::slice::from_ref(input),
            *coinbase_id,
            storage_tx,
        )?;
        let Input::Contract(input) = core::mem::take(input) else {
            return Err(ExecutorError::Other(
                "Input of the `Mint` transaction is not a contract".to_string(),
            ))
        };
        let Output::Contract(output) = outputs[0] else {
            return Err(ExecutorError::Other(
                "The output of the `Mint` transaction is not a contract".to_string(),
            ))
        };
        Ok((input, output))
    }

    fn update_tx_outputs<Tx, T>(
        &self,
        storage_tx: &TxStorageTransaction<T>,
        tx_id: TxId,
        tx: &mut Tx,
    ) -> ExecutorResult<()>
    where
        Tx: ExecutableTransaction,
        T: KeyValueInspect<Column = Column>,
    {
        let mut outputs = core::mem::take(tx.outputs_mut());
        self.compute_state_of_not_utxo_outputs(
            &mut outputs,
            tx.inputs(),
            tx_id,
            storage_tx,
        )?;
        *tx.outputs_mut() = outputs;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn update_execution_data<Tx: Chargeable>(
        &self,
        tx: &Tx,
        execution_data: &mut ExecutionData,
        receipts: Vec<Receipt>,
        gas_price: Word,
        reverted: bool,
        state: ProgramState,
        tx_id: TxId,
    ) -> ExecutorResult<()> {
        let (used_gas, tx_fee) = self.total_fee_paid(tx, &receipts, gas_price)?;
        let used_size = tx.metered_bytes_size().try_into().unwrap_or(u32::MAX);

        execution_data.coinbase = execution_data
            .coinbase
            .checked_add(tx_fee)
            .ok_or(ExecutorError::FeeOverflow)?;
        execution_data.used_gas = execution_data
            .used_gas
            .checked_add(used_gas)
            .ok_or_else(|| {
                ExecutorError::GasOverflow(
                    "Execution used gas overflowed.".into(),
                    execution_data.used_gas,
                    used_gas,
                )
            })?;
        execution_data.used_size = execution_data
            .used_size
            .checked_add(used_size)
            .ok_or(ExecutorError::TxSizeOverflow)?;

        if !reverted {
            execution_data
                .message_ids
                .extend(receipts.iter().filter_map(|r| r.message_id()));
        }

        let status = if reverted {
            TransactionExecutionResult::Failed {
                result: Some(state),
                receipts,
                total_gas: used_gas,
                total_fee: tx_fee,
            }
        } else {
            // else tx was a success
            TransactionExecutionResult::Success {
                result: Some(state),
                receipts,
                total_gas: used_gas,
                total_fee: tx_fee,
            }
        };

        // queue up status for this tx to be stored once block id is finalized.
        execution_data.tx_status.push(TransactionExecutionStatus {
            id: tx_id,
            result: status,
        });
        Ok(())
    }

    fn update_input_used_gas<Tx>(
        predicate_gas_used: Vec<Option<Word>>,
        tx_id: TxId,
        tx: &mut Tx,
    ) -> ExecutorResult<()>
    where
        Tx: ExecutableTransaction,
    {
        for (predicate_gas_used, produced_input) in
            predicate_gas_used.into_iter().zip(tx.inputs_mut())
        {
            if let Some(gas_used) = predicate_gas_used {
                match produced_input {
                    Input::CoinPredicate(CoinPredicate {
                        predicate_gas_used, ..
                    })
                    | Input::MessageCoinPredicate(MessageCoinPredicate {
                        predicate_gas_used,
                        ..
                    })
                    | Input::MessageDataPredicate(MessageDataPredicate {
                        predicate_gas_used,
                        ..
                    }) => {
                        *predicate_gas_used = gas_used;
                    }
                    _ => {
                        debug_assert!(
                            false,
                            "This error is not possible unless VM changes the order of inputs, \
                        or we added a new predicate inputs."
                        );
                        return Err(ExecutorError::InvalidTransactionOutcome {
                            transaction_id: tx_id,
                        })
                    }
                }
            }
        }
        Ok(())
    }

    fn extra_tx_checks<Tx, T>(
        &self,
        mut checked_tx: Checked<Tx>,
        header: &PartialBlockHeader,
        storage_tx: &TxStorageTransaction<T>,
        memory: &mut MemoryInstance,
    ) -> ExecutorResult<Checked<Tx>>
    where
        Tx: ExecutableTransaction + Send + Sync + 'static,
        <Tx as IntoChecked>::Metadata: CheckedMetadataTrait + Send + Sync,
        T: KeyValueInspect<Column = Column>,
    {
        checked_tx = checked_tx
            .check_predicates(
                &CheckPredicateParams::from(&self.consensus_params),
                memory,
                storage_tx,
            )
            .map_err(|e| {
                ExecutorError::TransactionValidity(TransactionValidityError::Validation(
                    e,
                ))
            })?;
        debug_assert!(checked_tx.checks().contains(Checks::Predicates));

        self.verify_inputs_exist_and_values_match(
            storage_tx,
            checked_tx.transaction().inputs(),
            header.da_height,
        )?;
        checked_tx = checked_tx
            .check_signatures(&self.consensus_params.chain_id())
            .map_err(TransactionValidityError::from)?;
        debug_assert!(checked_tx.checks().contains(Checks::Signatures));
        Ok(checked_tx)
    }

    #[allow(clippy::type_complexity)]
    fn attempt_tx_execution_with_vm<Tx, T>(
        &self,
        checked_tx: Checked<Tx>,
        header: &PartialBlockHeader,
        coinbase_contract_id: ContractId,
        gas_price: Word,
        storage_tx: &mut TxStorageTransaction<T>,
        memory: &mut MemoryInstance,
    ) -> ExecutorResult<(bool, ProgramState, Tx, Vec<Receipt>)>
    where
        Tx: ExecutableTransaction + Cacheable,
        <Tx as IntoChecked>::Metadata: CheckedMetadataTrait + Send + Sync,
        T: KeyValueInspect<Column = Column>,
    {
        let tx_id = checked_tx.id();

        let mut sub_block_db_commit = storage_tx
            .read_transaction()
            .with_policy(ConflictPolicy::Overwrite);

        let vm_db = VmStorage::new(
            &mut sub_block_db_commit,
            &header.consensus,
            &header.application,
            coinbase_contract_id,
        );

        let gas_costs = self.consensus_params.gas_costs();
        let fee_params = self.consensus_params.fee_params();

        let predicate_gas_used = checked_tx
            .transaction()
            .inputs()
            .iter()
            .map(|input| input.predicate_gas_used())
            .collect();
        let ready_tx = checked_tx.into_ready(
            gas_price,
            gas_costs,
            fee_params,
            Some(*header.height()),
        )?;

        let mut reverted;

        let (state, mut tx, receipts) = if !self.options.dry_run {
            let mut vm = Interpreter::<_, _, _, NotSupportedEcal,
                verification::Normal>::with_storage(
                memory,
                vm_db,
                InterpreterParams::new(gas_price, &self.consensus_params),
            );

            let vm_result: StateTransition<_> = vm
                .transact(ready_tx)
                .map_err(|error| ExecutorError::VmExecution {
                    error: error.to_string(),
                    transaction_id: tx_id,
                })?
                .into();
            reverted = vm_result.should_revert();

            vm_result.into_inner()
        } else {
            let mut vm = Interpreter::<
                _,
                _,
                _,
                NotSupportedEcal,
                verification::AttemptContinue,
            >::with_storage(
                memory,
                vm_db,
                InterpreterParams::new(gas_price, &self.consensus_params),
            );

            let vm_result: StateTransition<_> = vm
                .transact(ready_tx)
                .map_err(|error| ExecutorError::VmExecution {
                    error: error.to_string(),
                    transaction_id: tx_id,
                })?
                .into();

            reverted = vm_result.should_revert();

            let (state, tx, mut receipts) = vm_result.into_inner();

            // If transaction requires contract ids, then extend receipts with
            // `PanicReason::ContractNotInInputs` for each missing contract id.
            // The data like `$pc` or `$is` is not available in this case,
            // because panic generated outside of the execution.
            if !vm.verifier().missing_contract_inputs.is_empty() {
                debug_assert!(self.options.dry_run);
                reverted = true;

                let reason = PanicInstruction::error(
                    PanicReason::ContractNotInInputs,
                    op::noop().into(),
                );

                for contract_id in &vm.verifier().missing_contract_inputs {
                    receipts.push(Receipt::Panic {
                        id: ContractId::zeroed(),
                        reason,
                        pc: 0,
                        is: 0,
                        contract_id: Some(*contract_id),
                    });
                }
            }

            (state, tx, receipts)
        };

        #[cfg(debug_assertions)]
        {
            tx.precompute(&self.consensus_params.chain_id())?;
            debug_assert_eq!(tx.id(&self.consensus_params.chain_id()), tx_id);
        }

        Self::update_input_used_gas(predicate_gas_used, tx_id, &mut tx)?;

        // We always need to update inputs with storage state before execution,
        // because VM zeroes malleable fields during the execution.
        self.compute_inputs(tx.inputs_mut(), storage_tx)?;

        // only commit state changes if execution was a success
        if !reverted {
            let changes = sub_block_db_commit.into_changes();
            storage_tx.commit_changes(changes)?;
        }

        self.update_tx_outputs(storage_tx, tx_id, &mut tx)?;
        Ok((reverted, state, tx, receipts.to_vec()))
    }

    fn verify_inputs_exist_and_values_match<T>(
        &self,
        db: &StorageTransaction<T>,
        inputs: &[Input],
        block_da_height: DaBlockHeight,
    ) -> ExecutorResult<()>
    where
        T: KeyValueInspect<Column = Column>,
    {
        for input in inputs {
            match input {
                Input::CoinSigned(CoinSigned { utxo_id, .. })
                | Input::CoinPredicate(CoinPredicate { utxo_id, .. }) => {
                    match db.storage::<Coins>().get(utxo_id)? {
                        Some(coin) => {
                            if !coin.matches_input(input).unwrap_or_default() {
                                return Err(TransactionValidityError::CoinMismatch(
                                    *utxo_id,
                                )
                                .into())
                            }
                        }
                        _ => {
                            return Err(TransactionValidityError::CoinDoesNotExist(
                                *utxo_id,
                            )
                            .into())
                        }
                    }
                }
                Input::Contract(contract) => {
                    if !db
                        .storage::<ContractsLatestUtxo>()
                        .contains_key(&contract.contract_id)?
                    {
                        return Err(TransactionValidityError::ContractDoesNotExist(
                            contract.contract_id,
                        )
                        .into())
                    }
                }
                Input::MessageCoinSigned(MessageCoinSigned { nonce, .. })
                | Input::MessageCoinPredicate(MessageCoinPredicate { nonce, .. })
                | Input::MessageDataSigned(MessageDataSigned { nonce, .. })
                | Input::MessageDataPredicate(MessageDataPredicate { nonce, .. }) => {
                    match db.storage::<Messages>().get(nonce)? {
                        Some(message) => {
                            if message.da_height() > block_da_height {
                                return Err(
                                    TransactionValidityError::MessageSpendTooEarly(
                                        *nonce,
                                    )
                                    .into(),
                                )
                            }

                            if !message.matches_input(input).unwrap_or_default() {
                                return Err(TransactionValidityError::MessageMismatch(
                                    *nonce,
                                )
                                .into())
                            }
                        }
                        _ => {
                            return Err(TransactionValidityError::MessageDoesNotExist(
                                *nonce,
                            )
                            .into())
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Mark input utxos as spent
    fn spend_input_utxos<T>(
        &self,
        inputs: &[Input],
        db: &mut TxStorageTransaction<T>,
        reverted: bool,
        execution_data: &mut ExecutionData,
    ) -> ExecutorResult<()>
    where
        T: KeyValueInspect<Column = Column>,
    {
        for input in inputs {
            match input {
                Input::CoinSigned(CoinSigned {
                    utxo_id,
                    owner,
                    amount,
                    asset_id,
                    ..
                })
                | Input::CoinPredicate(CoinPredicate {
                    utxo_id,
                    owner,
                    amount,
                    asset_id,
                    ..
                }) => {
                    // prune utxo from db
                    let coin = db
                        .storage::<Coins>()
                        .take(utxo_id)
                        .map_err(Into::into)
                        .transpose()
                        .unwrap_or_else(|| {
                            // If the coin is not found in the database, it means that it was
                            // already spent or `utxo_validation` is `false`.
                            self.get_coin_or_default(
                                db, *utxo_id, *owner, *amount, *asset_id,
                            )
                        })?;

                    execution_data
                        .events
                        .push(ExecutorEvent::CoinConsumed(coin.uncompress(*utxo_id)));
                }
                Input::MessageDataSigned(_) | Input::MessageDataPredicate(_)
                    if reverted =>
                {
                    // Don't spend the retryable messages if transaction is reverted
                    continue
                }
                Input::MessageCoinSigned(MessageCoinSigned { nonce, .. })
                | Input::MessageCoinPredicate(MessageCoinPredicate { nonce, .. })
                | Input::MessageDataSigned(MessageDataSigned { nonce, .. })
                | Input::MessageDataPredicate(MessageDataPredicate { nonce, .. }) => {
                    // ensure message wasn't already marked as spent,
                    // and cleanup message contents
                    let message = db
                        .storage::<Messages>()
                        .take(nonce)?
                        .ok_or(ExecutorError::MessageDoesNotExist(*nonce))?;
                    execution_data
                        .events
                        .push(ExecutorEvent::MessageConsumed(message));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn total_fee_paid<Tx: Chargeable>(
        &self,
        tx: &Tx,
        receipts: &[Receipt],
        gas_price: Word,
    ) -> ExecutorResult<(Word, Word)> {
        let min_gas = tx.min_gas(
            self.consensus_params.gas_costs(),
            self.consensus_params.fee_params(),
        );
        let max_fee = tx.max_fee_limit();
        let mut used_gas = 0;
        for r in receipts {
            if let Receipt::ScriptResult { gas_used, .. } = r {
                used_gas = *gas_used;
                break
            }
        }

        let fee = tx
            .refund_fee(
                self.consensus_params.gas_costs(),
                self.consensus_params.fee_params(),
                used_gas,
                gas_price,
            )
            .ok_or(ExecutorError::FeeOverflow)?;
        let total_used_gas = min_gas.checked_add(used_gas).ok_or_else(|| {
            ExecutorError::GasOverflow(
                "Total used gas overflowed.".into(),
                min_gas,
                used_gas,
            )
        })?;
        // if there's no script result (i.e. create) then fee == base amount
        Ok((
            total_used_gas,
            max_fee.checked_sub(fee).ok_or(ExecutorError::FeeOverflow)?,
        ))
    }

    /// Computes all zeroed or variable inputs.
    fn compute_inputs<T>(
        &self,
        inputs: &mut [Input],
        db: &TxStorageTransaction<T>,
    ) -> ExecutorResult<()>
    where
        T: KeyValueInspect<Column = Column>,
    {
        for input in inputs {
            match input {
                Input::CoinSigned(CoinSigned {
                    tx_pointer,
                    utxo_id,
                    owner,
                    amount,
                    asset_id,
                    ..
                })
                | Input::CoinPredicate(CoinPredicate {
                    tx_pointer,
                    utxo_id,
                    owner,
                    amount,
                    asset_id,
                    ..
                }) => {
                    let coin = self
                        .get_coin_or_default(db, *utxo_id, *owner, *amount, *asset_id)?;
                    *tx_pointer = *coin.tx_pointer();
                }
                &mut Input::Contract(input::contract::Contract {
                    ref mut utxo_id,
                    ref mut balance_root,
                    ref mut state_root,
                    ref mut tx_pointer,
                    ref contract_id,
                    ..
                }) => {
                    let contract = ContractRef::new(db, *contract_id);
                    let utxo_info =
                        contract.validated_utxo(self.options.forbid_fake_coins)?;
                    *utxo_id = *utxo_info.utxo_id();
                    *tx_pointer = utxo_info.tx_pointer();
                    *balance_root = contract.balance_root()?;
                    *state_root = contract.state_root()?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    #[allow(clippy::type_complexity)]
    // TODO: Maybe we need move it to `fuel-vm`? O_o Because other `Outputs` are processed there
    /// Computes all zeroed or variable outputs.
    /// In production mode, updates the outputs with computed values.
    /// In validation mode, compares the outputs with computed inputs.
    fn compute_state_of_not_utxo_outputs<T>(
        &self,
        outputs: &mut [Output],
        inputs: &[Input],
        tx_id: TxId,
        db: &TxStorageTransaction<T>,
    ) -> ExecutorResult<()>
    where
        T: KeyValueInspect<Column = Column>,
    {
        for output in outputs {
            if let Output::Contract(contract_output) = output {
                let contract_id =
                    if let Some(Input::Contract(input::contract::Contract {
                        contract_id,
                        ..
                    })) = inputs.get(contract_output.input_index as usize)
                    {
                        contract_id
                    } else {
                        return Err(ExecutorError::InvalidTransactionOutcome {
                            transaction_id: tx_id,
                        })
                    };

                let contract = ContractRef::new(db, *contract_id);
                contract_output.balance_root = contract.balance_root()?;
                contract_output.state_root = contract.state_root()?;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn get_coin_or_default<T>(
        &self,
        db: &TxStorageTransaction<T>,
        utxo_id: UtxoId,
        owner: Address,
        amount: u64,
        asset_id: AssetId,
    ) -> ExecutorResult<CompressedCoin>
    where
        T: KeyValueInspect<Column = Column>,
    {
        if self.options.forbid_fake_coins {
            db.storage::<Coins>()
                .get(&utxo_id)?
                .ok_or(ExecutorError::TransactionValidity(
                    TransactionValidityError::CoinDoesNotExist(utxo_id),
                ))
                .map(Cow::into_owned)
        } else {
            // if utxo validation is disabled, just assign this new input to the original block
            let coin = CompressedCoinV1 {
                owner,
                amount,
                asset_id,
                tx_pointer: Default::default(),
            }
            .into();
            Ok(coin)
        }
    }

    fn persist_output_utxos<T>(
        &self,
        block_height: BlockHeight,
        execution_data: &mut ExecutionData,
        tx_id: &Bytes32,
        db: &mut TxStorageTransaction<T>,
        inputs: &[Input],
        outputs: &[Output],
    ) -> ExecutorResult<()>
    where
        T: KeyValueInspect<Column = Column>,
    {
        let tx_idx = execution_data.tx_count;
        for (output_index, output) in outputs.iter().enumerate() {
            let index =
                u16::try_from(output_index).map_err(|_| ExecutorError::TooManyOutputs)?;
            let utxo_id = UtxoId::new(*tx_id, index);
            match output {
                Output::Coin {
                    amount,
                    asset_id,
                    to,
                } => Self::insert_coin(
                    block_height,
                    execution_data,
                    utxo_id,
                    amount,
                    asset_id,
                    to,
                    db,
                )?,
                Output::Contract(contract) => {
                    if let Some(Input::Contract(input::contract::Contract {
                        contract_id,
                        ..
                    })) = inputs.get(contract.input_index as usize)
                    {
                        let tx_pointer = TxPointer::new(block_height, tx_idx);
                        db.storage::<ContractsLatestUtxo>().insert(
                            contract_id,
                            &ContractUtxoInfo::V1((utxo_id, tx_pointer).into()),
                        )?;
                    } else {
                        return Err(ExecutorError::TransactionValidity(
                            TransactionValidityError::InvalidContractInputIndex(utxo_id),
                        ))
                    }
                }
                Output::Change {
                    to,
                    asset_id,
                    amount,
                } => Self::insert_coin(
                    block_height,
                    execution_data,
                    utxo_id,
                    amount,
                    asset_id,
                    to,
                    db,
                )?,
                Output::Variable {
                    to,
                    asset_id,
                    amount,
                } => Self::insert_coin(
                    block_height,
                    execution_data,
                    utxo_id,
                    amount,
                    asset_id,
                    to,
                    db,
                )?,
                Output::ContractCreated { contract_id, .. } => {
                    let tx_pointer = TxPointer::new(block_height, tx_idx);
                    db.storage::<ContractsLatestUtxo>().insert(
                        contract_id,
                        &ContractUtxoInfo::V1((utxo_id, tx_pointer).into()),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn insert_coin<T>(
        block_height: BlockHeight,
        execution_data: &mut ExecutionData,
        utxo_id: UtxoId,
        amount: &Word,
        asset_id: &AssetId,
        to: &Address,
        db: &mut TxStorageTransaction<T>,
    ) -> ExecutorResult<()>
    where
        T: KeyValueInspect<Column = Column>,
    {
        // Only insert a coin output if it has some amount.
        // This is because variable or transfer outputs won't have any value
        // if there's a revert or panic and shouldn't be added to the utxo set.
        if *amount > Word::MIN {
            let coin = CompressedCoinV1 {
                owner: *to,
                amount: *amount,
                asset_id: *asset_id,
                tx_pointer: TxPointer::new(block_height, execution_data.tx_count),
            }
            .into();

            if db.storage::<Coins>().replace(&utxo_id, &coin)?.is_some() {
                return Err(ExecutorError::OutputAlreadyExists)
            }
            execution_data
                .events
                .push(ExecutorEvent::CoinCreated(coin.uncompress(utxo_id)));
        }

        Ok(())
    }
}
