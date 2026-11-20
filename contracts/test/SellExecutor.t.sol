// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {IERC20} from "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import {ReentrancyGuard} from "@openzeppelin/contracts/utils/ReentrancyGuard.sol";
import {SellExecutor} from "../src/SellExecutor.sol";
import {MiniFactory, MiniPair, MiniRouter, MiniWETH} from "./mocks/MiniV2.sol";
import {PlainToken, FeeToken, GatedToken, NoReturnToken, FalseReturnToken, ReentrantToken} from "./mocks/Tokens.sol";

contract RejectEther {}

contract SellExecutorTest is Test {
    SellExecutor ex;
    MiniFactory factory;
    MiniRouter router;
    MiniWETH weth;
    PlainToken token;

    address owner = makeAddr("owner");
    address keeper = makeAddr("keeper");
    address recipient = makeAddr("recipient");
    address stranger = makeAddr("stranger");

    uint256 constant AMT = 1_000_000 ether;
    uint256 constant POOL_TOKENS = 200_000_000 ether;
    uint256 constant POOL_WETH = 18 ether;

    function setUp() public {
        ex = new SellExecutor();
        factory = new MiniFactory();
        weth = new MiniWETH();
        router = new MiniRouter(address(factory), payable(address(weth)));
        token = new PlainToken("TKN");
        _pool(address(token), POOL_TOKENS, POOL_WETH);
        token.mint(owner, AMT * 3);
    }

    // ------------------------------------------------------------- helpers

    function _pool(address t, uint256 tokens, uint256 eth) internal returns (MiniPair pair) {
        pair = MiniPair(factory.createPair(t, address(weth)));
        PlainToken(t).mint(address(pair), tokens);
        vm.deal(address(this), eth);
        weth.deposit{value: eth}();
        weth.transfer(address(pair), eth);
        pair.sync();
    }

    function _params(address t) internal view returns (SellExecutor.TaskParams memory p) {
        address[] memory path = new address[](2);
        path[0] = t;
        path[1] = address(weth);
        p = SellExecutor.TaskParams({
            executor: keeper,
            tokenIn: t,
            tokenOut: address(weth),
            router: address(router),
            factory: address(factory),
            recipient: recipient,
            maxAmountIn: AMT,
            minOutFloor: 0.06 ether,
            deadline: uint64(block.timestamp + 1 days),
            minBlock: 0,
            maxBlock: 0,
            maxTransferLossBps: 0,
            pairFeeBps: 0,
            mode: SellExecutor.Mode.RouterHop,
            nativeOut: false,
            path: path
        });
    }

    function _create(SellExecutor.TaskParams memory p, address who, address t) internal returns (bytes32 id) {
        vm.startPrank(who);
        // SafeERC20-free approve so USDT-style tokens work in tests too.
        (bool ok,) = t.call(abi.encodeWithSignature("approve(address,uint256)", address(ex), p.maxAmountIn));
        require(ok);
        id = ex.createTask(p);
        vm.stopPrank();
    }

    function _expectedOut(uint256 amtIn, uint256 rIn, uint256 rOut) internal pure returns (uint256) {
        uint256 f = amtIn * 9975;
        return f * rOut / (rIn * 10_000 + f);
    }

    // ---------------------------------------------------------- happy path

    function test_standardSell() public {
        bytes32 id = _create(_params(address(token)), owner, address(token));
        uint256 expected = _expectedOut(AMT, POOL_TOKENS, POOL_WETH);

        vm.prank(keeper);
        ex.execute(id, AMT, 0.06 ether);

        assertEq(weth.balanceOf(recipient), expected);
        assertEq(token.balanceOf(owner), AMT * 2);
        assertEq(token.balanceOf(address(ex)), 0);
        assertEq(token.allowance(address(ex), address(router)), 0);
        assertEq(uint8(ex.getTask(id).status), uint8(SellExecutor.Status.Executed));
    }

    function test_nativeOut() public {
        SellExecutor.TaskParams memory p = _params(address(token));
        p.nativeOut = true;
        bytes32 id = _create(p, owner, address(token));
        vm.prank(keeper);
        ex.execute(id, AMT, 0.06 ether);
        assertEq(recipient.balance, _expectedOut(AMT, POOL_TOKENS, POOL_WETH));
        assertEq(address(ex).balance, 0);
    }

    function test_nativeOut_recipientRejects_revertsAndTaskStaysOpen() public {
        SellExecutor.TaskParams memory p = _params(address(token));
        p.nativeOut = true;
        p.recipient = address(new RejectEther());
        bytes32 id = _create(p, owner, address(token));
        vm.prank(keeper);
        vm.expectRevert(bytes("ETH_TRANSFER_FAILED"));
        ex.execute(id, AMT, 0.06 ether);
        assertEq(uint8(ex.getTask(id).status), uint8(SellExecutor.Status.Open));
        assertEq(token.balanceOf(owner), AMT * 3);
    }

    function test_partialSell_scalesFloor() public {
        bytes32 id = _create(_params(address(token)), owner, address(token));
        vm.startPrank(keeper);
        vm.expectRevert(SellExecutor.MinOutBelowFloor.selector);
        ex.execute(id, AMT / 2, 0.03 ether - 1);
        ex.execute(id, AMT / 2, 0.03 ether);
        vm.stopPrank();
        assertEq(token.balanceOf(owner), AMT * 3 - AMT / 2);
    }

    // --------------------------------------------------------- permissions

    function test_unauthorizedExecutor() public {
        bytes32 id = _create(_params(address(token)), owner, address(token));
        vm.prank(stranger);
        vm.expectRevert(SellExecutor.NotExecutor.selector);
        ex.execute(id, AMT, 0.06 ether);
        // the owner is not an executor either unless they set themselves
        vm.prank(owner);
        vm.expectRevert(SellExecutor.NotExecutor.selector);
        ex.execute(id, AMT, 0.06 ether);
    }

    function test_forgedOwnerCannotManage() public {
        SellExecutor.TaskParams memory p = _params(address(token));
        bytes32 id = _create(p, owner, address(token));
        p.recipient = stranger;
        for (uint256 i; i < 2; ++i) {
            vm.startPrank(i == 0 ? stranger : keeper);
            vm.expectRevert(SellExecutor.NotOwner.selector);
            ex.updateTask(id, p);
            vm.expectRevert(SellExecutor.NotOwner.selector);
            ex.cancelTask(id);
            vm.stopPrank();
        }
        assertEq(ex.getTask(id).p.recipient, recipient);
    }

    function test_strangerTaskCannotSpendVictimAllowance() public {
        // Victim approved the executor contract; attacker registers a task naming the victim's token.
        vm.prank(owner);
        token.approve(address(ex), AMT);
        SellExecutor.TaskParams memory p = _params(address(token));
        p.executor = stranger;
        p.recipient = stranger;
        vm.prank(stranger);
        bytes32 id = ex.createTask(p);
        // Task owner is the attacker, so transferFrom pulls from the attacker (who has nothing).
        vm.prank(stranger);
        vm.expectRevert();
        ex.execute(id, AMT, 0.06 ether);
        assertEq(token.balanceOf(owner), AMT * 3);
    }

    function test_excessInput() public {
        SellExecutor.TaskParams memory p = _params(address(token));
        bytes32 id = _create(p, owner, address(token));
        vm.prank(owner);
        token.approve(address(ex), AMT * 3); // allowance larger than the task
        vm.startPrank(keeper);
        vm.expectRevert(SellExecutor.AmountOutOfRange.selector);
        ex.execute(id, AMT + 1, 1 ether);
        vm.expectRevert(SellExecutor.AmountOutOfRange.selector);
        ex.execute(id, 0, 1 ether);
        vm.stopPrank();
    }

    function test_executorCannotLowerFloor_canTighten() public {
        bytes32 id = _create(_params(address(token)), owner, address(token));
        vm.startPrank(keeper);
        vm.expectRevert(SellExecutor.MinOutBelowFloor.selector);
        ex.execute(id, AMT, 0.06 ether - 1);
        // tighter than the pool can pay: reverts, nothing consumed
        vm.expectRevert(bytes("INSUFFICIENT_OUTPUT_AMOUNT"));
        ex.execute(id, AMT, 1 ether);
        ex.execute(id, AMT, 0.089 ether);
        vm.stopPrank();
    }

    // ------------------------------------------------- creation validation

    function test_invalidRouterFactoryPath() public {
        SellExecutor.TaskParams memory p = _params(address(token));
        vm.startPrank(owner);

        p.router = stranger; // EOA
        vm.expectRevert(SellExecutor.InvalidRouter.selector);
        ex.createTask(p);

        p = _params(address(token));
        p.factory = address(new MiniFactory()); // router does not belong to this factory
        vm.expectRevert(SellExecutor.InvalidRouter.selector);
        ex.createTask(p);

        p = _params(address(token));
        p.path[0] = address(weth);
        vm.expectRevert(SellExecutor.InvalidPath.selector);
        ex.createTask(p);

        p = _params(address(token));
        p.path[1] = address(token);
        vm.expectRevert(SellExecutor.InvalidPath.selector);
        ex.createTask(p);

        p = _params(address(token));
        p.path = new address[](5);
        vm.expectRevert(SellExecutor.InvalidPath.selector);
        ex.createTask(p);

        p = _params(address(token));
        p.minOutFloor = 0;
        vm.expectRevert(SellExecutor.InvalidParams.selector);
        ex.createTask(p);

        p = _params(address(token));
        p.recipient = address(ex);
        vm.expectRevert(SellExecutor.InvalidParams.selector);
        ex.createTask(p);

        p = _params(address(token));
        p.nativeOut = true;
        p.tokenOut = address(token);
        vm.expectRevert();
        ex.createTask(p);
        vm.stopPrank();
    }

    function test_pairMissingIsCheapRevert() public {
        PlainToken fresh = new PlainToken("NEW");
        fresh.mint(owner, AMT);
        bytes32 id = _create(_params(address(fresh)), owner, address(fresh));
        vm.prank(keeper);
        uint256 g = gasleft();
        vm.expectRevert(SellExecutor.PairNotFound.selector);
        ex.execute(id, AMT, 0.06 ether);
        assertLt(g - gasleft(), 80_000); // measured ~63k incl. cold storage reads; a full sell is ~150k+
        assertEq(uint8(ex.getTask(id).status), uint8(SellExecutor.Status.Open));
    }

    // ------------------------------------------------- lifecycle / windows

    function test_noReplay() public {
        bytes32 id = _create(_params(address(token)), owner, address(token));
        vm.prank(owner);
        token.approve(address(ex), AMT * 3);
        vm.startPrank(keeper);
        ex.execute(id, AMT, 0.06 ether);
        vm.expectRevert(SellExecutor.NotOpen.selector);
        ex.execute(id, AMT, 0.06 ether);
        vm.stopPrank();
        assertEq(token.balanceOf(owner), AMT * 2);
    }

    function test_cancel() public {
        bytes32 id = _create(_params(address(token)), owner, address(token));
        vm.prank(owner);
        ex.cancelTask(id);
        vm.prank(keeper);
        vm.expectRevert(SellExecutor.NotOpen.selector);
        ex.execute(id, AMT, 0.06 ether);
        vm.prank(owner);
        vm.expectRevert(SellExecutor.NotOpen.selector);
        ex.cancelTask(id);
    }

    function test_deadline() public {
        bytes32 id = _create(_params(address(token)), owner, address(token));
        vm.warp(block.timestamp + 1 days + 1);
        vm.prank(keeper);
        vm.expectRevert(SellExecutor.Expired.selector);
        ex.execute(id, AMT, 0.06 ether);
    }

    function test_blockRange() public {
        SellExecutor.TaskParams memory p = _params(address(token));
        p.minBlock = uint64(block.number + 10);
        p.maxBlock = uint64(block.number + 12);
        bytes32 id = _create(p, owner, address(token));
        vm.startPrank(keeper);
        vm.expectRevert(SellExecutor.BlockOutOfRange.selector);
        ex.execute(id, AMT, 0.06 ether);
        vm.roll(block.number + 13);
        vm.expectRevert(SellExecutor.BlockOutOfRange.selector);
        ex.execute(id, AMT, 0.06 ether);
        vm.roll(block.number - 2);
        ex.execute(id, AMT, 0.06 ether);
        vm.stopPrank();
    }

    function test_ownerUpdate() public {
        SellExecutor.TaskParams memory p = _params(address(token));
        bytes32 id = _create(p, owner, address(token));
        p.minOutFloor = 0.08 ether;
        p.recipient = owner;
        vm.prank(owner);
        ex.updateTask(id, p);
        vm.startPrank(keeper);
        vm.expectRevert(SellExecutor.MinOutBelowFloor.selector);
        ex.execute(id, AMT, 0.06 ether);
        ex.execute(id, AMT, 0.08 ether);
        vm.stopPrank();
        assertGt(weth.balanceOf(owner), 0.08 ether);
    }

    // --------------------------------------------------- atomicity / funds

    function test_failedSwapIsAtomicAndRetryable() public {
        SellExecutor.TaskParams memory p = _params(address(token));
        p.minOutFloor = 0.2 ether; // above what the pool pays
        bytes32 id = _create(p, owner, address(token));
        vm.prank(keeper);
        vm.expectRevert(bytes("INSUFFICIENT_OUTPUT_AMOUNT"));
        ex.execute(id, AMT, 0.2 ether);
        assertEq(token.balanceOf(owner), AMT * 3);
        assertEq(token.allowance(owner, address(ex)), AMT);
        assertEq(uint8(ex.getTask(id).status), uint8(SellExecutor.Status.Open));

        // price improves (someone buys), same task now succeeds
        MiniPair pair = MiniPair(factory.getPair(address(token), address(weth)));
        vm.deal(address(this), 60 ether);
        weth.deposit{value: 60 ether}();
        weth.transfer(address(pair), 60 ether);
        pair.sync();
        vm.prank(keeper);
        ex.execute(id, AMT, 0.2 ether);
        assertGe(weth.balanceOf(recipient), 0.2 ether);
    }

    function test_residualBalanceAndAllowanceIsolation() public {
        token.mint(address(ex), 777 ether); // stray donation sitting in the contract
        bytes32 id = _create(_params(address(token)), owner, address(token));
        vm.prank(owner);
        token.approve(address(ex), AMT * 3);

        vm.prank(keeper);
        ex.execute(id, AMT, 0.06 ether);

        assertEq(token.balanceOf(address(ex)), 777 ether, "stray balance must not be sold");
        assertEq(weth.balanceOf(recipient), _expectedOut(AMT, POOL_TOKENS, POOL_WETH));
        assertEq(token.balanceOf(owner), AMT * 2, "only the task amount leaves the owner");
        assertEq(token.allowance(owner, address(ex)), AMT * 2);
        // leftover allowance is unreachable: the task is spent and nobody else can create tasks for `owner`
        vm.prank(keeper);
        vm.expectRevert(SellExecutor.NotOpen.selector);
        ex.execute(id, 1, 1);
    }

    function test_multiTaskIsolation() public {
        address owner2 = makeAddr("owner2");
        address keeper2 = makeAddr("keeper2");
        token.mint(owner2, AMT);
        bytes32 a = _create(_params(address(token)), owner, address(token));
        SellExecutor.TaskParams memory p = _params(address(token));
        p.executor = keeper2;
        p.recipient = owner2;
        p.maxAmountIn = AMT / 4;
        p.minOutFloor = 0.01 ether;
        bytes32 b = _create(p, owner2, address(token));
        assertTrue(a != b);

        vm.prank(keeper);
        vm.expectRevert(SellExecutor.NotExecutor.selector);
        ex.execute(b, AMT / 4, 0.01 ether);
        vm.prank(keeper2);
        vm.expectRevert(SellExecutor.AmountOutOfRange.selector);
        ex.execute(b, AMT, 0.06 ether);

        vm.prank(keeper2);
        ex.execute(b, AMT / 4, 0.01 ether);
        assertEq(token.balanceOf(owner), AMT * 3, "task A owner untouched");
        assertEq(token.balanceOf(owner2), AMT - AMT / 4);
        assertEq(uint8(ex.getTask(a).status), uint8(SellExecutor.Status.Open));

        // same owner, second task gets a distinct id and its own cap
        bytes32 c = _create(_params(address(token)), owner, address(token));
        assertTrue(c != a);
    }

    // ------------------------------------------------------- token quirks

    function test_feeOnTransfer_rejectedByDefault() public {
        FeeToken fee = new FeeToken(300);
        _pool(address(fee), POOL_TOKENS, POOL_WETH);
        fee.mint(owner, AMT);
        bytes32 id = _create(_params(address(fee)), owner, address(fee));
        vm.prank(keeper);
        vm.expectRevert(SellExecutor.TransferLossTooHigh.selector);
        ex.execute(id, AMT, 0.06 ether);
    }

    function test_feeOnTransfer_routerHop_isTaxedTwice() public {
        FeeToken fee = new FeeToken(300);
        _pool(address(fee), POOL_TOKENS, POOL_WETH);
        fee.mint(owner, AMT);
        SellExecutor.TaskParams memory p = _params(address(fee));
        p.maxTransferLossBps = 300;
        bytes32 id = _create(p, owner, address(fee));
        vm.prank(keeper);
        ex.execute(id, AMT, 0.06 ether);
        uint256 reachedPair = AMT * 97 / 100 * 97 / 100;
        assertEq(weth.balanceOf(recipient), _expectedOut(reachedPair, POOL_TOKENS, POOL_WETH));
        assertEq(fee.balanceOf(address(ex)), 0);
    }

    function test_feeOnTransfer_directToPair_isTaxedOnce() public {
        FeeToken fee = new FeeToken(300);
        _pool(address(fee), POOL_TOKENS, POOL_WETH);
        fee.mint(owner, AMT);
        SellExecutor.TaskParams memory p = _params(address(fee));
        p.maxTransferLossBps = 300;
        p.mode = SellExecutor.Mode.DirectToPair;
        p.pairFeeBps = 25;
        bytes32 id = _create(p, owner, address(fee));
        vm.prank(keeper);
        ex.execute(id, AMT, 0.06 ether);
        assertEq(weth.balanceOf(recipient), _expectedOut(AMT * 97 / 100, POOL_TOKENS, POOL_WETH));
    }

    function test_feeAboveOwnerTolerance_rejected() public {
        FeeToken fee = new FeeToken(1000);
        _pool(address(fee), POOL_TOKENS, POOL_WETH);
        fee.mint(owner, AMT);
        SellExecutor.TaskParams memory p = _params(address(fee));
        p.maxTransferLossBps = 300;
        p.minOutFloor = 0.01 ether;
        bytes32 id = _create(p, owner, address(fee));
        vm.prank(keeper);
        vm.expectRevert(SellExecutor.TransferLossTooHigh.selector);
        ex.execute(id, AMT, 0.01 ether);
    }

    function test_directToPair_standardToken() public {
        SellExecutor.TaskParams memory p = _params(address(token));
        p.mode = SellExecutor.Mode.DirectToPair;
        p.pairFeeBps = 25;
        bytes32 id = _create(p, owner, address(token));
        vm.prank(keeper);
        ex.execute(id, AMT, 0.06 ether);
        assertEq(weth.balanceOf(recipient), _expectedOut(AMT, POOL_TOKENS, POOL_WETH));
    }

    function test_directToPair_wrongFeeAssumptionReverts() public {
        SellExecutor.TaskParams memory p = _params(address(token));
        p.mode = SellExecutor.Mode.DirectToPair;
        p.pairFeeBps = 10; // pool really charges 25 bps -> K check fails, nothing is lost
        bytes32 id = _create(p, owner, address(token));
        vm.prank(keeper);
        vm.expectRevert(bytes("K"));
        ex.execute(id, AMT, 0.06 ether);
        assertEq(token.balanceOf(owner), AMT * 3);
    }

    function test_gatedToken_approveBeforeOpen_sellAfterOpen() public {
        GatedToken gated = new GatedToken(); // this test contract is the launcher
        gated.mint(owner, AMT);
        bytes32 id = _create(_params(address(gated)), owner, address(gated)); // approve + task before any pool

        vm.prank(keeper);
        vm.expectRevert(SellExecutor.PairNotFound.selector);
        ex.execute(id, AMT, 0.06 ether);

        _pool(address(gated), POOL_TOKENS, POOL_WETH); // pool exists, trading still closed
        vm.prank(keeper);
        vm.expectRevert(bytes("TRADING_CLOSED"));
        ex.execute(id, AMT, 0.06 ether);
        assertEq(uint8(ex.getTask(id).status), uint8(SellExecutor.Status.Open));

        gated.openTrading();
        vm.prank(keeper);
        ex.execute(id, AMT, 0.06 ether);
        assertEq(weth.balanceOf(recipient), _expectedOut(AMT, POOL_TOKENS, POOL_WETH));
    }

    function test_noReturnToken() public {
        NoReturnToken t = new NoReturnToken();
        _pool2(address(t));
        t.mint(owner, AMT);
        bytes32 id = _create(_params(address(t)), owner, address(t));
        vm.prank(keeper);
        // MiniRouter itself requires a bool from transferFrom, so use the direct path for USDT-style tokens.
        vm.expectRevert();
        ex.execute(id, AMT, 0.06 ether);

        SellExecutor.TaskParams memory p = _params(address(t));
        p.mode = SellExecutor.Mode.DirectToPair;
        p.pairFeeBps = 25;
        vm.prank(owner);
        ex.updateTask(id, p);
        // MiniPair.swap only transfers the output token (WETH), so the USDT-style input is fine here.
        vm.prank(keeper);
        ex.execute(id, AMT, 0.06 ether);
        assertEq(weth.balanceOf(recipient), _expectedOut(AMT, POOL_TOKENS, POOL_WETH));
    }

    function _pool2(address t) internal {
        MiniPair pair = MiniPair(factory.createPair(t, address(weth)));
        NoReturnToken(t).mint(address(pair), POOL_TOKENS);
        vm.deal(address(this), POOL_WETH);
        weth.deposit{value: POOL_WETH}();
        weth.transfer(address(pair), POOL_WETH);
        pair.sync();
    }

    function test_falseReturnToken_reverts() public {
        FalseReturnToken t = new FalseReturnToken();
        _pool(address(t), POOL_TOKENS, POOL_WETH);
        t.mint(owner, AMT);
        bytes32 id = _create(_params(address(t)), owner, address(t));
        vm.prank(keeper);
        vm.expectRevert();
        ex.execute(id, AMT, 0.06 ether);
        assertEq(uint8(ex.getTask(id).status), uint8(SellExecutor.Status.Open));
    }

    function test_reentrancyBlocked() public {
        ReentrantToken t = new ReentrantToken();
        _pool(address(t), POOL_TOKENS, POOL_WETH);
        t.mint(owner, AMT * 2);
        bytes32 id = _create(_params(address(t)), owner, address(t));
        vm.prank(owner);
        t.approve(address(ex), AMT * 2);

        // token tries to run the same task again from inside transferFrom
        t.arm(address(ex), abi.encodeCall(SellExecutor.execute, (id, AMT, 0.06 ether)));
        vm.prank(keeper);
        ex.execute(id, AMT, 0.06 ether);
        assertFalse(t.lastCallOk());
        assertEq(bytes4(t.lastCallData()), ReentrancyGuard.ReentrancyGuardReentrantCall.selector);
        assertEq(t.balanceOf(owner), AMT, "sold exactly once");

        // and it cannot cancel/update mid-flight either
        t.arm(address(ex), abi.encodeCall(SellExecutor.cancelTask, (id)));
        bytes32 id2 = _create(_params(address(t)), owner, address(t));
        vm.prank(keeper);
        ex.execute(id2, AMT, 0.06 ether);
        assertFalse(t.lastCallOk());
        assertEq(uint8(ex.getTask(id2).status), uint8(SellExecutor.Status.Executed));
    }

    function test_taskIdBindsChainAndContract() public {
        bytes32 id = _create(_params(address(token)), owner, address(token));
        assertEq(id, keccak256(abi.encode(block.chainid, address(ex), owner, uint256(0))));
        SellExecutor other = new SellExecutor();
        assertEq(uint8(other.getTask(id).status), uint8(SellExecutor.Status.None));
        vm.prank(keeper);
        vm.expectRevert(SellExecutor.NotOpen.selector);
        other.execute(id, AMT, 0.06 ether);
    }

    function testFuzz_floorNeverUndercut(uint256 amountIn, uint256 minOut) public {
        amountIn = bound(amountIn, 1, AMT);
        bytes32 id = _create(_params(address(token)), owner, address(token));
        uint256 floor = ex.floorFor(0.06 ether, amountIn, AMT);
        minOut = bound(minOut, 0, floor * 2);
        uint256 before = weth.balanceOf(recipient);
        vm.prank(keeper);
        try ex.execute(id, amountIn, minOut) {
            assertGe(weth.balanceOf(recipient) - before, floor);
            assertGe(minOut, floor);
        } catch {
            assertEq(weth.balanceOf(recipient), before);
            assertEq(token.balanceOf(owner), AMT * 3);
        }
    }
}
