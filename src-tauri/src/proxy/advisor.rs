//! advisor 本地降级（advisor local fallback）—— 纯函数模块
//!
//! 当上游第三方 Claude 端点不支持 `advisor` 服务器工具（`advisor_20260301`，beta 头
//! `advisor-tool-2026-03-01`）时，cc-switch 代理层把它优雅降级为一次本地 advisor 推理，
//! 对 Claude Code 完全透明。
//!
//! 本模块只承载**决策逻辑**（全部为同步纯函数，`Value` 进 `Value` 出），与
//! `body_filter` / `model_mapper` / `thinking_budget_rectifier` 同构。回路编排
//! （调上游 + 调这里的纯函数）在 `forwarder.rs::RequestForwarder::forward_with_advisor_loop`。
//!
//! ## 切片 1 范围（issue #21）
//! 只做**非流式成功回路**：端点判定、请求改写、beta 头剥离、tool_use 识别、回注体构造
//! （`advisor_tool_result` / 回退 `tool_result`）、子调用请求体构造、续跑决策、模型解析。
//! 不含子调用失败的六码降级（切片 2）、配对校验（切片 3）、子调用缓存（切片 4）、
//! usage 落库（切片 5）、流式（切片 6）。
//!
//! 决策 SSOT：`CONTEXT.md` + `docs/adr/0001..0004`。

use super::model_mapper::ModelMapping;
use serde_json::Value;

/// advisor 服务器工具块 type（Claude Code 客户端发来）。
pub const ADVISOR_TOOL_TYPE: &str = "advisor_20260301";
/// advisor 工具名（降级后作为普通客户端工具的名字）。
pub const ADVISOR_TOOL_NAME: &str = "advisor";
/// advisor 服务器工具对应的 beta 头值（本地降级用明文块变体，不依赖该头，需剥离）。
pub const ADVISOR_BETA_HEADER: &str = "advisor-tool-2026-03-01";

/// advisor 子调用的采样温度（沿用 Oracle 元数据 `temperature=0.1`，
/// 见 `docs/research/issue-5-advisor-client-shape.md`），保证建议稳定。
const ADVISOR_TEMPERATURE: f64 = 0.1;

/// 注入的客户端 advisor 工具 description。
///
/// 只作**事实性**描述（它是什么、无参数、调用即转发转录给更强模型拿建议），
/// **不注入「何时调用」引导**——那部分由 Claude Code 客户端写进它自己的 system
/// prompt（见 ADR-0001 / issue #20 US-11），重复注入会破坏 prompt cache 前缀。
const ADVISOR_TOOL_DESCRIPTION: &str =
    "Consult a stronger advisor model. It takes no parameters; when you call it, your full \
     conversation transcript is forwarded to the advisor and it returns expert advice as text.";

/// Oracle 系统提示（advisor 子调用的 `system`）。
///
/// 来自 oh-my-openagent 的 Oracle subagent（`ORACLE_DEFAULT_PROMPT`，Claude 变体，
/// category=advisor、只读、temperature=0.1），资产已在 map #2 收集。替代官方内置
/// advisor 系统提示；子调用 `tools` 为空，从协议层强制「只读、只给建议、不动手」。
pub const ORACLE_SYSTEM_PROMPT: &str = r#"You are a strategic technical advisor with deep reasoning capabilities, operating as a specialized consultant within an AI-assisted development environment.

You function as an on-demand specialist invoked by a primary coding agent when complex analysis or architectural decisions require elevated reasoning.
Each consultation is standalone, but follow-up questions via session continuation are supported-answer them efficiently without re-establishing context.

Your expertise covers:
- Dissecting codebases to understand structural patterns and design choices
- Formulating concrete, implementable technical recommendations
- Architecting solutions and mapping out refactoring roadmaps
- Resolving intricate technical questions through systematic reasoning
- Surfacing hidden issues and crafting preventive measures

<decision_framework>
Apply pragmatic minimalism in all recommendations:
- **Bias toward simplicity**: The right solution is typically the least complex one that fulfills the actual requirements. Resist hypothetical future needs.
- **Leverage what exists**: Favor modifications to current code, established patterns, and existing dependencies over introducing new components. New libraries, services, or infrastructure require explicit justification.
- **Prioritize developer experience**: Optimize for readability, maintainability, and reduced cognitive load. Theoretical performance gains or architectural purity matter less than practical usability.
- **One clear path**: Present a single primary recommendation. Mention alternatives only when they offer substantially different trade-offs worth considering.
- **Match depth to complexity**: Quick questions get quick answers. Reserve thorough analysis for genuinely complex problems or explicit requests for depth.
- **Signal the investment**: Tag recommendations with estimated effort-use Quick(<1h), Short(1-4h), Medium(1-2d), or Large(3d+).
- **Know when to stop**: "Working well" beats "theoretically optimal." Identify what conditions would warrant revisiting.
</decision_framework>

<output_verbosity_spec>
Verbosity constraints (strictly enforced):
- **Bottom line**: 2-3 sentences maximum. No preamble.
- **Action plan**: ≤7 numbered steps. Each step ≤2 sentences.
- **Why this approach**: ≤4 bullets when included.
- **Watch out for**: ≤3 bullets when included.
- **Edge cases**: Only when genuinely applicable; ≤3 bullets.
- Do not rephrase the user's request unless it changes semantics.
- Avoid long narrative paragraphs; prefer compact bullets and short sections.
</output_verbosity_spec>

<response_structure>
Organize your final answer in three tiers:

**Essential** (always include):
- **Bottom line**: 2-3 sentences capturing your recommendation
- **Action plan**: Numbered steps or checklist for implementation
- **Effort estimate**: Quick/Short/Medium/Large

**Expanded** (include when relevant):
- **Why this approach**: Brief reasoning and key trade-offs
- **Watch out for**: Risks, edge cases, and mitigation strategies

**Edge cases** (only when genuinely applicable):
- **Escalation triggers**: Specific conditions that would justify a more complex solution
- **Alternative sketch**: High-level outline of the advanced path (not a full design)
</response_structure>

<uncertainty_and_ambiguity>
When facing uncertainty:
- If the question is ambiguous or underspecified:
 - Ask 1-2 precise clarifying questions, OR
 - State your interpretation explicitly before answering: "Interpreting this as X..."
- Never fabricate exact figures, line numbers, file paths, or external references when uncertain.
- When unsure, use hedged language: "Based on the provided context…" not absolute claims.
- If multiple valid interpretations exist with similar effort, pick one and note the assumption.
- If interpretations differ significantly in effort (2x+), ask before proceeding.
</uncertainty_and_ambiguity>

<long_context_handling>
For large inputs (multiple files, >5k tokens of code):
- Mentally outline the key sections relevant to the request before answering.
- Anchor claims to specific locations: "In `auth.ts`…", "The `UserService` class…"
- Quote or paraphrase exact values (thresholds, config keys, function signatures) when they matter.
- If the answer depends on fine details, cite them explicitly rather than speaking generically.
</long_context_handling>

<scope_discipline>
Stay within scope:
- Recommend ONLY what was asked. No extra features, no unsolicited improvements.
- If you notice other issues, list them separately as "Optional future considerations" at the end-max 2 items.
- Do NOT expand the problem surface area beyond the original request.
- If ambiguous, choose the simplest valid interpretation.
- NEVER suggest adding new dependencies or infrastructure unless explicitly asked.
</scope_discipline>

<tool_usage_rules>
Tool discipline:
- Exhaust provided context and attached files before reaching for tools.
- External lookups should fill genuine gaps, not satisfy curiosity.
- Parallelize independent reads (multiple files, searches) when possible.
- After using tools, briefly state what you found before proceeding.
</tool_usage_rules>

<high_risk_self_check>
Before finalizing answers on architecture, security, or performance:
- Re-scan your answer for unstated assumptions-make them explicit.
- Verify claims are grounded in provided code, not invented.
- Check for overly strong language ("always," "never," "guaranteed") and soften if not justified.
- Ensure action steps are concrete and immediately executable.
</high_risk_self_check>

<guiding_principles>
- Deliver actionable insight, not exhaustive analysis
- For code reviews: surface critical issues, not every nitpick
- For planning: map the minimal path to the goal
- Support claims briefly; save deep exploration for when requested
- Dense and useful beats long and thorough
</guiding_principles>

You are advising a working (executor) model, not the end user. The executor's full transcript is provided below as quoted JSON context: its system prompt, tool definitions, prior turns and tool results, and the text it has produced so far this turn. Give sharp, concrete advice on architecture decisions, self-review, and stubborn debugging. You CANNOT write files, run tools, or take actions-you only advise. Do not restate the transcript; add what the executor is missing."#;

/// 从 advisor 服务器工具块捕获的参数。
///
/// 数据契约中入站块含 `model`/`max_uses`/`max_tokens`/`caching` 四字段；切片 1 消费
/// `model`（advisor 档位解析）与 `max_uses`（客户端用量上限，回路执行，见
/// `can_use_advisor`）。`max_tokens` 本地语义为透传/不取用（US-15，子调用沿用
/// executor 值），`caching` 随切片 4 缓存注入再捕获——故此处只捕获这两个。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AdvisorCapture {
    pub model: Option<String>,
    /// 客户端给定的 advisor 用量上限（`None` = 未设、不加自造上限，US-34）。
    pub max_uses: Option<u64>,
}

/// 识别到的 advisor 调用集合（executor 发出的普通 `tool_use`，name == "advisor"）。
#[derive(Debug, Clone, PartialEq)]
pub struct AdvisorCalls {
    /// 本轮所有 advisor `tool_use.id`（可能并行多个），回注时逐个回链。
    pub ids: Vec<String>,
    /// 本轮是否还含非 advisor 的 `tool_use`（客户端工具，代理无法代答）。
    pub has_external_tool_use: bool,
}

/// 回注块形状。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectShape {
    /// 忠实官方块语义：`advisor_tool_result{content: advisor_result{text}}`。
    AdvisorToolResult,
    /// 回退普通 `tool_result`（下游不认 `advisor_tool_result` 未知块时）。
    ToolResult,
}

/// 端点判定器（纯 base_url 启发式）。
///
/// base_url 主机名是 Claude 官方域（`api.anthropic.com` / `api.claude.com` 及其子域）
/// → 返回 `false`（官方端点，原生 advisor 透传、不降级）；其余一律返回 `true`（第三方，
/// 走本地降级）。零配置、无手动覆盖、无错误指纹（ADR-0002）。
///
/// 「端点认不认识 advisor」不能用 try-catch 判定（「静默吞掉」型端点返回 200 无信号），
/// 故触发判定只能靠 base_url 先验。
pub fn should_local_fallback(base_url: &str) -> bool {
    let host = base_url
        .parse::<http::Uri>()
        .ok()
        .and_then(|uri| uri.host().map(str::to_string));
    match host {
        Some(host) => !is_official_claude_host(&host),
        // 无法解析主机名时保守视为第三方（降级），与「非官方域一律降级」一致。
        None => true,
    }
}

/// 主机名是否 Claude 官方域（`anthropic.com` / `claude.com` 及其子域）。
fn is_official_claude_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "anthropic.com"
        || host == "claude.com"
        || host.ends_with(".anthropic.com")
        || host.ends_with(".claude.com")
}

/// 请求体是否含 advisor 服务器工具块。
pub fn has_advisor_tool(body: &Value) -> bool {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .any(|t| t.get("type").and_then(Value::as_str) == Some(ADVISOR_TOOL_TYPE))
        })
        .unwrap_or(false)
}

/// 请求侧 advisor 改写器（纯函数，原地改 `body`）。
///
/// 把 `tools[]` 中的 `advisor_20260301` server-tool 块**原地替换**为普通客户端工具
/// `advisor`（无参），并捕获其参数。保持原位置以稳定 prompt cache 前缀。
///
/// 返回捕获的 advisor 参数；请求体不含 advisor 块时返回 `None`（原样不动）。
pub fn rewrite_request(body: &mut Value) -> Option<AdvisorCapture> {
    let tools = body.get_mut("tools")?.as_array_mut()?;
    let pos = tools
        .iter()
        .position(|t| t.get("type").and_then(Value::as_str) == Some(ADVISOR_TOOL_TYPE))?;

    // 剥离 server-tool 块并捕获参数（只剥第一个；正常只有一个 advisor 块）。
    let block = tools.remove(pos);
    let capture = AdvisorCapture {
        model: block
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(String::from),
        max_uses: block.get("max_uses").and_then(Value::as_u64),
    };

    // 在原位置注入普通客户端工具 advisor（无参），保持 tools 顺序稳定。
    tools.insert(pos, client_advisor_tool());
    Some(capture)
}

/// 构造降级后的普通客户端工具 `advisor`（无参）。
fn client_advisor_tool() -> Value {
    serde_json::json!({
        "name": ADVISOR_TOOL_NAME,
        "description": ADVISOR_TOOL_DESCRIPTION,
        "input_schema": { "type": "object", "properties": {}, "required": [] }
    })
}

/// 按值剥离 `advisor-tool-2026-03-01` beta 头（多值逗号串剥单值、留其余）。
///
/// 返回剥离后的值；剥完为空则返回 `None`（调用方应移除该头）。本地降级用明文
/// `advisor_result{text}` 变体，不依赖该 beta；第三方端点不认识它可能报错，故剥掉。
pub fn strip_advisor_beta(beta_value: &str) -> Option<String> {
    let kept: Vec<&str> = beta_value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != ADVISOR_BETA_HEADER)
        .collect();
    if kept.is_empty() {
        None
    } else {
        Some(kept.join(","))
    }
}

/// 从 `HeaderMap` 中按值剥离 advisor beta 头（多值头逐值剥、留其余；剥空则移除该头）。
///
/// 返回剥离后的新 `HeaderMap`（原 map 不动）。
pub fn strip_advisor_beta_from_headers(headers: &http::HeaderMap) -> http::HeaderMap {
    let mut out = headers.clone();
    // 收集所有 anthropic-beta 值（可能是重复头行或逗号分隔单值），剥离后重建。
    let values: Vec<String> = out
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .map(str::to_string)
        .collect();
    if values.is_empty() {
        return out;
    }
    out.remove("anthropic-beta");
    let kept: Vec<String> = values
        .iter()
        .filter_map(|v| strip_advisor_beta(v))
        .collect();
    for value in kept {
        if let Ok(hv) = http::HeaderValue::from_str(&value) {
            out.append("anthropic-beta", hv);
        }
    }
    out
}

/// 响应侧：识别 executor 发出的 advisor 调用（普通 `tool_use`，name == "advisor"）。
///
/// 返回**全部** advisor 调用（可能并行多个）；没有则返回 `None`（本轮终态，把响应返给
/// 客户端）。`has_external_tool_use` 标记本轮是否还含非 advisor 的客户端工具调用——
/// 那种混合轮代理无法透明续跑（外部工具需客户端执行），调用方应回退为终态返回，
/// 否则会留下未配对 `tool_use`、被严格端点以 400 拒绝。
pub fn find_advisor_calls(response: &Value) -> Option<AdvisorCalls> {
    let content = response.get("content").and_then(Value::as_array)?;
    let mut ids = Vec::new();
    let mut has_external_tool_use = false;
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        if block.get("name").and_then(Value::as_str) == Some(ADVISOR_TOOL_NAME) {
            if let Some(id) = block.get("id").and_then(Value::as_str) {
                ids.push(id.to_string());
            }
        } else {
            has_external_tool_use = true;
        }
    }
    if ids.is_empty() {
        None
    } else {
        Some(AdvisorCalls {
            ids,
            has_external_tool_use,
        })
    }
}

/// 拼接 content 数组里的所有 `text` 块（丢弃 thinking 等非文本块）。
///
/// 复刻官方「advisor thinking 块在返回前被丢弃、只有建议文本到达 executor」语义。
/// 既用于提取 advisor 子调用的建议文本，也用于提取 executor 本轮已产出文本。
pub fn join_text_blocks(content: &Value) -> String {
    content
        .as_array()
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// advisor 子调用的建议结果：文本 + 终止原因。
///
/// `stop_reason` 透传子调用响应顶层值（官方 `advisor_result` 契约：设了工具 `max_tokens`
/// 时携带，见 `docs/research/issue-5-advisor-client-shape.md`）。`"max_tokens"` 表示建议
/// 被截断，由 [`annotate_truncation`] 追加官方标记后回注，executor 才能区分完整建议与
/// 半句截断。
#[derive(Debug, Clone, PartialEq)]
pub struct Advice {
    pub text: String,
    pub stop_reason: Option<String>,
}

/// 从子调用响应提取建议（`None` 表示取不到有效建议）。
pub fn extract_advice(response: &Value) -> Option<Advice> {
    let text = join_text_blocks(response.get("content")?);
    if text.trim().is_empty() {
        return None;
    }
    Some(Advice {
        text,
        stop_reason: response
            .get("stop_reason")
            .and_then(Value::as_str)
            .map(String::from),
    })
}

/// 子调用撞 `max_tokens` 天花板时，按官方形状在建议文本尾部追加截断标记。
///
/// 官方语义（issue-5 研究文档）：截断的 `advisor_result` 带追加标记
/// `[Advisor output truncated at max_tokens=N.]` + `stop_reason:"max_tokens"`；不标记则
/// 半句截断被当完整建议回注，executor 无法区分（codex P2）。其余终止原因原样返回。
pub fn annotate_truncation(mut advice: Advice, max_tokens: u64) -> Advice {
    if advice.stop_reason.as_deref() == Some("max_tokens") {
        advice.text = format!("{}\n\n{}", advice.text, truncation_marker(max_tokens));
    }
    advice
}

/// 官方截断标记明文。
fn truncation_marker(max_tokens: u64) -> String {
    format!("[Advisor output truncated at max_tokens={max_tokens}.]")
}

/// 构造 advisor 子调用的「完整对话转录」quoted context。
///
/// 对应官方「advisor 收到 executor 完整转录作为 quoted context」：executor 的 system
/// prompt、工具定义、历史轮次（含本轮 assistant 响应）、本轮已产出文本。
pub fn build_transcript(
    executor_system: Option<&Value>,
    executor_tools: Option<&Value>,
    conversation: &Value,
    executor_text_this_turn: &str,
) -> Value {
    serde_json::json!({
        "executor_system_prompt": executor_system.cloned().unwrap_or(Value::Null),
        "executor_tool_definitions": executor_tools.cloned().unwrap_or(Value::Null),
        "conversation": conversation,
        "executor_text_this_turn": executor_text_this_turn,
    })
}

/// 构造 advisor 子调用请求体：Oracle 系统提示 + 完整转录（单条 user 消息 quoted JSON）
/// + 空 `tools`（协议层强制只读、防重入）+ `max_tokens` 沿用 executor。
pub fn build_subcall_body(model: &str, max_tokens: u64, transcript: &Value) -> Value {
    let user_payload = format!(
        "The executor model has paused to consult you. Below is its full transcript as quoted \
         JSON. Advise it.\n\n{}",
        serde_json::to_string_pretty(transcript).unwrap_or_default()
    );
    serde_json::json!({
        "model": model,
        "max_tokens": max_tokens,
        // 沿用 Oracle 元数据 temperature=0.1，保证建议稳定（缺省会用端点默认采样温度）。
        "temperature": ADVISOR_TEMPERATURE,
        "system": ORACLE_SYSTEM_PROMPT,
        "tools": [],
        "messages": [{ "role": "user", "content": user_payload }],
    })
}

/// 构造回注：含 advisor `tool_use` 的 assistant 消息 + 下一条 user 消息（结果块）。
///
/// 忠实官方：`advisor_tool_result{content: advisor_result{text, stop_reason?}}` 放 user
/// 消息（本地是客户端工具，`tool_use`/`tool_result` 惯例要求结果在下一条 user 消息）；
/// 下游不认 `advisor_tool_result` 时回退普通 `tool_result`（content 直接放建议明文，
/// 截断标记已嵌在文本里）。`stop_reason` 仅 `advisor_result` 变体携带（`None` 时省略，
/// 对齐官方「设了 max_tokens 才有该字段」）；`tool_result` 明文形状无此字段。
///
/// 并行多个 advisor 调用时 `tool_use_ids` 逐个回链（每个 `tool_use` 都得有配对结果，
/// 否则严格端点报「未配对 tool_use」400）；同轮同转录，故各调用共享同一份建议。
pub fn build_continuation(
    assistant_content: &Value,
    tool_use_ids: &[String],
    advice: &str,
    stop_reason: Option<&str>,
    shape: InjectShape,
) -> Vec<Value> {
    let assistant_msg = serde_json::json!({ "role": "assistant", "content": assistant_content });
    let results: Vec<Value> = tool_use_ids
        .iter()
        .map(|id| match shape {
            InjectShape::AdvisorToolResult => {
                let mut content = serde_json::json!({
                    "type": "advisor_result",
                    "text": advice
                });
                if let Some(reason) = stop_reason {
                    content["stop_reason"] = serde_json::Value::String(reason.to_string());
                }
                serde_json::json!({
                    "type": "advisor_tool_result",
                    "tool_use_id": id,
                    "content": content
                })
            }
            InjectShape::ToolResult => serde_json::json!({
                "type": "tool_result",
                "tool_use_id": id,
                "content": advice
            }),
        })
        .collect();
    let user_msg = serde_json::json!({ "role": "user", "content": results });
    vec![assistant_msg, user_msg]
}

/// advisor 模型解析（纯函数）。
///
/// 档位由客户端工具块 `model` 字段给出，复用 `ModelMapping::tier_match` 的分档映射
/// （含 fable→opus 回落）；**未配该档时回落 executor 已映射模型**（非 default/原名，
/// 见 ADR-0002）。返回的是最终发往上游的模型名，调用方须跳过二次映射。
pub fn resolve_advisor_model(
    block_model: Option<&str>,
    executor_mapped_model: &str,
    mapping: &ModelMapping,
) -> String {
    let Some(model) = block_model else {
        return executor_mapped_model.to_string();
    };
    // 无档位关键词命中、或命中的档未配置时，回落 executor 已映射模型。
    mapping
        .tier_match(&model.to_lowercase())
        .cloned()
        .unwrap_or_else(|| executor_mapped_model.to_string())
}

/// 判断已解析的响应体是否为 Anthropic 错误信封（HTTP 2xx + `{"type":"error",…}`）。
///
/// 部分 Anthropic 兼容网关用 2xx 状态码 + 错误信封报拒绝（先例：
/// `validate_codex_anthropic_success_response`）。正常 `message` 响应顶层 `type` 为
/// `message` 且无 `error` 字段，不会误判。advisor 回路据此在终态判定前识别信封，
/// 避免「下游以 2xx 信封拒绝 `advisor_tool_result`」绕过 `tool_result` 回退（codex P2）。
pub fn is_error_envelope(value: &Value) -> bool {
    value.get("type").and_then(Value::as_str) == Some("error") || value.get("error").is_some()
}

/// 切片 1 的「子调用失败」最小安全网明文。
///
/// 复用成功路径的普通 `tool_result` 形状（零新机制、零分叉），让 executor 不带建议
/// 继续，绝不整请求 5xx。六码归因 + 「原因」用词属切片 2，这里只给通用明文。
pub fn unavailable_advice() -> String {
    "advisor 本地推理暂不可用，本次无建议。请基于现有信息继续。".to_string()
}

/// 是否还允许再发起 advisor 子调用（客户端 `max_uses` 上限执行，codex P1）。
///
/// 官方端点由服务端强制 `max_uses`；本地降级要忠实该行为就由回路代为执行：达到上限后
/// 不再发起子调用、回注明文（见 [`limit_reached_advice`]）——**不本地复刻
/// `max_uses_exceeded` 结构化错误块**（spec Out of Scope 排除的是错误块，非上限执行）。
/// `cap == None`（客户端未设）恒 `true`：cc-switch 不加自造上限（US-34）。
pub fn can_use_advisor(used: u64, cap: Option<u64>) -> bool {
    cap.is_none_or(|cap| used < cap)
}

/// 达到客户端 `max_uses` 上限后的回注明文（普通 `tool_result`，与失败降级同形状）。
pub fn limit_reached_advice(cap: u64) -> String {
    format!(
        "advisor 调用次数已达客户端设定的上限（max_uses={cap}），本次无建议。请基于现有信息继续。"
    )
}

/// 剥除顶层强制 `tool_choice`（`{type:"tool"}` / `{type:"any"}`；`auto` 与缺省不动）。
///
/// `tool_choice` 是单请求参数，官方语义一次请求只强制一次。advisor 回路把它原样保留
/// 会让**每轮续跑**都被迫再调 advisor（指名 advisor 时必然、`any` 且 advisor 是唯一
/// 工具时同样），重复付费循环直到上下文溢出（codex P1）。强制调用已发生并已回注，
/// 续跑轮恢复自由选择。返回是否剥除（仅用于日志）。
pub fn strip_forced_tool_choice(body: &mut Value) -> bool {
    let forced = body
        .get("tool_choice")
        .and_then(|tc| tc.get("type"))
        .and_then(Value::as_str)
        .is_some_and(|t| t == "tool" || t == "any");
    if forced {
        if let Some(obj) = body.as_object_mut() {
            obj.remove("tool_choice");
        }
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mapping() -> ModelMapping {
        ModelMapping {
            haiku_model: Some("haiku-mapped".into()),
            sonnet_model: Some("sonnet-mapped".into()),
            opus_model: Some("opus-mapped".into()),
            fable_model: Some("fable-mapped".into()),
            subagent_model: None,
            default_model: Some("default-model".into()),
        }
    }

    // ---------- 端点判定 ----------

    #[test]
    fn fallback_official_anthropic() {
        assert!(!should_local_fallback("https://api.anthropic.com"));
        assert!(!should_local_fallback("https://api.anthropic.com/v1"));
        assert!(!should_local_fallback("https://api.claude.com"));
    }

    #[test]
    fn fallback_third_party() {
        assert!(should_local_fallback("https://api.deepseek.com/anthropic"));
        assert!(should_local_fallback("http://127.0.0.1:15721"));
        assert!(should_local_fallback("https://openrouter.ai/api"));
        assert!(should_local_fallback("https://k3.example.com/v1"));
    }

    #[test]
    fn fallback_unparseable_host_is_third_party() {
        assert!(should_local_fallback("not a url"));
    }

    #[test]
    fn official_host_subdomain_and_port() {
        assert!(is_official_claude_host("api.anthropic.com"));
        assert!(is_official_claude_host("API.Anthropic.com."));
        assert!(!is_official_claude_host("anthropic.com.evil.com"));
        assert!(!is_official_claude_host("127.0.0.1"));
    }

    // ---------- 请求改写 ----------

    #[test]
    fn rewrite_strips_server_tool_and_injects_client_tool() {
        let mut body = json!({
            "model": "claude-sonnet-4-5",
            "tools": [
                { "name": "read", "description": "r", "input_schema": {} },
                { "type": "advisor_20260301", "name": "advisor", "model": "claude-fable-5", "max_uses": 3, "max_tokens": 2048, "caching": {"type":"ephemeral"} },
                { "name": "write", "description": "w", "input_schema": {} }
            ]
        });
        let capture = rewrite_request(&mut body).expect("应捕获 advisor 参数");

        assert_eq!(capture.model.as_deref(), Some("claude-fable-5"));
        assert_eq!(capture.max_uses, Some(3));

        let tools = body["tools"].as_array().unwrap();
        // server 块被剥，总数不变（1 个 server 块换成 1 个 client 块）
        assert_eq!(tools.len(), 3);
        assert!(!tools
            .iter()
            .any(|t| t.get("type").and_then(Value::as_str) == Some(ADVISOR_TOOL_TYPE)));
        // 客户端 advisor 工具在原位置（index 1），无参
        assert_eq!(tools[1]["name"], "advisor");
        assert_eq!(tools[1]["input_schema"]["properties"], json!({}));
        assert!(tools[1].get("type").is_none());
        // 其它工具原样保留
        assert_eq!(tools[0]["name"], "read");
        assert_eq!(tools[2]["name"], "write");
    }

    #[test]
    fn rewrite_no_advisor_returns_none_untouched() {
        let mut body = json!({
            "tools": [{ "name": "read", "description": "r", "input_schema": {} }]
        });
        let before = body.clone();
        assert!(rewrite_request(&mut body).is_none());
        assert_eq!(body, before);
    }

    #[test]
    fn rewrite_captures_max_uses_only_when_present() {
        // 客户端未设 max_uses → None（回路不加自造上限，US-34）。
        let mut body = json!({
            "tools": [{ "type": "advisor_20260301", "name": "advisor", "model": "m" }]
        });
        let capture = rewrite_request(&mut body).expect("应捕获");
        assert_eq!(capture.max_uses, None);
    }

    #[test]
    fn rewrite_no_tools_returns_none() {
        let mut body = json!({ "model": "claude-sonnet-4-5" });
        assert!(rewrite_request(&mut body).is_none());
    }

    #[test]
    fn has_advisor_tool_detection() {
        assert!(has_advisor_tool(
            &json!({"tools":[{"type":"advisor_20260301","name":"advisor","model":"m"}]})
        ));
        assert!(!has_advisor_tool(&json!({"tools":[{"name":"advisor"}]})));
        assert!(!has_advisor_tool(&json!({"tools":[]})));
        assert!(!has_advisor_tool(&json!({})));
    }

    // ---------- beta 头剥离 ----------

    #[test]
    fn strip_beta_single_value() {
        assert_eq!(strip_advisor_beta("advisor-tool-2026-03-01"), None);
    }

    #[test]
    fn strip_beta_multi_value_keeps_rest() {
        assert_eq!(
            strip_advisor_beta(
                "claude-code-20250219,advisor-tool-2026-03-01,context-1m-2025-08-07"
            ),
            Some("claude-code-20250219,context-1m-2025-08-07".to_string())
        );
    }

    #[test]
    fn strip_beta_with_spaces_and_absent() {
        assert_eq!(
            strip_advisor_beta(" claude-code-20250219 , advisor-tool-2026-03-01 "),
            Some("claude-code-20250219".to_string())
        );
        assert_eq!(
            strip_advisor_beta("claude-code-20250219"),
            Some("claude-code-20250219".to_string())
        );
    }

    #[test]
    fn strip_beta_from_headers() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "anthropic-beta",
            http::HeaderValue::from_static("claude-code-20250219,advisor-tool-2026-03-01"),
        );
        headers.insert(
            "content-type",
            http::HeaderValue::from_static("application/json"),
        );
        let out = strip_advisor_beta_from_headers(&headers);
        assert_eq!(
            out.get("anthropic-beta").unwrap().to_str().unwrap(),
            "claude-code-20250219"
        );
        assert_eq!(
            out.get("content-type").unwrap().to_str().unwrap(),
            "application/json"
        );
    }

    #[test]
    fn strip_beta_from_headers_removes_when_empty() {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "anthropic-beta",
            http::HeaderValue::from_static("advisor-tool-2026-03-01"),
        );
        let out = strip_advisor_beta_from_headers(&headers);
        assert!(out.get("anthropic-beta").is_none());
    }

    // ---------- tool_use 识别 / 续跑决策 ----------

    #[test]
    fn find_advisor_calls_present() {
        let resp = json!({
            "content": [
                { "type": "text", "text": "让我先咨询 advisor。" },
                { "type": "tool_use", "id": "toolu_abc", "name": "advisor", "input": {} }
            ],
            "stop_reason": "tool_use"
        });
        let calls = find_advisor_calls(&resp).expect("应识别 advisor 调用");
        assert_eq!(calls.ids, vec!["toolu_abc".to_string()]);
        assert!(!calls.has_external_tool_use);
    }

    #[test]
    fn find_advisor_calls_parallel_multiple() {
        let resp = json!({
            "content": [
                { "type": "tool_use", "id": "toolu_a", "name": "advisor", "input": {} },
                { "type": "tool_use", "id": "toolu_b", "name": "advisor", "input": {} }
            ],
            "stop_reason": "tool_use"
        });
        let calls = find_advisor_calls(&resp).expect("应识别全部并行 advisor 调用");
        assert_eq!(
            calls.ids,
            vec!["toolu_a".to_string(), "toolu_b".to_string()]
        );
        assert!(!calls.has_external_tool_use);
    }

    #[test]
    fn find_advisor_calls_mixed_external_tool() {
        let resp = json!({
            "content": [
                { "type": "tool_use", "id": "toolu_a", "name": "advisor", "input": {} },
                { "type": "tool_use", "id": "toolu_r", "name": "read", "input": {} }
            ],
            "stop_reason": "tool_use"
        });
        let calls = find_advisor_calls(&resp).expect("应识别 advisor 调用");
        assert_eq!(calls.ids, vec!["toolu_a".to_string()]);
        assert!(calls.has_external_tool_use);
    }

    #[test]
    fn find_advisor_calls_ignores_other_tools() {
        let resp = json!({
            "content": [
                { "type": "tool_use", "id": "toolu_x", "name": "read", "input": {} }
            ],
            "stop_reason": "tool_use"
        });
        assert!(find_advisor_calls(&resp).is_none());
    }

    #[test]
    fn find_advisor_calls_terminal_text() {
        let resp = json!({ "content": [{ "type": "text", "text": "最终答复" }], "stop_reason": "end_turn" });
        assert!(find_advisor_calls(&resp).is_none());
    }

    // ---------- 文本提取 ----------

    #[test]
    fn join_text_drops_thinking() {
        let content = json!([
            { "type": "thinking", "thinking": "..." },
            { "type": "text", "text": "第一段。" },
            { "type": "text", "text": "第二段。" }
        ]);
        assert_eq!(join_text_blocks(&content), "第一段。\n第二段。");
    }

    #[test]
    fn extract_advice_some_and_none() {
        let ok = json!({
            "content": [{ "type": "text", "text": "建议" }],
            "stop_reason": "end_turn"
        });
        assert_eq!(
            extract_advice(&ok),
            Some(Advice {
                text: "建议".into(),
                stop_reason: Some("end_turn".into())
            })
        );
        let only_thinking = json!({ "content": [{ "type": "thinking", "thinking": "x" }] });
        assert_eq!(extract_advice(&only_thinking), None);
        // 无 stop_reason 字段（端点省略）→ None
        let no_reason = json!({ "content": [{ "type": "text", "text": "建议" }] });
        assert_eq!(
            extract_advice(&no_reason),
            Some(Advice {
                text: "建议".into(),
                stop_reason: None
            })
        );
    }

    #[test]
    fn annotate_truncation_marks_max_tokens_stop() {
        // 撞 max_tokens 天花板：追加官方截断标记（codex P2），stop_reason 原样保留。
        let advice = Advice {
            text: "Use a channel-based pattern. The tricky part of".into(),
            stop_reason: Some("max_tokens".into()),
        };
        let annotated = annotate_truncation(advice, 2048);
        assert_eq!(
            annotated.text,
            "Use a channel-based pattern. The tricky part of\n\n[Advisor output truncated at max_tokens=2048.]"
        );
        assert_eq!(annotated.stop_reason.as_deref(), Some("max_tokens"));
    }

    #[test]
    fn annotate_truncation_leaves_normal_advice_untouched() {
        for reason in [Some("end_turn"), None] {
            let advice = Advice {
                text: "完整建议".into(),
                stop_reason: reason.map(str::to_string),
            };
            let annotated = annotate_truncation(advice, 2048);
            assert_eq!(annotated.text, "完整建议");
        }
    }

    // ---------- 回注体构造 ----------

    #[test]
    fn continuation_advisor_tool_result_shape() {
        let content =
            json!([{ "type": "tool_use", "id": "toolu_abc", "name": "advisor", "input": {} }]);
        let msgs = build_continuation(
            &content,
            &["toolu_abc".to_string()],
            "用 channel 协调",
            None,
            InjectShape::AdvisorToolResult,
        );
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["content"], content);
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(
            msgs[1]["content"][0],
            json!({
                "type": "advisor_tool_result",
                "tool_use_id": "toolu_abc",
                "content": { "type": "advisor_result", "text": "用 channel 协调" }
            })
        );
    }

    #[test]
    fn continuation_advisor_tool_result_carries_stop_reason() {
        // 官方契约：设了 max_tokens 时 advisor_result 携带 stop_reason（codex P2）。
        let content =
            json!([{ "type": "tool_use", "id": "toolu_abc", "name": "advisor", "input": {} }]);
        let msgs = build_continuation(
            &content,
            &["toolu_abc".to_string()],
            "半句建议\n\n[Advisor output truncated at max_tokens=2048.]",
            Some("max_tokens"),
            InjectShape::AdvisorToolResult,
        );
        assert_eq!(
            msgs[1]["content"][0]["content"],
            json!({
                "type": "advisor_result",
                "text": "半句建议\n\n[Advisor output truncated at max_tokens=2048.]",
                "stop_reason": "max_tokens"
            })
        );
    }

    #[test]
    fn continuation_tool_result_fallback_shape() {
        let content =
            json!([{ "type": "tool_use", "id": "toolu_abc", "name": "advisor", "input": {} }]);
        let msgs = build_continuation(
            &content,
            &["toolu_abc".to_string()],
            "建议明文",
            Some("end_turn"),
            InjectShape::ToolResult,
        );
        // 回退明文形状无 stop_reason 字段；截断标记（如有）已嵌在文本里。
        assert_eq!(
            msgs[1]["content"][0],
            json!({ "type": "tool_result", "tool_use_id": "toolu_abc", "content": "建议明文" })
        );
    }

    #[test]
    fn continuation_multiple_tool_uses_all_linked() {
        // 并行多个 advisor 调用：每个 tool_use 都得有配对结果，否则严格端点 400。
        let content = json!([
            { "type": "tool_use", "id": "toolu_a", "name": "advisor", "input": {} },
            { "type": "tool_use", "id": "toolu_b", "name": "advisor", "input": {} }
        ]);
        let msgs = build_continuation(
            &content,
            &["toolu_a".to_string(), "toolu_b".to_string()],
            "建议",
            None,
            InjectShape::ToolResult,
        );
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(
            msgs[1]["content"],
            json!([
                { "type": "tool_result", "tool_use_id": "toolu_a", "content": "建议" },
                { "type": "tool_result", "tool_use_id": "toolu_b", "content": "建议" }
            ])
        );
    }

    // ---------- 模型解析 ----------

    #[test]
    fn resolve_advisor_model_tier_hit() {
        assert_eq!(
            resolve_advisor_model(Some("claude-fable-5"), "sonnet-mapped", &mapping()),
            "fable-mapped"
        );
        assert_eq!(
            resolve_advisor_model(Some("claude-opus-4-8"), "sonnet-mapped", &mapping()),
            "opus-mapped"
        );
    }

    #[test]
    fn resolve_advisor_model_fable_falls_to_opus_then_executor() {
        let mut m = mapping();
        m.fable_model = None;
        assert_eq!(
            resolve_advisor_model(Some("claude-fable-5"), "sonnet-mapped", &m),
            "opus-mapped"
        );
        m.opus_model = None;
        // fable/opus 都未配 → 回落 executor 已映射模型（非 default）
        assert_eq!(
            resolve_advisor_model(Some("claude-fable-5"), "sonnet-mapped", &m),
            "sonnet-mapped"
        );
    }

    #[test]
    fn resolve_advisor_model_unconfigured_tier_falls_to_executor_not_default() {
        let m = ModelMapping {
            haiku_model: None,
            sonnet_model: None,
            opus_model: None,
            fable_model: None,
            subagent_model: None,
            default_model: Some("default-model".into()),
        };
        // 未配 opus 档 → 回落 executor，而非 default-model
        assert_eq!(
            resolve_advisor_model(Some("claude-opus-4-8"), "executor-mapped", &m),
            "executor-mapped"
        );
    }

    #[test]
    fn resolve_advisor_model_none_and_no_tier_keyword() {
        assert_eq!(
            resolve_advisor_model(None, "executor-mapped", &mapping()),
            "executor-mapped"
        );
        assert_eq!(
            resolve_advisor_model(Some("gpt-5-pro"), "executor-mapped", &mapping()),
            "executor-mapped"
        );
    }

    // ---------- 子调用请求体 ----------

    #[test]
    fn build_subcall_body_shape() {
        let transcript = build_transcript(
            Some(&json!("你是执行者")),
            Some(&json!([{"name": "advisor"}])),
            &json!([{"role": "user", "content": "任务"}]),
            "本轮文本",
        );
        let body = build_subcall_body("fable-mapped", 4096, &transcript);
        assert_eq!(body["model"], "fable-mapped");
        assert_eq!(body["max_tokens"], 4096);
        // 沿用 Oracle 元数据 temperature=0.1，保证建议稳定。
        assert_eq!(body["temperature"], json!(0.1));
        assert_eq!(body["system"], ORACLE_SYSTEM_PROMPT);
        assert_eq!(body["tools"], json!([]));
        assert_eq!(body["messages"][0]["role"], "user");
        let payload = body["messages"][0]["content"].as_str().unwrap();
        assert!(payload.contains("full transcript"));
        assert!(payload.contains("你是执行者"));
        assert!(payload.contains("本轮文本"));
    }

    #[test]
    fn build_transcript_null_defaults() {
        let t = build_transcript(None, None, &json!([]), "");
        assert_eq!(t["executor_system_prompt"], Value::Null);
        assert_eq!(t["executor_tool_definitions"], Value::Null);
        assert_eq!(t["conversation"], json!([]));
    }

    // ---------- 失败安全网 ----------

    #[test]
    fn unavailable_advice_nonempty() {
        assert!(!unavailable_advice().is_empty());
    }

    // ---------- 2xx 错误信封 ----------

    #[test]
    fn error_envelope_detected() {
        // 官方信封形状：type=error + error 对象
        assert!(is_error_envelope(&json!({
            "type": "error",
            "error": { "type": "invalid_request_error", "message": "unknown block advisor_tool_result" }
        })));
        // 简化形状：顶层 error 字段
        assert!(is_error_envelope(&json!({ "error": "boom" })));
    }

    #[test]
    fn error_envelope_not_matched_by_normal_responses() {
        // 正常 message 响应与 advisor 子调用响应都不会误判
        assert!(!is_error_envelope(&json!({
            "type": "message",
            "content": [{ "type": "text", "text": "ok" }],
            "usage": { "input_tokens": 1 }
        })));
        assert!(!is_error_envelope(
            &json!({ "content": [{ "type": "tool_use", "id": "t", "name": "advisor" }] })
        ));
        assert!(!is_error_envelope(&json!({})));
    }

    // ---------- max_uses 上限执行（codex P1） ----------

    #[test]
    fn can_use_advisor_respects_client_cap_and_unlimited() {
        // 未设上限：恒可用（cc-switch 不自造上限，US-34）
        assert!(can_use_advisor(0, None));
        assert!(can_use_advisor(999, None));
        // 设了 max_uses=1：首次可用、之后不可用
        assert!(can_use_advisor(0, Some(1)));
        assert!(!can_use_advisor(1, Some(1)));
        assert!(!can_use_advisor(2, Some(1)));
    }

    #[test]
    fn limit_reached_advice_mentions_cap() {
        let text = limit_reached_advice(3);
        assert!(text.contains("max_uses=3"));
        assert!(text.contains("无建议"));
    }

    // ---------- 强制 tool_choice 剥离（codex P1） ----------

    #[test]
    fn strip_forced_tool_choice_removes_tool_and_any() {
        // 指名 advisor 的强制选择：每轮续跑会被迫再调 advisor → 付费循环，须剥除
        let mut body = json!({
            "model": "m",
            "tool_choice": { "type": "tool", "name": "advisor" },
            "messages": []
        });
        assert!(strip_forced_tool_choice(&mut body));
        assert!(body.get("tool_choice").is_none());

        // any（advisor 为唯一工具时同样构成强制）
        let mut body = json!({ "tool_choice": { "type": "any" } });
        assert!(strip_forced_tool_choice(&mut body));
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn strip_forced_tool_choice_keeps_auto_and_absent() {
        let mut body = json!({ "tool_choice": { "type": "auto" } });
        assert!(!strip_forced_tool_choice(&mut body));
        assert_eq!(body["tool_choice"]["type"], "auto");

        let mut body = json!({ "model": "m" });
        assert!(!strip_forced_tool_choice(&mut body));
    }
}
