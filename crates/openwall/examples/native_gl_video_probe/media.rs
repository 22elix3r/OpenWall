// SPDX-License-Identifier: AGPL-3.0-or-later
use super::decoder_trace;
use anyhow::{Context, Result, bail, ensure};
use gstreamer::{self as gst, glib, prelude::*};
use gstreamer_gl::{self as gst_gl, prelude::*};
use std::{
    io::Write,
    os::unix::net::UnixStream,
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct Latest {
    sample: Option<gst::Sample>,
    accepting: bool,
    received: u64,
    replaced: u64,
}

pub type Wake = Arc<Mutex<UnixStream>>;

fn deliver(
    sample: gst::Sample,
    latest: &Arc<Mutex<Latest>>,
    writer: &Wake,
) -> std::result::Result<gst::FlowSuccess, gst::FlowError> {
    let previous = {
        let mut slot = latest.lock().map_err(|_| gst::FlowError::Error)?;
        if !slot.accepting {
            return Ok(gst::FlowSuccess::Ok);
        }
        slot.received += 1;
        if slot.sample.is_some() {
            slot.replaced += 1;
        }
        slot.sample.replace(sample)
    };
    // Releasing a buffer can call into a producer pool. Never do it under the
    // slot lock, or run GL / wait for the UI thread from a streaming callback.
    drop(previous);
    wake(writer);
    Ok(gst::FlowSuccess::Ok)
}

pub fn wake(writer: &Wake) {
    if let Ok(mut writer) = writer.lock() {
        let _ = writer.write_all(&[1]);
    }
}

pub struct Media {
    pipeline: gst::Element,
    sink: gstreamer_app::AppSink,
    bus: gst::Bus,
    latest: Arc<Mutex<Latest>>,
    decoder_buffers: decoder_trace::DecoderBuffers,
    pub eos: bool,
}

impl Media {
    pub fn new(
        path: &Path,
        display: &gstreamer_gl_egl::GLDisplayEGL,
        wrapped: &gst_gl::GLContext,
        writer: Wake,
        paused: bool,
    ) -> Result<Self> {
        let uri = glib::filename_to_uri(path.canonicalize()?, None)?;
        let bin = gst::parse::bin_from_description(
            r#"glupload ! glcolorconvert ! capsfilter caps="video/x-raw(memory:GLMemory),format=RGBA,texture-target=2D" ! appsink name=probe sync=true max-buffers=1 enable-last-sample=false wait-on-eos=false"#,
            true,
        )?;
        let sink = bin
            .by_name("probe")
            .context("appsink missing")?
            .dynamic_cast::<gstreamer_app::AppSink>()
            .map_err(|_| anyhow::anyhow!("not an appsink"))?;
        let pipeline = gst::ElementFactory::make("playbin")
            .property("uri", uri.as_str())
            .property("video-sink", bin)
            .build()?;
        pipeline.set_property_from_str("flags", "video+native-video");
        let mut display_context = gst::Context::new("gst.gl.GLDisplay", true);
        display_context
            .get_mut()
            .context("display context not writable")?
            .set_gl_display(display);
        pipeline.set_context(&display_context);
        let mut app_context = gst::Context::new("gst.gl.app_context", true);
        app_context
            .get_mut()
            .context("app context not writable")?
            .structure_mut()
            .set("context", wrapped);
        pipeline.set_context(&app_context);
        let latest = Arc::new(Mutex::new(Latest {
            accepting: true,
            ..Default::default()
        }));
        let incoming = latest.clone();
        let sample_writer = writer.clone();
        let callbacks = gstreamer_app::AppSinkCallbacks::builder().new_sample(move |sink| {
            let sample = sink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
            deliver(sample, &incoming, &sample_writer)
        });
        let callbacks = if paused {
            let incoming = latest.clone();
            let preroll_writer = writer.clone();
            callbacks.new_preroll(move |sink| {
                let sample = sink.pull_preroll().map_err(|_| gst::FlowError::Eos)?;
                deliver(sample, &incoming, &preroll_writer)
            })
        } else {
            callbacks
        };
        sink.set_callbacks(callbacks.build());
        let bus = pipeline.bus().context("pipeline bus missing")?;
        bus.set_sync_handler(move |_, message| {
            if matches!(
                message.type_(),
                gst::MessageType::Eos | gst::MessageType::Error
            ) {
                wake(&writer);
            }
            gst::BusSyncReply::Pass
        });
        let media = Self {
            decoder_buffers: decoder_trace::watch_decoder_buffers(&pipeline)?,
            pipeline,
            sink,
            bus,
            latest,
            eos: false,
        };
        media.pipeline.set_state(if paused {
            gst::State::Paused
        } else {
            gst::State::Playing
        })?;
        Ok(media)
    }

    pub fn resume(&self) -> Result<()> {
        self.pipeline.set_state(gst::State::Playing)?;
        Ok(())
    }

    #[allow(dead_code)] // Replacement retains the previous source in PAUSED for rollback.
    pub fn pause(&self) -> Result<()> {
        self.pipeline.set_state(gst::State::Paused)?;
        Ok(())
    }

    pub fn take(&self) -> Result<Option<gst::Sample>> {
        Ok(self
            .latest
            .lock()
            .map_err(|_| anyhow::anyhow!("sample slot poisoned"))?
            .sample
            .take())
    }

    pub fn has_sample(&self) -> Result<bool> {
        Ok(self
            .latest
            .lock()
            .map_err(|_| anyhow::anyhow!("sample slot poisoned"))?
            .sample
            .is_some())
    }

    pub fn messages(&mut self) -> Result<()> {
        while let Some(message) = self.bus.pop() {
            match message.view() {
                gst::MessageView::Error(error) => bail!(
                    "native shared GL pipeline failed: {} ({:?})",
                    error.error(),
                    error.debug()
                ),
                gst::MessageView::Eos(_) => self.eos = true,
                _ => {}
            }
        }
        Ok(())
    }

    // Caller must first release every current sample and retire every consumer fence.
    pub fn rewind(&mut self) -> Result<()> {
        ensure!(self.eos, "rewind requires completed EOS delivery");
        drop(self.take()?);
        // EOS follows the last completed sample callback. Keep delivery enabled
        // so a fast new stream cannot lose its PTS-zero frame inside seek_simple.
        self.pipeline.seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::ClockTime::ZERO,
        )?;
        self.eos = false;
        Ok(())
    }

    pub fn counters(&self) -> Result<(u64, u64)> {
        let slot = self
            .latest
            .lock()
            .map_err(|_| anyhow::anyhow!("sample slot poisoned"))?;
        Ok((slot.received, slot.replaced))
    }

    pub fn decoders(&self) -> Result<Vec<String>> {
        let bin = self
            .pipeline
            .clone()
            .downcast::<gst::Bin>()
            .map_err(|_| anyhow::anyhow!("playbin is not a bin"))?;
        let mut result = Vec::new();
        for element in bin.iterate_recurse().into_iter().take(512) {
            if let Some(factory) = element?.factory()
                && factory
                    .metadata("klass")
                    .is_some_and(|klass| klass.contains("Decoder") && klass.contains("Video"))
            {
                result.push(factory.name().to_string());
            }
        }
        Ok(result)
    }

    pub fn decoder_buffers(&self) -> Result<serde_json::Value> {
        let buffers = self
            .decoder_buffers
            .lock()
            .map_err(|_| anyhow::anyhow!("decoder trace poisoned"))?;
        Ok(serde_json::to_value(&*buffers)?)
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        let pending = self.latest.lock().ok().and_then(|mut slot| {
            slot.accepting = false;
            slot.sample.take()
        });
        drop(pending);
        self.sink
            .set_callbacks(gstreamer_app::AppSinkCallbacks::builder().build());
        self.bus.unset_sync_handler();
        // Leases/current sample are declared after Media and dropped before this guard.
        // The wrapped context and borrowed EGLDisplay outlive all of these Gst objects.
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
