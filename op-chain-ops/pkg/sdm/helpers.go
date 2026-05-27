package sdm

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"

	"github.com/ethereum/go-ethereum/common"
	"github.com/ethereum/go-ethereum/common/hexutil"
)

const SDMTxType = 0x7d

// StateBloatBin is a tiny contract with run(uint256 n), which writes n stable storage slots.
// Sending repeated calls to the same contract in one block warms the same account/slots and
// should produce SDM refund entries in an SDM-enabled op-reth devnet.
const StateBloatBin = "6080604052348015600e575f5ffd5b5060f28061001b5f395ff3fe6080604052348015600e575f5ffd5b50600436106026575f3560e01c8063a444f5e914602a575b5f5ffd5b60406004803603810190603c91906096565b6042565b005b5f5f90505b8181101560605760018101815580806001019150506047565b5050565b5f5ffd5b5f819050919050565b6078816068565b81146081575f5ffd5b50565b5f813590506090816071565b92915050565b5f6020828403121560a85760a76064565b5b5f60b3848285016084565b9150509291505056fea2646970667358221220fb9ef6750b6ac6ded2dd901595e50b6daefe24726b41a0346f3a36ac6fcf5f8264736f6c634300081c0033"

const runSelector = "\xa4\x44\xf5\xe9" // run(uint256)

// EncodeRun returns calldata for StateBloat.run(n).
func EncodeRun(n uint64) []byte {
	data := make([]byte, 4+32)
	copy(data, runSelector)
	binary.BigEndian.PutUint64(data[len(data)-8:], n)
	return data
}

// Caller is the subset shared by go-ethereum RPC clients and op-service RPC wrappers.
type Caller interface {
	CallContext(ctx context.Context, result any, method string, args ...any) error
}

// RPCTransaction is a minimal transaction object returned by eth_getBlockByNumber(..., true).
type RPCTransaction struct {
	Hash  common.Hash    `json:"hash"`
	Type  hexutil.Uint64 `json:"type"`
	Input hexutil.Bytes  `json:"input"`
}

// RPCBlock is a minimal block object returned by eth_getBlockByNumber(..., true).
type RPCBlock struct {
	Number       hexutil.Uint64   `json:"number"`
	Hash         common.Hash      `json:"hash"`
	GasUsed      hexutil.Uint64   `json:"gasUsed"`
	Transactions []RPCTransaction `json:"transactions"`
}

func GetBlockWithTxs(ctx context.Context, rpcClient Caller, blockNum uint64) (*RPCBlock, error) {
	var raw json.RawMessage
	if err := rpcClient.CallContext(ctx, &raw, "eth_getBlockByNumber", fmt.Sprintf("0x%x", blockNum), true); err != nil {
		return nil, fmt.Errorf("eth_getBlockByNumber(%d): %w", blockNum, err)
	}
	if len(raw) == 0 || string(raw) == "null" {
		return nil, fmt.Errorf("block %d not found", blockNum)
	}

	var block RPCBlock
	if err := json.Unmarshal(raw, &block); err != nil {
		return nil, fmt.Errorf("unmarshal block %d: %w", blockNum, err)
	}
	return &block, nil
}

func FindPostExecTransaction(block *RPCBlock) (*RPCTransaction, int) {
	for i := range block.Transactions {
		tx := &block.Transactions[i]
		if uint64(tx.Type) == SDMTxType {
			return tx, i
		}
	}
	return nil, -1
}
