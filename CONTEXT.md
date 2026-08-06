# CONTEXT

本文件是 cc-switch 的**领域词汇表**（glossary）。只收录领域概念与术语，不含实现细节。

## advisor 本地降级（advisor local fallback）

当上游第三方 Claude API **不支持** `advisor` 服务器工具时，cc-switch 代理层把它**优雅降级**为一次本地 advisor 推理的能力。

### 核心角色

- **executor（执行模型）**：被代理的主模型。它把 advisor 当作一个**客户端工具**调用（普通 `tool_use`，id 形如 `toolu_…`），供代理捕获。
- **advisor（顾问模型）**：本地提供建议的强模型。代理捕获 executor 的 advisor 调用后，**自己**用 Oracle 系统提示词 + 完整对话转录，向端点起一次普通、不带工具的 `/v1/messages` 非流式调用，单轮即返回文本建议。

### 关键机制

- **advisor_tool_result（回注）**：把 advisor 返回的建议包成 `advisor_tool_result{content: advisor_result{text}}` 的形状，在下一条 user 消息里回注给 executor，让它带着建议继续。
- **Provider（供应商）**：cc-switch 的凭证与端点 SSOT。`settings_config.env` 装着 `ANTHROPIC_BASE_URL` / 认证 token 等；`meta`（`ProviderMeta`）挂着该端点的可选非 live 配置。
- **分档映射（model tier mapping）**：`model_mapper` 按模型名中的档位（fable/opus/sonnet/haiku）把请求模型映射到该 Provider 配置的 `ANTHROPIC_DEFAULT_{档位}_MODEL`，落不到档走 `ANTHROPIC_MODEL` 默认。executor 与 advisor **复用同一张分档映射表**。

### 已敲定的设计决策（issue #6）

- **端点/凭证沿用 executor**：advisor 子调用复用本次请求**实际命中**的那个 Provider 的 `base_url`+凭证；其开销计入该 Provider 的账单/限流。
- **failover 钉死**：一段对话内，advisor 子调用钉死在本次 executor 实际命中的 Provider，不随每次调用重新走 router 选择。
- **advisor 档位（advisor tier）**：用户显式设定 advisor 用哪一档（`fable`/`opus`/`sonnet`，默认 `fable`）。运行时取该档位对应的 `ANTHROPIC_DEFAULT_{档位}_MODEL` 映射结果作为 advisor 模型；未配置该档映射时，回落到「沿用 executor 已映射好的那个模型」。
- **max_tokens 沿用 executor**：advisor 子调用的 `max_tokens` 直接沿用 executor 请求里的值。
- **成本只记录不熔断**：advisor 的 token 计入 cc-switch 用量统计；不单独做硬熔断（端点健康由既有熔断器负责）。

### 触发时机（仅 Claude Code 场景）

- **cc-switch 不注入任何「何时调用 advisor」的引导**。本工具仅在 **Claude Code 客户端**场景使用；客户端已把「何时调用」注入到它自己的 system prompt，并把 advisor 工具放进 `tools[]`。executor 自行判断何时发 `tool_use`。
- **cc-switch 是被动的**：在请求定稿段剥离 `advisor_20260301` server-tool 块 → 替换为普通客户端工具 `advisor`（无参数）→ 捕获 executor 发来的 `tool_use` → 本地 advisor 推理 → 回注。不 nudge、不改写 executor 的 system。
- **节流靠透传**：`max_uses` 原样透传客户端给定的值；客户端未设则 cc-switch 不加自己的上限。

### 防重入

- **不做防重入**：advisor 子调用的 `tools` 为空（含 Oracle 系统提示 + 完整转录，无 advisor 工具），递归前提不成立，无需 `is_advisor_subcall` 之类标记。
