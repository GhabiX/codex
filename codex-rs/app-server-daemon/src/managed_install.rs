use std::path::Path;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Stdio;
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use tokio::io::AsyncWriteExt;

#[cfg(unix)]
use anyhow::Context;
#[cfg(unix)]
use anyhow::Result;
#[cfg(unix)]
use anyhow::anyhow;
#[cfg(unix)]
use sha2::Digest;
#[cfg(unix)]
use sha2::Sha256;
#[cfg(unix)]
use tokio::fs;
#[cfg(unix)]
use tokio::process::Command;

pub(crate) fn managed_codex_bin(codex_home: &Path) -> PathBuf {
    codex_home
        .join("packages")
        .join("standalone")
        .join("current")
        .join(managed_codex_file_name())
}

#[cfg(unix)]
pub(crate) async fn resolved_managed_codex_bin(codex_bin: &Path) -> Result<PathBuf> {
    fs::canonicalize(codex_bin).await.with_context(|| {
        format!(
            "failed to resolve managed Codex binary {}",
            codex_bin.display()
        )
    })
}

#[cfg(unix)]
pub(crate) async fn managed_codex_version(codex_bin: &Path) -> Result<String> {
    // MCP initialization reports the product identity in both legacy Spine and current
    // packages. The public CLI --version intentionally reports upstream compatibility.
    let mut child = Command::new(codex_bin)
        .arg("mcp-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "failed to probe managed Codex binary {}",
                codex_bin.display()
            )
        })?;
    let request = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "codex_app_server_daemon", "version": env!("CARGO_PKG_VERSION") }
        }
    });
    let output = tokio::time::timeout(Duration::from_secs(10), async move {
        let mut stdin = child
            .stdin
            .take()
            .context("product probe stdin was not piped")?;
        stdin.write_all(format!("{request}\n").as_bytes()).await?;
        drop(stdin);
        child.wait_with_output().await.map_err(anyhow::Error::from)
    })
    .await
    .context("timed out reading managed Codex product identity")??;
    if !output.status.success() {
        return Err(anyhow!(
            "managed Codex binary {} exited with status {}",
            codex_bin.display(),
            output.status
        ));
    }

    let stdout = String::from_utf8(output.stdout).with_context(|| {
        format!(
            "managed Codex version was not utf-8: {}",
            codex_bin.display()
        )
    })?;
    let responses = stdout
        .lines()
        .map(serde_json::from_str::<serde_json::Value>)
        .collect::<serde_json::Result<Vec<_>>>()
        .context("managed Codex product probe returned malformed JSON-RPC")?;
    responses
        .iter()
        .find(|response| response["id"] == 1)
        .and_then(|response| response.pointer("/result/serverInfo/version"))
        .and_then(serde_json::Value::as_str)
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
        .context("managed Codex MCP initialization did not return a product version")
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecutableIdentity {
    digest: [u8; 32],
}

#[cfg(unix)]
pub(crate) async fn executable_identity(executable: &Path) -> Result<ExecutableIdentity> {
    let bytes = fs::read(executable)
        .await
        .with_context(|| format!("failed to read executable {}", executable.display()))?;
    Ok(executable_identity_from_bytes(&bytes))
}

#[cfg(unix)]
pub(crate) fn executable_identity_from_bytes(bytes: &[u8]) -> ExecutableIdentity {
    ExecutableIdentity {
        digest: Sha256::digest(bytes).into(),
    }
}

fn managed_codex_file_name() -> &'static str {
    if cfg!(windows) { "codex.exe" } else { "codex" }
}

#[cfg(all(test, unix))]
#[path = "managed_install_tests.rs"]
mod tests;
