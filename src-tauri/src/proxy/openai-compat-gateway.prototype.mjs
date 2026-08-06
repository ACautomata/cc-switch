// ============================================================================
// PROTOTYPE — throwaway. Wayfinder 研究票 issue #15(父图 issue #2),问题 (2)。
//
// 要回答的问题(对「静默吞掉」型 OpenAI 兼容网关):
//   (a) 带 advisor_20260301 工具块的请求,网关是否真返回 200 不报错?
//   (b) executor 是否真把 advisor 当普通工具「误调」(发出指向 advisor 的 tool_use)?
//   (c) 工具块的 type / model 字段是否在 Anthropic→OpenAI 转发中真被丢弃?
//
// 形态:单文件零依赖 Node 桩件,内存起两个 HTTP 服务:
//   - MockOpenAIBackend  : OpenAI /chat/completions 形状,记录进站 body,按模式回。
//   - OpenAICompatGateway: Anthropic /v1/messages 形状,复刻 1rgs/claude-code-proxy
//                          server.py 的 convert_anthropic_to_openai 工具转换
//                          (关键一行: openai_tool = {"type":"function", "function":{name,
//                          description, parameters}} —— advisor 的 type/model/max_uses
//                          被无条件覆写/丢弃),转发给 MockOpenAIBackend。
//
// 为何自研而非跑真 claude-code-proxy:真的那个是 FastAPI+LiteLLM 重型栈,LiteLLM
//   本身是个黑盒;本桩件只复刻「#15 关心的那一段转换」,行为逐字可审、零外部依赖。
//
// 关键取证设计:client → Gateway(复刻转换) → Mock(捕获出站 payload)。
//   「type/model 是否被丢弃」看 Mock 捕获到的 tools[](硬事实,非模拟)。
//   「是否 200 不报错」看 Gateway 对 advisor 工具块的真实响应(硬事实)。
//   「executor 是否误调」是真实模型行为,Mock 只能模拟——回包 tool_calls=advisor
//   是「若上游模型选择调 advisor,网关会如何透传」的机制演示,报告中如实标注为模拟。
//
// 运行: node src-tauri/src/proxy/openai-compat-gateway.prototype.mjs
// ============================================================================

import http from "node:http";

// ---------------------------------------------------------------------------
// MockOpenAIBackend —— OpenAI /v1/chat/completions 形状
// ---------------------------------------------------------------------------
// mode:
//   "call-advisor" : 模拟「上游模型选择调用 advisor」→ 回 finish_reason=tool_calls,
//                    tool_calls=[{function:{name:"advisor"}}](机制演示 executor 误调)。
//   "text"         : 模拟「上游模型不调工具」→ 回普通 content,finish_reason=stop。
function startMockOpenAIBackend(mode) {
  const captured = [];
  const server = http.createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", () => {
      let json = null;
      try {
        json = JSON.parse(body);
      } catch {}
      captured.push({ url: req.url, body: json });

      const message =
        mode === "call-advisor"
          ? {
              role: "assistant",
              content: null,
              tool_calls: [
                {
                  id: "call_advisor_1",
                  type: "function",
                  function: { name: "advisor", arguments: "{}" },
                },
              ],
            }
          : { role: "assistant", content: "ok", tool_calls: null };

      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          id: "chatcmpl-mock",
          object: "chat.completion",
          created: 0,
          model: json?.model ?? "mock-model",
          choices: [
            {
              index: 0,
              message,
              finish_reason: mode === "call-advisor" ? "tool_calls" : "stop",
            },
          ],
          usage: { prompt_tokens: 1, completion_tokens: 1, total_tokens: 2 },
        })
      );
    });
  });
  return new Promise((resolve) =>
    server.listen(0, "127.0.0.1", () =>
      resolve({ server, captured, port: server.address().port })
    )
  );
}

// ---------------------------------------------------------------------------
// OpenAICompatGateway —— Anthropic /v1/messages 形状
// 复刻 claude-code-proxy server.py 的「工具固定 struct 化」转换。
//   Anthropic Tool 在 server.py 是 Pydantic {name, description, input_schema},
//   extra=ignore → 入站时 advisor 的 type/model/max_uses 已被丢弃;转换时
//   openai_tool 又硬编码 "type":"function" → 出站只见 {function:{name,...}}。
// ---------------------------------------------------------------------------
function startOpenAICompatGateway(backendPort) {
  const server = http.createServer((req, res) => {
    let body = "";
    req.on("data", (c) => (body += c));
    req.on("end", async () => {
      let anthropicReq = null;
      try {
        anthropicReq = JSON.parse(body);
      } catch {}

      // --- 复刻 server.py:convert_anthropic_to_openai 的 tools 转换 ---
      // (c) 的取证点:这里只保留 name/description/input_schema,type/model 全丢。
      const inTools = Array.isArray(anthropicReq?.tools) ? anthropicReq.tools : [];
      const openaiTools = inTools.map((t) => ({
        type: "function",
        function: {
          name: t?.name,
          description: t?.description ?? "",
          parameters: t?.input_schema ?? {},
        },
      }));

      const openaiReq = {
        model: anthropicReq?.model ?? "mock-model",
        messages: anthropicReq?.messages ?? [],
        max_completion_tokens: anthropicReq?.max_tokens,
        stream: false,
        ...(openaiTools.length ? { tools: openaiTools } : {}),
      };

      // 转发给 mock 后端。
      const upstream = await fetch(
        `http://127.0.0.1:${backendPort}/v1/chat/completions`,
        {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(openaiReq),
        }
      );
      const upstreamJson = await upstream.json();

      // --- 复刻 server.py:OpenAI → Anthropic 响应转换 ---
      // tool_calls → tool_use 块;stop→end_turn、tool_calls→tool_use。
      const choice = upstreamJson?.choices?.[0] ?? {};
      const msg = choice.message ?? {};
      const content = [];
      if (typeof msg.content === "string" && msg.content) {
        content.push({ type: "text", text: msg.content });
      }
      for (const tc of msg.tool_calls ?? []) {
        content.push({
          type: "tool_use",
          id: tc.id ?? "toolu_mock",
          name: tc.function?.name ?? "",
          input: (() => {
            try {
              return JSON.parse(tc.function?.arguments ?? "{}");
            } catch {
              return {};
            }
          })(),
        });
      }
      const stop_reason =
        choice.finish_reason === "tool_calls"
          ? "tool_use"
          : choice.finish_reason === "length"
          ? "max_tokens"
          : "end_turn";

      // (a) 的取证点:对带 advisor 工具块的请求,网关返回 200 + 正常 Anthropic 响应。
      res.writeHead(200, { "content-type": "application/json" });
      res.end(
        JSON.stringify({
          id: "msg_mock",
          type: "message",
          role: "assistant",
          model: anthropicReq?.model ?? "mock-model",
          content,
          stop_reason,
          stop_sequence: null,
          usage: { input_tokens: 1, output_tokens: 1 },
        })
      );
    });
  });
  return new Promise((resolve) =>
    server.listen(0, "127.0.0.1", () => resolve({ server, port: server.address().port }))
  );
}

// ---------------------------------------------------------------------------
// 驱动:起 mock + gateway,跑一组场景,打印逐字观察
// ---------------------------------------------------------------------------
const ADVISOR_TOOL = {
  type: "advisor_20260301",
  name: "advisor",
  model: "claude-opus-4-8",
  max_uses: 3,
  max_tokens: 1400,
};
const NORMAL_TOOL = {
  name: "get_weather",
  description: "Get weather for a city",
  input_schema: {
    type: "object",
    properties: { city: { type: "string" } },
    required: ["city"],
  },
};

async function postAnthropic(port, body) {
  const res = await fetch(`http://127.0.0.1:${port}/v1/messages`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  const text = await res.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {}
  return { status: res.status, json };
}

async function runScenario(label, mode, tools) {
  const backend = await startMockOpenAIBackend(mode);
  const gateway = await startOpenAICompatGateway(backend.port);
  const resp = await postAnthropic(gateway.port, {
    model: "mock-model",
    max_tokens: 256,
    messages: [{ role: "user", content: "design a rate limiter" }],
    tools,
  });
  const outbound = backend.captured[0]?.body ?? null;
  backend.server.close();
  gateway.server.close();

  console.log(`\n=== ${label} (backend mode=${mode}) ===`);
  console.log(`(a) gateway HTTP status           : ${resp.status}`);
  console.log(
    `(c) tools as sent to OpenAI backend: ${JSON.stringify(outbound?.tools ?? null)}`
  );
  console.log(
    `(b) anthropic response content    : ${JSON.stringify(resp.json?.content ?? null)}`
  );
  console.log(`    anthropic stop_reason         : ${resp.json?.stop_reason ?? null}`);
}

const mode = process.argv[2] === "text" ? "text" : "call-advisor";
await runScenario(
  "advisor_20260301 + normal tool through gateway",
  mode,
  [ADVISOR_TOOL, NORMAL_TOOL]
);
await runScenario("normal tool only (baseline)", mode, [NORMAL_TOOL]);
