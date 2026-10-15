# Licenses and source provenance

Original OpenWall code is licensed AGPL-3.0-or-later. Original sample content is CC0-1.0.
Third-party copyrights and licenses remain with their respective authors.

| Component | Source / revision | License / use |
| --- | --- | --- |
| Kirie `kirie-formats` | [UnhingedSoftware/kirie](https://github.com/UnhingedSoftware/kirie), `e6fa67a4860fc512c18373cec02a67579bcb4873` | AGPL-3.0-or-later; reused PKG/TEX format readers. Cargo fetches its original source/license. |
| wgpu / Naga | [gfx-rs/wgpu](https://github.com/gfx-rs/wgpu), versions in Cargo.lock | MIT OR Apache-2.0; GPU abstraction/shader validation. |
| CXX-Qt | [KDAB/cxx-qt](https://github.com/KDAB/cxx-qt), versions in Cargo.lock | MIT OR Apache-2.0; Rust/Qt bridge. Build/bridge patterns follow its published examples. |
| Qt 6 | [Qt licensing](https://doc.qt.io/qt-6/licensing.html) | Qt module/third-party licenses apply, including LGPLv3/GPL options. Dynamically linked platform installation in this source-build snapshot. |
| GStreamer / Rust bindings | [GStreamer](https://gstreamer.freedesktop.org/), versions in Cargo.lock / host packages | LGPL family and compatible component licenses; host codec plugins are separately installed. |
| winit / Wayland Rust crates | Sources and exact versions in Cargo.lock | MIT / Apache-2.0 / compatible licenses as declared by each crate. |
| AVFoundation, AppKit, CoreGraphics, CoreAnimation | System frameworks on macOS | Platform APIs supplied by the operating system, not redistributed by OpenWall. |

No Kirie renderer, web/CEF backend, launcher, Steam integration or bundled shared
Wallpaper Engine assets are imported. The original OpenWall WGSL shader implements
ordinary sprite compositing; it does not contain original engine shader code.

## Research references

[ScreenPlay](https://gitlab.com/kelteseth/ScreenPlay) informed the Qt UI/platform evaluation.
[Haru](https://github.com/UnhingedSoftware/haru) informed component separation.
[Phonto](https://github.com/museslabs/phonto) informed native video/platform feasibility.
[Lively](https://github.com/lively-community/lively) informed process isolation.
[Hanabi](https://github.com/jeffshee/gnome-ext-hanabi) and
[waywallen-display](https://github.com/waywallen/waywallen-display) inform future shell adapters.
WallpaperMachine's [licensing record](https://github.com/WallpaperMachine/WallpaperMachine/blob/main/LICENSING.md)
documents why its GPL-2.0-only renderer is unsuitable for the selected dependency stack.
No code from these research references is copied into this snapshot except the explicitly
declared Kirie dependency and ordinary CXX-Qt example patterns.

Release packaging must include exact corresponding source/build scripts, license texts,
third-party notices and a dependency SBOM. This document records provenance; it is not a
claim that every transitive dependency or future binary package has been fully audited.
