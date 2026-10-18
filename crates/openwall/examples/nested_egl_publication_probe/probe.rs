// SPDX-License-Identifier: AGPL-3.0-or-later
mod control;
#[path = "../probe_support/quadrant_painter.rs"]
mod quadrant_painter;
use openwall::native_egl::gl_api;
use openwall::native_egl::window as egl_window;
use quadrant_painter::Quadrants;
#[path = "../owned_egl_wayland_probe/wayland.rs"]
mod wayland;
#[path = "../probe_support/wayland_poll.rs"]
mod wayland_poll;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use control::Command;
use std::{
    io::{Read, Write},
    os::{fd::AsFd, unix::net::UnixStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use wayland_client::Proxy;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    output: String,
    #[arg(long)]
    control_socket: PathBuf,
    #[arg(long, value_enum, default_value_t=gl_api::Api::Desktop)]
    gl_api: gl_api::Api,
    #[arg(long, default_value_t=30, value_parser=clap::value_parser!(u64).range(1..=30))]
    seconds: u64,
    #[arg(long)]
    intercept_input: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Staging,
    Publishing,
    Published,
    RollingBack,
    RolledBack,
    Aborted,
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    ensure!(
        std::env::var("OPENWALL_PRIVATE_COMPOSITOR").as_deref() == Ok("1"),
        "nested EGL proof requires OPENWALL_PRIVATE_COMPOSITOR=1 from its private harness"
    );
    let control = control::Control::bind(&args.control_socket)?;
    let (mut reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    let stopped = Arc::new(AtomicBool::new(false));
    let signal = stopped.clone();
    let writer = Mutex::new(writer);
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::Relaxed);
        if let Ok(mut writer) = writer.lock() {
            let _ = writer.write_all(&[1]);
        }
    })?;
    let (connection, mut queue, mut state) = wayland::connect(&args.output, false)?;
    let startup = Instant::now() + Duration::from_secs(10);
    while state.configured.is_none() {
        queue.dispatch_pending(&mut state)?;
        ensure!(
            !state.closed && !stopped.load(Ordering::Relaxed) && Instant::now() < startup,
            "nested configure failed or timed out"
        );
        if state.configured.is_none() {
            wayland_poll::wait(&mut queue, &connection, &reader, startup)?;
        }
    }
    let root_id = state
        .surface
        .as_ref()
        .context("root missing")?
        .id()
        .protocol_id();
    let previous = [47, 81, 135];
    state.enable_nested_staging(&queue.handle(), previous, args.intercept_input)?;
    let session = egl_window::Session::new(&connection, args.gl_api)?;
    let counts = session.counts();
    let mut window: Option<egl_window::Window> = None;
    let mut phase = Phase::Staging;
    let mut publish_requested = false;
    let mut swaps = 0;
    let mut refreshes = 0;
    let mut prepared = false;
    let mut published_ready = false;
    let mut rollback_ready = false;
    let mut fixed_size = None;
    let mut validation_deadline = None;
    let mut aborted_lifecycle = None;
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    loop {
        queue.dispatch_pending(&mut state)?;
        let mut bytes = [0; 64];
        while reader.read(&mut bytes).is_ok_and(|count| count > 0) {}
        ensure!(!state.closed, "nested private output or witness closed");
        ensure!(
            !state.intercepted,
            "nested candidate intercepted private input"
        );
        ensure!(state.discarded == 0, "nested presentation was discarded");
        ensure!(
            state
                .surface
                .as_ref()
                .context("root missing")?
                .id()
                .protocol_id()
                == root_id,
            "desktop root changed"
        );
        if phase != Phase::Staging {
            ensure!(
                state.configured == fixed_size,
                "this diagnostic resizes only before candidate publication"
            );
            // Same-size mapping/configure acknowledgements need no fresh buffer.
            state.dirty = false;
        }
        if phase == Phase::Publishing && state.presented_frame_ids.contains(&1) {
            phase = Phase::Published;
        }
        if phase == Phase::Published && state.witness_buttons >= 2 && !published_ready {
            published_ready = true;
            validation_deadline = None;
            eprintln!("Nested candidate presented; pointer reached witness");
        }
        if phase == Phase::RollingBack && state.presented_frame_ids.contains(&2) {
            phase = Phase::RolledBack;
        }
        if phase == Phase::RolledBack && state.witness_buttons >= 4 && !rollback_ready {
            rollback_ready = true;
            validation_deadline = None;
            eprintln!("Nested previous image restored; pointer reached witness");
        }
        if let Some(command) = control.next()? {
            match command {
                Command::Publish => {
                    ensure!(
                        phase == Phase::Staging && !publish_requested,
                        "invalid nested command order"
                    );
                    publish_requested = true;
                }
                Command::Rollback => {
                    ensure!(
                        phase == Phase::Published && published_ready,
                        "invalid nested command order"
                    );
                    state.rollback_nested(&queue.handle())?;
                    phase = Phase::RollingBack;
                    validation_deadline = Some(Instant::now() + Duration::from_secs(5));
                }
                Command::Abort => {
                    ensure!(
                        phase == Phase::Staging && prepared && !publish_requested,
                        "invalid nested command order"
                    );
                    drop(window.take());
                    state.abort_nested()?;
                    phase = Phase::Aborted;
                    fixed_size = state.configured;
                    aborted_lifecycle = Some(counts.snapshot());
                    eprintln!("Nested candidate aborted; previous root retained");
                }
                Command::Finalize => anyhow::bail!("invalid nested command order"),
            }
        }
        if stopped.load(Ordering::Relaxed) {
            break;
        }
        ensure!(Instant::now() < deadline, "nested proof duration exceeded");
        ensure!(
            prepared || Instant::now() < startup,
            "nested preparation timed out"
        );
        if let Some(until) = validation_deadline {
            ensure!(
                Instant::now() < until,
                "nested presentation/input timed out"
            );
        }
        if phase == Phase::Staging && state.presented_frame_ids.contains(&0) {
            let (width, height) = state.configured.context("layer configure missing")?;
            if window.is_none() {
                let native = unsafe {
                    // SAFETY: The private State/tree retains this exclusive protocol surface and role until after its native window is dropped.
                    egl_window::Window::on_session(
                        session.clone(),
                        state.render_surface()?,
                        width,
                        height,
                    )
                }?;
                native.disable_swap_throttle()?;
                window = Some(native);
            }
            let native = window.as_ref().context("candidate native window missing")?;
            if state.dirty {
                native.paint(width, height)?;
                native.present()?;
                swaps += 1;
                state.flush_staged()?;
                state.dirty = native.take_refresh();
                refreshes += u32::from(state.dirty);
                ensure!(swaps <= 16, "nested swap bound exceeded");
                if state.dirty {
                    connection.flush()?;
                    continue;
                }
                prepared = true;
                eprintln!("Nested candidate cached {width}x{height}; previous root visible");
            }
            if publish_requested && prepared {
                native.paint(width, height)?;
                ensure!(
                    !native.refresh_required(),
                    "nested publication still needs a resize refresh"
                );
                state.request_frame_feedback(&queue.handle(), 1)?;
                native.present()?;
                swaps += 1;
                state.publish_staged()?;
                state.injected = false;
                state.dirty = false;
                fixed_size = Some((width, height));
                phase = Phase::Publishing;
                validation_deadline = Some(Instant::now() + Duration::from_secs(5));
                eprintln!("Nested candidate parent published {width}x{height}");
            }
        }
        if matches!(phase, Phase::Published | Phase::RolledBack) {
            state.inject_if_ready();
        }
        let next = validation_deadline
            .unwrap_or(if prepared { deadline } else { startup })
            .min(deadline);
        wayland_poll::wait_fds(
            &mut queue,
            &connection,
            &[reader.as_fd(), control.as_fd()],
            next,
        )?;
    }
    ensure!(
        rollback_ready || phase == Phase::Aborted,
        "nested proof stopped before rollback or abort validation"
    );
    let mut report = serde_json::json!({"schema_version":1,"status":"passed","scope":"persistent_root_nested_candidate",
        "gl":session.report()?,"previous_rgb":previous,"root_surface_id":root_id,"logical_size":state.configured,
        "swaps":swaps,"geometry_refreshes":refreshes,"candidate_aborted":phase==Phase::Aborted,
        "candidate_presented":published_ready,"previous_restored":rollback_ready,"aborted_lifecycle":aborted_lifecycle,
        "presented_frame_ids":state.presented_frame_ids,"witness_button_events":state.witness_buttons});
    eprintln!("Nested proof removing private root");
    drop(window);
    drop(session);
    report["egl_lifecycle"] = counts.snapshot();
    drop(state);
    connection.flush()?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
