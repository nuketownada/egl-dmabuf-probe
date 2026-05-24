//! Direct dlopen wrapper around libEGL.so.1 — gives us a single
//! consistent loading path for both core EGL entrypoints and the
//! extensions we care about, without dragging in a higher-level crate
//! whose API churns between minor versions.

use std::ffi::{c_void, CStr};
use std::os::raw::c_char;

use libloading::{Library, Symbol};

pub type EGLBoolean = u32;
pub type EGLDisplay = *mut c_void;
pub type EGLImage = *mut c_void;
pub type EGLContext = *mut c_void;
pub type EGLClientBuffer = *mut c_void;
pub type EGLenum = u32;
pub type EGLint = i32;

pub const EGL_TRUE: EGLBoolean = 1;
#[allow(dead_code)]
pub const EGL_FALSE: EGLBoolean = 0;
pub const EGL_NONE: EGLint = 0x3038;

pub const EGL_EXTENSIONS: EGLint = 0x3055;
pub const EGL_DEFAULT_DISPLAY: *mut c_void = std::ptr::null_mut();

// Platform display selection
pub const EGL_PLATFORM_GBM_KHR: EGLenum = 0x31D7;

// Image creation
pub const EGL_LINUX_DMA_BUF_EXT: EGLenum = 0x3270;
pub const EGL_LINUX_DRM_FOURCC_EXT: EGLint = 0x3271;
pub const EGL_WIDTH: EGLint = 0x3057;
pub const EGL_HEIGHT: EGLint = 0x3056;

pub const EGL_DMA_BUF_PLANE0_FD_EXT: EGLint = 0x3272;
pub const EGL_DMA_BUF_PLANE0_OFFSET_EXT: EGLint = 0x3273;
pub const EGL_DMA_BUF_PLANE0_PITCH_EXT: EGLint = 0x3274;
pub const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: EGLint = 0x3443;
pub const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: EGLint = 0x3444;

// Errors
pub const EGL_SUCCESS: EGLint = 0x3000;
pub const EGL_NOT_INITIALIZED: EGLint = 0x3001;
pub const EGL_BAD_ACCESS: EGLint = 0x3002;
pub const EGL_BAD_ALLOC: EGLint = 0x3003;
pub const EGL_BAD_ATTRIBUTE: EGLint = 0x3004;
pub const EGL_BAD_CONFIG: EGLint = 0x3005;
pub const EGL_BAD_CONTEXT: EGLint = 0x3006;
pub const EGL_BAD_CURRENT_SURFACE: EGLint = 0x3007;
pub const EGL_BAD_DISPLAY: EGLint = 0x3008;
pub const EGL_BAD_MATCH: EGLint = 0x3009;
pub const EGL_BAD_NATIVE_PIXMAP: EGLint = 0x300A;
pub const EGL_BAD_NATIVE_WINDOW: EGLint = 0x300B;
pub const EGL_BAD_PARAMETER: EGLint = 0x300C;
pub const EGL_BAD_SURFACE: EGLint = 0x300D;

pub fn egl_error_str(code: EGLint) -> &'static str {
    match code {
        EGL_SUCCESS => "EGL_SUCCESS",
        EGL_NOT_INITIALIZED => "EGL_NOT_INITIALIZED",
        EGL_BAD_ACCESS => "EGL_BAD_ACCESS",
        EGL_BAD_ALLOC => "EGL_BAD_ALLOC",
        EGL_BAD_ATTRIBUTE => "EGL_BAD_ATTRIBUTE",
        EGL_BAD_CONFIG => "EGL_BAD_CONFIG",
        EGL_BAD_CONTEXT => "EGL_BAD_CONTEXT",
        EGL_BAD_CURRENT_SURFACE => "EGL_BAD_CURRENT_SURFACE",
        EGL_BAD_DISPLAY => "EGL_BAD_DISPLAY",
        EGL_BAD_MATCH => "EGL_BAD_MATCH",
        EGL_BAD_NATIVE_PIXMAP => "EGL_BAD_NATIVE_PIXMAP",
        EGL_BAD_NATIVE_WINDOW => "EGL_BAD_NATIVE_WINDOW",
        EGL_BAD_PARAMETER => "EGL_BAD_PARAMETER",
        EGL_BAD_SURFACE => "EGL_BAD_SURFACE",
        _ => "UNKNOWN_EGL_ERROR",
    }
}

// Core EGL function pointer types.
type FnInitialize = unsafe extern "C" fn(EGLDisplay, *mut EGLint, *mut EGLint) -> EGLBoolean;
type FnTerminate = unsafe extern "C" fn(EGLDisplay) -> EGLBoolean;
type FnQueryString = unsafe extern "C" fn(EGLDisplay, EGLint) -> *const c_char;
type FnGetError = unsafe extern "C" fn() -> EGLint;
type FnGetProcAddress = unsafe extern "C" fn(*const c_char) -> *const c_void;

// Extension function pointer types.
type FnGetPlatformDisplayEXT = unsafe extern "C" fn(EGLenum, *mut c_void, *const EGLint) -> EGLDisplay;
type FnQueryDmaBufFormatsEXT =
    unsafe extern "C" fn(EGLDisplay, EGLint, *mut EGLint, *mut EGLint) -> EGLBoolean;
type FnQueryDmaBufModifiersEXT = unsafe extern "C" fn(
    EGLDisplay,
    EGLint,
    EGLint,
    *mut u64,
    *mut EGLBoolean,
    *mut EGLint,
) -> EGLBoolean;
type FnCreateImageKHR =
    unsafe extern "C" fn(EGLDisplay, EGLContext, EGLenum, EGLClientBuffer, *const EGLint) -> EGLImage;
type FnDestroyImageKHR = unsafe extern "C" fn(EGLDisplay, EGLImage) -> EGLBoolean;

pub struct Egl {
    _lib: Library, // keep loaded
    pub initialize: FnInitialize,
    #[allow(dead_code)] // we leak the display, but expose terminate for future cleanup
    pub terminate: FnTerminate,
    pub query_string: FnQueryString,
    pub get_error: FnGetError,
    #[allow(dead_code)] // reserved for future ad-hoc extension loading
    get_proc_address: FnGetProcAddress,

    pub get_platform_display: FnGetPlatformDisplayEXT,
    #[allow(dead_code)] // wired up by a future probe step
    pub query_dma_buf_formats: Option<FnQueryDmaBufFormatsEXT>,
    #[allow(dead_code)] // wired up by a future probe step
    pub query_dma_buf_modifiers: Option<FnQueryDmaBufModifiersEXT>,
    pub create_image_khr: FnCreateImageKHR,
    pub destroy_image_khr: FnDestroyImageKHR,
}

impl Egl {
    pub fn load() -> Result<Self, String> {
        let lib =
            unsafe { Library::new("libEGL.so.1") }.map_err(|e| format!("dlopen libEGL.so.1: {e}"))?;

        unsafe fn sym<T>(lib: &Library, name: &[u8]) -> Result<T, String>
        where
            T: Sized,
        {
            let s: Symbol<T> = lib
                .get(name)
                .map_err(|e| format!("dlsym {}: {e}", std::str::from_utf8(name).unwrap_or("?")))?;
            // Symbol derefs to the FFI fn pointer; copy it out so we
            // don't hold a borrow back into the Library handle.
            Ok(std::ptr::read(&*s as *const T))
        }

        let (initialize, terminate, query_string, get_error, get_proc_address) = unsafe {
            (
                sym::<FnInitialize>(&lib, b"eglInitialize\0")?,
                sym::<FnTerminate>(&lib, b"eglTerminate\0")?,
                sym::<FnQueryString>(&lib, b"eglQueryString\0")?,
                sym::<FnGetError>(&lib, b"eglGetError\0")?,
                sym::<FnGetProcAddress>(&lib, b"eglGetProcAddress\0")?,
            )
        };

        let lookup_ext = |name: &str| -> *const c_void {
            let c = std::ffi::CString::new(name).unwrap();
            unsafe { get_proc_address(c.as_ptr()) }
        };

        let must_have = |name: &str| -> Result<*const c_void, String> {
            let p = lookup_ext(name);
            if p.is_null() {
                Err(format!("{name} not exported by libEGL"))
            } else {
                Ok(p)
            }
        };

        let get_platform_display: FnGetPlatformDisplayEXT =
            unsafe { std::mem::transmute(must_have("eglGetPlatformDisplayEXT")?) };
        let create_image_khr: FnCreateImageKHR =
            unsafe { std::mem::transmute(must_have("eglCreateImageKHR")?) };
        let destroy_image_khr: FnDestroyImageKHR =
            unsafe { std::mem::transmute(must_have("eglDestroyImageKHR")?) };

        let q_formats = lookup_ext("eglQueryDmaBufFormatsEXT");
        let query_dma_buf_formats: Option<FnQueryDmaBufFormatsEXT> = if q_formats.is_null() {
            None
        } else {
            Some(unsafe { std::mem::transmute(q_formats) })
        };
        let q_mods = lookup_ext("eglQueryDmaBufModifiersEXT");
        let query_dma_buf_modifiers: Option<FnQueryDmaBufModifiersEXT> = if q_mods.is_null() {
            None
        } else {
            Some(unsafe { std::mem::transmute(q_mods) })
        };

        Ok(Egl {
            _lib: lib,
            initialize,
            terminate,
            query_string,
            get_error,
            get_proc_address,
            get_platform_display,
            query_dma_buf_formats,
            query_dma_buf_modifiers,
            create_image_khr,
            destroy_image_khr,
        })
    }

    pub fn last_error(&self) -> EGLint {
        unsafe { (self.get_error)() }
    }

    /// Safe wrapper around `eglQueryString` that handles the null pointer
    /// case (extension not supported / not initialized).
    pub fn query_str(&self, display: EGLDisplay, name: EGLint) -> Option<String> {
        let p = unsafe { (self.query_string)(display, name) };
        if p.is_null() {
            None
        } else {
            unsafe { Some(CStr::from_ptr(p).to_string_lossy().into_owned()) }
        }
    }
}
