# advisor 本地降级：错误降级（失败回注 `tool_result` 明文 + 六码内部归因）

**状态**：accepted（issue #16 决策）

承接 ADR-0001（#6，后端渠道与触发时机）、ADR-0002（#8，配置建模）、原型 #7（端到端回路实测）。本 ADR 记录**本地 advisor 子调用失败时**如何让 executor 继续而非整次请求失败。三道题全部通过 `/grilling` 与真人敲定。

## 背景：官方契约（逐字，一手文档核对）

官方 advisor 失败的回注形状是**嵌套在外层 `advisor_tool_result` 块内**的错误块：

```json
{
  "type": "advisor_tool_result",
  "tool_use_id": "srvtoolu_abc123",
  "content": { "type": "advisor_tool_result_error", "error_code": "overloaded" }
}
```

`error_code` 六码语义：`max_uses_exceeded`（达工具 `max_uses` 上限）/ `too_many_requests`（advisor 子推理被限流）/ `overloaded`（触容量上限）/ `prompt_too_long`（转录超 advisor 上下文窗）/ `execution_time_exceeded`（子推理超时）/ `unavailable`（其它一切 advisor 失败）。

核心原则（官方逐字）：**"The executor sees the error and continues without further advice. The request itself does not fail."** —— advisor 限流画在结果里（`too_many_requests`）；**executor** 限流才是整请求 HTTP 429。

## 决策与理由

**1. 基调：失败回注错误信号、让 executor 继续，绝不整请求 5xx。**
本地 advisor 推理可能因限流 / 超时 / 超载 / 上下文超窗等失败。无论哪种，cc-switch 都**回注一个「advisor 本次无建议」的信号**，让 executor 带着它继续，**绝不**让整次请求 5xx。
理由：①忠实官方契约——官方逐字写明 advisor 失败时请求不失败；②符合简单性——我们已 `try-catch` 住 advisor 子调用（见 ADR-0002「降级触发与报错判断的分界」），把失败硬甩给 executor 会让用户看到「明明能用、却因一个可选的建议工具而挂掉」，是最差体验；③advisor 是可选增强，它的缺席不该拖垮主任务。
**不发 `max_uses_exceeded`**：该码是官方服务端在 `max_uses` 封顶时发的；本地 `max_uses` 靠透传客户端（ADR-0001）、无服务端上限可触发，cc-switch 也不自造上限去发它。

**2. 失败 → 错误码映射（作内部归因 + 回注明文的「原因」用词）。**
本地 advisor 只是一次普通 `/v1/messages` 非流式调用，上游 HTTP 状态是唯一可观察信号。按官方语义逐字对位：

| 上游信号 | 错误码 | 官方语义对位 |
|---|---|---|
| HTTP 429 限流 | `too_many_requests` | 「advisor 子推理被限流」 |
| HTTP 529 / `overloaded` 触容量 | `overloaded` | 「advisor 子推理触容量上限」 |
| 超时 | `execution_time_exceeded` | 「advisor 子推理超时」 |
| HTTP 400 `prompt_too_long` / 上下文超窗 | `prompt_too_long` | 「转录超出 advisor 上下文窗」 |
| 其余一切（连接失败 / 5xx / 非 429·529·超窗 的 400 / 响应无法解析） | `unavailable` | 官方兜底「Any other advisor failure」 |

四点对位说明（推论非新决策）：
- **429 落 advisor** 恰好对齐官方「advisor 限流画在结果里、executor 限流才是 429」——因子调用复用 executor 端点（ADR-0001），二者共享同一 per-model 限流桶。
- **超载真实可发**：本地 advisor 就是一次真 inference，可能真打满容量——这正是官方 `overloaded` 的本义，故如实映射而非归 `unavailable`。
- **`prompt_too_long` 双触发**：既被 ADR-0002「配对校验」在前置拦（advisor 窗 ≥ 转录才放行），也在子调用真打超窗时由上游 400 捕获——两道防线归同一码。
- **`unavailable` 兜底**装「其余一切」，含 cc-switch 无法区分是不是端点不认 advisor 的 400——与 ADR-0002「base_url 先验判定」不冲突：那是决定要不要降级，这是降级后子调用失败的兜底，各管一段。

**3. 错误形状：复用原型 #7 的 `tool_result` 明文回退，六码不进回注体。**
本地 advisor 失败时，回退路径**直接复用原型已实测跑通的形状**——普通 `tool_result`，`content` 用明文写「advisor 不可用 + 原因」，`is_error` 按原型现状（不置 `true`）。executor 看到「本次无建议」照常继续。

```js
// 与原型 #7 成功路径同一形状；失败仅换 content 明文
{ type: "tool_result", tool_use_id, content: "advisor 本地推理失败：超时（execution_time_exceeded），本次无建议。" }
```

理由：①**贴合原型、不自创**——原型 #7 实测本机 k3 对外层 `advisor_tool_result` 未知块 type 严格 400、对普通 `tool_result` 200；套着 error 的同款 `advisor_tool_result` 块照样 400。②**六码不再作为结构化块 type 透给第三方端点**（它根本不认 `advisor_tool_result_error`），而是**降级成 `tool_result` 的明文说明**——与成功路径完全同一形状，零新机制、零分叉。③六码退居**内部日志 / 用量记录的分类归因**（供故障排查与配对校验复用），不进回注体；`prompt_too_long` 一码顺带喂给 ADR-0002 配对校验复用。
**`advisor_tool_result_error` 块形状仅作契约参照落此 ADR，不进生产回注体。**
与 ADR-0002 题 4「回注报错自动回退普通 `tool_result`」的关系：**#8 管的是「成功回注块本身被 400」、本票管的是「advisor 子推理失败」，两者共用同一条 `tool_result` 明文回退管道，不分叉。** 内联错误块只在「端点本就能消化 `advisor_tool_result`」时才有意义；对实测只收 `tool_result` 的端点，错误信息以 `tool_result` 明文 content 送达。

## 既定约束（流式保活，承接 #5，不另开决策）

若流式路径在 advisor 推理期间暂停 executor 流，会触发 `create_logged_passthrough_stream`（`response_processor.rs:701-736`）的首字节/静默超时假设——**暂停期需发 SSE `ping` 保活**。此为流式实现侧既定约束，直接记录，不在本票另立决策。

## 被否的替代方案

- **advisor 失败让整请求 5xx**：违背官方「request itself does not fail」，且让可选增强拖垮主任务，体验最差。
- **回注内联 `advisor_tool_result_error` 结构化块给所有端点**：原型 #7 实测第三方端点（k3）对外层 `advisor_tool_result` 未知块 type 严格 400——结构化错误块根本送达不到，等于让 executor 干等、违背题 1 基调。仅作契约参照，不进生产回注体。
- **`tool_result` 置 `is_error: true`**：语义虽诚实，但偏离原型已实测跑通的形状（原型 content 纯明文、不置 `is_error`）；为不引入未实测变量，贴合原型。
- **为「端点疑似不认 advisor 的 400」单独立码**：cc-switch 无法区分这类 400 与「其余失败」，猜测有假阳性；统归 `unavailable` 兜底。
- **`max_uses_exceeded` 本地复刻**：本地 `max_uses` 透传客户端、无服务端上限可触发，为不会发生的场景造码违反「不做不可能场景的错误处理」。
