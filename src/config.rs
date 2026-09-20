use std::{io::Read, path::Path};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "runner", content = "config", rename_all = "snake_case")]
pub enum Profile {
    Fake,
    Chat(Box<ChatConfig>),
}

impl Profile {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Fake => "fake",
            Self::Chat(_) => "chat",
        }
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ChatConfig {
    /// Enable the single ask_parent control tool; disabled profiles serialize as before.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ask_parent: bool,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    #[serde(default = "default_timeout")]
    pub request_timeout_ms: u64,
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default = "default_role")]
    pub instruction_role: String,
    #[serde(default = "default_token_parameter")]
    pub output_token_parameter: String,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default = "default_tokens")]
    pub max_output_tokens: u32,
    #[serde(default = "default_context")]
    pub max_context_bytes: usize,
    #[serde(default = "default_response")]
    pub max_response_bytes: usize,
    #[serde(default = "default_output")]
    pub max_output_bytes: usize,
}

fn is_false(value: &bool) -> bool {
    !value
}

fn default_timeout() -> u64 {
    120_000
}
fn default_role() -> String {
    "system".into()
}
fn default_token_parameter() -> String {
    "max_tokens".into()
}
fn default_tokens() -> u32 {
    4096
}
fn default_context() -> usize {
    256 * 1024
}
fn default_response() -> usize {
    1024 * 1024
}
fn default_output() -> usize {
    64 * 1024
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    provider: ChatConfig,
}

pub fn load(path: &Path) -> anyhow::Result<ChatConfig> {
    let mut text = String::new();
    std::fs::File::open(path)
        .context("cannot open provider configuration")?
        .take(64 * 1024 + 1)
        .read_to_string(&mut text)
        .context("cannot read provider configuration as UTF-8")?;
    ensure!(text.len() <= 64 * 1024, "configuration exceeds 64 KiB");
    // TOML diagnostics can contain source lines; do not echo potentially secret values.
    let parsed: ConfigFile = toml::from_str(&text).map_err(|_| {
        anyhow::anyhow!("invalid provider configuration; check TOML fields and types")
    })?;
    parsed.provider.validate()
}

impl ChatConfig {
    fn validate(mut self) -> anyhow::Result<Self> {
        let mut url = reqwest::Url::parse(&self.base_url)
            .map_err(|_| anyhow::anyhow!("invalid provider base_url"))?;
        ensure!(
            matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
            "base_url must be HTTP(S)"
        );
        ensure!(
            url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none(),
            "base_url must not contain credentials, query parameters, or fragments"
        );
        let path = url.path().trim_end_matches('/').to_owned();
        ensure!(
            !path.ends_with("/chat/completions"),
            "base_url must contain the API prefix, not /chat/completions"
        );
        url.set_path(&path);
        self.base_url = url.as_str().trim_end_matches('/').to_owned();
        ensure!(self.base_url.len() <= 2048, "base_url is too long");
        ensure!(
            !self.model.trim().is_empty() && self.model.len() <= 256,
            "model must contain 1 to 256 bytes"
        );
        ensure!(
            !self.api_key_env.is_empty()
                && self.api_key_env.len() <= 128
                && self
                    .api_key_env
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
            "invalid API-key environment-variable name"
        );
        ensure!(
            (1..=600_000).contains(&self.request_timeout_ms),
            "request_timeout_ms must be 1 to 600000"
        );
        ensure!(
            matches!(self.instruction_role.as_str(), "system" | "developer"),
            "instruction_role must be system or developer"
        );
        ensure!(
            matches!(
                self.output_token_parameter.as_str(),
                "max_tokens" | "max_completion_tokens"
            ),
            "unsupported output_token_parameter"
        );
        ensure!(
            self.reasoning_effort.as_deref().is_none_or(|s| matches!(
                s,
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
            )),
            "unsupported reasoning_effort"
        );
        ensure!(
            (1..=1_000_000).contains(&self.max_output_tokens),
            "max_output_tokens must be 1 to 1000000"
        );
        ensure!(
            (1..=4 * 1024 * 1024).contains(&self.max_context_bytes),
            "max_context_bytes must be 1 to 4194304"
        );
        ensure!(
            (1..=4 * 1024 * 1024).contains(&self.max_response_bytes),
            "max_response_bytes must be 1 to 4194304"
        );
        // Keeps the full JSON result below the 1 MiB socket frame even with escaping.
        ensure!(
            (1..=64 * 1024).contains(&self.max_output_bytes),
            "max_output_bytes must be 1 to 65536"
        );
        ensure!(
            self.system_prompt.len() <= self.max_context_bytes,
            "system_prompt exceeds context limit"
        );
        Ok(self)
    }
}
