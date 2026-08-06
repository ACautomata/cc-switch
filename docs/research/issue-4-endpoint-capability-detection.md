# 研究:第三方端点「不支持 advisor」的判定与本地降级触发

> Wayfinder 研究票 [issue #4](https://github.com/ACautomata/cc-switch/issues/4) 的成果。父图 [issue #2](https://github.com/ACautomata/cc-switch/issues/2)。
>
> **一手来源**(下文每条结论回链):
> - Anthropic 官方文档《Advisor tool》 `https://platform.claude.com/docs/en/agents-and-tools/tool-use/advisor-tool`(工具形状、`advisor-tool-2026-03-01` beta 头必要性、Model compatibility 表、Platform availability)
> - Anthropic 官方文档《API errors》 `https://platform.claude.com/docs/en/api/errors`(400 `invalid_request_error` 触发面)
> - Anthropic 官方文档《List Models》 `https://platform.claude.com/docs/en/api/models/list`(`/v1/models` 的 `capabilities` 结构)
> - 仓库源码:`songquanpeng/one-api` `relay/adaptor/anthropic/{model,main}.go`、`QuantumNous/new-api` `relay/claude_handler.go` + `relay/channel/claude/adaptor.go` + `relay/channel/claude/relay-claude.go`、`1rgs/claude-code-proxy` `server.py`、`musistudio/claude-code-router` `packages/core/src/gateway/request/pipeline.ts`
> - SDK 源码:`anthropics/anthropic-sdk-python` `src/anthropic/types/anthropic_beta_param.py`、`capability_support.py`
> - GitHub issue(实证报错):anthropics/claude-code#70563、musistudio/claude-code-router#1528、BerriAI/litellm#27655 / #22946 / #25516、anomalyco/opencode#21789、openclaw/openclaw#68006
>
> 「官方形状」细节(工具定义 / `server_tool_use` / `advisor_tool_result` / 错误码 / 流式)直接复用 [`issue-5-advisor-client-shape.md`](./issue-5-advisor-client-shape.md),本票不重述。本票只回答**事实层**:如何可靠判定某第三方端点不支持 advisor、从而触发本地降级。**不下最终方案结论**(方案由 grilling 票定)。

本票回答工单四问:(1) 第三方端点不支持 `type:"advisor_20260301"` 时实际返回什么、不同端点类型行为是否不同;(2) 代理应如何检测「不支持」——预检探测 / 配置表 / 捕获错误重试;(3) `anthropic-beta: advisor-tool-2026-03-01` 头应剥离还是透传;(4) 「executor ≥ advisor」官方配对校验在本地回退下是否仍需强制、如何放宽。

---

## TL;DR(给本地代理的结论)

- **「不支持」没有一个统一的报错形状**,按端点类型分裂成三类(问题 1):
  - **OpenAI 兼容网关**(one-api 主线、claude-code-proxy):Anthropic 工具被建模成固定 `{name,description,input_schema}` struct,**`type`/`model` 字段被静默丢弃**——advisor 工具被**静默降级为普通 function tool**,不报错,后端永远不知道它是 advisor。这类端点**不会产生可捕获的错误信号**,只能预检或配置表识别。
  - **Anthropic 透传型中转**(new-api `RelayFormatClaude` / 各 passthrough 网关):advisor 工具与 beta 头**原样转发给上游 Anthropic**;若上游是不支持 advisor 的 Bedrock/Vertex/自建校验器,由上游报 `400`。
  - **复刻了 Anthropic 校验的第三方端点**:对未知 beta 头报 `400 Unexpected value(s) 'advisor-tool-2026-03-01' for the 'anthropic-beta' header`(claude-code#70563 实证),对「beta-only 字段缺头」报 `400 invalid_request_error: <field>: Extra inputs are not permitted`(claude-code-router#1528 实证)。
- **检测策略(问题 2)**:三条路各有死穴——**预检探测**死于「官方 `/v1/models` 的 `capabilities` 里根本没有 advisor 字段」,无权威探测面;**配置表**死于「端点形态不可枚举、同一中转站可切换 passthrough 开关」;**捕获错误重试**死于「OpenAI 兼容网关静默吞掉 advisor、根本不报错」。**没有任何单一路径可靠**,实操上必然走向「配置表为主(用户显式声明该 endpoint 不支持 advisor)+ 错误指纹匹配兜底(识别上面两类 400 文案)」的组合——但具体组合方式属 grilling 票,本票只给证据。
- **beta 头(问题 3)**:**转发第三方时应剥离**。第三方端点对 `advisor-tool-2026-03-01` 报 400 是实测事实;且本地复刻用明文 `advisor_result{text}` 变体,整条链路不依赖该 beta(见 issue-5)。**注意**:「应剥离」是**实证 + 社区 workaround**(`CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1`、网关过滤 beta 头)支撑的结论,**Anthropic 官方未公开指导**代理该剥离还是透传(claude-code#70563 被关 duplicate、无官方回应)——如实标注。
- **配对校验(问题 4)**:官方「advisor ≥ executor 能力」是**服务端强制的 400 校验**(「If you request an invalid pair, the API returns a `400 invalid_request_error`」)。本地回退下**没有 Anthropic 服务端替你做这个校验**——advisor 由 proxy 自己编排、打到任意第三方模型,官方那张「Claude 模型 × Claude 模型」配对表**在语义上不再适用**(第三方 executor/advisor 很可能根本不是 Claude 模型)。是否复刻、如何放宽属方案决策,本票只确认「官方强制 + 本地无强制来源」这一事实。

---

## 问题 1 —— 第三方端点不支持 `type:"advisor_20260301"` 时实际返回什么

### 事实(有据可查)

**F1. 官方 advisor 是 beta、需显式 beta 头,且只在部分平台可用。**
官方《Advisor tool》:「The advisor tool is in beta. Include the beta header `advisor-tool-2026-03-01` in your requests.」;「Platform availability」节:「The advisor tool is available in beta on the Claude API and on Claude Platform on AWS. It is not currently available on Amazon Bedrock, Google Cloud, or Microsoft Foundry.」
→ 权威边界:**Bedrock / Vertex / Foundry 确定不支持 advisor**;Claude API 直连与 Claude Platform on AWS 支持。

**F2. 复刻 Anthropic 校验的第三方端点,对未知 beta 头报 `400 Unexpected value(s)`。**
claude-code#70563 实证(用户把 `ANTHROPIC_BASE_URL` 指向第三方端点):
```
API Error: 400 Unexpected value(s) `advisor-tool-2026-03-01` for the `anthropic-beta` header.
Please consult our documentation at docs.claude.com or try again without the header.
```
该 issue 被关为 duplicate、**无 Anthropic 官方回应**;Claude Code v2.1.187 起会自动注入该实验性 beta 头,`disableExperimentalBetas:true` 对此头**不生效**。同类实证:zed#42715(`Unexpected value(s) 'prompt-caching-2024-07-31'`)、claude-code#13770(`Unexpected value(s) ... oauth-2025-04-20` → 400 `invalid_request_error`)、claude-code#11672(Bedrock `Unexpected value for 'anthropic-beta' header`)。
→ 这是一类**可识别、可捕获**的错误指纹。

**F3. Anthropic 对「请求体含 beta-only 字段但缺对应 beta 头」报 `400 ... Extra inputs are not permitted`。**
claude-code-router#1528 实证:网关把客户端 `anthropic-beta` 覆写成只剩 `oauth-2025-04-20`,导致 beta-only 字段 `context_management` 缺头,Anthropic 报:
```
400 invalid_request_error: context_management: Extra inputs are not permitted
```
该 issue 还指出同类 beta-only 字段:「`context_management`、`effort`、tool `input_examples`」。
→ 印证 Anthropic 对请求体**未授权字段是 strict 校验**("Extra inputs are not permitted"),而非静默忽略。

**F4. Anthropic 对「判别联合 type 字段非法值」的标准报错是 `Input tag 'X' ... does not match any of the expected tags`。**
实证(同为 `type` 判别字段):`thinking.type:"adaptive"` → `Input tag 'adaptive' found using 'type' does not match any of the expected tags: 'disabled', 'enabled'`(claude-code#43258、new-api#3039);`document.source.type:"file"` → `Input tag 'file' ... does not match any of the expected tags: 'base64','content','text','url'`(anthropic-sdk-ruby#127,且需 `files-api-2025-04-14` 头才解锁)。

**F5. OpenAI 兼容网关(one-api 主线 / claude-code-proxy)把 Anthropic 工具建模为固定 struct,`type` 字段被静默丢弃。**
- one-api `relay/adaptor/anthropic/model.go`:`type Tool struct { Name, Description string; InputSchema InputSchema }`——**无 `Type` 字段**。Go `encoding/json` 反序列化对未知字段默认**静默丢弃**;收到 `{"type":"advisor_20260301","name":"advisor","model":"..."}` 会变成 `Tool{Name:"advisor", InputSchema:{}}`,`type`/`model` 丢失。
- claude-code-proxy `server.py`:`class Tool(BaseModel): name: str; description: Optional[str]; input_schema: Dict[str,Any]`——**无 `type` 字段**,Pydantic 默认 extra=ignore,同样静默丢弃。随后 `convert_anthropic_to_litellm` 把它转成 OpenAI `{"type":"function","function":{name,description,parameters}}` 发往后端。整个 `server.py` **从不读取 `anthropic-beta` 头**(只解析 body),也不向 LiteLLM 转发该头。
→ 这类端点**不报错**:advisor 被静默降级为普通 function tool。

**F6. LiteLLM 对未知工具类型,修复前「盲转发→下游 400」,修复后「丢弃 + warning」。**
litellm#27655:Responses→Chat 转换的 catch-all `else` 曾盲转发未知 tool 类型,下游报 `400 "tools[N].type: type is illegal"`;受影响类型含 `local_shell`/`file_search`/`namespace`/`mcp` 等。修复(PR #27652)改为 **丢弃未知类型 + warning log**。litellm#22946:LiteLLM 的 Anthropic **原生 passthrough** 端点 `/v1/messages` **逐字转发 messages 不消毒**,advisor/工具块会原样到 Anthropic,由 Anthropic 报错。

**F7. new-api(透传型中转)对 Claude 原生请求默认规范化、可配置透传;beta 头原样转发上游。**
- `relay/claude_handler.go:135-200`:默认走 `ConvertRequest(..., RelayFormatOpenAI, ...)` 规范化;仅当 `PassThroughRequestEnabled` 或 channel 的 `PassThroughBodyEnabled` 开启时才 `NewReplayableBodyReader` 透传原始 body(仍经 `RemoveDisabledFields` 按配置删字段)。
- `relay/channel/claude/adaptor.go:76-78`:`anthropicBeta := c.Request.Header.Get("anthropic-beta"); if anthropicBeta != "" { req.Set("anthropic-beta", anthropicBeta) }`——**客户端 beta 头原样透传给上游**,不剥离、不校验。
→ new-api 自己不对 advisor 报错;是否报错取决于上游(Anthropic 直连则支持、Bedrock/Vertex 则由上游拒)。

**F8. 整个第三方生态的「Anthropic 工具」模型都假设客户端工具形状,advisor 因形状不同而不被识别。**
- opencode#21789:`advisor_20260301` 类型**不在任何已发布版本**,编译产物 `No case for 'anthropic.advisor_20260301'`。
- openclaw#68006:`convertAnthropicTools()` **硬编码所有工具为 `{name, description, input_schema}`**,而「The advisor tool type has a different shape(`type`、`model`、`max_uses` instead of `description`/`input_schema`)」。

### 推断(基于证据,非一手实测)

**I1. 三类端点的行为分野**(由 F5/F6/F7 + F2/F3 归纳):

| 端点类型 | 收到 advisor 工具的行为 | 是否产生可捕获错误 |
| --- | --- | --- |
| **OpenAI 兼容网关**(one-api 主线、claude-code-proxy) | `type`/`model` 静默丢弃,降级为普通 function tool(F5) | **否**——静默吞掉 |
| **Anthropic 透传中转**(new-api `RelayFormatClaude`、LiteLLM passthrough、claude-code-router) | 原样转发给上游;上游是 Bedrock/Vertex 则由上游报 400(F6/F7) | 取决于上游 |
| **复刻 Anthropic 校验的端点** | 对未知 beta 头报 `Unexpected value(s)`(F2);对缺头的 beta-only 字段报 `Extra inputs are not permitted`(F3) | **是**,两类 400 指纹 |

**I2. advisor 工具 type 在 Anthropic 直连的确切报错文案,无公开一手证据。** 由 F4 类比,**推断**形如 `400 invalid_request_error: tools.N: Input tag 'advisor_20260301' found using 'type' does not match any of the expected tags: ...`(thinking/document.source 同型报错外推)。**但 Anthropic 官方《API errors》页列举的 400 触发面里,并没有「未知工具 type」这一条**(该页只列 prefill/thinking/schema 等),故确切文案**待真实第三方实测**。

**I3. 「静默吞掉」比「报错」更危险。** OpenAI 兼容网关(F5)不报错地把 advisor 降级为普通工具——此时 executor 会真的去「调用」这个它以为是本地工具的 advisor,产生 `tool_use{name:"advisor"}`,但**没有任何建议会被注入**,语义静默破坏。这类失败**没有任何 HTTP 信号**,只能靠预检/配置表识别,无法靠错误捕获。

---

## 问题 2 —— 代理应如何检测「不支持」:预检探测 / 配置表 / 捕获错误重试

### 事实(有据可查)

**F9. 官方 `/v1/models` 提供 `capabilities` 结构,但不含 advisor 字段。**
官方《List Models》:每个 `ModelInfo.capabilities` 列出 `batch`/`citations`/`code_execution`/`context_management`/`effort`/`image_input`/`pdf_input`/`structured_outputs`/`thinking`,各项为 `CapabilitySupport{supported: bool}`(SDK 侧 `capability_support.py` 同构)。**遍历整个 capabilities 列表,没有 advisor / advisor_tool 字段**——官方能力探测面不覆盖 advisor。
→ **预检探测没有权威数据源**:无法通过 `/v1/models` 探知某端点是否支持 advisor。

**F10. Python SDK 客户端不校验 beta 头合法性,beta 校验全在服务端。**
`anthropic_beta_param.py`:`AnthropicBetaParam = Union[str, Literal[...]]`——`str` 分支使 SDK **接受任意字符串**作 beta 值,原样放进 `anthropic-beta` 头发送;校验发生在服务端(印证 F2/F3 的服务端 400)。
→ 客户端/代理无法在发送前靠 SDK 预知 beta 是否被接受。

**F11. 「捕获错误重试」有可识别的错误指纹,但只覆盖「会报错」的端点。**
可用指纹(来自 F2/F3):`Unexpected value(s) '...' for the 'anthropic-beta' header`、`<field>: Extra inputs are not permitted`、以及 I2 推断的 `Input tag 'advisor_20260301' ...`。但 F5 的 OpenAI 兼容网关**不产生这些错误**(静默吞掉),错误捕获对它们失效。

**F12. 配置表的可行性受限于「同一端点行为可切换」。**
new-api 的 `PassThroughRequestEnabled` / channel 级 `PassThroughBodyEnabled` 是**运行时可配的开关**(F7)——同一个 new-api 实例,开/关 passthrough 后对 advisor 的行为截然不同(透传上游 vs 规范化丢弃)。配置表若只记「host → 支持与否」,会被这类开关打脸;需记到「host + 该实例的转发模式」粒度。

### 推断(基于证据,非一手实测)

**I4. 三条检测路径的可靠性 / 成本对比:**

| 路径 | 可靠性 | 成本 | 死穴 |
| --- | --- | --- | --- |
| **预检探测**(打 `/v1/models` 或发空 advisor 请求探) | 低 | 每端点至少一次额外请求 | F9:官方 `capabilities` 无 advisor 字段,**无权威探测面**;发真实 advisor 探测请求本身就是一次计费调用,且对「静默吞掉」型端点探不出(它会返回 200 但没有真 advisor) |
| **已知不支持端点配置表** | 中-高(用户显式声明时) | 维护成本 | F12:同一中转站可切 passthrough;端点形态不可枚举;新端点默认偏哪边需定夺 |
| **捕获特定错误后重试降级** | 中(只覆盖报错型) | 一次失败请求的延迟 + 重试成本 | F5/I3:OpenAI 兼容网关静默吞掉,**无错误可捕**;I2:Anthropic 直连 advisor type 报错文案未实测,指纹不全 |

**I5. 没有任何单一路径可靠。** 「静默吞掉」型(F5)只能预检/配置表;「报错」型(F2/F3)才能错误捕获;预检又无 advisor 探测面(F9)。**推断**实操收敛到「**配置表为主 + 错误指纹匹配兜底**」:让用户在 endpoint 配置里显式声明「该端点不支持 advisor → 走本地降级」,同时对已知两类 400 文案做指纹匹配作为运行时兜底。但具体默认取向(未声明时默认支持还是默认降级)、指纹库范围,属 grilling 票的方案决策,本票不定。

**I6. 一个低成本预检变体(推断,未验证):** 对疑似「复刻 Anthropic 校验」的端点,可发一个**带 `anthropic-beta: advisor-tool-2026-03-01` 头但 body 极简**的探测请求——若报 F2 的 `Unexpected value(s)` 即知该端点不认识此 beta。但这对「静默吞掉」型(OpenAI 兼容网关根本不读 beta 头,F5)仍无效,且会污染用量/产生一次请求。**是否值得做属方案决策。**

---

## 问题 3 —— `anthropic-beta: advisor-tool-2026-03-01` 头应剥离还是透传

### 事实(有据可查)

**F13. 第三方端点对该 beta 头报 400,是实测事实(F2)。** claude-code#70563 的 `400 Unexpected value(s) 'advisor-tool-2026-03-01'` 直接由「自定义 `ANTHROPIC_BASE_URL` 指向第三方端点」触发。

**F14. 社区标准 workaround 就是「别让该头到达第三方端点」。** claude-code#70563 等 issue 的官方/社区解法:`CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS=1`(让客户端不发),或用户提议的「代理在路由到非 Anthropic 端点时不过滤则剥离」。**注意:claude-code#70563 被关 duplicate、无 Anthropic 官方回应;Anthropic 未公开声明「代理应剥离 beta 头」**——所以「应剥离」是**实证 + 社区 workaround**支撑的结论,**非官方指导**。

**F15. 本地复刻用明文 `advisor_result{text}` 变体,整条链路不依赖该 beta 头。**(复用 issue-5 结论)本地模式下 advisor 由 proxy 编排、建议以明文回注,`advisor-tool-2026-03-01` 头对本地逻辑无用。

**F16. 网关覆写/剥离 `anthropic-beta` 头是真实发生的行为。** claude-code-router#1528:网关把客户端 beta 头覆写成只剩 `oauth-2025-04-20`(引发 F3 的 400)。印证「代理改 beta 头」在生态里是常态操作。

### 推断(基于证据)

**I7. 转发第三方时应剥离该头。** 理由:(a) 透传会触发 F13 的 400;(b) 本地复刻不需要它(F15);(c) 即便端点恰好是「透传到支持 advisor 的 Anthropic 直连」,那种情形下根本不需要本地降级、也就不在本票讨论路径内。**故在「已判定走本地降级」的路径上,剥离是无害且必要的。**

**I8. 剥离的边界(推断):** 只剥 `advisor-tool-2026-03-01` 这一个值,**不要清空整个 `anthropic-beta` 头**——其他 beta 值(如 `prompt-caching-2024-07-31`)可能是端点需要且支持的。F16 的教训正是「粗暴覆写整个头」导致的 400。多值 beta 头应按值过滤、保留其余。

---

## 问题 4 —— 「executor ≥ advisor 能力」官方配对校验在本地回退下是否仍需强制、如何放宽

### 事实(有据可查)

**F17. 官方配对规则是服务端强制的 400 校验。** 官方《Advisor tool》「Model compatibility」节逐字:
> The executor model (the top-level `model` field) and the advisor model (the `model` field inside the tool definition) must form a valid pair. The advisor must be Claude Sonnet 4.6 or a more capable model, and it must be at least as capable as the executor. Models of equal capability (for example, Claude Opus 4.7 and Claude Opus 4.8) can advise each other.
> If you request an invalid pair, the API returns a `400 invalid_request_error` naming the unsupported combination.

配对表(逐字,executor 行 × advisor 列,`[]` 内为可配对的 advisor):

| Executor | 可配对 Advisor |
| --- | --- |
| Haiku 4.5 | Mythos 5, Fable 5, Opus 5, Opus 4.8, Opus 4.7, Opus 4.6, Sonnet 5, Sonnet 4.6 |
| Sonnet 4.6 | Mythos 5, Fable 5, Opus 5, Opus 4.8, Opus 4.7, Opus 4.6, Sonnet 5, Sonnet 4.6 |
| Sonnet 5 | Mythos 5, Fable 5, Opus 5, Opus 4.8, Opus 4.7, Sonnet 5 |
| Opus 4.6 | Mythos 5, Fable 5, Opus 5, Opus 4.8, Opus 4.7, Opus 4.6, Sonnet 5 |
| Opus 4.7 | Mythos 5, Fable 5, Opus 5, Opus 4.8, Opus 4.7 |
| Opus 4.8 | Mythos 5, Fable 5, Opus 5, Opus 4.8, Opus 4.7 |
| Opus 5 | Mythos 5, Fable 5, Opus 5 |
| Fable 5 | Fable 5, Opus 5 |
| Mythos 5 | Mythos 5, Opus 5 |

规则两点:(a) advisor 必须 ≥ Sonnet 4.6 能力;(b) advisor 必须 ≥ executor 能力(同级可互配)。

**F18. 该校验是「Claude 模型 × Claude 模型」的配对表**,表内全是 Anthropic 模型 ID。本地回退下 advisor/executor 很可能打到**第三方模型**(GPT/Gemini/DeepSeek/Kimi 等,claude-code-router 的 presets 即含 openai/gemini/deepseek/moonshot/zhipu 等),这些模型**不在官方配对表内**,无官方「能力序」可对。

### 推断(基于证据)

**I9. 本地回退下「无强制来源」。** 官方校验由 Anthropic 服务端执行(F17);本地模式下 advisor 由 proxy 编排、打到任意第三方模型,**Anthropic 服务端不参与、无人替你跑这个 400**。因此「是否仍强制」不是「能否」问题,而是「要不要自己复刻一个本地校验」的设计决策。

**I10. 官方配对表在本地语义上不直接适用。** 它绑定 Claude 模型的官方能力序;第三方模型无对应表项。若本地要复刻,需自行定义一套「能力序」(如对本地配置的模型维护一个 tier 标注),或用更弱的启发(如「advisor 上下文窗 ≥ executor 本轮转录长度」这类可计算约束,F 中 `prompt_too_long` 错误码的官方语义支持「advisor 须装得下整段转录」这一硬约束)。**但「复刻严校验 / 只校验硬约束 / 完全不校验交给用户」属 grilling 票方案决策,本票只确认 F17(官方强制)+ F18(表是 Claude×Claude)两个事实,不定本地方案。**

**I11. 一个可计算的本地硬约束(推断):** 即便放宽「能力配对」,「advisor 模型的上下文窗必须装得下 executor 的完整转录」是物理硬约束(官方 `prompt_too_long` 错误码的存在即承认它,见 issue-5 §4)。本地复刻时这条**应保留**为校验项,因为它不依赖「能力序」、纯靠 `max_input_tokens` 可判。

---

## 来源

**一手(权威)**
- Anthropic《Advisor tool》 `https://platform.claude.com/docs/en/agents-and-tools/tool-use/advisor-tool` —— beta 头必要性(F1)、Model compatibility 表 + 服务端 400 强制(F17)、Platform availability(F1)、`prompt_too_long` 错误码(I11)。
- Anthropic《API errors》 `https://platform.claude.com/docs/en/api/errors` —— 400 `invalid_request_error` 触发面;**佐证「未知工具 type 未被官方列为 400 触发项」**(I2)。
- Anthropic《List Models》 `https://platform.claude.com/docs/en/api/models/list` —— `/v1/models` `capabilities` 结构,**无 advisor 字段**(F9)。

**仓库源码(一手)**
- `songquanpeng/one-api` `relay/adaptor/anthropic/model.go`、`main.go` —— `Tool` struct 无 `type` 字段、Go 静默丢未知字段(F5);`ConvertRequest` 为 OpenAI→Claude 方向。
  `https://github.com/songquanpeng/one-api/blob/master/relay/adaptor/anthropic/model.go`
- `1rgs/claude-code-proxy` `server.py` —— `Tool` Pydantic model 无 `type`(extra=ignore 静默丢)、转 OpenAI function tool、不读/不转发 `anthropic-beta` 头(F5)。
  `https://github.com/1rgs/claude-code-proxy/blob/main/server.py`
- `QuantumNous/new-api` `relay/claude_handler.go`(passthrough 开关 F7/F12)、`relay/channel/claude/adaptor.go`(beta 头原样透传 F7)、`relay/channel/claude/relay-claude.go`(`RelayFormatClaude` 透传格式)。
  `https://github.com/QuantumNous/new-api/blob/main/relay/claude_handler.go`
- `musistudio/claude-code-router` `packages/core/src/gateway/request/pipeline.ts` —— passthrough 型网关,转发 body/headers,仅特定功能改 `/body/tools`(F7 同类);`providers/presets/` 含 openai/gemini/deepseek/moonshot/zhipu 等(F18)。
  `https://github.com/musistudio/claude-code-router/tree/main/packages/core/src/gateway/request`
- `anthropics/anthropic-sdk-python` `src/anthropic/types/anthropic_beta_param.py`(beta 客户端不校验 F10)、`capability_support.py`(`CapabilitySupport{supported:bool}` F9)。
  `https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/types/anthropic_beta_param.py`

**GitHub issue(实证报错,一手)**
- anthropics/claude-code#70563 —— `400 Unexpected value(s) 'advisor-tool-2026-03-01'`(F2/F13);关 duplicate、无官方回应(F14)。`https://github.com/anthropics/claude-code/issues/70563`
- musistudio/claude-code-router#1528 —— `400 invalid_request_error: context_management: Extra inputs are not permitted`(F3);网关覆写 beta 头(F16)。`https://github.com/musistudio/claude-code-router/issues/1528`
- BerriAI/litellm#27655 —— 未知 tool 类型盲转发→下游 `400 "tools[N].type: type is illegal"`,修复后丢弃+warning(F6)。`https://github.com/BerriAI/litellm/issues/27655`
- BerriAI/litellm#22946 —— Anthropic passthrough 逐字转发不消毒(F6)。`https://github.com/BerriAI/litellm/issues/22946`
- BerriAI/litellm#25516 —— LiteLLM 的 advisor 编排(非 Anthropic provider 剥离 `advisor_20260301`、本地编排;Bedrock 原生不支持);advisor 块不 round-trip 则 400(佐证 I 系列与 issue-5 同构方案的可行性参照)。`https://github.com/BerriAI/litellm/issues/25516`
- anomalyco/opencode#21789 —— `advisor_20260301` 无 case、不在已发布版本(F8)。`https://github.com/anomalyco/opencode/issues/21789`
- openclaw/openclaw#68006 —— `convertAnthropicTools()` 硬编码客户端工具形状,advisor 形状不符(F8)。`https://github.com/openclaw/openclaw/issues/68006`
- 同类 beta 头 400 佐证:zed#42715、claude-code#13770 / #11672 / #12429 / #43258、new-api#3039、anthropic-sdk-ruby#127(F2/F4 佐证)。

**交叉参照(同构方案,非本票依据)**
- LiteLLM《Advisor Tool》文档 `https://docs.litellm.ai/docs/completion/anthropic_advisor_tool` —— Anthropic 直连自动补 beta 头透传;非 Anthropic provider `AdvisorOrchestrationHandler` 剥离 `advisor_20260301` 改普通 function tool + 本地编排 + 回发剥离 Anthropic 专属块。**与本工单 destination 几乎同构**,可作端到端原型(#7)的现成参照实现。

**仓库内复用**
- [`issue-5-advisor-client-shape.md`](./issue-5-advisor-client-shape.md) —— 官方工具形状 / `server_tool_use` / `advisor_tool_result` / 六错误码 / 流式 / 明文 result 变体不依赖 beta 头(F15)。

---

## 待真实第三方实测清单

> 以下点**无公开一手证据**,只有打了真实第三方端点才能定死。喂给端到端原型票 #7。

1. **advisor 工具 type 在 Anthropic 兼容端点的确切报错文案**(I2):对复刻 Anthropic 校验的端点发 `{"type":"advisor_20260301",...}` + 正确 beta 头,记录是否真报 `Input tag 'advisor_20260301' ... does not match any of the expected tags`、确切 path(`tools.N`?)与 `error.type`。——决定错误指纹库能否覆盖这一类。
2. **「静默吞掉」型端点的实测确认**(F5/I3):找一个真实 one-api 主线 / claude-code-proxy 实例,发 advisor 工具,确认 (a) 是否真的 200 不报错、(b) executor 是否真把 advisor 当普通工具发起 `tool_use`、(c) `type`/`model` 是否真的丢失。——决定这类端点能否纯靠配置表识别。
3. **带 advisor 工具但剥掉 beta 头,Anthropic 直连的行为**(F15 边界):确认在无 `advisor-tool-2026-03-01` 头时,Anthropic 直连对 `type:"advisor_20260301"` 是报「未知工具 type」还是「需 beta 头」类 400,确切文案。——影响 I7 剥离边界的指纹设计。
4. **预检变体的有效性**(I6):对疑似「复刻校验」端点发「带 beta 头 + 极简 body」探测,确认能否稳定用 `Unexpected value(s)` 区分「认识/不认识 advisor beta」;以及对 OpenAI 兼容网关发同样探测是否真无信号。——决定预检是否值得做。
5. **第三方模型的「能力序」如何标定**(F18/I10):本地回退若要做配对校验,第三方 advisor/executor(GPT/Gemini/DeepSeek 等)的能力 tier 无可引用的公开权威表,需实测/人工标注一批主流模型的相对能力,或确认「只校验上下文窗硬约束(I11)、能力配对交给用户」。——这是 grilling 票定方案前必须实测/确认的输入。
6. **beta 头多值过滤的端点兼容性**(I8):实测「只剥 `advisor-tool-2026-03-01`、保留其他 beta 值」在真实第三方端点是否被接受(有的端点可能对 `anthropic-beta` 头整体白名单校验,部分值过滤仍触发 400)。
