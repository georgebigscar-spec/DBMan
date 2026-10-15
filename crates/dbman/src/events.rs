//! Background work: database calls run on the tokio runtime and report back
//! through a channel that the UI drains every frame.

use std::future::Future;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use dbman_core::{Connection, DbObject, DbResult, ObjectPath, StatementOutcome, TableDetails};
use uuid::Uuid;

pub type TabId = u64;

/// Results of background work.
pub enum Event {
    Connected {
        conn: Uuid,
        result: DbResult<(Arc<dyn Connection>, String)>,
    },
    Databases {
        conn: Uuid,
        result: DbResult<Vec<String>>,
    },
    Schemas {
        conn: Uuid,
        db: String,
        result: DbResult<Vec<String>>,
    },
    Objects {
        conn: Uuid,
        db: String,
        schema: String,
        result: DbResult<Vec<DbObject>>,
    },
    Details {
        conn: Uuid,
        path: ObjectPath,
        result: DbResult<TableDetails>,
    },
    TestedConnection {
        result: DbResult<String>,
    },
    /// Statements of a confirm dialog ran.
    PromptDone {
        result: DbResult<usize>,
    },
    Tab {
        tab: TabId,
        event: TabEvent,
    },
}

pub enum TabEvent {
    /// A console opened its own session.
    Session(DbResult<Arc<dyn Connection>>),
    /// One statement of a console run finished.
    Statement {
        sql: String,
        elapsed: Duration,
        result: DbResult<StatementOutcome>,
    },
    /// The console run is over (all statements done or stopped at an error).
    RunFinished,
    Page {
        elapsed: Duration,
        result: DbResult<StatementOutcome>,
    },
    Ddl(DbResult<String>),
    /// Grid edits were written; the number of statements run.
    Submitted(DbResult<usize>),
    /// DDL from the table editor ran.
    Executed(DbResult<usize>),
    Details(DbResult<TableDetails>),
}

/// Spawns futures on the runtime and wakes the UI when they finish.
#[derive(Clone)]
pub struct Tasks {
    runtime: tokio::runtime::Handle,
    tx: Sender<Event>,
    ctx: egui::Context,
}

impl Tasks {
    pub fn new(runtime: tokio::runtime::Handle, ctx: egui::Context) -> (Self, Receiver<Event>) {
        let (tx, rx) = channel();
        (Self { runtime, tx, ctx }, rx)
    }

    /// Runs `fut` and delivers its event.
    pub fn spawn(&self, fut: impl Future<Output = Event> + Send + 'static) {
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        self.runtime.spawn(async move {
            let ev = fut.await;
            let _ = tx.send(ev);
            ctx.request_repaint();
        });
    }

    /// A sender for tasks that report several events.
    pub fn sender(&self) -> EventSender {
        EventSender { tx: self.tx.clone(), ctx: self.ctx.clone() }
    }

    pub fn runtime(&self) -> &tokio::runtime::Handle {
        &self.runtime
    }
}

#[derive(Clone)]
pub struct EventSender {
    tx: Sender<Event>,
    ctx: egui::Context,
}

impl EventSender {
    pub fn send(&self, ev: Event) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }
}

/// Measures a database call.
pub async fn timed<T>(fut: impl Future<Output = T>) -> (Duration, T) {
    let start = Instant::now();
    let out = fut.await;
    (start.elapsed(), out)
}
