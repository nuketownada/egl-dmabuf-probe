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
2. **`EGL_KHR_image_pixmap` test.** Allocates an X11 pixmap, attempts
   `eglCreateImage(EGL_NATIVE_PIXMAP_KHR, ...)`, reports the result.
   On NVIDIA this should fail because the extension isn't even
   exposed.
3. **DMA-BUF format × modifier matrix.** For each common format
   (ARGB8888, NV12, P010, etc.) and modifier (linear, implicit, vendor),
   tries to allocate a GBM buffer and import it as an EGL image via
   `eglCreateImage(EGL_LINUX_DMA_BUF_EXT, ...)`. Reports allocation
   success and import success independently — so you can distinguish
   "the driver can't allocate this" from "the driver allocated it but
   can't import it back".

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
