// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Test, console2} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {SellExecutor} from "../../src/SellExecutor.sol";

/// Replays the real SLERF launch on a BSC mainnet fork.
///   data source : BSC archive RPC given by BSC_RPC_URL (skipped when unset)
///   fork block  : 134305832 (state right before the opening block 134305833)
///   opening tx  : 0x13338bb0e2351637908b6806c8b9b84e35f6b4ce281589769bf6df32ea864c17 (tx index 48)
/// It shows that approve + task creation work before trading opens and that the executor sells right
/// after the opening call. It says nothing about winning block position on mainnet.
contract SlerfReplayTest is Test {
    uint256 constant OPEN_BLOCK = 134305833;
    address constant TOKEN = 0xEbE3069bC7B5F1602De421f7a1089759F15AB636;
    address constant SALE = 0x13406cE228a4c7A32699b384ECe4da874dc407D1;
    address constant OPENER = 0x40CCd75565f9f98eA64B5252F1781981410d0b27;
    address constant HOLDER = 0x14b61F3e84C0EE4efA978007404e45bb214F7018;
    address constant ROUTER = 0x10ED43C718714eb63d5aA57B78B54704E256024E;
    address constant FACTORY = 0xcA143Ce32Fe78f1f7019d7d551a6402fC5350c73;
    address constant WBNB = 0xbb4CdB9CBd36B01bD1cBaEBF2De08d9173bc095c;
    bytes constant OPEN_CALLDATA =
        hex"adbbadec000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000400000000000000000000000000000000000000000000000000000000000000000";

    uint256 constant COST_PER_TOKEN_WEI = 0.003 ether / 100_000; // per whole token (1e18 units)

    SellExecutor ex;
    address keeper = makeAddr("keeper");
    address recipient = makeAddr("recipient");
    bool forked;

    function setUp() public {
        string memory rpc = vm.envOr("BSC_RPC_URL", string(""));
        if (bytes(rpc).length == 0) return;
        vm.createSelectFork(rpc, OPEN_BLOCK - 1);
        forked = true;
        ex = new SellExecutor();
    }

    function _task(SellExecutor.Mode mode, uint256 amount, uint256 floor) internal returns (bytes32 id) {
        address[] memory path = new address[](2);
        path[0] = TOKEN;
        path[1] = WBNB;
        SellExecutor.TaskParams memory p = SellExecutor.TaskParams({
            executor: keeper,
            tokenIn: TOKEN,
            tokenOut: WBNB,
            router: ROUTER,
            factory: FACTORY,
            recipient: recipient,
            maxAmountIn: amount,
            minOutFloor: floor,
            deadline: uint64(block.timestamp + 7 days),
            minBlock: 0,
            maxBlock: 0,
            maxTransferLossBps: 0,
            pairFeeBps: mode == SellExecutor.Mode.DirectToPair ? 25 : 0,
            mode: mode,
            nativeOut: mode == SellExecutor.Mode.RouterHop,
            path: path
        });
        vm.startPrank(HOLDER);
        IERC20(TOKEN).approve(address(ex), amount); // pre-open approve
        id = ex.createTask(p);
        vm.stopPrank();
    }

    function _open() internal {
        vm.roll(OPEN_BLOCK);
        vm.deal(OPENER, 1 ether);
        vm.prank(OPENER);
        (bool ok,) = SALE.call{value: 0.006 ether}(OPEN_CALLDATA);
        require(ok, "open replay failed");
    }

    function _run(SellExecutor.Mode mode) internal {
        if (!forked) return;
        uint256 amount = IERC20(TOKEN).balanceOf(HOLDER);
        assertGt(amount, 0, "holder has tokens before open");
        uint256 cost = amount / 1e18 * COST_PER_TOKEN_WEI;
        uint256 floor = cost * 2; // owner demands at least 2x

        bytes32 id = _task(mode, amount, floor);

        // before the open: cheap revert, task untouched
        vm.prank(keeper);
        uint256 g = gasleft();
        vm.expectRevert(SellExecutor.PairNotFound.selector);
        ex.execute(id, amount, floor);
        console2.log("pre-open failed attempt gas (execution only)", g - gasleft());

        _open();

        uint256 before = mode == SellExecutor.Mode.RouterHop ? recipient.balance : IERC20(WBNB).balanceOf(recipient);
        vm.prank(keeper);
        g = gasleft();
        ex.execute(id, amount, floor);
        console2.log("sell gas (execution only)", g - gasleft());
        uint256 got =
            (mode == SellExecutor.Mode.RouterHop ? recipient.balance : IERC20(WBNB).balanceOf(recipient)) - before;

        console2.log("tokens sold (whole)", amount / 1e18);
        console2.log("cost wei", cost);
        console2.log("received wei", got);
        console2.log("multiple x100", got * 100 / cost);
        assertGe(got, floor);
        assertEq(IERC20(TOKEN).balanceOf(HOLDER), 0);
        assertEq(IERC20(TOKEN).balanceOf(address(ex)), 0);
    }

    function test_fork_routerHop_nativeOut() public {
        _run(SellExecutor.Mode.RouterHop);
    }

    function test_fork_directToPair() public {
        _run(SellExecutor.Mode.DirectToPair);
    }
}
