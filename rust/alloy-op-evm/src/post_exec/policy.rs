use alloc::{vec, vec::Vec};

use alloy_primitives::Address;
use revm::context::result::ExecutionResult;

use super::inspector::{
    ACCOUNT_REWARM_REFUND, PostExecSlotTouch, PostExecTxKind, PostExecTxTrace, SLOAD_REWARM_REFUND,
    SSTORE_REWARM_REFUND,
};
use alloy_primitives::map::HashSet;

/// Producer-side SDM policy configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SdmPolicyConfig {
    /// Block-level warming refund policy.
    BlockWarming,
    /// Refund a percentage of remaining gas for transactions touching a target contract.
    ContractRefund {
        /// Target contract address.
        target: Address,
        /// Refund in basis points (`10_000` = 100%).
        refund_bps: u16,
    },
}

/// Ordered set of producer-side SDM policies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SdmPolicySetConfig {
    /// Policies to apply in order.
    pub policies: Vec<SdmPolicyConfig>,
}

impl SdmPolicySetConfig {
    /// Creates an empty policy set.
    pub const fn empty() -> Self {
        Self { policies: Vec::new() }
    }

    /// Creates a policy set containing only the legacy block-warming policy.
    pub fn block_warming() -> Self {
        Self { policies: vec![SdmPolicyConfig::BlockWarming] }
    }

    /// Returns true when no policies are configured.
    pub const fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }
}

/// Producer-side transaction metadata available to SDM policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdmTxContext {
    /// Replay-local transaction index.
    pub tx_index: u64,
    /// Transaction classification.
    pub kind: PostExecTxKind,
    /// Transaction sender.
    pub sender: Address,
}

/// Per-policy transaction outcome context.
#[derive(Debug)]
pub struct SdmTxOutcome<'a, H> {
    /// Gas used by raw EVM execution, before SDM settlement.
    pub evm_gas_used: u64,
    /// Raw EVM execution result.
    pub result: &'a ExecutionResult<H>,
    /// Refund already emitted by earlier policies in the ordered set.
    pub already_refunded: u64,
}

/// Producer-side SDM refund policy.
pub trait SdmPolicy {
    /// Stable policy identifier for logs/debugging.
    fn id(&self) -> &'static str;

    /// Whether this policy needs EVM trace collection.
    fn needs_trace(&self) -> bool {
        true
    }

    /// Returns the refund to apply for this transaction.
    fn refund_for_tx<H>(
        &mut self,
        tx: &SdmTxContext,
        trace: &PostExecTxTrace,
        outcome: &SdmTxOutcome<'_, H>,
    ) -> u64;
}

/// Runtime policy engine that applies a configured ordered policy set.
#[derive(Debug, Clone, Default)]
pub struct SdmPolicyEngine {
    policies: Vec<SdmPolicyRuntime>,
}

impl SdmPolicyEngine {
    /// Builds a policy engine from configuration.
    pub fn new(config: SdmPolicySetConfig) -> Self {
        let policies = config.policies.into_iter().map(SdmPolicyRuntime::from).collect();
        Self { policies }
    }

    /// Returns true if any configured policy needs trace collection.
    pub fn needs_trace(&self) -> bool {
        self.policies.iter().any(SdmPolicyRuntime::needs_trace)
    }

    /// Returns true if no policies are configured.
    pub const fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// Computes the aggregate refund for a transaction, applying policies in order to the
    /// remaining gas after earlier policies.
    pub fn refund_for_tx<H>(
        &mut self,
        tx: &SdmTxContext,
        trace: &PostExecTxTrace,
        evm_gas_used: u64,
        result: &ExecutionResult<H>,
    ) -> u64 {
        let mut total = 0u64;

        for policy in &mut self.policies {
            let outcome = SdmTxOutcome { evm_gas_used, result, already_refunded: total };
            let refund = policy.refund_for_tx(tx, trace, &outcome);
            total = total.saturating_add(refund).min(evm_gas_used);
        }

        total
    }
}

/// Runtime policy enum.
#[derive(Debug, Clone)]
pub enum SdmPolicyRuntime {
    /// Block-level warming policy.
    BlockWarming(BlockWarmingPolicy),
    /// Contract percentage-refund policy.
    ContractRefund(ContractRefundPolicy),
}

impl From<SdmPolicyConfig> for SdmPolicyRuntime {
    fn from(value: SdmPolicyConfig) -> Self {
        match value {
            SdmPolicyConfig::BlockWarming => Self::BlockWarming(BlockWarmingPolicy::default()),
            SdmPolicyConfig::ContractRefund { target, refund_bps } => {
                Self::ContractRefund(ContractRefundPolicy::new(target, refund_bps))
            }
        }
    }
}

impl SdmPolicy for SdmPolicyRuntime {
    fn id(&self) -> &'static str {
        match self {
            Self::BlockWarming(policy) => policy.id(),
            Self::ContractRefund(policy) => policy.id(),
        }
    }

    fn needs_trace(&self) -> bool {
        match self {
            Self::BlockWarming(policy) => policy.needs_trace(),
            Self::ContractRefund(policy) => policy.needs_trace(),
        }
    }

    fn refund_for_tx<H>(
        &mut self,
        tx: &SdmTxContext,
        trace: &PostExecTxTrace,
        outcome: &SdmTxOutcome<'_, H>,
    ) -> u64 {
        match self {
            Self::BlockWarming(policy) => policy.refund_for_tx(tx, trace, outcome),
            Self::ContractRefund(policy) => policy.refund_for_tx(tx, trace, outcome),
        }
    }
}

/// Legacy SDM block-level warming policy.
#[derive(Debug, Clone, Default)]
pub struct BlockWarmingPolicy {
    warmed_accounts: HashSet<Address>,
    warmed_slots: HashSet<(Address, alloy_primitives::B256)>,
}

impl SdmPolicy for BlockWarmingPolicy {
    fn id(&self) -> &'static str {
        "block-warming"
    }

    fn refund_for_tx<H>(
        &mut self,
        tx: &SdmTxContext,
        trace: &PostExecTxTrace,
        _outcome: &SdmTxOutcome<'_, H>,
    ) -> u64 {
        let mut refund = 0u64;

        for touch in &trace.account_touches {
            if tx.kind.claims_refunds() &&
                touch.refund_eligible &&
                !trace.intrinsic_warm_accounts.contains(&touch.address) &&
                self.warmed_accounts.contains(&touch.address)
            {
                refund = refund.saturating_add(ACCOUNT_REWARM_REFUND);
            }
            self.warmed_accounts.insert(touch.address);
        }

        for PostExecSlotTouch { address, slot, is_sstore } in &trace.slot_touches {
            if tx.kind.claims_refunds() &&
                !trace.intrinsic_warm_slots.contains(&(*address, *slot)) &&
                self.warmed_slots.contains(&(*address, *slot))
            {
                refund = refund.saturating_add(if *is_sstore {
                    SSTORE_REWARM_REFUND
                } else {
                    SLOAD_REWARM_REFUND
                });
            }
            self.warmed_slots.insert((*address, *slot));
        }

        refund
    }
}

/// Percentage refund for transactions that touch a configured contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractRefundPolicy {
    target: Address,
    refund_bps: u16,
}

impl ContractRefundPolicy {
    /// Creates a new contract refund policy.
    pub const fn new(target: Address, refund_bps: u16) -> Self {
        Self { target, refund_bps }
    }
}

impl SdmPolicy for ContractRefundPolicy {
    fn id(&self) -> &'static str {
        "contract-refund"
    }

    fn refund_for_tx<H>(
        &mut self,
        tx: &SdmTxContext,
        trace: &PostExecTxTrace,
        outcome: &SdmTxOutcome<'_, H>,
    ) -> u64 {
        if !tx.kind.claims_refunds() || self.refund_bps == 0 {
            return 0;
        }

        if !trace.called_accounts.contains(&self.target) &&
            !trace.touched_accounts.contains(&self.target)
        {
            return 0;
        }

        let eligible = outcome.evm_gas_used.saturating_sub(outcome.already_refunded);
        ((eligible as u128).saturating_mul(u128::from(self.refund_bps)) / 10_000) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Bytes, address, b256};
    use revm::context::result::{ExecutionResult, Output, ResultGas, SuccessReason};

    const TARGET: Address = address!("000000000000000000000000000000000000c0de");
    const SLOT: alloy_primitives::B256 =
        b256!("0000000000000000000000000000000000000000000000000000000000000001");

    fn dummy_result() -> ExecutionResult<()> {
        ExecutionResult::Success {
            reason: SuccessReason::Stop,
            gas: ResultGas::default(),
            logs: vec![],
            output: Output::Call(Bytes::default()),
        }
    }

    fn normal_tx() -> SdmTxContext {
        SdmTxContext { tx_index: 0, kind: PostExecTxKind::Normal, sender: Address::ZERO }
    }

    fn trace_touching_target() -> PostExecTxTrace {
        let mut trace = PostExecTxTrace::default();
        trace.touched_accounts.insert(TARGET);
        trace
            .account_touches
            .push(super::super::PostExecAccountTouch { address: TARGET, refund_eligible: true });
        trace
    }

    #[test]
    fn contract_refund_applies_to_remaining_gas() {
        let result = dummy_result();
        let mut policy = ContractRefundPolicy::new(TARGET, 5_000);
        let outcome =
            SdmTxOutcome { evm_gas_used: 100_000, result: &result, already_refunded: 10_000 };

        assert_eq!(policy.refund_for_tx(&normal_tx(), &trace_touching_target(), &outcome), 45_000);
    }

    #[test]
    fn contract_refund_ignores_deposits_and_misses() {
        let result = dummy_result();
        let mut policy = ContractRefundPolicy::new(TARGET, 5_000);
        let outcome = SdmTxOutcome { evm_gas_used: 100_000, result: &result, already_refunded: 0 };
        let deposit = SdmTxContext { kind: PostExecTxKind::Deposit, ..normal_tx() };

        assert_eq!(policy.refund_for_tx(&deposit, &trace_touching_target(), &outcome), 0);
        assert_eq!(policy.refund_for_tx(&normal_tx(), &PostExecTxTrace::default(), &outcome), 0);
    }

    #[test]
    fn engine_orders_policy_refunds() {
        let result = dummy_result();
        let mut engine = SdmPolicyEngine::new(SdmPolicySetConfig {
            policies: vec![
                SdmPolicyConfig::BlockWarming,
                SdmPolicyConfig::ContractRefund { target: TARGET, refund_bps: 5_000 },
            ],
        });
        let mut trace = trace_touching_target();
        trace.touched_slots.insert((TARGET, SLOT));
        trace.slot_touches.push(PostExecSlotTouch {
            address: TARGET,
            slot: SLOT,
            is_sstore: false,
        });

        assert_eq!(engine.refund_for_tx(&normal_tx(), &trace, 100_000, &result), 50_000);
        assert_eq!(
            engine.refund_for_tx(&normal_tx(), &trace, 100_000, &result),
            ACCOUNT_REWARM_REFUND +
                SLOAD_REWARM_REFUND +
                (100_000 - ACCOUNT_REWARM_REFUND - SLOAD_REWARM_REFUND) / 2,
        );
    }
}
