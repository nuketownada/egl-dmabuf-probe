# egl-dmabuf-probe

Diagnostic for **NVIDIA's EGL extension support on Linux**, particularly the
gaps that break Chromium/Electron compositor surface sharing on X11.

## Why this exists

Chromium 148+ removed its native GLX backend; it now ships only
ANGLE-on-EGL. On X11 the compositor wraps X11 pixmaps as EGL images so
Skia can sample them. On NVIDIA, that wrap fails — but in a way that's
poorly documented:

- `EGL_KHR_image_pixmap` is **not exposed** at all on NVIDIA's EGL.
- `EGL_EXT_image_dma_buf_import` *is* advertised, but the actual
  format + modifier combinations the driver accepts are a subset of
  what Chromium / Wayland compositors / video pipelines ask for.
- Failures surface as `EGL_BAD_PARAMETER` or
  `VK_ERROR_FEATURE_NOT_PRESENT`, with no upstream documentation of
  which combinations are supported.

Neither Chromium nor NVIDIA has documented the supported subset.
[NVIDIA bug #644](https://github.com/NVIDIA/open-gpu-kernel-modules/issues/644)
has been open since May 2024 without a fix.

This tool produces an authoritative matrix you can attach to bug
reports: **"these (format, modifier) combinations work on EGL_LINUX_DMA_BUF_EXT
on your driver, these don't."**

## What it tests

1. **EGL extension presence.** Lists everything the driver advertises,
   flags the ones relevant to Chromium / compositor pixmap sharing.
2. **Driver-claimed dma-buf support.** Calls `eglQueryDmaBufFormatsEXT`
   and `eglQueryDmaBufModifiersEXT` to enumerate the formats and
   modifiers the driver *claims* to support. The probe matrix then
   verifies whether those claims hold up under real allocation + import.
3. **DMA-BUF format × modifier matrix.** For each common format
   (ARGB8888, NV12, P010, etc.) and modifier (linear, implicit, vendor),
   tries to allocate a GBM buffer and import it via
   `eglCreateImage(EGL_LINUX_DMA_BUF_EXT, ...)`. **Fills attribs for
   every plane the bo has** — multi-plane formats like NV12/P010 require
   all planes to be described, or EGL returns EGL_BAD_PARAMETER. Reports
   allocation and import outcomes independently.
4. **Modifier-on-import retry.** For INVALID-modifier rows that
   allocate with a concrete modifier, retries the import passing that
   driver-chosen modifier explicitly. Tells you whether the driver
   requires the modifier in import attribs.

## Findings so far (NVIDIA 595 / RTX 4000 Ada, X11)

- `EGL_KHR_image_pixmap` is **not exposed** at all — this is the
  extension Chromium's ozone-x11 backend uses to wrap X11 pixmaps for
  the compositor, and is the actual root cause of "VP9 dropped frames
  on YouTube" on NVIDIA + Chromium 148.
- `EGL_EXT_image_dma_buf_import` **works correctly** for both RGB
  formats and NV12 (multi-plane) when the caller fills all plane
  attribs per the spec. NVIDIA's dma-buf import is not the blocker.
- The driver claims to support P010 and YUYV but `gbm_bo_create`
  refuses to allocate them — so the claim is partly aspirational.
- Implication: a Chromium fix would convert its X11 pixmap path to
  use DRI3 → dma-buf → `EGL_LINUX_DMA_BUF_EXT` rather than rely on
  `EGL_KHR_image_pixmap`. The dma-buf path works on NVIDIA.

## Usage

```bash
egl-dmabuf-probe                              # probe default $DISPLAY EGL display
egl-dmabuf-probe -d /dev/dri/renderD129        # use specific DRM render node for GBM
egl-dmabuf-probe -v                            # verbose: dump all per-attempt EGL errors
egl-dmabuf-probe --format-only NV12,P010       # only test specific formats
```

## Building

With Nix:
```bash
nix build
./result/bin/egl-dmabuf-probe
```

Without:
```bash
meson setup build && meson compile -C build
./build/egl-dmabuf-probe
```

Dependencies: `libegl`, `libgbm`, `libdrm`, `libxcb` / `libX11`.

## License

MIT
