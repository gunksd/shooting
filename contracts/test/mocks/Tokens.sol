// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {ERC20} from "@openzeppelin/contracts/token/ERC20/ERC20.sol";

contract PlainToken is ERC20 {
    constructor(string memory s) ERC20(s, s) {}

    function mint(address to, uint256 a) external {
        _mint(to, a);
    }
}

/// Burns `feeBps` of every transfer (classic fee-on-transfer).
contract FeeToken is PlainToken("FEE") {
    uint256 public feeBps;

    constructor(uint256 f) {
        feeBps = f;
    }

    function _update(address from, address to, uint256 value) internal override {
        if (from == address(0) || to == address(0)) return super._update(from, to, value);
        uint256 fee = value * feeBps / 10_000;
        super._update(from, address(0), fee);
        super._update(from, to, value - fee);
    }
}

/// Transfers are blocked for everyone except the launcher until trading is opened.
contract GatedToken is PlainToken("GATED") {
    bool public tradingOpen;
    address public immutable launcher = msg.sender;

    function openTrading() external {
        require(msg.sender == launcher, "launcher");
        tradingOpen = true;
    }

    function _update(address from, address to, uint256 value) internal override {
        require(tradingOpen || from == address(0) || from == launcher, "TRADING_CLOSED");
        super._update(from, to, value);
    }
}

/// USDT-style token: transfer/transferFrom/approve return nothing.
contract NoReturnToken {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 a) external {
        balanceOf[to] += a;
    }

    function approve(address s, uint256 a) external {
        allowance[msg.sender][s] = a;
    }

    function transfer(address to, uint256 a) external {
        balanceOf[msg.sender] -= a;
        balanceOf[to] += a;
    }

    function transferFrom(address f, address to, uint256 a) external {
        allowance[f][msg.sender] -= a;
        balanceOf[f] -= a;
        balanceOf[to] += a;
    }
}

/// transferFrom returns false without moving anything.
contract FalseReturnToken is PlainToken("FALSE") {
    function transferFrom(address, address, uint256) public pure override returns (bool) {
        return false;
    }
}

/// Calls back into a target during transferFrom (reentrancy probe).
contract ReentrantToken is PlainToken("REENT") {
    address public target;
    bytes public payload;
    bool public lastCallOk;
    bytes public lastCallData;

    function arm(address t, bytes calldata p) external {
        target = t;
        payload = p;
    }

    function transferFrom(address f, address to, uint256 a) public override returns (bool) {
        if (target != address(0)) {
            (lastCallOk, lastCallData) = target.call(payload);
        }
        return super.transferFrom(f, to, a);
    }
}
