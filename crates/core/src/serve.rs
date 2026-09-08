//! Hot-index daemon: loads the index + pack once, serves queries over a
//! Unix domain socket (newline-delimited JSON), watches the corpus for
//! changes (dirty set over-included; verification reads live contents so
//! over-inclusion is free, only false negatives are fatal).
//!
//! Protocol: one JSON request per line, one JSON response per line.
//! Client helper [`query_via_daemon`] handles connect/send/recv; the CLI
//! auto-spawns a daemon under an exclusive lockfile and falls back to
//! in-process search on any failure (never hangs).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::index::BigramIndex;
use crate::query::{QueryAnswer, QueryOptions, run_query};

#[derive(Debug, Serialize, Deserialize)]
pub struct QueryRequest {
    pub pattern: String,
    #[serde(default)]
    pub fixed_strings: bool,
    #[serde(default)]
    pub case_insensitive: bool,
    #[serde(default)]
    pub check_stale: bool,
    #[serde(default)]
    pub mode: crate::search::OutputMode,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct QueryResponse {
    pub answer: Option<QueryAnswer>,
    pub error: Option<String>,
}

pub fn socket_path(index_dir: &Path) -> PathBuf {
    index_dir.join("serve.sock")
}

// ---------------------------------------------------------------------------
// client
// ---------------------------------------------------------------------------

/// Try one query through the daemon. Fast-fails if no listener.
pub fn query_via_daemon(socket: &Path, req: &QueryRequest) -> Result<QueryAnswer> {
    let mut stream =
        UnixStream::connect(socket).with_context(|| format!("connect to {}", socket.display()))?;
    write_request(&mut stream, req)?;
    read_response(&mut stream)
}

fn write_request(stream: &mut UnixStream, req: &QueryRequest) -> Result<()> {
    serde_json::to_writer(&mut *stream, req)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn read_response(stream: &mut UnixStream) -> Result<QueryAnswer> {
    let mut line = String::new();
    let mut reader = BufReader::new(stream);
    reader.read_line(&mut line)?;
    let resp: QueryResponse =
        serde_json::from_str(line.trim()).context("malformed daemon response")?;
    match resp.answer {
        Some(a) => Ok(a),
        None => bail!(resp.error.unwrap_or_else(|| "daemon error".into())),
    }
}

/// Spawn a detached daemon for `root` and wait until its socket accepts.
/// Returns Ok(()) even on spawn failure — the caller falls back silently.
pub fn try_spawn_daemon(current_exe: &Path, root: &Path, index_dir: &Path) -> Result<()> {
    let socket = socket_path(index_dir);
    let _ = std::fs::remove_file(&socket); // stale socket from a dead daemon

    use std::os::unix::process::CommandExt;
    let child = std::process::Command::new(current_exe)
        .arg("--serve")
        .arg(root)
        .process_group(0)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if child.is_err() {
        return Ok(()); // silent fallback
    }
    // Wait for the listener (index load can take a moment on big corpora).
    for _ in 0..200 {
        if UnixStream::connect(&socket).is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// server
// ---------------------------------------------------------------------------

struct Ctx {
    idx: std::sync::RwLock<Arc<BigramIndex>>,
    /// (mtime_ns, len) of index.bin at load time — generation marker.
    generation: Mutex<(SystemTime, u64)>,
    index_dir: PathBuf,
    root: PathBuf,
    dirty: Mutex<std::collections::HashSet<PathBuf>>,
    stop: AtomicBool,
}

impl Ctx {
    /// Snapshot the current index. If index.bin changed under us (rebuild
    /// while serving), reload it first: a stale index paired with the NEW
    /// contents.pack silently mis-slices every candidate (old spans, new
    /// bytes) — found the hard way at full parity. Reload is synchronous
    /// and single-threaded; dirty set resets (fresh index, nothing stale).
    fn current(&self) -> Arc<BigramIndex> {
        let gen_marker = self.detect_generation();
        let needs_reload = {
            let g = self.generation.lock().unwrap();
            *g != gen_marker
        };
        if needs_reload {
            let mut g = self.generation.lock().unwrap();
            // double-check: another thread may have reloaded already
            if *g != gen_marker {
                match BigramIndex::load(&self.index_dir) {
                    Ok(new_idx) => {
                        let arc = Arc::new(new_idx);
                        *self.idx.write().unwrap() = arc.clone();
                        *g = gen_marker;
                        self.dirty.lock().unwrap().clear();
                        eprintln!("[rrg-serve] index reloaded (generation changed)");
                    }
                    Err(e) => {
                        eprintln!("[rrg-serve] reload failed, serving stale index: {e:#}");
                        *g = gen_marker; // avoid reload-storm on a broken index
                    }
                }
            }
        }
        self.idx.read().unwrap().clone()
    }

    fn detect_generation(&self) -> (SystemTime, u64) {
        let p = self.index_dir.join("index.bin");
        match std::fs::metadata(&p) {
            Ok(m) => (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len()),
            Err(_) => (std::time::UNIX_EPOCH, 0),
        }
    }
}

/// Run the daemon until killed. `build_if_missing`: build the index first
/// when absent (nice for `rrg --serve` on a fresh repo).
pub fn serve(root: &Path, build_if_missing: bool) -> Result<()> {
    let index_dir = root.join(".rrg-index");
    let idx = if index_dir.join("index.bin").exists() {
        BigramIndex::load(&index_dir)?
    } else if build_if_missing {
        let (idx, timings) = BigramIndex::build(root, &Default::default(), Some(&index_dir))?;
        idx.save(&index_dir)?;
        eprintln!(
            "[rrg-serve] indexed {} files in {:.1?} + {:.1?} + {:.1?}",
            timings.files, timings.walk, timings.fill, timings.trigrams
        );
        idx
    } else {
        bail!(
            "no index at {} (run `rrg --build-index` first)",
            index_dir.display()
        );
    };
    eprintln!(
        "[rrg-serve] index loaded: {} files, {:.1} MiB",
        idx.paths.len(),
        idx.memory_bytes() as f64 / (1024.0 * 1024.0)
    );

    let socket = socket_path(&index_dir);
    let _ = std::fs::remove_file(&socket); // stale socket
    let listener = match UnixListener::bind(&socket) {
        Ok(l) => l,
        Err(_) if UnixStream::connect(&socket).is_ok() => {
            eprintln!("[rrg-serve] daemon already running on {}", socket.display());
            return Ok(());
        }
        Err(e) => {
            let _ = std::fs::remove_file(&socket); // dead socket, retry once
            UnixListener::bind(&socket)
                .with_context(|| format!("bind {} ({e})", socket.display()))?
        }
    };
    eprintln!("[rrg-serve] listening on {}", socket.display());

    let ctx = Arc::new(Ctx {
        idx: std::sync::RwLock::new(Arc::new(idx)),
        generation: Mutex::new((std::time::UNIX_EPOCH, 0)),
        index_dir: index_dir.clone(),
        root: root.to_path_buf(),
        dirty: Mutex::new(Default::default()),
        stop: AtomicBool::new(false),
    });
    // Record the real generation (post-load stat of index.bin).
    *ctx.generation.lock().unwrap() = ctx.detect_generation();

    // Watcher: dirty-set over-inclusion. Verification reads live contents,
    // so false-positive dirtiness costs a scan; false negatives would be
    // fatal, and notify's coalescing/drops are mitigated by --check-stale.
    spawn_watcher(root.to_path_buf(), ctx.clone())?;

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = handle_conn(stream, ctx);
        });
    }
    Ok(())
}

fn spawn_watcher(root: PathBuf, ctx: Arc<Ctx>) -> Result<()> {
    use notify::Watcher;
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })?;
    watcher.watch(&root, notify::RecursiveMode::Recursive)?;
    // The watcher must stay alive; move it into a leak-owned holder.
    std::thread::spawn(move || {
        let _watcher = watcher; // keep alive
        for res in rx {
            if let Ok(event) = res {
                let mut dirty = ctx.dirty.lock().unwrap();
                for p in event.paths {
                    dirty.insert(p.to_path_buf());
                }
            }
            if ctx.stop.load(Ordering::Relaxed) {
                break;
            }
        }
    });
    Ok(())
}

fn handle_conn(stream: UnixStream, ctx: Arc<Ctx>) -> Result<()> {
    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(()); // client closed
        }
        let Ok(req) = serde_json::from_str::<QueryRequest>(line.trim()) else {
            write_response(&stream, None, Some("malformed request".into()))?;
            continue;
        };
        // Dirty paths that are indexed get force-verified (live contents).
        let idx = ctx.current();
        let forced: Vec<PathBuf> = {
            let dirty = ctx.dirty.lock().unwrap();
            dirty
                .iter()
                .filter(|p| idx.paths.iter().any(|ip| ip == *p))
                .cloned()
                .collect()
        };
        let qopts = QueryOptions {
            fixed_strings: req.fixed_strings,
            case_insensitive: req.case_insensitive,
            check_stale: req.check_stale,
            mode: req.mode,
        };
        let answer = crate::query::run_query(&idx, &ctx.index_dir, &req.pattern, &qopts, &forced);
        match answer {
            Ok(a) => write_response(&stream, Some(a), None)?,
            Err(e) => write_response(&stream, None, Some(format!("{e:#}")))?,
        }
    }
}

fn write_response(
    stream: &UnixStream,
    answer: Option<QueryAnswer>,
    error: Option<String>,
) -> Result<()> {
    let mut stream = stream;
    serde_json::to_writer(&mut stream, &QueryResponse { answer, error })?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}
