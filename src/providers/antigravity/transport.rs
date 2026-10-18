//! Bounded native SSE collection. No generation retry; no success before EOF.
use super::replay::{self, Result};
use serde_json::{Map, Value, json};

const MAX_TURN: usize = 8 * 1024 * 1024;
const MAX_FRAME: usize = 1024 * 1024;

#[derive(Default)]
pub(crate) struct Reducer {
    bytes: usize,
    line: Vec<u8>,
    data: Vec<u8>,
    contents: Vec<Value>,
    usage: Map<String, Value>,
    terminal: Option<String>,
    parts: usize,
}

pub(crate) struct Turn {
    pub contents: Value,
    pub usage: Value,
    pub stop_reason: String,
}

impl Reducer {
    pub fn push(&mut self, chunk: &[u8]) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(chunk.len())
            .ok_or("native turn byte limit")?;
        if self.bytes > MAX_TURN {
            return Err("native turn byte limit");
        }
        for byte in chunk {
            if *byte == b'\n' {
                let mut line = std::mem::take(&mut self.line);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    self.frame()?;
                } else if let Some(data) = line.strip_prefix(b"data:") {
                    let data = data.strip_prefix(b" ").unwrap_or(data);
                    if self.data.len() + data.len() + 1 > MAX_FRAME {
                        return Err("native SSE frame byte limit");
                    }
                    self.data.extend_from_slice(data);
                    self.data.push(b'\n');
                }
            } else {
                self.line.push(*byte);
                if self.line.len() > MAX_FRAME {
                    return Err("native SSE line byte limit");
                }
            }
        }
        Ok(())
    }
    fn frame(&mut self) -> Result<()> {
        if self.data.is_empty() {
            return Ok(());
        }
        let mut data = std::mem::take(&mut self.data);
        data.pop();
        if data == b"[DONE]" {
            return if self.terminal.is_some() {
                Ok(())
            } else {
                Err("native stream missing terminal state")
            };
        }
        let value = replay::parse_json(&data, MAX_FRAME)?;
        if value.get("error").is_some() {
            return Err("native upstream error");
        }
        let response = value
            .get("response")
            .ok_or("invalid native response envelope")?;
        if response.get("error").is_some()
            || response
                .get("promptFeedback")
                .is_some_and(|p| p.get("blockReason").is_some())
        {
            return Err("native refusal/error");
        }
        if let Some(usage) = response.get("usageMetadata") {
            let usage = usage.as_object().ok_or("invalid native usage")?;
            for key in [
                "promptTokenCount",
                "cachedContentTokenCount",
                "candidatesTokenCount",
                "thoughtsTokenCount",
                "totalTokenCount",
            ] {
                if let Some(value) = usage.get(key) {
                    let number = value
                        .as_u64()
                        .filter(|n| *n <= 1_000_000_000)
                        .ok_or("invalid native usage count")?;
                    if self
                        .usage
                        .get(key)
                        .and_then(Value::as_u64)
                        .is_some_and(|old| number < old)
                    {
                        return Err("native cumulative usage decreased");
                    }
                    self.usage.insert(key.into(), value.clone());
                }
            }
        }
        if let Some(candidates) = response.get("candidates") {
            let candidates = candidates
                .as_array()
                .filter(|c| c.len() <= 1)
                .ok_or("multiple/invalid native candidates")?;
            for candidate in candidates {
                if candidate
                    .get("index")
                    .is_some_and(|v| v.as_u64() != Some(0))
                {
                    return Err("unsupported candidate index");
                }
                if let Some(content) = candidate.get("content") {
                    if content["role"] != "model" {
                        return Err("invalid native content role");
                    }
                    let parts = content["parts"].as_array().ok_or("invalid native parts")?;
                    self.parts += parts.len();
                    if self.parts > 4096 || self.contents.len() >= 512 {
                        return Err("native content/part limit");
                    }
                    for part in parts {
                        if !part.is_object()
                            || part.get("text").is_some_and(|v| !v.is_string())
                            || part.get("thought").is_some_and(|v| !v.is_boolean())
                            || part.get("thoughtSignature").is_some_and(|v| !v.is_string())
                        {
                            return Err("invalid native part");
                        }
                        if [
                            "inlineData",
                            "fileData",
                            "executableCode",
                            "codeExecutionResult",
                            "functionResponse",
                        ]
                        .iter()
                        .any(|k| part.get(*k).is_some())
                        {
                            return Err("unsupported native content/media");
                        }
                        if let Some(call) = part.get("functionCall") {
                            replay::keys(call, &["name", "args", "id"], &["name"])?;
                            replay::call_arguments(call)?;
                        }
                        if !["text", "functionCall", "thoughtSignature"]
                            .iter()
                            .any(|k| part.get(*k).is_some())
                        {
                            return Err("unsupported native part");
                        }
                    }
                    self.contents.push(content.clone());
                }
                if let Some(reason) = candidate.get("finishReason") {
                    let reason = reason
                        .as_str()
                        .filter(|r| matches!(*r, "STOP" | "MAX_TOKENS"))
                        .ok_or("unsupported native termination/refusal")?;
                    if self.terminal.as_ref().is_some_and(|old| old != reason) {
                        return Err("conflicting native terminal state");
                    }
                    self.terminal = Some(reason.to_string());
                }
            }
        }
        Ok(())
    }
    pub fn finish(self) -> Result<Turn> {
        if !self.line.is_empty() || !self.data.is_empty() {
            return Err("truncated native SSE frame");
        }
        let reason = self
            .terminal
            .ok_or("native stream missing terminal state")?;
        if self.contents.is_empty() {
            return Err("empty native turn");
        }
        let prompt = self
            .usage
            .get("promptTokenCount")
            .and_then(Value::as_u64)
            .ok_or("native prompt usage missing")?;
        let candidates = self
            .usage
            .get("candidatesTokenCount")
            .and_then(Value::as_u64)
            .ok_or("native output usage missing")?;
        let thoughts = self
            .usage
            .get("thoughtsTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let output = candidates
            .checked_add(thoughts)
            .ok_or("invalid native usage total")?;
        if self
            .usage
            .get("totalTokenCount")
            .and_then(Value::as_u64)
            .is_some_and(|total| total != prompt + output)
        {
            return Err("ambiguous native usage total");
        }
        let cache = self
            .usage
            .get("cachedContentTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(prompt);
        let contents = json!(self.contents);
        let has_calls = !replay::calls(&contents)?.is_empty();
        Ok(Turn {
            contents,
            usage: json!({"input_tokens":prompt-cache,"cache_read_input_tokens":cache,"output_tokens":output}),
            stop_reason: if reason == "MAX_TOKENS" {
                "max_tokens"
            } else if has_calls {
                "tool_use"
            } else {
                "end_turn"
            }
            .to_string(),
        })
    }
}

pub(crate) async fn collect(mut response: reqwest::Response) -> Result<Turn> {
    let mut reducer = Reducer::default();
    while let Some(chunk) =
        tokio::time::timeout(std::time::Duration::from_secs(30), response.chunk())
            .await
            .map_err(|_| "native stream idle timeout")?
            .map_err(|_| "native stream transport failed")?
    {
        reducer.push(&chunk)?;
    }
    reducer.finish()
}

pub(crate) fn message(blocks: Value, model: &str, turn: &Turn) -> Value {
    json!({"id":format!("msg_{}",uuid::Uuid::new_v4().simple()),"type":"message","role":"assistant","model":model,
        "content":blocks,"stop_reason":turn.stop_reason,"stop_sequence":null,"usage":turn.usage})
}

pub(crate) fn sse(message: &Value) -> Result<Vec<u8>> {
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    start["usage"]["output_tokens"] = json!(0);
    let mut events = vec![json!({"type":"message_start","message":start})];
    for (index, block) in message["content"]
        .as_array()
        .ok_or("invalid projected message")?
        .iter()
        .enumerate()
    {
        let (initial, deltas) = match block["type"].as_str() {
            Some("thinking") => (
                json!({"type":"thinking","thinking":"","signature":""}),
                vec![
                    json!({"type":"thinking_delta","thinking":block["thinking"]}),
                    json!({"type":"signature_delta","signature":block["signature"]}),
                ],
            ),
            Some("text") => (
                json!({"type":"text","text":""}),
                vec![json!({"type":"text_delta","text":block["text"]})],
            ),
            Some("tool_use") => (
                json!({"type":"tool_use","id":block["id"],"name":block["name"],"input":{}}),
                vec![
                    json!({"type":"input_json_delta","partial_json":serde_json::to_string(&block["input"]).map_err(|_| "invalid tool arguments")?}),
                ],
            ),
            _ => return Err("invalid projected block"),
        };
        events.push(json!({"type":"content_block_start","index":index,"content_block":initial}));
        for delta in deltas {
            events.push(json!({"type":"content_block_delta","index":index,"delta":delta}));
        }
        events.push(json!({"type":"content_block_stop","index":index}));
    }
    events.push(json!({"type":"message_delta","delta":{"stop_reason":message["stop_reason"],"stop_sequence":null},"usage":message["usage"]}));
    events.push(json!({"type":"message_stop"}));
    let mut wire = String::new();
    for event in events {
        wire.push_str(&format!(
            "event: {}\ndata: {}\n\n",
            event["type"].as_str().unwrap(),
            event
        ));
    }
    Ok(wire.into_bytes())
}
