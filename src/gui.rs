//! The desktop app: `kith` with no subcommand.
//!
//! It's `kith up` with a window. The same node serves the control socket,
//! so the CLI keeps working alongside it, and the buttons do what `friend`,
//! `live` and `watch` do.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use eframe::egui::{self, Color32, RichText};
use tokio::runtime::{Handle, Runtime};

use crate::{
    capture::{self, Recorder, Share},
    config::{Config, Home},
    control::{self, Request},
    node::{Capture, FriendState, Node, Presence, Role, Sink},
    player,
};

/// How often the window re-reads the node's state. A snapshot is a few mutex
/// reads, so polling is simpler than wiring events through.
const REFRESH: Duration = Duration::from_millis(500);
/// How often the window looks for a newly installed player.
const PLAYER_RECHECK: Duration = Duration::from_secs(5);
/// How often the Share picker re-reads the open windows.
#[cfg(windows)]
const SHARES_RECHECK: Duration = Duration::from_secs(2);

const RED: Color32 = Color32::from_rgb(0xe0, 0x40, 0x40);
const GREEN: Color32 = Color32::from_rgb(0x40, 0xb0, 0x60);

pub fn run(runtime: Runtime, home: Home) -> Result<()> {
    let shared = Shared::default();
    runtime.spawn(start(home.clone(), shared.clone()));
    runtime.spawn_blocking({
        let shared = shared.clone();
        move || {
            let detected = Recorder::detect().map_err(|err| format!("{err:#}"));
            shared.0.lock().unwrap().recorder = Some(detected);
        }
    });

    let app = App {
        runtime: runtime.handle().clone(),
        home,
        shared: shared.clone(),
        name: String::new(),
        code: String::new(),
        auto_open: false,
        source: Capture::Screen,
        share: Share::MainScreen,
        shares: Vec::new(),
        #[cfg(windows)]
        shares_listed: None,
        shown_stream_error: None,
        removing: None,
        was_live: HashSet::new(),
        browser_fallback: false,
        player_checked: None,
    };
    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Kith")
            .with_inner_size([540.0, 580.0])
            .with_min_inner_size([420.0, 360.0]),
        ..Default::default()
    };
    #[cfg(windows)]
    prefer_dx12(&mut options);
    eframe::run_native("kith", options, Box::new(|_| Ok(Box::new(app))))
        .map_err(|err| anyhow!("opening the window: {err}"))?;

    // The window closed: end a stream cleanly so viewers see it finish.
    if let NodeState::Ready(node) = shared.state() {
        runtime.block_on(node.shutdown());
    }
    runtime.shutdown_timeout(Duration::from_secs(2));
    Ok(())
}

/// DX12 is Windows' own graphics API. Vulkan there also loads the implicit
/// layers that overlays install (OBS, Steam, Discord), a common source of
/// crashes and hangs on exactly the gaming PCs Kith is for. GL stays as the
/// fallback, and `WGPU_BACKEND` still overrides both.
#[cfg(windows)]
fn prefer_dx12(options: &mut eframe::NativeOptions) {
    use eframe::{egui_wgpu::WgpuSetup, wgpu::Backends};
    if let WgpuSetup::CreateNew(setup) = &mut options.wgpu_options.wgpu_setup {
        setup.instance_descriptor.backends =
            Backends::from_env().unwrap_or(Backends::DX12 | Backends::GL);
    }
}

/// Starts the node, then serves the control socket for as long as the window
/// is open.
async fn start(home: Home, shared: Shared) {
    let socket = home.socket_path();
    // One endpoint per identity: a running `kith up` already is this node.
    match control::send(&socket, &Request::Status).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            shared.set(NodeState::Failed(
                "Kith is already running for this identity, probably as `kith up` \
                 in a terminal. Stop it, then open this window again."
                    .into(),
            ));
            return;
        }
        Err(err) => {
            shared.set(NodeState::Failed(format!("{err:#}")));
            return;
        }
    }
    let node = match Node::start(home, Role::Up).await {
        Ok(node) => node,
        Err(err) => {
            shared.set(NodeState::Failed(format!("{err:#}")));
            return;
        }
    };
    shared.set(NodeState::Ready(node.clone()));
    if let Err(err) = control::serve(node, &socket).await {
        shared.say(Message::Problem(format!(
            "the command line can't reach this window: {err:#}"
        )));
    }
}

/// State the window shares with the tasks it starts.
#[derive(Clone, Default)]
struct Shared(Arc<Mutex<Inner>>);

#[derive(Default)]
struct Inner {
    node: NodeState,
    /// The latest thing to tell the user.
    message: Option<Message>,
    /// Friends whose Watch was clicked and whose player isn't up yet.
    opening: HashSet<String>,
    /// What the screen recorder can do here, once it has said.
    recorder: Option<Result<Recorder, String>>,
}

#[derive(Clone, Default)]
enum NodeState {
    #[default]
    Starting,
    Ready(Node),
    Failed(String),
}

#[derive(Clone)]
enum Message {
    Info(String),
    Problem(String),
}

impl Message {
    fn from_result(result: Result<String>) -> Self {
        match result {
            Ok(news) => Self::Info(news),
            Err(err) => Self::Problem(format!("{err:#}")),
        }
    }
}

impl Shared {
    fn state(&self) -> NodeState {
        self.0.lock().unwrap().node.clone()
    }

    fn set(&self, state: NodeState) {
        self.0.lock().unwrap().node = state;
    }

    fn say(&self, message: Message) {
        self.0.lock().unwrap().message = Some(message);
    }

    fn message(&self) -> Option<Message> {
        self.0.lock().unwrap().message.clone()
    }

    fn recorder(&self) -> Option<Result<Recorder, String>> {
        self.0.lock().unwrap().recorder.clone()
    }

    fn is_opening(&self, name: &str) -> bool {
        self.0.lock().unwrap().opening.contains(name)
    }

    /// Marks `name` as opening or not; true if that changed anything.
    fn opening(&self, name: &str, opening: bool) -> bool {
        let set = &mut self.0.lock().unwrap().opening;
        if opening {
            set.insert(name.to_string())
        } else {
            set.remove(name)
        }
    }
}

struct App {
    runtime: Handle,
    home: Home,
    shared: Shared,
    /// The add-a-friend form.
    name: String,
    code: String,
    auto_open: bool,
    source: Capture,
    /// What a Windows stream shows, and what the picker last found to offer.
    share: Share,
    shares: Vec<Share>,
    #[cfg(windows)]
    shares_listed: Option<Instant>,
    /// The stream failure last put in the message bar, so it shows once.
    shown_stream_error: Option<String>,
    /// A friend whose Remove was clicked once and awaits confirmation.
    removing: Option<String>,
    /// Friends who were live at the last refresh, so a newly live one can flag
    /// the window.
    was_live: HashSet<String>,
    /// No player is installed, so streams open in the browser.
    browser_fallback: bool,
    player_checked: Option<Instant>,
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(REFRESH);
        egui::Panel::bottom("message").show(ui, |ui| self.message(ui));
        egui::CentralPanel::default_margins().show(ui, |ui| match self.shared.state() {
            NodeState::Starting => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Connecting…");
                });
            }
            NodeState::Failed(err) => {
                ui.colored_label(RED, err);
            }
            NodeState::Ready(node) => {
                egui::ScrollArea::vertical().show(ui, |ui| self.main(ui, &node));
            }
        });
    }
}

impl App {
    fn main(&mut self, ui: &mut egui::Ui, node: &Node) {
        let snapshot = node.snapshot();
        self.flag_newly_live(ui.ctx(), &snapshot.friends);
        if snapshot.stream_error != self.shown_stream_error {
            if let Some(err) = &snapshot.stream_error {
                self.shared
                    .say(Message::Problem(format!("Your stream stopped: {err}")));
            }
            self.shown_stream_error = snapshot.stream_error.clone();
        }

        if self.browser_fallback() {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("Streams open in your browser. A video player lags less:").weak(),
                );
                ui.hyperlink_to("get VLC", player::GET_VLC);
            });
            ui.separator();
        }

        ui.heading("Your code");
        ui.label(
            "Send it to your friends. They add it, and you add theirs: Kith only \
             connects friends who have added each other.",
        );
        ui.horizontal(|ui| {
            let code = snapshot.code.to_string();
            ui.monospace(format!("{}…{}", &code[..8], &code[code.len() - 8..]));
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(code);
                self.shared.say(Message::Info("Copied your code.".into()));
            }
        });
        ui.separator();

        self.streaming(ui, node, snapshot.live);
        ui.separator();

        ui.heading("Friends");
        if snapshot.friends.is_empty() {
            ui.label("None yet. Add one below.");
        }
        egui::Grid::new("friends")
            .num_columns(5)
            .spacing([14.0, 6.0])
            .striped(true)
            .show(ui, |ui| {
                for state in &snapshot.friends {
                    self.friend_row(ui, node, state);
                    ui.end_row();
                }
            });
        ui.separator();

        self.add_form(ui, node);
    }

    fn streaming(&mut self, ui: &mut egui::Ui, node: &Node, live: bool) {
        ui.horizontal(|ui| {
            if live {
                ui.label(RichText::new("● You're live").color(RED).strong());
                if ui.button("Stop").clicked() {
                    let node = node.clone();
                    let shared = self.shared.clone();
                    self.runtime.spawn(async move {
                        let stopped = node.stop_live().await;
                        shared.say(Message::from_result(
                            stopped.map(|_| "Stopped streaming.".to_string()),
                        ));
                    });
                }
                return;
            }
            egui::ComboBox::from_id_salt("source")
                .selected_text(match self.source {
                    Capture::Test => "Test pattern",
                    _ => "Screen",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.source, Capture::Screen, "Screen");
                    ui.selectable_value(&mut self.source, Capture::Test, "Test pattern");
                });
            if ui.button("Go live").clicked() {
                // Starting the capture spawns tasks, which needs the runtime.
                let _runtime = self.runtime.enter();
                let started = node
                    .start_live(self.source, &self.share)
                    .map(|()| "Live. Friends who are online are being told.".to_string());
                self.shared.say(Message::from_result(started));
            }
        });
        if !live && self.source == Capture::Screen {
            self.screen_settings(ui, node);
        }
    }

    /// The encoder, and whether Discord stays out of the sound, from what the
    /// screen recorder found on this machine.
    fn screen_settings(&mut self, ui: &mut egui::Ui, node: &Node) {
        let config = node.config();
        if config.capture.is_some() {
            ui.label(RichText::new("Recorded by the capture command in config.toml.").weak());
            return;
        }
        let recorder = match self.shared.recorder() {
            None => {
                ui.spinner();
                return;
            }
            Some(Err(err)) => {
                ui.colored_label(RED, err);
                return;
            }
            Some(Ok(recorder)) => recorder,
        };
        if recorder.can_pick() {
            self.share_picker(ui);
        }
        ui.horizontal(|ui| {
            ui.label("Video");
            let mut chosen = config.encoder.clone();
            egui::ComboBox::from_id_salt("encoder")
                .width(ui.available_width().min(380.0))
                .selected_text(recorder.describe(&chosen))
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut chosen,
                        capture::AUTO.to_string(),
                        recorder.describe(capture::AUTO),
                    )
                    .on_hover_text(capture::AUTO_HINT);
                    for encoder in &recorder.encoders {
                        ui.selectable_value(&mut chosen, encoder.name.clone(), encoder.label())
                            .on_hover_text(encoder.hint());
                    }
                });
            if chosen != config.encoder {
                let news = match recorder.choose(&chosen) {
                    Ok(encoder) if chosen == capture::AUTO => {
                        format!("Kith picks the encoder: {}.", encoder.label())
                    }
                    Ok(encoder) => format!("Your streams will use {}.", encoder.label()),
                    Err(err) => format!("{err:#}"),
                };
                self.edit_config(node, |config| {
                    config.encoder = chosen;
                    Ok(news)
                });
            }
        });

        let mut silenced = capture::silences(&config.silence, capture::DISCORD);
        let checkbox = ui
            .add_enabled(
                recorder.can_silence,
                egui::Checkbox::new(&mut silenced, "Keep Discord out of the sound"),
            )
            .on_hover_text(
                "Friends in a Discord call with you won't hear themselves. \
                 Everything else you hear still goes out.",
            )
            .on_disabled_hover_text(
                "The screen recorder can't leave apps out here: it needs PipeWire. \
                 Friends in a Discord call with you will hear themselves.",
            );
        if checkbox.changed() {
            self.edit_config(node, |config| {
                config
                    .silence
                    .retain(|app| !app.eq_ignore_ascii_case(capture::DISCORD));
                if silenced {
                    config.silence.push(capture::DISCORD.to_string());
                    Ok("Discord stays out of your stream's sound.".to_string())
                } else {
                    Ok("Discord's sound goes out with the rest.".to_string())
                }
            });
        }
    }

    /// Picks a monitor or a window to stream, on Windows.
    fn share_picker(&mut self, ui: &mut egui::Ui) {
        #[cfg(windows)]
        if self
            .shares_listed
            .is_none_or(|at| at.elapsed() > SHARES_RECHECK)
        {
            self.shares = capture::share::available();
            self.shares_listed = Some(Instant::now());
        }
        ui.horizontal(|ui| {
            ui.label("Share");
            let mut chosen = self.share.clone();
            egui::ComboBox::from_id_salt("share")
                .width(ui.available_width().min(380.0))
                .selected_text(chosen.label().to_string())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut chosen, Share::MainScreen, Share::MainScreen.label())
                        .on_hover_text("Whichever monitor Windows calls the main one");
                    let mut windows = false;
                    for share in &self.shares {
                        if matches!(share, Share::Window { .. }) && !windows {
                            windows = true;
                            ui.separator();
                        }
                        ui.selectable_value(&mut chosen, share.clone(), share.label());
                    }
                })
                .response
                .on_hover_text(
                    "A window streams just that window, even when it's behind others. \
                     Closing it ends the stream.",
                );
            self.share = chosen;
        });
    }

    fn friend_row(&mut self, ui: &mut egui::Ui, node: &Node, state: &FriendState) {
        let name = &state.friend.name;
        let (color, presence) = match state.presence {
            Presence::Live => (RED, "LIVE"),
            Presence::Online => (GREEN, "online"),
            Presence::Offline => (ui.visuals().weak_text_color(), "offline"),
        };
        ui.label(RichText::new(name).strong());
        ui.label(RichText::new(presence).color(color));
        ui.label(state.path.as_deref().unwrap_or("")).on_hover_text(
            "direct: peer to peer. relayed: through a public relay server, \
             which can cap the quality.",
        );
        // On every row, so opening a stream never hinges on presence being
        // right: like `kith watch`, the button just tries.
        if state.watching {
            if self.shared.opening(name, false) {
                self.shared.say(Message::Info(format!(
                    "Watching {name}. Close the player to stop."
                )));
            }
            ui.label("watching");
        } else if self.shared.is_opening(name) {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("opening…");
            });
        } else {
            let (label, hover) = match state.presence {
                Presence::Live => (
                    RichText::new("Watch").strong(),
                    format!("Open {name}'s stream"),
                ),
                Presence::Online => (
                    RichText::new("Watch"),
                    format!("{name} doesn't seem to be live, but this checks anyway"),
                ),
                Presence::Offline => (
                    RichText::new("Watch"),
                    format!("{name} seems to be offline, but this tries anyway"),
                ),
            };
            if ui.button(label).on_hover_text(hover).clicked() {
                self.watch(node, name.clone());
            }
        }
        ui.horizontal(|ui| {
            let mut auto_open = state.friend.auto_open;
            if ui
                .checkbox(&mut auto_open, "auto-open")
                .on_hover_text("Open their stream as soon as they go live")
                .changed()
            {
                self.edit_config(node, |config| {
                    config.friend_mut(name)?.auto_open = auto_open;
                    let state = if auto_open { "on" } else { "off" };
                    Ok(format!("Auto-open is {state} for {name}."))
                });
            }
            if self.removing.as_deref() == Some(name) {
                ui.label("Remove?");
                if ui.small_button("Yes").clicked() {
                    self.edit_config(node, |config| {
                        config.remove_friend(name)?;
                        Ok(format!("Removed {name}."))
                    });
                    self.removing = None;
                }
                if ui.small_button("No").clicked() {
                    self.removing = None;
                }
            } else if ui.small_button("Remove").clicked() {
                self.removing = Some(name.clone());
            }
        });
    }

    fn add_form(&mut self, ui: &mut egui::Ui, node: &Node) {
        ui.heading("Add a friend");
        egui::Grid::new("add")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Name");
                ui.add(
                    egui::TextEdit::singleline(&mut self.name)
                        .hint_text("what you call them")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
                ui.label("Their code");
                ui.add(
                    egui::TextEdit::singleline(&mut self.code)
                        .hint_text("the 64 characters their Kith shows")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
            });
        ui.checkbox(
            &mut self.auto_open,
            "Open their stream as soon as they go live",
        );
        if ui.button("Add").clicked() {
            let own = node.id();
            let (name, code, auto_open) = (self.name.clone(), self.code.clone(), self.auto_open);
            let added = self.edit_config(node, |config| {
                config.add_friend(&name, &code, auto_open, own)?;
                Ok(format!(
                    "Added {}. They need to add your code too.",
                    name.trim()
                ))
            });
            if added {
                self.name.clear();
                self.code.clear();
                self.auto_open = false;
            }
        }
    }

    /// Changes config.toml, then has the node pick it up. True if it worked;
    /// either way the outcome is shown.
    fn edit_config(&self, node: &Node, edit: impl FnOnce(&mut Config) -> Result<String>) -> bool {
        let result = (|| {
            let mut config = self.home.config()?;
            let news = edit(&mut config)?;
            self.home.save(&config)?;
            // Reloading redials friends, which spawns tasks.
            let _runtime = self.runtime.enter();
            node.reload()?;
            Ok(news)
        })();
        let ok = result.is_ok();
        self.shared.say(Message::from_result(result));
        ok
    }

    fn watch(&self, node: &Node, name: String) {
        self.shared.opening(&name, true);
        self.shared
            .say(Message::Info(format!("Opening {name}'s stream…")));
        let node = node.clone();
        let shared = self.shared.clone();
        self.runtime.spawn(async move {
            let watched = node.watch(&name, None, Sink::Player).await;
            shared.opening(&name, false);
            if let Err(err) = watched {
                shared.say(Message::Problem(format!("Watching {name}: {err:#}")));
            }
        });
    }

    /// Whether watching falls back to the browser, rechecked every few seconds
    /// so installing a player takes effect without a restart.
    fn browser_fallback(&mut self) -> bool {
        if self
            .player_checked
            .is_none_or(|at| at.elapsed() > PLAYER_RECHECK)
        {
            let setting = self.home.config().map(|config| config.player);
            self.browser_fallback = setting.is_ok_and(|setting| player::uses_browser(&setting));
            self.player_checked = Some(Instant::now());
        }
        self.browser_fallback
    }

    /// Flashes the taskbar entry when a friend goes live. On Windows, with no
    /// notify-send, that's the only notification there is.
    fn flag_newly_live(&mut self, ctx: &egui::Context, friends: &[FriendState]) {
        let live: HashSet<String> = friends
            .iter()
            .filter(|state| state.presence == Presence::Live)
            .map(|state| state.friend.name.clone())
            .collect();
        if live.difference(&self.was_live).next().is_some() {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }
        self.was_live = live;
    }

    fn message(&self, ui: &mut egui::Ui) {
        match self.shared.message() {
            Some(Message::Info(text)) => ui.label(text),
            Some(Message::Problem(text)) => ui.colored_label(RED, text),
            None => {
                ui.label(RichText::new("Friends can reach you while this window is open.").weak())
            }
        };
    }
}
