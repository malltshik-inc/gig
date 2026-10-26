use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug)]
pub struct ChatOptions {
    pub max_tokens: u32,
    pub temperature: f64,
}

#[allow(dead_code)] // token and timing telemetry is kept for the run log / future status bar
pub struct ModelReply {
    pub value: Value,
    pub prompt_tokens: u64,
    pub eval_tokens: u64,
    pub elapsed_ms: u128,
    /// True when Ollama stopped at the output limit, so the JSON may be cut off.
    pub truncated: bool,
}

#[async_trait]
pub trait ModelClient: Send + Sync {
    async fn chat_json(
        &self,
        system: &str,
        user: &str,
        schema: Value,
        options: ChatOptions,
    ) -> Result<ModelReply>;
}

pub struct OllamaClient {
    base: String,
    model: String,
    num_ctx: u32,
    http: reqwest::Client,
    verbose: bool,
}

impl OllamaClient {
    pub fn new(model: String, num_ctx: u32, verbose: bool) -> Self {
        let base = std::env::var("OLLAMA_HOST")
            .ok()
            .map(|host| {
                if host.starts_with("http://") || host.starts_with("https://") {
                    host
                } else {
                    format!("http://{host}")
                }
            })
            .unwrap_or_else(|| "http://127.0.0.1:11434".to_string());
        Self {
            base: base.trim_end_matches('/').to_string(),
            model,
            num_ctx,
            http: reqwest::Client::new(),
            verbose,
        }
    }

    pub async fn available_models(&self) -> Result<Vec<String>> {
        let response = self
            .http
            .get(format!("{}/api/tags", self.base))
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .context("failed to reach Ollama; is `ollama serve` running?")?;
        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read the Ollama model list")?;
        if !status.is_success() {
            bail!("Ollama returned {status} for /api/tags: {text}");
        }
        let parsed: TagsResponse =
            serde_json::from_str(&text).context("invalid response from Ollama /api/tags")?;
        Ok(parsed.models.into_iter().map(|model| model.name).collect())
    }

    pub async fn ensure_model(&self) -> Result<()> {
        let models = self.available_models().await?;
        let wanted = &self.model;
        let found = models.iter().any(|name| {
            name == wanted
                || (!wanted.contains(':') && name == &format!("{wanted}:latest"))
                || (!name.contains(':') && name == wanted)
        });
        if found {
            return Ok(());
        }
        let available = if models.is_empty() {
            "(none)".to_string()
        } else {
            models.join(", ")
        };
        bail!(
            "model `{wanted}` is not available in Ollama.\ninstalled: {available}\nrun: ollama pull {wanted}"
        );
    }
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagEntry>,
}

#[derive(Deserialize)]
struct TagEntry {
    name: String,
}

#[async_trait]
impl ModelClient for OllamaClient {
    async fn chat_json(
        &self,
        system: &str,
        user: &str,
        schema: Value,
        options: ChatOptions,
    ) -> Result<ModelReply> {
        let start = Instant::now();
        let mut messages = vec![
            json!({"role": "system", "content": system}),
            json!({"role": "user", "content": user}),
        ];
        let mut last_content = String::new();
        let mut last_truncated = false;

        for attempt in 0..2 {
            let body = json!({
                "model": self.model,
                "messages": messages.clone(),
                "stream": false,
                "format": schema,
                "options": {
                    "temperature": options.temperature,
                    "num_ctx": self.num_ctx,
                    "num_predict": options.max_tokens,
                    "repeat_penalty": 1.05,
                    "top_p": 0.9,
                }
            });

            if self.verbose {
                eprintln!("[model] request (attempt {})", attempt + 1);
                eprintln!("[model] system:\n{system}");
                eprintln!("[model] user:\n{}", truncate(user, 6000));
            }

            let response = self
                .http
                .post(format!("{}/api/chat", self.base))
                .json(&body)
                .timeout(Duration::from_secs(900))
                .send()
                .await
                .context("failed to reach Ollama; is `ollama serve` running?")?;
            let status = response.status();
            let text = response
                .text()
                .await
                .context("failed to read the Ollama response")?;
            if !status.is_success() {
                bail!("Ollama returned {status}: {}", truncate(&text, 600));
            }

            let parsed: ChatResponse = serde_json::from_str(&text)
                .context("Ollama returned an envelope that does not match /api/chat")?;
            let content = parsed.message.content;
            if self.verbose {
                eprintln!("[model] reply:\n{}", truncate(&content, 6000));
            }

            let truncated = match parsed.done_reason.as_deref() {
                Some(reason) => reason == "length",
                None => parsed
                    .eval_count
                    .is_some_and(|count| count >= u64::from(options.max_tokens)),
            };
            last_truncated = truncated;

            if let Some(value) = extract_json(&content) {
                return Ok(ModelReply {
                    value,
                    prompt_tokens: parsed.prompt_eval_count.unwrap_or(0),
                    eval_tokens: parsed.eval_count.unwrap_or(0),
                    elapsed_ms: start.elapsed().as_millis(),
                    truncated,
                });
            }

            last_content = content;
            if attempt == 0 {
                messages.push(json!({"role": "assistant", "content": last_content}));
                messages.push(json!({
                    "role": "user",
                    "content": "Your previous reply was not valid JSON. Reply again with only the JSON object required by the schema. No markdown fences, no prose."
                }));
            }
        }

        bail!(
            "the model did not return valid JSON{}; last reply:\n{}",
            if last_truncated {
                " (it hit the output limit before finishing)"
            } else {
                ""
            },
            truncate(&last_content, 800)
        )
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    message: ChatMessage,
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
    #[serde(default)]
    done_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatMessage {
    content: String,
}

pub fn extract_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
        return Some(value);
    }
    let start = trimmed.find('{')?;
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, character) in trimmed[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
            continue;
        }
        match character {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    let end = start + offset + character.len_utf8();
                    return serde_json::from_str(&trimmed[start..end]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

pub fn planner_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "thought": {"type": "string"},
            "needs_clarification": {"type": "boolean"},
            "clarification": {"type": "string"},
            "plan": {"type": "string"},
            "files": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "symbols": {"type": "array", "items": {"type": "string"}},
                        "intent": {"type": "string"},
                        "create": {"type": "boolean"}
                    },
                    "required": ["path", "symbols", "intent", "create"]
                }
            },
            "renames": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "from": {"type": "string"},
                        "to": {"type": "string"}
                    },
                    "required": ["from", "to"]
                }
            },
            "verification": {"type": "array", "items": {"type": "string"}}
        },
        "required": [
            "thought",
            "needs_clarification",
            "clarification",
            "plan",
            "files",
            "renames",
            "verification"
        ]
    })
}

pub fn editor_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "thought": {"type": "string"},
            "give_up": {"type": "boolean"},
            "new_imports": {"type": "array", "items": {"type": "string"}},
            "new_code": {"type": "string"}
        },
        "required": ["thought", "give_up", "new_imports", "new_code"]
    })
}

pub fn commit_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "message": {"type": "string"}
        },
        "required": ["message"]
    })
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    #[serde(default)]
    pub thought: String,
    #[serde(default)]
    pub needs_clarification: bool,
    #[serde(default)]
    pub clarification: String,
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub files: Vec<PlanFile>,
    /// Whole-crate identifier renames, applied deterministically before edits.
    #[serde(default)]
    pub renames: Vec<Rename>,
    #[serde(default)]
    pub verification: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Rename {
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanFile {
    pub path: String,
    #[serde(default)]
    pub symbols: Vec<String>,
    #[serde(default)]
    pub intent: String,
    #[serde(default)]
    pub create: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edit {
    #[serde(default)]
    pub thought: String,
    #[serde(default)]
    pub give_up: bool,
    #[serde(default)]
    pub new_imports: Vec<String>,
    #[serde(default)]
    pub new_code: String,
    /// Set by the harness when Ollama stopped at the output limit.
    #[serde(skip)]
    pub truncated: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitMessage {
    #[serde(default)]
    pub message: String,
}

/// Deterministic client for tests and dry runs.
#[allow(dead_code)]
pub struct MockClient {
    replies: Mutex<VecDeque<Value>>,
}

#[allow(dead_code)]
impl MockClient {
    pub fn new(replies: Vec<Value>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
        }
    }
}

#[async_trait]
impl ModelClient for MockClient {
    async fn chat_json(
        &self,
        _system: &str,
        _user: &str,
        _schema: Value,
        _options: ChatOptions,
    ) -> Result<ModelReply> {
        let value = self
            .replies
            .lock()
            .expect("mock client poisoned")
            .pop_front()
            .context("mock client ran out of replies")?;
        Ok(ModelReply {
            value,
            prompt_tokens: 0,
            eval_tokens: 0,
            elapsed_ms: 0,
            truncated: false,
        })
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… truncated", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_json_from_fenced_or_noisy_text() {
        let value = extract_json("```json\n{\"ok\": true}\n```").unwrap();
        assert_eq!(value["ok"], true);

        let value = extract_json("here you go: {\"a\": {\"b\": 2}} thanks").unwrap();
        assert_eq!(value["a"]["b"], 2);
    }

    #[test]
    fn ignores_braces_inside_strings() {
        let value = extract_json("{\"code\": \"fn main() {}\"}").unwrap();
        assert_eq!(value["code"], "fn main() {}");
    }
}
