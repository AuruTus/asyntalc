use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 1024 * 1024;
pub const MAX_INPUT: usize = 64 * 1024;
pub fn default_run_timeout_ms() -> u64 {
    600_000
}
pub const MAX_RUN_TIMEOUT_MS: u64 = 86_400_000;
pub fn default_page_limit() -> u32 {
    50
}
pub const MAX_PAGE_LIMIT: u32 = 100;
pub const RUN_STATUSES: [&str; 7] = [
    "queued",
    "running",
    "waiting_for_parent",
    "completed",
    "failed",
    "cancelled",
    "timed_out",
];
pub const MAX_WAIT_MS: u64 = 30_000;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub protocol_version: u32,
    pub request_id: String,
    pub operation: Operation,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Ping,
    Scope,
    Tools {
        run_id: String,
        #[serde(default)]
        after: i64,
        #[serde(default = "default_page_limit")]
        limit: u32,
    },
    List {
        session_id: Option<String>,
        status: Option<String>,
        #[serde(default)]
        after: i64,
        #[serde(default = "default_page_limit")]
        limit: u32,
    },
    Logs {
        run_id: String,
        #[serde(default)]
        after_seq: i64,
        #[serde(default = "default_page_limit")]
        limit: u32,
    },
    Submit {
        session_id: Option<String>,
        input: String,
        #[serde(default = "default_run_timeout_ms")]
        run_timeout_ms: u64,
        idempotency_key: Option<String>,
    },
    Resume {
        run_id: String,
        question_id: String,
        input: String,
    },
    Cancel {
        run_id: String,
    },
    Status {
        run_id: String,
    },
    Wait {
        run_id: String,
        timeout_ms: u64,
    },
    Result {
        run_id: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
pub struct Response {
    pub protocol_version: u32,
    pub request_id: Option<String>,
    pub ok: bool,
    #[serde(flatten)]
    pub body: serde_json::Map<String, Value>,
}

impl Response {
    pub fn success(request_id: &str, body: Value) -> Self {
        Self {
            protocol_version: VERSION,
            request_id: Some(request_id.into()),
            ok: true,
            body: body
                .as_object()
                .expect("response body is an object")
                .clone(),
        }
    }

    pub fn error(request_id: Option<&str>, code: &str, message: &str) -> Self {
        Self {
            protocol_version: VERSION,
            request_id: request_id.map(str::to_owned),
            ok: false,
            body: json!({"error": {"code": code, "message": message}})
                .as_object()
                .unwrap()
                .clone(),
        }
    }
}

// Limit allocation before accepting a newline, including malicious unterminated frames.
pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        anyhow::ensure!(!available.is_empty(), "connection closed before newline");
        let end = available.iter().position(|&b| b == b'\n');
        let count = end.map_or(available.len(), |i| i + 1);
        anyhow::ensure!(frame.len() + count <= MAX_FRAME, "frame exceeds size limit");
        frame.extend_from_slice(&available[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(frame);
        }
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &impl Serialize,
) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    anyhow::ensure!(bytes.len() <= MAX_FRAME, "response exceeds size limit");
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}
