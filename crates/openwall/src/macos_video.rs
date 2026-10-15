// SPDX-License-Identifier: AGPL-3.0-or-later
//! AVFoundation owns decoding/compositing; no CPU frame readback.
use anyhow::{Context, Result, bail};
use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::NSView;
use objc2_av_foundation::{
    AVLayerVideoGravityResizeAspectFill, AVPlayerItem, AVPlayerLayer, AVPlayerLooper,
    AVPlayerLooperStatus, AVQueuePlayer,
};
use objc2_foundation::{NSString, NSURL};
use objc2_quartz_core::CAAutoresizingMask;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::path::Path;

pub struct Player {
    layer: Retained<AVPlayerLayer>,
    looper: Retained<AVPlayerLooper>,
    queue: Retained<AVQueuePlayer>,
}

impl Player {
    pub fn new(window: &winit::window::Window, path: &Path) -> Result<Self> {
        let mtm =
            MainThreadMarker::new().context("video player must start on the AppKit main thread")?;
        let RawWindowHandle::AppKit(handle) = window.window_handle()?.as_raw() else {
            bail!("not an AppKit window")
        };
        // SAFETY: winit owns this live NSView, and all AVFoundation access stays on
        // this main thread. The view outlives Player and its retained layer.
        unsafe {
            let view = &*handle.ns_view.as_ptr().cast::<NSView>();
            view.setWantsLayer(true);
            let name = NSString::from_str(
                path.canonicalize()?
                    .to_str()
                    .context("video path must be UTF-8")?,
            );
            let url = NSURL::fileURLWithPath(&name);
            let item = AVPlayerItem::playerItemWithURL(&url, mtm);
            let queue = AVQueuePlayer::new(mtm);
            queue.setMuted(true);
            let looper = AVPlayerLooper::playerLooperWithPlayer_templateItem(&queue, &item);
            let layer = AVPlayerLayer::playerLayerWithPlayer(Some(&queue));
            layer.setVideoGravity(
                AVLayerVideoGravityResizeAspectFill.context("missing video gravity")?,
            );
            layer.setFrame(view.bounds());
            layer.setContentsScale(window.scale_factor());
            layer.setAutoresizingMask(
                CAAutoresizingMask::LayerWidthSizable | CAAutoresizingMask::LayerHeightSizable,
            );
            view.layer()
                .context("native view has no layer")?
                .addSublayer(&layer);
            queue.play();
            Ok(Self {
                layer,
                looper,
                queue,
            })
        }
    }

    pub fn ready(&self) -> Result<bool> {
        // SAFETY: the retained player is queried only on its owning main thread.
        unsafe {
            if self.looper.status() == AVPlayerLooperStatus::Failed {
                bail!(
                    "AVFoundation could not play video: {}",
                    self.looper
                        .error()
                        .map(|x| x.localizedDescription().to_string())
                        .unwrap_or_default()
                );
            }
            Ok(self.layer.isReadyForDisplay())
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: Player is owned/dropped by the main-thread event loop.
        unsafe {
            self.queue.pause();
        }
        self.layer.removeFromSuperlayer();
    }
}
