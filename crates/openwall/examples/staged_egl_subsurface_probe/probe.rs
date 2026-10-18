// SPDX-License-Identifier: AGPL-3.0-or-later
#[path = "../owned_egl_wayland_probe/egl_window.rs"]
mod egl_window;
#[path = "../probe_support/gl_api.rs"]
mod gl_api;
#[path = "../owned_egl_wayland_probe/wayland.rs"]
mod wayland;
#[path = "../probe_support/wayland_poll.rs"]
mod wayland_poll;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use std::{
    io::{ErrorKind, Read, Write},
    os::{
        fd::AsFd,
        unix::net::{UnixDatagram, UnixStream},
    },
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(about = "Private-compositor synchronized EGL staging diagnostic")]
struct Args {
    #[arg(long)]
    output: String,
    /// Harness-owned path in a private temporary directory. Must not exist.
    #[arg(long)]
    control_socket: PathBuf,
    #[arg(long, default_value_t=25, value_parser=clap::value_parser!(u64).range(1..=30))]
    seconds: u64,
    #[arg(long, value_enum, default_value_t=gl_api::Api::Desktop)]
    gl_api: gl_api::Api,
    /// Negative control: allow the native child to intercept the test click.
    #[arg(long)]
    intercept_input: bool,
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    ensure!(
        std::env::var("OPENWALL_PRIVATE_COMPOSITOR").as_deref() == Ok("1"),
        "staged EGL probe requires OPENWALL_PRIVATE_COMPOSITOR=1 from its private harness"
    );
    // Diagnostic-only control channel: no deletion or replacement of existing paths.
    // The harness owns and removes the enclosing 0700 temporary directory.
    let control = UnixDatagram::bind(&args.control_socket)?;
    control.set_nonblocking(true)?;
    let (mut wake_read, wake_write) = UnixStream::pair()?;
    wake_read.set_nonblocking(true)?;
    wake_write.set_nonblocking(true)?;
    let stopped = Arc::new(AtomicBool::new(false));
    let signal = stopped.clone();
    let writer = Mutex::new(wake_write);
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::Relaxed);
        if let Ok(mut writer) = writer.lock() {
            let _ = writer.write_all(&[1]);
        }
    })?;
    let (conn, mut queue, mut state) = wayland::connect(&args.output, false)?;
    state.enable_staging(&queue.handle(), args.intercept_input)?;
    // Native owners drop before the child role and layer root on every exit path.
    let mut window: Option<egl_window::Window> = None;
    let mut swaps = 0;
    let mut geometry_refreshes = 0;
    let mut published = false;
    let mut published_size = None;
    let mut requested = false;
    let mut ready = false;
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    let mut feedback_deadline = None;
    loop {
        queue.dispatch_pending(&mut state)?;
        let mut bytes = [0; 64];
        while wake_read.read(&mut bytes).is_ok_and(|count| count > 0) {}
        let mut message = [0; 32];
        loop {
            match control.recv(&mut message) {
                Ok(count) => {
                    ensure!(
                        !requested && &message[..count] == b"publish\n",
                        "invalid or duplicate staged publish command"
                    );
                    requested = true;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) => return Err(error.into()),
            }
        }
        ensure!(!state.closed, "selected private output or witness closed");
        ensure!(
            !state.intercepted,
            "test pointer was intercepted by staged EGL child"
        );
        ensure!(
            state.discarded == 0,
            "published staged buffer was discarded"
        );
        ensure!(
            published || state.presented == 0,
            "unpublished staged buffer received presentation feedback"
        );
        if state.witness_buttons >= 2 && !ready {
            ensure!(
                published && state.presented == 1,
                "staged input arrived without presentation"
            );
            ready = true;
            eprintln!("Staged EGL presented; pointer reached underlying witness");
        }
        if stopped.load(Ordering::Relaxed) {
            break;
        }
        ensure!(Instant::now() < deadline, "staged EGL diagnostic timed out");
        if let Some(feedback_deadline) = feedback_deadline {
            ensure!(
                ready || Instant::now() < feedback_deadline,
                "staged presentation/input timed out"
            );
        }
        if state.dirty && state.configured.is_some() {
            // Mapping the previously empty layer may repeat its configure. A
            // same-size acknowledgement needs no new EGL buffer or parent commit.
            if published && state.configured == published_size {
                state.dirty = false;
            }
        }
        if state.dirty && state.configured.is_some() {
            ensure!(
                !published,
                "this diagnostic only stages geometry before first publication"
            );
            let (width, height) = state.configured.context("layer configure missing")?;
            if window.is_none() {
                window = Some(egl_window::Window::on_session(
                    egl_window::Session::new(&conn, args.gl_api)?,
                    state.render_surface()?,
                    width,
                    height,
                )?);
                window
                    .as_ref()
                    .context("native window missing")?
                    .disable_swap_throttle()?;
            }
            let native = window.as_ref().context("native window missing")?;
            native.paint(width, height)?;
            native.present()?;
            state.flush_staged()?;
            swaps += 1;
            state.dirty = native.take_refresh();
            geometry_refreshes += u32::from(state.dirty);
            ensure!(swaps <= 16, "staged swap limit exceeded");
            if state.dirty {
                continue;
            }
            eprintln!("Staged EGL cached {width}x{height}; parent held");
        }
        if requested && !published && !state.dirty && window.is_some() {
            let (width, height) = state.configured.context("layer configure missing")?;
            let native = window.as_ref().context("native window missing")?;
            native.paint(width, height)?;
            ensure!(
                !native.refresh_required(),
                "final staged buffer still requires a geometry refresh"
            );
            // Only this last cached commit requests feedback. Earlier buffers can
            // be superseded without waiting for presentation behind the parent gate.
            state.request_frame_feedback(&queue.handle(), 1)?;
            native.present()?;
            swaps += 1;
            state.publish_staged()?;
            published = true;
            published_size = Some((width, height));
            feedback_deadline = Some(Instant::now() + Duration::from_secs(5));
            eprintln!("Staged EGL parent published {width}x{height}");
        }
        state.inject_if_ready();
        wayland_poll::wait_fds(
            &mut queue,
            &conn,
            &[wake_read.as_fd(), control.as_fd()],
            feedback_deadline
                .filter(|_| !ready)
                .unwrap_or(deadline)
                .min(deadline),
        )?;
    }
    ensure!(
        !published || ready,
        "staged EGL stopped before presentation/input validation"
    );
    let native = window
        .as_ref()
        .context("staged EGL stopped before native initialization")?;
    let counts = native.counts();
    let mut report = serde_json::json!({"schema_version":1,
        "status":if published {"passed"} else {"cancelled_before_publish"},
        "scope":"synchronized_egl_child_in_private_compositor", "output":args.output,
        "swaps":swaps,"geometry_refreshes":geometry_refreshes,"published":published,
        "presented":state.presented,"presented_frame_ids":state.presented_frame_ids,
        "witness_button_events":state.witness_buttons,"logical_size":state.configured,
        "gl":native.report()?});
    eprintln!("Staged EGL removing private surfaces");
    drop(window);
    report["egl_lifecycle"] = counts.snapshot();
    drop(state);
    conn.flush()?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
