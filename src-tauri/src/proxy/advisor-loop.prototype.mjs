// ============================================================================
// PROTOTYPE — throwaway. Wayfinder 原型票 issue #7(父图 issue #2)。
//
// 要回答的问题:
//   「拦截 advisor 调用 → 本地 /v1/messages 推理 → 以 advisor_tool_result 形状
//    回注」这条端到端回路,对第三方(不识 advisor 服务端工具的)端点,真的能跑通吗?
//    顺带实测契约里的坑:第三方对 advisor_tool_result 未知块 type 的容忍度、
//    回注形状兼容性、暂停/恢复在本地同步模式下的形态。
//
// 形态:独立 Node 桩件,真机打本机 cc-switch 代理(127.0.0.1:15721,当前路由 k3)。
//   桩件扮演「代理」:把 advisor_20260301 服务端工具降级为客户端工具,捕获其
//   tool_use,本地跑一次 Oracle 提示的 advisor 子推理(空 tools、无防重入——
//   子请求本就不带 advisor 工具),再把建议以 advisor_tool_result 回注、续 executor。
//
// 运行: node src-tauri/src/proxy/advisor-loop.prototype.mjs
//   (或用 npm script: pnpm proto:advisor)
// ============================================================================

import readline from "node:readline";

// ---------------------------------------------------------------------------
// 配置(内存态,可按键修改)
// ---------------------------------------------------------------------------
const CONFIG = {
  baseUrl: process.env.CCS_BASE_URL ?? "http://127.0.0.1:15721/v1/messages",
  model: process.env.CCS_MODEL ?? "k3", // 当前 cc-switch 路由模型别名
  executorMaxTokens: 1024,
  advisorMaxTokens: 1400, // 官方典型 advisor 输出 400-700 文本 token(含 thinking 更多)
  injectShape: "tool_result", // "tool_result" | "advisor_tool_result" —— 实测(见 #7):本机 k3 端点对 advisor_tool_result 未知块 400,故默认回退 tool_result
};

// Oracle 系统提示(map #2 资产 ORACLE_DEFAULT_PROMPT 的极简改造版;原型只用其「只读、给架构/调试建议、不执行」的语义)。
const ORACLE_SYSTEM = `You are the Oracle — a read-only strategic advisor consulted by a working (executor) model.
You receive the executor's full transcript as quoted context: its system prompt, tool definitions, prior turns and tool results, and the text it has produced so far this turn.
Give sharp, concrete advice on architecture decisions, self-review, and stubborn debugging. You CANNOT write files, run tools, or take actions — you only advise. Be concise (a few hundred words). Do not restate the transcript; add what the executor is missing.`;

// ---------------------------------------------------------------------------
// 纯逻辑(可移植,无 I/O)——这部分是原型真正在验证的东西
// ---------------------------------------------------------------------------

// 请求侧:剥离 advisor_20260301,捕获其参数,降级为客户端工具。
//   (serde_json Value 语义:输入是普通 JS 对象,原地不改,返回新对象)
function stripAdvisorTool(requestBody) {
  const tools = Array.isArray(requestBody.tools) ? requestBody.tools : [];
  const advisorTool = tools.find((t) => t?.type === "advisor_20260301");
  if (!advisorTool) return { capture: null, request: requestBody };

  const capture = {
    model: advisorTool.model ?? null,
    max_uses: advisorTool.max_uses ?? null,
    max_tokens: advisorTool.max_tokens ?? null,
    caching: advisorTool.caching ?? null,
  };

  // 剥掉 advisor_20260301,注入一个普通客户端工具 advisor(允许可选 focus——对官方「input 恒空」的有意偏离)。
  const clientAdvisorTool = {
    name: "advisor",
    description:
      "IMPORTANT: call this BEFORE any substantive design or decision, and when you believe the task is done. Consults a stronger reviewer model that sees your full transcript and returns expert advice. Optionally pass a short `focus` describing what to review.",
    input_schema: {
      type: "object",
      properties: { focus: { type: "string" } },
      required: [],
    },
  };
  const remaining = tools.filter((t) => t?.type !== "advisor_20260301");
  const request = {
    ...requestBody,
    tools: [...remaining, clientAdvisorTool],
  };
  return { capture, request };
}

// 响应侧:捕获 name=="advisor" 的 tool_use。
function findAdvisorCall(response) {
  const content = Array.isArray(response?.content) ? response.content : [];
  return content.find((b) => b?.type === "tool_use" && b?.name === "advisor") ?? null;
}

// 组装 advisor 子调用:system=Oracle、quoted 转录、空 tools(协议层强制只读)。
function buildAdvisorRequest(capture, ctx) {
  const { executorSystem, executorTools, messages, producedText, focus } = ctx;
  const transcript = {
    executor_system_prompt: executorSystem ?? null,
    executor_tool_definitions: executorTools ?? [],
    conversation: messages,
    executor_text_this_turn: producedText ?? "",
    executor_focus: focus ?? null,
  };
  const userPayload =
    "The executor model has paused to consult you. Below is its full transcript as quoted JSON. Advise it.\n\n" +
    JSON.stringify(transcript, null, 2);
  return {
    model: capture.model ?? CONFIG.model, // advisor 模型档位:本原型沿用 executor 端点(见 #6)
    max_tokens: capture.max_tokens ?? CONFIG.advisorMaxTokens,
    system: ORACLE_SYSTEM,
    tools: [], // 防重入 + 强制只读:子调用不带任何工具
    messages: [{ role: "user", content: userPayload }],
  };
}

// 回注:assistant 含 tool_use 的消息 + 下一条 user 消息(advisor_tool_result 或普通 tool_result)。
function buildAdvisorResultInjection({ assistantMessage, toolUseId, adviceText, shape }) {
  const resultBlock =
    shape === "tool_result"
      ? { type: "tool_result", tool_use_id: toolUseId, content: adviceText }
      : {
          type: "advisor_tool_result",
          tool_use_id: toolUseId,
          content: { type: "advisor_result", text: adviceText },
        };
  return { assistantMessage, userMessage: { role: "user", content: [resultBlock] } };
}

// ---------------------------------------------------------------------------
// 薄 TUI 外壳(fetch + 渲染 + 键盘)—— throwaway,不可移植
// ---------------------------------------------------------------------------
const B = (s) => `\x1b[1m${s}\x1b[0m`;
const D = (s) => `\x1b[2m${s}\x1b[0m`;
const C = (s) => `\x1b[36m${s}\x1b[0m`;
const G = (s) => `\x1b[32m${s}\x1b[0m`;
const R = (s) => `\x1b[31m${s}\x1b[0m`;
const Y = (s) => `\x1b[33m${s}\x1b[0m`;

const state = {
  running: false,
  seedTask:
    "We're designing the concurrency core of a production HTTP proxy that must drain in-flight requests on shutdown while enforcing per-client rate limits. Before you propose any design, you MUST first call the `advisor` tool to get a second opinion, then incorporate its advice into your final answer.",
  capture: null, // 请求侧捕获的 advisor 参数
  transcript: [], // 客户端视角的 executor 转录(含 assistant/user 各轮)
  pendingAdvisorToolUse: null, // 捕获到、待取建议的 tool_use
  lastAdvice: null, // 最近一次 advisor 建议文本
  advisorCalls: 0,
  loopGuard: 0,
  finished: false,
  log: [],
  lastError: null,
  notes: [], // 实测观察(契约坑)
};

function logLine(s) {
  state.log.push(s);
  if (state.log.length > 30) state.log.shift();
}
function note(s) {
  state.notes.push(s);
}

async function callEndpoint(body) {
  const res = await fetch(CONFIG.baseUrl, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  const text = await res.text();
  let json = null;
  try {
    json = JSON.parse(text);
  } catch {
    /* 非 JSON(如 SSE)——原型按错误处理 */
  }
  return { status: res.status, json, raw: text.slice(0, 600) };
}

// 跑 advisor 子推理,取回建议文本。
async function runAdvisor(focus) {
  state.advisorCalls += 1;
  const last = state.transcript[state.transcript.length - 1];
  const lastContent = last && Array.isArray(last.content) ? last.content : [];
  const producedText = lastContent
    .filter((b) => b && b.type === "text" && typeof b.text === "string")
    .map((b) => b.text)
    .join("\n");
  const req = buildAdvisorRequest(state.capture, {
    executorSystem: "(prototype seed task in first user msg)",
    executorTools: [{ name: "advisor" }],
    messages: state.transcript,
    producedText,
    focus,
  });
  const { status, json, raw } = await callEndpoint(req);
  if (status !== 200 || !json) {
    state.lastError = `advisor 子调用失败 HTTP ${status}: ${raw}`;
    note(`advisor 子调用 HTTP ${status}(shape=${CONFIG.injectShape})`);
    return `(advisor unavailable: HTTP ${status})`;
  }
  // advisor 返回 [thinking, text]:丢弃 thinking、只留 text(复刻官方「thinking 丢弃」)。
  const blocks = Array.isArray(json.content) ? json.content : [];
  const text = blocks
    .filter((b) => b && b.type === "text" && typeof b.text === "string")
    .map((b) => b.text)
    .join("\n");
  return text || `(advisor 返回空文本;块类型=[${blocks.map((b) => b?.type).join(",")}])`;
}

// executor 单步:发当前 transcript,处理 advisor 捕获/续跑。最多自转 loopGuard 次。
async function stepExecutor() {
  state.lastError = null;
  while (state.loopGuard < 6) {
    state.loopGuard += 1;

    const reqBody = {
      model: CONFIG.model,
      max_tokens: CONFIG.executorMaxTokens,
      messages: state.transcript,
      // 仅首轮注入客户端 advisor 工具;续跑轮保持工具可见,让 executor 可再调
      tools: [
        {
          name: "advisor",
          description:
            "IMPORTANT: call this BEFORE any substantive design or decision, and when you believe the task is done. Consults a stronger reviewer model that sees your full transcript and returns expert advice. Optionally pass a short `focus`.",
          input_schema: { type: "object", properties: { focus: { type: "string" } }, required: [] },
        },
      ],
    };

    const { status, json, raw } = await callEndpoint(reqBody);
    if (status !== 200 || !json) {
      state.lastError = `executor 调用失败 HTTP ${status}: ${raw}`;
      note(`executor 在 shape=${CONFIG.injectShape} 下续跑 HTTP ${status}`);
      return;
    }

    const assistantMsg = { role: "assistant", content: json.content ?? [] };
    state.transcript.push(assistantMsg);
    const stop = json.stop_reason;
    logLine(`executor 轮次 stop_reason=${D(stop)} content=[${(json.content ?? []).map((b) => b.type).join(", ")}]`);

    const advisorCall = findAdvisorCall(json);
    if (advisorCall) {
      state.pendingAdvisorToolUse = advisorCall;
      logLine(Y(`捕获 advisor 调用 id=${advisorCall.id} input=${JSON.stringify(advisorCall.input)}`));
      const advice = await runAdvisor(advisorCall.input?.focus ?? null);
      state.lastAdvice = advice;
      logLine(G(`advisor 建议(${advice.length} 字符): ${advice.slice(0, 120)}…`));

      const { userMessage } = buildAdvisorResultInjection({
        assistantMessage: assistantMsg,
        toolUseId: advisorCall.id,
        adviceText: advice,
        shape: CONFIG.injectShape,
      });
      state.transcript.push(userMessage);
      logLine(C(`回注 shape=${CONFIG.injectShape} → 续跑 executor`));
      state.pendingAdvisorToolUse = null;
      continue; // 续跑
    }

    // 无 advisor 调用:本轮结束
    if (stop === "end_turn" || stop === "stop_sequence" || stop == null) {
      state.finished = true;
    }
    return;
  }
  state.finished = true; // 防御:达到 guard 上限
}

// ---- 渲染 ----
function render() {
  console.clear();
  const lines = [];
  lines.push(B("advisor 本地降级回路 — 原型 (issue #7)"));
  lines.push(D(`端点 ${CONFIG.baseUrl}  model=${CONFIG.model}  回注形状=${C(CONFIG.injectShape)}`));
  lines.push("");
  lines.push(B("状态"));
  lines.push(`  阶段:            ${state.finished ? G("已结束") : state.pendingAdvisorToolUse ? Y("已捕获 advisor,待续") : state.running ? C("运行中") : "就绪"}`);
  lines.push(`  advisor 调用次数: ${state.advisorCalls}   executor 轮次: ${state.loopGuard}`);
  lines.push(`  捕获参数:        ${state.capture ? JSON.stringify(state.capture) : D("(首步发送时从 tools 剥离)")}`);
  lines.push(`  转录长度:        ${state.transcript.length} 条消息`);
  if (state.lastAdvice) {
    lines.push("");
    lines.push(B("最近 advisor 建议"));
    lines.push(G(wrap(state.lastAdvice, 96).map((l) => "  " + l).join("\n")));
  }
  if (state.lastError) {
    lines.push("");
    lines.push(R(B("错误: ") + state.lastError));
  }
  if (state.transcript.length) {
    lines.push("");
    lines.push(B("转录(末条)"));
    const last = state.transcript.at(-1);
    lines.push(`  ${B(last.role)}: ${D(JSON.stringify(last.content).slice(0, 400))}`);
  }
  if (state.notes.length) {
    lines.push("");
    lines.push(B("实测观察"));
    for (const n of state.notes) lines.push("  " + Y("• ") + n);
  }
  if (state.log.length) {
    lines.push("");
    lines.push(B("日志"));
    for (const l of state.log.slice(-10)) lines.push("  " + D(l));
  }
  lines.push("");
  lines.push(B("按键"));
  lines.push(
    `  ${B("[s]")} 播种并开始   ${B("[e]")} executor 续跑一步   ${B("[t]")} 切换回注形状(${CONFIG.injectShape})   ${B("[r]")} 重置   ${B("[q]")} 退出`
  );
  process.stdout.write(lines.join("\n") + "\n");
}

function wrap(s, w) {
  const out = [];
  for (const para of s.split("\n")) {
    let line = para;
    while (line.length > w) {
      out.push(line.slice(0, w));
      line = line.slice(w);
    }
    out.push(line);
  }
  return out.slice(0, 14);
}

async function handleKey(key) {
  if (state.running) return;
  if (key === "q") {
    process.exit(0);
  } else if (key === "t") {
    CONFIG.injectShape = CONFIG.injectShape === "advisor_tool_result" ? "tool_result" : "advisor_tool_result";
    note(`切换回注形状 → ${CONFIG.injectShape}(实测 D2:第三方对未知块 type 容忍度)`);
  } else if (key === "r") {
    Object.assign(state, {
      running: false,
      capture: null,
      transcript: [],
      pendingAdvisorToolUse: null,
      lastAdvice: null,
      advisorCalls: 0,
      loopGuard: 0,
      finished: false,
      log: [],
      lastError: null,
      notes: [],
    });
  } else if (key === "s") {
    state.running = true;
    render();
    // 请求侧:构造含 advisor_20260301 的原始请求,剥离并注入客户端工具(展示 capture)
    const raw = {
      model: CONFIG.model,
      max_tokens: CONFIG.executorMaxTokens,
      tools: [{ type: "advisor_20260301", name: "advisor", model: CONFIG.model, max_uses: 3 }],
      messages: [{ role: "user", content: state.seedTask }],
    };
    const { capture, request } = stripAdvisorTool(raw);
    state.capture = capture;
    logLine(`请求侧剥离 advisor_20260301 → 注入客户端 advisor 工具;捕获=${JSON.stringify(capture)}`);
    state.transcript = request.messages;
    await stepExecutor();
    state.running = false;
  } else if (key === "e") {
    if (state.finished) return;
    state.running = true;
    render();
    await stepExecutor();
    state.running = false;
  }
  render();
}

readline.emitKeypressEvents(process.stdin);
if (process.stdin.isTTY) process.stdin.setRawMode(true);
process.stdin.on("keypress", async (_str, key) => {
  if (key.ctrl && key.name === "c") process.exit(0);
  await handleKey(key.name ?? key.sequence);
});
render();
