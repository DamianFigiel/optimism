package broadcaster

import (
	"context"
	"errors"
	"fmt"
	"math/big"
	"sync"
	"time"

	"github.com/holiman/uint256"

	"github.com/ethereum-optimism/optimism/op-service/eth"

	"github.com/ethereum-optimism/optimism/op-chain-ops/script"
	opcrypto "github.com/ethereum-optimism/optimism/op-service/crypto"
	"github.com/ethereum-optimism/optimism/op-service/txmgr"
	"github.com/ethereum-optimism/optimism/op-service/txmgr/metrics"
	ethereum "github.com/ethereum/go-ethereum"
	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/core"
	"github.com/ethereum/go-ethereum/ethclient"
	"github.com/ethereum/go-ethereum/log"
)

const (
	GasPadFactor = 1.2
)

type KeyedBroadcaster struct {
	lgr    log.Logger
	mgr    txmgr.TxManager
	bcasts []script.Broadcast
	client *ethclient.Client
	mtx    sync.Mutex
}

type KeyedBroadcasterOpts struct {
	Logger  log.Logger
	ChainID *big.Int
	Client  *ethclient.Client
	Signer  opcrypto.SignerFn
	From    common.Address
}

func NewKeyedBroadcaster(cfg KeyedBroadcasterOpts) (*KeyedBroadcaster, error) {
	mgrCfg := &txmgr.Config{
		Backend:                   cfg.Client,
		ChainID:                   cfg.ChainID,
		TxSendTimeout:             5 * time.Minute,
		TxNotInMempoolTimeout:     time.Minute,
		NetworkTimeout:            10 * time.Second,
		ReceiptQueryInterval:      time.Second,
		NumConfirmations:          1,
		SafeAbortNonceTooLowCount: 3,
		Signer:                    cfg.Signer,
		From:                      cfg.From,
		GasPriceEstimatorFn:       DeployerGasPriceEstimator,
	}

	minTipCap, err := eth.GweiToWei(1.0)
	if err != nil {
		panic(err)
	}
	minBaseFee, err := eth.GweiToWei(1.0)
	if err != nil {
		panic(err)
	}

	mgrCfg.RebroadcastInterval.Store(int64(12 * time.Second))
	mgrCfg.ResubmissionTimeout.Store(int64(48 * time.Second))
	mgrCfg.FeeLimitMultiplier.Store(5)
	mgrCfg.FeeLimitThreshold.Store(big.NewInt(100))
	mgrCfg.MinTipCap.Store(minTipCap)
	mgrCfg.MinBaseFee.Store(minBaseFee)

	mgr, err := txmgr.NewSimpleTxManagerFromConfig(
		"transactor",
		cfg.Logger,
		&metrics.NoopTxMetrics{},
		mgrCfg,
	)
	if err != nil {
		return nil, fmt.Errorf("failed to create tx manager: %w", err)
	}

	return &KeyedBroadcaster{
		lgr:    cfg.Logger,
		mgr:    mgr,
		client: cfg.Client,
	}, nil
}

func (t *KeyedBroadcaster) Hook(bcast script.Broadcast) {
	if bcast.Type != script.BroadcastCreate2 && bcast.From != t.mgr.From() {
		panic(fmt.Sprintf("invalid from for broadcast:%v, expected:%v", bcast.From, t.mgr.From()))
	}
	t.mtx.Lock()
	t.bcasts = append(t.bcasts, bcast)
	t.mtx.Unlock()
}

func (t *KeyedBroadcaster) Broadcast(ctx context.Context) ([]BroadcastResult, error) {
	// Empty the internal broadcast buffer as soon as this method is called.
	t.mtx.Lock()
	bcasts := t.bcasts
	t.bcasts = nil
	t.mtx.Unlock()

	if len(bcasts) == 0 {
		return nil, nil
	}

	results := make([]BroadcastResult, len(bcasts))
	futures := make([]<-chan txmgr.SendResponse, len(bcasts))
	ids := make([]common.Hash, len(bcasts))

	latestBlock, err := t.client.BlockByNumber(ctx, nil)
	if err != nil {
		return nil, fmt.Errorf("failed to get latest block: %w", err)
	}

	for i, bcast := range bcasts {
		futures[i], ids[i] = t.broadcast(ctx, bcast, latestBlock.GasLimit())
		t.lgr.Info(
			"transaction broadcasted",
			"id", ids[i],
			"nonce", bcast.Nonce,
		)
	}

	var txErr error
	var completed int
	for i, fut := range futures {
		bcastRes := <-fut
		completed++
		outRes := BroadcastResult{
			Broadcast: bcasts[i],
		}

		if bcastRes.Err == nil {
			outRes.Receipt = bcastRes.Receipt
			outRes.TxHash = bcastRes.Receipt.TxHash

			if bcastRes.Receipt.Status == 0 {
				failErr := fmt.Errorf("transaction failed: %s", outRes.Receipt.TxHash.String())
				txErr = errors.Join(txErr, failErr)
				outRes.Err = failErr
				t.lgr.Error(
					"transaction failed on chain",
					"id", ids[i],
					"completed", completed,
					"total", len(bcasts),
					"hash", outRes.Receipt.TxHash.String(),
					"nonce", outRes.Broadcast.Nonce,
				)
			} else {
				t.lgr.Info(
					"transaction confirmed",
					"id", ids[i],
					"completed", completed,
					"total", len(bcasts),
					"hash", outRes.Receipt.TxHash.String(),
					"nonce", outRes.Broadcast.Nonce,
					"creation", outRes.Receipt.ContractAddress,
				)
			}
		} else {
			txErr = errors.Join(txErr, bcastRes.Err)
			outRes.Err = bcastRes.Err
			t.lgr.Error(
				"transaction failed",
				"id", ids[i],
				"completed", completed,
				"total", len(bcasts),
				"err", bcastRes.Err,
			)
		}

		results[i] = outRes
	}
	return results, txErr
}

func (t *KeyedBroadcaster) broadcast(ctx context.Context, bcast script.Broadcast, blockGasLimit uint64) (<-chan txmgr.SendResponse, common.Hash) {
	ch := make(chan txmgr.SendResponse, 1)

	id := bcast.ID()
	candidate := asTxCandidate(bcast, t.gasLimitFor(ctx, bcast, blockGasLimit))
	t.mgr.SendAsync(ctx, candidate, ch)
	return ch, id
}

// gasEstimator estimates the gas required for a message call against the target
// chain. *ethclient.Client satisfies it; tests use a stub.
type gasEstimator interface {
	EstimateGas(ctx context.Context, msg ethereum.CallMsg) (uint64, error)
}

// gasLimitFor computes the gas limit to use for a broadcast transaction by
// taking the larger of a live eth_estimateGas against the target chain and the
// simulation-derived estimate (bcast.GasUsed).
//
// Why the live estimate is needed: op-deployer's in-process script simulator
// runs a fixed Cancun-era gas schedule (see op-chain-ops/script), so the
// recorded bcast.GasUsed omits post-Cancun code-deposit repricing - in
// particular Glamsterdam's EIP-8037, which raises the code-deposit cost from 200
// to 1530 gas/byte (~8x). Against such a chain the simulated estimate is far too
// low and the transaction fails with "contract creation code storage out of
// gas". This bites any flow that deploys contract code, whether directly
// (bootstrap superchain/implementations, via CREATE/CREATE2) or indirectly
// through a contract that CREATEs internally (op-deployer apply / manage migrate,
// via a CALL to OPCM), so it must apply to all broadcast types.
//
// Why we keep the simulated value as a floor: a live estimate can legitimately
// fail or come back low for a broadcast that depends on state produced by an
// earlier, not-yet-mined transaction in the same bundle (the whole bundle is
// applied together in the simulator, but not yet on chain). Taking max(live,
// simulated) - and falling back to simulated on estimation error - means this is
// never worse than the previous simulation-only behaviour.
//
// The CallMsg intentionally leaves Gas unset so the node bounds its search by the
// block gas limit rather than the per-tx cap (params.MaxTxGas, 2^24), which is
// required for large post-Amsterdam deploys whose code deposit alone exceeds
// 2^24 gas.
func (t *KeyedBroadcaster) gasLimitFor(ctx context.Context, bcast script.Broadcast, blockGasLimit uint64) uint64 {
	return estimatedGasLimit(ctx, t.client, t.mgr.From(), bcast, blockGasLimit, t.lgr)
}

func estimatedGasLimit(ctx context.Context, est gasEstimator, from common.Address, bcast script.Broadcast, blockGasLimit uint64, lgr log.Logger) uint64 {
	creation := bcast.Type != script.BroadcastCall
	simulated := padGasLimit(bcast.Input, bcast.GasUsed, creation, blockGasLimit)

	msg := ethereum.CallMsg{From: from}
	switch bcast.Type {
	case script.BroadcastCall:
		to := bcast.To
		msg.To = &to
		msg.Data = bcast.Input
		msg.Value = ((*uint256.Int)(bcast.Value)).ToBig()
	case script.BroadcastCreate:
		msg.Data = bcast.Input
	case script.BroadcastCreate2:
		data := make([]byte, len(bcast.Salt)+len(bcast.Input))
		copy(data, bcast.Salt[:])
		copy(data[len(bcast.Salt):], bcast.Input)
		msg.To = &script.DeterministicDeployerAddress
		msg.Data = data
		msg.Value = ((*uint256.Int)(bcast.Value)).ToBig()
	default:
		return simulated
	}

	estimate, err := est.EstimateGas(ctx, msg)
	if err != nil {
		// Expected for broadcasts that depend on earlier, not-yet-mined bundle
		// txs (common for calls). Fall back to the simulated estimate. It is
		// logged more loudly for creations, where a fallback is more likely to
		// under-gas.
		if creation {
			lgr.Warn("live gas estimate for creation failed; using simulated estimate", "id", bcast.ID(), "err", err)
		} else {
			lgr.Debug("live gas estimate for call failed; using simulated estimate", "id", bcast.ID(), "err", err)
		}
		return simulated
	}

	limit := uint64(float64(estimate) * GasPadFactor)
	if limit > blockGasLimit {
		limit = blockGasLimit
	}
	// Never drop below the simulated estimate.
	if simulated > limit {
		limit = simulated
	}
	return limit
}

func asTxCandidate(bcast script.Broadcast, gasLimit uint64) txmgr.TxCandidate {
	value := ((*uint256.Int)(bcast.Value)).ToBig()
	var candidate txmgr.TxCandidate
	switch bcast.Type {
	case script.BroadcastCall:
		to := &bcast.To
		candidate = txmgr.TxCandidate{
			TxData:   bcast.Input,
			To:       to,
			Value:    value,
			GasLimit: gasLimit,
		}
	case script.BroadcastCreate:
		candidate = txmgr.TxCandidate{
			TxData:   bcast.Input,
			To:       nil,
			GasLimit: gasLimit,
		}
	case script.BroadcastCreate2:
		txData := make([]byte, len(bcast.Salt)+len(bcast.Input))
		copy(txData, bcast.Salt[:])
		copy(txData[len(bcast.Salt):], bcast.Input)

		candidate = txmgr.TxCandidate{
			TxData:   txData,
			To:       &script.DeterministicDeployerAddress,
			Value:    value,
			GasLimit: gasLimit,
		}
	default:
		panic(fmt.Sprintf("unrecognized broadcast type: '%s'", bcast.Type))
	}
	return candidate
}

// padGasLimit calculates the gas limit for a transaction based on the intrinsic gas and the gas used by
// the underlying call. Values are multiplied by a pad factor to account for any discrepancies. The output
// is clamped to the block gas limit since Geth will reject transactions that exceed it before letting them
// into the mempool.
func padGasLimit(data []byte, gasUsed uint64, creation bool, blockGasLimit uint64) uint64 {
	intrinsicGas, err := core.IntrinsicGas(data, nil, nil, creation, true, true, false)
	// This method never errors - we should look into it if it does.
	if err != nil {
		panic(err)
	}

	floorDataGas, err := core.FloorDataGas(data)
	// We should never cause an overflow here.
	if err != nil {
		panic(err)
	}

	gas := intrinsicGas + gasUsed
	if floorDataGas > gas {
		gas = floorDataGas
	}

	limit := uint64(float64(gas) * GasPadFactor)
	if limit > blockGasLimit {
		return blockGasLimit
	}
	return limit
}
