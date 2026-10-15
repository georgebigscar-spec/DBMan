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

/// Which native dialog to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    OpenFile,
    Folder,
    SaveAs,
}

/// Shows a file dialog without blocking the UI.
pub fn pick(ctx: &Context, dialog: rfd::AsyncFileDialog, kind: Pick) -> PendingPath {
    let (tx, rx) = channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let handle = pollster::block_on(async move {
            match kind {
                Pick::OpenFile => dialog.pick_file().await,
                Pick::Folder => dialog.pick_folder().await,
                Pick::SaveAs => dialog.save_file().await,
            }
        });
        let _ = tx.send(handle.map(|h| h.path().to_path_buf()));
        ctx.request_repaint();
    });
    PendingPath(rx)
}

/// Shows an open-file dialog without blocking the UI.
pub fn pick_file(ctx: &Context, dialog: rfd::AsyncFileDialog) -> PendingPath {
    pick(ctx, dialog, Pick::OpenFile)
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
