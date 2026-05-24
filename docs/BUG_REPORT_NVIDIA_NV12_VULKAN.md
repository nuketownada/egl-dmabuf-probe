# NVIDIA driver bug: `vkCreateImage` rejects NV12 dma-buf with the modifier NVIDIA's own GBM produced

`VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`, NVIDIA 595.71.05,
both proprietary and open driver branches, X11 and headless.

## Summary

NVIDIA's userspace stack is internally inconsistent for NV12 dma-buf
import:

- NVIDIA's GBM allocates an NV12 buffer with modifier `0x300000000606014`
  (a block-linear 2D variant) when given the implicit modifier.
- NVIDIA's EGL imports that buffer back via
  `eglCreateImage(EGL_LINUX_DMA_BUF_EXT, …)` cleanly — both with the
  modifier passed explicitly and without.
- NVIDIA's Vulkan **rejects** the same buffer at `vkCreateImage` with
  `VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT` from the
  `VK_EXT_image_drm_format_modifier` validation, even though the
  modifier is advertised as supported via the EGL query interface and
  the Vulkan extensions are all present.

This is the actual driver-side cause of "GPU process crashes /
hardware video decode disabled" for Chromium with
`--use-angle=vulkan` on NVIDIA + X11. The Chromium failure surfaces
as `DmaBufImageSiblingVkLinux.cpp:initImpl:616 VK_ERROR_FEATURE_NOT_PRESENT`,
but the underlying problem is that ANGLE's `vkCreateImage` call is
hitting this driver bug.

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

## Asks

Any one of these resolves the user-visible symptom:

1. Make `vkCreateImage` accept the modifier+layout combination that
   NVIDIA's GBM produces — the EGL path proves the buffer is valid.
2. If the modifier+layout combo legitimately is invalid for Vulkan,
   have NVIDIA's GBM not pick it for NV12 in the first place.
3. Document precisely which `(format, modifier, plane layout)`
   tuples are accepted by `VK_EXT_image_drm_format_modifier` so
   userspace can constrain its requests up front.

(Re-running the probe against an updated driver is one command —
happy to bisect a fix or verify.)
