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
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
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
    net::{TcpListener, TcpStream},
    process::{Child, ChildStderr, ChildStdin, Command},
    sync::oneshot,
    task::JoinHandle,
};
use tracing::{debug, info, warn};

use crate::{
    capture::{self, Recorder, Recording, Share},
    config::{Config, Friend, Home, Latency},
    player::{self, Player},
};

const LIVE_PREFIX: &str = "live/";
const DIAL_TIMEOUT: Duration = Duration::from_secs(15);
const ONLINE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long `watch` waits for a friend's broadcast to be announced.
const ANNOUNCE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a player handed a URL gets to open it.
const PLAYER_TIMEOUT: Duration = Duration::from_secs(60);
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

/// Where `kith live` reads MPEG-TS from.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum Capture {
    /// The screen, through the screen recorder with the `encoder` and
    /// `silence` settings, or through config.toml's `capture` command.
    Screen,
    /// ffmpeg's test pattern and tone; no screen picker involved.
    Test,
    /// MPEG-TS piped into Kith's own stdin.
    Stdin,
}

/// What this process is for, which decides how much of the node runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// `kith up`: keeps sessions to every friend and reacts when they go live.
    Up,
    /// A foreground `kith live`: dials friends so they hear about the stream,
    /// but doesn't pop players for *their* streams.
    Live,
    /// A foreground `kith watch`: dials only the friend being watched.
    Watch,
}

/// Where a watched stream goes.
pub enum Sink {
    Player,
    File(PathBuf),
    /// One HTTP client at this address, for players Kith can't spawn (an
    /// Android app, when Kith runs in a shell on the phone).
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
    /// Why the last stream stopped by itself, when it failed. Cleared by the
    /// next `go_live`.
    stream_error: Arc<Mutex<Option<String>>>,
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
            stream_error: Arc::default(),
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

    pub fn config(&self) -> Config {
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
    pub fn go_live(&self, capture: Capture, share: &Share) -> Result<LiveHandle> {
        *self.0.stream_error.lock().unwrap() = None;
        let (input, process) = self.open_capture(capture, share)?;
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
        let stream_error = self.0.stream_error.clone();
        // A foreground `kith live` hands the error to its caller instead.
        let log = self.0.role == Role::Up;
        let task = tokio::spawn(async move {
            let pumped = pump(input, process, import, catalog, broadcast, stopped).await;
            if let Err(err) = &pumped {
                if log {
                    warn!("stream stopped: {err:#}");
                }
                *stream_error.lock().unwrap() = Some(format!("{err:#}"));
            }
            pumped
        });
        // Sessions we already hold carry the announcement; dial everyone else
        // so it reaches them now rather than on their next redial.
        self.dial_friends();
        Ok(LiveHandle {
            stop: Some(stop),
            task,
        })
    }

    /// Daemon flavour of [`go_live`](Self::go_live): the handle lives in the node.
    pub fn start_live(&self, capture: Capture, share: &Share) -> Result<()> {
        let mut live = self.0.live.lock().unwrap();
        if live.as_ref().is_some_and(|live| !live.task.is_finished()) {
            bail!("already live; `kith live --stop` first");
        }
        *live = Some(self.go_live(capture, share)?);
        Ok(())
    }

    pub async fn stop_live(&self) -> Result<bool> {
        let live = self.0.live.lock().unwrap().take();
        match live {
            Some(live) => live.stop().await.map(|()| true),
            None => Ok(false),
        }
    }

    fn open_capture(
        &self,
        capture: Capture,
        share: &Share,
    ) -> Result<(Input, Option<CaptureProcess>)> {
        let recording = match capture {
            Capture::Stdin => return Ok((Box::pin(tokio::io::stdin()), None)),
            Capture::Screen => self.screen_recording(share)?,
            Capture::Test => Recording {
                argv: TEST_CAPTURE.iter().map(|arg| arg.to_string()).collect(),
                sound: None,
            },
        };
        let (program, args) = recording
            .argv
            .split_first()
            .context("the capture command is empty")?;
        let stdin = match recording.sound {
            Some(_) => Stdio::piped(),
            None => Stdio::null(),
        };
        let mut child = Command::new(program)
            .args(args)
            .stdin(stdin)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|err| spawn_error(program, "capture", err))?;
        if let Some(silence) = &recording.sound {
            feed_sound(child.stdin.take().expect("stdin is piped"), silence);
        }
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");
        let process = CaptureProcess {
            program: program.clone(),
            complaint: tokio::spawn(last_complaint(program.clone(), stderr)),
            child,
        };
        Ok((Box::pin(stdout), Some(process)))
    }

    /// config.toml's `capture` command, or else the screen recorder with the
    /// encoder the `encoder` setting picks here, and without `silence`.
    fn screen_recording(&self, share: &Share) -> Result<Recording> {
        let config = self.config();
        if let Some(argv) = config.capture {
            return Ok(Recording { argv, sound: None });
        }
        let recorder = Recorder::detect()?;
        let encoder = recorder.choose(&config.encoder)?;
        let sound = capture::sound(&config.silence, recorder.can_silence);
        info!("recording with {}, {sound}", encoder.label());
        #[cfg(windows)]
        if let Share::Window { handle, label } = share
            && !capture::share::still_open(*handle)
        {
            bail!("the window you picked ({label}) has closed; pick another");
        }
        Ok(recorder.command(encoder, &config.silence, share))
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
            Sink::Player => match player::resolve(&self.config().player, latency, &name)? {
                Player::Stdin(argv) => {
                    let mut child = Command::new(&argv[0])
                        .args(&argv[1..])
                        .stdin(Stdio::piped())
                        .kill_on_drop(true)
                        .spawn()
                        .map_err(|err| spawn_error(&argv[0], "player", err))?;
                    output = Box::pin(child.stdin.take().expect("stdin is piped"));
                    player = Some(child);
                }
                Player::Url(argv) => {
                    let (child, stream) = open_url_player(&argv).await?;
                    output = Box::pin(stream);
                    player = Some(child);
                }
                #[cfg(not(target_os = "android"))]
                Player::Browser(latency) => {
                    output = Box::pin(open_in_browser(&name, latency).await?)
                }
                #[cfg(target_os = "android")]
                Player::Browser(_) => bail!("there's no browser to hand the stream to here"),
            },
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
                    if !auto_open && !crate::notify::live(&name, streamer).await {
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

    /// Who's online and live right now, in config.toml's friend order.
    pub fn snapshot(&self) -> Snapshot {
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
        let watching = self.0.watching.lock().unwrap();

        let friends = config
            .friends
            .into_iter()
            .filter_map(|friend| {
                let id = friend.id().ok()?;
                let presence = match (sessions.contains_key(&id), live_friends.contains(&id)) {
                    (_, true) => Presence::Live,
                    (true, false) => Presence::Online,
                    (false, false) => Presence::Offline,
                };
                Some(FriendState {
                    path: sessions.get(&id).and_then(path_summary),
                    watching: watching.contains(&live_path(id)),
                    presence,
                    friend,
                })
            })
            .collect();
        Snapshot {
            code: self.id(),
            live,
            stream_error: self.0.stream_error.lock().unwrap().clone(),
            friends,
        }
    }

    pub fn status(&self) -> String {
        let snapshot = self.snapshot();
        let mut out = format!(
            "code: {}\nlive: {}\n",
            snapshot.code,
            if snapshot.live { "yes" } else { "no" }
        );
        if let Some(err) = &snapshot.stream_error {
            out.push_str(&format!("last stream stopped: {err}\n"));
        }
        if snapshot.friends.is_empty() {
            out.push_str("friends: none yet (`kith friend add <name> <code>`)\n");
        }
        for state in &snapshot.friends {
            let presence = match state.presence {
                Presence::Live => "LIVE",
                Presence::Online => "online",
                Presence::Offline => "offline",
            };
            let path = state
                .path
                .as_ref()
                .map(|path| format!("  {path}"))
                .unwrap_or_default();
            let auto = if state.friend.auto_open {
                " (auto-open)"
            } else {
                ""
            };
            let watching = if state.watching { ", watching" } else { "" };
            out.push_str(&format!(
                "  {:<16} {presence:<8}{path}{watching}{auto}\n",
                state.friend.name
            ));
        }
        out
    }
}

/// What [`Node::snapshot`] reports.
pub struct Snapshot {
    pub code: EndpointId,
    /// We're streaming.
    pub live: bool,
    /// Why the last stream stopped by itself, when it failed.
    pub stream_error: Option<String>,
    pub friends: Vec<FriendState>,
}

pub struct FriendState {
    pub friend: Friend,
    pub presence: Presence,
    /// The session's network path, e.g. "direct, 18 ms", while connected.
    pub path: Option<String>,
    /// Their stream is open in a player (or file) here.
    pub watching: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Offline,
    Online,
    Live,
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
    process: Option<CaptureProcess>,
    mut import: ContainerStream,
    mut catalog: moq_mux::catalog::Producer,
    broadcast: moq_net::broadcast::Producer,
    mut stopped: oneshot::Receiver<()>,
) -> Result<()> {
    let mut buffer = BytesMut::with_capacity(64 * 1024);
    // Ok(true) when the capture ended by itself rather than by a stop.
    let read: Result<bool> = async {
        loop {
            buffer.clear();
            tokio::select! {
                read = input.read_buf(&mut buffer) => {
                    if read.context("reading the capture")? == 0 {
                        return Ok(true);
                    }
                }
                _ = &mut stopped => return Ok(false),
            }
            import.decode(&buffer)?;
        }
    }
    .await;
    let read = match (read, process) {
        (Ok(true), Some(process)) => match process.failure().await {
            Some(err) => Err(err),
            None => Ok(()),
        },
        // Dropping the process handle stops it (kill_on_drop).
        (read, _) => read.map(drop),
    };

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

/// Records the desktop sound without `silence` and writes it to the capture's
/// stdin until the capture goes away, which also stops the recording.
#[cfg(windows)]
fn feed_sound(mut stdin: ChildStdin, silence: &[String]) {
    let mut sound = capture::loopback::start(silence);
    tokio::spawn(async move {
        while let Some(chunk) = sound.pcm.recv().await {
            if stdin.write_all(&chunk).await.is_err() {
                break;
            }
        }
    });
}

#[cfg(not(windows))]
fn feed_sound(_: ChildStdin, _: &[String]) {
    unreachable!("only Windows records the desktop sound for its capture")
}

/// A capture process, and what it last complained about.
struct CaptureProcess {
    program: String,
    child: Child,
    /// Finishes when the process's stderr closes, with its last error line.
    complaint: JoinHandle<Option<String>>,
}

impl CaptureProcess {
    /// Why the capture quit, if it failed: its last error line, or its exit status.
    async fn failure(mut self) -> Option<anyhow::Error> {
        // Its stdout already closed, so it's exiting. The bound covers a custom
        // command that closes stdout and keeps running; dropping it kills that.
        let status = tokio::time::timeout(Duration::from_secs(2), self.child.wait())
            .await
            .ok()?
            .ok()?;
        if status.success() {
            return None;
        }
        let complaint = tokio::time::timeout(Duration::from_secs(1), self.complaint)
            .await
            .ok()
            .and_then(Result::ok)
            .flatten();
        Some(match complaint {
            Some(line) => anyhow!("{} stopped: {line}", self.program),
            None => anyhow!("{} stopped ({status})", self.program),
        })
    }
}

/// Logs a capture's stderr at debug, since gpu-screen-recorder reports its
/// frame rate every second, and keeps the last line that mentions an error.
async fn last_complaint(program: String, stderr: ChildStderr) -> Option<String> {
    let mut lines = BufReader::new(stderr).lines();
    let mut complaint = None;
    while let Ok(Some(line)) = lines.next_line().await {
        debug!("{program}: {line}");
        if line.to_ascii_lowercase().contains("error") {
            complaint = Some(line.trim().to_string());
        }
    }
    complaint
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

/// Waits for one HTTP client on `addr`, for `watch --serve`.
///
/// HTTP rather than raw TCP because it's what Android players open from a URL.
async fn serve_one(addr: SocketAddr) -> Result<TcpStream> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("listening on {addr}"))?;
    info!("open http://{addr}/ in mpv or VLC to watch");
    accept_http(&listener, None).await
}

/// Starts a player on a one-time local URL and waits for it to connect.
async fn open_url_player(argv: &[String]) -> Result<(Child, TcpStream)> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("listening for the player")?;
    // Unguessable, so nothing else on this machine that connects first gets
    // the stream. `.ts` tells the player what's coming.
    let path = format!("/{:016x}.ts", random_u64());
    let url = format!("http://{}{path}", listener.local_addr()?);
    let argv: Vec<String> = argv.iter().map(|arg| arg.replace("{url}", &url)).collect();
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|err| spawn_error(&argv[0], "player", err))?;
    let stream = tokio::select! {
        stream = accept_http(&listener, Some(&path)) => stream?,
        status = child.wait() => {
            bail!("the player ({}) quit before opening the stream ({})", argv[0], status?)
        }
        // Generous: VLC on Windows asks a privacy question on its first run.
        () = tokio::time::sleep(PLAYER_TIMEOUT) => bail!(
            "the player ({}) didn't open the stream within {} s",
            argv[0],
            PLAYER_TIMEOUT.as_secs()
        ),
    };
    Ok((child, stream))
}

/// Accepts HTTP clients until one asks for `path` (anything, when `None`),
/// and answers it with an endless MPEG-TS body.
async fn accept_http(listener: &TcpListener, path: Option<&str>) -> Result<TcpStream> {
    loop {
        let (stream, client) = listener.accept().await.context("accepting the player")?;
        let (requested, mut stream) = read_request(stream).await?;
        if path.is_some_and(|path| requested != path) {
            debug!(%client, "refused {requested:?}");
            let _ = stream.write_all(NOT_FOUND).await;
            continue;
        }
        debug!(%client, "player connected");
        stream.write_all(STREAM_HEADER).await?;
        return Ok(stream);
    }
}

const STREAM_HEADER: &[u8] =
    b"HTTP/1.0 200 OK\r\nContent-Type: video/mp2t\r\nCache-Control: no-store\r\n\r\n";
const NOT_FOUND: &[u8] = b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n";

/// Reads one HTTP request and returns its path. Headers are read and ignored:
/// there's only one kind of thing to serve.
async fn read_request(stream: TcpStream) -> std::io::Result<(String, TcpStream)> {
    let mut stream = BufReader::new(stream);
    let mut request = String::new();
    let mut line = String::new();
    stream.read_line(&mut request).await?;
    loop {
        line.clear();
        if stream.read_line(&mut line).await? == 0 || line.trim().is_empty() {
            break;
        }
    }
    let path = request.split_whitespace().nth(1).unwrap_or_default();
    Ok((path.to_string(), stream.into_inner()))
}

/// Plays the stream in the default browser, through a page served on
/// 127.0.0.1 that hands it to the browser's own video element with mpegts.js.
/// There's no player process to watch, so the tab closing shows up as the
/// connection breaking.
#[cfg(not(target_os = "android"))]
async fn open_in_browser(title: &str, latency: Latency) -> Result<TcpStream> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .context("listening for the browser")?;
    // Unguessable, like the players' URLs, and every file lives under it.
    let base = format!("/{:016x}/", random_u64());
    let url = format!("http://{}{base}", listener.local_addr()?);
    let page = Arc::new(watch_page(title, latency));
    let (found, mut stream) = tokio::sync::mpsc::channel(1);
    // A task per connection: browsers open connections they don't send on
    // right away, and those mustn't hold up the stream's.
    let server = tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            tokio::spawn(serve_browser(
                socket,
                base.clone(),
                page.clone(),
                found.clone(),
            ));
        }
    });
    let opening = url.clone();
    tokio::task::spawn_blocking(move || webbrowser::open(&opening))
        .await?
        .with_context(|| format!("opening {url} in the browser"))?;
    let stream = tokio::time::timeout(PLAYER_TIMEOUT, stream.recv()).await;
    server.abort();
    match stream {
        Ok(Some(stream)) => Ok(stream),
        _ => bail!(
            "the browser didn't open the stream within {} s ({url})",
            PLAYER_TIMEOUT.as_secs()
        ),
    }
}

/// Answers one browser request: the page, mpegts.js, or the stream itself,
/// which goes back to [`open_in_browser`].
#[cfg(not(target_os = "android"))]
async fn serve_browser(
    socket: TcpStream,
    base: String,
    page: Arc<String>,
    found: tokio::sync::mpsc::Sender<TcpStream>,
) {
    const MPEGTS_JS: &[u8] = include_bytes!("../assets/mpegts.js/mpegts.js");
    let Ok(Ok((path, mut socket))) =
        tokio::time::timeout(Duration::from_secs(10), read_request(socket)).await
    else {
        return;
    };
    let (kind, body): (&str, &[u8]) = match path.strip_prefix(base.as_str()) {
        Some("") => ("text/html; charset=utf-8", page.as_bytes()),
        Some("mpegts.js") => ("text/javascript", MPEGTS_JS),
        Some("stream.ts") => {
            if socket.write_all(STREAM_HEADER).await.is_ok() {
                let _ = found.send(socket).await;
            }
            return;
        }
        _ => {
            let _ = socket.write_all(NOT_FOUND).await;
            return;
        }
    };
    let head = format!(
        "HTTP/1.0 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\n\r\n",
        body.len()
    );
    if socket.write_all(head.as_bytes()).await.is_ok() {
        let _ = socket.write_all(body).await;
    }
}

#[cfg(not(target_os = "android"))]
fn watch_page(title: &str, latency: Latency) -> String {
    // How far behind live the page lets playback drift before skipping ahead.
    let max_latency = match latency {
        Latency::Low => "0.8",
        Latency::Normal => "1.5",
        Latency::Smooth => "4",
    };
    let title = format!("Kith: {title}")
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;");
    include_str!("../assets/watch.html")
        .replace("{title}", &title)
        .replace("{max_latency}", max_latency)
        .replace("{get_vlc}", player::GET_VLC)
}

/// 64 random bits without a dependency: std seeds every `RandomState` from the
/// OS's randomness.
fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

async fn wait_for(child: &mut Option<Child>) -> std::io::Result<std::process::ExitStatus> {
    match child {
        Some(child) => child.wait().await,
        None => std::future::pending().await,
    }
}

/// Says what to do when the capture or player command can't start.
///
/// Windows looks for a bare program name next to kith.exe before PATH, so
/// a portable folder can carry its own mpv.exe.
pub(crate) fn spawn_error(program: &str, role: &str, err: std::io::Error) -> anyhow::Error {
    if err.kind() == std::io::ErrorKind::NotFound {
        let fix = if cfg!(windows) {
            "put it next to kith.exe or on PATH"
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
