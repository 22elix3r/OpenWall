// SPDX-License-Identifier: AGPL-3.0-or-later
//! Windowless M0 transfer diagnostic, not a wallpaper presenter or energy benchmark.
#[cfg(all(target_os = "linux", feature = "video"))]
mod probe {
    use anyhow::{Context, Result, bail, ensure};
    use clap::{Parser, ValueEnum};
    use gstreamer::{self as gst, glib, prelude::*};
    use serde::Serialize;
    use std::{
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    #[derive(Clone, Copy, Debug, Serialize, ValueEnum)]
    #[serde(rename_all = "kebab-case")]
    enum Mode {
        Cpu,
        GlDownload,
        Dmabuf,
    }

    #[derive(Parser)]
    #[command(about = "Measure decode/conversion into a fake sink; creates no wallpaper")]
    struct Args {
        video: PathBuf,
        #[arg(long, value_enum, default_value_t = Mode::Cpu)]
        mode: Mode,
        #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u64).range(1..=3600))]
        seconds: u64,
        #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u64).range(0..=300))]
        warmup: u64,
    }

    #[derive(Serialize)]
    struct BufferReport {
        caps: String,
        memories: usize,
        bytes: usize,
        all_memories_dmabuf: bool,
    }

    struct Playing(gst::Element);

    impl Drop for Playing {
        fn drop(&mut self) {
            let _ = self.0.set_state(gst::State::Null);
        }
    }

    fn cpu_seconds() -> f64 {
        let time = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
        time.tv_sec as f64 + time.tv_nsec as f64 / 1_000_000_000.0
    }

    fn wake(bus: &gst::Bus) {
        let _ = bus.post(gst::message::Application::new(gst::Structure::new_empty(
            "openwall-probe-wake",
        )));
    }

    fn decoder_caps(pipeline: &gst::Element) -> Result<Vec<serde_json::Value>> {
        let bin = pipeline
            .clone()
            .dynamic_cast::<gst::Bin>()
            .map_err(|_| anyhow::anyhow!("playbin is not a bin"))?;
        let mut result = Vec::new();
        for element in bin.iterate_recurse().into_iter().take(512) {
            let element = element?;
            if let Some(factory) = element.factory()
                && factory
                    .metadata("klass")
                    .is_some_and(|klass| klass.contains("Decoder") && klass.contains("Video"))
            {
                result.push(serde_json::json!({
                    "factory": factory.name().to_string(),
                    "output_caps": element.static_pad("src").and_then(|pad| pad.current_caps()).map(|caps| caps.to_string()),
                }));
            }
        }
        Ok(result)
    }

    pub fn run() -> Result<()> {
        let args = Args::parse();
        if !matches!(args.mode, Mode::Cpu) {
            ensure!(
                std::env::var("GST_GL_WINDOW").as_deref() == Ok("surfaceless"),
                "GL probes require GST_GL_WINDOW=surfaceless to prohibit a native output window"
            );
        }
        gst::init()?;
        let uri = glib::filename_to_uri(args.video.canonicalize()?, None)?;
        // Fixed descriptions only; paths are passed as URIs, never pipeline syntax.
        let description = match args.mode {
            Mode::Cpu => r#"videoconvert ! capsfilter caps="video/x-raw,format=RGBA""#,
            Mode::GlDownload => {
                r#"glupload ! glcolorconvert ! gldownload ! capsfilter caps="video/x-raw,format=RGBA""#
            }
            Mode::Dmabuf => {
                r#"glupload ! glcolorconvert ! gldownload ! capsfilter caps="video/x-raw(memory:DMABuf),format=DMA_DRM""#
            }
        };
        let sink_bin = gst::parse::bin_from_description(
            &format!("{description} ! fakesink name=probe sync=true enable-last-sample=false"),
            true,
        )
        .context("conversion elements are missing")?;
        let sink = sink_bin.by_name("probe").context("fake sink is missing")?;
        let pipeline = gst::ElementFactory::make("playbin")
            .property("uri", uri.as_str())
            .property("mute", true)
            .property("video-sink", sink_bin)
            .build()?;
        // The sink bin owns conversion and negotiation. Hidden playsink converters
        // would change the measured path and reject DMA_DRM before export is tested.
        pipeline.set_property_from_str("flags", "video+native-video");
        let bus = pipeline.bus().context("playbin has no bus")?;
        let first = Arc::new(Mutex::new(None));
        let capture = first.clone();
        let capture_bus = bus.clone();
        sink.static_pad("sink")
            .context("fake sink has no pad")?
            .add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                move |pad, info| {
                    let buffer = info
                        .buffer()
                        .map(|buffer| buffer.as_ref())
                        .or_else(|| info.buffer_list().and_then(|list| list.get(0)));
                    let Some(buffer) = buffer else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let count = buffer.n_memory();
                    let report = BufferReport {
                        caps: pad
                            .current_caps()
                            .map(|caps| caps.to_string())
                            .unwrap_or_default(),
                        memories: count,
                        bytes: buffer.size(),
                        all_memories_dmabuf: count > 0
                            && (0..count).all(|index| buffer.peek_memory(index).is_type("DMABuf")),
                    };
                    if let Ok(mut first) = capture.lock() {
                        *first = Some((Instant::now(), report));
                    }
                    eprintln!("Transfer probe received its first sink buffer");
                    wake(&capture_bus);
                    gst::PadProbeReturn::Remove
                },
            );
        let stopped = Arc::new(AtomicBool::new(false));
        let stop_flag = stopped.clone();
        let stop_bus = bus.clone();
        ctrlc::set_handler(move || {
            stop_flag.store(true, Ordering::Relaxed);
            wake(&stop_bus);
        })?;
        let started = Instant::now();
        let startup_deadline = started + Duration::from_secs(15);
        let mut baseline = None;
        let mut loops = 0;
        let mut decoders = Vec::new();
        let playing = Playing(pipeline);
        playing.0.set_state(gst::State::Playing)?;
        let (elapsed, cpu) = loop {
            ensure!(!stopped.load(Ordering::Relaxed), "probe interrupted");
            let now = Instant::now();
            let ready = first
                .lock()
                .map_err(|_| anyhow::anyhow!("probe state poisoned"))?
                .as_ref()
                .map(|(at, _)| *at);
            let next = if let Some(ready) = ready {
                if baseline.is_none() && now >= ready + Duration::from_secs(args.warmup) {
                    decoders = decoder_caps(&playing.0)?;
                    baseline = Some((Instant::now(), cpu_seconds()));
                }
                if let Some((at, before)) = baseline {
                    if now >= at + Duration::from_secs(args.seconds) {
                        break (at.elapsed().as_secs_f64(), cpu_seconds() - before);
                    }
                    at + Duration::from_secs(args.seconds)
                } else {
                    ready + Duration::from_secs(args.warmup)
                }
            } else {
                ensure!(
                    now < startup_deadline,
                    "no decoded buffer within 15 seconds"
                );
                startup_deadline
            };
            let timeout = gst::ClockTime::from_nseconds(
                next.saturating_duration_since(Instant::now()).as_nanos() as u64,
            );
            if let Some(message) = bus.timed_pop_filtered(
                timeout,
                &[
                    gst::MessageType::Eos,
                    gst::MessageType::Error,
                    gst::MessageType::Application,
                ],
            ) {
                match message.view() {
                    gst::MessageView::Eos(..) => {
                        playing.0.seek_simple(
                            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                            gst::ClockTime::ZERO,
                        )?;
                        loops += 1;
                    }
                    gst::MessageView::Error(error) => {
                        bail!("{} ({})", error.error(), error.debug().unwrap_or_default())
                    }
                    _ => {}
                }
            }
        };
        let first = first
            .lock()
            .map_err(|_| anyhow::anyhow!("probe state poisoned"))?;
        let (ready, buffer) = first.as_ref().context("sink did not receive a buffer")?;
        if matches!(args.mode, Mode::Dmabuf) {
            ensure!(
                buffer.all_memories_dmabuf,
                "DMA-BUF probe received non-DMA-BUF memory"
            );
        }
        let result = serde_json::json!({
            "schema_version": 1, "status": "passed", "scope": "decode_convert_fake_sink",
            "mode": args.mode, "gstreamer": gst::version_string().to_string(), "warmup_seconds": args.warmup,
            "sample_seconds": elapsed, "cpu_percent_one_core": cpu / elapsed * 100.0,
            "startup_to_sink_buffer_seconds": ready.duration_since(started).as_secs_f64(),
            "first_sink_buffer": buffer, "decoders": decoders, "loops": loops,
        });
        drop(first);
        if let Some(message) = bus.pop_filtered(&[gst::MessageType::Error])
            && let gst::MessageView::Error(error) = message.view()
        {
            bail!("{} ({})", error.error(), error.debug().unwrap_or_default());
        }
        // Finish teardown before publishing a successful report.
        playing
            .0
            .set_state(gst::State::Null)
            .context("could not stop probe")?;
        drop(playing);
        println!("{}", serde_json::to_string_pretty(&result)?);
        Ok(())
    }
}

#[cfg(all(target_os = "linux", feature = "video"))]
fn main() -> anyhow::Result<()> {
    probe::run()
}

#[cfg(not(all(target_os = "linux", feature = "video")))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("video_transfer_probe requires Linux with the video feature")
}
