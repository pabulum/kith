//! The desktop app: `kith` with no subcommand.
//!
//! It's `kith up` with a window. The same node serves the control socket,
//! so the CLI keeps working alongside it, and the buttons do what `friend`,
//! `live` and `watch` do. Closing the window leaves Kith in the tray, still
//! reachable, and the window comes back from the tray, from opening Kith
//! again, or from a `kith://` link.

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use eframe::egui::{self, Color32, RichText};
use tokio::runtime::{Handle, Runtime};

use crate::{
    capture::{self, Recorder, Share},
    config::{Config, Home},
    control::{self, Request},
    install,
    invite::{self, Invite},
    link::Link,
    node::{Capture, FriendState, Node, Presence, Role, Sink},
    notify, player, tray,
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

/// How the app starts.
pub struct Start {
    /// In the tray, without the window, as at login.
    pub background: bool,
    /// A `kith://` link to follow once connected.
    pub link: Option<Link>,
}

/// Runs the app until it quits. Returns a program to start then: the
/// installed Kith, when this one just installed it.
pub fn run(runtime: Runtime, home: Home, start: Start) -> Result<Option<PathBuf>> {
    #[cfg(windows)]
    register(&home);
    let shared = Shared::default();
    let window = AppWindow::default();
    runtime.spawn(start_node(
        home.clone(),
        shared.clone(),
        window.clone(),
        start.link,
    ));
    runtime.spawn_blocking({
        let shared = shared.clone();
        move || {
            let detected = Recorder::detect().map_err(|err| format!("{err:#}"));
            shared.0.lock().unwrap().recorder = Some(detected);
        }
    });

    let tray = tray::spawn(window.tray_actions(), start.background);
    let stays = Stays {
        tray: tray.is_some(),
        background: start.background,
    };
    let result = open_windows(&runtime, &home, &shared, &window, stays);
    drop(tray);

    // Quitting: end a stream cleanly so viewers see it finish.
    if let NodeState::Ready(node) = shared.state() {
        runtime.block_on(node.shutdown());
    }
    runtime.shutdown_timeout(Duration::from_secs(2));
    result.map(|()| window.state().then.take())
}

/// Why closing the window leaves Kith running.
#[derive(Clone, Copy)]
struct Stays {
    /// It's in the tray, to open again from there.
    tray: bool,
    /// It was running without a window before, as at login.
    background: bool,
}

impl Stays {
    fn at_all(self) -> bool {
        self.tray || self.background
    }
}

/// Opens the window, and again whenever asked, until Kith quits.
fn open_windows(
    runtime: &Runtime,
    home: &Home,
    shared: &Shared,
    window: &AppWindow,
    stays: Stays,
) -> Result<()> {
    let mut open = !stays.background;
    loop {
        if open {
            // A failure goes back to main, which tries again with OpenGL.
            open_window(runtime, home, shared, window, stays)?;
            if !stays.at_all() || window.quitting() {
                return Ok(());
            }
            if stays.tray {
                first_time_in_tray(runtime, home);
            }
        }
        if !window.wait() {
            return Ok(());
        }
        open = true;
    }
}

fn open_window(
    runtime: &Runtime,
    home: &Home,
    shared: &Shared,
    window: &AppWindow,
    stays: Stays,
) -> Result<()> {
    let app = App::new(
        runtime.handle().clone(),
        home.clone(),
        shared.clone(),
        window.clone(),
        stays,
    );
    #[allow(unused_mut)]
    let mut options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Kith")
            // Wayland finds the window's icon through the desktop entry of this name.
            .with_app_id("kith")
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon/kith-256.png"))
                    .unwrap_or_default(),
            )
            .with_inner_size([540.0, 580.0])
            .with_min_inner_size([420.0, 360.0]),
        ..Default::default()
    };
    #[cfg(windows)]
    prefer_dx12(&mut options);
    let window = window.clone();
    eframe::run_native(
        "kith",
        options,
        Box::new(move |creation| {
            window.opened(&creation.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|err| anyhow!("opening the window: {err}"))
}

/// Says where Kith went, the first time its window closes into the tray.
fn first_time_in_tray(runtime: &Runtime, home: &Home) {
    let marker = home.dir().join(".tray-hint-shown");
    if marker.exists() {
        return;
    }
    let _ = std::fs::write(&marker, "");
    let how = if cfg!(windows) {
        "Friends can reach you while it's in the tray. To quit, right-click its icon there."
    } else {
        "Friends can reach you while it's in the tray. To quit, use its menu there."
    };
    runtime.spawn(notify::tell("Kith is still running", how));
}

/// Gives notifications Kith's name and icon, and points `kith://` links (the
/// notifications' Watch button) at this program.
#[cfg(windows)]
fn register(home: &Home) {
    use windows_sys::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    let id: Vec<u16> = install::APP_ID.encode_utf16().chain([0]).collect();
    // SAFETY: a NUL-terminated string that outlives the call.
    unsafe { SetCurrentProcessExplicitAppUserModelID(id.as_ptr()) };
    if let Err(err) = install::register(home) {
        tracing::warn!("{err:#}");
    }
}

/// The window, as the tray, the control socket and the window itself open,
/// raise and close it, each from its own thread.
#[derive(Clone, Default)]
struct AppWindow(Arc<(Mutex<WindowState>, Condvar)>);

#[derive(Default)]
struct WindowState {
    /// The open window's, to raise or close it with.
    ctx: Option<egui::Context>,
    /// Asked to open while closed.
    open: bool,
    quit: bool,
    /// What to start once quit: the installed Kith.
    then: Option<PathBuf>,
    /// An invite someone opened, waiting for a yes or no.
    invited: Option<Invite>,
}

impl AppWindow {
    fn state(&self) -> std::sync::MutexGuard<'_, WindowState> {
        self.0.0.lock().unwrap()
    }

    fn show(&self) {
        let mut state = self.state();
        match &state.ctx {
            Some(ctx) => {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ctx.request_repaint();
            }
            None => {
                state.open = true;
                self.0.1.notify_all();
            }
        }
    }

    fn quit(&self) {
        let mut state = self.state();
        state.quit = true;
        if let Some(ctx) = &state.ctx {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ctx.request_repaint();
        }
        self.0.1.notify_all();
    }

    fn quitting(&self) -> bool {
        self.state().quit
    }

    /// Quits, and hands over to `program` once this Kith is gone.
    fn quit_into(&self, program: PathBuf) {
        self.state().then = Some(program);
        self.quit();
    }

    /// The window is up: from now on it's raised, not reopened.
    fn opened(&self, ctx: &egui::Context) {
        let mut state = self.state();
        state.open = false;
        if state.quit {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        state.ctx = Some(ctx.clone());
    }

    /// The window is closing: asks to show it wait for [`wait`](Self::wait).
    fn closing(&self) {
        self.state().ctx = None;
    }

    /// Blocks until asked to open the window (true) or to quit (false).
    fn wait(&self) -> bool {
        let mut state = self.state();
        while !state.open && !state.quit {
            state = self.0.1.wait(state).unwrap();
        }
        state.open = false;
        !state.quit
    }

    fn tray_actions(&self) -> tray::OnAction {
        let window = self.clone();
        Arc::new(move |action| match action {
            tray::Action::Open => window.show(),
            tray::Action::Quit => window.quit(),
        })
    }
}

impl control::Frontend for AppWindow {
    fn show(&self) {
        AppWindow::show(self);
    }

    fn quit(&self) {
        AppWindow::quit(self);
    }

    fn invited(&self, invite: Invite) {
        self.state().invited = Some(invite);
        AppWindow::show(self);
    }
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

/// Starts the node, then serves the control socket for as long as Kith runs.
async fn start_node(home: Home, shared: Shared, window: AppWindow, link: Option<Link>) {
    let socket = home.socket_path();
    // One endpoint per identity: main hands over to a Kith already running,
    // so one here started at the same moment as this.
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
    if let Some(Link::Watch(code)) = link {
        node.spawn_watch(code.to_string(), None);
    }
    let frontend: Arc<dyn control::Frontend> = Arc::new(window);
    if let Err(err) = control::serve(node, &socket, Some(frontend)).await {
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
    /// Install or Update was put off until next time.
    install_later: bool,
    /// The stream failure last put in the message bar, so it shows once,
    /// even when it happened with the window closed.
    shown_stream_error: Option<String>,
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
    window: AppWindow,
    stays: Stays,
    /// Kith starts at login.
    autostart: bool,
    /// Installing, when this isn't the installed Kith.
    offer: Option<install::Offer>,
    /// Kith is installed, so it can be uninstalled.
    installed: bool,
    /// Uninstall was clicked once and awaits confirmation.
    uninstalling: bool,
    /// What friends see you as, while being edited.
    own_name: String,
    /// The add-a-friend form: their invite or code, and a name for them.
    name: String,
    code: String,
    auto_open: bool,
    source: Capture,
    /// What a Windows stream shows, and what the picker last found to offer.
    share: Share,
    shares: Vec<Share>,
    #[cfg(windows)]
    shares_listed: Option<Instant>,
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
    fn on_exit(&mut self) {
        self.window.closing();
    }

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
    fn new(runtime: Handle, home: Home, shared: Shared, window: AppWindow, stays: Stays) -> Self {
        let own_name = home.config().map(|config| config.name).unwrap_or_default();
        Self {
            runtime,
            home,
            shared,
            window,
            stays,
            autostart: install::autostart(),
            offer: install::offer(),
            installed: install::installed().is_some(),
            uninstalling: false,
            own_name,
            name: String::new(),
            code: String::new(),
            auto_open: false,
            source: Capture::Screen,
            share: Share::MainScreen,
            shares: Vec::new(),
            #[cfg(windows)]
            shares_listed: None,
            removing: None,
            was_live: HashSet::new(),
            browser_fallback: false,
            player_checked: None,
        }
    }

    fn main(&mut self, ui: &mut egui::Ui, node: &Node) {
        let snapshot = node.snapshot();
        self.flag_newly_live(ui.ctx(), &snapshot.friends);
        let unseen_error = {
            let mut inner = self.shared.0.lock().unwrap();
            let unseen = snapshot.stream_error != inner.shown_stream_error;
            inner.shown_stream_error = snapshot.stream_error.clone();
            snapshot.stream_error.clone().filter(|_| unseen)
        };
        if let Some(err) = unseen_error {
            self.shared
                .say(Message::Problem(format!("Your stream stopped: {err}")));
        }

        self.install_banner(ui);
        if self.browser_fallback() {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("Streams open in your browser. A video player lags less:").weak(),
                );
                ui.hyperlink_to("get VLC", player::GET_VLC);
            });
            ui.separator();
        }

        self.invite_banner(ui, node);
        self.you(ui, node, &snapshot.code.to_string());
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
        ui.separator();

        self.settings(ui);
    }

    /// Offers to install this Kith, or to update the installed one to it.
    fn install_banner(&mut self, ui: &mut egui::Ui) {
        let Some(offer) = self.offer else { return };
        if self.shared.0.lock().unwrap().install_later {
            return;
        }
        let place = if cfg!(windows) {
            "the Start menu"
        } else {
            "your app menu"
        };
        let (text, button) = match offer {
            install::Offer::Install => (
                format!("Install Kith? It goes in {place}, so it's easy to find again."),
                "Install",
            ),
            install::Offer::Update(Some(old)) => (
                format!(
                    "Kith {old} is installed. Update it to {}?",
                    install::version()
                ),
                "Update",
            ),
            install::Offer::Update(None) => (
                format!(
                    "An older Kith is installed. Update it to {}?",
                    install::version()
                ),
                "Update",
            ),
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(text);
            if ui.button(RichText::new(button).strong()).clicked() {
                match install::install(&self.home) {
                    Ok(installed) => {
                        self.shared
                            .say(Message::Info("Installed. Opening it…".to_string()));
                        self.window.quit_into(installed);
                    }
                    Err(err) => self
                        .shared
                        .say(Message::Problem(format!("Installing Kith: {err:#}"))),
                }
            }
            if ui.button("Not now").clicked() {
                self.shared.0.lock().unwrap().install_later = true;
            }
        });
        ui.separator();
    }

    fn settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Settings");
        let mut autostart = self.autostart;
        let hint = if self.stays.tray {
            "Kith starts in the tray, so friends can reach you without you opening it."
        } else {
            "Kith starts without its window, so friends can reach you without you \
             opening it. Open Kith to see it."
        };
        if ui
            .checkbox(&mut autostart, "Start Kith when you log in")
            .on_hover_text(hint)
            .changed()
        {
            match install::set_autostart(&self.home, autostart) {
                Ok(()) => {
                    self.autostart = autostart;
                    let news = if autostart {
                        "Kith will start when you log in."
                    } else {
                        "Kith won't start when you log in."
                    };
                    self.shared.say(Message::Info(news.into()));
                }
                Err(err) => self.shared.say(Message::Problem(format!("{err:#}"))),
            }
        }
        ui.horizontal(|ui| {
            if ui
                .button("Quit Kith")
                .on_hover_text("Friends can't reach you until you open Kith again.")
                .clicked()
            {
                self.window.quit();
            }
            if !self.installed {
                return;
            }
            if !self.uninstalling {
                if ui
                    .button("Uninstall…")
                    .on_hover_text("Your code and friends stay, for if you install Kith again.")
                    .clicked()
                {
                    self.uninstalling = true;
                }
                return;
            }
            ui.label("Uninstall Kith?");
            if ui.small_button("Yes").clicked() {
                match install::uninstall() {
                    Ok(_) => {
                        let body = format!(
                            "Your code and friends stay in {}.",
                            self.home.dir().display()
                        );
                        self.runtime
                            .spawn(async move { notify::tell("Kith is uninstalled", &body).await });
                        self.window.quit();
                    }
                    Err(err) => self
                        .shared
                        .say(Message::Problem(format!("Uninstalling Kith: {err:#}"))),
                }
            }
            if ui.small_button("No").clicked() {
                self.uninstalling = false;
            }
        });
    }

    /// Your name, an invite to send, and your code for the long way round.
    fn you(&mut self, ui: &mut egui::Ui, node: &Node, code: &str) {
        ui.heading("You");
        ui.horizontal(|ui| {
            ui.label("Your name");
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.own_name)
                    .hint_text("what friends see")
                    .desired_width(200.0),
            );
            if field.lost_focus() {
                self.save_own_name(node);
            }
        });
        ui.horizontal_wrapped(|ui| {
            let named = !invite::clean_name(&self.own_name).is_empty();
            let button = ui
                .add_enabled(
                    named,
                    egui::Button::new(RichText::new("Invite a friend").strong()),
                )
                .on_hover_text(
                    "Copies a link for one friend. When they paste it into Kith, you're \
                     friends with each other. It works once, within a week.",
                )
                .on_disabled_hover_text("Fill in your name first: the invite carries it.");
            if button.clicked() {
                self.save_own_name(node);
                let _runtime = self.runtime.enter();
                match node.invite() {
                    Ok(link) => {
                        ui.ctx().copy_text(link);
                        self.shared.say(Message::Info(
                            "Copied an invite. Send it to one friend: it works once, within \
                             a week."
                                .into(),
                        ));
                    }
                    Err(err) => self.shared.say(Message::Problem(format!("{err:#}"))),
                }
            }
            ui.label(RichText::new("or send your code:").weak());
            ui.monospace(format!("{}…{}", &code[..8], &code[code.len() - 8..]));
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(code.to_string());
                self.shared.say(Message::Info(
                    "Copied your code. Your friend adds it, and you add theirs.".into(),
                ));
            }
        });
    }

    /// Saves the name field, if it changed.
    fn save_own_name(&mut self, node: &Node) {
        let name = invite::clean_name(&self.own_name);
        if name.is_empty() || name == node.config().name {
            return;
        }
        self.own_name = name.clone();
        self.edit_config(node, |config| {
            config.name = name.clone();
            Ok(format!("Friends see you as {name}."))
        });
    }

    /// An invite someone opened as a link: add them, or not.
    fn invite_banner(&mut self, ui: &mut egui::Ui, node: &Node) {
        let Some(invite) = self.window.state().invited.clone() else {
            return;
        };
        let who = if invite.name.is_empty() {
            "Someone".to_string()
        } else {
            invite.name.clone()
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{who} invited you to be friends.")).strong());
            if node.config().name.is_empty() {
                ui.label("Your name");
                ui.add(
                    egui::TextEdit::singleline(&mut self.own_name)
                        .hint_text("what they'll see")
                        .desired_width(140.0),
                );
            }
            let named = !invite::clean_name(&self.own_name).is_empty();
            if ui
                .add_enabled(named, egui::Button::new(format!("Add {who}")))
                .on_disabled_hover_text("Fill in your name first: their Kith saves you under it.")
                .clicked()
            {
                self.save_own_name(node);
                self.join(node, &invite, None, false);
                self.window.state().invited = None;
            }
            if ui.button("No thanks").clicked() {
                self.window.state().invited = None;
            }
        });
        ui.separator();
    }

    /// Adds whoever sent `invite`; their Kith adds us back once it hears
    /// from ours. True if that worked.
    fn join(&self, node: &Node, invite: &Invite, name: Option<&str>, auto_open: bool) -> bool {
        // Showing the invite to their Kith spawns tasks.
        let _runtime = self.runtime.enter();
        let joined = node.join(invite, name).and_then(|saved| {
            if auto_open {
                self.home.edit(|config| {
                    config.friend_mut(&saved)?.auto_open = true;
                    Ok(())
                })?;
                node.reload()?;
            }
            Ok(saved)
        });
        match joined {
            Ok(saved) => {
                self.shared.say(Message::Info(format!(
                    "Added {saved}. You're friends once their Kith hears from yours."
                )));
                true
            }
            Err(err) => {
                self.shared.say(Message::Problem(format!("{err:#}")));
                false
            }
        }
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
        if let Some(reason) = &state.refused {
            ui.label(RichText::new("turned down").color(RED))
                .on_hover_text(format!("Their Kith turned down the invite: {reason}"));
        } else if state.waiting() {
            ui.label(RichText::new("waiting").color(ui.visuals().weak_text_color()))
                .on_hover_text(
                    "Added from their invite. You're friends as soon as their Kith hears \
                     from yours, which needs both of you to have Kith open.",
                );
        } else {
            ui.label(RichText::new(presence).color(color));
        }
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
        let invite = parse_invite(&self.code);
        egui::Grid::new("add")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Their invite");
                ui.add(
                    egui::TextEdit::singleline(&mut self.code)
                        .hint_text("the link they sent, or their 64-character code")
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
                ui.label("Name");
                let hint = match &invite {
                    Some(invite) if !invite.name.is_empty() => invite.name.clone(),
                    _ => "what you call them".to_string(),
                };
                ui.add(
                    egui::TextEdit::singleline(&mut self.name)
                        .hint_text(hint)
                        .desired_width(f32::INFINITY),
                );
                ui.end_row();
            });
        ui.checkbox(
            &mut self.auto_open,
            "Open their stream as soon as they go live",
        );
        if ui.button("Add").clicked() {
            let added = if let Some(invite) = invite {
                self.save_own_name(node);
                let name = Some(self.name.trim()).filter(|name| !name.is_empty());
                self.join(node, &invite, name, self.auto_open)
            } else {
                let own = node.id();
                let (name, code, auto_open) =
                    (self.name.clone(), self.code.clone(), self.auto_open);
                self.edit_config(node, |config| {
                    config.add_friend(&name, &code, auto_open, own)?;
                    Ok(format!(
                        "Added {}. They need to add your code too, or send you an invite \
                         instead, which does both.",
                        name.trim()
                    ))
                })
            };
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
            let news = self.home.edit(edit)?;
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
        let idle = if self.stays.tray {
            "Closing this window leaves Kith in the tray, where friends can still reach you."
        } else if self.stays.background {
            "Closing this window leaves Kith running, where friends can still reach you."
        } else {
            "Friends can reach you while this window is open."
        };
        match self.shared.message() {
            Some(Message::Info(text)) => ui.label(text),
            Some(Message::Problem(text)) => ui.colored_label(RED, text),
            None => ui.label(RichText::new(idle).weak()),
        };
    }
}

/// An invite in the add-a-friend box: the whole link, or just its last part.
fn parse_invite(text: &str) -> Option<Invite> {
    let text = text.trim();
    match text.parse::<Link>() {
        Ok(Link::Invite(invite)) => Some(invite),
        Ok(_) => None,
        // A 64-character code is hex, which never decodes as an invite's base32.
        Err(_) => Invite::decode(text).ok(),
    }
}
