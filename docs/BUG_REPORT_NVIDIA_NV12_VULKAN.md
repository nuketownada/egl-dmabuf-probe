# NVIDIA Vulkan: `VkImageDrmFormatModifierExplicitCreateInfoEXT` rejects NV12 layouts that the parallel list-based path accepts

`VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`, NVIDIA 595.71.05,
proprietary userspace + open or proprietary kernel modules, X11.

## Summary

NVIDIA's Vulkan implementation accepts NV12 dma-buf imports via the
**list-based** path (`VkImageDrmFormatModifierListCreateInfoEXT` —
"buffer is one of these modifiers, you pick") used by libplacebo /
mpv / Wayland compositors, but **rejects the same buffer via the
explicit-layout path** (`VkImageDrmFormatModifierExplicitCreateInfoEXT` —
"buffer is exactly this modifier with this plane layout") used by
Chromium/ANGLE.

Concretely:

| Caller | Path | Result |
| --- | --- | --- |
| NVIDIA's GBM | `gbm_bo_create(NV12)` | ok, picks modifier `0x300000000606014` (block-linear) |
| NVIDIA's EGL | `eglCreateImage(EGL_LINUX_DMA_BUF_EXT)` on that fd | ok (both with and without modifier in attribs) |
| libplacebo | `VkImageDrmFormatModifierListCreateInfoEXT` on that fd | ok — smooth NV12 playback in mpv via VAAPI |
| **This probe / ANGLE** | **`VkImageDrmFormatModifierExplicitCreateInfoEXT`** | **`VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`** |

So NVIDIA's Vulkan can in fact import these buffers — only the
strict, explicit-layout entrypoint is broken. The result is that any
Vulkan client using `VkImageDrmFormatModifierExplicitCreateInfoEXT`
(notably ANGLE's `DmaBufImageSiblingVkLinux.cpp`) cannot use
hardware video decode on NVIDIA X11.

This is the driver-side cause of the Chromium failure
`DmaBufImageSiblingVkLinux.cpp:initImpl:616 VK_ERROR_FEATURE_NOT_PRESENT`
seen when running with `--use-angle=vulkan`. The error bubbles up
through ANGLE as `FEATURE_NOT_PRESENT`, but the underlying
`vkCreateImage` returns `INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`
for the layout our probe (and ANGLE) construct from the GBM bo's
plane offsets and pitches.

## Reproducer

Minimal Rust CLI: <https://github.com/nuketownada/egl-dmabuf-probe>
(MIT-licensed, ~700 lines, no Chromium dependency).

```
$ nix run github:nuketownada/egl-dmabuf-probe -- -d /dev/dri/renderD129 --formats NV12
```

Or build manually:

```
git clone https://github.com/nuketownada/egl-dmabuf-probe
cd egl-dmabuf-probe
nix develop --command cargo build --release
./target/release/egl-dmabuf-probe -d /dev/dri/renderD129 --formats NV12 -v
```

The probe allocates an NV12 GBM buffer, attempts `eglCreateImage`
(succeeds) and `vkCreateImage` (fails) on the same dmabuf fd, and
reports both outcomes with full error codes.

## System

- **GPU:** NVIDIA RTX 4000 Ada Generation Laptop GPU
- **Driver:** 595.71.05 (also reproduces on 580.x, 560.x, 555.x —
  see history in [open-gpu-kernel-modules#644](https://github.com/NVIDIA/open-gpu-kernel-modules/issues/644))
- **Kernel module:** `nvidia.ko.xz` v595.71.05, license `Dual MIT/GPL` (open variant)
- **Kernel:** Linux 6.18.29 x86_64
- **Vulkan API:** 1.4.329
- **VBIOS:** 95.04.3C.80.BD
- **Distro:** NixOS 25.11
- Tested on `/dev/dri/renderD129` (NVIDIA render node)

## Probe output (verbatim, NV12 row)

```
EGL Extensions present:
  ✓ EGL_EXT_image_dma_buf_import
  ✓ EGL_EXT_image_dma_buf_import_modifiers
  ✗ EGL_KHR_image_pixmap   (separately needed by Chromium ozone-x11)

Driver-claimed dma-buf support:
  Driver advertises 54 importable formats via eglQueryDmaBufFormatsEXT.
  NV12 modifiers: 0x300000000606010..0x300000000606015 (+ more) [ext-only]

Vulkan probe:
  Device:         NVIDIA RTX 4000 Ada Generation Laptop GPU
  API version:    1.4.329
  Driver version: 0x94d1c140
  Required extensions: all present
    (VK_KHR_external_memory_fd, VK_EXT_external_memory_dma_buf,
     VK_EXT_image_drm_format_modifier, VK_KHR_image_format_list,
     VK_KHR_sampler_ycbcr_conversion)

Format │ Modifier            │ GBM alloc                       │ EGL import │ Vulkan import
───────┼─────────────────────┼─────────────────────────────────┼────────────┼───────────────
NV12   │ INVALID (implicit)  │ ok (mod=0x300000000606014)      │ ok         │ FAIL
                                                                              ↑
                                                                              VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT
                                                                              from vkCreateImage
```

For comparison, ARGB8888 with the same allocator/import sequence
succeeds in all three columns on the same hardware — only NV12 with
the modifier NVIDIA GBM picks exhibits the EGL/Vulkan inconsistency.

## What the probe does, in detail

For the failing NV12 row:

1. Open `/dev/dri/renderD129` (NVIDIA render node) with O_RDWR | O_CLOEXEC.
2. `gbm_create_device` on that fd.
3. `gbm_bo_create(width=256, height=256, format=NV12, flags=RENDERING)`
   without an explicit modifier list (implicit, driver chooses).
   - Returns `ok` with `bo.modifier() == 0x300000000606014`.
   - `bo.plane_count() == 2` (Y plane + UV interleaved plane).
4. Export plane 0 dmabuf fd via `gbm_bo_get_fd_for_plane(0)`.
5. Construct `VkImageCreateInfo`:
   - `imageType: VK_IMAGE_TYPE_2D`
   - `format: VK_FORMAT_G8_B8R8_2PLANE_420_UNORM`
   - `extent: {256, 256, 1}`
   - `mipLevels: 1, arrayLayers: 1, samples: VK_SAMPLE_COUNT_1_BIT`
   - `tiling: VK_IMAGE_TILING_DRM_FORMAT_MODIFIER_EXT`
   - `usage: VK_IMAGE_USAGE_SAMPLED_BIT`
   - `sharingMode: VK_SHARING_MODE_EXCLUSIVE`
   - `pNext` chain:
     - `VkExternalMemoryImageCreateInfo` with
       `handleTypes = VK_EXTERNAL_MEMORY_HANDLE_TYPE_DMA_BUF_BIT_EXT`
     - `VkImageDrmFormatModifierExplicitCreateInfoEXT` with:
       - `drmFormatModifier = 0x300000000606014` (verbatim from GBM)
       - `drmFormatModifierPlaneCount = 2`
       - `pPlaneLayouts = [{offset, rowPitch}_Y, {offset, rowPitch}_UV]`
         where the offsets and pitches come from
         `bo.offset(0/1)` and `bo.stride_for_plane(0/1)`.
6. `vkCreateImage(...)` →
   **`VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`**

The exact same sequence, but going through
`eglCreateImage(EGL_LINUX_DMA_BUF_EXT, ...)` with PLANE0/PLANE1
attribs filled from the same `bo` data, **succeeds**. So both the
modifier and the plane layout are internally consistent on the EGL
side. The Vulkan rejection is therefore over-strict validation, a
modifier/layout encoding mismatch between NVIDIA's GBM and Vulkan,
or an undocumented additional requirement.

## Why this matters

Chromium 148+ ships only ANGLE-on-EGL on Linux. When NVIDIA users
configure ANGLE to use its Vulkan backend (`--use-angle=vulkan`) —
which is increasingly the recommended path for any hardware where
ANGLE-on-GL is broken — Chromium imports video frames via this same
`vkCreateImage` + dma-buf code path. The driver bug means hardware
video decode is structurally unavailable on NVIDIA X11 for
Chromium and any other ANGLE-using app (Electron, CEF, Brave, etc.).
On a 5K monitor that translates to >50% VP9 frame drops at native
panel resolution because the GPU process either crashes or falls
back to software decode.

Existing tracker entries that hit this driver path without
characterizing it cleanly:

- <https://github.com/NVIDIA/open-gpu-kernel-modules/issues/644>
- <https://forums.developer.nvidia.com/t/status-of-linux-dma-buf-support/209404>
- <https://github.com/NixOS/nixpkgs/issues/209101>

## Cross-check: libplacebo works on the same buffer

Confirming the buffer itself is valid — mpv with VAAPI decode +
Vulkan output plays the same content cleanly:

```
mpv --vo=gpu --gpu-api=vulkan --hwdec=vaapi /tmp/bbb-vp9.webm
  → "[vo/gpu/vaapi] using libplacebo dmabuf interop"
  → "Decoder format: 1920x1080 vaapi[nv12] bt.709 ..."
  → smooth playback, no errors
```

Same NV12 frames from NVIDIA's VAAPI (NVDEC under
nvidia-vaapi-driver) imported into a Vulkan VkImage and rendered —
which is exactly the producer/consumer pair Chromium uses. libplacebo
uses `VkImageDrmFormatModifierListCreateInfoEXT` for the import; we
suspect this is why it works while ANGLE's explicit-layout path
fails.

Separately, `mpv --gpu-api=vulkan --hwdec=auto` (which picks
Vulkan-native video decode `vp9-vulkan` via `VK_KHR_video_decode_vp9`)
plays but renders visibly garbled output ("bowl of jello") — likely
a different NVIDIA Vulkan bug, in the video-decode side rather than
the dma-buf interop side. Mentioning for context; not the focus of
this report.

## Asks

In rough order of "most general fix" to "most pragmatic":

1. Make `VkImageDrmFormatModifierExplicitCreateInfoEXT` accept the
   modifier+layout combination NVIDIA's GBM produces for NV12. The
   list-based path already accepts buffers with this layout, and
   `eglCreateImage` accepts them too — so the buffer is verifiably
   valid; the rejection appears to be in this entrypoint's
   validation specifically.
2. Document precisely which `(format, modifier, plane-layout)`
   tuples `VK_EXT_image_drm_format_modifier`'s explicit path accepts
   on NVIDIA, so callers know whether to fall back to the list path.
3. Update NVIDIA's Vulkan implementation notes to recommend the
   list-based path over the explicit path for dma-buf imports, so
   that ANGLE / other clients can be patched to use what works.

(Re-running the probe against an updated driver is one command —
happy to bisect a fix or verify a candidate. The probe's source is
~700 lines of MIT Rust and is structured so adding new format/modifier
test cases is a one-line change.)
