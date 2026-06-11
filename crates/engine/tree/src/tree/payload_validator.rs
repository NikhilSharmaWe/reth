//! Block validation and execution logic extracted from [`super::EngineApiTreeHandler`].

use super::{metrics::EngineApiMetrics, TreeState};
use alloy_consensus::BlockHeader;
use alloy_primitives::{B256, U256};
use reth_blockchain_tree::error::InsertBlockErrorKindTwo;
use reth_chain_state::ExecutedBlock;
use reth_consensus::{ConsensusError, FullConsensus, PostExecutionInput};
use reth_engine_primitives::InvalidBlockHook;
use reth_errors::ProviderError;
use reth_evm::execute::BlockExecutorProvider;
use reth_primitives::{GotExpected, NodePrimitives, SealedBlockWithSenders, SealedHeader};
use reth_primitives_traits::Block;
use reth_provider::{
    providers::ConsistentDbView, BlockReader, DatabaseProviderFactory, ExecutionOutcome,
    HashedPostStateProvider, StateCommitmentProvider, StateProviderBox, StateReader,
};
use reth_revm::database::StateProviderDatabase;
use reth_trie::{updates::TrieUpdates, HashedPostState, TrieInput};
use reth_trie_parallel::root::{ParallelStateRoot, ParallelStateRootError};
use revm_primitives::EvmState;
use std::{fmt::Debug, sync::Arc, time::Instant};
use tracing::{debug, error, trace, warn};

/// Context providing access to tree state during block validation.
pub(crate) struct TreeCtx<'a, N: NodePrimitives> {
    tree_state: &'a TreeState<N>,
}

impl<'a, N: NodePrimitives> TreeCtx<'a, N> {
    /// Creates a new tree context.
    pub(crate) const fn new(tree_state: &'a TreeState<N>) -> Self {
        Self { tree_state }
    }
}

/// Shared block validation and execution logic used by the engine tree.
///
/// This type is responsible for validating a block against its parent, executing it, verifying
/// the state root, and producing an [`ExecutedBlock`].
///
/// Provider and executor are borrowed from [`super::EngineApiTreeHandler`] per-call to avoid
/// duplicating those handles.
pub(crate) struct BasicEngineValidatorInner<N: NodePrimitives> {
    consensus: Arc<dyn FullConsensus<N>>,
    invalid_block_hook: Box<dyn InvalidBlockHook<N>>,
}

impl<N: NodePrimitives> Debug for BasicEngineValidatorInner<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasicEngineValidatorInner")
            .field("consensus", &self.consensus)
            .field("invalid_block_hook", &format!("{:p}", self.invalid_block_hook))
            .finish()
    }
}

impl<N: NodePrimitives> BasicEngineValidatorInner<N> {
    /// Creates a new [`BasicEngineValidatorInner`].
    pub(crate) fn new(consensus: Arc<dyn FullConsensus<N>>) -> Self {
        Self { consensus, invalid_block_hook: Box::new(super::NoopInvalidBlockHook) }
    }

    /// Sets the invalid block hook.
    pub(crate) fn set_invalid_block_hook(
        &mut self,
        invalid_block_hook: Box<dyn InvalidBlockHook<N>>,
    ) {
        self.invalid_block_hook = invalid_block_hook;
    }

    /// Validate if block is correct and satisfies all the consensus rules that concern the header
    /// and block body itself.
    pub(crate) fn validate_block(
        &self,
        block: &SealedBlockWithSenders<N::Block>,
    ) -> Result<(), ConsensusError> {
        if let Err(e) = self.consensus.validate_header_with_total_difficulty(block, U256::MAX) {
            error!(
                target: "engine::tree",
                ?block,
                "Failed to validate total difficulty for block {}: {e}",
                block.hash()
            );
            return Err(e)
        }

        if let Err(e) = self.consensus.validate_header(block) {
            error!(target: "engine::tree", ?block, "Failed to validate header {}: {e}", block.hash());
            return Err(e)
        }

        if let Err(e) = self.consensus.validate_block_pre_execution(block) {
            error!(target: "engine::tree", ?block, "Failed to validate block {}: {e}", block.hash());
            return Err(e)
        }

        Ok(())
    }

    /// Validates the block against its parent, executes it, computes the state root, and returns
    /// an [`ExecutedBlock`].
    pub(crate) fn validate_and_execute_block<P, E>(
        &self,
        provider: &P,
        executor_provider: &E,
        block: SealedBlockWithSenders<N::Block>,
        state_provider: StateProviderBox,
        parent_block: SealedHeader<N::BlockHeader>,
        ctx: TreeCtx<'_, N>,
        persistence_not_in_progress: bool,
        metrics: &EngineApiMetrics,
    ) -> Result<ExecutedBlock<N>, InsertBlockErrorKindTwo>
    where
        P: DatabaseProviderFactory
            + BlockReader<Block = N::Block, Header = N::BlockHeader>
            + StateReader<Receipt = N::Receipt>
            + StateCommitmentProvider
            + HashedPostStateProvider
            + Clone
            + 'static,
        <P as DatabaseProviderFactory>::Provider:
            BlockReader<Block = N::Block, Header = N::BlockHeader>,
        E: BlockExecutorProvider<Primitives = N>,
    {
        if let Err(e) = self.consensus.validate_header_against_parent(&block, &parent_block) {
            warn!(target: "engine::tree", ?block, "Failed to validate header {} against parent: {e}", block.hash());
            return Err(e.into())
        }

        trace!(target: "engine::tree", block=?block.num_hash(), "Executing block");
        let executor = executor_provider.executor(StateProviderDatabase::new(&state_provider));

        let block_number = block.number();
        let sealed_block = Arc::new(block.block.clone());
        let block = block.unseal();

        let exec_time = Instant::now();

        // TODO: uncomment to use StateRootTask

        // let (state_root_handle, state_hook) = if persistence_not_in_progress {
        //     let consistent_view = ConsistentDbView::new_with_latest_tip(provider.clone())?;
        //
        //     let state_root_config = StateRootConfig::new_from_input(
        //         consistent_view.clone(),
        //         self.compute_trie_input(ctx, consistent_view, block.header().parent_hash())
        //             .map_err(ParallelStateRootError::into)?,
        //     );
        //
        //     let provider_ro = consistent_view.provider_ro()?;
        //     let nodes_sorted = state_root_config.nodes_sorted.clone();
        //     let state_sorted = state_root_config.state_sorted.clone();
        //     let prefix_sets = state_root_config.prefix_sets.clone();
        //     let blinded_provider_factory = ProofBlindedProviderFactory::new(
        //         InMemoryTrieCursorFactory::new(
        //             DatabaseTrieCursorFactory::new(provider_ro.tx_ref()),
        //             &nodes_sorted,
        //         ),
        //         HashedPostStateCursorFactory::new(
        //             DatabaseHashedCursorFactory::new(provider_ro.tx_ref()),
        //             &state_sorted,
        //         ),
        //         prefix_sets,
        //     );
        //
        //     let state_root_task = StateRootTask::new(state_root_config,
        // blinded_provider_factory);     let state_hook = state_root_task.state_hook();
        //     (Some(state_root_task.spawn(scope)), Box::new(state_hook) as Box<dyn OnStateHook>)
        // } else {
        //     (None, Box::new(|_state: &EvmState| {}) as Box<dyn OnStateHook>)
        // };
        let state_hook = Box::new(|_state: &EvmState| {});

        let output = metrics.executor.execute_metered(
            executor,
            (&block, U256::MAX).into(),
            state_hook,
        )?;

        trace!(target: "engine::tree", elapsed=?exec_time.elapsed(), ?block_number, "Executed block");

        if let Err(err) = self.consensus.validate_block_post_execution(
            &block,
            PostExecutionInput::new(&output.receipts, &output.requests),
        ) {
            self.invalid_block_hook.on_invalid_block(
                &parent_block,
                &block.seal_slow(),
                &output,
                None,
            );
            return Err(err.into())
        }

        let hashed_state = provider.hashed_post_state(&output.state);

        trace!(target: "engine::tree", block=?sealed_block.num_hash(), "Calculating block state root");
        let root_time = Instant::now();

        // We attempt to compute state root in parallel if we are currently not persisting anything
        // to database. This is safe, because the database state cannot change until we
        // finish parallel computation. It is important that nothing is being persisted as
        // we are computing in parallel, because we initialize a different database transaction
        // per thread and it might end up with a different view of the database.
        let state_root_result = if persistence_not_in_progress {
            // TODO: uncomment to use StateRootTask

            // if let Some(state_root_handle) = state_root_handle {
            //     match state_root_handle.wait_for_result() {
            //         Ok((task_state_root, task_trie_updates)) => {
            //             info!(
            //                 target: "engine::tree",
            //                 block = ?sealed_block.num_hash(),
            //                 ?task_state_root,
            //                 "State root task finished"
            //             );
            //         }
            //         Err(error) => {
            //             info!(target: "engine::tree", ?error, "Failed to wait for state root task
            // result");         }
            //     }
            // }

            match self.compute_state_root_parallel(
                provider,
                ctx,
                block.header().parent_hash(),
                &hashed_state,
            ) {
                Ok(result) => Some(result),
                Err(ParallelStateRootError::Provider(ProviderError::ConsistentView(error))) => {
                    debug!(target: "engine", %error, "Parallel state root computation failed consistency check, falling back");
                    None
                }
                Err(error) => return Err(InsertBlockErrorKindTwo::Other(Box::new(error))),
            }
        } else {
            None
        };

        let (state_root, trie_output) = if let Some(result) = state_root_result {
            result
        } else {
            debug!(target: "engine::tree", block=?sealed_block.num_hash(), ?persistence_not_in_progress, "Failed to compute state root in parallel");
            state_provider.state_root_with_updates(hashed_state.clone())?
        };

        if state_root != block.header().state_root() {
            self.invalid_block_hook.on_invalid_block(
                &parent_block,
                &block.clone().seal_slow(),
                &output,
                Some((&trie_output, state_root)),
            );
            return Err(ConsensusError::BodyStateRootDiff(
                GotExpected { got: state_root, expected: block.header().state_root() }.into(),
            )
            .into())
        }

        let root_elapsed = root_time.elapsed();
        metrics.block_validation.record_state_root(&trie_output, root_elapsed.as_secs_f64());
        debug!(target: "engine::tree", ?root_elapsed, block=?sealed_block.num_hash(), "Calculated state root");

        Ok(ExecutedBlock {
            block: sealed_block,
            senders: Arc::new(block.senders),
            execution_output: Arc::new(ExecutionOutcome::from((output, block_number))),
            hashed_state: Arc::new(hashed_state),
            trie: Arc::new(trie_output),
        })
    }

    /// Compute state root for the given hashed post state in parallel.
    ///
    /// # Returns
    ///
    /// Returns `Ok(_)` if computed successfully.
    /// Returns `Err(_)` if error was encountered during computation.
    /// `Err(ProviderError::ConsistentView(_))` can be safely ignored and fallback computation
    /// should be used instead.
    fn compute_state_root_parallel<P>(
        &self,
        provider: &P,
        ctx: TreeCtx<'_, N>,
        parent_hash: B256,
        hashed_state: &HashedPostState,
    ) -> Result<(B256, TrieUpdates), ParallelStateRootError>
    where
        P: DatabaseProviderFactory<Provider: BlockReader<Block = N::Block, Header = N::BlockHeader>>
            + StateCommitmentProvider
            + Clone
            + 'static,
    {
        let consistent_view = ConsistentDbView::new_with_latest_tip(provider.clone())?;

        let mut input = self.compute_trie_input(ctx, consistent_view.clone(), parent_hash)?;
        input.append_ref(hashed_state);

        ParallelStateRoot::new(consistent_view, input).incremental_root_with_updates()
    }

    /// Computes the trie input at the provided parent hash.
    fn compute_trie_input<P>(
        &self,
        ctx: TreeCtx<'_, N>,
        consistent_view: ConsistentDbView<P>,
        parent_hash: B256,
    ) -> Result<TrieInput, ParallelStateRootError>
    where
        P: DatabaseProviderFactory<Provider: BlockReader<Block = N::Block, Header = N::BlockHeader>>
            + StateCommitmentProvider
            + Clone
            + 'static,
    {
        let mut input = TrieInput::default();

        if let Some((historical, blocks)) = ctx.tree_state.blocks_by_hash(parent_hash) {
            debug!(target: "engine::tree", %parent_hash, %historical, "Parent found in memory");
            let revert_state = consistent_view.revert_state(historical)?;
            input.append(revert_state);

            for block in blocks.iter().rev() {
                input.append_cached_ref(block.trie_updates(), block.hashed_state())
            }
        } else {
            debug!(target: "engine::tree", %parent_hash, "Parent found on disk");
            let revert_state = consistent_view.revert_state(parent_hash)?;
            input.append(revert_state);
        }

        Ok(input)
    }
}
