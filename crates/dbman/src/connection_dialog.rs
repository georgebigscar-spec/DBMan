//! "Data source" dialog: create or edit a connection and test it.

use std::path::PathBuf;

use dbman_core::{ConnectionConfig, DbKind, SslMode};
use egui::{Context, RichText, Ui};
use shadcn_egui::{Alert, Button, Checkbox, Dialog, Input, Select, Theme, footer, label};

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
    /// Whether `browsing` is the save dialog of "Create new…".
    creating: bool,
}

pub enum DialogOutcome {
    Save(ConnectionConfig),
    Test(ConnectionConfig),
    Close,
}

const KINDS: [&str; DbKind::ALL.len()] = {
    let mut labels = [""; DbKind::ALL.len()];
    let mut i = 0;
    while i < labels.len() {
        labels[i] = DbKind::ALL[i].label();
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
        Self { cfg, is_new, port, test: Load::Idle, browsing: None, creating: false }
    }

    fn config(&self) -> ConnectionConfig {
        let mut c = self.cfg.clone();
        c.port = self.port.trim().parse().unwrap_or_else(|_| c.kind.default_port().unwrap_or(0));
        c.name = c.name.trim().to_string();
        c.host = c.host.trim().to_string();
        c
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
                    let mut idx = DbKind::ALL.iter().position(|k| *k == self.cfg.kind);
                    ui.add(Select::new(&mut idx, &KINDS).width(full));
                    if let Some(k) = idx.map(|i| DbKind::ALL[i])
                        && k != self.cfg.kind
                    {
                        let old_default = self.cfg.kind.default_port().map(|p| p.to_string()).unwrap_or_default();
                        self.cfg.set_kind(k);
                        if self.port == old_default {
                            self.port = k.default_port().map(|p| p.to_string()).unwrap_or_default();
                        }
                    }
                });
                if self.cfg.kind.is_file_based() {
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
                            if let Some(answer) = self.browsing.as_ref().and_then(files::PendingPath::poll) {
                                self.browsing = None;
                                if let Some(p) = answer {
                                    if self.creating {
                                        outcome = self.create_at(p);
                                    } else {
                                        self.cfg.database = p.to_string_lossy().into_owned();
                                    }
                                }
                            }
                            let (name, exts): (&str, &[&str]) = match self.cfg.kind {
                                DbKind::DuckDb => ("DuckDB", &["duckdb", "ddb", "db"]),
                                _ => ("SQLite", &["db", "sqlite", "sqlite3", "db3"]),
                            };
                            let idle = self.browsing.is_none();
                            if ui.add_enabled(idle, Button::outline(tr("Browse…"))).clicked() {
                                let dialog = rfd::AsyncFileDialog::new()
                                    .add_filter(name, exts)
                                    .add_filter(tr("All files"), &["*"]);
                                self.creating = false;
                                self.browsing = Some(files::pick_file(ui.ctx(), dialog));
                            }
                            if ui.add_enabled(idle, Button::outline(tr("Create new…"))).clicked() {
                                let dialog = rfd::AsyncFileDialog::new()
                                    .set_title(tr("Create a new database file"))
                                    .set_file_name(format!("database.{}", exts[0]))
                                    .add_filter(name, exts);
                                self.creating = true;
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
                    ui.add(Checkbox::new(&mut self.cfg.save_password).label(tr("Save password")));
                    field(ui, "Database", |ui| {
                        let hint = if self.cfg.kind == DbKind::Postgres { "postgres" } else { tr("(none)") };
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
                            ui.add(egui::Spinner::new());
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
