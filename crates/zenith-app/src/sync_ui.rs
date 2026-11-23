//! Runs Settings Sync operations in the background.

use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncOp {
    Setup,
    Push,
    Pull,
}

pub type SyncResult = (SyncOp, Result<String, String>);

/// Start `op` (Setup clones `url`, then pulls if the repository has settings).
pub fn start(op: SyncOp, config: PathBuf, url: String, waker: impl Fn() + Send + 'static) -> Receiver<SyncResult> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let r = match op {
            SyncOp::Setup => crate::sync::setup(&config, &url),
            SyncOp::Push => crate::sync::push(&config),
            SyncOp::Pull => crate::sync::pull(&config),
        };
        let _ = tx.send((op, r));
        waker();
    });
    rx
}
