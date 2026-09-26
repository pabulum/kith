//! The node: one iroh endpoint and one MoQ origin per identity.
//!
//! Every friend session is bidirectional and shares the node origin, so the
//! announcement of our `live/<code>` broadcast *is* the "going live" push. A
//! friend's node sees it arrive on the session it already holds (or on the one
//! we dial when we go live) and either notifies or opens the player. No server
//! and no extra signalling: the relay connection iroh keeps for reachability
//! is what lets us dial a friend at all.

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use bytes::BytesMut;
use iroh::{
    Endpoint, EndpointId,
    endpoint::{Connection, presets},
    protocol::{AcceptError, ProtocolHandler, Router},
};
use iroh_moq::{IncomingSessionStream, Moq, MoqProtocolHandler, MoqSession};
use moq_mux::import::{ContainerFormat, ContainerStream};
use moq_net::announce::{Kind as AnnounceKind, Update as AnnounceUpdate};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::{Child, Command},
    sync::oneshot,
    task::JoinHandle,
};
use tracing::{debug, info, warn};

use crate::config::{Config, Friend, Home, Latency};

const LIVE_PREFIX: &str = "live/";
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `watch` waits for a friend's broadcast to be announced.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(10);
/// Offline friends are retried this often, so a friend who comes online hears
/// about a stream that's already running.
const REDIAL_INTERVAL: Duration = Duration::from_secs(30);

/// ffmpeg's test pattern and a tone, 1 s GOPs, as MPEG-TS on stdout.
const TEST_CAPTURE: &[&str] = &[
    "ffmpeg",
    "-hide_banner",
    "-loglevel",
    "error",
    "-re",
    "-f",
    "lavfi",
    "-i",
    "testsrc2=size=1280x720:rate=30",
    "-f",
    "lavfi",
    "-i",
    "sine=frequency=440:sample_rate=48000",
    "-c:v",
    "libx264",
    "-preset",
    "veryfast",
    "-tune",
    "zerolatency",
    "-g",
    "30",
    "-pix_fmt",
    "yuv420p",
    "-c:a",
    "aac",
    "-b:a",
    "128k",
    "-f",
    "mpegts",
    "-",
];

fn live_path(id: EndpointId) -> String {
    format!("{LIVE_PREFIX}{id}")
}

/// A byte stream feeding the MPEG-TS importer.
type Input = Pin<Box<dyn AsyncRead + Send>>;

/// Friends by endpoint id, shared with the connection gate.
type Friends = Arc<RwLock<HashMap<EndpointId, Friend>>>;

fn index(friends: &[Friend]) -> Result<HashMap<EndpointId, Friend>> {
    friends
        .iter()
        .map(|friend| Ok((friend.id()?, friend.clone())))
        .collect()
}

/// Where `pstream live` reads MPEG-TS from.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Capture {
    /// The `capture` command from config.toml (gpu-screen-recorder by default).
    Screen,
    /// ffmpeg's test pattern and tone; no screen picker involved.
    Test,
    /// MPEG-TS piped into pstream's own stdin.
    Stdin,
}

/// What this process is for, which decides how much of the node runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// `pstream up`: keeps sessions to every friend and reacts when they go live.
    Up,
    /// A foreground `pstream live`: dials friends so they hear about the stream,
    /// but doesn't pop players for *their* streams.
    Live,
    /// A foreground `pstream watch`: dials only the friend being watched.
    Watch,
}

/// Where a watched stream goes.
pub enum Sink {
    Player,
    File(PathBuf),
    /// One HTTP client at this address, for players pstream can't spawn (an
    /// Android app, when pstream runs in a shell on the phone).
    Serve(SocketAddr),
}

#[derive(Clone)]
pub struct Node(Arc<Inner>);

struct Inner {
    role: Role,
    home: Home,
    endpoint: Endpoint,
    moq: Moq,
    router: Router,
    config: RwLock<Config>,
    friends: Friends,
    sessions: Mutex<HashMap<EndpointId, MoqSession>>,
    /// Friends whose broadcast is currently announced to us.
    live_friends: Mutex<HashSet<EndpointId>>,
    /// Broadcast paths with a player (or file) attached.
    watching: Mutex<HashSet<String>>,
    live: Mutex<Option<LiveHandle>>,
}

impl Node {
    pub async fn start(home: Home, role: Role) -> Result<Self> {
        let config = home.config()?;
        let friends: Friends = Arc::new(RwLock::new(index(&config.friends)?));
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(home.secret()?)
            .bind()
            .await
            .context("binding the iroh endpoint")?;
        // Until a relay connection is up, friends behind NAT can't reach us and
        // our address isn't published, so a friend dialing right after we print
        // "live" would fail.
        if tokio::time::timeout(ONLINE_TIMEOUT, endpoint.online())
            .await
            .is_err()
        {
            warn!("no relay connection yet; friends may not be able to reach you");
        }
        let moq = Moq::new(endpoint.clone());
        // Subscribed before the router starts accepting, so no session is missed.
        let incoming = moq.incoming_sessions();
        let gate = Gate {
            inner: moq.protocol_handler(),
            friends: friends.clone(),
        };
        let router = iroh_moq::alpns()
            .into_iter()
            .fold(Router::builder(endpoint.clone()), |router, alpn| {
                router.accept(alpn, gate.clone())
            })
            .spawn();

        let node = Self(Arc::new(Inner {
            role,
            home,
            endpoint,
            moq,
            router,
            config: RwLock::new(config),
            friends,
            sessions: Mutex::default(),
            live_friends: Mutex::default(),
            watching: Mutex::default(),
            live: Mutex::default(),
        }));
        tokio::spawn(node.clone().run_sessions(incoming));
        if role != Role::Watch {
            tokio::spawn(node.clone().redial());
        }
        Ok(node)
    }

    pub fn id(&self) -> EndpointId {
        self.0.endpoint.id()
    }

    fn config(&self) -> Config {
        self.0.config.read().unwrap().clone()
    }

    fn friend(&self, id: EndpointId) -> Option<Friend> {
        self.0.friends.read().unwrap().get(&id).cloned()
    }

    /// Re-reads config.toml, picking up friends added or removed since start.
    pub fn reload(&self) -> Result<()> {
        let config = self.0.home.config()?;
        *self.0.friends.write().unwrap() = index(&config.friends)?;
        *self.0.config.write().unwrap() = config;
        self.dial_friends();
        Ok(())
    }

    pub async fn shutdown(&self) {
        let live = self.0.live.lock().unwrap().take();
        if let Some(live) = live
            && let Err(err) = live.stop().await
        {
            warn!("stream ended with an error: {err:#}");
        }
        self.0.moq.shutdown().await;
        if let Err(err) = self.0.router.shutdown().await {
            debug!("router shutdown: {err}");
        }
    }

    // --- Publishing -------------------------------------------------------

    /// Starts publishing `capture` as `live/<our code>` and tells every
    /// reachable friend.
    pub fn go_live(&self, capture: Capture) -> Result<LiveHandle> {
        let (input, child) = self.open_capture(capture)?;
        let path = live_path(self.id());
        let mut broadcast = self
            .0
            .moq
            .origin()
            .create_broadcast(&path)
            .context("creating the broadcast (already live?)")?;
        let catalog = moq_mux::catalog::Producer::new(&mut broadcast, Default::default())?;
        let import =
            ContainerStream::new(broadcast.clone(), catalog.reserve(), ContainerFormat::Ts)?;
        // Announced only once the catalog tracks exist, so a friend who
        // subscribes the moment the announcement lands finds them.
        broadcast.announce(Default::default())?;
        info!("live as {path}");

        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(pump(input, child, import, catalog, broadcast, stopped));
        // Sessions we already hold carry the announcement; dial everyone else
        // so it reaches them now rather than on their next redial.
        self.dial_friends();
        Ok(LiveHandle {
            stop: Some(stop),
            task,
        })
    }

    /// Daemon flavour of [`go_live`](Self::go_live): the handle lives in the node.
    pub fn start_live(&self, capture: Capture) -> Result<()> {
        let mut live = self.0.live.lock().unwrap();
        if live.as_ref().is_some_and(|live| !live.task.is_finished()) {
            bail!("already live; `pstream live --stop` first");
        }
        *live = Some(self.go_live(capture)?);
        Ok(())
    }

    pub async fn stop_live(&self) -> Result<bool> {
        let live = self.0.live.lock().unwrap().take();
        match live {
            Some(live) => live.stop().await.map(|()| true),
            None => Ok(false),
        }
    }

    fn open_capture(&self, capture: Capture) -> Result<(Input, Option<Child>)> {
        let argv: Vec<String> = match capture {
            Capture::Stdin => return Ok((Box::pin(tokio::io::stdin()), None)),
            Capture::Screen => self.config().capture,
            Capture::Test => TEST_CAPTURE.iter().map(|arg| arg.to_string()).collect(),
        };
        let (program, args) = argv.split_first().context("the capture command is empty")?;
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| spawn_error(program, "capture", err))?;
        let stdout = child.stdout.take().expect("stdout is piped");
        Ok((Box::pin(stdout), Some(child)))
    }

    // --- Watching ---------------------------------------------------------

    /// Connects to a friend and plays their stream until it ends or the
    /// player closes.
    pub async fn watch(&self, who: &str, latency: Option<Latency>, sink: Sink) -> Result<()> {
        let config = self.config();
        let (name, id) = config.resolve(who)?;
        let session = self.connect(id).await.map_err(|err| {
            anyhow!("couldn't reach {name} (offline, or they haven't added you?): {err:#}")
        })?;
        let path = live_path(id);
        // `subscribe` rides out the announcement arriving after the handshake.
        tokio::time::timeout(ANNOUNCE_TIMEOUT, session.subscribe(path.as_str()))
            .await
            .map_err(|_| anyhow!("{name} isn't live"))?
            .map_err(|err| anyhow!("{name} isn't live: {err}"))?;
        self.play(session, path, name, latency.unwrap_or(config.latency), sink)
            .await
    }

    /// Dials a friend, retrying for a while.
    ///
    /// Retries because a dial by bare id rides on address lookup (n0's DNS), and
    /// a friend whose node started a moment ago may not be published yet.
    async fn connect(&self, id: EndpointId) -> Result<MoqSession> {
        let deadline = tokio::time::Instant::now() + DIAL_TIMEOUT;
        let mut backoff = Duration::from_millis(500);
        loop {
            let attempt = tokio::time::timeout_at(deadline, self.0.moq.connect(id)).await;
            match attempt {
                Ok(Ok(session)) => return Ok(session),
                Ok(Err(err)) if tokio::time::Instant::now() + backoff < deadline => {
                    debug!("dial failed, retrying: {err}");
                    tokio::time::sleep(backoff).await;
                    backoff *= 2;
                }
                Ok(Err(err)) => return Err(err.into()),
                Err(_) => bail!("timed out"),
            }
        }
    }

    /// Spawns a watch; for the daemon, which answers before the stream ends.
    pub fn spawn_watch(&self, who: String, latency: Option<Latency>) {
        let node = self.clone();
        tokio::spawn(async move {
            if let Err(err) = node.watch(&who, latency, Sink::Player).await {
                warn!("watching {who}: {err:#}");
            }
        });
    }

    async fn play(
        &self,
        session: MoqSession,
        path: String,
        name: String,
        latency: Latency,
        sink: Sink,
    ) -> Result<()> {
        if !self.0.watching.lock().unwrap().insert(path.clone()) {
            bail!("already watching {name}");
        }
        let _watching = Watching {
            node: self.clone(),
            path: path.clone(),
        };

        let mut output: Pin<Box<dyn tokio::io::AsyncWrite + Send>>;
        let mut player = None;
        match sink {
            Sink::File(file) => {
                output = Box::pin(
                    tokio::fs::File::create(&file)
                        .await
                        .with_context(|| format!("creating {}", file.display()))?,
                );
            }
            Sink::Player => {
                let argv = player_argv(&self.config().player, latency, &name)?;
                let mut child = Command::new(&argv[0])
                    .args(&argv[1..])
                    .stdin(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                    .map_err(|err| spawn_error(&argv[0], "player", err))?;
                output = Box::pin(child.stdin.take().expect("stdin is piped"));
                player = Some(child);
            }
            Sink::Serve(addr) => output = Box::pin(serve_one(addr).await?),
        }

        // Subscribed only once the sink is ready, so a viewer that took a while
        // to connect starts at the live edge instead of behind a backlog.
        let source = moq_mux::Source::new(session.announced().clone(), path.as_str());
        let mut export = moq_mux::container::ts::Export::new(source)
            .await
            .with_context(|| format!("subscribing to {name}"))?
            .with_max_age(latency.max_age());
        info!(friend = %name, ?latency, "watching");

        let outcome = loop {
            let frame = tokio::select! {
                frame = export.next() => frame?,
                _ = wait_for(&mut player) => break "player closed",
            };
            let Some(frame) = frame else {
                break "stream ended";
            };
            // A player that quit shows up here as a broken pipe as often as it
            // does through `wait_for`, whichever the select polls first.
            if output.write_all(&frame.payload).await.is_err() {
                break "player closed";
            }
        };
        // EOF lets the player drain what it has and exit on its own.
        output.shutdown().await.ok();
        drop(output);
        if let Some(mut player) = player {
            player.wait().await.ok();
        }
        info!(friend = %name, "{outcome}");
        Ok(())
    }

    // --- Friends and sessions ---------------------------------------------

    /// Dials every friend we don't hold a session with. Failures are normal
    /// (they're offline) and only logged at debug.
    fn dial_friends(&self) {
        let sessions = self.0.sessions.lock().unwrap();
        let targets: Vec<Friend> = self
            .0
            .friends
            .read()
            .unwrap()
            .iter()
            .filter(|(id, _)| !sessions.contains_key(*id))
            .map(|(_, friend)| friend.clone())
            .collect();
        drop(sessions);
        for friend in targets {
            let moq = self.0.moq.clone();
            tokio::spawn(async move {
                let Ok(id) = friend.id() else { return };
                match tokio::time::timeout(DIAL_TIMEOUT, moq.connect(id)).await {
                    // The session surfaces on the incoming stream, which `run_sessions` follows.
                    Ok(Ok(_)) => {}
                    Ok(Err(err)) => debug!(friend = %friend.name, "not reachable: {err}"),
                    Err(_) => debug!(friend = %friend.name, "not reachable: timed out"),
                }
            });
        }
    }

    async fn redial(self) {
        loop {
            self.dial_friends();
            tokio::time::sleep(REDIAL_INTERVAL).await;
        }
    }

    /// Follows every session, dialed or accepted, for the announcements it carries.
    async fn run_sessions(self, mut incoming: IncomingSessionStream) {
        while let Some(session) = incoming.next().await {
            let remote = session.remote_id();
            let Some(friend) = self.friend(remote) else {
                // The gate refuses strangers; this is a friend removed mid-handshake.
                debug!(remote = %remote.fmt_short(), "ignoring a session from a non-friend");
                continue;
            };
            self.0
                .sessions
                .lock()
                .unwrap()
                .insert(remote, session.clone());
            tokio::spawn(self.clone().follow(friend.name, session));
        }
    }

    async fn follow(self, name: String, session: MoqSession) {
        info!(friend = %name, "connected");
        let mut announced = session.announced().announced();
        loop {
            tokio::select! {
                update = announced.next() => match update {
                    Some(update) => self.on_announce(&session, update),
                    None => break,
                },
                _ = session.closed() => break,
            }
        }
        info!(friend = %name, "disconnected");

        let remote = session.remote_id();
        self.0.live_friends.lock().unwrap().remove(&remote);
        let mut sessions = self.0.sessions.lock().unwrap();
        // A newer session to the same friend may have replaced this one.
        if sessions
            .get(&remote)
            .is_some_and(|held| held.conn().stable_id() == session.conn().stable_id())
        {
            sessions.remove(&remote);
        }
    }

    fn on_announce(&self, session: &MoqSession, update: AnnounceUpdate) {
        let path = update.prefix.as_str();
        let Some(code) = path.strip_prefix(LIVE_PREFIX) else {
            return;
        };
        let Ok(streamer) = code.parse::<EndpointId>() else {
            return;
        };
        if streamer == self.id() {
            return;
        }
        let friend = self.friend(streamer);
        let name = friend
            .as_ref()
            .map_or_else(|| streamer.fmt_short().to_string(), |f| f.name.clone());

        match update.kind {
            AnnounceKind::Announced => {
                // Two friends dialing each other at once hold two sessions, and each
                // carries the announcement; react once.
                if !self.0.live_friends.lock().unwrap().insert(streamer) {
                    return;
                }
                let auto_open = friend.is_some_and(|f| f.auto_open);
                info!(friend = %name, auto_open, "is live");
                if self.0.role != Role::Up {
                    return;
                }
                let node = self.clone();
                let session = session.clone();
                let path = path.to_string();
                tokio::spawn(async move {
                    if !auto_open && !notify(&name).await {
                        return;
                    }
                    let latency = node.config().latency;
                    if let Err(err) = node
                        .play(session, path, name.clone(), latency, Sink::Player)
                        .await
                    {
                        warn!(friend = %name, "{err:#}");
                    }
                });
            }
            AnnounceKind::Retracted => {
                let was_live = self.0.live_friends.lock().unwrap().remove(&streamer);
                if was_live {
                    info!(friend = %name, "stopped streaming");
                }
            }
            _ => {}
        }
    }

    pub fn status(&self) -> String {
        let config = self.config();
        // Read before taking `sessions`: `start_live` holds `live` while it dials,
        // which takes `sessions`, so holding both here in the other order could deadlock.
        let live = self
            .0
            .live
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|live| !live.task.is_finished());
        let sessions = self.0.sessions.lock().unwrap();
        let live_friends = self.0.live_friends.lock().unwrap();

        let mut out = format!(
            "code: {}\nlive: {}\n",
            self.id(),
            if live { "yes" } else { "no" }
        );
        if config.friends.is_empty() {
            out.push_str("friends: none yet (`pstream friend add <name> <code>`)\n");
        }
        for friend in &config.friends {
            let Ok(id) = friend.id() else { continue };
            let state = match (sessions.contains_key(&id), live_friends.contains(&id)) {
                (_, true) => "LIVE",
                (true, false) => "online",
                (false, false) => "offline",
            };
            let path = sessions
                .get(&id)
                .and_then(path_summary)
                .map(|path| format!("  {path}"))
                .unwrap_or_default();
            let auto = if friend.auto_open { " (auto-open)" } else { "" };
            out.push_str(&format!("  {:<16} {state:<8}{path}{auto}\n", friend.name));
        }
        out
    }
}

/// A running broadcast. Dropping it stops the capture without a clean finish.
pub struct LiveHandle {
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<()>>,
}

impl LiveHandle {
    /// Ends the stream cleanly, so viewers see it finish rather than fail.
    pub async fn stop(mut self) -> Result<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        (&mut self.task).await.context("the stream task panicked")?
    }

    /// Resolves when the capture ends on its own.
    pub async fn finished(&mut self) -> Result<()> {
        (&mut self.task).await.context("the stream task panicked")?
    }
}

/// Feeds capture output into the MPEG-TS importer until EOF or a stop.
async fn pump(
    mut input: Input,
    child: Option<Child>,
    mut import: ContainerStream,
    mut catalog: moq_mux::catalog::Producer,
    broadcast: moq_net::broadcast::Producer,
    mut stopped: oneshot::Receiver<()>,
) -> Result<()> {
    let mut buffer = BytesMut::with_capacity(64 * 1024);
    let read: Result<()> = async {
        loop {
            buffer.clear();
            tokio::select! {
                read = input.read_buf(&mut buffer) => {
                    if read.context("reading the capture")? == 0 {
                        return Ok(());
                    }
                }
                _ = &mut stopped => return Ok(()),
            }
            import.decode(&buffer)?;
        }
    }
    .await;
    // kill_on_drop stops the capture process.
    drop(child);

    let finished = read
        .and_then(|()| import.finish().map_err(Into::into))
        .and_then(|()| catalog.finish().map_err(Into::into));
    match finished {
        Ok(()) => {
            broadcast.finish();
            info!("stream ended");
            Ok(())
        }
        Err(err) => {
            // Subscribers see the real cause rather than a bare "dropped".
            import.abort(moq_net::Error::Transport(err.to_string()));
            Err(err)
        }
    }
}

/// Which network path a session's traffic takes, e.g. "direct, 18 ms".
///
/// "relayed" means holepunching hasn't succeeded (yet) and the stream is going
/// through n0's rate-limited public relay, which is the first suspect when a
/// stream stutters.
fn path_summary(session: &MoqSession) -> Option<String> {
    let paths = session.conn().paths();
    let path = paths.iter().find(|path| path.is_selected())?;
    let kind = if path.is_relay() { "relayed" } else { "direct" };
    Some(format!("{kind}, {} ms", path.rtt().as_millis()))
}

/// Removes a path from the watching set however the watch ends.
struct Watching {
    node: Node,
    path: String,
}

impl Drop for Watching {
    fn drop(&mut self) {
        self.node.0.watching.lock().unwrap().remove(&self.path);
    }
}

/// Waits for one HTTP client on `addr` and answers it with an endless MPEG-TS body.
///
/// HTTP rather than raw TCP because it's what Android players open from a URL.
/// The request is read and ignored: there is only one thing to serve.
async fn serve_one(addr: SocketAddr) -> Result<tokio::net::TcpStream> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("listening on {addr}"))?;
    info!("open http://{addr}/ in mpv or VLC to watch");
    let (stream, client) = listener.accept().await.context("accepting the player")?;
    debug!(%client, "player connected");
    let mut stream = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if stream.read_line(&mut line).await? == 0 || line.trim().is_empty() {
            break;
        }
    }
    let mut stream = stream.into_inner();
    stream
        .write_all(
            b"HTTP/1.0 200 OK\r\nContent-Type: video/mp2t\r\nCache-Control: no-store\r\n\r\n",
        )
        .await?;
    Ok(stream)
}

async fn wait_for(child: &mut Option<Child>) -> std::io::Result<std::process::ExitStatus> {
    match child {
        Some(child) => child.wait().await,
        None => std::future::pending().await,
    }
}

/// The player command, with latency flags and a title when it's mpv.
///
/// `$PSTREAM_PLAYER` (split on whitespace) overrides config.toml, which is how
/// the smoke test swaps in a headless mpv.
fn player_argv(player: &[String], latency: Latency, title: &str) -> Result<Vec<String>> {
    let player: Vec<String> = match std::env::var("PSTREAM_PLAYER") {
        Ok(env) if !env.trim().is_empty() => env.split_whitespace().map(String::from).collect(),
        _ => player.to_vec(),
    };
    let (program, rest) = player
        .split_first()
        .context("the player command is empty")?;
    let mut argv = vec![program.clone()];
    // `file_stem`, so `mpv.exe` and a full path to it count too.
    if Path::new(program)
        .file_stem()
        .is_some_and(|name| name.eq_ignore_ascii_case("mpv"))
    {
        argv.extend(latency.mpv_flags().iter().map(|flag| flag.to_string()));
        argv.push(format!("--title=pstream: {title}"));
    }
    argv.extend(rest.iter().cloned());
    Ok(argv)
}

/// Says what to do when the capture or player command can't start.
///
/// Windows looks for a bare program name next to pstream.exe before PATH, so
/// a portable folder can carry its own mpv.exe.
fn spawn_error(program: &str, role: &str, err: std::io::Error) -> anyhow::Error {
    if err.kind() == std::io::ErrorKind::NotFound {
        let fix = if cfg!(windows) {
            "put it next to pstream.exe or on PATH"
        } else {
            "install it"
        };
        anyhow!(
            "{program} (the {role} command) isn't installed; {fix}, or set `{role}` in config.toml"
        )
    } else {
        anyhow!("starting {program} (the {role} command): {err}")
    }
}

/// Shows a desktop notification with a Watch button; true if it was clicked.
async fn notify(name: &str) -> bool {
    let output = Command::new("notify-send")
        .args([
            "--app-name=pstream",
            "--icon=video-display",
            "--action=watch=Watch",
            "--wait",
        ])
        .arg(format!("{name} is live"))
        .arg("Watch opens the stream.")
        .output()
        .await;
    match output {
        Ok(output) => String::from_utf8_lossy(&output.stdout).trim() == "watch",
        Err(err) => {
            warn!("notify-send failed ({err}); `pstream watch {name}` opens the stream");
            false
        }
    }
}

/// Accepts MoQ connections from friends only.
///
/// Checked after the TLS handshake, which is where iroh proves the remote's
/// endpoint id; strangers are closed before any MoQ state exists for them.
#[derive(Clone, Debug)]
struct Gate {
    inner: MoqProtocolHandler,
    friends: Friends,
}

impl ProtocolHandler for Gate {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        let known = self.friends.read().unwrap().contains_key(&remote);
        if !known {
            info!(remote = %remote.fmt_short(), "refused a connection from someone who isn't a friend");
            connection.close(0u32.into(), b"not a friend");
            return Ok(());
        }
        self.inner.accept(connection).await
    }
}
