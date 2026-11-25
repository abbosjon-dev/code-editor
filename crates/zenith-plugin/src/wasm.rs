//! WebAssembly plugins: a WASI command module (`wasm32-wasip1`) that speaks
//! the same JSON-lines protocol on stdin/stdout, run in-process with
//! wasmtime. It is sandboxed: no files, no network, no environment
//! variables, no clock-based tricks beyond what WASI exposes — it only sees
//! the messages the editor sends it.

use std::io::{PipeReader, PipeWriter};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use anyhow::{anyhow, Context, Result};
use wasi_common::pipe::{ReadPipe, WritePipe};
use wasi_common::sync::WasiCtxBuilder;
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

/// Compile and start `path`; returns the process and its stdout / stderr.
pub fn spawn(path: &Path, id: &str) -> Result<(WasmProcess, PipeReader, PipeReader)> {
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
