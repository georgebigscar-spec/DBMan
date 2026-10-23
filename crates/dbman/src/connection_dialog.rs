//! "Data source" dialog: create or edit a connection and test it.

use std::path::PathBuf;

use dbman_core::{ConnColor, ConnectionConfig, DbKind, Product, SshAuth, SslMode};
use egui::{Context, RichText, Ui};
use shadcn_egui::{Alert, Button, Checkbox, Dialog, Input, Select, Theme, footer, label, tabs};

use crate::explorer::Load;
use crate::files;
use crate::i18n::tr;

pub struct ConnectionDialog {
    pub cfg: ConnectionConfig,
    pub is_new: bool,
    port: String,
    pub test: Load<String>,
    /// The file dialog of "Browse…", while it is open.
    browsing: Option<files::PendingPath>,
    /// What `browsing` picks.
    browse: Browse,
    /// 0: the database, 1: SSH (servers only), then options.
    tab: usize,
    ssh_port: String,
    /// Folders other connections are in, offered in the options.
    pub folders: Vec<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum Browse {
    /// An existing database file.
    Open,
    /// The save dialog of "Create new…".
    Create,
    /// The SSH private key.
    Key,
}

pub enum DialogOutcome {
    Save(ConnectionConfig),
    Test(ConnectionConfig),
    Close,
}

const KINDS: [&str; Product::CHOICES.len()] = {
    let mut labels = [""; Product::CHOICES.len()];
    let mut i = 0;
    while i < labels.len() {
        let (kind, product) = Product::CHOICES[i];
        labels[i] = product.label(kind);
        i += 1;
    }
    labels
};
const SSL: [&str; 4] =
    [SslMode::ALL[0].label(), SslMode::ALL[1].label(), SslMode::ALL[2].label(), SslMode::ALL[3].label()];

fn field(ui: &mut Ui, name: &str, add: impl FnOnce(&mut Ui)) {
    ui.add_space(4.0);
    label(ui, tr(name));
    add(ui);
}

impl ConnectionDialog {
    pub fn new(cfg: ConnectionConfig, is_new: bool) -> Self {
        let port = if cfg.port == 0 { String::new() } else { cfg.port.to_string() };
        let ssh_port = cfg.ssh.port.to_string();
        Self {
            cfg,
            is_new,
            port,
            test: Load::Idle,
            browsing: None,
            browse: Browse::Open,
            tab: 0,
            ssh_port,
            folders: Vec::new(),
        }
    }

    /// The port as typed.
    pub fn port(&self) -> &str {
        &self.port
    }

    /// The tab with color, folder and safety options.
    fn options_tab_index(&self) -> usize {
        if self.cfg.kind.is_file_based() { 1 } else { 2 }
    }

    fn options_tab(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let full = ui.available_width();
        field(ui, "Folder", |ui| {
            ui.add(Input::new(&mut self.cfg.folder).placeholder(tr("(top level)")).width(full));
        });
        let others: Vec<&String> = self.folders.iter().filter(|f| **f != self.cfg.folder).collect();
        if !others.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for f in others {
                    if ui.add(Button::ghost(f.as_str()).size(shadcn_egui::ButtonSize::Sm)).clicked() {
                        self.cfg.folder = f.clone();
                    }
                }
            });
        }
        field(ui, "Color", |ui| {
            ui.horizontal(|ui| {
                let labels: Vec<&str> = ConnColor::ALL.iter().map(|c| tr(c.label())).collect();
                let mut idx = ConnColor::ALL.iter().position(|c| *c == self.cfg.color);
                ui.add(Select::new(&mut idx, &labels).width(full - 30.0));
                if let Some(i) = idx {
                    self.cfg.color = ConnColor::ALL[i];
                }
                if let Some(c) = crate::explorer::conn_color(self.cfg.color) {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(18.0, 18.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 8.0, c);
                }
            });
        });
        ui.label(
            RichText::new(tr(
                "The color marks the connection in the explorer and its tabs, for example red for production.",
            ))
            .size(12.0)
            .color(t.muted_foreground),
        );
        ui.add_space(8.0);
        ui.add(Checkbox::new(&mut self.cfg.read_only).label(tr("Read-only")));
        ui.label(
            RichText::new(tr("Only statements that read run; changes, imports and restores are refused."))
                .size(12.0)
                .color(t.muted_foreground),
        );
        ui.add_space(4.0);
        ui.add(Checkbox::new(&mut self.cfg.confirm_changes).label(tr("Confirm changes in consoles")));
        ui.label(
            RichText::new(tr("A console asks before running statements that change data or structure."))
                .size(12.0)
                .color(t.muted_foreground),
        );
    }

    fn config(&self) -> ConnectionConfig {
        let mut c = self.cfg.clone();
        c.port = self.port.trim().parse().unwrap_or_else(|_| c.kind.default_port().unwrap_or(0));
        c.name = c.name.trim().to_string();
        c.host = c.host.trim().to_string();
        c.ssh.port = self.ssh_port.trim().parse().unwrap_or(22);
        c.ssh.host = c.ssh.host.trim().to_string();
        c.ssh.user = c.ssh.user.trim().to_string();
        c.ssh.key_file = c.ssh.key_file.trim().to_string();
        c.folder = c.folder.trim().to_string();
        c
    }

    fn ssh_tab(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let full = ui.available_width();
        ui.add(Checkbox::new(&mut self.cfg.ssh.enabled).label(tr("Connect through an SSH server")));
        ui.add_enabled_ui(self.cfg.ssh.enabled, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    field(ui, "SSH host", |ui| {
                        ui.add(
                            Input::new(&mut self.cfg.ssh.host).placeholder("bastion.example.com").width(full - 130.0),
                        );
                    });
                });
                ui.vertical(|ui| {
                    field(ui, "Port", |ui| {
                        let invalid = self.ssh_port.trim().parse::<u16>().is_err();
                        ui.add(Input::new(&mut self.ssh_port).width(118.0).invalid(invalid));
                    });
                });
            });
            field(ui, "SSH user", |ui| {
                ui.add(Input::new(&mut self.cfg.ssh.user).width(full));
            });
            field(ui, "Sign in with", |ui| {
                let labels = [tr("Password"), tr("Private key file")];
                let mut idx = Some(if self.cfg.ssh.auth == SshAuth::Key { 1 } else { 0 });
                ui.add(Select::new(&mut idx, &labels).width(full));
                self.cfg.ssh.auth = if idx == Some(1) { SshAuth::Key } else { SshAuth::Password };
            });
            if self.cfg.ssh.auth == SshAuth::Key {
                field(ui, "Private key file", |ui| {
                    ui.horizontal(|ui| {
                        ui.add(
                            Input::new(&mut self.cfg.ssh.key_file).placeholder("~/.ssh/id_ed25519").width(full - 110.0),
                        );
                        if ui.add_enabled(self.browsing.is_none(), Button::outline(tr("Browse…"))).clicked() {
                            self.browse = Browse::Key;
                            self.browsing = Some(files::pick_file(ui.ctx(), rfd::AsyncFileDialog::new()));
                        }
                    });
                });
                field(ui, "Passphrase", |ui| {
                    ui.add(Input::new(&mut self.cfg.ssh.password).password().placeholder(tr("(none)")).width(full));
                });
            } else {
                field(ui, "SSH password", |ui| {
                    ui.add(Input::new(&mut self.cfg.ssh.password).password().width(full));
                });
            }
            ui.add_space(4.0);
            ui.label(
                RichText::new(tr(
                    "The database's host and port are reached from the SSH server, so localhost is the SSH \
                     server itself. The server's host key is remembered on the first connection, and a \
                     different key later is refused.",
                ))
                .size(12.0)
                .color(t.muted_foreground),
            );
        });
    }

    /// Points the dialog at a new database file at `path` and tests the
    /// connection, which creates the file. An existing file is never replaced.
    fn create_at(&mut self, mut path: PathBuf) -> Option<DialogOutcome> {
        if path.extension().is_none() {
            path.set_extension(if self.cfg.kind == DbKind::DuckDb { "duckdb" } else { "db" });
        }
        self.cfg.database = path.to_string_lossy().into_owned();
        if self.cfg.name.trim().is_empty()
            && let Some(stem) = path.file_stem()
        {
            self.cfg.name = stem.to_string_lossy().into_owned();
        }
        if path.exists() {
            self.test =
                Load::Failed(tr("This file already exists. Choose another name, or open it with Browse…").into());
            return None;
        }
        Some(DialogOutcome::Test(self.config()))
    }

    pub fn show(&mut self, ctx: &Context) -> Option<DialogOutcome> {
        let title = if self.is_new { tr("New connection") } else { tr("Connection properties") };
        let mut outcome = None;
        let r = Dialog::new("connection-dialog", title)
            .description(tr("Where DBMan should connect."))
            .width(520.0)
            .show(ctx, |ui| {
                let t = Theme::get(ui.ctx());
                let full = ui.available_width();
                let placeholder = self.cfg.display_name();
                field(ui, "Name", |ui| {
                    ui.add(Input::new(&mut self.cfg.name).placeholder(placeholder).width(full));
                });
                field(ui, "Database type", |ui| {
                    let current = (self.cfg.kind, self.cfg.product);
                    let mut idx = Product::CHOICES.iter().position(|c| *c == current);
                    ui.add(Select::new(&mut idx, &KINDS).width(full));
                    if let Some((k, p)) = idx.map(|i| Product::CHOICES[i])
                        && (k, p) != current
                    {
                        let default_port =
                            |(k, p): (DbKind, Product)| p.default_port(k).map(|p| p.to_string()).unwrap_or_default();
                        let old_default = default_port(current);
                        self.cfg.set_product(k, p);
                        if self.port == old_default {
                            self.port = default_port((k, p));
                        }
                    }
                });
                // Answers of "Browse…" and "Create new…".
                if let Some(answer) = self.browsing.as_ref().and_then(files::PendingPath::poll) {
                    self.browsing = None;
                    if let Some(p) = answer {
                        match self.browse {
                            Browse::Create => outcome = self.create_at(p),
                            Browse::Open => self.cfg.database = p.to_string_lossy().into_owned(),
                            Browse::Key => self.cfg.ssh.key_file = p.to_string_lossy().into_owned(),
                        }
                    }
                }
                ui.add_space(6.0);
                if self.cfg.kind.is_file_based() {
                    self.tab = self.tab.min(1);
                    tabs(ui, &mut self.tab, &[tr("General"), tr("Options")]);
                } else {
                    let ssh = if self.cfg.ssh.enabled { tr("SSH (on)") } else { tr("SSH") };
                    tabs(ui, &mut self.tab, &[tr("General"), ssh, tr("Options")]);
                }
                if self.tab == self.options_tab_index() {
                    self.options_tab(ui);
                } else if !self.cfg.kind.is_file_based() && self.tab == 1 {
                    self.ssh_tab(ui);
                } else if self.cfg.kind.is_file_based() {
                    field(ui, "Database file", |ui| {
                        ui.horizontal(|ui| {
                            ui.add(
                                Input::new(&mut self.cfg.database)
                                    .placeholder(if self.cfg.kind == DbKind::DuckDb {
                                        tr("/path/to/database.duckdb  (or :memory:)")
                                    } else {
                                        tr("/path/to/database.db")
                                    })
                                    .width(full - 230.0),
                            );
                            let (name, exts): (&str, &[&str]) = match self.cfg.kind {
                                DbKind::DuckDb => ("DuckDB", &["duckdb", "ddb", "db"]),
                                _ => ("SQLite", &["db", "sqlite", "sqlite3", "db3"]),
                            };
                            let idle = self.browsing.is_none();
                            if ui.add_enabled(idle, Button::outline(tr("Browse…"))).clicked() {
                                let dialog = rfd::AsyncFileDialog::new()
                                    .add_filter(name, exts)
                                    .add_filter(tr("All files"), &["*"]);
                                self.browse = Browse::Open;
                                self.browsing = Some(files::pick_file(ui.ctx(), dialog));
                            }
                            if ui.add_enabled(idle, Button::outline(tr("Create new…"))).clicked() {
                                let dialog = rfd::AsyncFileDialog::new()
                                    .set_title(tr("Create a new database file"))
                                    .set_file_name(format!("database.{}", exts[0]))
                                    .add_filter(name, exts);
                                self.browse = Browse::Create;
                                self.browsing = Some(files::pick(ui.ctx(), dialog, files::Pick::SaveAs));
                            }
                        });
                    });
                    ui.label(
                        RichText::new(tr("A missing file is created on first connect."))
                            .size(12.0)
                            .color(t.muted_foreground),
                    );
                } else {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            field(ui, "Host", |ui| {
                                ui.add(Input::new(&mut self.cfg.host).placeholder("localhost").width(full - 130.0));
                            });
                        });
                        ui.vertical(|ui| {
                            field(ui, "Port", |ui| {
                                let invalid = !self.port.trim().is_empty() && self.port.trim().parse::<u16>().is_err();
                                ui.add(Input::new(&mut self.port).width(118.0).invalid(invalid));
                            });
                        });
                    });
                    ui.horizontal(|ui| {
                        let half = (full - 12.0) / 2.0;
                        ui.vertical(|ui| {
                            field(ui, "User", |ui| {
                                ui.add(Input::new(&mut self.cfg.user).width(half));
                            });
                        });
                        ui.vertical(|ui| {
                            field(ui, "Password", |ui| {
                                ui.add(Input::new(&mut self.cfg.password).password().width(half));
                            });
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.add(Checkbox::new(&mut self.cfg.save_password).label(tr("Save password")));
                        if self.cfg.save_password {
                            let place = if crate::store::keychain().is_some() {
                                tr("in the system keychain")
                            } else {
                                tr("in settings.json (no system keychain found)")
                            };
                            ui.label(RichText::new(place).size(12.0).color(t.muted_foreground));
                        }
                    });
                    field(ui, "Database", |ui| {
                        let hint = match self.cfg.product.default_database() {
                            "" if self.cfg.kind == DbKind::Postgres => "postgres",
                            "" => tr("(none)"),
                            d => d,
                        };
                        ui.add(Input::new(&mut self.cfg.database).placeholder(hint).width(full));
                    });
                    ui.add(Checkbox::new(&mut self.cfg.only_default_database).label(tr("Show only this database")));
                    field(ui, "SSL mode", |ui| {
                        let mut idx = SslMode::ALL.iter().position(|m| *m == self.cfg.ssl_mode);
                        ui.add(Select::new(&mut idx, &SSL).width(full));
                        if let Some(i) = idx {
                            self.cfg.ssl_mode = SslMode::ALL[i];
                        }
                    });
                }

                ui.add_space(8.0);
                match &self.test {
                    Load::Idle => {}
                    Load::Loading => {
                        ui.horizontal(|ui| {
                            ui.add(shadcn_egui::Spinner::new());
                            ui.label(RichText::new(tr("Connecting…")).color(t.muted_foreground));
                        });
                    }
                    Load::Ready(v) => {
                        Alert::new(tr("Connection succeeded")).icon("✔").description(v.as_str()).show(ui);
                    }
                    Load::Failed(e) => {
                        Alert::new(tr("Connection failed")).icon("!").description(e.as_str()).destructive().show(ui);
                    }
                }
                ui.add_space(8.0);
                footer(ui, |ui| {
                    if ui.add(Button::new(tr("Save"))).clicked() {
                        outcome = Some(DialogOutcome::Save(self.config()));
                    }
                    if ui.add(Button::outline(tr("Cancel"))).clicked() {
                        outcome = Some(DialogOutcome::Close);
                    }
                    let testing = matches!(self.test, Load::Loading);
                    if ui.add_enabled(!testing, Button::ghost(tr("Test connection"))).clicked() {
                        outcome = Some(DialogOutcome::Test(self.config()));
                    }
                });
            });
        if r.should_close && outcome.is_none() {
            outcome = Some(DialogOutcome::Close);
        }
        outcome
    }
}
