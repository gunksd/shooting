// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

/// Minimal but functional V2 AMM (constant product, 0.25% fee, K check) used for local tests.
/// It follows PancakeSwap V2 semantics for swap(), getReserves() and the fee-on-transfer router paths.
contract MiniPair {
    address public token0;
    address public token1;
    uint112 private r0;
    uint112 private r1;

    event Sync(uint112 reserve0, uint112 reserve1);

    function initialize(address t0, address t1) external {
        require(token0 == address(0), "INIT");
        (token0, token1) = (t0, t1);
    }

    function getReserves() public view returns (uint112, uint112, uint32) {
        return (r0, r1, 0);
    }

    function sync() public {
        r0 = uint112(IERC20(token0).balanceOf(address(this)));
        r1 = uint112(IERC20(token1).balanceOf(address(this)));
        emit Sync(r0, r1);
    }

    function swap(uint256 out0, uint256 out1, address to, bytes calldata) external {
        require(out0 > 0 || out1 > 0, "INSUFFICIENT_OUTPUT_AMOUNT");
        require(out0 < r0 && out1 < r1, "INSUFFICIENT_LIQUIDITY");
        if (out0 > 0) require(IERC20(token0).transfer(to, out0), "T0");
        if (out1 > 0) require(IERC20(token1).transfer(to, out1), "T1");
        uint256 b0 = IERC20(token0).balanceOf(address(this));
        uint256 b1 = IERC20(token1).balanceOf(address(this));
        uint256 in0 = b0 > r0 - out0 ? b0 - (r0 - out0) : 0;
        uint256 in1 = b1 > r1 - out1 ? b1 - (r1 - out1) : 0;
        require(in0 > 0 || in1 > 0, "INSUFFICIENT_INPUT_AMOUNT");
        require((b0 * 10_000 - in0 * 25) * (b1 * 10_000 - in1 * 25) >= uint256(r0) * r1 * 1e8, "K");
        r0 = uint112(b0);
        r1 = uint112(b1);
        emit Sync(r0, r1);
    }
}

contract MiniFactory {
    mapping(address => mapping(address => address)) public getPair;

    bytes32 public constant INIT_CODE_HASH = keccak256(type(MiniPair).creationCode);

    event PairCreated(address indexed token0, address indexed token1, address pair, uint256 index);

    function createPair(address a, address b) external returns (address pair) {
        require(getPair[a][b] == address(0), "EXISTS");
        (address t0, address t1) = a < b ? (a, b) : (b, a);
        pair = address(new MiniPair{salt: keccak256(abi.encodePacked(t0, t1))}());
        MiniPair(pair).initialize(t0, t1);
        getPair[a][b] = pair;
        getPair[b][a] = pair;
        emit PairCreated(t0, t1, pair, 1);
    }
}

contract MiniWETH is ERC20("Wrapped", "WETH") {
    receive() external payable {
        _mint(msg.sender, msg.value);
    }

    function deposit() external payable {
        _mint(msg.sender, msg.value);
    }

    function withdraw(uint256 a) external {
        _burn(msg.sender, a);
        (bool ok,) = msg.sender.call{value: a}("");
        require(ok, "ETH");
    }
}

contract MiniRouter {
    address public immutable factory;
    address payable public immutable WETH;

    constructor(address f, address payable w) {
        factory = f;
        WETH = w;
    }

    receive() external payable {}

    function _swap(address[] calldata path, address to) private {
        for (uint256 i; i + 1 < path.length; ++i) {
            address next = i + 2 < path.length ? MiniFactory(factory).getPair(path[i + 1], path[i + 2]) : to;
            _hop(path[i], path[i + 1], next);
        }
    }

    function _hop(address input, address output, address next) private {
        MiniPair pair = MiniPair(MiniFactory(factory).getPair(input, output));
        bool inIs0 = pair.token0() == input;
        uint256 out;
        {
            (uint256 a, uint256 b,) = pair.getReserves();
            (uint256 rIn, uint256 rOut) = inIs0 ? (a, b) : (b, a);
            uint256 fee = (IERC20(input).balanceOf(address(pair)) - rIn) * 9975;
            out = fee * rOut / (rIn * 10_000 + fee);
        }
        if (inIs0) pair.swap(0, out, next, "");
        else pair.swap(out, 0, next, "");
    }

    function swapExactTokensForTokensSupportingFeeOnTransferTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        address[] calldata path,
        address to,
        uint256 deadline
    ) external {
        require(deadline >= block.timestamp, "EXPIRED");
        require(IERC20(path[0]).transferFrom(msg.sender, MiniFactory(factory).getPair(path[0], path[1]), amountIn));
        IERC20 last = IERC20(path[path.length - 1]);
        uint256 before = last.balanceOf(to);
        _swap(path, to);
        require(last.balanceOf(to) - before >= amountOutMin, "INSUFFICIENT_OUTPUT_AMOUNT");
    }

    function swapExactTokensForETHSupportingFeeOnTransferTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        address[] calldata path,
        address to,
        uint256 deadline
    ) external {
        require(deadline >= block.timestamp, "EXPIRED");
        require(path[path.length - 1] == WETH, "INVALID_PATH");
        require(IERC20(path[0]).transferFrom(msg.sender, MiniFactory(factory).getPair(path[0], path[1]), amountIn));
        _swap(path, address(this));
        uint256 out = IERC20(WETH).balanceOf(address(this));
        require(out >= amountOutMin, "INSUFFICIENT_OUTPUT_AMOUNT");
        MiniWETH(WETH).withdraw(out);
        (bool ok,) = to.call{value: out}("");
        require(ok, "ETH_TRANSFER_FAILED");
    }
}
