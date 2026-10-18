# SPDX-License-Identifier: AGPL-3.0-or-later
"""Original planar SDR fixtures and independent reference equations."""
WIDTH, HEIGHT = 130, 68  # Deliberately not a common decoder/GL row alignment.
PATCH_WIDTH, PATCH_HEIGHT = 26, 34


def patches(full_range):
    return [(0 if full_range else 16, 128, 128),
            (255 if full_range else 235, 128, 128),
            (81, 90, 240), (145, 54, 34), (41, 240, 110),
            (126, 128, 128), (200, 80, 190), (64, 180, 90),
            (100, 160, 160), (180, 100, 100)]


def original_yuv(colors):
    planes = []
    for component, scale in [(0, 1), (1, 2), (2, 2)]:
        plane = bytearray()
        for row in range(HEIGHT // scale):
            for column in range(WIDTH // scale):
                patch = (row * scale // PATCH_HEIGHT) * 5 + column * scale // PATCH_WIDTH
                plane.append(colors[patch][component])
        planes.append(bytes(plane))
    return b"".join(planes)


def expected_rgba(yuv, matrix, full_range):
    # Independently evaluate the BT.601/BT.709 Y'CbCr equations in floating point.
    # No conversion implementation from OpenWall, GStreamer or FFmpeg is reused.
    kr, kb = (.2126, .0722) if matrix == "bt709" else (.299, .114)
    y = yuv[0] / 255 if full_range else (yuv[0] - 16) / 219
    cb, cr = ((value - 128) / (255 if full_range else 224) for value in yuv[1:])
    red, blue = y + 2 * (1 - kr) * cr, y + 2 * (1 - kb) * cb
    green = (y - kr * red - kb * blue) / (1 - kr - kb)
    return tuple(round(max(0, min(1, value)) * 255) for value in [red, green, blue]) + (255,)


def enlarged_yuv(colors):
    # Preserve the independently generated samples, doubling each plane in both
    # axes to satisfy nvvp9dec's minimum dimensions (the usual fixture is 130x68).
    source = original_yuv(colors)
    result = bytearray()
    offset = 0
    for width, height in [(WIDTH, HEIGHT), (WIDTH // 2, HEIGHT // 2), (WIDTH // 2, HEIGHT // 2)]:
        for row in range(height):
            expanded = bytes(value for value in source[offset + row * width:offset + (row + 1) * width]
                             for _ in range(2))
            result.extend(expanded)
            result.extend(expanded)
        offset += width * height
    return bytes(result)
