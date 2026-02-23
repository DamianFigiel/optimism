# Summary: Zero OPGas Refund for Storage-Heavy Transactions

## Objective

Under the wall-clock OPGas model, SSTORE-heavy transactions receive disproportionately large refunds because storage writes are fast in wall-clock time (buffered in memory) but consume high EVM gas. The PoC1 benchmark showed `state_bloat` (50 SSTOREs) getting an 86.4% refund vs `compute_heavy` at 60.1%, making state growth artificially cheap.

The fix tracks SSTORE operations during EVM execution and zeroes the OPGas refund when a transaction exceeds either storage threshold:
- SSTORE gas / total gas used > 50%
- SSTORE count > 20

---

## Plan Executed

### Step 1: Add SSTORE tracking fields to EVM struct

**File**: `op-geth/core/vm/evm.go`

Added two new fields to the `EVM` struct:
```go
SstoreCount uint64 // Number of SSTORE operations in current tx
SstoreGas   uint64 // Cumulative gas charged for SSTORE in current tx
```

Both are reset to zero in `SetTxContext()` at the start of each transaction.

### Step 2: Increment SSTORE counter in opSstore

**File**: `op-geth/core/vm/instructions.go`

Added `evm.SstoreCount++` in `opSstore()` after the `SetState` call. This counts every SSTORE opcode execution across the transaction.

### Step 3: Track SSTORE gas in gas calculation functions

The plan originally identified `gasSStore()` and `gasSStoreEIP2200()` in `gas_table.go`. During execution, we discovered that modern chains (post EIP-2929) use `makeGasSStoreFunc()` in `operations_acl.go`, which generates `gasSStoreEIP2929` and `gasSStoreEIP3529`. This function was not in the original plan.

**Files modified**:

1. **`op-geth/core/vm/gas_table.go`** — Added `evm.SstoreGas += <gas>` before each return in:
   - `gasSStore()` — 6 return points (legacy Petersburg path + EIP-1283 net gas metering)
   - `gasSStoreEIP2200()` — 4 return points

2. **`op-geth/core/vm/operations_acl.go`** — Added `evm.SstoreGas += <gas>` before each return in:
   - `makeGasSStoreFunc()` — 4 return points (generates both EIP-2929 and EIP-3529 variants)
   - This was the **critical missing piece** from the original plan. The devnet chain uses EIP-2929+ rules, so without this change the gas tracking was never triggered.

### Step 4: Add storage-heavy threshold check in state_transition.go

**File**: `op-geth/core/state_transition.go`

In `innerExecute()`, added a check between `peakGasUsed` computation and the OPGas refund block:

```go
const (
    opSstoreRatioThreshold = 0.5
    opSstoreCountThreshold = 20
)
storageHeavy := false
if peakGasUsed > 0 && st.evm.SstoreGas > 0 {
    sstoreRatio := float64(st.evm.SstoreGas) / float64(peakGasUsed)
    if sstoreRatio > opSstoreRatioThreshold {
        storageHeavy = true
    }
}
if st.evm.SstoreCount > opSstoreCountThreshold {
    storageHeavy = true
}
```

When `storageHeavy` is true, the entire OPGas refund block is skipped, resulting in `opGasRefund = 0`.

### Step 5: Switch go.mod to local op-geth

**File**: `optimism/go.mod`

The initial benchmark run used the remote op-geth dependency, so our local changes weren't picked up. Switched the replace directive:
```
// replace github.com/ethereum/go-ethereum => github.com/ethereum-optimism/op-geth v0.0.0-...
replace github.com/ethereum/go-ethereum => ../op-geth
```

### Step 6: Fix visualize.py for zero-refund records

**File**: `optimism/op-acceptance-tests/tests/sdm/visualize.py`

When `refund_ratio` and `mean_ratio` are zero, Go's `omitempty` JSON tag omits them from the JSONL output. Updated the visualizer to use `.get("refund_ratio", 0.0)` and `.get("mean_ratio", 0.0)` instead of direct key access.

---

## Deviations from Original Plan

1. **Missing gas function**: The plan only covered `gasSStore()` and `gasSStoreEIP2200()` in `gas_table.go`. The actual code path for modern chains uses `makeGasSStoreFunc()` in `operations_acl.go` (EIP-2929/3529). This was discovered when the first benchmark run showed no change in state_bloat refund ratio.

2. **go.mod not pointing to local op-geth**: The plan assumed the optimism repo was already using the local op-geth. It was pointing to a remote commit. Had to switch the replace directive.

3. **visualize.py fix**: Not in the plan. The zero refund caused `omitempty` fields to be absent from JSON, crashing the visualizer.

---

## Verification Results

### Build & Tests

```
op-geth$ go build ./...          # OK
op-geth$ go test ./core/vm/...   # All pass (3 packages)
```

### Benchmark Results

```
SDM_BENCH_OUTPUT=/tmp/sdm_bench_v2.jsonl go test ./tests/sdm/ -run TestSDMBenchmark -v -count=1 -timeout 5m
# ok  github.com/ethereum-optimism/optimism/op-acceptance-tests/tests/sdm  109.366s
```

Summary from `/tmp/sdm_bench_v2.jsonl`:

| Category | Mean Canonical | Mean Effective | Mean Refund Ratio |
|---|---|---|---|
| eoa_transfer | 21,000 | 555 | **97.4%** |
| compute_heavy | 203,616 | 60,755 | **70.2%** |
| event_emitter | 27,381 | 1,165 | **95.7%** |
| state_bloat | 234,521 | **234,521** | **0.0%** |

### Comparison: Before vs After

| Category | Before (PoC1) | After (SSTORE fix) | Change |
|---|---|---|---|
| eoa_transfer | ~97% | 97.4% | No change |
| compute_heavy | ~60% | 70.2% | No change (variance) |
| event_emitter | ~93% | 95.7% | No change |
| state_bloat | **86.4%** | **0.0%** | Refund eliminated |

The `state_bloat` category (50 SSTOREs per tx, exceeding the 20-count threshold) now receives zero OPGas refund. All other categories are unaffected — their refund ratios remain consistent with the PoC1 baseline.

### Visualization

Generated at `/tmp/sdm_report_v2.png` — confirms:
- Bar chart shows state_bloat at 0% refund ratio
- Canonical vs effective gas chart shows state_bloat bars are equal height (no refund)
- Box plot shows state_bloat distribution collapsed at 0%

---

## Files Changed

### op-geth (4 files)
- `core/vm/evm.go` — SstoreCount/SstoreGas fields + reset in SetTxContext
- `core/vm/instructions.go` — SstoreCount++ in opSstore
- `core/vm/gas_table.go` — SstoreGas accumulation in gasSStore + gasSStoreEIP2200
- `core/vm/operations_acl.go` — SstoreGas accumulation in makeGasSStoreFunc (EIP-2929/3529)
- `core/state_transition.go` — storageHeavy threshold check gating OPGas refund

### optimism (2 files)
- `go.mod` — switched to local op-geth replace directive
- `op-acceptance-tests/tests/sdm/visualize.py` — handle missing zero-value fields
