# 研究：advisor 调用在客户端的形状与本地复刻

> Wayfinder 研究票 [issue #5](https://github.com/ACautomata/cc-switch/issues/5) 的成果。父图 [issue #2](https://github.com/ACautomata/cc-switch/issues/2)。
>
> **一手来源**：Anthropic 官方文档《Advisor tool》，`https://platform.claude.com/docs/en/agents-and-tools/tool-use/advisor-tool`（原 `docs.anthropic.com/en/agents-and-tools/tool-use/advisor-tool` 301 重定向至此）。下文凡未另注出处者，JSON 均逐字引自该页。次级佐证见文末「来源」。

本票回答：第三方 API 把 advisor 工具降级为客户端工具后，其调用块与结果回注块的**确切线协议形状**，以及 cc-switch 代理要捕获调用、回注建议时各该如何复刻。

---

## TL;DR(给本地代理的结论)

第三方 executor(不经 Anthropic 服务端）时，**没有「免费的 `server_tool_use`」**——那只是官方服务端执行的副产品。本地复刻的正确做法是把 advisor 暴露为**一个普通客户端工具**(custom tool,`name: "advisor"`):

- 模型发出**普通 `tool_use`**(`id: "toolu_…"`,**非** `srvtoolu_…`)，代理在响应里认出 `name=="advisor"` 即捕获，这是可识别、可中断的信号。
- 代理本地跑一次 advisor 推理，把建议**以 `advisor_tool_result{content: advisor_result{text}}` 原形状**放在**下一条 user 消息**里回注——精确复刻官方 shape,executor 看到的就是它认识的那个块。
- 因为用了明文 `advisor_result{text}` 变体(**不**用 `advisor_redacted_result`)，整条会话**完全不依赖** beta 头 `advisor-tool-2026-03-01`,proxy 应在转发第三方时剥掉该头。
- 官方「服务端读完整转录」的语义，在本地由 **proxy 显式组装**进 advisor 的 `/v1/messages` 调用(system + tools + 历史 + 工具结果 + 本轮已产出文本),advisor 端套 oh-my-openagent 的 Oracle 系统提示(见 map #2 资产)替代官方内置 advisor 系统提示。
- 流式下官方 advisor 子推理**不流式**、整包在一个 `content_block_start` 到达；本地复刻时 proxy「暂停 executor 流 → 本地推理 → 用 #3 查明的 `streaming.rs` 手法合成一个 `advisor_tool_result` 块一次性注入 → 续流」。官方 `pause_turn` 恢复语义在本地模式下**不适用**(那是服务端 iteration-cap 机制)。

---

## 契约事实(逐字引用)

### 1. 工具定义(request `tools[]`)

```json
{
  "type": "advisor_20260301",
  "name": "advisor",
  "model": "claude-fable-5"
}
```

参数表(官方「Tool parameters」):

| 参数 | 类型 | 默认 | 说明 |
| --- | --- | --- | --- |
| `type` | string | *必填* | 恒 `"advisor_20260301"` |
| `name` | string | *必填* | 恒 `"advisor"` |
| `model` | string | *必填* | advisor 模型 ID,按该模型费率计费 |
| `max_uses` | integer | 不限 | 单请求内 advisor 调用上限;超限返回 `advisor_tool_result_error{error_code:"max_uses_exceeded"}` |
| `max_tokens` | integer | advisor 模型输出上限 | 单次 advisor 总输出(thinking+text)上限,最小 1024 |
| `caching` | object\|null | `null`(关) | advisor 自身转录的 prompt caching,形如 `{"type":"ephemeral","ttl":"5m"\|"1h"}` |

另接受通用工具属性:`cache_control`、`allowed_callers`、`defer_loading`、`strict`。

### 2. 调用块 `server_tool_use`(官方服务端形状)

官方「How it works」:executor 发出 `server_tool_use`,`name:"advisor"`、`input` 为空:

> The executor emits a `server_tool_use` block with `name: "advisor"` and an empty `input`. The executor signals timing, and the server supplies context.

> The `server_tool_use.input` is always empty. The server constructs the advisor's view from the full transcript automatically. Nothing the executor puts in `input` reaches the advisor.

响应里(assistant content)逐字示例:

```json
{
  "type": "server_tool_use",
  "id": "srvtoolu_abc123",
  "name": "advisor",
  "input": {}
}
```

**注意 id 前缀 `srvtoolu_`**(服务端工具),区别于客户端工具的 `toolu_`。

### 3. 结果块 `advisor_tool_result`(回注给 executor)

成功调用时,`server_tool_use` 之后紧跟 `advisor_tool_result`,逐字示例(plaintext 变体,如 advisor 为 `claude-opus-4-8`):

```json
{
  "type": "advisor_tool_result",
  "tool_use_id": "srvtoolu_abc123",
  "content": {
    "type": "advisor_result",
    "text": "Use a channel-based coordination pattern. The tricky part is draining in-flight work during shutdown: close the input channel first, then wait on a WaitGroup..."
  }
}
```

`content` 是判别联合(discriminated union),成功两变体:

| 变体 | 字段 | 何时返回 |
| --- | --- | --- |
| `advisor_result` | `text`, `stop_reason` | advisor 返回明文(如 Claude Opus 4.8) |
| `advisor_redacted_result` | `encrypted_content`, `stop_reason` | advisor 返回加密输出(Claude Opus 5 / Fable 5 / Mythos 5) |

> 两变体仅当你设置了工具定义的 `max_tokens` 时携带 `stop_reason` 字段,否则省略。`advisor_redacted_result.encrypted_content` 是客户端读不到的不透明 blob,下一轮由服务端解密渲染进 executor 提示。**两种情形都要在后续轮次把 content 原样回传(round-trip verbatim)。**

截断时(设了 `max_tokens`),官方追加标记并带 `stop_reason:"max_tokens"`:

```json
{
  "type": "advisor_tool_result",
  "tool_use_id": "srvtoolu_abc123",
  "content": {
    "type": "advisor_result",
    "text": "Use a channel-based coordination pattern. The tricky part is\n\n[Advisor output truncated at max_tokens=2048.]",
    "stop_reason": "max_tokens"
  }
}
```

### 4. 错误块 `advisor_tool_result_error`

```json
{
  "type": "advisor_tool_result",
  "tool_use_id": "srvtoolu_abc123",
  "content": {
    "type": "advisor_tool_result_error",
    "error_code": "overloaded"
  }
}
```

`error_code` 全集:

| `error_code` | 含义 |
| --- | --- |
| `max_uses_exceeded` | 达到工具定义上的 `max_uses` 上限 |
| `too_many_requests` | advisor 子推理被限流 |
| `overloaded` | advisor 子推理触容量上限 |
| `prompt_too_long` | 转录超出 advisor 模型上下文窗 |
| `execution_time_exceeded` | advisor 子推理超时 |
| `unavailable` | 其它 advisor 失败 |

> executor 看到错误后**不带建议地继续**,请求本身不失败。advisor 限流画在结果里(`too_many_requests`);executor 限流才是整个请求 HTTP 429。

### 5. 流式行为 + `pause_turn`

官方「Streaming」:

> The advisor sub-inference does not stream. The executor's stream pauses while the advisor runs, then the full result arrives in a single event. … `server_tool_use`(name=advisor)在 `content_block_stop` 处开始暂停;暂停期间流静默,仅有约每 30s 一次的 SSE `ping` keepalive。advisor 完成后,`advisor_tool_result` 在**单个 `content_block_start` 事件**里整包到达(无 deltas)。executor 输出随后恢复流式。随后一个 `message_delta` 携带更新的 `usage.iterations`。

`pause_turn`(官方「Resuming a paused turn」/「Combining with other tools」表):

> 响应可能以 `stop_reason:"pause_turn"` 结束而 advisor 调用仍挂起:响应含 `server_tool_use` 但**无对应 `advisor_tool_result`**。恢复法:把该 assistant 消息**内容不变**(保留 `server_tool_use`)追加进 `messages`,带相同 advisor 工具与 beta 头重发,**无需**加 user 消息或 `tool_result`。API 跑挂起的 advisor 并续 executor 轮次。恢复轮可能再 pause,重复即可。恢复请求省略 advisor 工具 → `400`。若同一轮 executor 还调了你的客户端工具,则响应以 `stop_reason:"tool_use"` 结束,挂起的 advisor 在你发 `tool_result` 后的下一请求开头运行。

**`pause_turn` 是服务端 sampling-loop 的 iteration-cap 机制**,属服务端执行语义,本地客户端复刻不涉及(见下文问题 4)。

### 6. 转录如何提供给 advisor

官方「How it works」第 2 条:

> Anthropic runs a separate inference pass on the advisor model server-side. The advisor runs under its own Anthropic-supplied system prompt and receives the executor's **full transcript as quoted context** in its input. That transcript includes **your system prompt, the tool definitions, the prior turns and tool results, and the text the executor has produced so far in this turn**.

> The advisor itself runs **without tools and without context management**. Its thinking blocks are dropped before the result returns. Only the advice text reaches the executor.

官方另注(「Trimming advisor output length」):

> The advisor sees both your system prompt and your user messages as **quoted context** about the executor's task, so instructions that address the advisor directly are followed much more reliably…

### 7. 官方建议执行侧系统提示(coding tasks)

官方建议 prepend 到 executor 系统提示的两段(逐字,见来源页「Suggested system prompt for coding tasks」;另有 Haiku 替代块与 Opus 增调块变体):

Timing 段:

```text
You have access to an `advisor` tool backed by a stronger reviewer model. It takes NO parameters — when you call advisor(), your entire conversation history is automatically forwarded. They see the task, every tool call you've made, every result you've seen.

Call advisor BEFORE substantive work — before writing, before committing to an interpretation, before building on an assumption. If the task requires orientation first (finding files, fetching a source, seeing what's there), do that, then call advisor. Orientation is not substantive work. Writing, editing, and declaring an answer are.

Also call advisor:
- When you believe the task is complete. BEFORE this call, make your deliverable durable: write the file, save the result, commit the change. The advisor call takes time; if the session ends during it, a durable result persists and an unwritten one doesn't.
- When stuck — errors recurring, approach not converging, results that don't fit.
- When considering a change of approach.

On tasks longer than a few steps, call advisor at least once before committing to an approach and once before declaring done. On short reactive tasks where the next action is dictated by tool output you just read, you don't need to keep calling — the advisor adds most of its value on the first call, before the approach crystallizes.
```

对待建议段(紧随 timing 段):

```text
Give the advice serious weight. If you follow a step and it fails empirically, or you have primary-source evidence that contradicts a specific claim (the file says X, the paper states Y), adapt. A passing self-test is not evidence the advice is wrong — it's evidence your test doesn't check what the advice is checking.

If you've already retrieved data pointing one way and the advisor points another: don't silently switch. Surface the conflict in one more advisor call — "I found X, you suggest Y, which constraint breaks the tie?" The advisor saw your evidence but may have underweighted it; a reconcile call is cheaper than committing to the wrong branch.
```

> 注意:官方提示假设「调用时整段历史自动转发、无参数」。本地复刻时,因 advisor 变客户端工具,`input` 不必恒空——可由 executor 在 `input` 里带一句「本次想问什么」作为 focus,proxy 再把它并进 advisor 输入(见问题 1/3)。这是与官方语义的**有意偏离**,需在注入的系统提示里相应改写「It takes NO parameters」一句。

### 8. Usage / 计费结构

官方「Usage and billing」逐字示例:

```json
{
  "usage": {
    "input_tokens": 412,
    "cache_read_input_tokens": 0,
    "cache_creation_input_tokens": 0,
    "output_tokens": 531,
    "iterations": [
      { "type": "message", "input_tokens": 412, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "output_tokens": 89 },
      { "type": "advisor_message", "model": "claude-fable-5", "input_tokens": 823, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0, "output_tokens": 1612 },
      { "type": "message", "input_tokens": 1348, "cache_read_input_tokens": 412, "cache_creation_input_tokens": 0, "output_tokens": 442 }
    ]
  }
}
```

> 顶层 `usage` 只反映 executor token;advisor token **不并入**顶层(费率不同),在 `usage.iterations[]` 里 `type:"advisor_message"`(按 advisor 模型费率)与 `type:"message"`(按 executor 模型费率)分行。advisor 输出典型 400–700 文本 token(含 thinking 约 1400–1800)。

---

## ticket #5 五问逐条回答

### 问题 1 —— 调用块:`server_tool_use` 还是 `tool_use`?如何产生可识别、可中断的信号

**官方(服务端执行)**:executor 发 `server_tool_use`(`id:"srvtoolu_…"`,`input` 恒空)。`input` 空是因为「executor 只发时机信号,上下文由服务端补」——**这是服务端自动注入转录的副产品**。

**本地复刻(第三方 executor)**:第三方 API 不执行服务端工具,`server_tool_use` 无从谈起。正确做法是把 advisor 暴露为**普通客户端工具**:

```json
{
  "name": "advisor",
  "description": "<oh-my-openagent Oracle 的“何时调用”说明改造而来>",
  "input_schema": { "type": "object", "properties": {}, "required": [] }
}
```

于是模型发出**普通 `tool_use`**:`{"type":"tool_use","id":"toolu_…","name":"advisor","input":{…}}`。**代理在响应侧认出 `name=="advisor"` 的 `tool_use` 即捕获**——这就是「可被识别、可被中断去取建议」的信号(id 前缀 `toolu_` 而非 `srvtoolu_`,明确区分于官方)。

- **非流式**:executor 整包返回 `stop_reason:"tool_use"` + `content` 含该 `tool_use`,proxy 在 `response_processor.rs:213 handle_non_streaming`(见 #3)识别并**拦截不回客户端**,转去本地跑 advisor。
- **流式**:`tool_use` 块经 `content_block_start`(`tool_use`)→ `input_json_delta` 若干 → `content_block_stop` 流式到达,`message_delta` 带 `stop_reason:"tool_use"`。proxy 在 `message_delta` 见到 `tool_use` 停因 + 已收集的 `tool_use` 块 `name=="advisor"` 时**扣住该轮不回客户端**,去取建议。
- **关于 `input`**:本地模式不必恒空。让 `input_schema` 允许一个可选 `question`/`focus` 字段,executor 可在 `input` 里带一句本次关注点;proxy 把它并入 advisor 输入(官方语义是「input 不到达 advisor」,这里是**有意偏离**,见问题 3/7)。

### 问题 2 —— 回注块:能否精确复刻 `advisor_tool_result`?放哪里?

**能,且应精确复刻** `advisor_tool_result{content: advisor_result{text}}`(明文变体)。executor 看到的块形状与官方一致,无需它额外适应。

- **用明文 `advisor_result{text}` 变体**,不用 `advisor_redacted_result`。理由:本地模式下 proxy 需要把建议**真正喂回** executor 的下一轮输入,而 `encrypted_content` 是「客户端读不到、由 Anthropic 服务端解密」的不透明 blob——离开 Anthropic 服务端毫无意义。
- **放置位置**:官方里 `advisor_tool_result` 出现在 **assistant 转录**中;但本地是**客户端工具**,`tool_use`/`tool_result` 惯例要求结果放在**下一条 user 消息**。因此 proxy 应:
  1. 把 executor 含 `tool_use`(advisor)的那条 assistant 消息原样保留进历史;
  2. 在下一条 **user** 消息里放 `{"type":"advisor_tool_result","tool_use_id":"<该 tool_use 的 id>","content":{"type":"advisor_result","text":"<建议>"}}`。
  - 用 `advisor_tool_result` 形状而非普通 `tool_result`,是为了让「剥离/回注逻辑」与官方契约、与 map #2 术语保持一致;`tool_use_id` 回链对应 `tool_use.id`。
- **第三方 executor 能否接受**:能。Anthropic Messages API 的 `user` 消息 `content` 数组接受 `tool_result` 及同源结构块;`advisor_tool_result` 本质是「带 `tool_use_id` 的结果块」。**风险点**:某些第三方实现对未知块 `type` 校验严格。故 proxy 应做成**可配置回退**:首选 `advisor_tool_result` 形状,若上游报错则降级为等价普通 `tool_result`(`content` 直接放建议字符串)。该回退开关属实现细节,列入待决(见「待决项」D4)。

### 问题 3 —— 「读完整转录」如何映射为本地 `/v1/messages` 的 `messages`

官方语义:advisor 收到 executor **完整转录作为 quoted context**——含你的 system prompt、工具定义、历史轮次与工具结果、本轮 executor 已产出文本;且 advisor **自身无工具、无 context 管理**,跑在 **Anthropic 内置 advisor 系统提示**下。

本地复刻,proxy 在捕获 advisor 调用后**显式组装**一次 `/v1/messages` 调用:

| 官方(advisor 输入) | 本地复刻 |
| --- | --- |
| Anthropic 内置 advisor 系统提示 | **oh-my-openagent Oracle 系统提示**(map #2 资产,`ORACLE_DEFAULT_PROMPT`,temperature=0.1、只读)作 advisor 的 `system` |
| executor 的 system prompt(quoted) | 放进 advisor 输入(如一段 user 消息或单独 system 段),标明「这是被顾问的任务背景」 |
| 工具定义(quoted) | 序列化 executor 的 `tools[]` 进 advisor 输入文本 |
| 历史轮次 + 工具结果(quoted) | 把 executor 的 `messages[]`(含既往 `tool_use`/`tool_result`)原样作为 advisor 的 `messages` |
| 本轮 executor 已产出文本(quoted) | 把本轮 executor 到捕获点为止的 `content`(text + advisor `tool_use` 及其 `input.focus`)追加为最后一条 assistant/user 段 |
| executor 在 `input` 里的 focus(本地扩展) | 并入 advisor 输入,作为「本次想问什么」 |

要点:advisor 调用**本身是一条独立 `/v1/messages`**(#3 未决项 2 的「再起一次非流 `/v1/messages`」路径),**必须带防重入守卫**,使 advisor 子请求不再触发 proxy 的 advisor 拦截(否则递归)。该守卫属实现待决(D3)。

### 问题 4 —— 流式与非流式各自如何支持与恢复;`pause_turn` 怎么办

- **非流式**:最简单。executor 整包返回 `stop_reason:"tool_use"`;proxy 在 `handle_non_streaming`(#3 挂点)识别 advisor `tool_use`,本地跑 advisor(同步/阻塞至拿到建议),把 `advisor_tool_result` 注入**下一条 user 消息**后**代 executor 续跑一轮**(见下「恢复」),或先把建议回注让 executor 继续。无流式状态机。
- **流式**:仿官方语义——「advisor 子推理不流式、整包单次到达」。proxy:
  1. 在 `message_delta` 见 `stop_reason:"tool_use"` 且 advisor `tool_use` 已收齐时,**扣住 executor 流**(不回客户端),进入「暂停-取建议」;
  2. 本地跑 advisor 至完整(其子调用可流式累积,但对 executor/客户端表现为整包);
  3. 用 #3 查明的 `streaming.rs:467-478` 手法(`json!` + `format!("event: …\ndata: …\n\n")` + `yield Bytes`)**合成一个 `advisor_tool_result` 块,一次性注入**(对应官方「单个 `content_block_start` 到达、无 deltas」);
  4. 续 executor 流。
  - **风险(#3 已标)**:「暂停-回注」会触 `create_logged_passthrough_stream:701-736` 的首字节/静默超时假设,`SseUsageCollector` 也不认识 advisor token——暂停期间需像官方一样发 SSE `ping` keepalive 保活,usage 统计需另立模型(见待决 D5)。
- **`pause_turn` 恢复**:**本地模式不适用**。官方 `pause_turn` 是「服务端 sampling-loop 达 iteration 上限、结果未就绪,让客户端原样重发以续」的服务端机制。本地复刻里,advisor 由 proxy **同步跑完再注入**,不存在「结果未就绪、让 executor 先停」的中间态,故 executor 停因只会是普通的 `tool_use`(客户端工具语义),由 proxy 走「捕获→注入→续跑」,而非 `pause_turn`。**无需复刻 `pause_turn`。**(若未来对接真正支持服务端 advisor 的上游,才需处理 `pause_turn`——那是另一情形。)

### 问题 5 —— 「只读、给建议、不自行执行」如何与「读转录但不替 executor 动手」对齐

官方:advisor **自身无工具、无 context 管理**,thinking 块在返回前被丢弃,只有建议文本到达 executor——即「读转录、给建议、绝不动手」。

本地复刻与 oh-my-openagent Oracle 的对齐:
- **只读**:Oracle 提示词本就禁用 write/edit/apply_patch/task(map #2 资产);**实现上**还需在 advisor 的 `/v1/messages` 调用里**不传任何工具**(`tools` 为空),从协议层强制「无法动手」,与官方「advisor runs without tools」一致。
- **只给建议、不执行**:advisor 输出仅是文本建议,proxy 把它包成 `advisor_result{text}` 回注;executor 才是唯一执行者。Oracle「给架构/自评/调试建议、不替你修」的触发域与「何时避免」清单,正好充当注入给 executor 的「何时调用 advisor」说明(问题 1 的工具 `description` / 系统提示素材)。
- **temperature=0.1**:沿用 Oracle 元数据,保证建议稳定。
- **thinking 丢弃**:官方丢弃 advisor thinking;本地若给 advisor 开 thinking,proxy 在回注前剥离 thinking 块、只留文本,复刻该语义。

---

## 对 #3 挂点的含义(请求侧 / 响应侧)

承接 [issue #3](https://github.com/ACautomata/cc-switch/issues/3) 查明的挂点,本票把「形状」落到其上:

- **请求侧**(`forwarder.rs:1579-1592` 定稿段):
  - 识别 `tools[]` 中 `type=="advisor_20260301"`(#3 已有 `transform.rs:216-219` retain 先例),**剥离**之,并把 `model`、`max_uses`、`max_tokens`、`caching` 等参数捕获进 `AdvisorCapture`(#3 候选 A),经 `RequestForwarder`→`ForwardResult`→`RequestContext` 带到响应侧(#3 未决项 4)。
  - **剥掉 beta 头** `advisor-tool-2026-03-01`(本地模式用明文变体,不依赖该 beta;第三方不认识它可能报错)。
  - **注入执行侧系统提示**(官方「何时调用 advisor」timing/对待建议两段,或经 Oracle 改造的等价物),并说明 advisor 已变客户端工具、「调用时转录由系统转发」的措辞需相应调整。
  - 把 advisor 以**客户端工具**形状(问题 1)注入 `tools[]`。
- **响应侧**:
  - **非流式** `response_processor.rs:213 handle_non_streaming`:识别 advisor `tool_use`,本地跑 advisor,合成 `advisor_tool_result` 注入下一条 user 消息,代 executor 续跑或回注。
  - **流式**:仿 `response_processor.rs:683 create_logged_passthrough_stream` 造**可改写流**,在 advisor `tool_use` 收齐处「暂停→本地推理→用 `streaming.rs:467-478` 合成 `advisor_tool_result` 单块注入→续流」,暂停期发 SSE `ping` 保活。

---

## 待决项(交回 map,非本票可定)

> 这些是本研究**暴露**的后续问题,属 map #2 的迷雾/新票,不在 #5 范围内。

- **D1(配置建模)**:前端 `src/types/proxy.ts` 目前**无任何 advisor 类型**;advisor 开关注入(advisor 模型选哪个、`max_uses`/`max_tokens`/`caching` 默认、executor 侧提示模板)需在 proxy 配置与前端类型里建模。(承接 map 迷雾区「缓存/成本语义」。)
- **D2(回注协议选择)**:首选 `advisor_tool_result{content:advisor_result{text}}` 形状 vs 回退普通 `tool_result`,取决于目标第三方 executor 对未知块 `type` 的容忍度——需对真实第三方实测后定默认值。
- **D3(防重入守卫)**:advisor 子请求如何标记以跳过 advisor 拦截(避免递归),及 advisor 子请求走「复用 `forward` 再起非流 `/v1/messages`」还是「直连固定上游」(#3 未决项 2)。
- **D4(调用身份)**:advisor 子调用用哪份凭证/哪个 provider(沿用当前 executor 的第三方,还是单独配一个官方/第三方端点),涉及 `provider_router`。
- **D5(usage/成本建模)**:`usage.iterations[]` 的 `advisor_message` 在本地模式下如何建模进 cc-switch 的用量统计;流式暂停对 `SseUsageCollector` 与首字节/静默超时的影响。(承接 map 迷雾区「缓存/成本语义」+「错误降级」。)
- **D6(缓存语义)**:advisor 侧 `caching:{type:"ephemeral"}` 在本地回退下能否/如何复刻;注入系统提示改变 prompt 前缀对 executor 侧 cache 命中的影响(`forwarder.rs:1601` 观测)。(承接 map 迷雾区「缓存/成本语义」。)
- **D7(错误降级)**:`advisor_tool_result_error` 的 `error_code` 语义(overloaded/too_many_requests/…)在本地 advisor 失败时如何映射,让 executor 继续而非整体失败。(承接 map 迷雾区「错误降级」。)

---

## 来源

- **一手(权威)**：Anthropic 官方文档《Advisor tool》—— `https://platform.claude.com/docs/en/agents-and-tools/tool-use/advisor-tool`(beta 头 `advisor-tool-2026-03-01`,工具类型 `advisor_20260301`)。本文全部逐字 JSON 块形状、错误码、`pause_turn`、流式 SSE 序列、usage 结构、系统提示段皆出自该页。
- **次级佐证(仅交叉印证,未作依据)**:
  - Zenn《Introduction to Claude Advisor Tool》`https://zenn.dev/kai_kou/articles/207-claude-advisor-tool-guide` —— 印证 `server_tool_use`/`advisor_tool_result` 明文块 JSON。
  - Anthropic engineering blog《The Advisor Strategy》`https://www.anthropic.com/engineering/the-advisor-strategy`。
- **管线挂点(仓库内)**：[issue #3 研究评论](https://github.com/ACautomata/cc-switch/issues/3) —— `forwarder.rs` / `response_processor.rs` / `streaming.rs` 各 `文件:行号` 挂点。
- **Oracle 提示词资产(仓库内)**:map [issue #2](https://github.com/ACautomata/cc-switch/issues/2)「资产」节 —— `ORACLE_DEFAULT_PROMPT` 等三份、元数据(category=advisor、temperature=0.1、只读)。
