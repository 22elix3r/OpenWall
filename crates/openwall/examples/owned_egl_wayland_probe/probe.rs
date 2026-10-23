// SPDX-License-Identifier: AGPL-3.0-or-later
#[path = "../probe_support/quadrant_painter.rs"]
mod quadrant_painter;
use openwall::native_egl::gl_api;
use openwall::native_egl::window as egl_window;
use quadrant_painter::Quadrants;
#[path = "wayland.rs"]
mod wayland;

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use std::{
    io::{Read, Write},
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Parser)]
#[command(about = "Private-compositor ownership/input probe; never launch on a desktop session")]
struct Args {
    #[arg(long)]
    output: String,
    #[arg(long,default_value_t=15,value_parser=clap::value_parser!(u64).range(1..=30))]
    seconds: u64,
    /// Negative control: leave the EGL overlay's input region opaque.
    #[arg(long)]
    intercept_input: bool,
    #[arg(long, value_enum, default_value_t=gl_api::Api::Gles)]
    gl_api: gl_api::Api,
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    // Accidental-launch guard, not an OS sandbox. The harness creates its own
    // socket/runtime directory and passes no desktop socket or physical input.
    ensure!(
        std::env::var("OPENWALL_PRIVATE_COMPOSITOR").as_deref() == Ok("1"),
        "owned EGL Wayland probe requires OPENWALL_PRIVATE_COMPOSITOR=1 from its private harness"
    );
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
    let (conn, mut queue, mut state) = wayland::connect(&args.output, args.intercept_input)?;
    // Declared after State so native EGL wrappers always drop before protocol roots.
    let mut window: Option<egl_window::Window> = None;
    let mut creations = 0;
    let mut draws = 0;
    let mut ready = false;
    let mut reported_buttons = 0;
    let startup = Instant::now() + Duration::from_secs(10);
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    let mut feedback_deadline = None;
    loop {
        queue.dispatch_pending(&mut state)?;
        let mut bytes = [0; 64];
        while wake_read.read(&mut bytes).is_ok_and(|count| count > 0) {}
        ensure!(!state.closed, "selected private output or witness closed");
        ensure!(
            !state.intercepted,
            "test pointer was intercepted by EGL surface"
        );
        ensure!(
            state.discarded <= 3,
            "EGL commits repeatedly discarded by compositor"
        );
        if state.witness_buttons >= reported_buttons + 2 {
            reported_buttons = state.witness_buttons;
            eprintln!("Private EGL click-through confirmed ({reported_buttons} button events)");
        }
        if state.witness_buttons >= 2 && !ready {
            ready = true;
            eprintln!("Owned EGL presented; pointer reached underlying witness");
        }
        if stopped.load(Ordering::Relaxed) || Instant::now() >= deadline {
            break;
        }
        ensure!(
            ready || Instant::now() < startup,
            "owned EGL presentation/input readiness timed out"
        );
        if !state.pending_feedback {
            feedback_deadline = None;
        }
        if let Some(deadline) = feedback_deadline {
            ensure!(
                Instant::now() < deadline,
                "owned EGL presentation feedback timed out"
            );
        }
        if state.dirty && !state.pending_feedback {
            let (width, height) = state.configured.context("layer configure missing")?;
            egl_window::validate_size(width, height)?;
            if window.is_none() {
                window = Some(unsafe {
                    // SAFETY: The private State/tree retains this exclusive protocol surface and role until after its native window is dropped.
                    egl_window::Window::on_session(
                        egl_window::Session::new(&conn, args.gl_api)?,
                        state.surface.as_ref().context("native surface missing")?,
                        width,
                        height,
                    )
                }?);
                creations += 1;
            }
            state.request_feedback(&queue.handle())?;
            window
                .as_mut()
                .context("EGL window missing")?
                .draw(width, height)?;
            draws += 1;
            state.dirty = window
                .as_ref()
                .context("EGL window missing")?
                .take_refresh();
            feedback_deadline = Some(Instant::now() + Duration::from_secs(5));
            eprintln!("Owned EGL submitted {width}x{height}");
        }
        state.inject_if_ready();
        conn.flush()?;
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let next = [
            Some(deadline),
            feedback_deadline,
            (!ready).then_some(startup),
        ]
        .into_iter()
        .flatten()
        .min()
        .context("probe deadline missing")?;
        let remaining = next.saturating_duration_since(Instant::now());
        let timeout = Timespec {
            tv_sec: remaining.as_secs() as i64,
            tv_nsec: remaining.subsec_nanos() as i64,
        };
        let mut fds = [
            PollFd::new(&conn, PollFlags::IN),
            PollFd::new(&wake_read, PollFlags::IN),
        ];
        match poll(&mut fds, Some(&timeout)) {
            Ok(_)
                if fds[0]
                    .revents()
                    .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL) =>
            {
                bail!("private Wayland compositor disconnected")
            }
            Ok(_) if fds[0].revents().contains(PollFlags::IN) => {
                guard.read()?;
            }
            Ok(_) => drop(guard),
            Err(rustix::io::Errno::INTR) => drop(guard),
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(ready, "probe stopped before presentation/input validation");
    let counts = window.as_ref().context("EGL window missing")?.counts();
    let mut report = serde_json::json!({"schema_version":1,"status":"passed",
        "scope":"owned_egl_surface_in_private_compositor","output":args.output,
        "egl_context_creations":creations,"draws":draws,"presented":state.presented,
        "discarded":state.discarded,"witness_button_events":state.witness_buttons,
        "input_region":"empty","logical_size":state.configured,
        "gl":window.as_ref().context("EGL window missing")?.report()?});
    drop(window);
    report["egl_lifecycle"] = counts.snapshot();
    drop(state);
    conn.flush()?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
