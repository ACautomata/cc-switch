# advisor 本地降级：子调用 token 的用量建模（非流式，独立 advisor 行）

**状态**：accepted（issue #14 决策）

承接 ADR-0001（#6，后端渠道与触发时机）、ADR-0002（#8，配置建模），本 ADR 记录本地 advisor 子调用自身消耗的 token **如何建模进 cc-switch 的用量统计**。只覆盖**非流式**路径；流式路径 `SseUsageCollector` 如何识别/采集 advisor token 不在此范围。三道题全部通过 `/grilling` 与真人敲定。

## 背景约束

- 官方契约（#5）：顶层 `usage` **只含 executor token**；advisor token **不并入**顶层（费率不同），在 `usage.iterations[]` 里 `type:"advisor_message"`（按 advisor 模型费率）与 `type:"message"`（按 executor 模型费率）分行。
- cc-switch 现有用量结构 `TokenUsage`（`proxy/usage/parser.rs:52`）只有 `input/output/cache_read/cache_creation` 四个计费字段 + `model`/`message_id`；落库表 `proxy_request_logs` 为**固定列**（四个 token 列 + 五个成本列 + 每行一个标量 `model`），计价 `CostCalculator::calculate_for_app(usage, pricing, multiplier)` 用**单一 `pricing_model`** 查一份费率。
- 去重主键 `request_id = usage.dedup_request_id(dedup_scope_for_app(app_type, provider_id))`；同一 `request_id` 语义不同会落到 `:collision:<sha>` 兜底行。

## 决策与理由

**1. 方向：独立 advisor 行，不并入 executor 顶层（对应官方 `iterations[]` 分行语义）。**
本地 advisor 是一次**独立的上游 `/v1/messages` 调用**（不同 `message_id`、分档映射后常与 executor 不同 `model`），天然就是 `proxy_request_logs` 里**独立一行**。executor 与 advisor 各占一行、各自计价，两者成本合计进 provider 总账。
理由：并入 executor 顶层会让 advisor token 被按 executor 费率计价（费率错），还要动 `TokenUsage`/schema/去重逻辑——既贵又错，且与官方顶层语义相悖；独立行**零 schema 改动**即同时满足「计入总数」与「按模型区分」。

**2. 行识别：独立行，仅靠 `model` 列区分，不新增任何区分维度。**
advisor 行与 executor 行用同一 schema， advisor 的归属靠它**实际命中的 `model`** 体现（如 executor=sonnet 档、advisor=fable 档）；不引入 provider_id 命名空间、不新增 schema 列。
理由：DB 列只存 token 数与成本，不加列本就无法按行标记「这是 advisor」；而 advisor 复用 executor 的端点与凭证（ADR-0001），其开销本就该计入同一 provider。按模型区分已足够回答「这条用量来自哪个档」，再为「advisor」加专用维度超出本票所需，且会让 provider 维度统计/计价倍率解析变复杂。

**3. 计价模型：advisor 行按「分档映射后实际命中的那个模型」计价。**
advisor 行的 `model`/`request_model`/`pricing_model` 都写它**实际命中的模型**，与现行链路一致——advisor 模型可能不同于 executor，各自费率不同（官方 `iterations[]` 正因此分行）。零额外处理。
理由：这是官方「按 advisor 模型费率计费」语义在本地链路的直接复刻；若按 executor 费率计，会算错费率。

**4. 成本口径：计入总用量总成本，不单独出 advisor 报表。**
advisor 行落同一张表、按自身费率计价后，其成本自然算进 provider/全局的总用量与总成本；不单独设 advisor 专用成本项、不出 advisor 报表。
理由：承接 ADR-0001「成本只记录不熔断」——advisor 开销是这次对话真实发生、且复用 executor 端点凭证的成本，理应计入总账；单列报表依赖「能识别 advisor 行」的额外维度，与第 2 题的取舍相悖且无当前需求。

## 落库形状（非流式）

advisor 子调用复用既有 `log_usage` → `UsageLogger.log_with_calculation` 管线，从**它自己的响应**解析 `TokenUsage`（`message_id` 为 advisor 那条上游调用的 id），按上述四题落一行：

- `provider_id` / `app_type` / `session_id` 沿用 executor 本次请求（ advisor 开销计入同一 provider 的同一段会话）；
- `model` / `request_model` / `pricing_model` = advisor 实际命中的模型；
- 四个 token 列 = advisor 子调用自身消耗的 input/output/cache_read/cache_creation；
- `request_id` 由 advisor 自身的 `message_id` 派生，天然唯一、不与 executor 行碰撞，也无 session 导入双计（官方契约 executor 顶层 `usage` 本就不含 advisor token，session 导入器不会重记）。

## 被否的替代方案

- **(a) 并入 executor 顶层 `usage`**（题 1）：advisor token 被按 executor 费率计价（费率错），且要动 `TokenUsage`/schema/去重——既贵又错，与官方顶层语义相悖。
- **独立行 + provider_id 命名空间（如 `<executor_pid>:advisor`）**（题 2）：不动表结构但会污染 provider 维度统计与计价倍率解析，且 advisor 成本不再计入 executor 那个 provider，违背「复用同一端点凭证」。
- **独立行 + 新增 schema 列（如 `source=executor/advisor`）**（题 2）：最明确可查，但要动表结构/迁移/查询/前端展示，超出「非流式建模」本意，成本最高，当前无此需求。
- **按 executor 模型计价 advisor token**（题 3）：费率错，官方 `iterations[]` 分行的意义正是区分费率。
- **advisor 成本单列报表 / 不进总账**（题 4）：前者依赖额外区分维度且无当前需求；后者违背「成本只记录」与「复用同一端点凭证计入同账」。
