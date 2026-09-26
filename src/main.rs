//! pstream: stream your screen to friends, peer to peer.
//!
//! See MANIFEST.md for where this is going and README.md for how to use it.

// Release builds on Windows are GUI programs, so double-clicking one opens no
// console window. `attach_console` gives subcommands their terminal back.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
mod control;
#[cfg(not(target_os = "android"))]
mod gui;
mod node;

use std::{net::SocketAddr, path::PathBuf, sync::Mutex};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, fmt::writer::MakeWriterExt};

use crate::{
    config::{Home, Latency},
    control::{Request, Response},
    node::{Capture, Node, Role, Sink},
};

#[derive(Parser)]
#[command(
    version,
    about = "Stream your screen to friends, peer to peer",
    long_about = "Stream your screen to friends, peer to peer.\n\n\
                  With no command, pstream opens its window, which keeps you reachable \
                  like `pstream up`."
)]
struct Cli {
    /// State directory (identity, friends, settings). Two homes are two identities.
    #[arg(long, env = "PSTREAM_HOME", global = true)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
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

fn main() -> Result<()> {
    #[cfg(windows)]
    attach_console();
    let cli = Cli::parse();
    let home = Home::resolve(cli.home)?;
    let runtime = tokio::runtime::Runtime::new().context("starting the async runtime")?;
    match cli.command {
        Some(command) => {
            init_logging(None);
            runtime.block_on(run(home, command))
        }
        None => app(runtime, home),
    }
}

#[cfg(not(target_os = "android"))]
fn app(runtime: tokio::runtime::Runtime, home: Home) -> Result<()> {
    // A window has no terminal to read on Windows, so the log also goes to a
    // file a friend can send when something breaks, and panics go in it too.
    let log = home.dir().join("pstream.log");
    // The previous run's log survives one restart, which is usually when
    // someone goes looking for it (and it holds the crash an OpenGL retry
    // follows).
    let _ = std::fs::rename(&log, log.with_extension("log.old"));
    init_logging(std::fs::File::create(&log).ok());
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        tracing::error!("{info}");
        default_hook(info);
    }));

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| gui::run(runtime, home)))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("pstream crashed")));
    let Err(err) = result else { return Ok(()) };
    tracing::error!("{err:#}");
    // Most window failures are the GPU driver refusing DX12 or Vulkan, and
    // OpenGL often works where they don't. winit can't open a second event
    // loop in one process, so the retry is a new one. By now the runtime is
    // gone, and with it this node's endpoint and control socket.
    if std::env::var_os("WGPU_BACKEND").is_none() {
        let retry = std::env::current_exe().and_then(|exe| {
            std::process::Command::new(exe)
                .args(std::env::args_os().skip(1))
                .env("WGPU_BACKEND", "gl")
                .spawn()
        });
        match retry {
            Ok(_) => {
                tracing::info!("trying again with OpenGL");
                return Ok(());
            }
            Err(retry_err) => tracing::error!("couldn't retry with OpenGL: {retry_err}"),
        }
    }
    #[cfg(windows)]
    message_box(&format!("{err:#}\n\nThe log is {}", log.display()));
    Err(err)
}

/// Shows an error where a double-clicked program can: nothing else is on
/// screen when the window fails to open.
#[cfg(windows)]
fn message_box(text: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let (text, title) = (wide(text), wide("pstream"));
    // SAFETY: both strings are NUL-terminated and outlive the call.
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            text.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        )
    };
}

#[cfg(target_os = "android")]
fn app(_: tokio::runtime::Runtime, _: Home) -> Result<()> {
    use clap::CommandFactory;
    Cli::command().print_help()?;
    Ok(())
}

/// Logs to stderr, and to `file` as well when given.
fn init_logging(file: Option<std::fs::File>) {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("pstream=info,warn"));
    let subscriber = tracing_subscriber::fmt().with_env_filter(filter);
    match file {
        Some(file) => subscriber
            .with_ansi(false)
            .with_writer(std::io::stderr.and(Mutex::new(file)))
            .init(),
        None => subscriber.with_writer(std::io::stderr).init(),
    }
}

/// A GUI-subsystem program starts with no console. Attaching to the one it was
/// run from (if any) lets `pstream status` and friends print there; it fails
/// harmlessly on a double-click, or when a console build already has one.
#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // SAFETY: AttachConsole takes a process id and touches no memory of ours.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
}

async fn run(home: Home, command: Command) -> Result<()> {
    let socket = home.socket_path();
    match command {
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
#[cfg(unix)]
async fn shutdown_signal() {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("installing a SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

/// Ctrl-C, or the console window closing (Windows allows a few seconds to
/// finish): both end cleanly, so viewers see the stream finish rather than fail.
#[cfg(windows)]
async fn shutdown_signal() {
    let mut close = tokio::signal::windows::ctrl_close().expect("installing a close handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = close.recv() => {}
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
            config.add_friend(&name, &code, auto_open, home.secret()?.public())?;
            home.save(&config)?;
            println!("added {name}");
        }
        FriendCommand::Rm { name } => {
            config.remove_friend(&name)?;
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
            let friend = config.friend_mut(&name)?;
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
