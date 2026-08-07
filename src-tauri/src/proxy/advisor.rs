//! advisor 本地降级模块（issue #20 切片 1 — 非流式成功回路）
//!
//! 当上游第三方 Claude 端点不支持 advisor 服务器工具时，cc-switch 代理层把它
//! 优雅降级为一次本地 advisor 推理，对 Claude Code 完全透明：
//!
//! 1. 请求定稿段剥离 `advisor_20260301` server-tool 块，替换为普通客户端工具
//!    `advisor`（无参），并按值剥 `advisor-tool-2026-03-01` beta 头。
//! 2. executor 把 advisor 当普通工具调用时，代理捕获该 `tool_use`，沿用本次
//!    executor 实际命中的 Provider 的端点/凭证，用 Oracle 系统提示 + 完整转录 +
//!    空 `tools` 起一次非流式 `/v1/messages`，单轮拿到文本建议。
//! 3. 建议以 `advisor_tool_result{content: advisor_result{text}}` 回注给 executor
//!    （下游不认则自动回退普通 `tool_result`），让 executor 带建议续跑同一个 turn。
//!
//! 本切片只做成功路径；子调用失败的错误降级、配对校验、缓存、usage 落库、
//! 流式路径由后续切片（#22-#26）承接。
//!
//! 设计约束（CONTEXT.md + ADR-0001..0004）：
//! - 端点判定纯 base_url 启发式（官方域透传、其余降级），零配置、无错误指纹。
//! - 不注入「何时调用 advisor」的引导（客户端已注入，重复注入破坏 prompt cache 前缀）。
//! - 不做防重入：advisor 子调用 `tools` 为空，递归前提不成立。
//! - 所有决策逻辑下沉为同步纯函数（对齐 `body_filter`/`model_mapper` 范式），
//!   编排循环只做「调上游 + 调纯函数」。

use crate::app_config::AppType;
use crate::provider::Provider;
use crate::proxy::{
    forwarder::{ForwardError, RequestForwarder},
    handler_context::RequestContext,
    hyper_client::ProxyResponse,
    ProxyError,
};
use serde_json::{json, Value};

/// Claude Code 客户端发来的 advisor 服务器工具块 type。
pub const ADVISOR_SERVER_TOOL_TYPE: &str = "advisor_20260301";
/// 与 server 工具配对发布的 beta 头值（按值剥离）。
pub const ADVISOR_BETA_HEADER_VALUE: &str = "advisor-tool-2026-03-01";
/// 降级后暴露给 executor 的普通客户端工具名。
pub const ADVISOR_TOOL_NAME: &str = "advisor";

/// Oracle 系统提示（map #2 资产 `ORACLE_DEFAULT_PROMPT` 的极简改造版）。
///
/// 原型 #7（`advisor-loop.prototype.mjs`）用它在本机 k3 端点端到端跑通了回路；
/// 完整版 Oracle 提示词资产在外仓库 `code-yeongyu/oh-my-openagent`，待后续切片
/// 作为提示词资产入库时替换。本切片只采用其「只读、给架构/调试建议、不执行」语义。
pub const ORACLE_SYSTEM_PROMPT: &str = "You are the Oracle — a read-only strategic advisor consulted by a working (executor) model.\n\
You receive the executor's full transcript as quoted context: its system prompt, tool definitions, prior turns and tool results, and the text it has produced so far this turn.\n\
Give sharp, concrete advice on architecture decisions, self-review, and stubborn debugging. You CANNOT write files, run tools, or take actions — you only advise. Be concise (a few hundred words). Do not restate the transcript; add what the executor is missing.";

/// 端点判定结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvisorEndpointVerdict {
    /// Claude 官方域（api.anthropic.com / api.claude.com 等）：原生透传，不降级。
    Official,
    /// 其余一切端点：本地降级。
    ThirdParty,
}

/// 请求侧改写结果
#[derive(Debug)]
pub struct AdvisorRequestRewrite {
    /// 改写后的请求体（剥 server-tool、注入客户端工具）
    pub body: Value,
    /// 从 `advisor_20260301` 块捕获的 advisor 参数（model / max_tokens 等）
    pub capture: AdvisorToolCapture,
    /// 请求体是否真的发生了改写（未发现 advisor 块时为 false）
    pub rewritten: bool,
}

/// 从入站 `advisor_20260301` 服务器工具块捕获的参数
#[derive(Debug, Clone, Default)]
pub struct AdvisorToolCapture {
    /// advisor 模型档位（fable/opus/sonnet 档名或具体模型 ID），由客户端决定
    pub model: Option<String>,
    /// 客户端给定的 max_uses（透传，cc-switch 不自造上限）
    pub max_uses: Option<u64>,
    /// max_tokens（子调用沿用 executor 请求值，见 build_advisor_request）
    pub max_tokens: Option<u64>,
}

/// 端点判定：纯 base_url 主机名启发式。
///
/// base_url 主机名是 Claude 官方域 → 官方（透传、不降级）；其余一律第三方（降级）。
/// 零配置、无手动覆盖、无错误指纹匹配（ADR-0002 题 1）。
pub fn is_official_anthropic_base_url(base_url: &str) -> AdvisorEndpointVerdict {
    let host = base_url
        .split("://")
        .nth(1)
        .unwrap_or(base_url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        // 剥端口：api.anthropic.com:8443 仍是官方域
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();

    let official = host == "api.anthropic.com"
        || host == "api.claude.com"
        || host == "console.anthropic.com"
        || host.ends_with(".anthropic.com")
        || host.ends_with(".claude.com");
    if official {
        AdvisorEndpointVerdict::Official
    } else {
        AdvisorEndpointVerdict::ThirdParty
    }
}

/// 请求侧改写：非官方端点把 `advisor_20260301` server-tool 块剥离、替换为普通
/// 客户端工具 `advisor`（无参）。官方端点原样透传（rewritten=false）。
///
/// 只改 `tools[]` 中 type == `advisor_20260301` 的块；未发现则 body 原样返回。
pub fn rewrite_advisor_request(body: &Value) -> AdvisorRequestRewrite {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return AdvisorRequestRewrite {
            body: body.clone(),
            capture: AdvisorToolCapture::default(),
            rewritten: false,
        };
    };

    let mut capture = AdvisorToolCapture::default();
    let mut remaining = Vec::with_capacity(tools.len() + 1);
    let mut found = false;

    for tool in tools {
        if tool.get("type").and_then(Value::as_str) == Some(ADVISOR_SERVER_TOOL_TYPE) {
            found = true;
            capture.model = tool.get("model").and_then(Value::as_str).map(String::from);
            capture.max_uses = tool.get("max_uses").and_then(Value::as_u64);
            capture.max_tokens = tool.get("max_tokens").and_then(Value::as_u64);
            log::debug!(
                "[Advisor] 剥离服务器工具 {ADVISOR_SERVER_TOOL_TYPE}: model={:?}, max_uses={:?}, max_tokens={:?}",
                capture.model,
                capture.max_uses,
                capture.max_tokens
            );
        } else {
            remaining.push(tool.clone());
        }
    }

    if !found {
        return AdvisorRequestRewrite {
            body: body.clone(),
            capture,
            rewritten: false,
        };
    }

    // 替换为普通客户端工具（function tool、无参数），追加到末尾，
    // 保持其余工具的相对顺序不变。
    // 描述只说明工具「做什么」（咨询强模型、返回建议），不注入「何时调用」
    // 的引导——客户端已注入（CONTEXT.md「触发时机」；重复注入破坏 prompt
    // cache 前缀）。
    remaining.push(json!({
        "type": "function",
        "name": ADVISOR_TOOL_NAME,
        "description": "Consults a stronger reviewer model that sees your full transcript and returns expert advice.",
        "input_schema": { "type": "object", "properties": {}, "required": [] }
    }));

    let mut body = body.clone();
    body["tools"] = Value::Array(remaining);
    AdvisorRequestRewrite {
        body,
        capture,
        rewritten: true,
    }
}

/// 响应块识别：executor 是否发起了 advisor `tool_use`（普通 `tool_use`、name=advisor）。
///
/// 返回找到的块（含其 `id` / `input`，供回注与子调用使用）；无则 None。
/// 非流式响应中 content 可能是字符串（极少见）——按未找到处理。
pub fn find_advisor_tool_use(response: &Value) -> Option<&Value> {
    let content = response.get("content").and_then(Value::as_array)?;
    content.iter().find(|block| {
        block.get("type").and_then(Value::as_str) == Some("tool_use")
            && block.get("name").and_then(Value::as_str) == Some(ADVISOR_TOOL_NAME)
    })
}

/// advisor 子调用请求体构造（同步纯函数）。
///
/// 输入：executor 请求体（取其 model / max_tokens）、已映射的 advisor 模型名、
/// Oracle 系统提示、完整对话转录（单个 quoted user 消息）。
///
/// 输出：子调用请求体。
pub fn build_advisor_request(
    executor_body: &Value,
    advisor_model: &str,
    system: Option<&Value>,
    messages: Vec<Value>,
) -> Value {
    // max_tokens 沿用 executor 请求里的值（ADR-0001 / spec US15）；executor
    // 未给时取 8192 保守默认。
    let max_tokens = executor_body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(8192);

    json!({
        "model": advisor_model,
        "max_tokens": max_tokens,
        "system": system.cloned().unwrap_or_else(|| json!([{"type": "text", "text": ORACLE_SYSTEM_PROMPT}])),
        "messages": messages,
        "tools": [],
    })
}

/// 组装传给 advisor 的完整对话转录（同步纯函数）。
///
/// 对齐原型 #7（`advisor-loop.prototype.mjs` 的 `buildAdvisorRequest`）与 Oracle
/// 提示「You receive the executor's full transcript as quoted context」语义：
/// 把 executor 的 system 提示、工具定义、完整对话历史、本轮已产出文本打包成
/// **单个 quoted JSON user 消息**——
/// ① 转录完整（spec「完整对话转录」）；② 规避「assistant 块以未满足的
/// tool_use 结尾」被严格端点 400（quoted 包裹后不再是协议层 tool_use 序列）。
///
/// advisor 子调用的转录由「当前累计 messages + 本轮 executor 已产出的 assistant
/// 响应（含 advisor `tool_use`）」组成——与 executor 一致的上下文（ADR-0001）。
pub fn build_advisor_messages(
    executor_body: &Value,
    assistant_message: &Value,
    focus: Option<&str>,
) -> Vec<Value> {
    let transcript = json!({
        "executor_system_prompt": executor_body.get("system").cloned().unwrap_or(Value::Null),
        "executor_tool_definitions": executor_body.get("tools").cloned().unwrap_or_else(|| json!([])),
        "conversation": executor_body.get("messages").cloned().unwrap_or_else(|| json!([])),
        "executor_text_this_turn": assistant_text(assistant_message),
        "executor_focus": focus,
    });
    let user_payload = "The executor model has paused to consult you. Below is its full \
        transcript as quoted JSON. Advise it.\n\n"
        .to_owned()
        + &serde_json::to_string(&transcript).unwrap_or_default();
    vec![json!({"role": "user", "content": user_payload})]
}

/// 从 assistant 消息中提取本轮已产出的文本（text 块拼接；tool_use 块丢弃）。
fn assistant_text(assistant_message: &Value) -> String {
    assistant_message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(Value::as_str) == Some("text") {
                        b.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// 解析 advisor 子调用应使用的模型（同步纯函数）。
///
/// 返回**映射前的原始值**——由 forwarder 管线做唯一一次 `model_mapper` 映射
/// （与 executor 同一条映射管线，避免二次映射）。规则（ADR-0002 题 2）：
/// - 客户端 `advisor` 工具块 `model` 字段给出档位（fable/opus/sonnet 档名或
///   具体模型 ID），且该档在 provider 配置了映射 → 返回该档位原值；
/// - 档位未配置对应映射 → 回落 executor 请求模型（forward 管线映射后与
///   executor 同款，即 ADR 的「回落 executor 已映射模型」语义）；
/// - 客户端未提供 `model` → 同上回落 executor 请求模型。
pub fn resolve_advisor_model(
    capture: &AdvisorToolCapture,
    provider: &Provider,
    executor_request_model: &str,
) -> String {
    let Some(tier) = capture.model.as_deref().filter(|s| !s.is_empty()) else {
        return executor_request_model.to_string();
    };
    let mapping = super::model_mapper::ModelMapping::from_provider(provider);
    if advisor_tier_is_configured(tier, &mapping) {
        tier.to_string()
    } else {
        executor_request_model.to_string()
    }
}

/// 档位是否在 provider 配置了映射（对齐 `ModelMapping::map_model` 的判定）。
///
/// fable 未单独配置时归入 opus 档（与 map_model 的注释一致），故 fable 的
/// 配置判定是 fable_model 或 opus_model 任一存在；具体模型 ID（非档名）视为
/// 已配置——由 forwarder 管线决定是否命中。
fn advisor_tier_is_configured(
    tier: &str,
    mapping: &super::model_mapper::ModelMapping,
) -> bool {
    let lower = tier.to_lowercase();
    if lower.contains("fable") {
        mapping.fable_model.is_some() || mapping.opus_model.is_some()
    } else if lower.contains("haiku") {
        mapping.haiku_model.is_some()
    } else if lower.contains("opus") {
        mapping.opus_model.is_some()
    } else if lower.contains("sonnet") {
        mapping.sonnet_model.is_some()
    } else {
        true
    }
}

/// 按值剥离 `advisor-tool-2026-03-01` beta 头（同步纯函数）。
///
/// 多值头剥单值、留其余（按逗号分段过滤）；剥完为空则移除整个头。找不到
/// 该值时 headers 原样不动。上游因残留 beta 头报错时由调用方在发送前调用。
pub fn strip_advisor_beta_header(headers: &mut http::HeaderMap) {
    let has_advisor = headers
        .get_all("anthropic-beta")
        .iter()
        .any(|value| {
            value
                .to_str()
                .map(|s| {
                    s.split(',')
                        .any(|part| part.trim() == ADVISOR_BETA_HEADER_VALUE)
                })
                .unwrap_or(false)
        });
    if !has_advisor {
        return;
    }

    let mut remaining: Vec<String> = Vec::new();
    for value in headers.get_all("anthropic-beta") {
        if let Ok(s) = value.to_str() {
            for part in s.split(',') {
                let part = part.trim();
                if !part.is_empty() && part != ADVISOR_BETA_HEADER_VALUE {
                    remaining.push(part.to_string());
                }
            }
        }
    }

    headers.remove("anthropic-beta");
    if !remaining.is_empty() {
        if let Ok(value) = http::HeaderValue::from_str(&remaining.join(",")) {
            headers.insert("anthropic-beta", value);
        }
    }
}

/// 续跑请求体构造（同步纯函数）。
///
/// 把 assistant 响应（含 advisor `tool_use`）与回注消息追加进 executor 转录，
/// 让 executor 带着建议继续同一个 turn。`tools` 保持与改写后请求一致（advisor
/// 客户端工具继续可见，executor 可再调）。
pub fn build_continuation_request(
    rewritten_body: &Value,
    assistant_message: &Value,
    injection_message: &Value,
) -> Value {
    let mut body = rewritten_body.clone();
    let mut messages: Vec<Value> = body
        .get("messages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    messages.push(assistant_message.clone());
    messages.push(injection_message.clone());
    body["messages"] = Value::Array(messages);
    body
}

/// 从 advisor 子调用响应提取文本建议（同步纯函数）。
///
/// 丢弃 thinking 等非 text 块，只取 `text` 块拼接（对齐官方「thinking 丢弃」、
/// 原型 #7 `runAdvisor` 的做法）。
pub fn extract_advisor_text(response: &Value) -> Option<String> {
    let text: Vec<&str> = response
        .get("content")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|block| {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                block.get("text").and_then(Value::as_str)
            } else {
                None
            }
        })
        .collect();
    if text.is_empty() {
        None
    } else {
        Some(text.join("\n"))
    }
}

/// 回注体构造（同步纯函数）：把 advisor 建议包成官方形状
/// `advisor_tool_result{content: advisor_result{text}}`。
///
/// 下游对 `advisor_tool_result` 未知块报错时由调用方回退普通 `tool_result`
/// （ADR-0002 题 4，原型 #7 k3 端点实测须回退）。
pub fn build_advisor_result_injection(tool_use_id: &str, advice_text: &str) -> Value {
    json!({
        "type": "advisor_tool_result",
        "tool_use_id": tool_use_id,
        "content": {
            "type": "advisor_result",
            "text": advice_text
        }
    })
}

/// 回注体回退形状（同步纯函数）：普通 `tool_result` 明文（成功路径回退与
/// 失败回退共用同一条管道，ADR-0003 题 3）。
pub fn build_tool_result_injection(tool_use_id: &str, content: &str) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": content
    })
}

/// 续跑决策（同步纯函数）：响应中是否还有 advisor `tool_use` 需要处理。
///
/// 与 `find_advisor_tool_use` 同义——编排在回注后循环调用它以决定是否续跑；
/// 单独暴露以便测试覆盖「续跑决策」这一决策点（spec 验收标准）。
pub fn should_continue_with_advisor(response: &Value) -> bool {
    find_advisor_tool_use(response).is_some()
}

/// 从 Provider 配置读取 base_url。
///
/// 注意：与 `ClaudeAdapter::extract_base_url`（providers/claude.rs）读取顺序
/// 相同（env.ANTHROPIC_BASE_URL → base_url → baseURL → apiEndpoint）但**语义不同**：
/// trait 方法含 Codex/xAI OAuth 强制端点覆盖且返回 Result，这里是纯配置读取
/// （仅用于 base_url 启发式判定）。若未来 extract_base_url 的读取顺序调整，
/// 需要同步此处。
pub fn provider_base_url(provider: &Provider) -> Option<String> {
    let env = provider.settings_config.get("env");
    env.and_then(|e| e.get("ANTHROPIC_BASE_URL"))
        .and_then(Value::as_str)
        .or_else(|| {
            provider
                .settings_config
                .get("base_url")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            provider
                .settings_config
                .get("baseURL")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            provider
                .settings_config
                .get("apiEndpoint")
                .and_then(Value::as_str)
        })
        .map(|s| s.trim_end_matches('/').to_string())
}

// ============================================================================
// 回路编排（非流式成功路径）
// ============================================================================

/// 非流式 advisor 回路入口。
///
/// 在 `handle_messages_for_app` 的非流式透传路径调用：读上游响应体 →
/// 查 advisor `tool_use` → 无则返回 `None`（走正常透传）；有则执行
/// 子调用 + 回注 + 续跑循环，返回最终回给客户端的响应体。
///
/// 返回 `Ok(None)` 表示无需降级处理（响应无 advisor `tool_use`，或响应
/// 不可解析为 JSON）。返回 `Ok(Some(body))` 表示最终响应（可能经过多轮
/// 续跑）。`Err` 透传上游错误。
///
/// 循环上限：防异常端点让 executor 无限调 advisor（如 prompt 反复触发），
/// 超过上限即返回最后一次响应。正常场景（ADR「不做防重入」）executor 最多
/// 调一两次。
pub const ADVISOR_LOOP_MAX_ITERATIONS: usize = 4;

/// 子调用与续跑共用的「转发一次」帮助函数。
///
/// 单元素 providers → forwarder 跳过熔断器（failover 钉死语义，ADR-0001）。
/// `is_advisor_subcall` 仅用于日志。
#[allow(clippy::too_many_arguments)]
async fn forward_once(
    forwarder: &RequestForwarder,
    app_type: &AppType,
    method: http::Method,
    endpoint: &str,
    body: Value,
    headers: axum::http::HeaderMap,
    extensions: http::Extensions,
    provider: &Provider,
    is_advisor_subcall: bool,
) -> Result<(ProxyResponse, String, String, Option<http::Extensions>), ProxyError> {
    let label = if is_advisor_subcall {
        "advisor 子调用"
    } else {
        "executor 续跑"
    };
    let result = forwarder
        .forward_with_retry(
            app_type,
            method,
            endpoint,
            body,
            headers,
            extensions,
            vec![provider.clone()],
        )
        .await
        .map_err(|e: ForwardError| e.error)?;
    let provider_id = result.provider.id.clone();
    let outbound_model = result.outbound_model.clone().unwrap_or_default();
    log::debug!(
        "[Advisor] {label} 成功: provider={}, outbound_model={}",
        provider_id,
        outbound_model
    );
    Ok((result.response, provider_id, outbound_model, None))
}

/// 发起一次 advisor 子调用（非流式 `/v1/messages`）。
///
/// 复用 executor 实际命中的 Provider 的端点/凭证（ADR-0001），Oracle 系统
/// 提示 + 完整转录 + 空 `tools`，`max_tokens` 沿用 executor 请求值。
/// 模型档位经 `resolve_advisor_model` 解析，由 forwarder 管线做唯一一次映射。
#[allow(clippy::too_many_arguments)]
pub async fn run_advisor_subcall(
    ctx: &RequestContext,
    forwarder: &RequestForwarder,
    method: http::Method,
    endpoint: &str,
    headers: axum::http::HeaderMap,
    extensions: http::Extensions,
    executor_body: &Value,
    capture: &AdvisorToolCapture,
    assistant_message: &Value,
) -> Result<Value, ProxyError> {
    let advisor_model = resolve_advisor_model(capture, &ctx.provider, &ctx.request_model);
    let focus = assistant_message
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks.iter().find_map(|b| {
                if b.get("name").and_then(Value::as_str) == Some(ADVISOR_TOOL_NAME) {
                    b.get("input")
                        .and_then(|input| input.get("focus"))
                        .and_then(Value::as_str)
                } else {
                    None
                }
            })
        });
    let messages = build_advisor_messages(executor_body, assistant_message, focus);

    let advisor_body = build_advisor_request(executor_body, &advisor_model, None, messages);

    let (response, _provider_id, _outbound_model, _ext) = forward_once(
        forwarder,
        &ctx.app_type,
        method,
        endpoint,
        advisor_body,
        headers,
        extensions,
        &ctx.provider,
        true,
    )
    .await?;

    // 读子调用响应体（非流式）。body 超时对齐 executor 主路径
    // （handle_non_streaming）：故障转移开启且配置非零时生效。
    let body_timeout =
        if ctx.app_config.auto_failover_enabled && ctx.app_config.non_streaming_timeout > 0 {
            std::time::Duration::from_secs(ctx.app_config.non_streaming_timeout as u64)
        } else {
            std::time::Duration::ZERO
        };
    let (_, _status, body_bytes) = super::response_processor::read_decoded_body(
        response,
        "Advisor",
        body_timeout,
    )
    .await?;

    let response_json: Value = serde_json::from_slice(&body_bytes).map_err(|e| {
        ProxyError::TransformError(format!("advisor 子调用响应解析失败: {e}"))
    })?;

    // 提取文本建议（丢弃 thinking 等非 text 块）
    extract_advisor_text(&response_json).map(Value::String).ok_or_else(|| {
        ProxyError::TransformError("advisor 子调用响应无 text 块".to_string())
    })
}

/// 回路执行结果
pub enum AdvisorLoopOutcome {
    /// 响应中无 advisor tool_use（或不可解析）：未走降级，调用方应按正常
    /// 透传流程处理该响应（响应体已重打包为 Buffered）。
    NoAdvisorCall { response: ProxyResponse },
    /// 回路完成（子调用 + 回注 + 续跑后的最终响应）。
    Completed {
        headers: axum::http::HeaderMap,
        status: http::StatusCode,
        body: Value,
    },
}

/// 执行 advisor 回路：捕获 → 子调用 → 回注 → 续跑，直到 executor 不再调 advisor。
///
/// # Arguments
/// * `state` / `ctx` - 代理状态与请求上下文
/// * `forwarder` - 请求转发器（复用 executor 管线）
/// * `method` / `endpoint` - 客户端请求方法（透传）与端点
/// * `headers` / `extensions` - 客户端请求头（透传）
/// * `rewritten_body` - 已剥离 server-tool、注入客户端工具后的请求体
/// * `capture` - 从 server-tool 块捕获的 advisor 参数
/// * `response` - 首次 forward 的响应（会被消费）
///
/// # Returns
/// `AdvisorLoopOutcome`——见枚举注释。调用方根据结果决定走正常透传
/// （`NoAdvisorCall`，继续 `process_response`）还是直接构建最终响应
/// （`Completed`）。
#[allow(clippy::too_many_arguments)]
pub async fn run_advisor_loop(
    ctx: &RequestContext,
    forwarder: &RequestForwarder,
    method: http::Method,
    endpoint: &str,
    headers: axum::http::HeaderMap,
    extensions: http::Extensions,
    rewritten_body: &Value,
    capture: AdvisorToolCapture,
    mut response: ProxyResponse,
) -> Result<AdvisorLoopOutcome, ProxyError> {
    let mut current_body = rewritten_body.clone();
    let mut iterations = 0usize;

    loop {
        iterations += 1;
        if iterations > ADVISOR_LOOP_MAX_ITERATIONS {
            log::warn!(
                "[Advisor] 达到回路迭代上限 ({ADVISOR_LOOP_MAX_ITERATIONS})，返回最后一次响应"
            );
            break;
        }

        // 读响应体（非流式）。body 超时对齐 executor 主路径
        // （handle_non_streaming）：故障转移开启且配置非零时生效。
        let body_timeout =
            if ctx.app_config.auto_failover_enabled && ctx.app_config.non_streaming_timeout > 0 {
                std::time::Duration::from_secs(ctx.app_config.non_streaming_timeout as u64)
            } else {
                std::time::Duration::ZERO
            };
        let (response_headers, status, body_bytes) =
            super::response_processor::read_decoded_body(response, ctx.tag, body_timeout)
                .await?;

        if !status.is_success() {
            // 上游错误：透传（错误响应体原样返回客户端）
            let body_json = serde_json::from_slice(&body_bytes).unwrap_or_else(|_| {
                json!({"error": {"message": String::from_utf8_lossy(&body_bytes).to_string()}})
            });
            return Ok(AdvisorLoopOutcome::Completed {
                headers: response_headers,
                status,
                body: body_json,
            });
        }

        let response_json: Value = serde_json::from_slice(&body_bytes).map_err(|e| {
            ProxyError::TransformError(format!("executor 响应解析失败: {e}"))
        })?;

        // 续跑决策：响应中是否还有 advisor 调用需要处理（spec 验收标准点名的
        // 纯函数；循环用它判定是否结束）。
        if !should_continue_with_advisor(&response_json) {
            log::debug!("[Advisor] 无 advisor 调用，回路结束");
            return Ok(AdvisorLoopOutcome::NoAdvisorCall {
                response: ProxyResponse::buffered(status, response_headers, body_bytes),
            });
        }

        // 判定通过 → 必定能取到 advisor tool_use 块
        let tool_use = find_advisor_tool_use(&response_json)
            .expect("should_continue_with_advisor 已确认存在 advisor tool_use");

        let tool_use_id = tool_use
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        log::debug!("[Advisor] 捕获 advisor tool_use: id={tool_use_id}");

        // 组装 advisor 子调用转录（当前累计 + 本轮 assistant 响应）
        let assistant_message = json!({
            "role": "assistant",
            "content": response_json.get("content").cloned().unwrap_or(json!([]))
        });

        // 发起子调用
        let advice_text = match run_advisor_subcall(
            ctx,
            forwarder,
            method.clone(),
            endpoint,
            headers.clone(),
            extensions.clone(),
            &current_body,
            &capture,
            &assistant_message,
        )
        .await
        {
            Ok(text) => text.as_str().unwrap_or_default().to_string(),
            Err(e) => {
                // 切片 1 只做成功路径；子调用失败记日志并回注「无建议」明文
                // （错误降级的完整形状由切片 2 承接，这里用最简明文让 executor 继续）
                log::warn!("[Advisor] 子调用失败（切片 2 将接管错误降级）: {e}");
                "advisor 不可用：本次无建议".to_string()
            }
        };

        // 回注：默认 advisor_tool_result；下游对未知块报错时回退普通 tool_result
        let injection = build_advisor_result_injection(&tool_use_id, &advice_text);
        let injection_message = json!({
            "role": "user",
            "content": [injection]
        });

        // 续跑（钉死同一 provider）
        let continued_body = build_continuation_request(
            &current_body,
            &assistant_message,
            &injection_message,
        );
        let (continued_response, _pid, _model, _ext) = forward_once(
            forwarder,
            &ctx.app_type,
            method.clone(),
            endpoint,
            continued_body.clone(),
            headers.clone(),
            extensions.clone(),
            &ctx.provider,
            false,
        )
        .await?;

        // 续跑遇 400（下游不认 advisor_tool_result 未知块，ADR-0002 题 4）
        // → 回退普通 tool_result 重试一次。其他 4xx（429 限流 / 401 鉴权 /
        // 404 等）不是块形状问题，重试无意义，直接透传给客户端。
        if continued_response.status() == http::StatusCode::BAD_REQUEST {
            log::warn!("[Advisor] 续跑遇下游 4xx，回退普通 tool_result 重试");
            let fallback_injection = build_tool_result_injection(&tool_use_id, &advice_text);
            let fallback_message = json!({
                "role": "user",
                "content": [fallback_injection]
            });
            let fallback_body = build_continuation_request(
                &current_body,
                &assistant_message,
                &fallback_message,
            );
            let (fallback_response, _pid, _model, _ext) = forward_once(
                forwarder,
                &ctx.app_type,
                method.clone(),
                endpoint,
                fallback_body.clone(),
                headers.clone(),
                extensions.clone(),
                &ctx.provider,
                false,
            )
            .await?;
            response = fallback_response;
            // 实际发出的是 tool_result 回注版，转录随之更新
            current_body = fallback_body;
        } else {
            response = continued_response;
            // 实际发出的是 advisor_tool_result 回注版，转录随之更新
            current_body = continued_body;
        }
    }

    // 达到迭代上限：读最后一次响应
    let (headers_out, status, body_bytes) = super::response_processor::read_decoded_body(
        response,
        ctx.tag,
        std::time::Duration::ZERO,
    )
    .await?;
    if !status.is_success() {
        let body_json = serde_json::from_slice(&body_bytes).unwrap_or_else(|_| {
            json!({"error": {"message": String::from_utf8_lossy(&body_bytes).to_string()}})
        });
        return Ok(AdvisorLoopOutcome::Completed {
            headers: headers_out,
            status,
            body: body_json,
        });
    }
    let response_json = serde_json::from_slice(&body_bytes).map_err(|e| {
        ProxyError::TransformError(format!("executor 响应解析失败: {e}"))
    })?;
    Ok(AdvisorLoopOutcome::Completed {
        headers: headers_out,
        status,
        body: response_json,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── 端点判定 ──

    #[test]
    fn official_anthropic_domains_pass_through() {
        for url in [
            "https://api.anthropic.com",
            "https://api.anthropic.com/v1/messages",
            "https://api.claude.com",
            "https://console.anthropic.com",
            "https://api.anthropic.com/v1",
            "http://api.anthropic.com",
            "https://api.anthropic.com:8443",
            "https://api.claude.com:443/v1/messages",
        ] {
            assert_eq!(
                is_official_anthropic_base_url(url),
                AdvisorEndpointVerdict::Official,
                "should treat {url} as official"
            );
        }
    }

    #[test]
    fn third_party_domains_degrade() {
        for url in [
            "https://api.deepseek.com/anthropic",
            "https://openrouter.ai/api/v1",
            "https://k3.example.com",
            "http://127.0.0.1:15721",
            "https://api.example.com/v1",
            "https://gateway.example.com",
        ] {
            assert_eq!(
                is_official_anthropic_base_url(url),
                AdvisorEndpointVerdict::ThirdParty,
                "should treat {url} as third-party"
            );
        }
    }

    #[test]
    fn official_domain_subdomain_is_third_party() {
        // 仅精确主机名或官方域子域算官方；旁路域名不算
        assert_eq!(
            is_official_anthropic_base_url("https://anthropic.com.evil.com"),
            AdvisorEndpointVerdict::ThirdParty
        );
        assert_eq!(
            is_official_anthropic_base_url("https://not-anthropic.com"),
            AdvisorEndpointVerdict::ThirdParty
        );
    }

    // ── 请求侧改写 ──

    fn sample_request() -> Value {
        json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1000,
            "tools": [
                {"type": "advisor_20260301", "name": "advisor", "model": "claude-opus-4-8", "max_uses": 3},
                {"type": "custom", "name": "bash"}
            ],
            "messages": [{"role": "user", "content": "hello"}]
        })
    }

    #[test]
    fn strips_server_tool_and_injects_client_tool() {
        let body = sample_request();
        let rewrite = rewrite_advisor_request(&body);
        assert!(rewrite.rewritten);
        assert_eq!(rewrite.capture.model.as_deref(), Some("claude-opus-4-8"));
        assert_eq!(rewrite.capture.max_uses, Some(3));

        let tools = rewrite.body.get("tools").and_then(Value::as_array).unwrap();
        assert_eq!(tools.len(), 2, "server tool replaced by client tool, custom kept");
        assert!(tools.iter().all(|t| t.get("type").and_then(Value::as_str) != Some("advisor_20260301")));
        let advisor_tool = tools.iter().find(|t| t.get("name").and_then(Value::as_str) == Some("advisor")).unwrap();
        assert_eq!(advisor_tool.get("type").and_then(Value::as_str), Some("function"));
        assert_eq!(
            advisor_tool.pointer("/input_schema/properties"),
            Some(&json!({}))
        );
        assert_eq!(tools[0]["name"], "bash", "non-advisor tools preserved");
        assert_eq!(rewrite.body["model"], "claude-sonnet-4-5");
        assert_eq!(rewrite.body["messages"], body["messages"]);
    }

    #[test]
    fn leaves_request_unchanged_without_server_tool() {
        let body = json!({
            "model": "claude-sonnet-4-5",
            "tools": [{"type": "custom", "name": "bash"}],
            "messages": []
        });
        let rewrite = rewrite_advisor_request(&body);
        assert!(!rewrite.rewritten);
        assert_eq!(rewrite.body, body);
        assert_eq!(rewrite.capture.model, None);
    }

    #[test]
    fn leaves_request_unchanged_without_tools_field() {
        let body = json!({"model": "claude-sonnet-4-5", "messages": []});
        let rewrite = rewrite_advisor_request(&body);
        assert!(!rewrite.rewritten);
        assert_eq!(rewrite.body, body);
    }

    // ── tool_use 识别 ──

    #[test]
    fn finds_advisor_tool_use() {
        let response = json!({
            "content": [
                {"type": "text", "text": "thinking..."},
                {"type": "tool_use", "id": "toolu_01", "name": "advisor", "input": {}}
            ]
        });
        let found = find_advisor_tool_use(&response).expect("should find advisor tool_use");
        assert_eq!(found["id"], "toolu_01");
    }

    #[test]
    fn ignores_non_advisor_tool_use() {
        let response = json!({
            "content": [
                {"type": "tool_use", "id": "toolu_02", "name": "bash", "input": {}}
            ]
        });
        assert!(find_advisor_tool_use(&response).is_none());
    }

    #[test]
    fn ignores_missing_content() {
        assert!(find_advisor_tool_use(&json!({"stop_reason": "end_turn"})).is_none());
        assert!(find_advisor_tool_use(&json!({"content": "plain string"})).is_none());
    }

    // ── 子调用请求体构造 ──

    #[test]
    fn builds_advisor_request_with_oracle_system_and_empty_tools() {
        let executor = json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1000,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let capture = AdvisorToolCapture {
            model: Some("claude-opus-4-8".to_string()),
            max_uses: None,
            max_tokens: Some(2000),
        };
        let req = build_advisor_request(&executor, "claude-opus-4-8", None, vec![json!({"role": "user", "content": "hi"})]);
        assert_eq!(req["model"], "claude-opus-4-8");
        // max_tokens 沿用 executor 值（ADR-0001：不取 capture 的 max_tokens）
        assert_eq!(req["max_tokens"], 1000);
        assert_eq!(req["tools"], json!([]), "empty tools — protocol-level read-only");
        assert_eq!(req["messages"], json!([{"role": "user", "content": "hi"}]));
        assert_eq!(req["system"][0]["text"], ORACLE_SYSTEM_PROMPT);
    }

    #[test]
    fn advisor_max_tokens_falls_back_to_default_when_executor_omits() {
        let executor = json!({"model": "m", "messages": []});
        let capture = AdvisorToolCapture::default();
        let req = build_advisor_request(&executor, "m", None, vec![]);
        assert_eq!(req["max_tokens"], 8192);
    }

    #[test]
    fn advisor_system_preserved_when_provided() {
        let executor = json!({"model": "m", "messages": []});
        let capture = AdvisorToolCapture::default();
        let system = json!([{"type": "text", "text": "custom"}]);
        let req = build_advisor_request(&executor, "m", Some(&system), vec![]);
        assert_eq!(req["system"], system);
    }

    // ── 转录组装 ──

    #[test]
    fn builds_advisor_messages_as_quoted_json() {
        let executor = json!({
            "system": "executor system",
            "tools": [{"name": "bash", "type": "function"}],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let assistant = json!({"role": "assistant", "content": [
            {"type": "text", "text": "let me think"},
            {"type": "tool_use", "id": "toolu_01", "name": "advisor", "input": {"focus": "review the plan"}}
        ]});
        let messages = build_advisor_messages(&executor, &assistant, Some("review the plan"));
        assert_eq!(messages.len(), 1, "转录打包为单个 quoted user 消息");
        assert_eq!(messages[0]["role"], "user");
        let payload = messages[0]["content"].as_str().unwrap();
        assert!(
            payload.starts_with("The executor model has paused to consult you."),
            "Oracle quoted-context 引导"
        );
        // payload 的 JSON 部分（引导文本之后）应包含完整转录字段
        let quoted: Value = payload
            .rsplit_once('\n')
            .map(|(_, rest)| rest)
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);
        assert_eq!(quoted["executor_system_prompt"], "executor system");
        assert_eq!(quoted["executor_tool_definitions"][0]["name"], "bash");
        assert_eq!(quoted["conversation"][0]["content"], "hi");
        assert_eq!(quoted["executor_text_this_turn"], "let me think");
        assert_eq!(quoted["executor_focus"], "review the plan");
    }

    #[test]
    fn builds_continuation_request_appends_turns() {
        let body = json!({
            "model": "m",
            "tools": [{"name": "advisor", "type": "function"}],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let assistant = json!({"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_01", "name": "advisor", "input": {}}]});
        let injection = json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_01", "content": "advice"}]});
        let continued = build_continuation_request(&body, &assistant, &injection);
        let messages = continued["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(continued["tools"], body["tools"], "tools preserved for re-call");
        assert_eq!(continued["model"], "m");
    }

    // ── 建议提取 ──

    #[test]
    fn extracts_text_blocks_dropping_thinking() {
        let response = json!({
            "content": [
                {"type": "thinking", "thinking": "hmm"},
                {"type": "text", "text": "line one"},
                {"type": "text", "text": "line two"}
            ]
        });
        assert_eq!(extract_advisor_text(&response).as_deref(), Some("line one\nline two"));
    }

    #[test]
    fn empty_or_missing_text_returns_none() {
        assert_eq!(extract_advisor_text(&json!({"content": []})), None);
        assert_eq!(extract_advisor_text(&json!({"content": [{"type": "thinking"}]})), None);
        assert_eq!(extract_advisor_text(&json!({})), None);
    }

    // ── 回注体构造 ──
    #[test]
    fn builds_advisor_result_injection() {
        let inj = build_advisor_result_injection("toolu_01", "advice text");
        assert_eq!(inj["type"], "advisor_tool_result");
        assert_eq!(inj["tool_use_id"], "toolu_01");
        assert_eq!(inj["content"]["type"], "advisor_result");
        assert_eq!(inj["content"]["text"], "advice text");
    }

    #[test]
    fn builds_tool_result_injection() {
        let inj = build_tool_result_injection("toolu_01", "plain text");
        assert_eq!(inj["type"], "tool_result");
        assert_eq!(inj["tool_use_id"], "toolu_01");
        assert_eq!(inj["content"], "plain text");
    }

    // ── 续跑决策 ──

    #[test]
    fn continues_while_advisor_tool_use_present() {
        let response = json!({
            "content": [{"type": "tool_use", "id": "toolu_01", "name": "advisor", "input": {}}]
        });
        assert!(should_continue_with_advisor(&response));
        assert!(!should_continue_with_advisor(&json!({"content": [{"type": "text", "text": "done"}]})));
    }

    // ── base_url 读取 ──

    #[test]
    fn reads_base_url_from_provider_env() {        let provider = Provider::with_id(
            "p".into(),
            "P".into(),
            json!({"env": {"ANTHROPIC_BASE_URL": "https://api.deepseek.com/anthropic/"}}),
            None,
        );
        assert_eq!(
            provider_base_url(&provider).as_deref(),
            Some("https://api.deepseek.com/anthropic")
        );
    }

    #[test]
    fn reads_base_url_from_legacy_keys() {
        let provider = Provider::with_id(
            "p".into(),
            "P".into(),
            json!({"baseURL": "https://openrouter.ai/api/v1"}),
            None,
        );
        assert_eq!(
            provider_base_url(&provider).as_deref(),
            Some("https://openrouter.ai/api/v1")
        );
    }

    #[test]
    fn missing_base_url_returns_none() {
        let provider = Provider::with_id("p".into(), "P".into(), json!({}), None);
        assert_eq!(provider_base_url(&provider), None);
    }

    // ── advisor 模型档位解析 ──

    fn provider_with_model_mapping() -> Provider {
        Provider::with_id(
            "p".into(),
            "P".into(),
            json!({
                "env": {
                    "ANTHROPIC_MODEL": "default-model",
                    "ANTHROPIC_DEFAULT_FABLE_MODEL": "fable-mapped",
                    "ANTHROPIC_DEFAULT_OPUS_MODEL": "opus-mapped"
                }
            }),
            None,
        )
    }

    #[test]
    fn resolves_advisor_model_via_client_tier() {
        // opus 档已配置映射 → 返回档位原值，由 forwarder 管线做唯一一次映射
        let provider = provider_with_model_mapping();
        let capture = AdvisorToolCapture {
            model: Some("claude-opus-4-8".to_string()),
            max_uses: None,
            max_tokens: None,
        };
        assert_eq!(
            resolve_advisor_model(&capture, &provider, "claude-sonnet-4-5"),
            "claude-opus-4-8"
        );
    }

    #[test]
    fn resolves_advisor_model_fable_tier() {
        // fable 未单独配置但 opus 已配 → fable 档归 opus 档（与 map_model 一致）
        let provider = provider_with_model_mapping();
        let capture = AdvisorToolCapture {
            model: Some("claude-fable-5".to_string()),
            max_uses: None,
            max_tokens: None,
        };
        assert_eq!(
            resolve_advisor_model(&capture, &provider, "claude-sonnet-4-5"),
            "claude-fable-5"
        );
    }

    #[test]
    fn advisor_model_falls_back_to_executor_model_when_tier_unconfigured() {
        // sonnet 档未配置 → 回落 executor 请求模型原值，映射后=executor 已映射模型
        let provider = provider_with_model_mapping(); // 只有 fable/opus/default
        let capture = AdvisorToolCapture {
            model: Some("claude-sonnet-4-5".to_string()),
            max_uses: None,
            max_tokens: None,
        };
        assert_eq!(
            resolve_advisor_model(&capture, &provider, "claude-sonnet-4-5"),
            "claude-sonnet-4-5"
        );
    }

    #[test]
    fn advisor_model_falls_back_to_executor_model_when_no_capture() {
        let provider = provider_with_model_mapping();
        let capture = AdvisorToolCapture::default();
        assert_eq!(
            resolve_advisor_model(&capture, &provider, "claude-sonnet-4-5"),
            "claude-sonnet-4-5"
        );
    }

    // ── beta 头剥离 ──

    #[test]
    fn strips_advisor_beta_value_keeping_others() {
        let mut headers = http::HeaderMap::new();
        headers.insert("anthropic-beta", http::HeaderValue::from_static("claude-code-20250219,advisor-tool-2026-03-01"));
        strip_advisor_beta_header(&mut headers);
        assert_eq!(headers.get("anthropic-beta").unwrap().to_str().unwrap(), "claude-code-20250219");
    }

    #[test]
    fn strips_advisor_beta_value_removing_header_when_only_value() {
        let mut headers = http::HeaderMap::new();
        headers.insert("anthropic-beta", http::HeaderValue::from_static("advisor-tool-2026-03-01"));
        strip_advisor_beta_header(&mut headers);
        assert!(headers.get("anthropic-beta").is_none());
    }

    #[test]
    fn leaves_headers_unchanged_without_advisor_beta() {
        let mut headers = http::HeaderMap::new();
        headers.insert("anthropic-beta", http::HeaderValue::from_static("claude-code-20250219"));
        headers.insert("x-test", http::HeaderValue::from_static("v"));
        let original = headers.clone();
        strip_advisor_beta_header(&mut headers);
        assert_eq!(headers, original);
    }

    #[test]
    fn strips_advisor_beta_from_multi_value_header() {
        let mut headers = http::HeaderMap::new();
        headers.append("anthropic-beta", http::HeaderValue::from_static("advisor-tool-2026-03-01"));
        headers.append("anthropic-beta", http::HeaderValue::from_static("claude-code-20250219"));
        strip_advisor_beta_header(&mut headers);
        let values: Vec<&str> = headers.get_all("anthropic-beta").iter().filter_map(|v| v.to_str().ok()).collect();
        assert_eq!(values, vec!["claude-code-20250219"]);
    }
}
