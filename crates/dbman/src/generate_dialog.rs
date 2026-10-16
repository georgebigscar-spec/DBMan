//! "Generate test data" dialog: a generator per column, picked from its name
//! and type and changeable, with a preview of the first rows. The rows are
//! inserted by a background job.

use dbman_core::generate::{self, ColumnPlan, Generator, Kind};
use dbman_core::{Cell, ConnectionConfig, DbKind, ObjectPath, TableDetails};
use dbman_drivers::generate::GenerateJob;
use egui::{Context, RichText, Ui};
use shadcn_egui::{Alert, Button, Checkbox, Dialog, Input, Select, Theme, footer, label};
use uuid::Uuid;

use crate::i18n::{tr, trf};
use crate::jobs::JobSpec;
use crate::transfer_dialog::{Start, TransferOutcome};

const PREVIEW_ROWS: usize = 5;

/// A column's generator as edited.
struct Row {
    plan: ColumnPlan,
    data_type: String,
    nullable: bool,
    kind: Option<usize>,
    params: String,
    null_text: String,
    error: Option<String>,
}

impl Row {
    fn scale(&self) -> u32 {
        match &self.plan.kind {
            Kind::Decimal { scale, .. } => *scale,
            _ => match self.plan.generic {
                dbman_core::copy::Generic::Decimal(Some((_, s))) => s,
                dbman_core::copy::Generic::Float32 | dbman_core::copy::Generic::Float64 => 3,
                _ => 0,
            },
        }
    }

    /// Reads the edited settings into the plan.
    fn apply(&mut self) {
        let index = self.kind.unwrap_or(0);
        match Kind::parse(index, &self.params, self.scale()) {
            Ok(k) => {
                self.plan.kind = k;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        match self.null_text.trim().trim_end_matches('%').parse::<u8>() {
            Ok(n) if n <= 100 => self.plan.null_percent = if self.nullable { n } else { 0 },
            _ if self.null_text.trim().is_empty() => self.plan.null_percent = 0,
            _ => self.error = Some(format!("NULL %: {}", self.null_text)),
        }
    }
}

pub struct GenerateDialog {
    conn: Uuid,
    cfg: ConnectionConfig,
    table: ObjectPath,
    rows: Option<Vec<Row>>,
    count: String,
    seed: String,
    clear_first: bool,
}

impl GenerateDialog {
    pub fn new(conn: Uuid, cfg: ConnectionConfig, table: ObjectPath) -> Self {
        Self { conn, cfg, table, rows: None, count: "1000".into(), seed: "1".into(), clear_first: false }
    }

    pub fn conn(&self) -> Uuid {
        self.conn
    }

    pub fn table(&self) -> &ObjectPath {
        &self.table
    }

    pub fn needs_details(&self) -> bool {
        self.rows.is_none()
    }

    pub fn set_details(&mut self, details: TableDetails) {
        let plan = generate::default_plan(self.cfg.kind, &details);
        self.rows = Some(
            plan.into_iter()
                .zip(&details.columns)
                .map(|(plan, c)| Row {
                    kind: Some(plan.kind.index()),
                    params: plan.kind.params(),
                    null_text: plan.null_percent.to_string(),
                    data_type: c.data_type.clone(),
                    nullable: c.nullable,
                    error: None,
                    plan,
                })
                .collect(),
        );
    }

    /// The plan as edited (tests).
    pub fn plan(&self) -> Vec<ColumnPlan> {
        self.rows.as_ref().map_or(Vec::new(), |r| r.iter().map(|r| r.plan.clone()).collect())
    }

    /// Sets how many rows to insert (tests).
    pub fn set_count(&mut self, n: u64) {
        self.count = n.to_string();
    }

    /// Changes a column's generator, as picking it does (tests).
    pub fn set_kind(&mut self, column: &str, kind: Kind) {
        if let Some(r) = self.rows.as_mut().and_then(|rows| rows.iter_mut().find(|r| r.plan.column == column)) {
            r.kind = Some(kind.index());
            r.params = kind.params();
            r.apply();
        }
    }

    fn title(&self) -> String {
        trf("Generate test data for {name}", &[("name", &self.table.name)])
    }

    fn job(&self) -> Result<GenerateJob, String> {
        let rows = self.rows.as_ref().ok_or_else(|| tr("Reading the table's columns…").to_string())?;
        if let Some(r) = rows.iter().find(|r| r.error.is_some()) {
            return Err(trf("Check the settings of {column}.", &[("column", &r.plan.column)]));
        }
        let count: u64 = self
            .count
            .trim()
            .replace(['_', ' '], "")
            .parse()
            .map_err(|_| tr("Rows must be a whole number.").to_string())?;
        if count == 0 {
            return Err(tr("Rows must be a whole number.").into());
        }
        let seed = self.seed.trim().parse().map_err(|_| tr("The seed must be a whole number.").to_string())?;
        if !rows.iter().any(|r| r.plan.writes()) {
            return Err(tr("Every column is skipped; choose a generator for at least one.").into());
        }
        let batch_rows = if self.cfg.kind == DbKind::MsSql { 1000 } else { 500 };
        Ok(GenerateJob {
            table: self.table.clone(),
            rows: count,
            plan: rows.iter().map(|r| r.plan.clone()).collect(),
            seed,
            clear_first: self.clear_first,
            batch_rows,
        })
    }

    pub fn show(&mut self, ctx: &Context) -> Option<TransferOutcome> {
        let mut outcome = None;
        let r = Dialog::new(("generate-dialog", self.conn), self.title())
            .description(tr("Makes rows that look real and inserts them in the background, in one transaction."))
            .width(940.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .max_height(ctx.content_rect().height() * 0.65)
                    .show(ui, |ui| {
                        self.options_ui(ui);
                        ui.add_space(10.0);
                        self.columns_ui(ui);
                        ui.add_space(10.0);
                        self.preview_ui(ui);
                    });
                let job = self.job();
                if let Err(e) = &job
                    && self.rows.is_some()
                {
                    ui.add_space(8.0);
                    Alert::new(tr("Not ready")).icon("!").description(e).show(ui);
                }
                ui.add_space(12.0);
                footer(ui, |ui| {
                    let text = tr("Generate");
                    let button = if self.clear_first { Button::destructive(text) } else { Button::new(text) };
                    if ui.add_enabled(job.is_ok(), button).clicked()
                        && let Ok(job) = job
                    {
                        let title = trf("Generate {n} rows in {name}", &[("n", &job.rows), ("name", &self.table.name)]);
                        outcome = Some(TransferOutcome::Start(Box::new(Start {
                            title,
                            spec: JobSpec::Generate { cfg: Box::new(self.cfg.clone()), job },
                            output: None,
                            refresh: None,
                        })));
                    }
                    if ui.add(Button::outline(tr("Cancel"))).clicked() {
                        outcome = Some(TransferOutcome::Close);
                    }
                });
            });
        if r.should_close && outcome.is_none() {
            outcome = Some(TransferOutcome::Close);
        }
        outcome
    }

    fn options_ui(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                label(ui, tr("Rows"));
                ui.add(Input::new(&mut self.count).width(120.0));
            });
            ui.vertical(|ui| {
                label(ui, tr("Seed"));
                ui.add(Input::new(&mut self.seed).width(80.0)).on_hover_text(tr("The same seed makes the same rows."));
            });
            ui.vertical(|ui| {
                ui.add_space(22.0);
                ui.add(Checkbox::new(&mut self.clear_first).label(tr("Delete the table's rows first")));
            });
        });
    }

    fn columns_ui(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let Some(rows) = &mut self.rows else {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0));
                ui.label(RichText::new(tr("Reading the table's columns…")).color(t.muted_foreground));
            });
            return;
        };
        label(ui, tr("Columns"));
        let names: Vec<&str> = Kind::NAMES.iter().map(|n| tr(n)).collect();
        egui::Grid::new("generate-columns").spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(RichText::new(tr("Column")).strong().size(12.0));
            ui.label(RichText::new(tr("Type")).strong().size(12.0));
            ui.label(RichText::new(tr("Generator")).strong().size(12.0));
            ui.label(RichText::new(tr("Settings")).strong().size(12.0));
            ui.label(RichText::new("NULL %").strong().size(12.0));
            ui.end_row();
            for (i, r) in rows.iter_mut().enumerate() {
                ui.push_id(i, |ui| ui.label(RichText::new(&r.plan.column).monospace()));
                let ty: String = r.data_type.chars().take(24).collect();
                ui.label(RichText::new(ty).size(12.0).color(t.muted_foreground)).on_hover_text(&r.data_type);
                let before = r.kind;
                ui.push_id(("kind", i), |ui| ui.add(Select::new(&mut r.kind, &names).width(190.0)));
                let mut changed = false;
                if before != r.kind {
                    r.params = Kind::with_name(r.kind.unwrap_or(0)).params();
                    changed = true;
                }
                let index = r.kind.unwrap_or(0);
                if Kind::has_params(index) {
                    let hint = match index {
                        1 | 20 => "1",
                        2 | 3 | 5 => "min..max",
                        15 | 16 => "2020-01-01..2025-12-31",
                        21 => "a, b, c",
                        23 => "table.column",
                        _ => "",
                    };
                    let resp = ui
                        .push_id(("params", i), |ui| ui.add(Input::new(&mut r.params).placeholder(hint).width(200.0)));
                    changed |= resp.inner.changed();
                } else {
                    ui.label("");
                }
                if r.nullable {
                    let resp = ui.push_id(("null", i), |ui| ui.add(Input::new(&mut r.null_text).width(50.0)));
                    changed |= resp.inner.changed();
                } else {
                    ui.label(RichText::new("—").color(t.muted_foreground));
                }
                if changed {
                    r.apply();
                }
                if let Some(e) = &r.error {
                    ui.label(RichText::new(tr("Check the settings")).size(12.0).color(t.destructive)).on_hover_text(e);
                } else if r.plan.unique && r.plan.writes() {
                    ui.label(RichText::new(tr("unique")).size(12.0).color(t.muted_foreground));
                }
                ui.end_row();
            }
        });
    }

    fn preview_ui(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let Some(rows) = &self.rows else { return };
        if rows.iter().any(|r| r.error.is_some()) {
            return;
        }
        let plan: Vec<ColumnPlan> = rows.iter().map(|r| r.plan.clone()).collect();
        let seed = self.seed.trim().parse().unwrap_or(1);
        let mut g = Generator::new(plan, seed, Default::default());
        let header: Vec<String> = g.columns().iter().map(|c| c.column.clone()).collect();
        let refs: Vec<Option<String>> = g
            .columns()
            .iter()
            .map(|c| match &c.kind {
                Kind::Reference { table, column } => Some(format!("{table}.{column}")),
                _ => None,
            })
            .collect();
        label(ui, tr("Preview"));
        egui::ScrollArea::horizontal().id_salt("generate-preview").show(ui, |ui| {
            egui::Grid::new("generate-preview-grid").striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                for c in &header {
                    ui.label(RichText::new(c).strong().size(12.0));
                }
                ui.end_row();
                for _ in 0..PREVIEW_ROWS {
                    for (cell, r) in g.row().into_iter().zip(&refs) {
                        let text = match (cell, r) {
                            (_, Some(r)) => RichText::new(trf("from {parent}", &[("parent", r)])).italics(),
                            (Cell::Null, _) => RichText::new("NULL").italics().color(t.muted_foreground),
                            (Cell::Bytes(b), _) => RichText::new(format!(
                                "0x{}",
                                b.iter().take(8).map(|x| format!("{x:02x}")).collect::<String>()
                            )),
                            (Cell::Text(s), _) => RichText::new(s.chars().take(40).collect::<String>()),
                        };
                        ui.label(text.size(12.0).monospace());
                    }
                    ui.end_row();
                }
            });
        });
    }
}
