//! Stateless OP Stack L2 block builder implementation.
//!
//! The [`StatelessL2Builder`] provides a complete block building and execution engine
//! for OP Stack L2 chains that operates in a stateless manner, pulling required state
//! data from a [`TrieDB`] during execution rather than maintaining full state.

use crate::{ExecutorError, ExecutorResult, TrieDB, TrieDBError, TrieDBProvider};
use alloc::{string::ToString, vec::Vec};
use alloy_consensus::{Header, Sealed, crypto::RecoveryError};
use alloy_evm::{
    EvmFactory, FromRecoveredTx, FromTxWithEncoded, RecoveredTx,
    block::{BlockExecutionResult, BlockExecutor, BlockExecutorFactory},
};
use alloy_op_evm::{
    OpBlockExecutionCtx, OpBlockExecutorFactory, PostExecMode,
    block::{OpAlloyReceiptBuilder, OpTxEnv},
};
use core::fmt::Debug;
use kona_genesis::RollupConfig;
use kona_mpt::TrieHinter;
use op_alloy_consensus::{
    OpReceiptEnvelope, OpTxEnvelope, parse_post_exec_payload_from_transactions,
};
use op_alloy_rpc_types_engine::OpPayloadAttributes;
use op_revm::OpSpecId;
use revm::{
    context::BlockEnv,
    database::{State, states::bundle_state::BundleRetention},
};

/// Stateless OP Stack L2 block builder that derives state from trie proofs during execution.
///
/// The [`StatelessL2Builder`] is a specialized block execution engine designed for fault proof
/// systems and stateless verification. Instead of maintaining full L2 state, it dynamically
/// retrieves required state data from a [`TrieDB`] backed by Merkle proofs and witnesses.
///
/// # Architecture
///
/// The builder operates in a stateless manner by:
/// 1. **Trie Database**: Uses [`TrieDB`] to access state via Merkle proofs
/// 2. **EVM Factory**: Creates execution environments with proof-backed state
/// 3. **Block Executor**: Executes transactions using witness-provided state
/// 4. **Receipt Generation**: Produces execution receipts and state commitments
///
/// # Stateless Execution Model
///
/// Traditional execution engines maintain full state databases, but the stateless model:
/// - Receives state witnesses containing only required data
/// - Verifies state access against Merkle proofs
/// - Executes transactions without persistent state storage
/// - Produces verifiable execution results and state commitments
///
/// # Use Cases
///
/// ## Fault Proof Systems
/// - Enables dispute resolution without full state replication
/// - Provides verifiable execution results for challenge games
/// - Supports optimistic rollup fraud proof generation
///
/// ## Stateless Verification
/// - Allows third parties to verify L2 blocks without full state
/// - Enables light clients to validate L2 execution
/// - Supports decentralized verification networks
///
/// # Performance Characteristics
///
/// - **Memory**: Lower memory usage than stateful execution (no full state)
/// - **I/O**: Higher I/O for proof verification and witness access
/// - **CPU**: Additional overhead for cryptographic proof verification
/// - **Determinism**: Guaranteed deterministic execution results
///
/// # Type Parameters
///
/// * `P` - Trie database provider implementing [`TrieDBProvider`]
/// * `H` - Trie hinter implementing [`TrieHinter`] for state access optimization
/// * `Evm` - EVM factory implementing [`EvmFactory`] for execution environment creation
#[derive(Debug)]
pub struct StatelessL2Builder<'a, P, H, Evm>
where
    P: TrieDBProvider,
    H: TrieHinter,
    Evm: EvmFactory,
{
    /// The rollup configuration containing chain parameters and activation heights.
    ///
    /// Provides access to network-specific parameters including gas limits,
    /// hard fork activation heights, and system addresses needed for proper
    /// L2 block execution and validation.
    pub(crate) config: &'a RollupConfig,
    /// The trie database providing stateless access to L2 state via Merkle proofs.
    ///
    /// The [`TrieDB`] serves as the primary interface for state access during
    /// execution, resolving account and storage queries using witness data
    /// and cryptographic proofs rather than a traditional state database.
    pub(crate) trie_db: TrieDB<P, H>,
    /// The block executor factory for creating OP Stack execution environments.
    ///
    /// This factory creates specialized OP Stack execution environments that
    /// understand OP-specific transaction types, system calls, and state
    /// management required for proper L2 block execution.
    pub(crate) factory: OpBlockExecutorFactory<OpAlloyReceiptBuilder, RollupConfig, Evm>,
    /// Test-only override for SDM activation while the fork is not yet scheduled.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) sdm_active_override: Option<bool>,
}

impl<'a, P, H, Evm> StatelessL2Builder<'a, P, H, Evm>
where
    P: TrieDBProvider + Debug,
    H: TrieHinter + Debug,
    Evm: EvmFactory<Spec = OpSpecId, BlockEnv = BlockEnv> + 'static,
    <Evm as EvmFactory>::Tx:
        FromTxWithEncoded<OpTxEnvelope> + FromRecoveredTx<OpTxEnvelope> + OpTxEnv,
    OpBlockExecutorFactory<OpAlloyReceiptBuilder, RollupConfig, Evm>: for<'b> BlockExecutorFactory<
            EvmFactory = Evm,
            ExecutionCtx<'b> = OpBlockExecutionCtx,
            Transaction = OpTxEnvelope,
            Receipt = OpReceiptEnvelope,
        >,
{
    /// Creates a new stateless L2 block builder instance.
    ///
    /// Initializes the builder with the necessary components for stateless block execution
    /// including the trie database, execution factory, and rollup configuration.
    ///
    /// # Arguments
    /// * `config` - Rollup configuration with chain parameters and activation heights
    /// * `evm_factory` - EVM factory for creating execution environments
    /// * `provider` - Trie database provider for state access
    /// * `hinter` - Trie hinter for optimizing state access patterns
    /// * `parent_header` - Sealed header of the parent block to build upon
    ///
    /// # Returns
    /// A new [`StatelessL2Builder`] ready for block building operations
    ///
    /// # Usage
    /// ```rust,ignore
    /// let builder = StatelessL2Builder::new(
    ///     &rollup_config,
    ///     evm_factory,
    ///     trie_provider,
    ///     trie_hinter,
    ///     parent_header,
    /// );
    /// ```
    pub fn new(
        config: &'a RollupConfig,
        evm_factory: Evm,
        provider: P,
        hinter: H,
        parent_header: Sealed<Header>,
    ) -> Self {
        let trie_db = TrieDB::new(parent_header, provider, hinter);
        let factory = OpBlockExecutorFactory::new(
            OpAlloyReceiptBuilder::default(),
            config.clone(),
            evm_factory,
        );
        Self {
            config,
            trie_db,
            factory,
            #[cfg(any(test, feature = "test-utils"))]
            sdm_active_override: None,
        }
    }

    /// Overrides SDM activation for tests and fixture tooling.
    #[cfg(any(test, feature = "test-utils"))]
    pub const fn set_sdm_active_override(&mut self, sdm_active_override: Option<bool>) {
        self.sdm_active_override = sdm_active_override;
    }

    /// Returns whether SDM is active at the given timestamp.
    const fn is_sdm_active(&self, timestamp: u64) -> bool {
        #[cfg(any(test, feature = "test-utils"))]
        if let Some(active) = self.sdm_active_override {
            return active;
        }

        self.config.is_sdm_active(timestamp)
    }

    /// Builds and executes a new L2 block using the provided payload attributes.
    ///
    /// This method performs the complete block building and execution process in a stateless
    /// manner, dynamically retrieving required state data via the trie database and producing
    /// a fully executed block with receipts and state commitments.
    ///
    /// # Arguments
    /// * `attrs` - Payload attributes containing transactions and block metadata
    ///
    /// # Returns
    /// * `Ok(BlockBuildingOutcome)` - Successfully built and executed block with receipts
    /// * `Err(ExecutorError)` - Block building or execution failure
    ///
    /// # Errors
    /// This method can fail due to various conditions:
    ///
    /// ## Input Validation Errors
    /// - [`ExecutorError::MissingGasLimit`]: Gas limit not provided in attributes
    /// - [`ExecutorError::MissingTransactions`]: Transaction list not provided
    /// - [`ExecutorError::MissingEIP1559Params`]: Required fee parameters missing (post-Holocene)
    /// - [`ExecutorError::MissingParentBeaconBlockRoot`]: Beacon root missing (post-Dencun)
    ///
    /// ## Execution Errors
    /// - [`ExecutorError::BlockGasLimitExceeded`]: Cumulative gas exceeds block limit
    /// - [`ExecutorError::UnsupportedTransactionType`]: Unknown transaction type encountered
    /// - [`ExecutorError::ExecutionError`]: EVM-level execution failures
    ///
    /// ## State Access Errors
    /// - [`ExecutorError::TrieDBError`]: State tree access or proof verification failures
    /// - Missing account data in witness
    /// - Invalid Merkle proofs
    ///
    /// ## Data Integrity Errors
    /// - [`ExecutorError::Recovery`]: Transaction signature recovery failures
    /// - [`ExecutorError::RLPError`]: Data encoding/decoding errors
    ///
    /// # Block Building Process
    ///
    /// The block building process follows these steps:
    ///
    /// 1. **Environment Setup**: Configure EVM environment with proper gas settings
    /// 2. **Witness Hinting**: Send payload witness hints to optimize state access
    /// 3. **Transaction Execution**: Execute each transaction in order with state updates
    /// 4. **Receipt Generation**: Generate execution receipts for all transactions
    /// 5. **State Commitment**: Compute final state roots and output commitments
    /// 6. **Block Assembly**: Assemble complete block with header and execution results
    ///
    /// # Stateless Execution Details
    ///
    /// Unlike traditional execution engines, this builder:
    /// - Resolves state access via Merkle proofs instead of database lookups
    /// - Validates all state access against cryptographic witnesses
    /// - Produces deterministic results independent of execution environment
    /// - Enables verification without full state replication
    ///
    /// # Performance Considerations
    ///
    /// - State access latency depends on proof verification overhead
    /// - Memory usage scales with witness size rather than full state
    /// - CPU overhead from cryptographic proof verification
    /// - I/O patterns optimized through trie hinter guidance
    pub fn build_block(
        &mut self,
        attrs: OpPayloadAttributes,
    ) -> ExecutorResult<BlockBuildingOutcome> {
        // Step 1. Set up the execution environment.
        let (base_fee_params, min_base_fee) = Self::active_base_fee_params(
            self.config,
            self.trie_db.parent_block_header(),
            attrs.payload_attributes.timestamp,
        )?;
        let evm_env = self.evm_env(
            self.config.spec_id(attrs.payload_attributes.timestamp),
            self.trie_db.parent_block_header(),
            &attrs,
            &base_fee_params,
            min_base_fee,
        )?;
        let block_env = evm_env.block_env().clone();
        let parent_hash = self.trie_db.parent_block_header().seal();

        // Attempt to send a payload witness hint to the host. This hint instructs the host to
        // populate its preimage store with the preimages required to statelessly execute
        // this payload. This feature is experimental, so if the hint fails, we continue
        // without it and fall back on on-demand preimage fetching for execution.
        self.trie_db
            .hinter
            .hint_execution_witness(parent_hash, &attrs)
            .map_err(|e| TrieDBError::Provider(e.to_string()))?;

        info!(
            target: "block_builder",
            block_number = %block_env.number,
            block_timestamp = %block_env.timestamp,
            block_gas_limit = block_env.gas_limit,
            transactions = attrs.transactions.as_ref().map_or(0, |txs| txs.len()),
            "Beginning block building."
        );

        // Compute SDM activation before borrowing `self.trie_db` mutably below.
        let sdm_active = self.is_sdm_active(block_env.timestamp.saturating_to());

        // Step 2. Create the executor, using the trie database.
        let mut state =
            State::builder().with_database(&mut self.trie_db).with_bundle_update().build();
        let evm = self.factory.evm_factory().create_evm(&mut state, evm_env);
        // Step 3. Decode and validate the block transactions within the payload attributes.
        let transactions = attrs
            .recovered_transactions_with_encoded()
            .collect::<Result<Vec<_>, RecoveryError>>()
            .map_err(ExecutorError::Recovery)?;
        let post_exec_mode = parse_post_exec_payload_from_transactions(
            transactions.iter().map(RecoveredTx::tx),
            block_env.number.saturating_to(),
            sdm_active,
        )
        .map_err(|err| ExecutorError::InvalidPostExecPayload(err.into_string()))?
        .map(|parsed| PostExecMode::Verify(parsed.payload))
        .unwrap_or_default();

        let ctx = OpBlockExecutionCtx {
            parent_hash,
            parent_beacon_block_root: attrs.payload_attributes.parent_beacon_block_root,
            // This field is unused for individual block building jobs.
            extra_data: Default::default(),
            post_exec_mode,
        };
        let executor = self.factory.create_executor(evm, ctx);

        let ex_result = executor.execute_block(transactions.iter())?;

        info!(
            target: "block_builder",
            gas_used = ex_result.gas_used,
            gas_limit = block_env.gas_limit,
            "Finished block building. Beginning sealing job."
        );

        // Step 4. Merge state transitions and seal the block.
        state.merge_transitions(BundleRetention::Reverts);
        let bundle = state.take_bundle();
        let header = self.seal_block(&attrs, parent_hash, &block_env, &ex_result, bundle)?;

        info!(
            target: "block_builder",
            number = header.number,
            hash = ?header.seal(),
            state_root = ?header.state_root,
            transactions_root = ?header.transactions_root,
            receipts_root = ?header.receipts_root,
            "Sealed new block",
        );

        // Update the parent block hash in the state database, preparing for the next block.
        self.trie_db.set_parent_block_header(header.clone());
        Ok((header, ex_result).into())
    }
}

/// The outcome of a block building operation, returning the sealed block [`Header`] and the
/// [`BlockExecutionResult`].
#[derive(Debug, Clone)]
pub struct BlockBuildingOutcome {
    /// The block header.
    pub header: Sealed<Header>,
    /// The block execution result.
    pub execution_result: BlockExecutionResult<OpReceiptEnvelope>,
}

impl From<(Sealed<Header>, BlockExecutionResult<OpReceiptEnvelope>)> for BlockBuildingOutcome {
    fn from(
        (header, execution_result): (Sealed<Header>, BlockExecutionResult<OpReceiptEnvelope>),
    ) -> Self {
        Self { header, execution_result }
    }
}

#[cfg(test)]
mod test {
    use crate::{
        ExecutorError,
        test_utils::{execute_loaded_fixture, load_test_fixture, run_test_fixture},
    };
    use alloy_consensus::Header;
    use alloy_eips::Encodable2718;
    use op_alloy_consensus::{OpReceiptEnvelope, SDMGasEntry, build_post_exec_tx};
    use rstest::rstest;
    use std::path::PathBuf;

    /// Path to the fixture used by all post-exec tests.
    ///
    /// The chosen fixture must contain a regular (non-deposit, non-post-exec) tx at index 1, since
    /// several tests target that index when constructing payload entries.
    fn post_exec_fixture_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("testdata/block-26207960.tar.gz")
    }

    fn fixture_block_number(parent_header: &Header) -> u64 {
        parent_header.number + 1
    }

    fn append_post_exec_tx(
        transactions: &mut Vec<alloy_primitives::Bytes>,
        block_number: u64,
        gas_refund_entries: Vec<SDMGasEntry>,
    ) {
        let tx = build_post_exec_tx(block_number, gas_refund_entries);
        let mut encoded = Vec::with_capacity(tx.eip2718_encoded_length());
        tx.encode_2718(&mut encoded);
        transactions.push(encoded.into());
    }

    /// Asserts that `err` is a post-exec validation failure containing `expected`.
    ///
    /// Matches both the parser-level [`ExecutorError::InvalidPostExecPayload`] and the
    /// execution-level `OpBlockExecutionError::InvalidPostExecPayload` wrapped in
    /// [`ExecutorError::ExecutionError`], since both render with the substring
    /// `"invalid post-exec payload"`.
    fn assert_post_exec_validation_failure(err: ExecutorError, expected: &str) {
        let err = err.to_string();
        assert!(
            err.to_lowercase().contains("invalid post-exec payload"),
            "unexpected error: {err}"
        );
        assert!(err.contains(expected), "expected {err:?} to contain {expected:?}");
    }

    #[rstest]
    #[tokio::test]
    async fn test_statelessly_execute_block(
        #[base_dir = "./testdata"]
        #[files("*.tar.gz")]
        path: PathBuf,
    ) {
        run_test_fixture(path).await;
    }

    /// Verifies the default fallthrough: with no override, [`StatelessL2Builder`] consults the
    /// rollup config, where SDM is currently unscheduled and reports inactive.
    #[tokio::test]
    async fn post_exec_sdm_inherit_rejects_post_exec_tx() {
        let mut loaded = load_test_fixture(post_exec_fixture_path()).await;
        let block_number = fixture_block_number(&loaded.fixture.parent_header);
        append_post_exec_tx(
            loaded.fixture.executing_payload.transactions.as_mut().unwrap(),
            block_number,
            Vec::new(),
        );

        let err = execute_loaded_fixture(loaded, None).unwrap_err();
        assert_post_exec_validation_failure(err, "SDM not active");
    }

    /// Verifies the explicit-override deactivation path. Pairs with
    /// [`post_exec_sdm_inherit_rejects_post_exec_tx`] above, which exercises the inherit branch.
    #[tokio::test]
    async fn post_exec_sdm_forced_inactive_rejects_appended_post_exec_tx() {
        let mut loaded = load_test_fixture(post_exec_fixture_path()).await;
        let block_number = fixture_block_number(&loaded.fixture.parent_header);
        append_post_exec_tx(
            loaded.fixture.executing_payload.transactions.as_mut().unwrap(),
            block_number,
            Vec::new(),
        );

        let err = execute_loaded_fixture(loaded, Some(false)).unwrap_err();
        assert_post_exec_validation_failure(err, "SDM not active");
    }

    #[tokio::test]
    async fn post_exec_sdm_enabled_rejects_wrong_block_number() {
        let mut loaded = load_test_fixture(post_exec_fixture_path()).await;
        let block_number = fixture_block_number(&loaded.fixture.parent_header);
        append_post_exec_tx(
            loaded.fixture.executing_payload.transactions.as_mut().unwrap(),
            block_number + 1,
            Vec::new(),
        );

        let err = execute_loaded_fixture(loaded, Some(true)).unwrap_err();
        assert_post_exec_validation_failure(err, "does not match block number");
    }

    #[tokio::test]
    async fn post_exec_sdm_enabled_rejects_duplicate_post_exec_txs() {
        let mut loaded = load_test_fixture(post_exec_fixture_path()).await;
        let block_number = fixture_block_number(&loaded.fixture.parent_header);
        let transactions = loaded.fixture.executing_payload.transactions.as_mut().unwrap();
        append_post_exec_tx(transactions, block_number, Vec::new());
        append_post_exec_tx(transactions, block_number, Vec::new());

        let err = execute_loaded_fixture(loaded, Some(true)).unwrap_err();
        assert_post_exec_validation_failure(err, "multiple post-exec transactions");
    }

    #[tokio::test]
    async fn post_exec_valid_empty_payload_executes_without_state_or_gas_change() {
        let baseline =
            execute_loaded_fixture(load_test_fixture(post_exec_fixture_path()).await, None)
                .expect("baseline fixture must execute");

        let mut loaded = load_test_fixture(post_exec_fixture_path()).await;
        let block_number = fixture_block_number(&loaded.fixture.parent_header);
        append_post_exec_tx(
            loaded.fixture.executing_payload.transactions.as_mut().unwrap(),
            block_number,
            Vec::new(),
        );

        let outcome =
            execute_loaded_fixture(loaded, Some(true)).expect("post-exec fixture executes");
        assert_eq!(
            outcome.execution_result.receipts.len(),
            baseline.execution_result.receipts.len() + 1
        );
        assert!(matches!(
            outcome.execution_result.receipts.last(),
            Some(OpReceiptEnvelope::PostExec(_))
        ));
        assert_eq!(outcome.execution_result.gas_used, baseline.execution_result.gas_used);
        assert_eq!(outcome.header.state_root, baseline.header.state_root);
        assert_ne!(outcome.header.transactions_root, baseline.header.transactions_root);
        assert_ne!(outcome.header.receipts_root, baseline.header.receipts_root);
    }

    #[tokio::test]
    async fn post_exec_payload_rejects_deposit_target() {
        let mut loaded = load_test_fixture(post_exec_fixture_path()).await;
        let block_number = fixture_block_number(&loaded.fixture.parent_header);
        append_post_exec_tx(
            loaded.fixture.executing_payload.transactions.as_mut().unwrap(),
            block_number,
            vec![SDMGasEntry { index: 0, gas_refund: 1 }],
        );

        let err = execute_loaded_fixture(loaded, Some(true)).unwrap_err();
        assert_post_exec_validation_failure(err, "payload entry targets deposit tx index 0");
    }

    #[tokio::test]
    async fn post_exec_payload_rejects_post_exec_target() {
        let mut loaded = load_test_fixture(post_exec_fixture_path()).await;
        let block_number = fixture_block_number(&loaded.fixture.parent_header);
        let post_exec_index =
            loaded.fixture.executing_payload.transactions.as_ref().unwrap().len() as u64;
        append_post_exec_tx(
            loaded.fixture.executing_payload.transactions.as_mut().unwrap(),
            block_number,
            vec![SDMGasEntry { index: post_exec_index, gas_refund: 1 }],
        );

        let err = execute_loaded_fixture(loaded, Some(true)).unwrap_err();
        assert_post_exec_validation_failure(
            err,
            &format!("payload entry targets post-exec tx index {post_exec_index}"),
        );
    }

    #[tokio::test]
    async fn post_exec_payload_rejects_duplicate_entries() {
        let mut loaded = load_test_fixture(post_exec_fixture_path()).await;
        let block_number = fixture_block_number(&loaded.fixture.parent_header);
        append_post_exec_tx(
            loaded.fixture.executing_payload.transactions.as_mut().unwrap(),
            block_number,
            vec![SDMGasEntry { index: 1, gas_refund: 1 }, SDMGasEntry { index: 1, gas_refund: 2 }],
        );

        let err = execute_loaded_fixture(loaded, Some(true)).unwrap_err();
        assert_post_exec_validation_failure(
            err,
            "duplicate post-exec payload entry for tx index 1",
        );
    }

    #[tokio::test]
    async fn post_exec_payload_rejects_unconsumed_entry() {
        let mut loaded = load_test_fixture(post_exec_fixture_path()).await;
        let block_number = fixture_block_number(&loaded.fixture.parent_header);
        let out_of_range_index =
            loaded.fixture.executing_payload.transactions.as_ref().unwrap().len() as u64 + 1;
        append_post_exec_tx(
            loaded.fixture.executing_payload.transactions.as_mut().unwrap(),
            block_number,
            vec![SDMGasEntry { index: out_of_range_index, gas_refund: 1 }],
        );

        let err = execute_loaded_fixture(loaded, Some(true)).unwrap_err();
        assert_post_exec_validation_failure(err, "unconsumed post-exec payload entries");
    }

    #[tokio::test]
    async fn post_exec_payload_rejects_refund_exceeding_gas_used() {
        let mut loaded = load_test_fixture(post_exec_fixture_path()).await;
        let block_number = fixture_block_number(&loaded.fixture.parent_header);
        append_post_exec_tx(
            loaded.fixture.executing_payload.transactions.as_mut().unwrap(),
            block_number,
            vec![SDMGasEntry { index: 1, gas_refund: u64::MAX }],
        );

        let err = execute_loaded_fixture(loaded, Some(true)).unwrap_err();
        assert_post_exec_validation_failure(err, "exceeds evm_gas_used");
    }
}
