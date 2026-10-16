//! Running a batch of statements that change the database (grid edits, DDL)
//! on a fresh session, so other tabs sharing a connection never see a
//! half-done transaction.

use dbman_core::{ConnectionConfig, DbError, DbKind, DbResult, StatementOutcome, dml};

use crate::i18n::trf;

/// How to run a batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// One by one; stops at the first error (DDL on MySQL commits by itself).
    Sequential,
    /// In one transaction where the engine supports transactional DDL
    /// (PostgreSQL, SQLite); `PRAGMA` statements run outside it.
    Transaction,
    /// In one transaction, and every UPDATE or DELETE must hit exactly one row.
    RowEdits,
}

async fn open(cfg: &ConnectionConfig, db: &str) -> DbResult<std::sync::Arc<dyn dbman_core::Connection>> {
    let mut cfg = cfg.clone();
    if cfg.kind == DbKind::MySql && !db.is_empty() {
        cfg.database = db.to_string();
    }
    dbman_drivers::connect(&cfg).await
}

/// Runs `stmts` in database `db`; returns how many ran.
pub async fn run(cfg: ConnectionConfig, db: String, stmts: Vec<String>, mode: Mode) -> DbResult<usize> {
    let kind = cfg.kind;
    let s = open(&cfg, &db).await?;
    let db = Some(db.as_str()).filter(|d| !d.is_empty());
    let transactional = match mode {
        Mode::Sequential => false,
        Mode::Transaction => kind != DbKind::MySql,
        Mode::RowEdits => true,
    };
    let is_pragma = |s: &str| s.trim_start().get(..6).is_some_and(|p| p.eq_ignore_ascii_case("PRAGMA"));
    let lead = if transactional { stmts.iter().take_while(|s| is_pragma(s)).count() } else { 0 };
    let tail = if transactional { stmts[lead..].iter().rev().take_while(|s| is_pragma(s)).count() } else { 0 };
    let body = &stmts[lead..stmts.len() - tail];

    for sql in &stmts[..lead] {
        s.execute(db, sql, 0).await.map_err(|e| with_sql(e, sql))?;
    }
    if transactional {
        s.execute(db, dml::begin(kind), 0).await?;
    }
    for sql in body {
        let err = match s.execute(db, sql, 0).await {
            Ok(StatementOutcome::Done { affected: Some(n) })
                if mode == Mode::RowEdits && n != 1 && !sql.starts_with("INSERT") =>
            {
                let msg = trf(
                    "{n} rows matched instead of 1, so nothing was saved. The row may have been changed by someone else; refresh and try again.",
                    &[("n", &n)],
                );
                Some(DbError::new(format!("{msg}\n\n{sql}")))
            }
            Ok(_) => None,
            Err(e) => Some(with_sql(e, sql)),
        };
        if let Some(e) = err {
            if transactional {
                let _ = s.execute(db, "ROLLBACK", 0).await;
            }
            for sql in &stmts[stmts.len() - tail..] {
                let _ = s.execute(db, sql, 0).await;
            }
            return Err(e);
        }
    }
    if transactional {
        s.execute(db, "COMMIT", 0).await?;
    }
    for sql in &stmts[stmts.len() - tail..] {
        s.execute(db, sql, 0).await.map_err(|e| with_sql(e, sql))?;
    }
    Ok(stmts.len())
}

fn with_sql(e: DbError, sql: &str) -> DbError {
    DbError::new(format!("{}\n\n{sql}", e.message))
}
