// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SafeERC20} from "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import {IV2Factory, IV2Pair, IV2Router} from "./interfaces/IV2.sol";

/// @title SellExecutor
/// @notice Constrained one-shot sell executor for V2-style pools.
///         The token owner approves this contract for a bounded amount and registers a task that pins
///         router, factory, path, recipient and a minimum-received floor. A separate executor account
///         may only trigger that exact sale. The contract has no admin, no upgrade path, no arbitrary
///         call and never keeps balances between transactions.
contract SellExecutor is ReentrancyGuard {
    using SafeERC20 for IERC20;

    enum Status {
        None,
        Open,
        Executed,
        Cancelled
    }

    /// RouterHop: owner -> this -> router -> pair (two token transfers; a transfer tax is charged twice).
    /// DirectToPair: owner -> pair, then low-level pair.swap (one transfer; advanced, single-hop only).
    enum Mode {
        RouterHop,
        DirectToPair
    }

    struct TaskParams {
        address executor;
        address tokenIn;
        address tokenOut;
        address router;
        address factory;
        address recipient;
        uint256 maxAmountIn;
        /// Minimum the recipient must actually receive when the full maxAmountIn is sold.
        /// Partial sells scale it pro rata (rounded up).
        uint256 minOutFloor;
        uint64 deadline;
        uint64 minBlock; // 0 = no lower bound
        uint64 maxBlock; // 0 = no upper bound
        /// Max tolerated loss (bps) on the owner's outgoing transfer. 0 = plain ERC-20 required.
        uint16 maxTransferLossBps;
        /// LP fee in bps, DirectToPair only (PancakeSwap V2 = 25).
        uint16 pairFeeBps;
        Mode mode;
        bool nativeOut; // RouterHop only: unwrap to native coin, tokenOut must be router.WETH()
        address[] path;
    }

    struct Task {
        address owner;
        Status status;
        TaskParams p;
    }

    uint256 public constant MAX_PATH = 4;
    uint16 public constant MAX_TRANSFER_LOSS_BPS = 3000;
    uint16 public constant MAX_PAIR_FEE_BPS = 100;

    mapping(bytes32 => Task) private _tasks;
    mapping(address => uint256) public nonces;

    event TaskCreated(bytes32 indexed taskId, address indexed owner, address indexed executor, address tokenIn);
    event TaskUpdated(bytes32 indexed taskId);
    event TaskCancelled(bytes32 indexed taskId);
    event TaskExecuted(bytes32 indexed taskId, uint256 amountIn, uint256 actualIn, uint256 received);

    error InvalidParams();
    error InvalidRouter();
    error InvalidPath();
    error NotOwner();
    error NotExecutor();
    error NotOpen();
    error Expired();
    error BlockOutOfRange();
    error AmountOutOfRange();
    error MinOutBelowFloor();
    error PairNotFound();
    error TransferLossTooHigh();
    error ResidualBalance();
    error InsufficientReceived();

    // ------------------------------------------------------------------ owner

    function createTask(TaskParams calldata p) external nonReentrant returns (bytes32 taskId) {
        _validate(p);
        taskId = keccak256(abi.encode(block.chainid, address(this), msg.sender, nonces[msg.sender]++));
        Task storage t = _tasks[taskId];
        t.owner = msg.sender;
        t.status = Status.Open;
        t.p = p;
        emit TaskCreated(taskId, msg.sender, p.executor, p.tokenIn);
    }

    /// @notice Owner replaces the task parameters. Executors can never call this.
    function updateTask(bytes32 taskId, TaskParams calldata p) external nonReentrant {
        Task storage t = _open(taskId);
        if (t.owner != msg.sender) revert NotOwner();
        _validate(p);
        t.p = p;
        emit TaskUpdated(taskId);
    }

    function cancelTask(bytes32 taskId) external nonReentrant {
        Task storage t = _open(taskId);
        if (t.owner != msg.sender) revert NotOwner();
        t.status = Status.Cancelled;
        emit TaskCancelled(taskId);
    }

    // --------------------------------------------------------------- executor

    /// @param amountIn tokens to pull from the owner, 0 < amountIn <= maxAmountIn
    /// @param minOut   executor's own minimum; must be >= the owner's pro-rata floor
    function execute(bytes32 taskId, uint256 amountIn, uint256 minOut) external nonReentrant {
        Task storage t = _open(taskId);
        if (msg.sender != t.p.executor) revert NotExecutor();
        // Cheap early exit while the pool does not exist yet: touches only the slots it needs, so a
        // pre-armed attempt that lands before the launch costs as little gas as possible.
        _requirePairs(t.p.factory, t.p.path);

        TaskParams memory p = t.p;
        if (block.timestamp > p.deadline) revert Expired();
        if (block.number < p.minBlock || (p.maxBlock != 0 && block.number > p.maxBlock)) revert BlockOutOfRange();
        if (amountIn == 0 || amountIn > p.maxAmountIn) revert AmountOutOfRange();
        if (minOut < floorFor(p.minOutFloor, amountIn, p.maxAmountIn)) revert MinOutBelowFloor();

        t.status = Status.Executed;

        uint256 beforeOut = _recipientBalance(p);
        uint256 actualIn =
            p.mode == Mode.RouterHop ? _sellViaRouter(t.owner, p, amountIn, minOut) : _sellDirect(t.owner, p, amountIn);
        uint256 received = _recipientBalance(p) - beforeOut;
        if (received < minOut) revert InsufficientReceived();

        emit TaskExecuted(taskId, amountIn, actualIn, received);
    }

    // ------------------------------------------------------------------ views

    function getTask(bytes32 taskId) external view returns (Task memory) {
        return _tasks[taskId];
    }

    function floorFor(uint256 minOutFloor, uint256 amountIn, uint256 maxAmountIn) public pure returns (uint256) {
        // ceil(minOutFloor * amountIn / maxAmountIn)
        return (minOutFloor * amountIn + maxAmountIn - 1) / maxAmountIn;
    }

    // --------------------------------------------------------------- internal

    function _open(bytes32 taskId) private view returns (Task storage t) {
        t = _tasks[taskId];
        if (t.status != Status.Open) revert NotOpen();
    }

    function _requirePairs(address factory, address[] storage path) private view {
        uint256 n = path.length;
        address a = path[0];
        for (uint256 i = 1; i < n; ++i) {
            address b = path[i];
            if (IV2Factory(factory).getPair(a, b) == address(0)) revert PairNotFound();
            a = b;
        }
    }

    function _validate(TaskParams calldata p) private view {
        if (
            p.executor == address(0) || p.tokenIn == address(0) || p.tokenOut == address(0) || p.tokenIn == p.tokenOut
                || p.recipient == address(0) || p.recipient == address(this) || p.maxAmountIn == 0 || p.minOutFloor == 0
                || p.deadline <= block.timestamp || (p.maxBlock != 0 && p.maxBlock < p.minBlock)
                || p.maxTransferLossBps > MAX_TRANSFER_LOSS_BPS
        ) revert InvalidParams();

        uint256 n = p.path.length;
        if (n < 2 || n > MAX_PATH || p.path[0] != p.tokenIn || p.path[n - 1] != p.tokenOut) revert InvalidPath();
        for (uint256 i; i + 1 < n; ++i) {
            if (p.path[i] == address(0) || p.path[i] == p.path[i + 1]) revert InvalidPath();
        }

        if (p.router.code.length == 0 || p.factory.code.length == 0) revert InvalidRouter();
        if (IV2Router(p.router).factory() != p.factory) revert InvalidRouter();

        if (p.mode == Mode.RouterHop) {
            if (p.pairFeeBps != 0) revert InvalidParams();
            if (p.nativeOut && IV2Router(p.router).WETH() != p.tokenOut) revert InvalidRouter();
        } else {
            if (n != 2 || p.nativeOut || p.pairFeeBps == 0 || p.pairFeeBps > MAX_PAIR_FEE_BPS) revert InvalidParams();
        }
    }

    function _recipientBalance(TaskParams memory p) private view returns (uint256) {
        return p.nativeOut ? p.recipient.balance : IERC20(p.tokenOut).balanceOf(p.recipient);
    }

    function _checkLoss(uint256 amountIn, uint256 actualIn, uint16 lossBps) private pure {
        if (actualIn * 10_000 < amountIn * (10_000 - lossBps)) revert TransferLossTooHigh();
    }

    function _sellViaRouter(address owner, TaskParams memory p, uint256 amountIn, uint256 minOut)
        private
        returns (uint256 actualIn)
    {
        IERC20 token = IERC20(p.tokenIn);
        uint256 held = token.balanceOf(address(this));
        token.safeTransferFrom(owner, address(this), amountIn);
        actualIn = token.balanceOf(address(this)) - held;
        _checkLoss(amountIn, actualIn, p.maxTransferLossBps);

        token.forceApprove(p.router, actualIn);
        if (p.nativeOut) {
            IV2Router(p.router)
                .swapExactTokensForETHSupportingFeeOnTransferTokens(
                    actualIn, minOut, p.path, p.recipient, block.timestamp
                );
        } else {
            IV2Router(p.router)
                .swapExactTokensForTokensSupportingFeeOnTransferTokens(
                    actualIn, minOut, p.path, p.recipient, block.timestamp
                );
        }
        token.forceApprove(p.router, 0);
        // Exactly what was pulled for this task must have left again: no residue, no use of older balance.
        if (token.balanceOf(address(this)) != held) revert ResidualBalance();
    }

    function _sellDirect(address owner, TaskParams memory p, uint256 amountIn) private returns (uint256 actualIn) {
        IV2Pair pair = IV2Pair(IV2Factory(p.factory).getPair(p.tokenIn, p.tokenOut));
        bool inIs0 = pair.token0() == p.tokenIn;
        uint256 rIn;
        uint256 rOut;
        {
            (uint256 r0, uint256 r1,) = pair.getReserves();
            (rIn, rOut) = inIs0 ? (r0, r1) : (r1, r0);
        }

        IERC20(p.tokenIn).safeTransferFrom(owner, address(pair), amountIn);
        // Input is what the pair really holds above its reserve, not the nominal amount.
        actualIn = IERC20(p.tokenIn).balanceOf(address(pair)) - rIn;
        _checkLoss(amountIn, actualIn, p.maxTransferLossBps);

        uint256 out = _amountOut(actualIn, rIn, rOut, p.pairFeeBps);
        if (inIs0) pair.swap(0, out, p.recipient, "");
        else pair.swap(out, 0, p.recipient, "");
    }

    function _amountOut(uint256 amtIn, uint256 rIn, uint256 rOut, uint16 feeBps) private pure returns (uint256) {
        uint256 inAfterFee = amtIn * (10_000 - feeBps);
        return (inAfterFee * rOut) / (rIn * 10_000 + inAfterFee);
    }
}
