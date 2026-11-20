// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

interface IV2Factory {
    function getPair(address a, address b) external view returns (address);
}

interface IV2Pair {
    function token0() external view returns (address);
    function getReserves() external view returns (uint112 r0, uint112 r1, uint32 ts);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

interface IV2Router {
    function factory() external view returns (address);
    function WETH() external view returns (address);
    function swapExactTokensForTokensSupportingFeeOnTransferTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        address[] calldata path,
        address to,
        uint256 deadline
    ) external;
    function swapExactTokensForETHSupportingFeeOnTransferTokens(
        uint256 amountIn,
        uint256 amountOutMin,
        address[] calldata path,
        address to,
        uint256 deadline
    ) external;
}
