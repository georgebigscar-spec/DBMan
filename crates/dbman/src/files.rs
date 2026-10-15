//! Native file dialogs and file writes off the UI thread, so a dialog or a
//! large export never freezes the window.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError, channel};

use egui::Context;
use shadcn_egui::{toast, toast_error};

/// The answer of a file dialog that is still open.
pub struct PendingPath(Receiver<Option<PathBuf>>);

impl PendingPath {
    /// `Some(answer)` once the dialog closed (`None` inside when cancelled).
    pub fn poll(&self) -> Option<Option<PathBuf>> {
        match self.0.try_recv() {
            Ok(p) => Some(p),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => Some(None),
        }
    }
}

/// Shows an open-file dialog without blocking the UI.
pub fn pick_file(ctx: &Context, dialog: rfd::AsyncFileDialog) -> PendingPath {
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let picked = pollster::block_on(dialog.pick_file()).map(|h| h.path().to_path_buf());
        let _ = tx.send(picked);
        ctx.request_repaint();
    });
    PendingPath(rx)
}

/// Asks where to save, then renders and writes the file, all off the UI thread,
/// and reports the outcome with a toast.
pub fn save_file(ctx: &Context, dialog: rfd::AsyncFileDialog, render: impl FnOnce() -> String + Send + 'static) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let Some(handle) = pollster::block_on(dialog.save_file()) else { return };
        let path = handle.path().to_path_buf();
        match std::fs::write(&path, render()) {
            Ok(()) => toast(&ctx, "Saved", Some(&path.to_string_lossy())),
            Err(e) => toast_error(&ctx, "Could not save the file", Some(&e.to_string())),
        }
        ctx.request_repaint();
    });
}
