use std::process::Stdio;

use anyhow::Context;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RpcRequest {
    pub method: String,
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RpcResponse {
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Low-level helpers
// ---------------------------------------------------------------------------

/// Write a length-prefixed JSON message to any `AsyncWrite`.
async fn send_message<W>(writer: &mut W, obj: &impl serde::Serialize) -> anyhow::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let data = serde_json::to_vec(obj).context("serialize RPC request")?;
    let len = u32::try_from(data.len()).context("message too large")?;
    writer
        .write_all(&len.to_le_bytes())
        .await
        .context("write length prefix")?;
    writer.write_all(&data).await.context("write message body")?;
    writer.flush().await.context("flush stdin")?;
    Ok(())
}

/// Read a length-prefixed message from any `AsyncRead`.
async fn read_message<R>(reader: &mut R) -> anyhow::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .context("read length prefix")?;
    let length = u32::from_le_bytes(len_buf) as usize;
    let mut body = vec![0u8; length];
    reader
        .read_exact(&mut body)
        .await
        .context("read message body")?;
    Ok(body)
}

// ---------------------------------------------------------------------------
// Sidecar handle
// ---------------------------------------------------------------------------

pub struct Sidecar {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

impl Sidecar {
    /// Spawn a Python sidecar process for `script_path`.
    ///
    /// Runs `python3 <script_path>` — never uses shell=true.
    pub async fn spawn(script_path: &str) -> anyhow::Result<Self> {
        debug!("spawning sidecar: python3 {script_path}");
        let mut child = tokio::process::Command::new("python3")
            .arg(script_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("spawn python3 sidecar: {script_path}"))?;

        let stdin = child.stdin.take().context("child stdin missing")?;
        let stdout_raw = child.stdout.take().context("child stdout missing")?;
        let stdout = BufReader::new(stdout_raw);

        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    /// Send `method`/`params`, return the result value or propagate the error.
    pub async fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let request = RpcRequest {
            method: method.to_owned(),
            params,
        };
        send_message(&mut self.stdin, &request).await?;

        let body = read_message(&mut self.stdout).await?;
        let resp: RpcResponse =
            serde_json::from_slice(&body).context("deserialize RPC response")?;

        if let Some(err) = resp.error {
            anyhow::bail!("sidecar error from method '{method}': {err}");
        }
        resp.result
            .ok_or_else(|| anyhow::anyhow!("sidecar returned null result for '{method}'"))
    }

    /// Gracefully terminate the sidecar by sending a `shutdown` request, then
    /// waiting for the process to exit.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        let request = RpcRequest {
            method: "shutdown".to_owned(),
            params: serde_json::Value::Null,
        };
        if let Err(e) = send_message(&mut self.stdin, &request).await {
            warn!("could not send shutdown to sidecar: {e}");
        }
        // Drain the response (best-effort).
        let _ = read_message(&mut self.stdout).await;
        self.child
            .wait()
            .await
            .context("waiting for sidecar process")?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SidecarPool — lazily spawns and reuses a single sidecar instance.
// ---------------------------------------------------------------------------

/// A thin wrapper that lazily spawns a sidecar on first use and reuses it.
///
/// If the sidecar dies mid-session it is automatically respawned before the
/// next call.
pub struct SidecarPool {
    script_path: String,
    instance: tokio::sync::Mutex<Option<Sidecar>>,
}

impl SidecarPool {
    pub fn new(script_path: String) -> Self {
        Self {
            script_path,
            instance: tokio::sync::Mutex::new(None),
        }
    }

    /// Call a method on the pooled sidecar, respawning if necessary.
    pub async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let mut guard = self.instance.lock().await;

        // Ensure we have a live sidecar.
        if guard.is_none() {
            *guard = Some(Sidecar::spawn(&self.script_path).await?);
        }

        match guard.as_mut().unwrap().call(method, params.clone()).await {
            Ok(v) => Ok(v),
            Err(e) => {
                // The sidecar may have crashed — drop it and respawn once.
                warn!("sidecar call failed ({e}), respawning and retrying");
                *guard = None;
                let mut fresh = Sidecar::spawn(&self.script_path).await?;
                let result = fresh.call(method, params).await;
                *guard = Some(fresh);
                result
            }
        }
    }
}
