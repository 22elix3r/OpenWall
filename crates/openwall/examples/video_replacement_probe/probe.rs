// SPDX-License-Identifier: AGPL-3.0-or-later
#[path = "../nested_egl_publication_probe/control.rs"]
mod control;
#[path = "../probe_support/decoder_trace.rs"]
mod decoder_trace;
use openwall::native_egl::gl_api;
use openwall::native_egl::window as egl_window;
use openwall::presentation::feedback::{Completion, Receipt};
#[path = "../probe_support/gl_painter.rs"]
mod gl_painter;
#[path = "../native_gl_video_probe/media.rs"]
#[allow(dead_code)] // This bounded replacement diagnostic does not seek EOS.
mod media;
#[path = "../probe_support/native_media_context.rs"]
mod native_media_context;
#[path = "../probe_support/shared_gl.rs"]
mod shared_gl;
#[path = "../owned_egl_wayland_probe/wayland.rs"]
#[allow(dead_code)] // The other diagnostics also use single and multiple output roots.
mod wayland;
#[path = "../probe_support/wayland_poll.rs"]
mod wayland_poll;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use control::Command;
use gstreamer as gst;
use std::{
    cell::Cell,
    collections::VecDeque,
    io::Read,
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
    previous: PathBuf,
    candidate: PathBuf,
    #[arg(long)]
    output: String,
    #[arg(
        long,
        required_unless_present = "control_stdin",
        conflicts_with = "control_stdin"
    )]
    control_socket: Option<PathBuf>,
    /// Inherited, connected same-user Unix stream; EOF represents controller loss.
    #[arg(long)]
    control_stdin: bool,
    #[arg(long, value_enum, default_value_t=gl_api::Api::Desktop)]
    gl_api: gl_api::Api,
    #[arg(long, default_value_t=30, value_parser=clap::value_parser!(u64).range(1..=30))]
    seconds: u64,
    #[arg(long)]
    intercept_input: bool,
    /// Post an explicit diagnostic GStreamer error after confirmed candidate readiness.
    #[arg(long, value_parser=clap::value_parser!(u64).range(500..=2000))]
    fail_candidate_after_ms: Option<u64>,
    /// Inject before accepting candidate feedback; its media clock must never start.
    #[arg(long, conflicts_with_all = ["fail_candidate_after_ms", "discard_candidate_submission"])]
    fail_before_candidate_confirmation: bool,
    /// Supersede the hidden candidate commit before mapping it; require real discard feedback.
    #[arg(long, conflicts_with = "fail_candidate_after_ms")]
    discard_candidate_submission: bool,
    /// Negative control: discarded restoration must fail instead of claiming recovery.
    #[arg(long, conflicts_with_all = ["discard_candidate_submission", "fail_before_candidate_confirmation", "fail_candidate_after_ms"])]
    discard_restoration_submission: bool,
    /// Prepare another hidden candidate in the retired slot after committing the first.
    #[arg(long, conflicts_with_all = ["fail_candidate_after_ms", "fail_before_candidate_confirmation", "discard_candidate_submission", "discard_restoration_submission"])]
    reprepare_after_finalize: Option<PathBuf>,
    /// Fill diagnostic presentation history before starting; readiness must use receipts.
    #[arg(long)]
    saturate_feedback_history: bool,
}

struct Source {
    // Struct fields drop in order: retained samples must precede pipeline NULL.
    current: Option<gst::Sample>,
    media: media::Media,
    memory: Option<serde_json::Value>,
    size: Option<(u32, u32)>,
    dirty: bool,
    prepared: bool,
    started: bool,
    loops: u32,
}
impl Source {
    fn new(media: media::Media) -> Self {
        Self {
            current: None,
            media,
            memory: None,
            size: None,
            dirty: false,
            prepared: false,
            started: false,
            loops: 0,
        }
    }
    fn report(&self) -> Result<serde_json::Value> {
        let (received, replaced) = self.media.counters()?;
        Ok(
            serde_json::json!({"input_memory":self.memory,"source_size":self.size,
            "decoders":self.media.decoders()?,"decoder_buffers":self.media.decoder_buffers()?,
            "delivery":{"received":received,"replaced":replaced,"latest_slot_limit":1},
            "prepared":self.prepared,"playback_started":self.started,"completed_loops":self.loops}),
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Initial,
    Previous,
    Publishing,
    Candidate,
    Restoring,
    Restored,
    Aborted,
    Failed,
    Finalized,
}

pub fn run() -> Result<()> {
    let args = Args::parse();
    ensure!(
        std::env::var("OPENWALL_PRIVATE_COMPOSITOR").as_deref() == Ok("1"),
        "video replacement requires OPENWALL_PRIVATE_COMPOSITOR=1 from its private harness"
    );
    gst::init()?;
    ensure!(
        gst::version().0 == 1 && gst::version().1 >= 26,
        "video replacement requires GStreamer 1.26 or newer"
    );
    let mut control = if args.control_stdin {
        control::Control::stdin()?
    } else {
        control::Control::bind(
            args.control_socket
                .as_deref()
                .context("control socket missing")?,
        )?
    };
    let (mut reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    let writer = Arc::new(Mutex::new(writer));
    let stopped = Arc::new(AtomicBool::new(false));
    let signal = stopped.clone();
    let signal_writer = writer.clone();
    ctrlc::set_handler(move || {
        signal.store(true, Ordering::Relaxed);
        media::wake(&signal_writer);
    })?;
    let (connection, mut queue, mut state) = wayland::connect(&args.output, args.intercept_input)?;
    if args.saturate_feedback_history {
        state.presented_frame_ids = (u64::MAX - 255..=u64::MAX).collect();
        state.discarded_frame_ids = state.presented_frame_ids.clone();
    }
    let startup = Instant::now() + Duration::from_secs(10);
    while state.configured.is_none() {
        queue.dispatch_pending(&mut state)?;
        ensure!(
            !state.closed && !stopped.load(Ordering::Relaxed) && Instant::now() < startup,
            "video replacement configure failed or timed out"
        );
        if state.configured.is_none() {
            wayland_poll::wait(&mut queue, &connection, &reader, startup)?;
        }
    }
    let size = state.configured.context("layer configure missing")?;
    let root_id = state
        .surface
        .as_ref()
        .context("root missing")?
        .id()
        .protocol_id();
    let mut tree = wayland::video_tree::create(&state, &queue.handle())?;
    let session = egl_window::Session::new(&connection, args.gl_api)?;
    let counts = session.counts();
    let mut windows = [None, None];
    for (index, window) in windows.iter_mut().enumerate() {
        let native = unsafe {
            // SAFETY: The private State/tree retains this exclusive protocol surface and role until after its native window is dropped.
            egl_window::Window::on_session(session.clone(), tree.surface(index)?, size.0, size.1)
        }?;
        native.disable_swap_throttle()?;
        *window = Some(native);
    }
    let sharing = native_media_context::Sharing::new(session.clone(), args.gl_api)?;
    let gl = session.gl()?;
    let painter = gl_painter::Painter::new(gl, args.gl_api)?;
    let mut sources = [
        Some(Source::new(media::Media::new(
            &args.previous,
            &sharing.display,
            &sharing.wrapped,
            writer.clone(),
            true,
        )?)),
        None,
    ];
    // Leases drop before Source.current, then Media, then painter/sharing/windows/session/tree/root.
    let finishes = Cell::new(0);
    let mut leases = VecDeque::new();
    let mut records = Vec::new();
    let mut last_visible_submission: Option<(Receipt, usize)> = None;
    let mut retired = 0;
    let mut peak = 0;
    let mut phase = Phase::Initial;
    let mut transition: Option<Receipt> = None;
    let mut feedback_deadline = None;
    let mut validation_deadline = None;
    let mut prepared_deadline = None;
    let mut attempted = false;
    let mut old_ready = false;
    let mut candidate_ready = false;
    let mut restored_ready = false;
    let mut command_pending = None;
    let mut candidate_report = None;
    let mut candidate_report_error = None;
    let mut previous_report = None;
    let mut candidate_failure = None;
    let mut removal_counts = None;
    let mut rollback_pts = None;
    let mut finalization_counts = None;
    let mut finalization_frame_id = None;
    let mut fault_deadline = None;
    let mut fault_injected = false;
    let mut auto_rollback = false;
    let mut candidate_first_confirmed = false;
    let mut candidate_publication_id = None;
    let mut candidate_receipt: Option<Receipt> = None;
    let mut early_cancel_requested = false;
    let mut early_cancel_applied = false;
    let mut handled_discards = 0;
    let mut discard_injected = false;
    let mut reprepare_started = false;
    let mut reprepare_aborted = false;
    let mut reprepare_failure = None;
    let mut reprepare_report = None;
    let mut reprepare_report_error = None;
    let mut reprepare_removal_counts = None;
    let mut controller_failure = None;
    let mut controller_failure_handled = false;
    let mut restored_button_goal = 6;
    let mut clock_starts = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(args.seconds);
    loop {
        queue.dispatch_pending(&mut state)?;
        let mut bytes = [0; 64];
        while reader.read(&mut bytes).is_ok_and(|count| count > 0) {}
        ensure!(!state.closed, "replacement output or witness closed");
        ensure!(
            !state.intercepted,
            "replacement surface intercepted private input"
        );
        // Observe ownership loss before accepting any queued publication feedback.
        let incoming = match control.next() {
            Ok(command) => command,
            Err(error) if args.control_stdin => {
                controller_failure.get_or_insert_with(|| error.to_string());
                control.close();
                None
            }
            Err(error) => return Err(error),
        };
        if args.control_stdin && control.is_closed() {
            controller_failure.get_or_insert_with(|| "controller disconnected".to_owned());
        }
        if let Some(reason) = &controller_failure
            && !controller_failure_handled
        {
            controller_failure_handled = true;
            command_pending = None;
            match phase {
                Phase::Initial | Phase::Previous | Phase::Publishing | Phase::Candidate => {
                    candidate_failure.get_or_insert_with(|| reason.clone());
                    if phase == Phase::Candidate
                        && let Some((receipt, 1)) = &last_visible_submission
                    {
                        state.cancel_frame_feedback(receipt.submission_id());
                    }
                }
                Phase::Finalized if reprepare_started && reprepare_removal_counts.is_none() => {
                    reprepare_failure.get_or_insert_with(|| reason.clone());
                }
                _ => {}
            }
            eprintln!("Replacement controller lost; preserving retained assignment");
        }
        let incoming = if controller_failure.is_none() {
            incoming
        } else {
            None
        };
        if state.discarded > handled_discards {
            ensure!(
                matches!(phase, Phase::Publishing | Phase::Candidate)
                    && state.discarded == handled_discards + 1
                    && last_visible_submission
                        .as_ref()
                        .is_some_and(|(receipt, index)| *index == 1
                            && receipt.completion() == Some(Completion::Discarded)),
                "previous/restoration/finalized presentation discarded"
            );
            candidate_failure.get_or_insert_with(|| "candidate presentation discarded".to_owned());
            handled_discards = state.discarded;
        }
        if candidate_failure.is_some() && matches!(phase, Phase::Publishing | Phase::Candidate) {
            validation_deadline = None;
        }
        ensure!(
            state.configured == Some(size),
            "this diagnostic keeps published geometry fixed"
        );
        ensure!(
            state
                .surface
                .as_ref()
                .context("root missing")?
                .id()
                .protocol_id()
                == root_id,
            "replacement desktop root changed"
        );
        if let Some(source) = &mut sources[0]
            && let Err(error) = source.media.messages()
        {
            if phase == Phase::Finalized && reprepare_started {
                reprepare_failure.get_or_insert_with(|| error.to_string());
            } else {
                return Err(error);
            }
        }
        if fault_deadline.is_some_and(|until| Instant::now() >= until) && !fault_injected {
            sources[1]
                .as_ref()
                .context("fault source missing")?
                .media
                .diagnostic_error()?;
            fault_injected = true;
            fault_deadline = None;
        }
        if candidate_failure.is_none()
            && let Some(source) = &mut sources[1]
            && let Err(error) = source.media.messages()
        {
            ensure!(
                phase != Phase::Finalized,
                "finalized source failed after previous-source retirement: {error}"
            );
            candidate_failure = Some(error.to_string());
        }
        if candidate_failure.is_some() && phase == Phase::Publishing {
            let id = transition
                .take()
                .context("failed publication feedback missing")?
                .submission_id();
            early_cancel_applied = state.cancel_frame_feedback(id);
            early_cancel_requested = true;
            command_pending = Some(Command::Rollback);
            auto_rollback = true;
            phase = Phase::Candidate;
            eprintln!(
                "Replacement candidate readiness cancelled; restoring retained previous video"
            );
        }
        while shared_gl::retire_one(&mut leases, false, &AtomicBool::new(false))? {
            retired += 1;
        }
        if let Some(lease) = leases.front() {
            ensure!(
                lease.submitted_at.elapsed() < Duration::from_secs(5),
                "replacement consumer fence timed out"
            );
        }
        if let Some(receipt) = &transition
            && receipt.completion() == Some(Completion::Presented)
        {
            let id = receipt.submission_id();
            let index = if phase == Phase::Publishing { 1 } else { 0 };
            let source = sources[index]
                .as_mut()
                .context("transition source missing")?;
            if index == 0 || candidate_failure.is_none() {
                source.media.resume()?;
                source.started = true;
                clock_starts.push(serde_json::json!({"source":index,"presented_frame_id":id}));
            }
            candidate_first_confirmed |= index == 1;
            if phase == Phase::Restoring {
                restored_button_goal = state.witness_buttons + 2;
            }
            phase = match phase {
                Phase::Initial => Phase::Previous,
                Phase::Publishing => Phase::Candidate,
                Phase::Restoring => Phase::Restored,
                _ => anyhow::bail!("unexpected transition presentation"),
            };
            transition = None;
            state.injected = false;
        }
        state.dirty = false; // Repeated same-size configure does not need a priming frame.
        if phase == Phase::Previous && state.witness_buttons >= 2 && !old_ready {
            old_ready = true;
            validation_deadline = None;
            eprintln!("Replacement previous video presented; pointer reached witness");
        }
        if phase == Phase::Candidate && state.witness_buttons >= 4 && !candidate_ready {
            candidate_ready = true;
            validation_deadline = None;
            eprintln!("Replacement candidate video presented; pointer reached witness");
            fault_deadline = args
                .fail_candidate_after_ms
                .map(|millis| Instant::now() + Duration::from_millis(millis));
        }
        if phase == Phase::Restored
            && state.witness_buttons >= restored_button_goal
            && !restored_ready
        {
            restored_ready = true;
            validation_deadline = None;
            eprintln!("Replacement previous video restored; pointer reached witness");
        }
        if old_ready && !attempted && candidate_failure.is_none() {
            attempted = true;
            prepared_deadline = Some(Instant::now() + Duration::from_secs(8));
            match media::Media::new(
                &args.candidate,
                &sharing.display,
                &sharing.wrapped,
                writer.clone(),
                true,
            ) {
                Ok(media) => sources[1] = Some(Source::new(media)),
                Err(error) => candidate_failure = Some(error.to_string()),
            }
        }
        if let Some(command) = incoming {
            let valid = command_pending.is_none()
                && match command {
                    Command::Publish => {
                        phase == Phase::Previous
                            && old_ready
                            && sources[1].as_ref().is_some_and(|source| source.prepared)
                    }
                    Command::Abort => {
                        phase == Phase::Previous
                            && old_ready
                            && sources[1].as_ref().is_some_and(|source| source.prepared)
                            || phase == Phase::Finalized
                                && reprepare_started
                                && sources[0].as_ref().is_some_and(|source| source.prepared)
                    }
                    Command::Rollback | Command::Finalize => {
                        phase == Phase::Candidate && candidate_ready
                    }
                };
            if !valid {
                if args.control_stdin {
                    controller_failure = Some("invalid replacement command order".into());
                    control.close();
                    continue; // Re-enter ownership-loss handling before another submission.
                }
                anyhow::bail!("invalid replacement command order");
            }
            command_pending = Some(command);
        }
        if candidate_failure.is_some() && phase == Phase::Candidate && !auto_rollback {
            command_pending = Some(Command::Rollback);
            auto_rollback = true;
            eprintln!("Replacement candidate failed; restoring retained previous video");
        }
        if stopped.load(Ordering::Relaxed) {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "replacement diagnostic duration exceeded"
        );
        ensure!(
            old_ready || Instant::now() < startup,
            "previous video readiness timed out"
        );
        if let Some(until) = validation_deadline {
            ensure!(
                Instant::now() < until,
                "replacement presentation/input timed out"
            );
        }
        if state.pending_feedback {
            ensure!(
                feedback_deadline.is_some_and(|until| Instant::now() < until),
                "replacement feedback timed out"
            );
        } else {
            feedback_deadline = None;
        }
        if let Some(until) = prepared_deadline
            && Instant::now() >= until
        {
            if phase == Phase::Finalized && reprepare_started {
                reprepare_failure.get_or_insert_with(|| "next candidate preroll timed out".into());
            } else if !sources[1].as_ref().is_some_and(|source| source.prepared) {
                candidate_failure = Some("candidate preroll timed out".into());
            }
        }
        if phase == Phase::Finalized
            && reprepare_started
            && reprepare_removal_counts.is_none()
            && (reprepare_failure.is_some() || matches!(command_pending, Some(Command::Abort)))
        {
            while shared_gl::retire_one(&mut leases, true, &AtomicBool::new(false))? {
                retired += 1;
            }
            match sources[0].as_ref().map(Source::report).transpose() {
                Ok(report) => reprepare_report = report,
                Err(error) if reprepare_failure.is_some() => {
                    reprepare_report_error = Some(error.to_string())
                }
                Err(error) => return Err(error),
            }
            drop(sources[0].take());
            drop(windows[0].take());
            tree.remove(0)?;
            reprepare_removal_counts = Some(counts.snapshot());
            reprepare_aborted = reprepare_failure.is_none();
            command_pending = None;
            prepared_deadline = None;
            eprintln!("Replacement re-prepared candidate removed; committed video continues");
        }
        if phase == Phase::Previous
            && (candidate_failure.is_some() || matches!(command_pending, Some(Command::Abort)))
        {
            while shared_gl::retire_one(&mut leases, true, &AtomicBool::new(false))? {
                retired += 1;
            }
            match sources[1].as_ref().map(Source::report).transpose() {
                Ok(report) => candidate_report = report,
                Err(error) if candidate_failure.is_some() => {
                    candidate_report_error = Some(error.to_string())
                }
                Err(error) => return Err(error),
            }
            drop(sources[1].take());
            drop(windows[1].take());
            tree.remove(1)?;
            removal_counts = Some(counts.snapshot());
            prepared_deadline = None;
            phase = if candidate_failure.is_some() {
                Phase::Failed
            } else {
                Phase::Aborted
            };
            command_pending = None;
            eprintln!("Replacement candidate removed; previous video retained");
        }
        if phase == Phase::Restored && sources[1].is_some() && !state.pending_feedback {
            while shared_gl::retire_one(&mut leases, true, &AtomicBool::new(false))? {
                retired += 1;
            }
            match sources[1].as_ref().map(Source::report).transpose() {
                Ok(report) => candidate_report = report,
                Err(error) if candidate_failure.is_some() => {
                    candidate_report_error = Some(error.to_string())
                }
                Err(error) => return Err(error),
            }
            drop(sources[1].take());
            drop(windows[1].take());
            tree.remove(1)?;
            removal_counts = Some(counts.snapshot());
            fault_deadline = None;
            eprintln!("Replacement rolled-back candidate retired; previous video continues");
        }
        if matches!(command_pending, Some(Command::Finalize)) {
            while shared_gl::retire_one(&mut leases, true, &AtomicBool::new(false))? {
                retired += 1;
            }
            previous_report = sources[0].as_ref().map(Source::report).transpose()?;
            drop(sources[0].take());
            drop(windows[0].take());
            tree.remove(0)?;
            finalization_counts = Some(counts.snapshot());
            finalization_frame_id = Some(records.len());
            command_pending = None;
            phase = Phase::Finalized;
            fault_deadline = None;
            eprintln!("Replacement finalized; previous source retired; candidate video continues");
            if let Some(path) = &args.reprepare_after_finalize {
                tree.recreate(0, &queue.handle())?;
                ensure!(
                    tree.recreate(0, &queue.handle()).is_err()
                        && tree.recreate(2, &queue.handle()).is_err()
                        && tree.remove(1).is_err(),
                    "recreated tree ownership guard failed"
                );
                let native = unsafe {
                    // SAFETY: The recreated leaf belongs exclusively to this window;
                    // the persistent tree survives all native windows and producers.
                    egl_window::Window::on_session(
                        session.clone(),
                        tree.surface(0)?,
                        size.0,
                        size.1,
                    )
                }?;
                native.disable_swap_throttle()?;
                windows[0] = Some(native);
                reprepare_started = true;
                prepared_deadline = Some(Instant::now() + Duration::from_secs(8));
                match media::Media::new(
                    path,
                    &sharing.display,
                    &sharing.wrapped,
                    writer.clone(),
                    true,
                ) {
                    Ok(media) => sources[0] = Some(Source::new(media)),
                    Err(error) => reprepare_failure = Some(error.to_string()),
                }
            }
        }
        if matches!(
            phase,
            Phase::Previous
                | Phase::Candidate
                | Phase::Restored
                | Phase::Aborted
                | Phase::Failed
                | Phase::Finalized
        ) {
            state.inject_if_ready();
        }
        // A looping source briefly releases its cached sample before FLUSH.
        // Finish acquiring/rendering the next old sample before parking it for
        // rollback; a queued publish must not suppress that acquisition.
        let switching = command_pending.is_some()
            && sources[0]
                .as_ref()
                .is_some_and(|source| source.current.is_some());
        let target = match command_pending.as_ref().filter(|_| switching) {
            Some(Command::Publish) => 1,
            Some(Command::Rollback) => 0,
            _ => tree.active().unwrap_or(0),
        };
        let mut again = false;
        for index in 0..2 {
            let hidden_candidate = ((index == 1 && phase == Phase::Previous)
                || (index == 0 && phase == Phase::Finalized && reprepare_started))
                && !switching
                && sources[index]
                    .as_ref()
                    .is_some_and(|source| !source.prepared);
            let visible_target =
                index == target && !matches!(phase, Phase::Publishing | Phase::Restoring);
            if !hidden_candidate && !visible_target
                || leases.len() >= 3
                || visible_target && state.pending_feedback
            {
                continue;
            }
            let Some(source) = sources[index].as_mut() else {
                continue;
            };
            if !switching && let Some(sample) = source.media.take()? {
                source.current = Some(sample);
                source.dirty = true;
            }
            if switching && visible_target {
                source.dirty = true;
            }
            if !source.dirty {
                continue;
            }
            let Some(sample) = source.current.as_ref() else {
                continue;
            };
            ensure!(records.len() < 240, "replacement frame limit exceeded");
            let window = windows[index]
                .as_ref()
                .context("native video window missing")?;
            window.resize(size.0, size.1)?;
            let texture = shared_gl::source(sample, &sharing.wrapped, gl)?;
            let dimensions = (texture.width, texture.height);
            ensure!(
                source.size.is_none_or(|previous| previous == dimensions),
                "dynamic video dimensions unsupported"
            );
            source.size = Some(dimensions);
            if source.memory.is_none() {
                source.memory =
                    Some(serde_json::json!({"caps":texture.caps,"memory":texture.memory}));
            }
            let visible = visible_target && !window.refresh_required();
            let kind = if !visible {
                "prime"
            } else if phase == Phase::Initial {
                "initial"
            } else {
                match command_pending.as_ref().filter(|_| switching) {
                    Some(Command::Publish) => "publish",
                    Some(Command::Rollback) => "rollback",
                    _ => "frame",
                }
            };
            let mut lease = shared_gl::Lease {
                gl,
                sample: Some(sample.clone()),
                fence: None,
                pending: true,
                cleanup_finishes: &finishes,
                submitted_at: Instant::now(),
            };
            painter.draw(&texture, None, [0, 0, size.0, size.1])?;
            lease.seal()?;
            let id = records.len() as u64 + 1;
            let discard_requested = kind == "publish" && args.discard_candidate_submission
                || kind == "rollback" && args.discard_restoration_submission;
            let mut feedback = None;
            if visible {
                let target = if discard_requested {
                    tree.container(index)?
                } else {
                    tree.surface(index)?
                };
                let receipt = state.request_surface_feedback(&queue.handle(), target, id)?;
                last_visible_submission = Some((receipt.clone(), index));
                feedback = Some(receipt);
            }
            window.present()?;
            let refresh = window.take_refresh();
            ensure!(
                !visible || !refresh,
                "published replacement needs a geometry refresh"
            );
            if visible && kind != "frame" {
                // Only the old source's last rendered sample is retained for rollback.
                // Do not consume its asynchronous pause preroll until after restoration.
                if kind == "publish" {
                    let previous = sources[0].as_ref().context("previous source missing")?;
                    rollback_pts = previous
                        .current
                        .as_ref()
                        .and_then(|sample| sample.buffer())
                        .and_then(|buffer| buffer.pts())
                        .map(|pts| pts.nseconds());
                    previous.media.pause()?;
                    phase = Phase::Publishing;
                } else if kind == "rollback" {
                    let paused = sources[1]
                        .as_ref()
                        .context("candidate missing")?
                        .media
                        .pause();
                    if candidate_failure.is_none() {
                        paused?;
                    }
                    phase = Phase::Restoring;
                }
                if discard_requested {
                    wayland::video_tree::discard_cached_publication(
                        &tree,
                        &state,
                        &queue.handle(),
                        index,
                    )?;
                    discard_injected = true;
                } else {
                    tree.show(index)?;
                }
                transition = feedback;
                if kind == "publish" {
                    candidate_receipt = transition.clone();
                    candidate_publication_id = Some(id);
                    if args.fail_before_candidate_confirmation {
                        sources[1]
                            .as_ref()
                            .context("candidate fault source missing")?
                            .media
                            .diagnostic_error()?;
                        fault_injected = true;
                    }
                }
                command_pending = None;
                validation_deadline = Some(Instant::now() + Duration::from_secs(5));
            } else {
                tree.flush(index)?;
            }
            records.push(
                serde_json::json!({"id":id,"source":index,"pts_ns":texture.pts_ns,
                "generation":if index==0 && reprepare_started {2} else {index},
                "texture_id":texture.texture.0.get(),"visible":visible,"kind":kind,
                "geometry_refresh_required":refresh}),
            );
            leases.push_back(lease);
            peak = peak.max(leases.len());
            let source = sources[index].as_mut().context("drawn source missing")?;
            source.dirty = refresh;
            if !refresh && !source.prepared {
                source.prepared = true;
                if index == 1 {
                    prepared_deadline = None;
                    eprintln!("Replacement candidate cached; previous video continues");
                } else if phase == Phase::Finalized && reprepare_started {
                    prepared_deadline = None;
                    eprintln!("Replacement next candidate cached; committed video continues");
                }
            }
            again |= refresh;
            if visible {
                feedback_deadline = Some(Instant::now() + Duration::from_secs(5));
            }
        }
        if again {
            connection.flush()?;
            continue;
        }
        if leases.is_empty()
            && !state.pending_feedback
            && command_pending.is_none()
            && transition.is_none()
            && let Some(index) = tree.active()
            && let Some(source) = sources[index].as_mut()
            && source.media.eos
            && !source.media.has_sample()?
            && !source.dirty
        {
            source.current.take();
            source.loops += 1;
            source.media.rewind()?;
        }
        let mut next = deadline;
        for until in [
            feedback_deadline,
            prepared_deadline,
            validation_deadline,
            fault_deadline,
        ]
        .into_iter()
        .flatten()
        {
            next = next.min(until);
        }
        if !old_ready {
            next = next.min(startup);
        }
        if !leases.is_empty() {
            next = next.min(Instant::now() + Duration::from_millis(1));
        }
        let watched = [reader.as_fd(), control.as_fd()];
        let watched_count = if control.is_closed() { 1 } else { 2 };
        wayland_poll::wait_fds(&mut queue, &connection, &watched[..watched_count], next)?;
    }
    while shared_gl::retire_one(&mut leases, true, &AtomicBool::new(false))? {
        retired += 1;
    }
    ensure!(
        old_ready,
        "replacement stopped before previous video presentation/input"
    );
    let complete =
        restored_ready || matches!(phase, Phase::Aborted | Phase::Failed | Phase::Finalized);
    if candidate_report.is_none() {
        candidate_report = sources[1].as_ref().map(Source::report).transpose()?;
    }
    if previous_report.is_none() {
        previous_report = sources[0].as_ref().map(Source::report).transpose()?;
    }
    let mut report = serde_json::json!({"schema_version":1,"scope":if reprepare_started {"reprepare_video_candidate_persistent_root"} else {"two_video_sources_persistent_root"},
        "status":if complete { "passed" } else { "interrupted" },"gl":session.report()?,
        "gstreamer":gst::version_string().to_string(),"root_surface_id":root_id,
        "previous":previous_report,"candidate":candidate_report,"candidate_report_error":candidate_report_error,
        "candidate_failure":candidate_failure,"candidate_aborted":phase==Phase::Aborted,
        "candidate_presented":candidate_ready,"previous_restored":restored_ready,"rollback_pts_ns":rollback_pts,
        "removed_candidate_lifecycle":removal_counts,"frames":records,"presented_frame_ids":state.presented_frame_ids,
        "finalized":phase==Phase::Finalized,"retired_previous_lifecycle":finalization_counts,
        "finalization_frame_id":finalization_frame_id,
        "fault_injected":fault_injected,"automatic_rollback":auto_rollback,
        "candidate_first_frame_confirmed":candidate_receipt.as_ref().is_some_and(|receipt|matches!(receipt.completion(), Some(Completion::Presented | Completion::CancelledPresented))),
        "candidate_readiness_accepted":candidate_first_confirmed,"candidate_publication_id":candidate_publication_id,
        "early_cancel_requested":early_cancel_requested,"early_cancel_applied":early_cancel_applied,
        "presentation_feedback":state.feedback_tracker.snapshot(),
        "clock_starts":clock_starts,"witness_button_events":state.witness_buttons,
        "consumer_cpu_buffer_maps":0,"consumer_readbacks":0,"consumer_offscreen_targets":0,
        "consumer_fences":{"submitted":records.len(),"retired":retired,"peak_in_flight":peak,
            "limit":3,"cleanup_finish_calls":finishes.get()}});
    report["discard_injected"] = discard_injected.into();
    report["handled_candidate_discards"] = handled_discards.into();
    report["discarded_frame_ids"] = serde_json::json!(state.discarded_frame_ids);
    report["feedback_history_saturated"] = args.saturate_feedback_history.into();
    report["candidate_presentation_receipt"] = serde_json::json!(candidate_receipt.as_ref().map(|receipt|
        serde_json::json!({"submission_id":receipt.submission_id(), "completion":receipt.completion()})));
    report["controller_failure"] = serde_json::json!(controller_failure);
    report["control_transport"] = if args.control_stdin {
        "inherited_unix_stream"
    } else {
        "private_datagram"
    }
    .into();
    if reprepare_started && reprepare_report.is_none() && reprepare_removal_counts.is_none() {
        reprepare_report = sources[0].as_ref().map(Source::report).transpose()?;
    }
    report["reprepared"] = serde_json::json!({"attempted":reprepare_started,
        "aborted":reprepare_aborted,"failure":reprepare_failure,"candidate":reprepare_report,
        "report_error":reprepare_report_error,"removed_lifecycle":reprepare_removal_counts});
    eprintln!("Replacement removing private surfaces");
    drop(leases);
    drop(sources);
    drop(painter);
    drop(sharing);
    drop(windows);
    drop(session);
    report["egl_lifecycle"] = counts.snapshot();
    drop(tree);
    drop(state);
    connection.flush()?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
