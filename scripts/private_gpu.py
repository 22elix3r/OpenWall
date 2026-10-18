# SPDX-License-Identifier: AGPL-3.0-or-later
"""Explicit existing-driver selection for isolated headless hardware diagnostics."""
from pathlib import Path
import stat


def nvidia_renderer(requested_node=None):
    vendor = Path("/usr/share/glvnd/egl_vendor.d/10_nvidia.json")
    assert vendor.is_file(), "an already installed NVIDIA EGL driver is required"
    candidates = [Path("/dev/dri") / node.name for node in sorted(Path("/sys/class/drm").glob("renderD*"))
                  if (node / "device/vendor").is_file() and (node / "device/vendor").read_text().strip() == "0x10de"]
    assert requested_node or len(candidates) == 1, "select an existing NVIDIA render node with --render-node"
    node = (requested_node or candidates[0]).resolve(strict=True)
    assert node in candidates and stat.S_ISCHR(node.stat().st_mode), "not an NVIDIA DRM render node"
    assert (Path("/sys/class/drm") / node.name / "device/driver").resolve().name == "nvidia", "the existing NVIDIA driver is required"
    environment = {"WLR_RENDERER": "gles2", "WLR_RENDER_DRM_DEVICE": str(node),
                   "LIBGL_ALWAYS_SOFTWARE": "0", "__EGL_VENDOR_LIBRARY_FILENAMES": str(vendor)}
    return vendor, node, environment
