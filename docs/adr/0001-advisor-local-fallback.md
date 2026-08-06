# advisor 本地降级：复用 executor 端点、不注入引导、被动捕获

**状态**：accepted（issue #6 决策）

当上游第三方 Claude 端点不支持 `advisor` server 工具时，cc-switch 代理层把它降级为一次本地 advisor 推理。本 ADR 记录这次降级的三个高层取舍。

## 决策与理由

**1. advisor 子调用沿用 executor 端点，不独立配置。** advisor 子调用复用本次请求**实际命中**的那个 Provider 的 `base_url`+凭证（开销计入该 Provider 账单/限流），且在一段对话内**钉死**该 Provider、不随每次调用重走 router。理由：降级后 advisor 只是一次普通、不带工具的 `/v1/messages`，任何能跑 executor 的端点都能跑它，无需独立配置端点；钉死保证同段对话的 advisor 上下文连续（前缀缓存、语义一致）。advisor 模型用用户显式设定的档位（`advisor_tier`：fable/opus/sonnet，默认 fable），读对应 `ANTHROPIC_DEFAULT_{tier}_MODEL` 映射，未配则回落到 executor 已映射好的模型；`max_tokens` 沿用 executor 请求值；token 计入用量统计但不单独硬熔断。

**2. cc-switch 不注入「何时调用 advisor」的引导。** 本工具仅在 Claude Code 客户端场景使用，客户端已把触发引导注入自己的 system prompt 并把 advisor 工具放进 `tools[]`。cc-switch 是被动的：剥离 `advisor_20260301` server-tool 块 → 替换为普通客户端工具 `advisor`（无参数）→ 捕获 executor 的 `tool_use` → 本地推理 → 以 `advisor_tool_result{advisor_result{text}}` 回注。不 nudge、不改写 executor 的 system（避免破坏其 prompt cache 前缀）。`max_uses` 透传客户端给定值，cc-switch 不加自己的上限。

**3. 不做防重入。** advisor 子调用的 `tools` 为空（Oracle 系统提示 + 完整转录，无 advisor 工具），递归触发的前提不成立，故无需 `is_advisor_subcall` 之类标记。子调用复用 forwarder（享受熔断与用量记录），通过「复用 executor 已选定的 provider、绕开重选」满足钉死。

## 被否的替代方案

- **独立配置 advisor 专用端点**：能指向强模型、隔离账单，但降级场景下「同一端点跑 executor 就能跑 advisor」，多一处配置无收益。
- **注入 advisor 引导到 system / 工具 description**：Claude Code 已注入，重复注入会破坏 cache 前缀；非 Claude Code executor 场景被明确排除在外（本工具只在 Claude Code 用）。
- **`is_advisor_subcall` 防重入标记**：为不会发生的场景加保险，违反「不做不可能场景的错误处理」。
- **直连裸调（绕过 forwarder）**：会丢掉熔断与用量记录，与「成本要记录」冲突。
