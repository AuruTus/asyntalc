use std::time::Duration;

use reqwest::{
    Client,
    header::{AUTHORIZATION, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::ChatConfig;

#[derive(Clone, Deserialize, Serialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Usage {
    pub model_requests: u32,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
}

pub struct Completion {
    pub text: String,
    pub finish_reason: String,
    pub usage: Usage,
}

pub struct Failure {
    pub code: &'static str,
    pub message: &'static str,
    pub usage: Usage,
    pub finish_reason: Option<String>,
    pub partial_text: Option<String>,
}

impl Failure {
    pub fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            code,
            message,
            usage: Usage::default(),
            finish_reason: None,
            partial_text: None,
        }
    }
}

pub struct ChatProvider {
    client: Client,
    authorization: HeaderValue,
    config: ChatConfig,
}

impl ChatProvider {
    pub fn new(config: ChatConfig) -> anyhow::Result<Self> {
        let key = std::env::var(&config.api_key_env).map_err(|_| {
            anyhow::anyhow!("configured API-key environment variable is missing or is not UTF-8")
        })?;
        anyhow::ensure!(!key.trim().is_empty(), "configured API key is empty");
        let mut authorization = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| anyhow::anyhow!("configured API key cannot be used as an HTTP header"))?;
        authorization.set_sensitive(true);
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_millis(config.request_timeout_ms))
            .build()
            .map_err(|_| anyhow::anyhow!("cannot initialize provider HTTP client"))?;
        Ok(Self {
            client,
            authorization,
            config,
        })
    }

    pub async fn complete(&self, mut messages: Vec<Message>) -> Result<Completion, Failure> {
        if !self.config.system_prompt.is_empty() {
            messages.insert(
                0,
                Message {
                    role: self.config.instruction_role.clone(),
                    content: self.config.system_prompt.clone(),
                },
            );
        }
        let mut body = json!({"model": self.config.model, "messages": messages, "stream": false});
        body[&self.config.output_token_parameter] = json!(self.config.max_output_tokens);
        if let Some(effort) = &self.config.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        let result = self.request(body).await;
        // Attempted requests remain distinguishable from preflight failures.
        result.map_err(|mut failure| {
            failure.usage.model_requests = 1;
            failure
        })
    }

    async fn request(&self, body: Value) -> Result<Completion, Failure> {
        let mut response = self
            .client
            .post(format!("{}/chat/completions", self.config.base_url))
            .header(AUTHORIZATION, self.authorization.clone())
            .json(&body)
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            // Never persist or print raw provider errors, headers, or URLs.
            return Err(match status {
                401 | 403 => Failure::new(
                    "provider_auth_error",
                    "Provider rejected authentication or access",
                ),
                429 => Failure::new(
                    "provider_rate_limited",
                    "Provider rate limit reached; request was not retried",
                ),
                500..=599 => Failure::new(
                    "provider_unavailable",
                    "Provider service failed; request was not retried",
                ),
                300..=399 => Failure::new(
                    "provider_redirect",
                    "Provider redirect refused; configure the final API base URL",
                ),
                _ => Failure::new(
                    "provider_request_error",
                    "Provider rejected the request; check endpoint, model, and compatibility settings",
                ),
            });
        }
        let limit = self.config.max_response_bytes;
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(Failure::new(
                "response_limit",
                "Provider response exceeds the configured byte limit",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
            if bytes.len().saturating_add(chunk.len()) > limit {
                return Err(Failure::new(
                    "response_limit",
                    "Provider response exceeds the configured byte limit",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        parse_completion(&bytes, self.config.max_output_bytes)
    }
}

fn transport_error(error: reqwest::Error) -> Failure {
    if error.is_timeout() {
        Failure::new(
            "provider_timeout",
            "Provider request timed out; remote generation may continue",
        )
    } else {
        Failure::new(
            "provider_transport_error",
            "Provider connection failed; request was not retried",
        )
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    usage: Option<TokenUsage>,
}

#[derive(Deserialize)]
struct TokenUsage {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct Choice {
    index: u32,
    finish_reason: String,
    message: AssistantMessage,
}

#[derive(Deserialize)]
struct AssistantMessage {
    role: String,
    content: Option<String>,
    refusal: Option<String>,
    tool_calls: Option<Vec<Value>>,
    function_call: Option<Value>,
}

fn parse_completion(bytes: &[u8], output_limit: usize) -> Result<Completion, Failure> {
    let protocol_error = || {
        Failure::new(
            "provider_protocol_error",
            "Provider returned an invalid Chat Completions response",
        )
    };
    let response: ChatResponse = serde_json::from_slice(bytes).map_err(|_| protocol_error())?;
    if response.usage.as_ref().is_some_and(|u| {
        [u.prompt_tokens, u.completion_tokens]
            .into_iter()
            .flatten()
            .any(|n| n > i64::MAX as u64)
    }) {
        return Err(protocol_error());
    }
    let usage = Usage {
        model_requests: 1,
        input_tokens: response
            .usage
            .as_ref()
            .and_then(|u| u.prompt_tokens)
            .map(|n| n as i64),
        output_tokens: response
            .usage
            .as_ref()
            .and_then(|u| u.completion_tokens)
            .map(|n| n as i64),
    };
    if response.choices.len() != 1 {
        return Err(protocol_error());
    }
    let choice = response.choices.into_iter().next().unwrap();
    if choice.index != 0 || choice.message.role != "assistant" {
        return Err(protocol_error());
    }
    // Bound provider-controlled metadata as well as text.
    if choice.finish_reason.len() > 64 {
        return Err(protocol_error());
    }
    let text = choice.message.content.unwrap_or_default();
    let error = if choice
        .message
        .refusal
        .as_ref()
        .is_some_and(|s| !s.is_empty())
    {
        Some(Failure::new(
            "provider_refusal",
            "Provider refused to produce an answer",
        ))
    } else if choice
        .message
        .tool_calls
        .as_ref()
        .is_some_and(|v| !v.is_empty())
        || choice.message.function_call.is_some()
        || matches!(
            choice.finish_reason.as_str(),
            "tool_calls" | "function_call"
        )
    {
        Some(Failure::new(
            "unsupported_capability",
            "Tool calls are not supported in this milestone",
        ))
    } else if text.len() > output_limit || choice.finish_reason == "length" {
        Some(Failure::new(
            "output_limit",
            "Provider answer reached an output limit",
        ))
    } else if choice.finish_reason == "content_filter" {
        Some(Failure::new(
            "provider_content_filter",
            "Provider filtered the answer",
        ))
    } else if choice.finish_reason != "stop" || text.trim().is_empty() {
        Some(protocol_error())
    } else {
        None
    };
    if let Some(mut failure) = error {
        failure.usage = usage;
        failure.finish_reason = Some(choice.finish_reason);
        // Retain bounded partial text for output-limit failures only, never as a final answer.
        if failure.code == "output_limit" && !text.is_empty() {
            let mut end = text.len().min(output_limit).min(16 * 1024);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            failure.partial_text = Some(text[..end].to_owned());
        }
        return Err(failure);
    }
    Ok(Completion {
        text,
        finish_reason: choice.finish_reason,
        usage,
    })
}
