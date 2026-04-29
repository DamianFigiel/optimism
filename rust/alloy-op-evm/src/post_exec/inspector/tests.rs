use super::*;
use crate::post_exec::{BlockWarmingPolicy, SdmPolicy, SdmTxContext, SdmTxOutcome};
use alloy_primitives::{Bytes, address, b256};
use revm::context::result::{ExecutionResult, Output, ResultGas, SuccessReason};

const ACCOUNT_A: Address = address!("00000000000000000000000000000000000000aa");
const ACCOUNT_B: Address = address!("00000000000000000000000000000000000000bb");
const SLOT_1: B256 = b256!("0000000000000000000000000000000000000000000000000000000000000001");

fn account_trace(addr: Address, refund_eligible: bool) -> PostExecTxTrace {
    let mut insp = PostExecTraceInspector::default();
    insp.begin_tx(PostExecTxContext { tx_index: 0, kind: PostExecTxKind::Normal });
    insp.observe_account_touch(addr, refund_eligible);
    insp.finish_tx().trace
}

fn slot_trace(addr: Address, slot: B256, is_sstore: bool) -> PostExecTxTrace {
    let mut insp = PostExecTraceInspector::default();
    insp.begin_tx(PostExecTxContext { tx_index: 0, kind: PostExecTxKind::Normal });
    insp.observe_slot_touch(addr, slot, is_sstore);
    insp.finish_tx().trace
}

fn dummy_result() -> ExecutionResult<()> {
    ExecutionResult::Success {
        reason: SuccessReason::Stop,
        gas: ResultGas::default(),
        logs: vec![],
        output: Output::Call(Bytes::default()),
    }
}

fn warming_refund(
    policy: &mut BlockWarmingPolicy,
    kind: PostExecTxKind,
    trace: &PostExecTxTrace,
) -> u64 {
    let result = dummy_result();
    let tx = SdmTxContext { tx_index: 0, kind, sender: Address::ZERO };
    let outcome = SdmTxOutcome { evm_gas_used: 100_000, result: &result, already_refunded: 0 };
    policy.refund_for_tx(&tx, trace, &outcome)
}

#[test]
fn trace_records_first_account_touch() {
    let trace = account_trace(ACCOUNT_A, true);

    assert!(trace.touched_accounts.contains(&ACCOUNT_A));
    assert_eq!(
        trace.account_touches,
        vec![PostExecAccountTouch { address: ACCOUNT_A, refund_eligible: true }]
    );
}

#[test]
fn trace_records_storage_touch_without_account_refund_eligibility() {
    let trace = slot_trace(ACCOUNT_A, SLOT_1, true);

    assert!(trace.touched_accounts.contains(&ACCOUNT_A));
    assert!(trace.touched_slots.contains(&(ACCOUNT_A, SLOT_1)));
    assert_eq!(
        trace.account_touches,
        vec![PostExecAccountTouch { address: ACCOUNT_A, refund_eligible: false }]
    );
    assert_eq!(
        trace.slot_touches,
        vec![PostExecSlotTouch { address: ACCOUNT_A, slot: SLOT_1, is_sstore: true }]
    );
}

#[test]
fn block_warming_repeated_account_touch_refunds_once() {
    let mut policy = BlockWarmingPolicy::default();
    let trace = account_trace(ACCOUNT_A, true);

    assert_eq!(warming_refund(&mut policy, PostExecTxKind::Normal, &trace), 0);
    assert_eq!(warming_refund(&mut policy, PostExecTxKind::Normal, &trace), ACCOUNT_REWARM_REFUND,);
}

#[test]
fn block_warming_repeated_storage_refunds_without_account_double_count() {
    for (is_sstore, expected) in [(false, SLOAD_REWARM_REFUND), (true, SSTORE_REWARM_REFUND)] {
        let mut policy = BlockWarmingPolicy::default();
        let trace = slot_trace(ACCOUNT_A, SLOT_1, is_sstore);

        assert_eq!(warming_refund(&mut policy, PostExecTxKind::Normal, &trace), 0);
        assert_eq!(warming_refund(&mut policy, PostExecTxKind::Normal, &trace), expected);
    }
}

#[test]
fn block_warming_deposit_warms_but_does_not_claim() {
    let mut policy = BlockWarmingPolicy::default();
    let trace = account_trace(ACCOUNT_B, true);

    assert_eq!(warming_refund(&mut policy, PostExecTxKind::Deposit, &trace), 0);
    assert_eq!(warming_refund(&mut policy, PostExecTxKind::Normal, &trace), ACCOUNT_REWARM_REFUND,);
}

#[test]
fn intrinsic_access_list_warmth_does_not_claim() {
    let mut policy = BlockWarmingPolicy::default();
    let mut trace = slot_trace(ACCOUNT_A, SLOT_1, false);
    trace.intrinsic_warm_accounts.insert(ACCOUNT_A);
    trace.intrinsic_warm_slots.insert((ACCOUNT_A, SLOT_1));

    assert_eq!(warming_refund(&mut policy, PostExecTxKind::Normal, &trace), 0);

    let trace = slot_trace(ACCOUNT_A, SLOT_1, false);
    assert_eq!(warming_refund(&mut policy, PostExecTxKind::Normal, &trace), SLOAD_REWARM_REFUND);
}

#[test]
fn take_last_tx_result_round_trips() {
    let mut insp = PostExecTraceInspector::default();

    insp.begin_tx(PostExecTxContext { tx_index: 0, kind: PostExecTxKind::Normal });
    insp.observe_account_touch(ACCOUNT_A, true);
    let _ = insp.finish_tx();

    assert!(insp.take_last_tx_result().trace.touched_accounts.contains(&ACCOUNT_A));
    assert!(insp.take_last_tx_result().trace.touched_accounts.is_empty());
}
