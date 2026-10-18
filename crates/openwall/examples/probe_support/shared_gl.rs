// SPDX-License-Identifier: AGPL-3.0-or-later
//! Shared diagnostic texture validation and bounded source-buffer leases.
use anyhow::{Context, Result, bail, ensure};
use glow::HasContext;
use gstreamer as gst;
use gstreamer_gl::{self as gst_gl, prelude::*};
use std::{
    cell::Cell,
    collections::VecDeque,
    num::NonZeroU32,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

// A sample must remain leased until the consumer's read commands have completed.
// The guard also covers draw/fence failures before it can enter the bounded queue.
pub struct Lease<'a> {
    pub gl: &'a glow::Context,
    pub sample: Option<gst::Sample>,
    pub fence: Option<glow::NativeFence>,
    pub pending: bool,
    pub cleanup_finishes: &'a Cell<u32>,
    pub submitted_at: Instant,
}

impl Lease<'_> {
    pub fn seal(&mut self) -> Result<()> {
        // SAFETY: the owned context is current and the lease retains the source.
        unsafe {
            let fence = self
                .gl
                .fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)
                .map_err(anyhow::Error::msg)?;
            ensure!(!fence.0.is_null(), "consumer fence creation returned null");
            self.fence = Some(fence);
            self.submitted_at = Instant::now();
            self.gl.flush();
            ensure!(
                self.gl.get_error() == glow::NO_ERROR,
                "consumer fence submission failed"
            );
        }
        Ok(())
    }

    pub fn poll(&mut self, wait: bool) -> Result<bool> {
        let fence = self.fence.context("consumer fence is missing")?;
        // SAFETY: this fence belongs to the current context, and has not been deleted.
        let result = unsafe {
            self.gl
                .client_wait_sync(fence, 0, if wait { 1_000_000 } else { 0 })
        };
        match result {
            glow::ALREADY_SIGNALED | glow::CONDITION_SATISFIED => {
                // SAFETY: the completed fence is deleted once before releasing the sample.
                unsafe { self.gl.delete_sync(fence) };
                self.fence = None;
                self.pending = false;
                self.sample.take();
                Ok(true)
            }
            glow::TIMEOUT_EXPIRED => Ok(false),
            _ => bail!("consumer fence wait failed: {result:#x}"),
        }
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        // SAFETY: the creating context outlives every lease and is still current.
        // Only an incomplete/error path finishes globally. Successful retirement
        // deletes the fence and releases the sample without glFinish.
        unsafe {
            if self.pending {
                self.gl.finish();
                self.cleanup_finishes.set(self.cleanup_finishes.get() + 1);
            }
            if let Some(fence) = self.fence.take() {
                self.gl.delete_sync(fence);
            }
        }
    }
}

pub fn retire_one(
    leases: &mut VecDeque<Lease<'_>>,
    wait: bool,
    interrupted: &AtomicBool,
) -> Result<bool> {
    let Some(first) = leases.front_mut() else {
        return Ok(false);
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        ensure!(
            !interrupted.load(Ordering::Relaxed),
            "owned GL probe interrupted"
        );
        if first.poll(wait)? {
            leases.pop_front();
            return Ok(true);
        }
        if !wait {
            return Ok(false);
        }
        ensure!(
            Instant::now() < deadline,
            "consumer fence did not signal within five seconds"
        );
    }
}

pub struct Source {
    pub width: u32,
    pub height: u32,
    pub texture: glow::NativeTexture,
    pub texture_size: (i32, i32),
    pub pts_ns: Option<u64>,
    pub caps: String,
    pub memory: serde_json::Value,
}

pub fn source(
    sample: &gst::Sample,
    wrapped: &gst_gl::GLContext,
    gl: &glow::Context,
) -> Result<Source> {
    let caps = sample.caps().context("sample has no caps")?;
    let info = gstreamer_video::VideoInfo::from_caps(caps)?;
    let (width, height) = (info.width(), info.height());
    ensure!(
        info.format() == gstreamer_video::VideoFormat::Rgba && info.n_planes() == 1,
        "proof supports one-plane RGBA only"
    );
    ensure!(
        info.interlace_mode() == gstreamer_video::VideoInterlaceMode::Progressive
            && matches!(
                info.multiview_mode(),
                gstreamer_video::VideoMultiviewMode::None
                    | gstreamer_video::VideoMultiviewMode::Mono
            ),
        "proof supports progressive mono frames only"
    );
    ensure!(
        width > 0
            && height > 0
            && width <= 16_384
            && height <= 16_384
            && width as usize * height as usize * 4 <= 256 * 1024 * 1024,
        "frame dimensions exceed proof limits"
    );
    let buffer = sample.buffer().context("sample has no buffer")?;
    ensure!(buffer.n_memory() == 1, "proof requires one GL memory block");
    ensure!(
        buffer
            .meta::<gstreamer_video::VideoAffineTransformationMeta>()
            .is_none(),
        "affine metadata is not supported by this proof"
    );
    ensure!(
        buffer.meta::<gstreamer_video::VideoCropMeta>().is_none(),
        "crop metadata is not supported by this proof"
    );
    let memory = buffer
        .peek_memory(0)
        .downcast_memory_ref::<gst_gl::GLMemory>()
        .context("sample memory is not a GL texture")?;
    ensure!(
        memory.texture_target() == gst_gl::GLTextureTarget::_2d,
        "proof requires a 2D texture"
    );
    ensure!(
        memory.context().can_share(wrapped),
        "producer did not share the owned context"
    );
    let size = (memory.texture_width(), memory.texture_height());
    ensure!(
        size.0 >= width as i32 && size.1 >= height as i32 && size.0 <= 16_384 && size.1 <= 16_384,
        "texture dimensions are invalid"
    );
    let sync = buffer
        .meta::<gst_gl::GLSyncMeta>()
        .context("producer provided no GL synchronization meta")?;
    sync.wait(wrapped);
    let texture =
        glow::NativeTexture(NonZeroU32::new(memory.texture_id()).context("texture ID is zero")?);
    // SAFETY: context is current; check before creating any of our own texture names.
    ensure!(
        unsafe { gl.is_texture(texture) },
        "shared texture is not visible in the owned context"
    );
    Ok(Source {
        width,
        height,
        texture,
        texture_size: size,
        pts_ns: buffer.pts().map(|pts| pts.nseconds()),
        caps: caps.to_string(),
        memory: serde_json::json!({"allocator_type":memory.allocator().map(|allocator|allocator.type_().name().to_owned()),
            "texture_width":size.0,"texture_height":size.1,"texture_target":"2D","shared_context":true,
            "producer_sync_meta":true}),
    })
}
