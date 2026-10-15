// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native GStreamer presentation; decoded frames are never sent through UI IPC.
use anyhow::{Context, Result, bail};
use gstreamer::{self as gst, glib, prelude::*};
use gstreamer_video::prelude::*;
use std::{
    io::Write,
    os::unix::net::UnixStream,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

pub struct Player {
    pipeline: gst::Element,
    bus: gst::Bus,
}

impl Player {
    pub fn new(
        path: &Path,
        native: Option<(usize, usize, u32, u32)>,
        wake: Option<Arc<Mutex<UnixStream>>>,
    ) -> Result<Self> {
        gst::init()?;
        let path = path.canonicalize()?;
        let uri = glib::filename_to_uri(&path, None)?;
        let pipeline = gst::ElementFactory::make("playbin")
            .property("uri", uri.as_str())
            .property("mute", true)
            .build()
            .context("GStreamer playbin is missing")?;
        pipeline.set_property_from_str("flags", "video");
        if let Some((display, surface, width, height)) = native {
            let sink = gst::ElementFactory::make("waylandsink")
                .build()
                .context("GStreamer waylandsink is missing")?;
            if let Some(pad) = sink.static_pad("sink") {
                pad.add_probe(gst::PadProbeType::BUFFER, |pad, info| {
                    let dmabuf = info.buffer().is_some_and(|buffer| {
                        buffer.n_memory() > 0 && buffer.peek_memory(0).is_type("DMABuf")
                    });
                    eprintln!(
                        "First video buffer: DMA-BUF={dmabuf}; caps={}",
                        pad.current_caps()
                            .map(|x| x.to_string())
                            .unwrap_or_default()
                    );
                    gst::PadProbeReturn::Remove
                });
            }
            let mut context = gst::Context::new("GstWaylandDisplayHandleContextType", true);
            let structure = context
                .get_mut()
                .context("context is not writable")?
                .structure_mut();
            // SAFETY: wl_display supports cross-thread use. The owning Wayland connection
            // outlives the pipeline; Player::drop puts the pipeline in NULL before teardown.
            let pointer = unsafe { (display as glib::Pointer).to_value().into_send_value() };
            structure.set_value("handle", pointer.clone());
            structure.set_value("display", pointer);
            sink.set_context(&context);
            pipeline.set_context(&context);
            let overlay = sink
                .clone()
                .dynamic_cast::<gstreamer_video::VideoOverlay>()
                .map_err(|_| anyhow::anyhow!("sink has no video overlay interface"))?;
            // SAFETY: the surface is live on the supplied display, and remains alive
            // until after the pipeline has stopped and released its subsurface.
            unsafe {
                overlay.set_window_handle(surface);
            }
            overlay.set_render_rectangle(0, 0, width as i32, height as i32)?;
            pipeline.set_property("video-sink", sink);
        }
        pipeline.connect("deep-element-added", false, |values| {
            if let Ok(element) = values[2].get::<gst::Element>()
                && let Some(factory) = element.factory()
                && factory
                    .metadata("klass")
                    .is_some_and(|x| x.contains("Decoder"))
            {
                eprintln!("GStreamer decoder: {}", factory.name());
            }
            None
        });
        let bus = pipeline
            .bus()
            .context("GStreamer player has no message bus")?;
        if let Some(wake) = wake {
            bus.set_sync_handler(move |_, message| {
                if matches!(
                    message.type_(),
                    gst::MessageType::Eos | gst::MessageType::Error
                ) && let Ok(mut writer) = wake.lock()
                {
                    let _ = writer.write_all(&[1]);
                }
                gst::BusSyncReply::Pass
            });
        }
        let player = Self { pipeline, bus };
        player
            .pipeline
            .set_state(gst::State::Playing)
            .context("could not start video playback")?;
        Ok(player)
    }

    pub fn poll(&self) -> Result<()> {
        for _ in 0..512 {
            let Some(message) = self.bus.pop() else { break };
            match message.view() {
                gst::MessageView::Eos(..) => {
                    self.pipeline
                        .seek_simple(
                            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                            gst::ClockTime::ZERO,
                        )
                        .context("video could not loop")?;
                    eprintln!("Video loop restarted");
                }
                gst::MessageView::Error(error) => bail!(
                    "video playback failed: {} ({})",
                    error.error(),
                    error.debug().unwrap_or_default()
                ),
                _ => {}
            }
        }
        Ok(())
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.bus.unset_sync_handler();
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

pub fn preview(path: &Path, duration: Option<Duration>) -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    let stopped = Arc::new(AtomicBool::new(false));
    let signal = stopped.clone();
    ctrlc::set_handler(move || signal.store(true, Ordering::Relaxed))?;
    let player = Player::new(path, None, None)?;
    let deadline = duration.map(|duration| Instant::now() + duration);
    while !stopped.load(Ordering::Relaxed) && deadline.is_none_or(|x| Instant::now() < x) {
        // This wait is confined to the preview worker. Decoder/sink pacing is native.
        std::thread::sleep(Duration::from_millis(100));
        player.poll()?;
    }
    Ok(())
}
