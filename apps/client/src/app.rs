use egui::{Color32, Context, Vec2, Visuals};

/// Application state machine.
pub struct App {
    state: AppState,
    auth_token: Option<String>,
    user: Option<UserInfo>,
    machines: Vec<MachineInfo>,
    error_message: Option<String>,
    server_url: String,
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

#[derive(Clone)]
pub struct UserInfo {
    pub id: String,
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

impl App {
    pub fn new() -> Self {
        Self {
            state: AppState::Login,
            auth_token: None,
            user: None,
            machines: Vec::new(),
            error_message: None,
            server_url: "http://localhost:8080".to_string(),
        }
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

        match &self.state {
            AppState::Login => self.login_ui(ctx),
            AppState::Authenticating => self.authenticating_ui(ctx),
            AppState::Dashboard => self.dashboard_ui(ctx),
            AppState::Connecting => self.connecting_ui(ctx),
            AppState::Connected => self.connected_ui(ctx),
            AppState::Error(msg) => self.error_ui(ctx, msg.clone()),
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
                    self.state = AppState::Login;
                }
            });
        });
    }

    fn dashboard_ui(&mut self, ctx: &Context) {
        let user = self.user.clone().unwrap_or_default();
        
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("RemoteKVM");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(ref user) = self.user {
                        ui.label(format!("{}", user.name));
                    }
                    if ui.button("Logout").clicked() {
                        self.logout();
                    }
                });
            });
            ui.separator();
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("My Machines");
            ui.add_space(20.0);

            if self.machines.is_empty() {
                ui.label("No machines registered yet. Install the agent on your workstation.");
            } else {
                let available_width = ui.available_width();
                let card_width = 280.0f32;
                let cards_per_row = (available_width / (card_width + 16.0)).max(1.0) as usize;

                let mut machines = self.machines.clone();
                let chunks = machines.chunks_mut(cards_per_row);

                for chunk in chunks {
                    ui.horizontal(|ui| {
                        for machine in chunk {
                            ui.add_space(8.0);
                            machine_card(ui, machine);
                        }
                    });
                    ui.add_space(16.0);
                }
            }
        });
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
                    self.state = AppState::Dashboard;
                }
            });
        });
    }

    fn connected_ui(&mut self, ctx: &Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            // TODO: Video stream rendering here
            ui.vertical_centered(|ui| {
                ui.add_space(100.0);
                ui.heading("Connected");
                ui.add_space(20.0);
                ui.label("Video stream will appear here");
                ui.add_space(20.0);
                if ui.button("Disconnect").clicked() {
                    self.disconnect();
                }
            });
        });
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

    fn start_auth(&mut self) {
        self.state = AppState::Authenticating;
        
        // TODO: Open browser to WorkOS AuthKit URL
        // TODO: Start local HTTP server to receive deep link callback
        // TODO: Exchange code for JWT
        
        // For now, simulate successful auth
        self.auth_token = Some("mock_token".to_string());
        self.user = Some(UserInfo {
            id: "user_123".to_string(),
            email: "user@example.com".to_string(),
            name: "Test User".to_string(),
        });
        
        // Load mock machines
        self.machines = vec![
            MachineInfo {
                id: "machine_1".to_string(),
                name: "Workstation".to_string(),
                hostname: "DESKTOP-ABC123".to_string(),
                platform: "windows".to_string(),
                online: true,
                tailscale_ip: Some("100.64.1.1".to_string()),
            },
            MachineInfo {
                id: "machine_2".to_string(),
                name: "MacBook".to_string(),
                hostname: "macbook-pro".to_string(),
                platform: "macos".to_string(),
                online: false,
                tailscale_ip: Some("100.64.1.2".to_string()),
            },
        ];
        
        self.state = AppState::Dashboard;
    }

    fn logout(&mut self) {
        self.auth_token = None;
        self.user = None;
        self.machines.clear();
        self.state = AppState::Login;
    }

    fn disconnect(&mut self) {
        // TODO: Close WebRTC connection
        self.state = AppState::Dashboard;
    }
}

fn machine_card(ui: &mut egui::Ui, machine: &mut MachineInfo) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.set_min_width(260.0);
        ui.set_min_height(120.0);

        ui.horizontal(|ui| {
            // Status indicator
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
            ui.label(format!("Tailscale IP: {}", ip));
        }

        ui.add_space(8.0);
        
        let button_text = if machine.online { "Connect" } else { "Offline" };
        let button = egui::Button::new(button_text);
        
        if machine.online && ui.add(button).clicked() {
            // TODO: Initiate connection
            tracing::info!(machine_id = %machine.id, "connect clicked");
        }
    });
}

impl Default for UserInfo {
    fn default() -> Self {
        Self {
            id: String::new(),
            email: String::new(),
            name: String::new(),
        }
    }
}
