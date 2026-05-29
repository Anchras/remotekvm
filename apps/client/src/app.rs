use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;

use egui::{Color32, Context, Visuals};
use remotekvm_protocol::{ChannelMessage, InputEvent, MouseButton};
use remotekvm_transport::PeerSession;
use tokio::runtime::Handle;

use crate::auth::{AuthFlow, LoopbackReceiver, TokenStore};
use crate::server_client::{ApiClient, UserProfile};
use crate::signaling::establish_session;

/// How long we wait for the user to finish the browser login before giving up.
const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// Application state machine.
pub struct App {
    state: AppState,
    auth_token: Option<String>,
    user: Option<UserInfo>,
    machines: Vec<MachineInfo>,
    error_message: Option<String>,
    server_url: String,
    api_url: String,
    ws_url: String,
    rt: Handle,
    /// Pending background results, polled each frame.
    auth_rx: Option<Receiver<AuthMsg>>,
    connect_rx: Option<Receiver<ConnectMsg>>,
    /// The connected WebRTC session, once established. Input is forwarded here.
    active_session: Option<Arc<PeerSession>>,
}

#[derive(Clone, PartialEq)]
pub enum AppState {
    Login,
    Authenticating,
    Dashboard,
    Connecting,
    Connected,
    Error(String),
}

#[derive(Clone, Default)]
pub struct UserInfo {
    // Retained from the profile for upcoming per-user UI (account menu,
    // showing the signed-in email); only `name` is rendered today.
    #[allow(dead_code)]
    pub id: String,
    #[allow(dead_code)]
    pub email: String,
    pub name: String,
}

#[derive(Clone)]
pub struct MachineInfo {
    pub id: String,
    pub name: String,
    pub hostname: String,
    pub platform: String,
    pub online: bool,
    pub tailscale_ip: Option<String>,
}

/// Result of the background login + initial data fetch.
enum AuthMsg {
    Success {
        token: String,
        user: UserInfo,
        machines: Vec<MachineInfo>,
    },
    Failure(String),
}

/// Result of a background connection request.
enum ConnectMsg {
    Success {
        session_id: String,
        session: Arc<PeerSession>,
    },
    Failure(String),
}

impl App {
    pub fn new(server_url: String, api_url: String, ws_url: String, rt: Handle) -> Self {
        let mut app = Self {
            state: AppState::Login,
            auth_token: None,
            user: None,
            machines: Vec::new(),
            error_message: None,
            server_url,
            api_url,
            ws_url,
            rt,
            auth_rx: None,
            connect_rx: None,
            active_session: None,
        };

        // Auto-resume if a session token is already stored in the OS keychain.
        if let Ok(Some(token)) = TokenStore::load_token() {
            app.auth_token = Some(token.clone());
            app.auth_rx = Some(app.spawn_fetch(token));
            app.state = AppState::Authenticating;
        }

        app
    }

    /// Spawn a background task that fetches the user + machine list for a token.
    fn spawn_fetch(&self, token: String) -> Receiver<AuthMsg> {
        let (tx, rx) = std::sync::mpsc::channel();
        let api_url = self.api_url.clone();
        self.rt.spawn(async move {
            let mut client = ApiClient::new(&api_url);
            client.set_token(&token);
            let msg = match fetch_session(&client).await {
                Ok((user, machines)) => AuthMsg::Success {
                    token,
                    user,
                    machines,
                },
                Err(e) => AuthMsg::Failure(e.to_string()),
            };
            let _ = tx.send(msg);
        });
        rx
    }

    pub fn ui(&mut self, ctx: &Context) {
        // Set custom theme
        let mut visuals = Visuals::dark();
        visuals.widgets.inactive.rounding = egui::Rounding::same(8.0);
        visuals.widgets.active.rounding = egui::Rounding::same(8.0);
        visuals.widgets.hovered.rounding = egui::Rounding::same(8.0);
        visuals.panel_fill = Color32::from_rgb(15, 23, 42); // Slate 900
        visuals.window_fill = Color32::from_rgb(30, 41, 59); // Slate 800
        visuals.widgets.inactive.bg_fill = Color32::from_rgb(51, 65, 85); // Slate 700
        visuals.selection.bg_fill = Color32::from_rgb(6, 182, 212); // Cyan 500
        ctx.set_visuals(visuals);

        // Drain any background results before rendering.
        self.poll_auth(ctx);
        self.poll_connect(ctx);

        match &self.state {
            AppState::Login => self.login_ui(ctx),
            AppState::Authenticating => self.authenticating_ui(ctx),
            AppState::Dashboard => self.dashboard_ui(ctx),
            AppState::Connecting => self.connecting_ui(ctx),
            AppState::Connected => self.connected_ui(ctx),
            AppState::Error(msg) => self.error_ui(ctx, msg.clone()),
        }
    }

    fn poll_auth(&mut self, ctx: &Context) {
        let Some(rx) = &self.auth_rx else { return };
        match rx.try_recv() {
            Ok(AuthMsg::Success {
                token,
                user,
                machines,
            }) => {
                // Persist the token for next launch; non-fatal if it fails.
                if let Err(e) = TokenStore::save_token(&token) {
                    tracing::warn!(error = %e, "failed to persist session token");
                }
                self.auth_token = Some(token);
                self.user = Some(user);
                self.machines = machines;
                self.error_message = None;
                self.state = AppState::Dashboard;
                self.auth_rx = None;
            }
            Ok(AuthMsg::Failure(err)) => {
                self.error_message = Some(err.clone());
                self.state = AppState::Error(err);
                self.auth_rx = None;
                self.auth_token = None;
            }
            Err(TryRecvError::Empty) => ctx.request_repaint(),
            Err(TryRecvError::Disconnected) => {
                self.state = AppState::Error("login task ended unexpectedly".to_string());
                self.auth_rx = None;
            }
        }
    }

    fn poll_connect(&mut self, ctx: &Context) {
        let Some(rx) = &self.connect_rx else { return };
        match rx.try_recv() {
            Ok(ConnectMsg::Success {
                session_id,
                session,
            }) => {
                tracing::info!(%session_id, "WebRTC session established");
                self.active_session = Some(session);
                self.state = AppState::Connected;
                self.connect_rx = None;
            }
            Ok(ConnectMsg::Failure(err)) => {
                self.error_message = Some(err);
                self.state = AppState::Dashboard;
                self.connect_rx = None;
            }
            Err(TryRecvError::Empty) => ctx.request_repaint(),
            Err(TryRecvError::Disconnected) => {
                self.state = AppState::Dashboard;
                self.connect_rx = None;
            }
        }
    }

    fn login_ui(&mut self, ctx: &Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(100.0);

                ui.heading("RemoteKVM");
                ui.add_space(20.0);
                ui.label("Sign in to access your remote machines");
                ui.add_space(40.0);

                let button_width = 280.0;
                let button_height = 44.0;

                if ui
                    .add_sized(
                        [button_width, button_height],
                        egui::Button::new("Continue with WorkOS"),
                    )
                    .clicked()
                {
                    self.start_auth();
                }

                ui.add_space(20.0);

                ui.horizontal(|ui| {
                    ui.label("Server URL:");
                    ui.text_edit_singleline(&mut self.server_url);
                });

                if let Some(ref err) = self.error_message {
                    ui.add_space(20.0);
                    ui.colored_label(Color32::RED, err);
                }
            });
        });
    }

    fn authenticating_ui(&mut self, ctx: &Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(200.0);
                ui.heading("Authenticating...");
                ui.add_space(20.0);
                ui.label("Please complete login in your browser");
                ui.add_space(20.0);
                if ui.button("Cancel").clicked() {
                    self.auth_rx = None;
                    self.state = AppState::Login;
                }
            });
        });
    }

    fn dashboard_ui(&mut self, ctx: &Context) {
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("RemoteKVM");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(ref user) = self.user {
                        ui.label(user.name.clone());
                    }
                    if ui.button("Logout").clicked() {
                        self.logout();
                    }
                });
            });
            ui.separator();
        });

        let mut connect_to: Option<String> = None;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("My Machines");
            ui.add_space(20.0);

            if self.machines.is_empty() {
                ui.label("No machines registered yet. Install the agent on your workstation.");
            } else {
                let available_width = ui.available_width();
                let card_width = 280.0f32;
                let cards_per_row = (available_width / (card_width + 16.0)).max(1.0) as usize;

                let machines = self.machines.clone();
                for chunk in machines.chunks(cards_per_row) {
                    ui.horizontal(|ui| {
                        for machine in chunk {
                            ui.add_space(8.0);
                            if machine_card(ui, machine) {
                                connect_to = Some(machine.id.clone());
                            }
                        }
                    });
                    ui.add_space(16.0);
                }
            }
        });

        if let Some(id) = connect_to {
            self.connect_to(id);
        }
    }

    fn connecting_ui(&mut self, ctx: &Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(200.0);
                ui.heading("Connecting...");
                ui.add_space(20.0);
                ui.label("Establishing secure connection to remote machine");
                ui.add_space(20.0);
                if ui.button("Cancel").clicked() {
                    self.connect_rx = None;
                    self.state = AppState::Dashboard;
                }
            });
        });
    }

    fn connected_ui(&mut self, ctx: &Context) {
        // Forward this frame's mouse/keyboard input to the host over the
        // control channel. (Video decode → wgpu texture is the next milestone.)
        self.forward_input(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(100.0);
                ui.heading("Connected");
                ui.add_space(20.0);
                ui.label("Input is being forwarded. Video stream will appear here.");
                ui.add_space(20.0);
                if ui.button("Disconnect").clicked() {
                    self.disconnect();
                }
            });
        });
        // Keep polling input promptly while connected.
        ctx.request_repaint();
    }

    fn error_ui(&mut self, ctx: &Context, msg: String) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(100.0);
                ui.heading("Error");
                ui.add_space(20.0);
                ui.colored_label(Color32::RED, &msg);
                ui.add_space(20.0);
                if ui.button("Back to Login").clicked() {
                    self.state = AppState::Login;
                    self.error_message = None;
                }
            });
        });
    }

    /// Begin the loopback OAuth login: bind a local receiver, open the browser,
    /// and spawn a task that waits for the callback then fetches session data.
    fn start_auth(&mut self) {
        self.error_message = None;

        // Synchronous bind — safe to call from the egui frame even though we're
        // inside a Tokio context (no `block_on`, which would panic here).
        let receiver = match LoopbackReceiver::bind() {
            Ok(r) => r,
            Err(e) => {
                self.error_message = Some(format!("could not start login listener: {e}"));
                return;
            }
        };
        let redirect_uri = receiver.redirect_uri();

        let flow = AuthFlow::new(&self.server_url);
        if let Err(e) = flow.open_browser(&redirect_uri) {
            self.error_message = Some(format!("could not open browser: {e}"));
            return;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        let api_url = self.api_url.clone();
        self.rt.spawn(async move {
            let msg = match receiver.wait_for_token(LOGIN_TIMEOUT).await {
                Ok(token) => {
                    let mut client = ApiClient::new(&api_url);
                    client.set_token(&token);
                    match fetch_session(&client).await {
                        Ok((user, machines)) => AuthMsg::Success {
                            token,
                            user,
                            machines,
                        },
                        Err(e) => AuthMsg::Failure(e.to_string()),
                    }
                }
                Err(e) => AuthMsg::Failure(e.to_string()),
            };
            let _ = tx.send(msg);
        });

        self.auth_rx = Some(rx);
        self.state = AppState::Authenticating;
    }

    fn connect_to(&mut self, machine_id: String) {
        let Some(token) = self.auth_token.clone() else {
            self.state = AppState::Error("not authenticated".to_string());
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let api_url = self.api_url.clone();
        let ws_url = self.ws_url.clone();
        self.rt.spawn(async move {
            let mut client = ApiClient::new(&api_url);
            client.set_token(&token);
            let msg = match connect_and_establish(&client, &ws_url, &token, &machine_id).await {
                Ok((session_id, session)) => ConnectMsg::Success {
                    session_id,
                    session: Arc::new(session),
                },
                Err(e) => ConnectMsg::Failure(e.to_string()),
            };
            let _ = tx.send(msg);
        });
        self.connect_rx = Some(rx);
        self.state = AppState::Connecting;
    }

    fn logout(&mut self) {
        let _ = TokenStore::delete_token();
        self.teardown_session();
        self.auth_token = None;
        self.user = None;
        self.machines.clear();
        self.auth_rx = None;
        self.state = AppState::Login;
    }

    fn disconnect(&mut self) {
        self.teardown_session();
        self.state = AppState::Dashboard;
    }

    fn teardown_session(&mut self) {
        self.connect_rx = None;
        if let Some(session) = self.active_session.take() {
            self.rt.spawn(async move {
                let _ = session.close().await;
            });
        }
    }

    /// Forward the frame's input events to the host over the control channel.
    fn forward_input(&self, ctx: &Context) {
        let Some(session) = &self.active_session else {
            return;
        };
        let events = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(egui_event_to_input)
                .collect::<Vec<_>>()
        });
        if events.is_empty() {
            return;
        }
        // Send the whole frame's events in one task, in order — spawning a task
        // per event would let the control-channel mutex reorder key down/up
        // pairs.
        let session = session.clone();
        self.rt.spawn(async move {
            for event in events {
                let _ = session.send(&ChannelMessage::Input(event)).await;
            }
        });
    }
}

/// Provision a session via REST, then negotiate the WebRTC connection.
async fn connect_and_establish(
    client: &ApiClient,
    ws_url: &str,
    token: &str,
    machine_id: &str,
) -> anyhow::Result<(String, PeerSession)> {
    let resp = client.connect_machine(machine_id).await?;
    let session = establish_session(ws_url, token, &resp.session_id).await?;
    Ok((resp.session_id, session))
}

/// Translate an egui input event into a protocol [`InputEvent`], if it maps.
///
/// Pointer coordinates are in the client's logical points; mapping them to the
/// host's resolution is part of the video-pipeline milestone (we don't yet know
/// the remote display size on the client).
fn egui_event_to_input(event: &egui::Event) -> Option<InputEvent> {
    match event {
        egui::Event::PointerMoved(p) => Some(InputEvent::MouseMove { x: p.x, y: p.y }),
        egui::Event::PointerButton {
            button, pressed, ..
        } => Some(InputEvent::MouseButton {
            button: map_pointer_button(*button),
            pressed: *pressed,
        }),
        egui::Event::MouseWheel { delta, .. } => Some(InputEvent::MouseScroll {
            dx: delta.x,
            dy: delta.y,
        }),
        egui::Event::Key { key, pressed, .. } => {
            let hid_usage = egui_key_to_hid(*key)?;
            Some(if *pressed {
                InputEvent::KeyDown { hid_usage }
            } else {
                InputEvent::KeyUp { hid_usage }
            })
        }
        _ => None,
    }
}

fn map_pointer_button(button: egui::PointerButton) -> MouseButton {
    match button {
        egui::PointerButton::Primary => MouseButton::Left,
        egui::PointerButton::Secondary => MouseButton::Right,
        egui::PointerButton::Middle => MouseButton::Middle,
        egui::PointerButton::Extra1 => MouseButton::X1,
        egui::PointerButton::Extra2 => MouseButton::X2,
    }
}

/// Map an egui key to a USB HID keyboard usage id (common keys).
fn egui_key_to_hid(key: egui::Key) -> Option<u16> {
    use egui::Key;
    Some(match key {
        Key::A => 0x04,
        Key::B => 0x05,
        Key::C => 0x06,
        Key::D => 0x07,
        Key::E => 0x08,
        Key::F => 0x09,
        Key::G => 0x0A,
        Key::H => 0x0B,
        Key::I => 0x0C,
        Key::J => 0x0D,
        Key::K => 0x0E,
        Key::L => 0x0F,
        Key::M => 0x10,
        Key::N => 0x11,
        Key::O => 0x12,
        Key::P => 0x13,
        Key::Q => 0x14,
        Key::R => 0x15,
        Key::S => 0x16,
        Key::T => 0x17,
        Key::U => 0x18,
        Key::V => 0x19,
        Key::W => 0x1A,
        Key::X => 0x1B,
        Key::Y => 0x1C,
        Key::Z => 0x1D,
        Key::Num1 => 0x1E,
        Key::Num2 => 0x1F,
        Key::Num3 => 0x20,
        Key::Num4 => 0x21,
        Key::Num5 => 0x22,
        Key::Num6 => 0x23,
        Key::Num7 => 0x24,
        Key::Num8 => 0x25,
        Key::Num9 => 0x26,
        Key::Num0 => 0x27,
        Key::Enter => 0x28,
        Key::Escape => 0x29,
        Key::Backspace => 0x2A,
        Key::Tab => 0x2B,
        Key::Space => 0x2C,
        Key::ArrowRight => 0x4F,
        Key::ArrowLeft => 0x50,
        Key::ArrowDown => 0x51,
        Key::ArrowUp => 0x52,
        _ => return None,
    })
}

/// Fetch the current user + machines and map them into the UI's view types.
async fn fetch_session(client: &ApiClient) -> anyhow::Result<(UserInfo, Vec<MachineInfo>)> {
    let profile = client.get_me().await?;
    let machines = client.get_machines().await?;
    Ok((
        user_info_from(profile),
        machines
            .into_iter()
            .map(|m| MachineInfo {
                id: m.id,
                name: m.name,
                hostname: m.hostname,
                platform: m.platform,
                online: m.online,
                tailscale_ip: m.tailscale_ip,
            })
            .collect(),
    ))
}

fn user_info_from(p: UserProfile) -> UserInfo {
    let name = match (&p.first_name, &p.last_name) {
        (Some(f), Some(l)) => format!("{f} {l}"),
        (Some(f), None) => f.clone(),
        (None, Some(l)) => l.clone(),
        (None, None) => p.email.clone(),
    };
    UserInfo {
        id: p.id,
        email: p.email,
        name,
    }
}

/// Render a machine card. Returns `true` when its Connect button was clicked.
fn machine_card(ui: &mut egui::Ui, machine: &MachineInfo) -> bool {
    let mut clicked = false;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_width(260.0);
        ui.set_min_height(120.0);

        ui.horizontal(|ui| {
            let (color, label) = if machine.online {
                (Color32::GREEN, "Online")
            } else {
                (Color32::GRAY, "Offline")
            };
            ui.colored_label(color, "●");
            ui.label(label);
        });

        ui.add_space(8.0);
        ui.heading(&machine.name);
        ui.label(format!("{} • {}", machine.hostname, machine.platform));

        if let Some(ref ip) = machine.tailscale_ip {
            ui.label(format!("Tailscale IP: {ip}"));
        }

        ui.add_space(8.0);

        let button_text = if machine.online { "Connect" } else { "Offline" };
        let button = egui::Button::new(button_text);

        if machine.online && ui.add(button).clicked() {
            clicked = true;
        }
    });
    clicked
}
