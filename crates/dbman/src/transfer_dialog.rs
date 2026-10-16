//! Export and import dialogs: export a table or query to a file in any
//! format, or load a CSV / JSON file into a table with a column mapping and a
//! preview. Both run as background jobs.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use dbman_core::export::Format;
use dbman_core::import::{self, ColumnMap, CsvOptions, Field, ImportFormat};
use dbman_core::sql::Dialect;
use dbman_core::{ConnectionConfig, ObjectPath, TableDetails};
use dbman_drivers::transfer::{ExportJob, ExportSource, ImportJob};
use egui::{Context, RichText, Ui};
use shadcn_egui::{Alert, Button, Checkbox, Dialog, Input, Select, Theme, footer, label};
use uuid::Uuid;

use crate::backup_dialog::timestamp;
use crate::explorer::Action;
use crate::files::{self, PendingPath, Pick};
use crate::i18n::{tr, trf};
use crate::jobs::JobSpec;

#[derive(Debug, Clone)]
pub struct Start {
    pub title: String,
    pub spec: JobSpec,
    pub output: Option<PathBuf>,
    pub refresh: Option<Action>,
}

pub enum TransferOutcome {
    Start(Box<Start>),
    Close,
}

fn default_dir() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|u| u.document_dir().map(Path::to_path_buf).or_else(|| Some(u.home_dir().to_path_buf())))
        .unwrap_or_else(std::env::temp_dir)
}

fn error_alert(ui: &mut Ui, title: &str, text: &str) {
    Alert::new(title).icon("!").description(text).show(ui);
}

/// Exports a table or a query's rows to a file.
pub struct ExportDialog {
    conn: Uuid,
    cfg: ConnectionConfig,
    source: ExportSource,
    format: Format,
    path: String,
    /// Target table of INSERT statements, as typed.
    insert_into: String,
    browsing: Option<PendingPath>,
}

impl ExportDialog {
    pub fn new(conn: Uuid, cfg: ConnectionConfig, source: ExportSource) -> Self {
        let insert_into = match &source {
            ExportSource::Table(t) => t.name.clone(),
            ExportSource::Query { .. } => "exported".into(),
        };
        let mut me = Self { conn, cfg, source, format: Format::Csv, path: String::new(), insert_into, browsing: None };
        me.path = default_dir().join(me.file_name()).to_string_lossy().into_owned();
        me
    }

    fn file_name(&self) -> String {
        let base = match &self.source {
            ExportSource::Table(t) => t.name.clone(),
            ExportSource::Query { .. } => "query".into(),
        };
        format!("{base}_{}.{}", timestamp(), self.format.extension())
    }

    fn title(&self) -> String {
        match &self.source {
            ExportSource::Table(t) => trf("Export {name}", &[("name", &t.name)]),
            ExportSource::Query { .. } => tr("Export query result").into(),
        }
    }

    /// Sets the file to write (tests; the dialog has a Browse button).
    pub fn set_path(&mut self, path: &Path) {
        self.path = path.to_string_lossy().into_owned();
    }

    pub fn set_format(&mut self, format: Format) {
        self.format = format;
    }

    fn job(&self) -> Result<ExportJob, String> {
        if self.path.trim().is_empty() {
            return Err(tr("Choose a file.").into());
        }
        let path = PathBuf::from(self.path.trim());
        if path.parent().is_some_and(|p| !p.as_os_str().is_empty() && !p.is_dir()) {
            return Err(tr("The folder does not exist.").into());
        }
        let insert_into = (self.format == Format::SqlInserts).then(|| {
            let name = self.insert_into.trim();
            match &self.source {
                ExportSource::Table(t) => ObjectPath::new(&t.database, &t.schema, name),
                ExportSource::Query { .. } => ObjectPath::new("", "", name),
            }
        });
        Ok(ExportJob { source: self.source.clone(), format: self.format, path, insert_into })
    }

    pub fn show(&mut self, ctx: &Context) -> Option<TransferOutcome> {
        if let Some(p) = &self.browsing
            && let Some(answer) = p.poll()
        {
            self.browsing = None;
            if let Some(path) = answer {
                self.path = path.to_string_lossy().into_owned();
            }
        }
        let mut outcome = None;
        let description = match &self.source {
            ExportSource::Table(_) => tr("Writes every row of the table, page by page, in the background."),
            ExportSource::Query { .. } => tr("Runs the query again on its own session and writes all of its rows."),
        };
        let r = Dialog::new(("export-dialog", self.conn), self.title()).description(description).width(560.0).show(
            ctx,
            |ui| {
                let t = Theme::get(ui.ctx());
                if let ExportSource::Query { sql, .. } = &self.source {
                    label(ui, tr("Query"));
                    let first = sql.lines().next().unwrap_or_default();
                    ui.add(egui::Label::new(RichText::new(first).monospace().color(t.muted_foreground)).truncate());
                    ui.add_space(8.0);
                }
                label(ui, tr("Format"));
                let before = self.format;
                let labels: Vec<&str> = Format::ALL.iter().map(|f| tr(f.label())).collect();
                let mut idx = Format::ALL.iter().position(|f| *f == self.format);
                ui.add(Select::new(&mut idx, &labels).width(240.0));
                if let Some(i) = idx {
                    self.format = Format::ALL[i];
                }
                if self.format != before {
                    let p = PathBuf::from(self.path.trim());
                    self.path = p.with_extension(self.format.extension()).to_string_lossy().into_owned();
                }
                if self.format == Format::SqlInserts {
                    ui.add_space(8.0);
                    label(ui, tr("Insert into table"));
                    ui.add(Input::new(&mut self.insert_into).width(240.0));
                }
                ui.add_space(8.0);
                label(ui, tr("File"));
                ui.horizontal(|ui| {
                    ui.add(Input::new(&mut self.path).width(ui.available_width() - 100.0));
                    if ui.add_enabled(self.browsing.is_none(), Button::outline(tr("Browse…"))).clicked() {
                        let mut d = rfd::AsyncFileDialog::new()
                            .set_file_name(self.file_name())
                            .add_filter(tr(self.format.label()), &[self.format.extension()]);
                        if let Some(dir) = Path::new(self.path.trim()).parent().filter(|d| d.is_dir()) {
                            d = d.set_directory(dir);
                        }
                        self.browsing = Some(files::pick(ui.ctx(), d, Pick::SaveAs));
                    }
                });
                if Path::new(self.path.trim()).is_file() {
                    ui.label(
                        RichText::new(tr("The file exists and will be replaced.")).size(12.0).color(t.muted_foreground),
                    );
                }
                let job = self.job();
                if let Err(e) = &job {
                    ui.add_space(8.0);
                    error_alert(ui, tr("Not ready"), e);
                }
                ui.add_space(12.0);
                footer(ui, |ui| {
                    if ui.add_enabled(job.is_ok(), Button::new(tr("Start export"))).clicked()
                        && let Ok(job) = job
                    {
                        let output = Some(job.path.clone());
                        outcome = Some(TransferOutcome::Start(Box::new(Start {
                            title: self.title(),
                            spec: JobSpec::Export { cfg: Box::new(self.cfg.clone()), job },
                            output,
                            refresh: None,
                        })));
                    }
                    if ui.add(Button::outline(tr("Cancel"))).clicked() {
                        outcome = Some(TransferOutcome::Close);
                    }
                });
            },
        );
        if r.should_close && outcome.is_none() {
            outcome = Some(TransferOutcome::Close);
        }
        outcome
    }
}

/// The first records of a file, read off the UI thread.
struct Preview {
    columns: Vec<String>,
    rows: Vec<Vec<Field>>,
    /// A workbook's sheets.
    sheets: Vec<String>,
}

/// What a preview was read with; a change re-reads it.
#[derive(Clone, PartialEq)]
struct PreviewKey {
    path: String,
    format: ImportFormat,
    csv: CsvOptions,
}

const DELIMITERS: [(char, &str); 4] = [(',', "Comma ,"), (';', "Semicolon ;"), ('\t', "Tab"), ('|', "Pipe |")];

/// Loads a file into a table.
pub struct ImportDialog {
    conn: Uuid,
    cfg: ConnectionConfig,
    table: ObjectPath,
    details: Option<TableDetails>,
    path: String,
    format: ImportFormat,
    csv: CsvOptions,
    empty_as_null: bool,
    clear_first: bool,
    transaction: bool,
    batch_text: String,
    columns: Vec<ColumnMap>,
    preview: Option<Result<Preview, String>>,
    preview_key: Option<PreviewKey>,
    loading: Option<Receiver<Result<Preview, String>>>,
    browsing: Option<PendingPath>,
    /// The workbook's sheets, from the last preview.
    sheets: Vec<String>,
}

impl ImportDialog {
    pub fn new(conn: Uuid, cfg: ConnectionConfig, table: ObjectPath) -> Self {
        let batch_text = import::default_batch(cfg.kind).to_string();
        Self {
            conn,
            cfg,
            table,
            details: None,
            path: String::new(),
            format: ImportFormat::Delimited,
            csv: CsvOptions::default(),
            empty_as_null: true,
            clear_first: false,
            transaction: true,
            batch_text,
            columns: Vec::new(),
            preview: None,
            preview_key: None,
            loading: None,
            browsing: None,
            sheets: Vec::new(),
        }
    }

    pub fn conn(&self) -> Uuid {
        self.conn
    }

    pub fn table(&self) -> &ObjectPath {
        &self.table
    }

    pub fn needs_details(&self) -> bool {
        self.details.is_none()
    }

    /// The table's columns, once loaded.
    pub fn set_details(&mut self, details: TableDetails) {
        self.details = Some(details);
        self.remap();
    }

    /// Picks a workbook's sheet (tests; the dialog has a list).
    pub fn set_sheet(&mut self, sheet: &str) {
        self.csv.sheet = sheet.into();
    }

    /// Picks a file and its format by extension (tests; the dialog has a Browse button).
    pub fn set_path(&mut self, path: &Path) {
        self.path = path.to_string_lossy().into_owned();
        let (format, csv) = import::detect(path);
        self.format = format;
        self.csv = csv;
    }

    pub fn set_clear_first(&mut self, on: bool) {
        self.clear_first = on;
    }

    /// The preview has been read (tests wait for it).
    pub fn preview_ready(&self) -> bool {
        self.loading.is_none() && self.preview.is_some()
    }

    fn key(&self) -> PreviewKey {
        PreviewKey { path: self.path.trim().to_string(), format: self.format, csv: self.csv.clone() }
    }

    fn refresh_preview(&mut self, ctx: &Context) {
        if let Some(rx) = &self.loading {
            match rx.try_recv() {
                Ok(p) => {
                    if let Ok(p) = &p {
                        self.sheets = p.sheets.clone();
                    }
                    self.preview = Some(p);
                    self.loading = None;
                    self.remap();
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => self.loading = None,
            }
        }
        let key = self.key();
        if self.preview_key.as_ref() == Some(&key) {
            return;
        }
        self.preview_key = Some(key.clone());
        self.preview = None;
        self.columns.clear();
        if key.path.is_empty() || !Path::new(&key.path).is_file() {
            return;
        }
        let (tx, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let path = Path::new(&key.path);
            let result = dbman_drivers::files::open(path, key.format, &key.csv, Default::default())
                .and_then(|mut s| {
                    let rows = import::preview(s.as_mut(), 8)?;
                    let sheets = match key.format {
                        ImportFormat::Excel => dbman_drivers::files::sheets(path)?,
                        _ => Vec::new(),
                    };
                    Ok(Preview { columns: s.columns().to_vec(), rows, sheets })
                })
                .map_err(|e| e.message);
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        self.loading = Some(rx);
    }

    /// Matches the table's columns to the file's again.
    fn remap(&mut self) {
        if let (Some(d), Some(Ok(p))) = (&self.details, &self.preview) {
            let named = self.format.named(&self.csv);
            self.columns = import::auto_map(&d.columns, &p.columns, named);
        }
    }

    fn job(&self) -> Result<ImportJob, String> {
        if self.path.trim().is_empty() {
            return Err(tr("Choose a file.").into());
        }
        if !Path::new(self.path.trim()).is_file() {
            return Err(tr("The file does not exist.").into());
        }
        if self.details.is_none() {
            return Err(tr("Reading the table's columns…").into());
        }
        match &self.preview {
            None => return Err(tr("Reading the file…").into()),
            Some(Err(e)) => return Err(e.clone()),
            Some(Ok(_)) => {}
        }
        if self.columns.iter().all(|c| c.source.is_none()) {
            return Err(tr("Map at least one column.").into());
        }
        let batch_rows = self
            .batch_text
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| tr("Rows per INSERT must be a number.").to_string())?;
        Ok(ImportJob {
            table: self.table.clone(),
            path: PathBuf::from(self.path.trim()),
            format: self.format,
            csv: self.csv.clone(),
            columns: self.columns.clone(),
            empty_as_null: self.empty_as_null,
            clear_first: self.clear_first,
            transaction: self.transaction,
            batch_rows,
        })
    }

    fn title(&self) -> String {
        trf("Import into {name}", &[("name", &self.table.name)])
    }

    pub fn show(&mut self, ctx: &Context) -> Option<TransferOutcome> {
        if let Some(p) = &self.browsing
            && let Some(answer) = p.poll()
        {
            self.browsing = None;
            if let Some(path) = answer {
                self.set_path(&path);
            }
        }
        self.refresh_preview(ctx);
        let mut outcome = None;
        let r = Dialog::new(("import-dialog", self.conn), self.title())
            .description(tr("Reads the file in the background and inserts its rows in batches."))
            .width(680.0)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().max_height(ctx.content_rect().height() * 0.7).show(ui, |ui| {
                    self.options_ui(ui);
                    ui.add_space(10.0);
                    self.preview_ui(ui);
                    ui.add_space(10.0);
                    self.mapping_ui(ui);
                });
                let job = self.job();
                if let Err(e) = &job {
                    ui.add_space(8.0);
                    error_alert(ui, tr("Not ready"), e);
                }
                ui.add_space(12.0);
                footer(ui, |ui| {
                    let button = if self.clear_first {
                        Button::destructive(tr("Start import"))
                    } else {
                        Button::new(tr("Start import"))
                    };
                    if ui.add_enabled(job.is_ok(), button).clicked()
                        && let Ok(job) = job
                    {
                        outcome = Some(TransferOutcome::Start(Box::new(Start {
                            title: self.title(),
                            spec: JobSpec::Import { cfg: Box::new(self.cfg.clone()), job },
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
        label(ui, tr("File"));
        ui.horizontal(|ui| {
            ui.add(Input::new(&mut self.path).width(ui.available_width() - 100.0));
            if ui.add_enabled(self.browsing.is_none(), Button::outline(tr("Browse…"))).clicked() {
                let d = rfd::AsyncFileDialog::new()
                    .add_filter(
                        "CSV, TSV, JSON, Excel, Parquet",
                        &["csv", "tsv", "txt", "json", "jsonl", "ndjson", "xlsx", "xlsm", "parquet"],
                    )
                    .add_filter(tr("All files"), &["*"]);
                self.browsing = Some(files::pick(ui.ctx(), d, Pick::OpenFile));
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                label(ui, tr("Format"));
                let labels: Vec<&str> = ImportFormat::ALL.iter().map(|f| tr(f.label())).collect();
                let mut idx = ImportFormat::ALL.iter().position(|f| *f == self.format);
                ui.add(Select::new(&mut idx, &labels).width(200.0));
                if let Some(i) = idx {
                    self.format = ImportFormat::ALL[i];
                }
            });
            if self.format == ImportFormat::Excel && !self.sheets.is_empty() {
                ui.vertical(|ui| {
                    label(ui, tr("Sheet"));
                    let labels: Vec<&str> = self.sheets.iter().map(String::as_str).collect();
                    let mut idx = match self.csv.sheet.as_str() {
                        "" => Some(0),
                        s => self.sheets.iter().position(|n| n == s),
                    };
                    ui.add(Select::new(&mut idx, &labels).width(160.0));
                    if let Some(i) = idx
                        && (i > 0 || !self.csv.sheet.is_empty())
                    {
                        self.csv.sheet = self.sheets[i].clone();
                    }
                });
            }
            if self.format == ImportFormat::Delimited {
                ui.vertical(|ui| {
                    label(ui, tr("Separator"));
                    let labels: Vec<&str> = DELIMITERS.iter().map(|(_, l)| tr(l)).collect();
                    let mut idx = DELIMITERS.iter().position(|(c, _)| *c == self.csv.delimiter);
                    ui.add(Select::new(&mut idx, &labels).width(160.0));
                    if let Some(i) = idx {
                        self.csv.delimiter = DELIMITERS[i].0;
                    }
                });
            }
            ui.vertical(|ui| {
                label(ui, tr("Rows per INSERT"));
                ui.add(Input::new(&mut self.batch_text).width(110.0));
            });
        });
        ui.add_space(6.0);
        match self.format {
            ImportFormat::Delimited => {
                ui.add(Checkbox::new(&mut self.csv.header).label(tr("The first line holds column names")));
                ui.add(Checkbox::new(&mut self.empty_as_null).label(tr("Empty fields are NULL")));
            }
            ImportFormat::Excel => {
                ui.add(Checkbox::new(&mut self.csv.header).label(tr("The first row holds column names")));
            }
            ImportFormat::Json | ImportFormat::Parquet => {}
        }
        ui.add(Checkbox::new(&mut self.clear_first).label(tr("Delete the table's rows first")));
        if self.cfg.kind != dbman_core::DbKind::ClickHouse {
            ui.add(Checkbox::new(&mut self.transaction).label(tr("All or nothing: roll back everything on an error")));
        }
    }

    fn preview_ui(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        match &self.preview {
            None if self.loading.is_some() => {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0));
                    ui.label(RichText::new(tr("Reading the file…")).color(t.muted_foreground));
                });
            }
            None => {}
            Some(Err(e)) => error_alert(ui, tr("Could not read the file"), e),
            Some(Ok(p)) => {
                label(ui, tr("Preview"));
                egui::ScrollArea::horizontal().id_salt("import-preview").show(ui, |ui| {
                    egui::Grid::new("import-preview-grid").striped(true).spacing([12.0, 4.0]).show(ui, |ui| {
                        for c in &p.columns {
                            ui.label(RichText::new(c).strong().size(12.0));
                        }
                        ui.end_row();
                        for row in &p.rows {
                            for f in row {
                                let text = match f {
                                    None => RichText::new("NULL").italics().color(t.muted_foreground),
                                    Some(s) => {
                                        let short: String = s.chars().take(40).collect();
                                        RichText::new(short.replace('\n', "⏎"))
                                    }
                                };
                                ui.label(text.size(12.0).monospace());
                            }
                            ui.end_row();
                        }
                    });
                });
            }
        }
    }

    fn mapping_ui(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let Some(Ok(p)) = &self.preview else { return };
        if self.details.is_none() {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(14.0));
                ui.label(RichText::new(tr("Reading the table's columns…")).color(t.muted_foreground));
            });
            return;
        }
        label(ui, tr("Columns"));
        let mut options: Vec<&str> = vec![tr("— skip (default value) —")];
        options.extend(p.columns.iter().map(String::as_str));
        let d = Dialect(self.cfg.kind);
        egui::Grid::new("import-mapping").spacing([12.0, 6.0]).show(ui, |ui| {
            for c in &mut self.columns {
                let mut text = RichText::new(d.ident(&c.column)).monospace();
                if c.auto_increment {
                    text = text.color(t.muted_foreground);
                }
                ui.label(text);
                ui.label(RichText::new(tr("from")).size(12.0).color(t.muted_foreground));
                let mut idx = Some(c.source.map_or(0, |s| s + 1));
                ui.add(Select::new(&mut idx, &options).width(260.0));
                c.source = idx.and_then(|i| i.checked_sub(1));
                ui.end_row();
            }
        });
    }
}
