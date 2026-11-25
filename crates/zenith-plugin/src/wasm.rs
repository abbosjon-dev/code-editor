//! WebAssembly plugins: a WASI command module (`wasm32-wasip1`) that speaks
//! the same JSON-lines protocol on stdin/stdout, run in-process with
//! wasmtime. It is sandboxed: no network, no environment variables, and no
//! files — unless its manifest asks for `read_workspace`, which shows the
//! workspace at `/workspace`, read-only.

use std::io::{PipeReader, PipeWriter};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::{anyhow, Context, Result};
use wasi_common::dir::{OpenResult, ReaddirCursor, ReaddirEntity};
use wasi_common::file::{FdFlags, Filestat, OFlags};
use wasi_common::pipe::{ReadPipe, WritePipe};
use wasi_common::sync::WasiCtxBuilder;
use wasi_common::{ErrorExt, WasiDir};
use wasmtime::{Config, Engine, Linker, Module, Store};

/// A running WASM plugin.
pub struct WasmProcess {
    engine: Engine,
    pub stdin: Option<PipeWriter>,
    done: Arc<AtomicBool>,
}

impl WasmProcess {
    pub fn is_running(&self) -> bool {
        !self.done.load(Ordering::SeqCst)
    }

    /// Close its stdin and interrupt it if it's still busy.
    pub fn kill(&mut self) {
        self.stdin = None;
        self.engine.increment_epoch();
    }
}

impl Drop for WasmProcess {
    fn drop(&mut self) {
        self.kill();
    }
}

/// wasmtime's own error type as an anyhow error.
fn w(e: impl std::fmt::Display) -> anyhow::Error {
    anyhow!("{e}")
}

/// A directory the module may read but not change: every call that would
/// write is refused, and folders opened through it are wrapped the same way.
struct ReadOnlyDir(Box<dyn WasiDir>);

#[async_trait::async_trait]
impl WasiDir for ReadOnlyDir {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn open_file(
        &self,
        symlink_follow: bool,
        path: &str,
        oflags: OFlags,
        read: bool,
        write: bool,
        fdflags: FdFlags,
    ) -> Result<OpenResult, wasi_common::Error> {
        if write || oflags.intersects(OFlags::CREATE | OFlags::TRUNCATE | OFlags::EXCLUSIVE) {
            return Err(wasi_common::Error::perm());
        }
        Ok(match self.0.open_file(symlink_follow, path, oflags, read, false, fdflags).await? {
            OpenResult::Dir(d) => OpenResult::Dir(Box::new(ReadOnlyDir(d))),
            f => f,
        })
    }

    async fn readdir(
        &self,
        cursor: ReaddirCursor,
    ) -> Result<Box<dyn Iterator<Item = Result<ReaddirEntity, wasi_common::Error>> + Send>, wasi_common::Error> {
        self.0.readdir(cursor).await
    }

    async fn read_link(&self, path: &str) -> Result<std::path::PathBuf, wasi_common::Error> {
        self.0.read_link(path).await
    }

    async fn get_filestat(&self) -> Result<Filestat, wasi_common::Error> {
        self.0.get_filestat().await
    }

    async fn get_path_filestat(&self, path: &str, follow: bool) -> Result<Filestat, wasi_common::Error> {
        self.0.get_path_filestat(path, follow).await
    }
}

/// Where a WASM plugin sees the workspace (with `read_workspace`).
pub const GUEST_WORKSPACE: &str = "/workspace";

/// Compile and start `path`; returns the process and its stdout / stderr.
/// `workspace`: a folder to show read-only at [`GUEST_WORKSPACE`].
pub fn spawn(path: &Path, id: &str, workspace: Option<&Path>) -> Result<(WasmProcess, PipeReader, PipeReader)> {
    let mut config = Config::new();
    // Lets `kill` interrupt a plugin stuck in a loop.
    config.epoch_interruption(true);
    let engine = Engine::new(&config).map_err(w)?;
    let module = Module::from_file(&engine, path).map_err(w).with_context(|| format!("loading {}", path.display()))?;

    let (in_r, in_w) = std::io::pipe()?;
    let (out_r, out_w) = std::io::pipe()?;
    let (err_r, err_w) = std::io::pipe()?;
    let wasi = WasiCtxBuilder::new()
        .stdin(Box::new(ReadPipe::new(in_r)))
        .stdout(Box::new(WritePipe::new(out_w)))
        .stderr(Box::new(WritePipe::new(err_w)))
        .arg(id)
        .map_err(w)?
        .build();
    if let Some(ws) = workspace {
        let dir = wasi_common::sync::Dir::open_ambient_dir(ws, wasi_common::sync::ambient_authority())
            .with_context(|| format!("opening {}", ws.display()))?;
        let dir = wasi_common::sync::dir::Dir::from_cap_std(dir);
        wasi.push_preopened_dir(Box::new(ReadOnlyDir(Box::new(dir))), GUEST_WORKSPACE).map_err(w)?;
    }
    let mut linker: Linker<wasi_common::WasiCtx> = Linker::new(&engine);
    wasi_common::sync::add_to_linker(&mut linker, |s| s).map_err(w)?;
    let mut store = Store::new(&engine, wasi);
    store.set_epoch_deadline(1);
    let instance = linker.instantiate(&mut store, &module).map_err(w)?;
    let start = instance
        .get_typed_func::<(), ()>(&mut store, "_start")
        .map_err(w)
        .context("the module has no `_start` (build it as a WASI command, e.g. --target wasm32-wasip1)")?;

    let done = Arc::new(AtomicBool::new(false));
    let flag = done.clone();
    let name = id.to_string();
    thread::Builder::new().name(format!("wasm-{id}")).spawn(move || {
        if let Err(e) = start.call(&mut store, ()) {
            // exit(0) arrives as an error too.
            let clean = e.downcast_ref::<wasi_common::I32Exit>().is_some_and(|c| c.0 == 0);
            if !clean {
                log::warn!("wasm plugin {name}: {e:#}");
            }
        }
        // Dropping the store closes stdout, so the reader sees the end.
        drop(store);
        flag.store(true, Ordering::SeqCst);
    })?;
    Ok((WasmProcess { engine, stdin: Some(in_w), done }, out_r, err_r))
}
