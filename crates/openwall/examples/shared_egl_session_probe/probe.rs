// SPDX-License-Identifier: AGPL-3.0-or-later
#[path = "../probe_support/quadrant_painter.rs"]
mod quadrant_painter;
use openwall::native_egl::gl_api;
use openwall::native_egl::window as egl_window;
use quadrant_painter::Quadrants;
#[path = "../owned_egl_wayland_probe/wayland.rs"]
#[allow(dead_code)] // Static and video diagnostics share the private protocol roots.
mod wayland;
#[path = "../probe_support/wayland_poll.rs"]
mod wayland_poll;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use glow::HasContext;
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
struct Args {
    #[arg(long)]
    first: String,
    #[arg(long)]
    second: String,
    #[arg(long, default_value_t=30, value_parser=clap::value_parser!(u64).range(1..=30))]
    seconds: u64,
    #[arg(long, value_enum, default_value_t=gl_api::Api::Desktop)]
    gl_api: gl_api::Api,
}

struct Marker<'a> {
    gl: &'a glow::Context,
    texture: glow::NativeTexture,
}
impl<'a> Marker<'a> {
    fn new(gl: &'a glow::Context) -> Result<Self> {
        // SAFETY: the shared session's context is current and retained by caller.
        unsafe {
            let marker = Self {
                gl,
                texture: gl.create_texture().map_err(anyhow::Error::msg)?,
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(marker.texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                1,
                1,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&[17, 29, 43, 255])),
            );
            ensure!(
                gl.get_error() == glow::NO_ERROR,
                "marker texture setup failed"
            );
            Ok(marker)
        }
    }
    fn check(&self) -> Result<()> {
        // SAFETY: same live current context; the marker was not deleted.
        ensure!(
            unsafe { self.gl.is_texture(self.texture) && self.gl.get_error() == glow::NO_ERROR },
            "shared GL resource was lost when a native window disappeared"
        );
        Ok(())
    }
}
impl Drop for Marker<'_> {
    fn drop(&mut self) {
        // SAFETY: the session outlives this marker, even with no output windows.
        unsafe { self.gl.delete_texture(self.texture) };
    }
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    ensure!(
        std::env::var("OPENWALL_PRIVATE_COMPOSITOR").as_deref() == Ok("1"),
        "shared EGL session probe requires OPENWALL_PRIVATE_COMPOSITOR=1 from its private harness"
    );
    ensure!(
        args.first != args.second,
        "shared session proof requires distinct private outputs"
    );
    let (mut reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    let writer = Mutex::new(writer);
    let stopped = Arc::new(AtomicBool::new(false));
    let signal = stopped.clone();
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::Relaxed);
        if let Ok(mut writer) = writer.lock() {
            let _ = writer.write_all(&[1]);
        }
    })?;
    let (connection, mut queue, mut state) = wayland::connect(&args.first, false)?;
    state.add_secondary(&args.second, &queue.handle())?;
    let startup = Instant::now() + Duration::from_secs(10);
    while state.configured.is_none()
        || state
            .secondary
            .as_ref()
            .is_none_or(|secondary| secondary.configured.is_none())
    {
        queue.dispatch_pending(&mut state)?;
        ensure!(
            !state.closed
                && !state
                    .secondary
                    .as_ref()
                    .is_some_and(|secondary| secondary.closed),
            "private output closed during configure"
        );
        ensure!(
            !stopped.load(Ordering::Relaxed) && Instant::now() < startup,
            "shared session configure timed out or interrupted"
        );
        if state.configured.is_none()
            || state
                .secondary
                .as_ref()
                .is_none_or(|secondary| secondary.configured.is_none())
        {
            wayland_poll::wait(&mut queue, &connection, &reader, startup)?;
        }
    }
    let session = egl_window::Session::new(&connection, args.gl_api)?;
    let reused = egl_window::Session::new(&connection, args.gl_api)?;
    ensure!(
        std::rc::Rc::ptr_eq(&session, &reused),
        "same connection did not reuse its owner"
    );
    drop(reused);
    let other_api = match args.gl_api {
        gl_api::Api::Desktop => gl_api::Api::Gles,
        gl_api::Api::Gles => gl_api::Api::Desktop,
    };
    let conflict = egl_window::Session::new(&connection, other_api)
        .err()
        .context("conflicting API unexpectedly created a second display owner")?;
    ensure!(
        conflict.to_string().contains("different native GL API"),
        "unexpected API conflict: {conflict}"
    );
    let foreign = connection.clone();
    let selected = args.gl_api;
    let cross_thread = std::thread::spawn(move || {
        egl_window::Session::new(&foreign, selected)
            .err()
            .map(|error| error.to_string())
    })
    .join()
    .map_err(|_| anyhow::anyhow!("ownership check thread panicked"))?
    .context("another thread unexpectedly created a duplicate display owner")?;
    ensure!(
        cross_thread.contains("already has a native EGL owner"),
        "unexpected cross-thread failure: {cross_thread}"
    );
    let counts = session.counts();
    let (width, height) = state.configured.context("first configure missing")?;
    let mut first = Some(unsafe {
        // SAFETY: The private State/tree retains this exclusive protocol surface and role until after its native window is dropped.
        egl_window::Window::on_session(
            session.clone(),
            state.surface.as_ref().context("first surface missing")?,
            width,
            height,
        )
    }?);
    let secondary = state.secondary.as_ref().context("second surface missing")?;
    let (width, height) = secondary.configured.context("second configure missing")?;
    let mut second = Some(unsafe {
        // SAFETY: The private State/tree retains this exclusive protocol surface and role until after its native window is dropped.
        egl_window::Window::on_session(session.clone(), &secondary.surface, width, height)
    }?);
    ensure!(
        first.as_ref().context("first window missing")?.handles()?
            == second
                .as_ref()
                .context("second window missing")?
                .handles()?,
        "private windows did not retain the same EGL display/context"
    );
    let marker = Marker::new(session.gl()?)?;
    let independent_connection = wayland_client::Connection::connect_to_env()?;
    let mixed_api = egl_window::Session::new(&independent_connection, other_api)
        .err()
        .context("same thread unexpectedly mixed native GL APIs")?;
    ensure!(
        mixed_api
            .to_string()
            .contains("thread already owns a different GL API"),
        "unexpected mixed API error: {mixed_api}"
    );
    let independent = egl_window::Session::new(&independent_connection, args.gl_api)?;
    let independent_counts = independent.counts();
    ensure!(
        independent.handles()?.0 != session.handles()?.0,
        "independent connections share an EGLDisplay"
    );
    session.surfaceless()?;
    drop(independent);
    session.report()?; // Its current binding must survive the other owner's Drop.
    marker.check()?;
    let independent_lifecycle = independent_counts.snapshot();
    let mut ready = false;
    let mut removed_first = false;
    let mut survivor_presented = false;
    let mut reported_second = 0;
    let mut second_size = None;
    let mut first_feedback_deadline = None;
    let mut second_feedback_deadline = None;
    let mut survivor_baseline = 0;
    let mut second_requires_refresh = false;
    let mut geometry_refreshes = 0;
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    loop {
        queue.dispatch_pending(&mut state)?;
        let mut bytes = [0; 64];
        while reader.read(&mut bytes).is_ok_and(|count| count > 0) {}
        ensure!(
            !state.intercepted,
            "shared EGL surface intercepted private input"
        );
        ensure!(
            state.discarded <= 3,
            "first EGL surface repeatedly discarded"
        );
        let secondary = state.secondary.as_ref().context("second surface missing")?;
        ensure!(
            !secondary.closed && secondary.discarded <= 3,
            "surviving private output closed or repeatedly discarded"
        );
        if !state.pending_feedback {
            first_feedback_deadline = None;
        }
        if !secondary.pending_feedback {
            second_feedback_deadline = None;
        }
        if secondary.presented > reported_second {
            reported_second = secondary.presented;
            let (width, height) = second_size.context("second submission size missing")?;
            if !second_requires_refresh {
                eprintln!("Shared EGL second presented {width}x{height}");
            }
        }
        if !ready
            && state.presented > 0
            && secondary.presented > 0
            && state.witness_buttons >= 2
            && !state.dirty
            && !state.pending_feedback
            && !second_requires_refresh
            && !secondary.dirty
            && !secondary.pending_feedback
        {
            ready = true;
            eprintln!("Shared EGL presented on both private outputs");
        }
        if state.closed && first.is_some() {
            ensure!(ready, "first output closed before initial validation");
            first.take();
            marker.check()?;
            removed_first = true;
            survivor_baseline = secondary.presented;
            state
                .secondary
                .as_mut()
                .context("second surface missing")?
                .dirty = true;
            first_feedback_deadline = None;
            eprintln!("Shared EGL primary removed; surviving texture is live");
        }
        if removed_first
            && !second_requires_refresh
            && state.secondary.as_ref().is_some_and(|secondary| {
                secondary.presented > survivor_baseline
                    && !secondary.pending_feedback
                    && !secondary.dirty
            })
        {
            survivor_presented = true;
        }
        if stopped.load(Ordering::Relaxed) {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "shared EGL probe exceeded bounded duration"
        );
        ensure!(
            ready || Instant::now() < startup,
            "shared EGL readiness timed out"
        );
        for until in [first_feedback_deadline, second_feedback_deadline]
            .into_iter()
            .flatten()
        {
            ensure!(
                Instant::now() < until,
                "shared EGL presentation feedback timed out"
            );
        }
        if !removed_first {
            state.inject_if_ready();
        }
        if !state.closed && state.dirty && !state.pending_feedback {
            let (width, height) = state.configured.context("first configure missing")?;
            state.request_feedback(&queue.handle())?;
            first
                .as_mut()
                .context("first window missing")?
                .draw(width, height)?;
            state.dirty = first
                .as_ref()
                .context("first window missing")?
                .take_refresh();
            geometry_refreshes += u32::from(state.dirty);
            first_feedback_deadline = Some(Instant::now() + Duration::from_secs(5));
        }
        let secondary = state.secondary.as_ref().context("second surface missing")?;
        if secondary.dirty && !secondary.pending_feedback {
            let (width, height) = secondary.configured.context("second configure missing")?;
            state.secondary_feedback(&queue.handle())?;
            second
                .as_mut()
                .context("second window missing")?
                .draw(width, height)?;
            second_requires_refresh = second
                .as_ref()
                .context("second window missing")?
                .take_refresh();
            state
                .secondary
                .as_mut()
                .context("second surface missing")?
                .dirty = second_requires_refresh;
            geometry_refreshes += u32::from(second_requires_refresh);
            second_size = Some((width, height));
            marker.check()?;
            second_feedback_deadline = Some(Instant::now() + Duration::from_secs(5));
        }
        let next = [
            Some(deadline),
            (!ready).then_some(startup),
            first_feedback_deadline,
            second_feedback_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
        .context("probe deadline missing")?;
        wayland_poll::wait(&mut queue, &connection, &reader, next)?;
    }
    ensure!(
        ready && removed_first && survivor_presented,
        "shared session stopped before surviving-output validation"
    );
    eprintln!("Shared EGL removing private surfaces");
    first.take();
    second.take();
    // Keep the session and marker alive after the final drawable disappears.
    session.surfaceless()?;
    marker.check()?;
    let gl = session.report()?;
    let second_presented = state
        .secondary
        .as_ref()
        .context("second surface missing")?
        .presented;
    drop(marker);
    drop(first);
    drop(second);
    drop(session);
    let lifecycle = counts.snapshot();
    ensure!(
        lifecycle
            == serde_json::json!({"display_initializations":1,"context_creations":1,"surface_creations":2,
        "surface_destructions":2,"context_destructions":1,"display_terminations":1}),
        "shared EGL destruction counts incorrect: {lifecycle}"
    );
    // Releasing the final owner must release its process-wide reservation too.
    let recreated = egl_window::Session::new(&connection, args.gl_api)?;
    let recreated_counts = recreated.counts();
    let marker = Marker::new(recreated.gl()?)?;
    marker.check()?;
    drop(marker);
    drop(recreated);
    let recreated_lifecycle = recreated_counts.snapshot();
    let report = serde_json::json!({"schema_version":1,"status":"passed","scope":"two_private_outputs_one_egl_session",
        "gl":gl,"first_presented":state.presented,"second_presented":second_presented,
        "first_removed":removed_first,"survivor_presented":survivor_presented,"texture_survived_no_windows":true,
        "session_reused":true,"conflicting_api_rejected":true,"cross_thread_rejected":true,
        "mixed_thread_api_rejected":true,"independent_connection_lifecycle":independent_lifecycle,
        "recreated_owner_lifecycle":recreated_lifecycle,
        "witness_button_events":state.witness_buttons,"egl_lifecycle":lifecycle,"geometry_refreshes":geometry_refreshes});
    drop(state);
    connection.flush()?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
