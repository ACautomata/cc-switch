# advisor 本地降级：配置建模（端点判定 / advisor_tier / 缓存 / 回注 / 校验）

**状态**：accepted（issue #8 决策）

承接 ADR-0001（#6，后端渠道与触发时机），本 ADR 记录 advisor 本地降级的**配置建模**——开关、取值、默认值如何暴露给用户、落在哪个配置结构上。五道题全部通过 `/grilling` 与真人敲定。

## 决策与理由

**1. 端点「是否支持 advisor」的判定：纯 base_url 启发式，零配置。**
base_url 主机名是 Claude 官方域（`api.anthropic.com` / `api.claude.com` 等）→ 关闭本地降级、原生 advisor 透传；**其余一律默认本地降级**。无手动覆盖开关、无错误指纹匹配。
理由：「端点认不认识 advisor」的报错发生在 **executor 主请求**上，而本地降级要在主请求发出**之前**就改写好——等看到 400 时已牺牲一次请求；且「静默吞掉」型端点（OpenAI 兼容网关）对 advisor 块**返回 200 不报错**（`issue-4` F5/I3），try-catch 永远没有可 catch 的东西。base_url 恰好绕开这个死穴：不支持的端点主机名都不是官方域，默认即被降级，无需等任何信号。**配置上因此无需新增「是否支持 advisor」字段。**

**2. advisor 模型档位：由 Claude Code 客户端在工具块 `model` 字段给出，cc-switch 只后端映射——前端零配置。**
**这不是 cc-switch 的用户配置项。** Claude Code 客户端发来的 advisor 工具块 `{type:"advisor_20260301", name:"advisor", model, ...}` 里，`model` 字段已给出 advisor 档位（fable/opus/sonnet 档名或具体模型 ID）。cc-switch 后端**被动读取该值**，走既有 `model_mapper` 分档映射成第三方模型；未配该档映射时回落 executor 已映射模型。**前端 `src/types.ts` / `proxy.ts` 零改动、零表单**。
理由：「advisor 用哪档」是客户端的语义决定，cc-switch 作为代理不该替用户重选；复用既有 `model_mapper` 即可，无需新建配置。
**更正**：此项**推翻了 #6 / ADR-0001** 中「用户显式设 `advisor_tier`（fable/opus/sonnet，默认 fable）落 `ProviderMeta`」的提法——`advisor_tier` 不是用户配置，而是客户端 `model` 字段的后端透传映射；「默认 fable」是 Claude Code 自己发的值，非 cc-switch 的默认。

**3. advisor 侧 ephemeral 缓存：暴露开关、默认开；不支持则关掉重试。**
advisor 子调用注入 `cache_control`。若端点因 `cache_control` 报错（400 之类），去掉缓存**重试一次**。理由：官方缓存作用于 advisor 侧（转录前缀稳定，≥3 次回本），值得默认开；但第三方端点对 `cache_control` 支持参差，报错时优雅回退而非整体失败。此处保留极窄 try-catch，与题 1「触发判定无指纹」不矛盾——触发仍纯靠 base_url，这里只是缓存优化失败时的回退。

**4. 回注协议：默认 `advisor_tool_result`，报错自动回退普通 `tool_result`，逐 Provider 留手动覆盖。**
理由：忠实官方块形状语义最准，但第三方 executor 对未知块 `type` 容忍度未知（#5 D2，待 #7 实测）。故默认发 `advisor_tool_result{content: advisor_result{text}}`，下游报错则自动回退普通 `tool_result`；逐 Provider 留手动覆盖字段，供 #7 实测后标定已知不兼容端点。

**5. 配对校验：只查物理硬约束（advisor 上下文窗装得下整段转录）。**
官方「advisor ≥ executor」是服务端强制 400，但本地降级下无 Anthropic 服务端执行、官方「Claude × Claude」配对表对第三方模型语义不适用（`issue-4` F17/F18）。故本地**只校验**「advisor 上下文窗 ≥ executor 完整转录长度」（官方 `prompt_too_long` 语义，纯靠 `max_input_tokens` 可判，可靠、无假阳性）；**能力配对交给用户**，不做能力档启发式警告。

## 被否的替代方案

- **默认支持 advisor（opt-out 声明）/ 运行时探测 / 错误指纹匹配**（题 1）：对「静默吞掉」型端点会静默失效或无信号可捕（`issue-4` I3/I5）。
- **把 advisor 档位建成 cc-switch 用户配置（`ProviderMeta.advisor_tier` 三档表单 / 全局配置）**（题 2）：「advisor 用哪档」是客户端语义决定，代理不该替用户重选；复用既有 `model_mapper` 后端映射即可，新建配置无收益且会覆盖客户端意图。
- **不暴露缓存开关 / 默认关**（题 3）：放弃官方「前缀稳定、≥3 次回本」的收益。
- **回注写死回退普通 `tool_result` / 纯手动开关无自动回退**（题 4）：前者丢官方块语义，后者把兼容负担全推给用户。
- **能力档启发式警告 / 完全不校验**（题 5）：前者需自维护第三方模型能力序、有假阳性；后者把 `prompt_too_long` 直接甩给上游、体验差。
