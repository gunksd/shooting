# 可行性调查（2026-11-18）

区分三类结论：**已实测**（本机或链上直接验证）、**文档核实**（官方文档有明确表述）、**未核实**。

## 1. Binance Agentic Wallet

| 项 | 结论 | 依据 |
|---|---|---|
| 对任意合约做限额 approve | 已实测可行 | `baw contract-call preview`（CLI 1.12.0）对 PancakeRouter 和一个随机地址的 `approve(spender, 1000e18)` 模拟均通过，`simulationCode=000000000`，`requireConfirmation=false` |
| 调用自部署合约 | 机制可行，主网未实测 | `contract-call preview/execute` 接受任意 `--to` 与 `--inputData`；`createTask` 需合约部署后才能预览 |
| 流程限制 | 已实测 | 必须先 preview 拿 `requestId` 再 execute；preview 有过期时间；高风险交易按钱包设置（`abnormalTxnHandling`）转 App 二次确认 |
| 开发者模式与限额 | 已实测 | `baw wallet settings` 返回 devMode（开关、到期时间、日限额）和总日限额，各钱包以自己的设置为准 |
| 消息签名 | 不依赖 | 方案不使用签名任务，任务由钱包直接发交易创建 |
| 私钥 | 不导出 | 全程只调用 CLI |

注意：`baw` 是 Node 程序，默认不走 `HTTPS_PROXY`，需设 `NODE_USE_ENV_PROXY=1`（控制台已自动处理）。

## 2. 目标代币：以 SLERF 为实例（已实测，链上数据）

代币 `0xEbE3069bC7B5F1602De421f7a1089759F15AB636`，销售合约 `0x13406cE228a4c7A32699b384ECe4da874dc407D1`，数据来源 `bsc-rpc.publicnode.com`。

- 开盘交易 `0x13338bb0…864c17`，区块 134305833，序号 48：最后一笔 `mintWithAgentId` 在同一笔交易里建池（18 BNB + 2 亿代币，Pancake V2）、销毁 LP、打开 `tradingOpen`。
- **开盘前 approve 可行**：开盘前链上已有 547 笔 Approval 事件。
- **开盘前 transferFrom 不可行**：`tradingOpen()` 在 N-1 块为 0，N 块为 1。
- **合约代卖可行**：同块卖出者正是 `owner → 合约 → Router → Pair` 的合约代卖。分叉重放中本仓库的 SellExecutor 以 owner 身份的预授权在开盘后卖出成功。
- **无转账税**：分叉重放实际到账与无税公式逐位一致（0.0447758402483806 BNB / 50 万枚）。
- mint 需 Agent ID；池子交易不需要（官网自述，且非 Agent 地址的卖出成功）。
- 开盘调用不可由他人代触发：非原发送者模拟 revert（`0x4e19b3be`）。

**开盘后价格衰减（实测）**，倍数 = 卖出均价 ÷ mint 成本：

| 区块 | 卖出笔数 | 首笔 | 末笔 |
|---|---|---|---|
| N（开盘块） | 1 | 2.99x | 2.99x |
| N+1 | 14 | 2.96x | 2.61x |
| N+2 | 10 | 2.58x | 2.37x |
| N+3 | 18 | 2.35x | 2.11x |
| N+4 | 47 | 首笔为尘埃交易，不计 | 1.49x |
| N+8 | 13 | 1.06x | 1.01x |

出块间隔 0.45 秒：开盘块之后只有 3 个区块（约 1.35 秒）还能卖到 2x 以上。

**同块卖出者的做法（实测）**：地址 `0x9f3b90a3…` 在 N-3 块起连续发送预埋交易，3 笔失败（各 39,224 gas），第 4 笔落在开盘块序号 108（开盘为 48），gas price 0.15 gwei，公共通道。它在 N 块的序号 38 那笔排在开盘之前而失败。这是概率性结果。

其他代币的税、黑名单、合约地址限制、冷却时间：**每个代币单独待验证**，本仓库不预设兼容。

## 3. Builder 通道（文档核实，未做主网提交实测）

| 项 | 48 Club | BlockRazor |
|---|---|---|
| bundle | `eth_sendBundle`，`https://puissant-builder.48.club/`；字段 `txs`、`maxBlockNumber`、`maxTimestamp`、`revertingTxHashes`、`noMerge`、`backrunTarget` | `eth_sendBundle`，`https://rpc.blockrazor.builders`；字段 `txs`、`maxBlockNumber`、`minTimestamp`、`maxTimestamp`、`revertingTxHashes`、`noMerge` |
| 私有单笔 | `eth_sendPrivateTransaction` | `eth_sendPrivateTransaction` |
| 包含他人公共池交易 | 仅由计价规则推得，无明示 | 计价示例明示 `{tx1(mempool), tx2, tx3}` |
| 私有订单流 backrun | 订阅拍卖 feed 后带 `backrunTarget`，需 48 SoulPoint | SSE 流 + `eth_sendMevBundle`，需出价合约 |
| 认证 | 无签名可用（限流 50 笔/9 秒） | 必须 `Authorization` token |
| 最低费用 | bundle 平均 gas price 校验 | 非 mempool 交易均价 ≥ 0.05 gwei |
| 小费 | 转 BNB 给 builder EOA | 转 BNB 给 builder 地址 |
| 有效期 | `maxBlockNumber` 默认 +100 | 同左 |
| 取消 | 文档无取消接口 | 文档无取消接口 |
| 模拟 | 文档无 `eth_callBundle` | `eth_callBundle`，付费 |
| 回滚 | 未列入 `revertingTxHashes` 的交易 revert 则不上链 | 文档称原子 |

来源：docs.48.club/puissant-builder（send-bundle、send-privatetransaction、auction-transaction-feed、48-soulpoint-benefits），docs.blockrazor.io/transaction-submission/block-builder（send-bundle、send-privatetransaction、call-bundle、endpoint），docs.bnbchain.org/bnb-smart-chain/validator/mev。

接入状态：两家的私有单笔与 bundle 请求格式已实现；`bundle_foreign_target` 能力只对 BlockRazor 置真。**小费转账未实现**，出价只靠交易自身 gas price。主网提交均未实测。

## 4. pending 数据源

- 公共节点的 `newPendingTransactions` 只能看到在公共 mempool 传播的交易。
- 通过 builder 私有接口或 bundle 发出的交易在任何公共源都不可见，上链前无信号。
- 公共节点对 pending 的覆盖范围：**未核实**。BlockRazor 有付费公共 mempool gRPC 流（含 raw tx）。
- SLERF 的开盘交易是普通用户的最后一笔 mint（0.093 gwei，区块中部），从特征看走公共池，但**是否曾在公共 mempool 可见无法事后证明**。

## 5. 对三个核心问题的回答

1. **Agent 钱包限额授权 + 合约代卖**：对 SLERF 这类"开盘前可 approve、开盘后放开转账、无税"的代币可行，已由分叉重放验证。
2. **费用与权限约束**：权限由合约强制（31 项测试）；费用由"签名前预留最坏花费"强制，legacy 交易的花费上限 = gasLimit × gasPrice，是硬上限。BNB 上限按启动时价格固定，美元价值随后会浮动。
3. **同块成交**：只监听已上链日志做不到同块。两条可行路径：公共 mempool 可见目标 + bundle 绑定（`strict_same_block`，需付费数据源，主网未实测）；开盘前预埋（`pre_armed_spray`，已在 anvil 上用真实合约验证同块且在开盘之后，主网排序由出块方决定，是概率性的）。任何模式都不保证同块或下一块。
