// SPDX-License-Identifier: AGPL-3.0-or-later
//! One-shot bounded allocator/caps traces; never map source memory.
use anyhow::Result;
use gstreamer::{self as gst, prelude::*};
use gstreamer_gl as gst_gl;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub type DecoderBuffers = Arc<Mutex<BTreeMap<String, serde_json::Value>>>;

pub fn watch_decoder_buffers(pipeline: &gst::Element) -> Result<DecoderBuffers> {
    let result = DecoderBuffers::default();
    let recorded = result.clone();
    pipeline
        .clone()
        .downcast::<gst::Bin>()
        .map_err(|_| anyhow::anyhow!("playbin is not a bin"))?
        .connect_deep_element_added(move |_, _, element| {
            let is_video_decoder = element.factory().is_some_and(|factory| {
                factory
                    .metadata("klass")
                    .is_some_and(|klass| klass.contains("Decoder") && klass.contains("Video"))
            });
            if !is_video_decoder {
                return;
            }
            if let Some(pad) = element.static_pad("src") {
                let recorded = recorded.clone();
                let name = element.name().to_string();
                let factory_name = element.factory().map(|factory| factory.name().to_string());
                pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
                    if let Some(buffer) = info.buffer()
                        && let Ok(mut recorded) = recorded.lock()
                        && !recorded.contains_key(&name)
                        && recorded.len() < 8
                    {
                        let count = buffer.n_memory();
                        let all_gl = count > 0
                            && count <= 16
                            && (0..count).all(|index| {
                                buffer
                                    .peek_memory(index)
                                    .downcast_memory_ref::<gst_gl::GLMemory>()
                                    .is_some()
                            });
                        let allocators: Vec<_> = (0..count.min(16))
                            .map(|index| {
                                buffer
                                    .peek_memory(index)
                                    .allocator()
                                    .map(|allocator| allocator.memory_type().to_string())
                            })
                            .collect();
                        // Introspection only: no source memory is mapped or copied.
                        recorded.insert(
                            name.clone(),
                            serde_json::json!({
                                "factory": factory_name,
                                "memories": count,
                                "all_memory_is_gl": all_gl,
                                "allocator_types": allocators,
                                "caps": pad.current_caps().map(|caps| caps.to_string()),
                            }),
                        );
                        return gst::PadProbeReturn::Remove;
                    }
                    gst::PadProbeReturn::Ok
                });
            }
        });
    Ok(result)
}
