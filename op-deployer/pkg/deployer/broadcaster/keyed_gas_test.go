package broadcaster

import (
	"context"
	"errors"
	"math/big"
	"testing"

	"github.com/ethereum-optimism/optimism/op-chain-ops/script"
	"github.com/ethereum-optimism/optimism/op-service/testlog"
	ethereum "github.com/ethereum/go-ethereum"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/common/hexutil"
	"github.com/ethereum/go-ethereum/log"
	"github.com/holiman/uint256"
	"github.com/stretchr/testify/require"
)

type stubEstimator struct {
	gas     uint64
	err     error
	lastMsg ethereum.CallMsg
	calls   int
}

func (s *stubEstimator) EstimateGas(_ context.Context, msg ethereum.CallMsg) (uint64, error) {
	s.calls++
	s.lastMsg = msg
	return s.gas, s.err
}

func TestEstimatedGasLimit(t *testing.T) {
	lgr := testlog.Logger(t, log.LevelError)
	from := common.Address{'F'}
	const blockGasLimit = uint64(60_000_000)

	// A ~24KB deploy: op-deployer's in-process (Cancun) simulator recorded a low
	// GasUsed at 200 gas/byte, but the live chain charges EIP-8037's 1530 gas/byte
	// (~8x). The live estimate must win.
	create := script.Broadcast{
		Type:    script.BroadcastCreate,
		Input:   []byte{0x60, 0x00},
		Value:   (*hexutil.U256)(new(uint256.Int)),
		GasUsed: 5_000_000, // stale, far too low for a post-Amsterdam L1
	}
	simulated := padGasLimit(create.Input, create.GasUsed, true, blockGasLimit)

	t.Run("prefers the live estimate for creations", func(t *testing.T) {
		est := &stubEstimator{gas: 37_000_000}
		got := estimatedGasLimit(context.Background(), est, from, create, blockGasLimit, lgr)
		require.Equal(t, uint64(float64(37_000_000)*GasPadFactor), got)
		require.Greater(t, got, simulated, "live estimate must beat the stale simulated estimate")
		require.Equal(t, 1, est.calls)
		require.Equal(t, from, est.lastMsg.From)
		require.Nil(t, est.lastMsg.To, "plain CREATE has no recipient")
		require.Equal(t, []byte(create.Input), est.lastMsg.Data)
		require.Zero(t, est.lastMsg.Gas, "Gas must be unset so the node bounds by block gas limit, not the 2^24 per-tx cap")
	})

	t.Run("clamps to the block gas limit", func(t *testing.T) {
		est := &stubEstimator{gas: 58_000_000} // *1.2 = 69.6M > block gas limit
		got := estimatedGasLimit(context.Background(), est, from, create, blockGasLimit, lgr)
		require.Equal(t, blockGasLimit, got)
	})

	t.Run("falls back to the simulated estimate on error", func(t *testing.T) {
		est := &stubEstimator{err: errors.New("boom")}
		got := estimatedGasLimit(context.Background(), est, from, create, blockGasLimit, lgr)
		require.Equal(t, simulated, got)
	})

	t.Run("never drops below the simulated estimate", func(t *testing.T) {
		est := &stubEstimator{gas: 1} // padded ~1, far below simulated
		got := estimatedGasLimit(context.Background(), est, from, create, blockGasLimit, lgr)
		require.Equal(t, simulated, got)
	})

	t.Run("create2 targets the deterministic deployer with salt-prefixed data", func(t *testing.T) {
		c2 := script.Broadcast{
			Type:    script.BroadcastCreate2,
			Salt:    common.Hash{'S'},
			Input:   []byte{0xAA, 0xBB},
			Value:   (*hexutil.U256)(new(uint256.Int).SetUint64(7)),
			GasUsed: 100_000,
		}
		est := &stubEstimator{gas: 1_000_000}
		_ = estimatedGasLimit(context.Background(), est, from, c2, blockGasLimit, lgr)
		require.NotNil(t, est.lastMsg.To)
		require.Equal(t, script.DeterministicDeployerAddress, *est.lastMsg.To)
		require.Equal(t, append(append([]byte{}, c2.Salt[:]...), c2.Input...), est.lastMsg.Data)
		require.Equal(t, big.NewInt(7), est.lastMsg.Value)
	})

	// Calls are also live-estimated: op-deployer apply / manage migrate deploy a
	// chain's contracts via a CALL to OPCM that CREATEs internally, so the same
	// EIP-8037 under-charge applies.
	t.Run("live-estimates calls (e.g. an OPCM deploy) to the call target", func(t *testing.T) {
		to := common.Address{'O', 'P', 'C', 'M'}
		call := script.Broadcast{
			Type:    script.BroadcastCall,
			To:      to,
			Input:   []byte{0x12, 0x34},
			Value:   (*hexutil.U256)(new(uint256.Int)),
			GasUsed: 5_000_000, // stale simulated estimate
		}
		est := &stubEstimator{gas: 40_000_000}
		got := estimatedGasLimit(context.Background(), est, from, call, blockGasLimit, lgr)
		require.Equal(t, uint64(float64(40_000_000)*GasPadFactor), got)
		require.Equal(t, 1, est.calls)
		require.NotNil(t, est.lastMsg.To)
		require.Equal(t, to, *est.lastMsg.To)
		require.Equal(t, []byte(call.Input), est.lastMsg.Data)
	})

	t.Run("call falls back to simulated when live estimate fails (dependent tx)", func(t *testing.T) {
		call := script.Broadcast{
			Type:    script.BroadcastCall,
			To:      common.Address{'X'},
			Input:   []byte{0x12},
			Value:   (*hexutil.U256)(new(uint256.Int)),
			GasUsed: 250_000,
		}
		est := &stubEstimator{err: errors.New("execution reverted: dependency not deployed yet")}
		got := estimatedGasLimit(context.Background(), est, from, call, blockGasLimit, lgr)
		require.Equal(t, padGasLimit(call.Input, call.GasUsed, false, blockGasLimit), got)
	})
}
