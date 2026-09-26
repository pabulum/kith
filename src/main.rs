//! pstream: stream your screen to friends, peer to peer.
//!
//! See MANIFEST.md for where this is going and README.md for how to use it.

mod config;
mod control;
mod node;

use std::{net::SocketAddr, path::PathBuf, str::FromStr};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use iroh::EndpointId;
use tracing_subscriber::EnvFilter;

use crate::{
    config::{Friend, Home, Latency},
    control::{Request, Response},
    node::{Capture, Node, Role, Sink},
};

#[derive(Parser)]
#[command(version, about = "Stream your screen to friends, peer to peer")]
struct Cli {
    /// State directory (identity, friends, settings). Two homes are two identities.
    #[arg(long, env = "PSTREAM_HOME", global = true)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print your code, which friends add to reach you.
    Id,
    /// Add, remove, and list friends.
    #[command(subcommand)]
    Friend(FriendCommand),
    /// Stay reachable: get a notification (or the player) when a friend goes live.
    Up,
    /// Stream to your friends.
    Live {
        /// What to stream.
        #[arg(long, value_enum, default_value = "screen")]
        source: Capture,
        /// Stop the stream `pstream up` is running.
        #[arg(long, conflicts_with = "source")]
        stop: bool,
    },
    /// Watch a friend's stream.
    Watch {
        /// Friend name, or a raw code.
        who: String,
        #[arg(long, value_enum)]
        latency: Option<Latency>,
        /// Write the MPEG-TS to a file instead of opening the player.
        #[arg(long)]
        output: Option<PathBuf>,
        /// Serve the stream over HTTP at this address (e.g. 127.0.0.1:8080) for
        /// one player to open, instead of starting one.
        #[arg(long, conflicts_with = "output")]
        serve: Option<SocketAddr>,
    },
    /// Show who's online and who's live.
    Status,
}

#[derive(Subcommand)]
enum FriendCommand {
    /// Add a friend by the code `pstream id` printed for them.
    Add {
        name: String,
        code: String,
        /// Open the player as soon as they go live, instead of notifying.
        #[arg(long)]
        auto_open: bool,
    },
    /// Remove a friend.
    Rm { name: String },
    /// List friends.
    Ls,
    /// Change a friend's settings.
    Set {
        name: String,
        #[arg(long)]
        auto_open: bool,
        #[arg(long, conflicts_with = "auto_open")]
        no_auto_open: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("pstream=info,warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let home = Home::resolve(cli.home)?;
    let socket = home.socket_path();

    match cli.command {
        Command::Id => println!("{}", home.secret()?.public()),
        Command::Friend(command) => friend(&home, command).await?,
        Command::Up => up(home).await?,
        Command::Live { stop: true, .. } => match control::send(&socket, &Request::Stop).await? {
            Some(response) => report(response)?,
            None => {
                bail!("`pstream up` isn't running; a foreground `pstream live` stops with Ctrl-C")
            }
        },
        Command::Live { source, .. } => {
            match control::send(&socket, &Request::Live { capture: source }).await? {
                Some(response) => report(response)?,
                None => live(home, source).await?,
            }
        }
        Command::Watch {
            who,
            latency,
            output,
            serve,
        } if output.is_some() || serve.is_some() => {
            // These sinks run in the foreground so scripts (or a phone shell) can
            // wait on them, which needs this process to own the endpoint.
            if control::send(&socket, &Request::Status).await?.is_some() {
                bail!("--output and --serve need their own node; stop `pstream up` first");
            }
            let sink = match (output, serve) {
                (Some(output), _) => Sink::File(output),
                (None, Some(addr)) => Sink::Serve(addr),
                (None, None) => unreachable!("guarded above"),
            };
            let node = Node::start(home, Role::Watch).await?;
            let result = tokio::select! {
                result = node.watch(&who, latency, sink) => result,
                () = shutdown_signal() => Ok(()),
            };
            node.shutdown().await;
            result?;
        }
        Command::Watch { who, latency, .. } => {
            let request = Request::Watch {
                who: who.clone(),
                latency,
            };
            match control::send(&socket, &request).await? {
                Some(response) => report(response)?,
                None => {
                    let node = Node::start(home, Role::Watch).await?;
                    let result = tokio::select! {
                        result = node.watch(&who, latency, Sink::Player) => result,
                        () = shutdown_signal() => Ok(()),
                    };
                    node.shutdown().await;
                    result?;
                }
            }
        }
        Command::Status => match control::send(&socket, &Request::Status).await? {
            Some(response) => report(response)?,
            None => {
                println!("code: {}", home.secret()?.public());
                println!("pstream isn't running; `pstream up` keeps you reachable");
            }
        },
    }
    Ok(())
}

/// Ctrl-C, or SIGTERM from systemd or `kill`: both end cleanly, so viewers see
/// the stream finish rather than fail.
async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing a SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

fn report(response: Response) -> Result<()> {
    if response.ok {
        println!("{}", response.message.trim_end());
        Ok(())
    } else {
        bail!("{}", response.message)
    }
}

async fn up(home: Home) -> Result<()> {
    let socket = home.socket_path();
    if control::send(&socket, &Request::Status).await?.is_some() {
        bail!("pstream is already running for {}", home.dir().display());
    }
    let node = Node::start(home, Role::Up).await?;
    println!("pstream is up. Your code: {}", node.id());
    println!(
        "Friends add you with: pstream friend add <your name> {}",
        node.id()
    );

    let result = tokio::select! {
        result = control::serve(node.clone(), &socket) => result,
        () = shutdown_signal() => Ok(()),
    };
    node.shutdown().await;
    let _ = std::fs::remove_file(&socket);
    result
}

async fn live(home: Home, capture: Capture) -> Result<()> {
    let node = Node::start(home, Role::Live).await?;
    let mut live = node.go_live(capture)?;
    println!(
        "Live as {}. Friends who are online are being told. Ctrl-C to stop.",
        node.id()
    );
    let result = tokio::select! {
        result = live.finished() => result,
        () = shutdown_signal() => live.stop().await,
    };
    node.shutdown().await;
    result
}

async fn friend(home: &Home, command: FriendCommand) -> Result<()> {
    let mut config = home.config()?;
    match command {
        FriendCommand::Add {
            name,
            code,
            auto_open,
        } => {
            let id = EndpointId::from_str(code.trim()).with_context(|| {
                format!("{code:?} isn't a pstream code (`pstream id` prints one)")
            })?;
            if id == home.secret()?.public() {
                bail!("that's your own code");
            }
            if let Some(existing) = config.friends.iter().find(|f| f.name == name) {
                bail!("you already have a friend named {:?}", existing.name);
            }
            if let Some(existing) = config.friends.iter().find(|f| f.id().ok() == Some(id)) {
                bail!("that code is already saved as {:?}", existing.name);
            }
            config.friends.push(Friend {
                name: name.clone(),
                code: id.to_string(),
                auto_open,
            });
            home.save(&config)?;
            println!("added {name}");
        }
        FriendCommand::Rm { name } => {
            let before = config.friends.len();
            config.friends.retain(|f| f.name != name);
            if config.friends.len() == before {
                bail!("no friend named {name:?}");
            }
            home.save(&config)?;
            println!("removed {name}");
        }
        FriendCommand::Ls => {
            if config.friends.is_empty() {
                println!("no friends yet; `pstream friend add <name> <code>`");
            }
            for friend in &config.friends {
                let auto = if friend.auto_open { "  auto-open" } else { "" };
                println!("{:<16} {}{auto}", friend.name, friend.code);
            }
            return Ok(());
        }
        FriendCommand::Set {
            name,
            auto_open,
            no_auto_open,
        } => {
            let friend = config
                .friends
                .iter_mut()
                .find(|f| f.name == name)
                .with_context(|| format!("no friend named {name:?}"))?;
            if auto_open || no_auto_open {
                friend.auto_open = auto_open;
            }
            let state = if friend.auto_open { "on" } else { "off" };
            home.save(&config)?;
            println!("{name}: auto-open {state}");
        }
    }
    // A running node gates connections on the friends list, so it has to hear about changes.
    if let Some(response) = control::send(&home.socket_path(), &Request::Reload).await? {
        report(response)?;
    }
    Ok(())
}
