//! The control socket between `kith up` and the other subcommands.
//!
//! One identity means one iroh endpoint: a second process binding the same key
//! would fight the first for its relay slot. So while `up` runs, `live`,
//! `watch` and `status` hand their request to it over a Unix socket (a named
//! pipe on Windows), one JSON line each way, instead of starting a node of
//! their own. Opening Kith a second time shows the first one's window.

use std::{path::Path, sync::Arc};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::warn;

use crate::{
    capture::Share,
    config::Latency,
    link::Link,
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
    /// Show the window, or follow a `kith://` link: what opening Kith again
    /// does.
    Open {
        link: Option<String>,
    },
    /// Quit: the window, the tray icon and the node.
    Quit,
}

/// What the control socket can ask of the app, when Kith runs as one.
pub trait Frontend: Send + Sync {
    /// Shows the window, or brings it forward.
    fn show(&self);
    /// Closes the window and quits.
    fn quit(&self);
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    pub message: String,
}

/// Sends a request to the running node. `None` when no node is running.
pub async fn send(socket: &Path, request: &Request) -> Result<Option<Response>> {
    let stream = match transport::connect(socket).await {
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
        Err(err) => return Err(err).context("connecting to the Kith node"),
    };
    let (read, mut write) = tokio::io::split(stream);
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    write.write_all(line.as_bytes()).await?;
    let mut reply = String::new();
    BufReader::new(read).read_line(&mut reply).await?;
    Ok(Some(
        serde_json::from_str(&reply).context("reading the node's reply")?,
    ))
}

/// Serves requests until the process exits, or until a quit when there's no
/// `frontend` to hand that to (`kith up`).
pub async fn serve(node: Node, socket: &Path, frontend: Option<Arc<dyn Frontend>>) -> Result<()> {
    let mut listener = transport::Listener::bind(socket)
        .with_context(|| format!("binding {}", socket.display()))?;
    let quit = Arc::new(tokio::sync::Notify::new());
    loop {
        let stream = tokio::select! {
            stream = listener.accept() => stream?,
            () = quit.notified() => return Ok(()),
        };
        let (node, frontend, quit) = (node.clone(), frontend.clone(), quit.clone());
        tokio::spawn(async move {
            if let Err(err) = handle(node, stream, frontend, quit).await {
                warn!("control request failed: {err:#}");
            }
        });
    }
}

async fn handle(
    node: Node,
    stream: impl AsyncRead + AsyncWrite,
    frontend: Option<Arc<dyn Frontend>>,
    quit: Arc<tokio::sync::Notify>,
) -> Result<()> {
    let (read, mut write) = tokio::io::split(stream);
    let mut line = String::new();
    BufReader::new(read).read_line(&mut line).await?;
    let request: Request = serde_json::from_str(&line)?;

    let result = match request {
        Request::Live {
            capture: Capture::Stdin,
        } => Err(anyhow::anyhow!(
            "--source stdin only works without `kith up` running"
        )),
        Request::Live { capture } => node
            .start_live(capture, &Share::MainScreen)
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
        Request::Open { link } => open(&node, frontend.as_deref(), link.as_deref()),
        Request::Quit => {
            match &frontend {
                Some(frontend) => frontend.quit(),
                None => quit.notify_one(),
            }
            Ok("Kith is quitting".to_string())
        }
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

/// Follows a link, or shows the window.
fn open(node: &Node, frontend: Option<&dyn Frontend>, link: Option<&str>) -> Result<String> {
    match link.map(str::parse::<Link>).transpose()? {
        Some(Link::Watch(code)) => {
            node.spawn_watch(code.to_string(), None);
            Ok("opening the stream".to_string())
        }
        Some(Link::Open) | None => match frontend {
            Some(frontend) => {
                frontend.show();
                Ok("showing Kith".to_string())
            }
            None => bail!(
                "Kith is already running without its window, as `kith up` in a terminal. \
                 Stop that, then open Kith again"
            ),
        },
    }
}

#[cfg(unix)]
mod transport {
    use std::{
        io,
        path::{Path, PathBuf},
    };

    use tokio::net::{UnixListener, UnixStream};

    pub async fn connect(socket: &Path) -> io::Result<UnixStream> {
        UnixStream::connect(socket).await
    }

    /// Removes its socket file when dropped, which is when `up` exits.
    pub struct Listener {
        listener: UnixListener,
        path: PathBuf,
    }

    impl Listener {
        pub fn bind(socket: &Path) -> io::Result<Self> {
            // Only reached after `send` found nothing listening, so a file here is stale.
            let _ = std::fs::remove_file(socket);
            Ok(Self {
                listener: UnixListener::bind(socket)?,
                path: socket.to_owned(),
            })
        }

        pub async fn accept(&mut self) -> io::Result<UnixStream> {
            Ok(self.listener.accept().await?.0)
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(windows)]
mod transport {
    use std::{ffi::OsString, io, path::Path, time::Duration};

    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    /// All pipe instances are taken: the server is between accepting one client
    /// and opening the next instance, so the wait is short.
    const ERROR_PIPE_BUSY: i32 = 231;

    pub async fn connect(pipe: &Path) -> io::Result<NamedPipeClient> {
        loop {
            match ClientOptions::new().open(pipe) {
                Err(err) if err.raw_os_error() == Some(ERROR_PIPE_BUSY) => {}
                result => return result,
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A pipe server always holds one unconnected instance for the next client.
    pub struct Listener {
        name: OsString,
        next: NamedPipeServer,
    }

    impl Listener {
        pub fn bind(pipe: &Path) -> io::Result<Self> {
            // `first_pipe_instance` fails if another `up` already serves this
            // home; pipes vanish with their process, so nothing is ever stale.
            let next = ServerOptions::new()
                .first_pipe_instance(true)
                .create(pipe)?;
            Ok(Self {
                name: pipe.as_os_str().to_owned(),
                next,
            })
        }

        pub async fn accept(&mut self) -> io::Result<NamedPipeServer> {
            self.next.connect().await?;
            let fresh = ServerOptions::new().create(&self.name)?;
            Ok(std::mem::replace(&mut self.next, fresh))
        }
    }
}
