// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {MiniFactory, MiniPair, MiniWETH} from "./MiniV2.sol";
import {GatedToken} from "./Tokens.sol";

/// Launchpad stand-in: holders get non-transferable tokens first; `launch()` creates the pool, adds
/// liquidity and opens trading in one transaction, like the SLERF sale contract did on mainnet.
contract Launcher {
    GatedToken public immutable token = new GatedToken();
    MiniFactory public immutable factory;
    MiniWETH public immutable weth;

    constructor(MiniFactory f, MiniWETH w) {
        factory = f;
        weth = w;
    }

    function issue(address to, uint256 amount) external {
        token.mint(to, amount);
    }

    function launch() external payable {
        MiniPair pair = MiniPair(factory.createPair(address(token), address(weth)));
        token.mint(address(pair), 200_000_000 ether);
        weth.deposit{value: msg.value}();
        weth.transfer(address(pair), msg.value);
        pair.sync();
        token.openTrading();
    }
}
