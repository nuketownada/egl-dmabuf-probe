# NVIDIA Vulkan: `VkImageDrmFormatModifierExplicitCreateInfoEXT` rejects NV12 layouts that the list-based path accepts

`VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`, NVIDIA 595.71.05.

**Filed at:** <https://forums.developer.nvidia.com/t/nvidia-vulkan-vkimagedrmformatmodifierexplicitcreateinfoext-rejects-nv12-layouts-that-the-list-based-path-accepts/371199>

## Symptom

Same NV12 dma-buf, allocated by NVIDIA's own GBM, fed into Vulkan via
two different create-info chains:

```
Format │ GBM alloc                       │ EGL import │ VK (explicit) │ VK (list)
───────┼─────────────────────────────────┼────────────┼───────────────┼──────────
NV12   │ ok (mod=0x300000000606014)      │ ok         │ FAIL          │ ok
                                                        ↑
                                          VK_ERROR_INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT
                                          (vkCreateImage rejects)
```

- **`VkImageDrmFormatModifierExplicitCreateInfoEXT`** (caller passes
  modifier + per-plane offsets/pitches): **rejected**. This is the
  path ANGLE's `DmaBufImageSiblingVkLinux.cpp` uses.
- **`VkImageDrmFormatModifierListCreateInfoEXT`** (caller passes a
  list of candidate modifiers, driver picks the layout itself):
  **accepted**. This is the path libplacebo / mpv / Wayland
  compositors use.

So the buffer itself is valid; only the explicit-layout entrypoint's
validation rejects it.

User-visible: Chromium with `--use-angle=vulkan` on NVIDIA X11 falls
back to software video decode, surfacing as
`DmaBufImageSiblingVkLinux.cpp:initImpl:616 VK_ERROR_FEATURE_NOT_PRESENT`
in chrome logs. Underlying `vkCreateImage` returns
`INVALID_DRM_FORMAT_MODIFIER_PLANE_LAYOUT_EXT`. There is no
workaround because the latest stable Chromium has dropped support
for the GLX path.

## Reproducer

Minimal MIT-licensed Rust CLI: <https://github.com/nuketownada/egl-dmabuf-probe>

```sh
nix run github:nuketownada/egl-dmabuf-probe -- -d /dev/dri/renderD129 --formats NV12
```

Or:

```sh
git clone https://github.com/nuketownada/egl-dmabuf-probe
cd egl-dmabuf-probe
nix develop --command cargo build --release
./target/release/egl-dmabuf-probe -d /dev/dri/renderD129 --formats NV12 -v
```

The probe allocates an NV12 GBM bo and attempts import via both
Vulkan strategies on the same dmabuf fd. Source for the two paths
in [src/vulkan_import.rs](https://github.com/nuketownada/egl-dmabuf-probe/blob/master/src/vulkan_import.rs)
(see `try_import_explicit` and `try_import_list`).

## Cross-check: libplacebo (mpv) works on the same buffer

```
mpv --vo=gpu --gpu-api=vulkan --hwdec=vaapi <NV12 VP9 file>
  → "[vo/gpu/vaapi] using libplacebo dmabuf interop"
  → "Decoder format: 1920x1080 vaapi[nv12] bt.709 ..."
  → smooth playback, no errors
```

Same NV12 frames from NVIDIA's VAAPI (NVDEC under
nvidia-vaapi-driver) imported into Vulkan via libplacebo — which is
exactly the producer/consumer pair Chromium uses, just with
libplacebo as the import layer instead of ANGLE. libplacebo's
import call uses `VkImageDrmFormatModifierListCreateInfoEXT`.

(Separately: `mpv --hwdec=auto` picks Vulkan-native video decode
`vp9-vulkan` via `VK_KHR_video_decode_vp9` and renders visibly
garbled output — a different NVIDIA Vulkan bug, in the video-decode
side rather than the dma-buf interop side. Mentioned for context,
not the focus of this report.)

## System

- GPU: NVIDIA RTX 4000 Ada Generation Laptop GPU
- Driver: 595.71.05 (also reproduces on 555.x–580.x — see
  [open-gpu-kernel-modules#644](https://github.com/NVIDIA/open-gpu-kernel-modules/issues/644))
- Kernel module: `nvidia.ko.xz` v595.71.05, license `Dual MIT/GPL` (open variant)
- Userspace: NVIDIA proprietary (`DRIVER_ID_NVIDIA_PROPRIETARY`)
- Vulkan API: 1.4.329
- Kernel: Linux 6.18.29 x86_64
- Tested on `/dev/dri/renderD129`

(`nvidia-bug-report.log.gz` attached.)

## Asks

In rough order of "most general fix" to "most pragmatic":

1. Make `VkImageDrmFormatModifierExplicitCreateInfoEXT` accept the
   modifier+layout combination NVIDIA's GBM produces for NV12 — the
   list-based path already accepts buffers with this layout, so the
   buffer is verifiably valid; the rejection appears to be in this
   entrypoint's validation specifically.
2. Document precisely which `(format, modifier, plane-layout)`
   tuples the explicit path accepts on NVIDIA so callers can know
   whether to fall back to the list path.
3. Update NVIDIA's Vulkan implementation notes to recommend the
   list-based path over the explicit path for dma-buf imports, so
   ANGLE / other clients can be patched to use what works.

The probe takes one flag to re-run against any driver
(`-d /dev/dri/renderDXXX`) and one line to add new format/modifier
test cases — happy to bisect a fix or verify a candidate.
