// SPDX-License-Identifier: AGPL-3.0-or-later
//! Finite, bounded queries to an owned worker; wallpaper playback uses Worker.
use std::{
    io,
    process::{Command, Output},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

static CLOSING: AtomicBool = AtomicBool::new(false);
static QUERIES: Mutex<Vec<JoinHandle<()>>> = Mutex::new(Vec::new());

/// Cancellation belongs to one query; cancelling it does not close the application.
#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub fn is_closing() -> bool {
    CLOSING.load(Ordering::Acquire)
}

pub fn spawn(work: impl FnOnce() + Send + 'static) -> io::Result<()> {
    let mut queries = QUERIES.lock().unwrap_or_else(|poison| poison.into_inner());
    if is_closing() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "application is closing",
        ));
    }
    let mut index = 0;
    while index < queries.len() {
        if queries[index].is_finished() {
            let _ = queries.swap_remove(index).join();
        } else {
            index += 1;
        }
    }
    if queries.len() >= 8 {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "too many pending player queries",
        ));
    }
    queries.push(
        std::thread::Builder::new()
            .name("openwall-query".into())
            .spawn(work)?,
    );
    Ok(())
}

/// Called while the Qt objects still exist, after its event loop exits.
pub fn shutdown() {
    let queries = {
        let mut queries = QUERIES.lock().unwrap_or_else(|poison| poison.into_inner());
        CLOSING.store(true, Ordering::Release);
        std::mem::take(&mut *queries)
    };
    for query in queries {
        let _ = query.join();
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "query/unix.rs"]
mod platform;

pub fn run(command: &mut Command, timeout: Duration) -> io::Result<Output> {
    run_cancellable(command, timeout, &Cancellation::default())
}

pub fn run_cancellable(
    command: &mut Command,
    timeout: Duration,
    cancellation: &Cancellation,
) -> io::Result<Output> {
    if is_closing() || cancellation.is_cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "player query was cancelled",
        ));
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    return platform::run(command, timeout, cancellation);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (command, timeout, cancellation);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "worker queries require macOS or Linux",
        ))
    }
}
