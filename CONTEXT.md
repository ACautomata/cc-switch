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
- **advisor 模型**：由客户端 advisor 工具块的 `model` 字段给出（见下方「配置建模」节）；后端走 `model_mapper` 分档映射，未配该档映射时回落到「沿用 executor 已映射好的那个模型」。~~（#6 原表述「用户显式设定 advisor 档位」已被 #8 更正为非用户配置）~~
- **max_tokens 沿用 executor**：advisor 子调用的 `max_tokens` 直接沿用 executor 请求里的值。
- **成本只记录不熔断**：advisor 的 token 计入 cc-switch 用量统计；不单独做硬熔断（端点健康由既有熔断器负责）。「按哪个维度建模」见下方「用量建模（非流式）」节（issue #14）。

### 触发时机（仅 Claude Code 场景）

- **cc-switch 不注入任何「何时调用 advisor」的引导**。本工具仅在 **Claude Code 客户端**场景使用；客户端已把「何时调用」注入到它自己的 system prompt，并把 advisor 工具放进 `tools[]`。executor 自行判断何时发 `tool_use`。
- **cc-switch 是被动的**：在请求定稿段剥离 `advisor_20260301` server-tool 块 → 替换为普通客户端工具 `advisor`（无参数）→ 捕获 executor 发来的 `tool_use` → 本地 advisor 推理 → 回注。不 nudge、不改写 executor 的 system。
- **节流靠透传**：`max_uses` 原样透传客户端给定的值；客户端未设则 cc-switch 不加自己的上限。

### 防重入

- **不做防重入**：advisor 子调用的 `tools` 为空（含 Oracle 系统提示 + 完整转录，无 advisor 工具），递归前提不成立，无需 `is_advisor_subcall` 之类标记。

### 配置建模（issue #8）

- **官方端点判定**：是否走本地降级，纯按 base_url 启发式——base_url 主机名是 Claude 官方域（`api.anthropic.com` / `api.claude.com` 等）则关闭本地降级、原生 advisor 透传；其余一律默认本地降级。零配置、无手动覆盖、无错误指纹匹配。
- **降级触发与报错判断的分界**：端点认不认识 advisor 的报错发生在 executor 主请求上、且本地降级要在主请求发出**之前**判定，「静默吞掉」型端点还返回 200 假成功——故触发判定不能靠 try-catch，只能用 base_url 先验。反之，**advisor 子调用 / 回注**是 cc-switch 自己发起或能直接观察响应的，这两处（缓存、回注块）才能用报错兜底。
- **advisor 模型档位**：**由 Claude Code 客户端决定**——客户端 advisor 工具块 `{type:"advisor_20260301", name:"advisor", model, ...}` 的 `model` 字段给出档位（fable/opus/sonnet 档名或具体模型 ID）。cc-switch **不做用户配置、前端零改动**；后端被动读取该 `model` 值，走既有 `model_mapper` 分档映射成第三方模型，未配该档映射时回落 executor 已映射模型。（更正 #6 的「用户显式设 `advisor_tier`」提法——它非用户配置。）
- **advisor 侧缓存开关**：暴露、默认开，子调用注入 `cache_control`；端点因 `cache_control` 报错则去掉缓存重试一次。
- **回注协议偏好**：默认 `advisor_tool_result{content: advisor_result{text}}`；下游报错自动回退普通 `tool_result`；逐 Provider 留手动覆盖（供 #7 实测后标定）。
- **配对校验**：只查物理硬约束「advisor 上下文窗 ≥ executor 完整转录长度」（`max_input_tokens` 可判）；能力配对交给用户，不做能力档启发式警告。

### 用量建模（非流式，issue #14）

本地 advisor 子调用自身消耗的 token 如何进 cc-switch 用量统计（只覆盖非流式；流式 `SseUsageCollector` 不在此范围）：

- **独立 advisor 行**：advisor 是一次独立上游 `/v1/messages` 调用（不同 `message_id`、分档映射后常与 executor 不同 `model`），在 `proxy_request_logs` 里**独立一行**、**不并入** executor 顶层 `usage`——对应官方 `iterations[]` 的 `advisor_message`/`message` 分行语义。
- **仅靠 `model` 列区分**：不引入 provider_id 命名空间、不新增 schema 列；advisor 的归属靠它实际命中的 `model` 体现。
- **按实际命中模型计价**：advisor 行的 `model`/`request_model`/`pricing_model` 都写分档映射后实际命中的模型（费率可能不同于 executor），零额外处理。
- **计入总用量总成本、不单独报表**：advisor 行落同一张表、按自身费率计价后自然计入 provider/全局总账（承接 #6「成本只记录不熔断」），不单列 advisor 成本项。
- **去重/session 安全**：`request_id` 由 advisor 自身 `message_id` 派生，天然唯一；官方契约 executor 顶层 `usage` 本不含 advisor token，session 导入器不会与代理行双计。

### 错误降级（issue #16）

- **失败不打断**：本地 advisor 子调用失败（限流 / 超时 / 超载 / 上下文超窗等）时，cc-switch **回注一个「advisor 本次无建议」的信号**让 executor 继续，**绝不整请求 5xx**（忠实官方「request itself does not fail」）。**不发 `max_uses_exceeded`**——`max_uses` 透传客户端、本地无服务端上限可触发。
- **失败→错误码映射**（仅作内部日志/用量记录的分类归因 + 回注明文的「原因」用词）：429→`too_many_requests`、529/`overloaded`→`overloaded`、超时→`execution_time_exceeded`、400 超窗→`prompt_too_long`、其余一切→`unavailable`。
- **错误形状**：复用原型 #7 的**普通 `tool_result` 明文回退**（与成功路径同一形状），`content` 明文写「advisor 不可用 + 原因」。六错误码**不作为结构化块 type 透给第三方端点**（第三方对 `advisor_tool_result` 未知块 400）；内联 `advisor_tool_result_error` 块仅作契约参照落 ADR-0003，不进生产回注体。
- **流式保活（既定约束）**：流式路径在 advisor 推理暂停期需发 SSE `ping` 保活（承接 #5，`create_logged_passthrough_stream` 首字节/静默超时假设）。
