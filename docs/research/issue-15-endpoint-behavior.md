# 研究:第三方端点对 advisor 工具块 / beta 头的真实行为实测

> Wayfinder 研究票 [issue #15](https://github.com/ACautomata/cc-switch/issues/15) 的成果。父图 [issue #2](https://github.com/ACautomata/cc-switch/issues/2)。
>
> **本票范围**:承接 #4「待真实第三方实测清单」的第 (2)(3) 项——两项**独立于「用量建模」「错误降级」决策**的端点行为,作为后续配对/触发决策的事实输入。原清单的 (1)(4)(5)(6) 仍留地图迷雾区,依赖 #14/#16 决策后再毕业。
>
> **一手来源**(下文每条结论回链):
> - Anthropic 官方文档《Advisor tool》 `https://platform.claude.com/docs/en/agents-and-tools/tool-use/advisor-tool`(工具形状、beta 头措辞、结果变体、错误码)
> - Anthropic 官方文档《API errors》 `https://platform.claude.com/docs/en/api/errors`(400 `invalid_request_error` 触发面)
> - 仓库源码:`1rgs/claude-code-proxy` `server.py`(`convert_anthropic_to_openai` 工具转换、`Tool` Pydantic model)
> - SDK 源码:`anthropics/anthropic-sdk-python` `src/anthropic/types/beta/beta_tool_union_param.py`、`beta/beta_advisor_tool_20260301_param.py`、`types/tool_union_param.py`
> - GitHub issue(beta-only 字段缺头报错实证):musistudio/claude-code-router#1528、BerriAI/litellm#31582 / #27532 / #23825、anthropics/anthropic-sdk-python#1179、anthropics/claude-code#41966 / #53581、Arize-ai/phoenix#12976、decolua/9router#1468
> - **本票实测产物**:throwaway 桩件 `src-tauri/src/proxy/openai-compat-gateway.prototype.mjs`(本文件同分支)

本票只回答**事实层**两问,**不下方案结论**(方案归后续决策票)。

---

## TL;DR(给本地代理的结论)

**(2)「静默吞掉」型 OpenAI 兼容网关——#4 的 F5/I3 推断被真实端点行为确证。**
对带 `advisor_20260301` 工具块的请求:
- **(a) 真返回 200 不报错**——网关对 advisor 工具块无任何错误,正常走完 Anthropic→OpenAI→Anthropic 转换。
- **(c) `type`/`model` 真被丢弃**——出站 OpenAI payload 里 advisor 退化为 `{"type":"function","function":{"name":"advisor","description":"","parameters":{}}}`;`type:"advisor_20260301"`、`model`、`max_uses`、`max_tokens` 全部消失,`description`/`input_schema` 也丢(advisor 本无这两字段)。
- **(b) executor 误调 advisor 的机制成立**(本票为 mock 后端**模拟**,非真实模型):网关把 advisor 当普通 function tool 暴露,上游模型一旦选择调用,网关把 `tool_calls[name=advisor]` 转回 Anthropic `tool_use{name:"advisor"}`——**但没有任何建议会被注入**,语义静默破坏。
- **→ 印证 #4 I3:「静默吞掉」比「报错」更危险**。这类端点**不产生任何 HTTP 错误信号**,只能靠预检/配置表识别,无法靠错误捕获——直接支撑 #8「端点判定用 base_url 先验、抓不到的用报错兜底」的分界。

**(3) 剥掉 beta 头后 Anthropic 直连的行为——无 Anthropic 凭证,本票退为文档/源码查证(明确标注未实测)。**
- 官方文档**只说「应带上 beta 头」,只字未提「缺头时服务端如何响应」**;《API errors》页 400 触发面**也无「未知工具 type」条目**。
- SDK 证据:`advisor_20260301` **只存在于 `beta/` 目录的 `BetaToolUnion`**,非 beta 稳定 `ToolUnion` 不含 advisor——**缺 beta 头时该 `type` 不在服务端合法 schema 内**。
- 大量同型实证(beta-gated 字段/工具缺头):`context_management`、`computer_*`、`eager_input_streaming`、`code_execution` 缺头均报 `400 invalid_request_error: <field>: Extra inputs are not permitted`。
- **→ 推断(未实测)**:剥掉 `advisor-tool-2026-03-01` 头后,`{"type":"advisor_20260301",...}` 极可能报 **`400 invalid_request_error`**,文案形如「`tools.N: Input tag 'advisor_20260301' ... does not match any of the expected tags`」(未知判别 type)或「`tools.N: Extra inputs are not permitted`」(beta-only 字段缺头)。**确切文案/path 仍待真实官方直连实测**——这正是仍留迷雾区的原清单第 (1) 项,依赖凭证。

---

## (2)「静默吞掉」型端点的真实行为实测

### 实测设计

**对象**:OpenAI 兼容网关(以 `1rgs/claude-code-proxy` `server.py` 为蓝本——#4 F5 已确认其 `Tool` 为固定 `{name, description, input_schema}` Pydantic model、`extra=ignore` 静默丢 `type`/`model`,且 `convert_anthropic_to_openai` 硬编码 `openai_tool = {"type":"function","function":{...}}`)。

**环境**:本地无任何在跑的 OpenAI 兼容网关,环境也无 OpenAI key;上游 Kimi/DeepSeek/Zhipu 均为 **Anthropic 兼容**(非本问类别)。故按用户裁定 **自建零依赖 Node 桩件复刻 server.py 的转换行为 + mock OpenAI 后端捕获出站 payload**。

**为何自研而非跑真 claude-code-proxy**:真身是 FastAPI+LiteLLM 重型栈,LiteLLM 本身是黑盒;桩件只复刻 #15 关心的那一段工具转换,行为逐字可审、零外部依赖。桩件 `openai-compat-gateway.prototype.mjs` 内聚两块:
- `MockOpenAIBackend`:OpenAI `/v1/chat/completions` 形状,**记录进站 body**,按模式回包(`call-advisor`=模拟上游模型选择调 advisor;`text`=不调工具)。
- `OpenAICompatGateway`:Anthropic `/v1/messages` 形状,**逐字复刻** server.py 的工具转换,转发给 mock 后端。

**取证链路**:client → Gateway(复刻转换)→ Mock(捕获出站 payload)。
- 「`type`/`model` 是否被丢弃」→ 看 Mock 捕获到的 `tools[]`(**硬事实**,非模拟)。
- 「是否 200 不报错」→ 看 Gateway 对 advisor 工具块的真实 HTTP 响应(**硬事实**)。
- 「executor 是否误调」→ 真实模型行为,Mock 只能**模拟**;回包 `tool_calls=advisor` 演示的是「若上游模型选择调 advisor,网关会如何透传」的**机制**,如实标注为模拟。

### 实测观察(逐字)

发送请求(含 advisor 工具 + 一个普通工具作对照):
```json
"tools": [
  {"type":"advisor_20260301","name":"advisor","model":"claude-opus-4-8","max_uses":3,"max_tokens":1400},
  {"name":"get_weather","description":"Get weather for a city","input_schema":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}
]
```

**(a) 网关 HTTP 状态 = `200`**(对 advisor 工具块无任何报错)。

**(c) 出站发往 OpenAI 后端的 `tools[]`(逐字捕获)**:
```json
[
  {"type":"function","function":{"name":"advisor","description":"","parameters":{}}},
  {"type":"function","function":{"name":"get_weather","description":"Get weather for a city","parameters":{"type":"object","properties":{"city":{"type":"string"}},"required":["city"]}}}
]
```
→ advisor 的 `type:"advisor_20260301"` 被**覆写为 `"function"`**;`model`/`max_uses`/`max_tokens` **全部消失**;`description`/`input_schema` 因 advisor 本无而变空。**普通工具 `get_weather` 完整保留**——证明丢弃只作用于 advisor 的 Anthropic 专属字段。

**(b) Anthropic 响应(后端 mock 模式=`call-advisor`,模拟上游模型选择调 advisor)**:
```json
"content": [{"type":"tool_use","id":"call_advisor_1","name":"advisor","input":{}}],
"stop_reason": "tool_use"
```
→ 网关把上游 `tool_calls[name=advisor]` 转回 Anthropic `tool_use{name:"advisor"}`。**机制上 executor 会「误调」它以为是本地工具的 advisor,但没有任何建议会被注入。**

**(b 对照,后端模式=`text`,不调工具)**:返回 `[{"type":"text","text":"ok"}]`、`stop_reason:"end_turn"`——语义看似正常,advisor 静默缺席。

### 结论(印证 #4)

| 子问 | 实测结果 |
| --- | --- |
| (a) 是否真 200 不报错 | **是**——200 正常响应,无错误信号 |
| (c) `type`/`model` 是否真被丢弃 | **是**——`type`→`"function"`,`model`/`max_uses`/`max_tokens` 消失 |
| (b) executor 是否真误调 advisor | **机制成立**(mock 模拟)——产生 `tool_use{name:"advisor"}` 但无建议注入 |

**#4 F5/I3 的「静默吞掉」推断被真实端点行为确证。** 这类端点对 advisor **不产生任何 HTTP 错误信号**,`error-capture` 路径对它们失效,只能靠预检/配置表(base_url 先验)识别——与 #8「能 try-catch 的用报错兜底、抓不到的用 base_url 先验」的分界一致。

---

## (3) 剥掉 beta 头后 Anthropic 直连的行为(文档/源码查证,未实测)

> **环境约束**:本地无 Anthropic 官方直连凭证(环境 `ANTHROPIC_BASE_URL` 指向本机 cc-switch,无官方路由),用户裁定本项**退为文档/源码查证**。下文区分「有据可查」与「推断」,确切报错文案**未实测**。

### 事实(有据可查)

**G1. 官方文档只说「应带 beta 头」,未提「缺头如何响应」。**
官方《Advisor tool》:「The advisor tool is in beta. Include the beta header `advisor-tool-2026-03-01` in your requests.」所有示例均经 `client.beta.messages.create(betas=["advisor-tool-2026-03-01"])` 调用。**文档无任何一节描述缺头时服务端的响应行为。**

**G2. 官方《API errors》页 400 触发面无「未知工具 type」条目。**(与 #4 I2 一致)
该页 `invalid_request_error` 的「Common validation errors」只列:prefill 不支持、thinking 块被改、扩展/自适应 thinking 不支持、thinking 不能禁用、AWS web-identity-federation。**无「未知工具 `type`」、无「beta-only 字段缺头」条目。**

**G3. SDK 中 `advisor_20260301` 只在 beta 作用域,稳定 schema 不含它。**
`anthropic-sdk-python`:`BetaToolUnion`(`types/beta/beta_tool_union_param.py`)含 `BetaAdvisorTool20260301Param`;**非 beta 稳定 `ToolUnion` 不含 advisor**。advisor 类型定义位于 `types/beta/beta_advisor_tool_20260301_param.py`,beta-only。
→ **缺 beta 头时,`type:"advisor_20260301"` 不在服务端(非 beta)合法工具 schema 内。**

**G4. 官方工具形状逐字(《Advisor tool》「Tool parameters」)。**

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `type` | string | *required* | Must be `"advisor_20260301"`. |
| `name` | string | *required* | Must be `"advisor"`. |
| `model` | string | *required* | The advisor model ID, such as claude-fable-5. |
| `max_uses` | integer | unlimited | 单次请求 advisor 调用上限,超限返回 `advisor_tool_result_error{error_code:"max_uses_exceeded"}`。 |
| `max_tokens` | integer | advisor 模型输出上限 | advisor 单次总输出(thinking+text)上限,最小 1024。 |
| `caching` | object\|null | `null`(关) | advisor 转录跨调用的 prompt 缓存开关。 |

**G5. 大量同型实证:beta-gated 字段/工具缺头 → `400 invalid_request_error: <field>: Extra inputs are not permitted`。**

| 字段/工具 | 缺头报错实证 |
| --- | --- |
| `context_management` | claude-code-router#1528、9router#1468:`context_management: Extra inputs are not permitted` |
| `computer_20250124`/`computer_20251124` | phoenix#12976:Playground 无 beta 头拒 `computer_*` type |
| `tools[].custom.eager_input_streaming` | litellm#31582:`tools.0.custom.eager_input_streaming: Extra inputs are not permitted` |
| `code_execution_20250825.use_web_search_purpose` | anthropic-sdk-python#1179:`use_web_search_purpose: Extra inputs are not permitted` |
| `system.*.cache_control.ephemeral.scope` | claude-code#41966:`Extra inputs are not permitted` |
| `trigger_id`(Routines) | claude-code#53581:`trigger_id: Extra inputs are not permitted` |

→ Anthropic 对「beta-only 字段/工具缺对应 beta 头」是**严格 400 校验**,而非静默忽略。

### 推断(未实测)

**I-a. 剥掉 `advisor-tool-2026-03-01` 头后,`{"type":"advisor_20260301",...}` 极可能报 `400 invalid_request_error`,而非被忽略/按未知工具放行。** 依据:G3(该 type 不在非 beta schema)+ G5(同型严格 400)。**「拒绝」是主线推断;「忽略工具」「按未知工具处理」均被 G5 的严格校验先例排除。**

**I-b. 确切文案二选一(均未实测到 advisor 专属文案):**
- 「未知判别 `type`」型:`tools.N: Input tag 'advisor_20260301' found using 'type' does not match any of the expected tags: ...`(#4 F4 thinking/document.source 同型外推);
- 「beta-only 字段缺头」型:`tools.N: Extra inputs are not permitted`(G5 同型)。
具体落到哪一型、确切 path 与 `error.type`,**待真实官方直连实测**——即仍留迷雾区的原清单第 (1) 项,依赖凭证。

**I-c. 对 #5/#8 的含义**:本地降级路径「转发第三方时按值剥离 `advisor-tool-2026-03-01` 头」(#5 F15/#8)与「官方直连须带该头」不冲突——已判定走本地降级的端点本就不达官方;而对**误达官方直连**的场景,I-a 提示会吃 400 而非静默成功,这为「误配官方端点却走本地降级」提供了一个可捕获的错误信号(非本票结论,供后续触发决策参考)。

---

## 来源

**一手(权威)**
- Anthropic《Advisor tool》 `https://platform.claude.com/docs/en/agents-and-tools/tool-use/advisor-tool` —— beta 头措辞(G1)、工具形状逐字(G4)、结果变体/错误码、`server_tool_use` 契约。
- Anthropic《API errors》 `https://platform.claude.com/docs/en/api/errors` —— 400 `invalid_request_error` 触发面,**佐证「未知工具 type 未被官方列为 400 触发项」**(G2)。

**仓库源码(一手)**
- `1rgs/claude-code-proxy` `server.py` —— `Tool` Pydantic model 无 `type`(extra=ignore)、`convert_anthropic_to_openai` 硬编码 `openai_tool={"type":"function","function":{name,description,parameters}}`((2) 转换蓝本)。
  `https://github.com/1rgs/claude-code-proxy/blob/main/server.py`
- `anthropics/anthropic-sdk-python` `src/anthropic/types/beta/beta_tool_union_param.py`(advisor 在 BetaToolUnion G3)、`beta/beta_advisor_tool_20260301_param.py`(beta-only 类型 G3)、`types/tool_union_param.py`(稳定 union 不含 advisor G3)。

**GitHub issue(beta-only 字段缺头报错实证,一手)**
- musistudio/claude-code-router#1528 —— `context_management: Extra inputs are not permitted`(G5)。
- BerriAI/litellm#31582 —— `tools.0.custom.eager_input_streaming: Extra inputs are not permitted`(G5)。
- anthropics/anthropic-sdk-python#1179 —— `use_web_search_purpose: Extra inputs are not permitted`(G5)。
- anthropics/claude-code#41966 / #53581、Arize-ai/phoenix#12976、decolua/9router#1468、BerriAI/litellm#27532 / #23825 —— 同型 beta-only 缺头 400(G5)。

**本票实测产物(throwaway,勿合 main)**
- `src-tauri/src/proxy/openai-compat-gateway.prototype.mjs`(本文件同分支 `research/issue-15-endpoint-behavior`)—— (2) 的 OpenAI 兼容网关复刻桩件 + mock 后端,(a)(b)(c) 逐字观察由其产生。

**仓库内复用**
- [`issue-4-endpoint-capability-detection.md`](./issue-4-endpoint-capability-detection.md) —— F5(claude-code-proxy 固定 struct 丢 type/model)、I2(官方未列未知工具 type 为 400)、I3(静默吞掉更危险)、待实测清单 (2)(3) 即本票两问。
- [`issue-5-advisor-client-shape.md`](./issue-5-advisor-client-shape.md) —— beta 头按值剥离(F15)、明文 `advisor_result{text}` 变体不依赖 beta 头。

---

## 仍待实测(喂回地图迷雾区)

1. **advisor 工具 type 在 Anthropic 官方直连的确切报错文案/path**(本票 I-b):需一把真实 Anthropic key,对官方直连发 `{"type":"advisor_20260301",...}` + 剥掉 beta 头,记录是「`Input tag ...`」型还是「`Extra inputs are not permitted`」型、确切 `tools.N` path 与 `error.type`。——即原清单第 (1) 项,**依赖凭证**,仍留迷雾区。
2. **(b) executor 误调 advisor 的真实模型确认**:本票为 mock 模拟;需一个真实 OpenAI 兼容上游(GPT/Gemini 等)确认真实模型在 advisor 被降级为普通 function tool 后,是否真会主动发起 `tool_calls[name=advisor]`。——可在有真实 OpenAI 兼容 key 时复跑本桩件(把 Mock 后端换成真实上游)确证。
