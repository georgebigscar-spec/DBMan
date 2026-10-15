//! Runs backup and restore jobs: the engines' dump tools as child processes,
//! and SQLite's online backup API on a blocking thread. Jobs report log lines
//! and progress through a callback and stop when their cancel flag is set
//! (child processes are killed, SQLite copies stop between steps).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use dbman_core::backup::{SqliteMethod, Tool, ToolCommand};
use dbman_core::{DbError, DbResult};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

/// What a running job tells the UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    Log(String),
    /// Bytes written to the backup so far.
    Bytes(u64),
    /// Share of the work done, 0.0–1.0, when it can be known.
    Fraction(f32),
}

pub type Report = Arc<dyn Fn(Progress) + Send + Sync>;

/// Set to stop a job; it then ends with a "cancelled" error.
pub type Cancel = Arc<AtomicBool>;

fn cancelled() -> DbError {
    DbError::new("cancelled")
}

/// Where to look for `tool`: `extra_dir` first, then `PATH`, then the places
/// installers usually put it (Homebrew, Postgres.app, Program Files, Debian's
/// versioned directories; newest version first).
pub fn find_tool(tool: Tool, extra_dir: Option<&Path>) -> Option<PathBuf> {
    let exe = if cfg!(windows) { format!("{}.exe", tool.program()) } else { tool.program().to_string() };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = extra_dir.filter(|d| !d.as_os_str().is_empty()) {
        dirs.push(d.to_path_buf());
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.extend(known_dirs(tool));
    dirs.into_iter().map(|d| d.join(&exe)).find(|p| p.is_file())
}

/// Subdirectories of `parent` whose names start with `prefix`, newest-looking first.
fn versioned(parent: &str, prefix: &str, suffix: &str) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(parent) else { return Vec::new() };
    let mut names: Vec<String> = rd
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(prefix))
        .collect();
    // Compare version numbers numerically ("16" after "9").
    names.sort_by_key(|n| {
        n[prefix.len()..].split(|c: char| !c.is_ascii_digit()).filter_map(|p| p.parse::<u32>().ok()).collect::<Vec<_>>()
    });
    names.into_iter().rev().map(|n| Path::new(parent).join(n).join(suffix)).collect()
}

fn known_dirs(tool: Tool) -> Vec<PathBuf> {
    let pg = matches!(tool, Tool::PgDump | Tool::PgRestore | Tool::Psql);
    let mut dirs = Vec::new();
    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        if pg {
            dirs.push(PathBuf::from("/Applications/Postgres.app/Contents/Versions/latest/bin"));
            dirs.push(PathBuf::from("/opt/homebrew/opt/libpq/bin"));
            dirs.push(PathBuf::from("/usr/local/opt/libpq/bin"));
            dirs.extend(versioned("/opt/homebrew/opt", "postgresql@", "bin"));
        } else {
            dirs.push(PathBuf::from("/opt/homebrew/opt/mysql-client/bin"));
            dirs.push(PathBuf::from("/usr/local/opt/mysql-client/bin"));
            dirs.push(PathBuf::from("/usr/local/mysql/bin"));
            dirs.push(PathBuf::from("/opt/homebrew/opt/mariadb/bin"));
        }
    } else if cfg!(windows) {
        if pg {
            dirs.extend(versioned(r"C:\Program Files\PostgreSQL", "", "bin"));
        } else {
            dirs.extend(versioned(r"C:\Program Files\MySQL", "MySQL Server ", "bin"));
            dirs.extend(versioned(r"C:\Program Files", "MariaDB ", "bin"));
        }
    } else if pg {
        dirs.extend(versioned("/usr/lib/postgresql", "", "bin"));
    }
    dirs
}

/// Size of a file, or of every file under a directory.
fn size_of(path: &Path) -> u64 {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => {
            std::fs::read_dir(path).map(|rd| rd.filter_map(|e| e.ok()).map(|e| size_of(&e.path())).sum()).unwrap_or(0)
        }
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// Runs `cmd` with `program`, streaming its output to the log. `watch` is the
/// file or directory being written, reported as `Bytes`. A file given as
/// standard input is fed by DBMan, which reports how much of it was read.
pub async fn run_tool(
    program: &Path,
    cmd: &ToolCommand,
    watch: Option<&Path>,
    report: Report,
    cancel: Cancel,
) -> DbResult<()> {
    report(Progress::Log(format!("$ {}", cmd.display())));
    let mut child = tokio::process::Command::new(program);
    child.args(&cmd.args).envs(cmd.env.iter().map(|(k, v)| (k, v))).kill_on_drop(true);
    child.stdin(if cmd.stdin.is_some() { Stdio::piped() } else { Stdio::null() });
    child.stderr(Stdio::piped());
    match &cmd.stdout {
        Some(p) => {
            let f = std::fs::File::create(p).map_err(|e| DbError::new(format!("{}: {e}", p.display())))?;
            child.stdout(Stdio::from(f));
        }
        None => {
            child.stdout(Stdio::piped());
        }
    }
    #[cfg(windows)]
    {
        // No console window flashing up for every dump.
        child.creation_flags(0x0800_0000);
    }
    let mut child = child.spawn().map_err(|e| DbError::new(format!("could not start {}: {e}", program.display())))?;

    // Errors are on stderr; keep the tail for the failure message.
    let tail = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let pump = |stream: Option<Box<dyn tokio::io::AsyncRead + Unpin + Send>>, keep: bool| {
        let report = report.clone();
        let tail = tail.clone();
        async move {
            let Some(s) = stream else { return };
            let mut lines = BufReader::new(s).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if keep && let Ok(mut t) = tail.lock() {
                    t.push(line.clone());
                    if t.len() > 20 {
                        t.remove(0);
                    }
                }
                report(Progress::Log(line));
            }
        }
    };
    let stderr = pump(child.stderr.take().map(|s| Box::new(s) as _), true);
    let stdout = pump(child.stdout.take().map(|s| Box::new(s) as _), false);

    let feed = {
        let input = cmd.stdin.clone();
        let stdin = child.stdin.take();
        let report = report.clone();
        async move {
            let (Some(path), Some(mut stdin)) = (input, stdin) else { return Ok(()) };
            let mut file =
                tokio::fs::File::open(&path).await.map_err(|e| DbError::new(format!("{}: {e}", path.display())))?;
            let total = file.metadata().await.map(|m| m.len()).unwrap_or(0).max(1);
            let mut buf = vec![0u8; 256 * 1024];
            let mut done = 0u64;
            loop {
                let n = file.read(&mut buf).await.map_err(DbError::from_display)?;
                if n == 0 {
                    break;
                }
                // The tool may exit early on an error; its stderr says why.
                if stdin.write_all(&buf[..n]).await.is_err() {
                    break;
                }
                done += n as u64;
                report(Progress::Fraction(done as f32 / total as f32));
            }
            drop(stdin);
            Ok::<(), DbError>(())
        }
    };

    let watcher = async {
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            tick.tick().await;
            if cancel.load(Ordering::Relaxed) {
                return;
            }
            if let Some(w) = watch {
                report(Progress::Bytes(size_of(w)));
            }
        }
    };

    let work = async {
        let (fed, _, _, status) = tokio::join!(feed, stderr, stdout, child.wait());
        fed?;
        status.map_err(DbError::from_display)
    };
    let status = tokio::select! {
        r = work => r?,
        _ = watcher => {
            let _ = child.kill().await;
            return Err(cancelled());
        }
    };
    if let Some(w) = watch {
        report(Progress::Bytes(size_of(w)));
    }
    if status.success() {
        return Ok(());
    }
    let tail = tail.lock().map(|t| t.join("\n")).unwrap_or_default();
    let code = status.code().map_or_else(|| "a signal".to_string(), |c| format!("code {c}"));
    Err(DbError::new(if tail.is_empty() {
        format!("{} exited with {code}", cmd.tool.program())
    } else {
        format!("{} exited with {code}:\n{tail}", cmd.tool.program())
    }))
}

/// First line of `program --version`, for the log and the dialog.
pub async fn tool_version(program: &Path) -> Option<String> {
    let out = tokio::process::Command::new(program).arg("--version").kill_on_drop(true).output().await.ok()?;
    String::from_utf8_lossy(&out.stdout).lines().next().map(|l| l.trim().to_string())
}

/// Copies the SQLite database `from` into `to` page by page with the online
/// backup API. Used both ways: database → backup file, and backup file →
/// database (restore). Other connections may keep using the database.
fn copy_pages(
    from: &rusqlite::Connection,
    to: &mut rusqlite::Connection,
    report: &Report,
    cancel: &Cancel,
) -> DbResult<()> {
    use rusqlite::backup::{Backup, StepResult};
    let backup = Backup::new(from, to).map_err(DbError::from_display)?;
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        let step = backup.step(256).map_err(DbError::from_display)?;
        let p = backup.progress();
        if p.pagecount > 0 {
            report(Progress::Fraction((p.pagecount - p.remaining) as f32 / p.pagecount as f32));
        }
        match step {
            StepResult::Done => return Ok(()),
            StepResult::More => {}
            // Someone holds a lock: wait a little and go on.
            StepResult::Busy | StepResult::Locked => std::thread::sleep(Duration::from_millis(50)),
            _ => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn open(path: &Path) -> DbResult<rusqlite::Connection> {
    let c = rusqlite::Connection::open(path).map_err(|e| DbError::new(format!("{}: {e}", path.display())))?;
    c.busy_timeout(Duration::from_secs(10)).map_err(DbError::from_display)?;
    Ok(c)
}

/// Backs up the SQLite database `db` into the new file `target`.
pub async fn sqlite_backup(
    db: PathBuf,
    target: PathBuf,
    method: SqliteMethod,
    report: Report,
    cancel: Cancel,
) -> DbResult<()> {
    tokio::task::spawn_blocking(move || {
        if target.exists() {
            std::fs::remove_file(&target).map_err(|e| DbError::new(format!("{}: {e}", target.display())))?;
        }
        let src = open(&db)?;
        match method {
            SqliteMethod::OnlineBackup => {
                report(Progress::Log(format!("Copying pages of {} to {}", db.display(), target.display())));
                let mut dst = open(&target)?;
                let r = copy_pages(&src, &mut dst, &report, &cancel);
                drop(dst);
                if r.is_err() {
                    let _ = std::fs::remove_file(&target);
                }
                r?;
            }
            SqliteMethod::VacuumInto => {
                report(Progress::Log(format!("VACUUM INTO {}", target.display())));
                // VACUUM has no steps to check between, so a watcher interrupts it.
                let interrupt = src.get_interrupt_handle();
                let done = Arc::new(AtomicBool::new(false));
                let watcher = {
                    let (done, cancel) = (done.clone(), cancel.clone());
                    std::thread::spawn(move || {
                        while !done.load(Ordering::Relaxed) {
                            if cancel.load(Ordering::Relaxed) {
                                interrupt.interrupt();
                                return;
                            }
                            std::thread::sleep(Duration::from_millis(100));
                        }
                    })
                };
                let r = src.execute("VACUUM INTO ?1", [target.to_string_lossy()]);
                done.store(true, Ordering::Relaxed);
                let _ = watcher.join();
                if r.is_err() {
                    let _ = std::fs::remove_file(&target);
                }
                if cancel.load(Ordering::Relaxed) {
                    return Err(cancelled());
                }
                r.map_err(DbError::from_display)?;
                report(Progress::Fraction(1.0));
            }
        }
        report(Progress::Bytes(size_of(&target)));
        let check: String =
            open(&target)?.query_row("PRAGMA quick_check", [], |r| r.get(0)).map_err(DbError::from_display)?;
        report(Progress::Log(format!("quick_check: {check}")));
        Ok(())
    })
    .await
    .map_err(DbError::from_display)?
}

/// Replaces the content of the SQLite database `db` with the backup `source`.
pub async fn sqlite_restore(source: PathBuf, db: PathBuf, report: Report, cancel: Cancel) -> DbResult<()> {
    tokio::task::spawn_blocking(move || {
        let mut head = [0u8; 16];
        use std::io::Read;
        std::fs::File::open(&source)
            .and_then(|mut f| f.read_exact(&mut head))
            .map_err(|e| DbError::new(format!("{}: {e}", source.display())))?;
        if &head != b"SQLite format 3\0" {
            return Err(DbError::new(format!("{} is not an SQLite database", source.display())));
        }
        report(Progress::Log(format!("Copying pages of {} into {}", source.display(), db.display())));
        let src = rusqlite::Connection::open_with_flags(&source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(DbError::from_display)?;
        let mut dst = open(&db)?;
        copy_pages(&src, &mut dst, &report, &cancel)
    })
    .await
    .map_err(DbError::from_display)?
}
