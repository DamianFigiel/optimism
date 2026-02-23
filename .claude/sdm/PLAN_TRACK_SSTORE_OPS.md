# Op-geth: Zero OPGas Refund for Storage-Heavy Transactions

## Context

The SDM PoC1 benchmark confirmed that SSTORE-heavy transactions receive disproportionately large OPGas refunds under the wall-clock model (state_bloat: 86.4% vs compute_heavy: 60.1%). Storage writes are fast in wall-clock (buffered in memory) but consume high EVM gas, so the refund makes state growth artificially cheap.

**Fix**: Track SSTORE operations during EVM execution and zero the OPGas refund when a transaction exceeds storage thresholds.

**Thresholds** (both configurable, either triggers zero refund):
1. SSTORE gas / total gas used > 50%
2. SSTORE count > 20

All changes are in **op-geth** at `/Users/nonsenseop/code/src/github.com/ethereum-optimism/op-geth/` (branch `nonsense/opgas-geth`).

---

## Files to Modify

### 1. `core/vm/evm.go` — Add SSTORE tracking fields to EVM struct

Add two fields to the `EVM` struct (after `returnData` at line 142):

```go
SstoreCount uint64 // Number of SSTORE operations in current tx
SstoreGas   uint64 // Cumulative gas charged for SSTORE in current tx
```

Reset both in `SetTxContext()` (line 225):

```go
func (evm *EVM) SetTxContext(txCtx TxContext) {
    if evm.chainRules.IsEIP4762 {
        txCtx.AccessEvents = state.NewAccessEvents(evm.StateDB.PointCache())
    }
    evm.TxContext = txCtx
    evm.SstoreCount = 0
    evm.SstoreGas = 0
}
```

### 2. `core/vm/instructions.go` — Increment SSTORE counter

In `opSstore()` (line 520), increment `evm.SstoreCount` after the `SetState` call:

```go
func opSstore(pc *uint64, evm *EVM, scope *ScopeContext) ([]byte, error) {
    if evm.readOnly {
        return nil, ErrWriteProtection
    }
    loc := scope.Stack.pop()
    val := scope.Stack.pop()
    evm.StateDB.SetState(scope.Contract.Address(), loc.Bytes32(), val.Bytes32())
    evm.SstoreCount++
    return nil, nil
}
```

### 3. `core/vm/gas_table.go` — Track SSTORE gas

In `gasSStore()` (line 99) and `gasSStoreEIP2200()` (around line 168), accumulate gas into `evm.SstoreGas` before returning. Wrap each return point — simplest approach is a helper closure at the top of each function:

```go
func gasSStore(evm *EVM, contract *Contract, stack *Stack, mem *Memory, memorySize uint64) (uint64, error) {
    // ... existing logic unchanged ...
    // At the end, before the final return, or: wrap with a deferred accumulator
}
```

**Preferred approach** — add a wrapper that captures the return value:

```go
// At the top of gasSStore, after the var block:
defer func() {
    // Note: we can't capture return values from defer easily.
}()
```

Actually, the cleanest approach: create a small wrapper function that both `gasSStore` and `gasSStoreEIP2200` call at the end. But since these functions have many return points, the most maintainable approach is to **track gas inside `opSstore` using the contract's gas delta**:

In `core/vm/interpreter.go`, the gas cost is already computed before calling the operation. The gas charged for each opcode is `cost` (computed at ~line 230 in `Run()`). But `opSstore` doesn't have direct access to this value.

**Simplest correct approach**: Track SSTORE gas in the `gasSStore`/`gasSStoreEIP2200` functions by adding `evm.SstoreGas += <return_value>` at each return point. There are ~6 return points in `gasSStore` and ~6 in `gasSStoreEIP2200`:

For `gasSStore` (line 99-166), add `evm.SstoreGas += X` before each `return X, nil`:
- Line 115: `evm.SstoreGas += params.SstoreSetGas`
- Line 118: `evm.SstoreGas += params.SstoreClearGas`
- Line 120: `evm.SstoreGas += params.SstoreResetGas`
- Line 140: `evm.SstoreGas += params.NetSstoreNoopGas`
- Line 149: `evm.SstoreGas += params.NetSstoreCleanGas`
- Line 165: `evm.SstoreGas += params.NetSstoreDirtyGas`

Similarly for `gasSStoreEIP2200` (all its return points).

### 4. `core/state_transition.go` — Check thresholds before granting refund

In `innerExecute()`, after computing `peakGasUsed` (line 653) and before the opGasRefund block (lines 655-667), add the storage check:

```go
peakGasUsed := st.gasUsed()

// Check if tx is storage-heavy; if so, zero the OPGas refund.
storageHeavy := false
if peakGasUsed > 0 && st.evm.SstoreGas > 0 {
    sstoreRatio := float64(st.evm.SstoreGas) / float64(peakGasUsed)
    if sstoreRatio > 0.5 { // TODO: make configurable
        storageHeavy = true
    }
}
if st.evm.SstoreCount > 20 { // TODO: make configurable
    storageHeavy = true
}

var opGasRefund uint64
if !storageHeavy {
    if st.evm.Context.OPContainer == nil &&
        st.evm.ChainConfig().ChainID != nil &&
        st.evm.ChainConfig().ChainID.Cmp(big.NewInt(900)) != 0 &&
        st.evm.ChainConfig().ChainID.Cmp(big.NewInt(11155111)) != 0 {
        if !st.msg.IsDepositTx {
            opgas := evmgasToOpgas(peakGasUsed, uint64(microseconds_used))
            opGasRefund = peakGasUsed - opgas
        }
    } else if msg.OPGasRefund != nil {
        opGasRefund = *msg.OPGasRefund
    }
}
st.state.AddRefund(opGasRefund)
```

---

## Threshold Configuration (future)

For now, hardcode the thresholds as constants in `state_transition.go`:

```go
const (
    opSstoreRatioThreshold = 0.5  // SSTORE gas / total gas
    opSstoreCountThreshold = 20   // max SSTORE operations
)
```

These can later be moved to `params.ChainConfig` or a fork-gated config if needed.

---

## Verification

```bash
cd /Users/nonsenseop/code/src/github.com/ethereum-optimism/op-geth

# 1. Build op-geth
go build ./...

# 2. Run existing tests
go test ./core/vm/... -count=1
go test ./core/... -run TestStateTransition -count=1

# 3. Re-run the SDM benchmark (from optimism repo)
cd /Users/nonsenseop/code/src/github.com/ethereum-optimism/optimism/op-acceptance-tests
SDM_BENCH_OUTPUT=/tmp/sdm_bench_v2.jsonl go test ./tests/sdm/ -run TestSDMBenchmark -v -count=1 -timeout 5m

# 4. Compare: state_bloat refund ratio should now be ~0% (zeroed)
grep '"type":"summary"' /tmp/sdm_bench_v2.jsonl

# 5. Visualize
python3 tests/sdm/visualize.py --input /tmp/sdm_bench_v2.jsonl --output /tmp/sdm_report_v2.png
```

**Expected results after fix**:
- `state_bloat`: refund ratio drops to 0% (50 SSTOREs > 20 threshold)
- `compute_heavy`: refund ratio stays ~60% (no SSTOREs)
- `eoa_transfer`: refund ratio stays ~97% (no SSTOREs)
- `event_emitter`: refund ratio stays ~93% (no SSTOREs)
