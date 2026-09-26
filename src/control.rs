//! The control socket between `pstream up` and the other subcommands.
//!
//! One identity means one iroh endpoint: a second process binding the same key
//! would fight the first for its relay slot. So while `up` runs, `live`,
//! `watch` and `status` hand their request to it over a Unix socket (one JSON
//! line each way) instead of starting a node of their own.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
};
use tracing::warn;

use crate::{
    config::Latency,
    node::{Capture, Node},
};

#[derive(Debug, Serialize, Deserialize)]
pub enum Request {
    Live {
        capture: Capture,
    },
    Stop,
    Watch {
        who: String,
        latency: Option<Latency>,
    },
    Status,
    Reload,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub message: String,
}

/// Sends a request to the running node. `None` when no node is running.
pub async fn send(socket: &Path, request: &Request) -> Result<Option<Response>> {
    let stream = match UnixStream::connect(socket).await {
        Ok(stream) => stream,
        // A stale socket file from a crashed node refuses; treat it as "not running".
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(err) => return Err(err).context("connecting to the pstream node"),
    };
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    let mut reply = String::new();
    BufReader::new(read).read_line(&mut reply).await?;
    Ok(Some(
        serde_json::from_str(&reply).context("reading the node's reply")?,
    ))
}

/// Serves requests until the process exits.
pub async fn serve(node: Node, socket: &Path) -> Result<()> {
    // Only reached after `send` found nothing listening, so a file here is stale.
    let _ = std::fs::remove_file(socket);
    let listener =
        UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
    loop {
        let (stream, _) = listener.accept().await?;
        let node = node.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(node, stream).await {
                warn!("control request failed: {err:#}");
            }
        });
    }
}

async fn handle(node: Node, stream: UnixStream) -> Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    let request: Request = serde_json::from_str(&line)?;

    let result = match request {
        Request::Live {
            capture: Capture::Stdin,
        } => Err(anyhow::anyhow!(
            "--source stdin only works without `pstream up` running"
        )),
        Request::Live { capture } => node
            .start_live(capture)
            .map(|()| "live; friends who are online are being told".to_string()),
        Request::Stop => node
            .stop_live()
            .await
            .map(|stopped| if stopped { "stopped" } else { "wasn't live" }.to_string()),
        Request::Watch { who, latency } => {
            node.spawn_watch(who.clone(), latency);
            Ok(format!("opening {who}'s stream"))
        }
        Request::Status => Ok(node.status()),
        Request::Reload => node.reload().map(|()| "reloaded".to_string()),
    };
    let response = match result {
        Ok(message) => Response { ok: true, message },
        Err(err) => Response {
            ok: false,
            message: format!("{err:#}"),
        },
    };
    let mut reply = serde_json::to_string(&response)?;
    reply.push('\n');
    write.write_all(reply.as_bytes()).await?;
    Ok(())
}
