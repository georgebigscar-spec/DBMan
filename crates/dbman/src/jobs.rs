//! Background jobs (backups, restores): they run on the runtime while the
//! window stays usable, and show up in a panel above the status bar with
//! progress, elapsed time, a log and a Cancel button.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use dbman_core::DbResult;
use dbman_core::backup::{SqliteMethod, ToolCommand};
use dbman_drivers::backup::{self as runner, Progress};
use egui::{RichText, Ui};
use shadcn_egui::{Button, ButtonSize, Theme, TooltipExt, toast, toast_error};

use crate::events::{Event, JobEvent, JobId, Tasks};
use crate::explorer::Action;
use crate::tabs::format_elapsed;

/// What a job does.
#[derive(Debug, Clone)]
pub enum JobSpec {
    /// Run a dump or restore tool; `watch` is the file or directory it writes.
    Tool {
        program: PathBuf,
        cmd: ToolCommand,
        watch: Option<PathBuf>,
    },
    SqliteBackup {
        db: PathBuf,
        target: PathBuf,
        method: SqliteMethod,
    },
    SqliteRestore {
        source: PathBuf,
        db: PathBuf,
    },
    /// BACKUP or RESTORE run by ClickHouse; `shown` hides secrets.
    ClickHouse {
        cfg: Box<dbman_core::ConnectionConfig>,
        sql: String,
        shown: String,
    },
    /// BACKUP or RESTORE run by SQL Server itself.
    MsSql {
        cfg: Box<dbman_core::ConnectionConfig>,
        job: runner::MsJob,
    },
    /// Backup or restore statements run on a session of a DuckDB file.
    DuckDb {
        db: String,
        statements: Vec<String>,
        watch: Option<PathBuf>,
    },
}

impl JobSpec {
    async fn run(self, report: runner::Report, cancel: runner::Cancel) -> DbResult<()> {
        match self {
            JobSpec::Tool { program, cmd, watch } => {
                if let Some(v) = runner::tool_version(&program).await {
                    report(Progress::Log(format!("{} ({v})", program.display())));
                }
                runner::run_tool(&program, &cmd, watch.as_deref(), report, cancel).await
            }
            JobSpec::SqliteBackup { db, target, method } => {
                runner::sqlite_backup(db, target, method, report, cancel).await
            }
            JobSpec::SqliteRestore { source, db } => runner::sqlite_restore(source, db, report, cancel).await,
            JobSpec::ClickHouse { cfg, sql, shown } => runner::clickhouse_run(*cfg, sql, shown, report, cancel).await,
            JobSpec::MsSql { cfg, job } => runner::mssql_run(*cfg, job, report, cancel).await,
            JobSpec::DuckDb { db, statements, watch } => {
                runner::duckdb_run(db, statements, watch, report, cancel).await
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum JobState {
    Running,
    Done,
    Failed(String),
    Cancelled,
}

pub struct Job {
    pub id: JobId,
    pub title: String,
    pub state: JobState,
    started: Instant,
    took: Option<Duration>,
    pub log: Vec<String>,
    bytes: Option<u64>,
    fraction: Option<f32>,
    cancel: runner::Cancel,
    /// Reload the explorer here when the job succeeds (after a restore).
    refresh: Option<Action>,
    /// The file or folder written, shown when done.
    output: Option<PathBuf>,
    show_log: bool,
}

/// Lines kept per job; older ones are dropped.
const MAX_LOG: usize = 5000;

#[derive(Default)]
pub struct Jobs {
    pub list: Vec<Job>,
    next: JobId,
    /// The panel is open (it opens by itself when a job starts).
    pub open: bool,
}

impl Jobs {
    pub fn running(&self) -> usize {
        self.list.iter().filter(|j| j.state == JobState::Running).count()
    }

    pub fn start(
        &mut self,
        tasks: &Tasks,
        title: String,
        spec: JobSpec,
        output: Option<PathBuf>,
        refresh: Option<Action>,
    ) -> JobId {
        self.next += 1;
        let id = self.next;
        let cancel = Arc::new(AtomicBool::new(false));
        let tx = tasks.sender();
        let report: runner::Report = {
            let tx = tasks.sender();
            Arc::new(move |p| tx.send(Event::Job { job: id, event: JobEvent::Progress(p) }))
        };
        let c = cancel.clone();
        tasks.runtime().spawn(async move {
            let result = spec.run(report, c).await;
            tx.send(Event::Job { job: id, event: JobEvent::Finished(result) });
        });
        self.list.push(Job {
            id,
            title,
            state: JobState::Running,
            started: Instant::now(),
            took: None,
            log: Vec::new(),
            bytes: None,
            fraction: None,
            cancel,
            refresh,
            output,
            show_log: false,
        });
        self.open = true;
        id
    }

    /// Applies a job's news; returns the explorer refresh to run when it succeeded.
    pub fn on_event(&mut self, id: JobId, ev: JobEvent, ctx: &egui::Context) -> Option<Action> {
        let job = self.list.iter_mut().find(|j| j.id == id)?;
        match ev {
            JobEvent::Progress(Progress::Log(line)) => {
                job.log.push(line);
                if job.log.len() > MAX_LOG {
                    job.log.drain(..job.log.len() - MAX_LOG);
                }
            }
            JobEvent::Progress(Progress::Bytes(n)) => job.bytes = Some(n),
            JobEvent::Progress(Progress::Fraction(f)) => job.fraction = Some(f.clamp(0.0, 1.0)),
            JobEvent::Finished(result) => {
                job.took = Some(job.started.elapsed());
                match result {
                    Ok(()) => {
                        job.state = JobState::Done;
                        job.fraction = Some(1.0);
                        let where_ = job.output.as_ref().map(|p| p.to_string_lossy().into_owned());
                        toast(ctx, format!("{} finished", job.title), where_.as_deref());
                        return job.refresh.clone();
                    }
                    Err(_) if job.cancel.load(Ordering::Relaxed) => {
                        job.state = JobState::Cancelled;
                        job.log.push("Cancelled.".into());
                    }
                    Err(e) => {
                        job.log.push(e.message.clone());
                        job.show_log = true;
                        toast_error(ctx, format!("{} failed", job.title), e.message.lines().last());
                        job.state = JobState::Failed(e.message);
                    }
                }
            }
        }
        None
    }

    /// Cancels every running job (the window is closing).
    pub fn cancel_all(&self) {
        for j in &self.list {
            j.cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        let t = Theme::get(ui.ctx());
        let mut remove = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Background tasks").strong().color(t.foreground));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.add(Button::ghost("×").size(ButtonSize::Icon)).tooltip("Hide").clicked() {
                    self.open = false;
                }
                let finished = self.list.iter().any(|j| j.state != JobState::Running);
                if finished && ui.add(Button::ghost("Clear finished").size(ButtonSize::Sm)).clicked() {
                    self.list.retain(|j| j.state == JobState::Running);
                }
            });
        });
        egui::ScrollArea::vertical().id_salt("jobs-scroll").max_height(260.0).show(ui, |ui| {
            for job in self.list.iter_mut().rev() {
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    match &job.state {
                        JobState::Running => {
                            ui.add(egui::Spinner::new().size(14.0));
                        }
                        JobState::Done => {
                            ui.label(RichText::new("✔").color(t.primary));
                        }
                        JobState::Failed(_) => {
                            ui.label(RichText::new("×").color(t.destructive));
                        }
                        JobState::Cancelled => {
                            ui.label(RichText::new("×").color(t.muted_foreground));
                        }
                    }
                    ui.label(RichText::new(&job.title).color(t.foreground));
                    let elapsed = job.took.unwrap_or_else(|| job.started.elapsed());
                    let mut info = format_elapsed(elapsed);
                    if let Some(b) = job.bytes {
                        info = format!("{} · {info}", human_bytes(b));
                    }
                    match &job.state {
                        JobState::Failed(_) => info.push_str(" · failed"),
                        JobState::Cancelled => info.push_str(" · cancelled"),
                        _ => {}
                    }
                    ui.label(RichText::new(info).size(12.0).color(t.muted_foreground));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if job.state == JobState::Running {
                            if ui
                                .add(Button::outline("Cancel").size(ButtonSize::Sm))
                                .tooltip("Stop the job; a partial backup file is left as is")
                                .clicked()
                            {
                                job.cancel.store(true, Ordering::Relaxed);
                            }
                        } else if ui.add(Button::ghost("×").size(ButtonSize::Icon)).tooltip("Remove").clicked() {
                            remove = Some(job.id);
                        }
                        let label = if job.show_log { "Hide log" } else { "Log" };
                        if ui.add(Button::ghost(label).size(ButtonSize::Sm)).clicked() {
                            job.show_log = !job.show_log;
                        }
                        if job.state == JobState::Running {
                            match job.fraction {
                                Some(f) => {
                                    ui.add(shadcn_egui::Progress::new(f).width(160.0));
                                }
                                None => {
                                    // Unknown total: a bar that keeps moving.
                                    let phase = (ui.input(|i| i.time) * 0.6).fract() as f32;
                                    ui.add(shadcn_egui::Progress::new(phase).width(160.0));
                                }
                            }
                            ui.ctx().request_repaint_after(Duration::from_millis(100));
                        }
                    });
                });
                if job.show_log {
                    let frame = egui::Frame::new()
                        .fill(t.muted)
                        .corner_radius(t.radius_md())
                        .inner_margin(egui::Margin::same(6));
                    frame.show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::ScrollArea::vertical()
                            .id_salt(("job-log", job.id))
                            .max_height(160.0)
                            .stick_to_bottom(true)
                            .auto_shrink([false, true])
                            .show_rows(ui, 14.0, job.log.len(), |ui, rows| {
                                for line in &job.log[rows] {
                                    ui.label(RichText::new(line).monospace().size(11.5).color(t.foreground));
                                }
                            });
                    });
                }
            }
        });
        if let Some(id) = remove {
            self.list.retain(|j| j.id != id);
            if self.list.is_empty() {
                self.open = false;
            }
        }
    }
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[u]) }
}
