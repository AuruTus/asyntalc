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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}
impl Message {
    pub fn text(role: &str, content: String) -> Self {
        Self {
            role: role.into(),
            content,
            tool_calls: None,
            tool_call_id: None,
        }
    }
}

pub enum Turn {
    Complete(Completion),
    Question(Question),
    Tool(WorkspaceCall),
}

pub struct WorkspaceCall {
    pub call_id: String,
    pub name: String,
    pub arguments: String,
    pub assistant: Message,
    pub usage: Usage,
}

pub struct Question {
    pub call_id: String,
    pub prompt: String,
    pub choices: Option<Vec<String>>,
    pub assistant: Message,
    pub usage: Usage,
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

    pub async fn complete(&self, mut messages: Vec<Message>) -> Result<Turn, Failure> {
        if !self.config.system_prompt.is_empty() {
            messages.insert(
                0,
                Message::text(
                    &self.config.instruction_role,
                    self.config.system_prompt.clone(),
                ),
            );
        }
        let mut body = json!({"model": self.config.model, "messages": messages, "stream": false});
        let mut tools = self
            .config
            .workspace
            .as_ref()
            .map_or_else(Vec::new, |w| w.tool_definitions());
        if self.config.ask_parent {
            tools.push(json!({"type":"function","function":{
                "name":"ask_parent","description":"Ask the parent for clarification and pause until it answers.",
                "parameters":{"type":"object","properties":{"prompt":{"type":"string"},"choices":{"type":"array","items":{"type":"string"}}},"required":["prompt"],"additionalProperties":false}
            }}));
        }
        if !tools.is_empty() {
            body["tools"] = json!(tools);
            body["parallel_tool_calls"] = json!(false);
        }
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

    async fn request(&self, body: Value) -> Result<Turn, Failure> {
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
        parse_completion(
            &bytes,
            self.config.max_output_bytes,
            self.config.ask_parent,
            self.config.workspace.as_ref(),
        )
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

fn parse_completion(
    bytes: &[u8],
    output_limit: usize,
    ask_parent: bool,
    workspace: Option<&crate::workspace::WorkspaceConfig>,
) -> Result<Turn, Failure> {
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
    if (ask_parent || workspace.is_some())
        && choice.finish_reason == "tool_calls"
        && choice.message.refusal.as_deref().is_none_or(str::is_empty)
        && choice.message.function_call.is_none()
    {
        let is_question =
            choice.message.tool_calls.as_ref().is_some_and(|calls| {
                calls.len() == 1 && calls[0]["function"]["name"] == "ask_parent"
            });
        return if ask_parent && (is_question || workspace.is_none()) {
            parse_question(choice.message, usage.clone(), output_limit).map(Turn::Question)
        } else {
            parse_workspace_call(choice.message, usage.clone(), output_limit, workspace)
                .map(Turn::Tool)
        }
        .map_err(|mut error| {
            error.usage = usage;
            error.finish_reason = Some("tool_calls".into());
            error
        });
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
            "Provider requested a capability that is not enabled",
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
    Ok(Turn::Complete(Completion {
        text,
        finish_reason: choice.finish_reason,
        usage,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionArgs {
    prompt: String,
    choices: Option<Vec<String>>,
}

fn parse_workspace_call(
    message: AssistantMessage,
    usage: Usage,
    output_limit: usize,
    workspace: Option<&crate::workspace::WorkspaceConfig>,
) -> Result<WorkspaceCall, Failure> {
    let invalid = || {
        Failure::new(
            "invalid_tool_call",
            "Provider returned an invalid workspace tool call",
        )
    };
    let calls = message.tool_calls.ok_or_else(|| {
        Failure::new(
            "invalid_tool_call",
            "Provider omitted tool_calls for a tool_calls response",
        )
    })?;
    if calls.len() != 1 {
        return Err(Failure::new(
            "invalid_tool_call",
            "Expected exactly one workspace tool call per response; provider returned zero or multiple calls",
        ));
    }
    let call = &calls[0];
    let id = call["id"].as_str().ok_or_else(invalid)?;
    let name = call["function"]["name"].as_str().ok_or_else(invalid)?;
    let arguments = call["function"]["arguments"].as_str().ok_or_else(invalid)?;
    if call["type"] != "function"
        || id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        || arguments.len() > 16 * 1024
    {
        return Err(invalid());
    }
    if !workspace.is_some_and(|w| w.permits(name)) {
        return Err(Failure::new(
            "unsupported_capability",
            "Provider requested a capability that is not enabled",
        ));
    }
    let content = message.content.unwrap_or_default();
    if content.len() > output_limit {
        return Err(Failure::new(
            "output_limit",
            "Provider tool call exceeds output limit",
        ));
    }
    Ok(WorkspaceCall {
        call_id: id.into(),
        name: name.into(),
        arguments: arguments.into(),
        usage,
        assistant: Message {
            role: "assistant".into(),
            content,
            tool_call_id: None,
            tool_calls: Some(vec![
                json!({"id":id,"type":"function","function":{"name":name,"arguments":arguments}}),
            ]),
        },
    })
}

fn parse_question(
    message: AssistantMessage,
    usage: Usage,
    output_limit: usize,
) -> Result<Question, Failure> {
    let invalid = || {
        Failure::new(
            "invalid_parent_question",
            "Provider returned an invalid ask_parent call",
        )
    };
    let calls = message.tool_calls.ok_or_else(invalid)?;
    if calls.len() != 1 {
        return Err(invalid());
    }
    let call = &calls[0];
    let id = call["id"].as_str().ok_or_else(invalid)?;
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
    {
        return Err(invalid());
    }
    if call["type"] != "function" || call["function"]["name"] != "ask_parent" {
        return Err(invalid());
    }
    let arguments = call["function"]["arguments"].as_str().ok_or_else(invalid)?;
    if arguments.len() > 16 * 1024 {
        return Err(invalid());
    }
    let args: QuestionArgs = serde_json::from_str(arguments).map_err(|_| invalid())?;
    if args.prompt.trim().is_empty()
        || args.prompt.len() > 8 * 1024
        || args.choices.as_ref().is_some_and(|v| {
            v.is_empty() || v.len() > 8 || v.iter().any(|s| s.trim().is_empty() || s.len() > 256)
        })
    {
        return Err(invalid());
    }
    let content = message.content.unwrap_or_default();
    if content.len() > output_limit {
        return Err(Failure::new(
            "output_limit",
            "Provider question exceeds output limit",
        ));
    }
    let call_id = id.to_owned();
    // Persist only validated fields, not arbitrary provider metadata.
    let tool_calls =
        json!({"id":id,"type":"function","function":{"name":"ask_parent","arguments":arguments}});
    Ok(Question {
        call_id,
        prompt: args.prompt,
        choices: args.choices,
        usage,
        assistant: Message {
            role: "assistant".into(),
            content,
            tool_calls: Some(vec![tool_calls]),
            tool_call_id: None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> crate::workspace::WorkspaceConfig {
        serde_json::from_value(json!({"root":"/tmp","operations":["read_file"]})).unwrap()
    }

    fn call(name: &str, arguments: &str) -> Value {
        json!({"id":"call_1","type":"function","function":{"name":name,"arguments":arguments},"untrusted":"discard"})
    }

    fn response(calls: Vec<Value>) -> Vec<u8> {
        serde_json::to_vec(&json!({"choices":[{"index":0,"finish_reason":"tool_calls","message":{"role":"assistant","content":null,"tool_calls":calls}}],"usage":{"prompt_tokens":12,"completion_tokens":3}})).unwrap()
    }

    fn error_code(
        bytes: &[u8],
        config: Option<&crate::workspace::WorkspaceConfig>,
    ) -> &'static str {
        match parse_completion(bytes, 1024, false, config) {
            Err(error) => error.code,
            Ok(_) => panic!("expected provider failure"),
        }
    }

    #[test]
    fn workspace_call_retains_only_protocol_fields_and_defers_argument_errors() {
        let config = workspace();
        let bytes = response(vec![call("workspace_read_file", "invalid JSON")]);
        let parsed = match parse_completion(&bytes, 1024, false, Some(&config)) {
            Ok(Turn::Tool(call)) => call,
            _ => panic!("expected workspace call"),
        };
        assert_eq!(parsed.name, "workspace_read_file");
        assert_eq!(parsed.arguments, "invalid JSON");
        assert_eq!(parsed.usage.input_tokens, Some(12));
        assert!(
            parsed.assistant.tool_calls.unwrap()[0]
                .get("untrusted")
                .is_none()
        );
    }

    #[test]
    fn workspace_calls_require_one_enabled_bounded_call() {
        let config = workspace();
        let valid = call("workspace_read_file", r#"{"path":"file.txt"}"#);
        assert_eq!(
            error_code(&response(vec![valid.clone()]), None),
            "unsupported_capability"
        );
        assert_eq!(
            error_code(
                &response(vec![call("workspace_search", "{}")]),
                Some(&config)
            ),
            "unsupported_capability"
        );
        assert_eq!(
            error_code(&response(vec![valid.clone(), valid.clone()]), Some(&config)),
            "invalid_tool_call"
        );
        match parse_completion(
            &response(vec![valid.clone(), valid.clone()]),
            1024,
            false,
            Some(&config),
        ) {
            Err(failure) => assert!(failure.message.contains("zero or multiple calls")),
            Ok(_) => panic!("batched workspace calls must be rejected"),
        }
        let mut bad_id = valid;
        bad_id["id"] = json!("bad/id");
        assert_eq!(
            error_code(&response(vec![bad_id]), Some(&config)),
            "invalid_tool_call"
        );
        assert_eq!(
            error_code(
                &response(vec![call(
                    "workspace_read_file",
                    &"x".repeat(16 * 1024 + 1)
                )]),
                Some(&config)
            ),
            "invalid_tool_call"
        );
    }
}
