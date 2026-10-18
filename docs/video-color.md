# SDR video conversion regression checks

`scripts/test_video_color.py` exercises the windowless Rust transfer probe with original
lossless VP9 fixtures. It checks BT.709 and BT.601/SMPTE 170M matrices, each with limited
and full range. An asymmetric grid includes black, white, gray and saturated colors;
the second frame is different so capturing the wrong frame fails. The 130×68 size also
exercises decoder/GL alignment. No downloaded media or engine assets are used.

```sh
cargo test -p openwall --example video_transfer_probe --locked
cargo build -p openwall --example video_transfer_probe --locked
python3 scripts/test_video_color.py --software-gl
```

The script builds raw I420 planes, encodes them losslessly, and independently verifies
that FFmpeg decodes the first frame to the original YUV bytes. Input and output color
metadata are both specified; output metadata alone can cause FFmpeg to convert the raw
fixture before encoding. The Rust probe then decodes through `vp9dec` and captures its
first converted RGBA frame. The test checks PTS zero, dimensions, opaque alpha and three
interior samples per patch against independent floating-point equations. It does not
reuse GStreamer or FFmpeg's RGB conversion as the reference.

The reference uses the luma coefficients and limited-range scaling in
[BT.709](https://www.itu.int/rec/R-REC-BT.709-6-201506-I/en) and
[BT.601](https://www.itu.int/rec/R-REC-BT.601-7-201103-I/en). Full-range normalization
uses 255 levels with chroma centered at 128. These tests cover encoded SDR sample
conversion, not gamut conversion, transfer-function remapping or display calibration.

## Known CPU precision gap

Local GStreamer 1.26.11/Mesa software-GL results on 2026-10-18:

| Matrix/range | Default CPU conversion: max channel error | GL conversion: max channel error |
| --- | ---: | ---: |
| BT.709 limited | 5 | 0 |
| BT.709 full | 2 | 0 |
| BT.601 limited | 5 | 0 |
| BT.601 full | 2 | 0 |

Errors are absolute 8-bit channel levels at patch interiors. In one limited-range
BT.709 patch, the CPU chain returned green 211 where the reference and GL chain returned
216. The default CPU chain uses `videoconvert`; upstream has specialized integer
[conversion paths](https://github.com/GStreamer/gstreamer/blob/1.26.11/subprojects/gst-plugins-base/gst-libs/gst/video/video-converter.c)
and [ORC operations](https://github.com/GStreamer/gstreamer/blob/1.26.11/subprojects/gst-plugins-base/gst-libs/gst/video/video-orc.orc).
The source and observed bias warrant further precision work; this suite does not assert
that the existing CPU output is an accurate reference for GL.

CI bounds the current CPU regression error to six levels and GL error to one. Those are
separate checks: a passing regression suite does not establish CPU/GL parity. JSON
reports each measured error and an independent `all_within_one_level_of_reference` flag;
the local result for that flag is **false**. Retain the gap as an open M0 issue when
choosing a production conversion path. Do not silently widen the budgets after a failure.

## Limits

Software GL checks are deterministic infrastructure checks, not hardware qualification.
The suite samples patch interiors to isolate matrix/range and orientation; it does not
qualify chroma interpolation at edges, HDR, 10-bit content, alpha-bearing video, dynamic
format changes, hardware decoders or compositor presentation. GL output caps can also
omit color metadata retained by the CPU chain. Matching encoded bytes does not settle
that presentation contract. Production rendering remains unchanged by this diagnostic.

Separate Rust tests exercise a padded, nonzero-offset `GstVideoMeta` layout and confirm
that capture preserves pixel rows without overwriting an existing PNG. Capture uses the
[GStreamer video-frame API](https://gstreamer.freedesktop.org/documentation/video/video-frame.html)
to honor the allocator's mapping and layout instead of assuming tightly packed bytes.
