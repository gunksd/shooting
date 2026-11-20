// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Script, console2} from "forge-std/Script.sol";
import {SellExecutor} from "../src/SellExecutor.sol";

/// forge script script/Deploy.s.sol --rpc-url $BSC_RPC_URL --account <keystore> [--broadcast]
/// Without --broadcast this only simulates. The contract has no constructor args and no owner.
contract Deploy is Script {
    function run() external returns (SellExecutor ex) {
        vm.startBroadcast();
        ex = new SellExecutor();
        vm.stopBroadcast();
        console2.log("SellExecutor", address(ex));
    }
}
