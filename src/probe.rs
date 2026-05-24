//! Core probe logic: open GBM on a render node, init EGL on the GBM
//! platform, walk a matrix of (format, modifier) combinations,
//! recording allocation and import outcomes for each.

use std::ffi::c_void;
use std::fs::OpenOptions;
use std::os::fd::{IntoRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use drm_fourcc::DrmFourcc;
use gbm::{AsRaw, BufferObjectFlags, Device as GbmDevice, Format as GbmFormat, Modifier};

use crate::egl_ffi::*;

pub struct Probe {
    pub device_path: String,
    pub driver_name: String,
    pub gbm: GbmDevice<OwnedFd>,
    pub egl: Egl,
    pub display: EGLDisplay,
    pub client_extensions: String,
    pub display_extensions: String,
}

impl Probe {
    pub fn new(device: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let device_path = device.display().to_string();

        let fd: OwnedFd = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(device)?
            .into();

        let driver_name = read_driver_name(device).unwrap_or_else(|| "unknown".to_string());

        let gbm = GbmDevice::new(fd)?;

        let egl = Egl::load().map_err(|e| format!("EGL load: {e}"))?;

        // Before any display exists, EGL_EXTENSIONS on the null display
        // gives us the *client* extension list.
        let client_extensions = egl
            .query_str(EGL_DEFAULT_DISPLAY, EGL_EXTENSIONS)
            .unwrap_or_default();

        // Get a display tied to the GBM device.
        let display = unsafe {
            (egl.get_platform_display)(
                EGL_PLATFORM_GBM_KHR,
                gbm.as_raw() as *mut c_void,
                std::ptr::null(),
            )
        };
        if display.is_null() {
            return Err(format!(
                "eglGetPlatformDisplayEXT returned NULL ({})",
                egl_error_str(egl.last_error())
            )
            .into());
        }

        let mut major: EGLint = 0;
        let mut minor: EGLint = 0;
        let ok = unsafe { (egl.initialize)(display, &mut major, &mut minor) };
        if ok != EGL_TRUE {
            return Err(format!(
                "eglInitialize failed ({})",
                egl_error_str(egl.last_error())
            )
            .into());
        }

        let display_extensions = egl.query_str(display, EGL_EXTENSIONS).unwrap_or_default();

        Ok(Self {
            device_path,
            driver_name,
            gbm,
            egl,
            display,
            client_extensions,
            display_extensions,
        })
    }

    pub fn run_matrix(
        &self,
        formats: &[FormatSpec],
        modifiers: &[ModifierSpec],
        verbose: bool,
    ) -> Vec<MatrixCell> {
        let mut results = Vec::with_capacity(formats.len() * modifiers.len());
        for f in formats {
            for m in modifiers {
                results.push(self.probe_one(f, m, verbose));
            }
        }
        results
    }

    fn probe_one(&self, f: &FormatSpec, m: &ModifierSpec, verbose: bool) -> MatrixCell {
        const W: u32 = 256;
        const H: u32 = 256;

        let gbm_format = match GbmFormat::try_from(f.fourcc as u32) {
            Ok(f) => f,
            Err(_) => {
                return MatrixCell {
                    format: f.clone(),
                    modifier: m.clone(),
                    alloc: AllocResult::Unsupported,
                    import_as_requested: ImportResult::Skipped,
                    import_with_actual_modifier: None,
                };
            }
        };

        let alloc_result = if m.value == DRM_FORMAT_MOD_INVALID {
            self.gbm
                .create_buffer_object::<()>(W, H, gbm_format, BufferObjectFlags::RENDERING)
        } else {
            self.gbm.create_buffer_object_with_modifiers2::<()>(
                W,
                H,
                gbm_format,
                std::iter::once(Modifier::from(m.value)),
                BufferObjectFlags::RENDERING,
            )
        };

        let bo = match alloc_result {
            Ok(bo) => bo,
            Err(e) => {
                if verbose {
                    eprintln!("alloc {} / {} failed: {}", f.name, m.name, e);
                }
                return MatrixCell {
                    format: f.clone(),
                    modifier: m.clone(),
                    alloc: AllocResult::Failed(e.to_string()),
                    import_as_requested: ImportResult::Skipped,
                    import_with_actual_modifier: None,
                };
            }
        };

        let actual_modifier: u64 = u64::from(bo.modifier());
        let stride: u32 = bo.stride();
        let offset: u32 = bo.offset(0);

        // First import attempt: use whatever modifier the caller
        // requested (none for INVALID, explicit for others).
        let modifier_for_first_attempt = if m.value == DRM_FORMAT_MOD_INVALID {
            None
        } else {
            Some(m.value)
        };
        let import_as_requested =
            self.try_import(&bo, f, modifier_for_first_attempt, W, H, stride, offset, verbose);

        // Second import attempt: only for INVALID-modifier rows where
        // the driver returned a concrete non-INVALID modifier. Retry
        // with that modifier passed explicitly to EGL. Tells us
        // whether NVIDIA's EGL requires the modifier to be specified
        // even though it picked it itself.
        let import_with_actual_modifier = if m.value == DRM_FORMAT_MOD_INVALID
            && actual_modifier != DRM_FORMAT_MOD_INVALID
            && actual_modifier != DRM_FORMAT_MOD_LINEAR
        {
            Some(self.try_import(
                &bo,
                f,
                Some(actual_modifier),
                W,
                H,
                stride,
                offset,
                verbose,
            ))
        } else {
            None
        };

        MatrixCell {
            format: f.clone(),
            modifier: m.clone(),
            alloc: AllocResult::Ok { actual_modifier },
            import_as_requested,
            import_with_actual_modifier,
        }
    }

    /// Attempt one `eglCreateImage(EGL_LINUX_DMA_BUF_EXT, ...)`. The
    /// dmabuf fd is exported fresh from the bo each call (EGL dups it
    /// on success, but explicit closing keeps the accounting clean).
    fn try_import<U>(
        &self,
        bo: &gbm::BufferObject<U>,
        format: &FormatSpec,
        modifier: Option<u64>,
        width: u32,
        height: u32,
        stride: u32,
        offset: u32,
        verbose: bool,
    ) -> ImportResult {
        let fd = match bo.fd_for_plane(0) {
            Ok(fd) => fd,
            Err(e) => return ImportResult::Failed(format!("gbm_bo_get_fd: {e}")),
        };
        let fd_raw = fd.into_raw_fd();

        let mut attribs: Vec<EGLint> = vec![
            EGL_WIDTH,
            width as EGLint,
            EGL_HEIGHT,
            height as EGLint,
            EGL_LINUX_DRM_FOURCC_EXT,
            format.fourcc as EGLint,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            fd_raw,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            offset as EGLint,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            stride as EGLint,
        ];
        if let Some(mod_val) = modifier {
            attribs.push(EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT);
            attribs.push((mod_val & 0xFFFF_FFFF) as EGLint);
            attribs.push(EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT);
            attribs.push(((mod_val >> 32) & 0xFFFF_FFFF) as EGLint);
        }
        attribs.push(EGL_NONE);

        let image = unsafe {
            (self.egl.create_image_khr)(
                self.display,
                std::ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attribs.as_ptr(),
            )
        };
        unsafe { libc::close(fd_raw) };

        if image.is_null() {
            let code = self.egl.last_error();
            if verbose {
                let mod_repr = modifier
                    .map(|m| format!("0x{:x}", m))
                    .unwrap_or_else(|| "(no modifier)".to_string());
                eprintln!(
                    "import {} {} failed: {} ({})",
                    format.name,
                    mod_repr,
                    code,
                    egl_error_str(code)
                );
            }
            ImportResult::Failed(format!("{} ({})", code, egl_error_str(code)))
        } else {
            unsafe { (self.egl.destroy_image_khr)(self.display, image) };
            ImportResult::Ok
        }
    }
}

// ── Format & modifier specifications ────────────────────────────────────

#[derive(Clone, Debug)]
pub struct FormatSpec {
    pub name: &'static str,
    pub fourcc: DrmFourcc,
}

#[derive(Clone, Debug)]
pub struct ModifierSpec {
    pub name: &'static str,
    pub value: u64,
}

pub const DRM_FORMAT_MOD_INVALID: u64 = (1u64 << 56) - 1;
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;
// Intel modifiers (vendor = 0x01)
pub const I915_FORMAT_MOD_X_TILED: u64 = (0x01u64 << 56) | 1;
pub const I915_FORMAT_MOD_Y_TILED: u64 = (0x01u64 << 56) | 2;
pub const I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS: u64 = (0x01u64 << 56) | 6;
// NVIDIA modifier (vendor = 0x03), block-linear 2D, one common GOB layout.
pub const NV_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D_16B2_GOB: u64 =
    (0x03u64 << 56) | 0x10000000000fb;

const DEFAULT_FORMATS: &[FormatSpec] = &[
    FormatSpec { name: "ARGB8888", fourcc: DrmFourcc::Argb8888 },
    FormatSpec { name: "XRGB8888", fourcc: DrmFourcc::Xrgb8888 },
    FormatSpec { name: "ABGR8888", fourcc: DrmFourcc::Abgr8888 },
    FormatSpec { name: "XBGR8888", fourcc: DrmFourcc::Xbgr8888 },
    FormatSpec { name: "RGB565",   fourcc: DrmFourcc::Rgb565 },
    FormatSpec { name: "NV12",     fourcc: DrmFourcc::Nv12 },
    FormatSpec { name: "P010",     fourcc: DrmFourcc::P010 },
    FormatSpec { name: "YUYV",     fourcc: DrmFourcc::Yuyv },
];

const DEFAULT_MODIFIERS: &[ModifierSpec] = &[
    ModifierSpec { name: "INVALID (implicit)", value: DRM_FORMAT_MOD_INVALID },
    ModifierSpec { name: "LINEAR",             value: DRM_FORMAT_MOD_LINEAR },
    ModifierSpec { name: "I915_X_TILED",       value: I915_FORMAT_MOD_X_TILED },
    ModifierSpec { name: "I915_Y_TILED",       value: I915_FORMAT_MOD_Y_TILED },
    ModifierSpec { name: "I915_Y_TILED_GEN12_RC_CCS", value: I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS },
    ModifierSpec { name: "NVIDIA_BLOCK_LINEAR_2D_16Bx2", value: NV_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D_16B2_GOB },
];

pub fn formats_to_test(filter: Option<&[String]>) -> Vec<FormatSpec> {
    match filter {
        None => DEFAULT_FORMATS.to_vec(),
        Some(names) => DEFAULT_FORMATS
            .iter()
            .filter(|f| names.iter().any(|n| n.eq_ignore_ascii_case(f.name)))
            .cloned()
            .collect(),
    }
}

pub fn modifiers_to_test(filter: Option<&[String]>) -> Vec<ModifierSpec> {
    match filter {
        None => DEFAULT_MODIFIERS.to_vec(),
        Some(names) => DEFAULT_MODIFIERS
            .iter()
            .filter(|m| {
                names
                    .iter()
                    .any(|n| m.name.to_ascii_lowercase().contains(&n.to_ascii_lowercase()))
            })
            .cloned()
            .collect(),
    }
}

#[derive(Debug)]
pub struct MatrixCell {
    pub format: FormatSpec,
    pub modifier: ModifierSpec,
    pub alloc: AllocResult,
    /// Import attempt using the modifier the caller asked for. For
    /// INVALID-modifier rows that means no modifier was passed in
    /// the EGL attribs; for explicit-modifier rows it means that
    /// modifier was passed.
    pub import_as_requested: ImportResult,
    /// Only populated for INVALID-modifier rows that successfully
    /// allocated with a concrete non-INVALID modifier: a second
    /// import attempt that passes that driver-chosen modifier
    /// explicitly. Lets us see whether NVIDIA's EGL requires the
    /// modifier in the import attribs even when it picked it during
    /// allocation.
    pub import_with_actual_modifier: Option<ImportResult>,
}

#[derive(Debug)]
pub enum AllocResult {
    Ok { actual_modifier: u64 },
    Failed(String),
    /// drm-fourcc doesn't recognize the format; we skip it.
    Unsupported,
}

#[derive(Debug)]
pub enum ImportResult {
    Ok,
    Failed(String),
    /// Allocation failed, so we never attempted import.
    Skipped,
}

/// Look up the kernel driver name behind a DRM render node by walking
/// the sysfs link. e.g. /dev/dri/renderD129 →
/// /sys/dev/char/<major>:<minor>/device/driver → resolves to e.g.
/// /sys/bus/pci/drivers/nvidia.
fn read_driver_name(dev: &Path) -> Option<String> {
    let meta = std::fs::metadata(dev).ok()?;
    use std::os::unix::fs::MetadataExt;
    let rdev = meta.rdev();
    // glibc-compatible major/minor extraction. On Linux,
    // major = (rdev >> 8) & 0xfff,  minor = (rdev & 0xff) | ((rdev >> 12) & 0xfff00).
    let major = ((rdev >> 8) & 0xfff) as u32;
    let minor = ((rdev & 0xff) | ((rdev >> 12) & 0xfff_00)) as u32;
    let path: PathBuf = format!("/sys/dev/char/{major}:{minor}/device/driver").into();
    let target = std::fs::read_link(&path).ok()?;
    target
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
}
