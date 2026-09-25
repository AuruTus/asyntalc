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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<crate::workspace::WorkspaceConfig>,
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
    workspace: Option<crate::workspace::WorkspaceConfig>,
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
    let mut parsed: ConfigFile = toml::from_str(&text).map_err(|_| {
        anyhow::anyhow!("invalid provider configuration; check TOML fields and types")
    })?;
    ensure!(
        parsed.provider.workspace.is_none(),
        "use the top-level [workspace] table"
    );
    if let Some(workspace) = &mut parsed.workspace {
        workspace.validate()?;
        exclude_private_path(workspace, &std::fs::canonicalize(path)?)?;
    }
    parsed.provider.workspace = parsed.workspace;
    parsed.provider.validate()
}

pub fn exclude_private_path(
    workspace: &mut crate::workspace::WorkspaceConfig,
    path: &Path,
) -> anyhow::Result<()> {
    workspace
        .exclude
        .extend([".git".into(), ".env".into(), ".asyntalc".into()]);
    let root = std::fs::canonicalize(&workspace.root)?;
    if let Ok(relative) = path.strip_prefix(root) {
        ensure!(
            !relative.as_os_str().is_empty(),
            "workspace root cannot be the private data directory"
        );
        workspace.exclude.push(
            relative
                .to_str()
                .context("private path must be UTF-8")?
                .to_owned(),
        );
    }
    workspace.exclude.sort();
    workspace.exclude.dedup();
    Ok(())
}

impl Profile {
    pub fn scope(&self) -> serde_json::Value {
        match self {
            Self::Fake => serde_json::json!({"runner":"fake", "workspace":null}),
            Self::Chat(config) => serde_json::json!({
                "runner":"chat", "provider":{"base_url":config.base_url,"model":config.model},
                "ask_parent":config.ask_parent,"workspace":config.workspace,
                "filesystem_semantics":"live_files", "writes":false,"shell":false,
                "workspace_limits":config.workspace.as_ref().map(|_| serde_json::json!({
                    "max_depth":32,"max_scan_bytes":4194304,"max_result_bytes":262144,
                    "max_matching_lines_per_file":100,"max_calls_per_run":16
                })),
                "max_model_requests":if config.workspace.is_some() {25} else {9},
                "max_parent_questions":if config.ask_parent {8} else {0}
            }),
        }
    }
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
