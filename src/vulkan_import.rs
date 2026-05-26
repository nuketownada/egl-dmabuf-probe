//! Vulkan-side dma-buf import probe.
//!
//! Mirrors what ANGLE-on-Vulkan does when Chromium imports a video
//! frame: take a dma-buf fd from another producer (NVDEC, GBM, VAAPI),
//! wrap it as a `VkImage` via `VK_KHR_external_memory_fd` +
//! `VK_EXT_external_memory_dma_buf` + `VK_EXT_image_drm_format_modifier`.
//!
//! The exact code path Chromium hit in the
//! `DmaBufImageSiblingVkLinux.cpp:initImpl:616` error you saw earlier.

use std::ffi::CStr;
use std::os::fd::IntoRawFd;

use ash::vk;

use crate::probe::FormatSpec;

pub struct VulkanProbe {
    _entry: ash::Entry,
    instance: ash::Instance,
    pub device_name: String,
    pub api_version: String,
    pub driver_version: String,
    pub missing_extensions: Vec<String>,
    /// None if any required device extension was missing; we still
    /// keep the probe to report what we found, but can't import.
    state: Option<ImportState>,
}

struct ImportState {
    device: ash::Device,
    external_memory_fd: ash::khr::external_memory_fd::Device,
}

#[derive(Debug)]
pub enum VulkanImportResult {
    Ok,
    Failed(String),
    /// Skipped because the DRM format has no Vulkan equivalent, or the
    /// device doesn't expose the required extensions.
    Skipped(String),
}

/// Which of the two `VK_EXT_image_drm_format_modifier` import patterns
/// to exercise.
#[derive(Clone, Copy, Debug)]
enum Strategy {
    /// `VkImageDrmFormatModifierExplicitCreateInfoEXT` — caller
    /// specifies the modifier AND per-plane offsets/pitches. Used by
    /// ANGLE's `DmaBufImageSiblingVkLinux`.
    Explicit,
    /// `VkImageDrmFormatModifierListCreateInfoEXT` — caller passes a
    /// list of candidate modifiers, driver picks one and computes the
    /// layout. Used by libplacebo / mpv / Wayland compositors.
    List,
}

impl Strategy {
    fn label(&self) -> &'static str {
        match self {
            Strategy::Explicit => "explicit",
            Strategy::List => "list",
        }
    }
}

/// How elaborate the `VkImageCreateInfo` should be — minimal vs the
/// full chain Chromium / ANGLE constructs.
#[derive(Clone, Copy, Debug)]
pub enum CreateProfile {
    /// Minimal: SAMPLED usage, no flags, no pNext other than ext mem
    /// + modifier. What a textbook dma-buf importer does.
    Simple,
    /// Chromium-like: SAMPLED + TRANSFER_SRC/DST usage, MUTABLE_FORMAT
    /// + EXTENDED_USAGE flags, and a VkImageFormatListCreateInfoKHR in
    /// the pNext chain listing view-compatible formats. Matches what
    /// ANGLE's DmaBufImageSiblingVkLinux::initWithFormat builds.
    ChromiumLike,
}

impl CreateProfile {
    fn label(&self) -> &'static str {
        match self {
            CreateProfile::Simple => "simple",
            CreateProfile::ChromiumLike => "chromium-like",
        }
    }
}

const REQUIRED_DEVICE_EXTS: &[&CStr] = &[
    ash::khr::external_memory_fd::NAME,
    ash::ext::external_memory_dma_buf::NAME,
    ash::ext::image_drm_format_modifier::NAME,
    ash::khr::image_format_list::NAME,
    ash::khr::sampler_ycbcr_conversion::NAME, // needed for YUV image creation
];

impl VulkanProbe {
    pub fn new() -> Result<Self, String> {
        let entry = unsafe { ash::Entry::load() }
            .map_err(|e| format!("load libvulkan: {e}"))?;

        let app_name = c"egl-dmabuf-probe";
        let app_info = vk::ApplicationInfo::default()
            .application_name(app_name)
            .application_version(0)
            .engine_name(app_name)
            .engine_version(0)
            .api_version(vk::API_VERSION_1_2);

        let instance_exts = [
            ash::khr::get_physical_device_properties2::NAME.as_ptr(),
            ash::khr::external_memory_capabilities::NAME.as_ptr(),
        ];
        let create_info = vk::InstanceCreateInfo::default()
            .application_info(&app_info)
            .enabled_extension_names(&instance_exts);

        let instance = unsafe { entry.create_instance(&create_info, None) }
            .map_err(|e| format!("vkCreateInstance: {e:?}"))?;

        let phys_devices = unsafe { instance.enumerate_physical_devices() }
            .map_err(|e| format!("enumerate_physical_devices: {e:?}"))?;
        if phys_devices.is_empty() {
            unsafe { instance.destroy_instance(None) };
            return Err("no Vulkan physical devices".to_string());
        }

        // Pick the first device with a graphics queue. Future: match
        // VK_EXT_physical_device_drm against the DRM render node major/minor.
        let (physical_device, queue_family) = pick_device(&instance, &phys_devices)
            .map_err(|e| {
                unsafe { instance.destroy_instance(None) };
                e
            })?;

        let props = unsafe { instance.get_physical_device_properties(physical_device) };
        let device_name = cstr_from_chars(&props.device_name)
            .to_string_lossy()
            .into_owned();
        let api_version = format!(
            "{}.{}.{}",
            vk::api_version_major(props.api_version),
            vk::api_version_minor(props.api_version),
            vk::api_version_patch(props.api_version)
        );
        // The vendor driver version field has vendor-specific encoding;
        // we just print the raw u32.
        let driver_version = format!("0x{:x}", props.driver_version);

        // Check required device extensions.
        let available = unsafe {
            instance.enumerate_device_extension_properties(physical_device)
        }
        .map_err(|e| format!("enumerate_device_extension_properties: {e:?}"))?;

        let mut missing_extensions = Vec::new();
        for req in REQUIRED_DEVICE_EXTS {
            let req_s = req.to_string_lossy();
            let present = available.iter().any(|p| {
                cstr_from_chars(&p.extension_name).to_string_lossy() == req_s
            });
            if !present {
                missing_extensions.push(req_s.into_owned());
            }
        }

        let state = if missing_extensions.is_empty() {
            // Enable extensions and create the logical device.
            let prio = [1.0f32];
            let queue_ci = vk::DeviceQueueCreateInfo::default()
                .queue_family_index(queue_family)
                .queue_priorities(&prio);
            let queue_cis = [queue_ci];
            let ext_ptrs: Vec<*const i8> =
                REQUIRED_DEVICE_EXTS.iter().map(|n| n.as_ptr()).collect();

            // YUV image creation needs sampler-ycbcr-conversion feature.
            let mut ycbcr_features =
                vk::PhysicalDeviceSamplerYcbcrConversionFeatures::default()
                    .sampler_ycbcr_conversion(true);

            let device_ci = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queue_cis)
                .enabled_extension_names(&ext_ptrs)
                .push_next(&mut ycbcr_features);

            match unsafe { instance.create_device(physical_device, &device_ci, None) } {
                Ok(device) => {
                    let external_memory_fd =
                        ash::khr::external_memory_fd::Device::new(&instance, &device);
                    Some(ImportState { device, external_memory_fd })
                }
                Err(e) => {
                    eprintln!(
                        "warning: vkCreateDevice failed ({:?}); Vulkan probe will report extension-only",
                        e
                    );
                    None
                }
            }
        } else {
            None
        };

        Ok(VulkanProbe {
            _entry: entry,
            instance,
            device_name,
            api_version,
            driver_version,
            missing_extensions,
            state,
        })
    }

    /// Import via the *explicit-layout* path: caller specifies the
    /// modifier AND the plane offsets/pitches in
    /// `VkImageDrmFormatModifierExplicitCreateInfoEXT`. This is the
    /// path ANGLE's `DmaBufImageSiblingVkLinux` uses.
    pub fn try_import_explicit<U>(
        &self,
        bo: &gbm::BufferObject<U>,
        format: &FormatSpec,
        width: u32,
        height: u32,
        verbose: bool,
    ) -> VulkanImportResult {
        self.try_import_inner(
            bo, format, width, height, Strategy::Explicit, CreateProfile::Simple, verbose,
        )
    }

    /// Import via the *list* path: caller specifies just a list of
    /// candidate modifiers in `VkImageDrmFormatModifierListCreateInfoEXT`
    /// and lets the driver figure out the layout. This is the path
    /// libplacebo (mpv, Wayland compositors) uses.
    pub fn try_import_list<U>(
        &self,
        bo: &gbm::BufferObject<U>,
        format: &FormatSpec,
        width: u32,
        height: u32,
        verbose: bool,
    ) -> VulkanImportResult {
        self.try_import_inner(
            bo, format, width, height, Strategy::List, CreateProfile::Simple, verbose,
        )
    }

    /// Like `try_import_explicit` / `try_import_list` but with the
    /// fuller `VkImageCreateInfo` chain Chromium/ANGLE constructs:
    /// MUTABLE_FORMAT + EXTENDED_USAGE flags, multi-usage bits, and
    /// `VkImageFormatListCreateInfoKHR` listing view-compatible
    /// formats.
    pub fn try_import_chromium_like<U>(
        &self,
        bo: &gbm::BufferObject<U>,
        format: &FormatSpec,
        width: u32,
        height: u32,
        strategy_label: &str,
        verbose: bool,
    ) -> VulkanImportResult {
        let strategy = if strategy_label == "list" {
            Strategy::List
        } else {
            Strategy::Explicit
        };
        self.try_import_inner(
            bo, format, width, height, strategy, CreateProfile::ChromiumLike, verbose,
        )
    }

    fn try_import_inner<U>(
        &self,
        bo: &gbm::BufferObject<U>,
        format: &FormatSpec,
        width: u32,
        height: u32,
        strategy: Strategy,
        profile: CreateProfile,
        verbose: bool,
    ) -> VulkanImportResult {
        let state = match &self.state {
            Some(s) => s,
            None => {
                return VulkanImportResult::Skipped(format!(
                    "missing device extensions: {}",
                    self.missing_extensions.join(", ")
                ));
            }
        };

        let vk_format = match drm_to_vk_format(format.fourcc) {
            Some(f) => f,
            None => {
                return VulkanImportResult::Skipped(format!(
                    "no Vulkan format for {:?}",
                    format.fourcc
                ));
            }
        };

        let plane_count = bo.plane_count();
        let modifier: u64 = u64::from(bo.modifier());

        // Per-plane subresource layouts (offsets / row pitches within the dmabuf).
        // Only used by the Explicit strategy; in the List strategy the driver
        // figures them out.
        let mut plane_layouts: Vec<vk::SubresourceLayout> =
            Vec::with_capacity(plane_count as usize);
        for i in 0..plane_count {
            plane_layouts.push(vk::SubresourceLayout {
                offset: bo.offset(i as i32) as u64,
                size: 0, // spec: size is ignored for DRM-modifier images
                row_pitch: bo.stride_for_plane(i as i32) as u64,
                array_pitch: 0,
                depth_pitch: 0,
            });
        }

        let mut ext_mem_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);

        let extent = vk::Extent3D {
            width,
            height,
            depth: 1,
        };
        // Build the modifier struct for the chosen strategy. Both
        // strategies are kept alive for the duration of the image
        // creation call; only one is wired into the pNext chain.
        let mut explicit_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&plane_layouts);
        let modifier_list = [modifier];
        let mut list_info = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
            .drm_format_modifiers(&modifier_list);

        // Adjust usage / flags / pNext chain based on profile.
        let (usage_flags, create_flags) = match profile {
            CreateProfile::Simple => (vk::ImageUsageFlags::SAMPLED, vk::ImageCreateFlags::empty()),
            CreateProfile::ChromiumLike => (
                vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST,
                vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE,
            ),
        };

        // For the chromium-like profile, declare a list of formats the
        // image may be viewed as. For NV12 the per-plane views need R8
        // (Y) and R8G8 (UV); for RGB formats the storage compatibility
        // pair (sRGB ↔ UNORM) is what chromium typically lists.
        let view_formats: Vec<vk::Format> = if matches!(profile, CreateProfile::ChromiumLike) {
            chromium_like_view_formats(vk_format)
        } else {
            Vec::new()
        };
        let mut format_list_info =
            vk::ImageFormatListCreateInfo::default().view_formats(&view_formats);

        let base_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(extent)
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage_flags)
            .flags(create_flags)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED);

        // Build the pNext chain. Order: external memory → modifier
        // (explicit OR list) → optional format list.
        let mut image_ci = base_ci.push_next(&mut ext_mem_info);
        image_ci = match strategy {
            Strategy::Explicit => image_ci.push_next(&mut explicit_info),
            Strategy::List => image_ci.push_next(&mut list_info),
        };
        if matches!(profile, CreateProfile::ChromiumLike) && !view_formats.is_empty() {
            image_ci = image_ci.push_next(&mut format_list_info);
        }

        let image = match unsafe { state.device.create_image(&image_ci, None) } {
            Ok(img) => img,
            Err(e) => {
                if verbose {
                    eprintln!(
                        "vk_import[{}/{}] {} mod=0x{:x} planes={} vkCreateImage: {:?}",
                        profile.label(), strategy.label(), format.name, modifier, plane_count, e
                    );
                }
                return VulkanImportResult::Failed(format!("vkCreateImage: {:?}", e));
            }
        };

        // GBM may give us one dmabuf shared across planes (typical for NV12)
        // or one per plane. For the typical shared case we hand a single fd
        // to Vulkan and the plane offsets are inside the same buffer.
        // Plane > 0 fd is only needed for the disjoint case which we don't
        // attempt yet.
        let fd = match bo.fd_for_plane(0) {
            Ok(fd) => fd.into_raw_fd(),
            Err(e) => {
                unsafe { state.device.destroy_image(image, None) };
                return VulkanImportResult::Failed(format!(
                    "gbm_bo_get_fd(0): {e}"
                ));
            }
        };

        // Image memory requirements.
        let img_mem_req_info =
            vk::ImageMemoryRequirementsInfo2::default().image(image);
        let mut mem_reqs = vk::MemoryRequirements2::default();
        unsafe {
            state
                .device
                .get_image_memory_requirements2(&img_mem_req_info, &mut mem_reqs)
        };

        // What memory types can this fd back?
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        if let Err(e) = unsafe {
            state.external_memory_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd,
                &mut fd_props,
            )
        } {
            unsafe {
                libc::close(fd);
                state.device.destroy_image(image, None);
            }
            return VulkanImportResult::Failed(format!(
                "vkGetMemoryFdPropertiesKHR: {:?}",
                e
            ));
        }

        let usable = mem_reqs.memory_requirements.memory_type_bits & fd_props.memory_type_bits;
        if usable == 0 {
            unsafe {
                libc::close(fd);
                state.device.destroy_image(image, None);
            }
            return VulkanImportResult::Failed(format!(
                "no usable memory type (image needs 0x{:x}, fd supports 0x{:x})",
                mem_reqs.memory_requirements.memory_type_bits, fd_props.memory_type_bits
            ));
        }
        let memory_type_index = usable.trailing_zeros();

        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(fd);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(mem_reqs.memory_requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut import_info)
            .push_next(&mut dedicated);

        let memory = match unsafe { state.device.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(e) => {
                // vkAllocateMemory consumes the fd only on SUCCESS; close it on failure.
                unsafe {
                    libc::close(fd);
                    state.device.destroy_image(image, None);
                }
                if verbose {
                    eprintln!(
                        "vk_import[{}] {} vkAllocateMemory: {:?}",
                        strategy.label(), format.name, e
                    );
                }
                return VulkanImportResult::Failed(format!("vkAllocateMemory: {:?}", e));
            }
        };
        // Successful import: Vulkan now owns the fd; do not close.

        let bind_info = vk::BindImageMemoryInfo::default()
            .image(image)
            .memory(memory)
            .memory_offset(0);
        let bind_result = unsafe {
            state
                .device
                .bind_image_memory2(std::slice::from_ref(&bind_info))
        };
        let result = match bind_result {
            Ok(()) => VulkanImportResult::Ok,
            Err(e) => VulkanImportResult::Failed(format!("vkBindImageMemory2: {:?}", e)),
        };

        unsafe {
            state.device.free_memory(memory, None);
            state.device.destroy_image(image, None);
        }
        result
    }
}

impl Drop for VulkanProbe {
    fn drop(&mut self) {
        unsafe {
            if let Some(state) = self.state.take() {
                state.device.destroy_device(None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

fn pick_device(
    instance: &ash::Instance,
    devices: &[vk::PhysicalDevice],
) -> Result<(vk::PhysicalDevice, u32), String> {
    for &pd in devices {
        let qfp = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        for (i, p) in qfp.iter().enumerate() {
            if p.queue_flags.contains(vk::QueueFlags::GRAPHICS) {
                return Ok((pd, i as u32));
            }
        }
    }
    Err("no Vulkan device with a graphics queue".to_string())
}

/// View-compatible formats Chromium's image-create chain advertises
/// for the chromium-like profile. For multi-planar YUV formats it
/// includes the per-plane views. Returning empty means the format
/// list shouldn't be added (the spec disallows an empty list).
fn chromium_like_view_formats(format: vk::Format) -> Vec<vk::Format> {
    use vk::Format;
    match format {
        // NV12: 2 planes — Y (R8) + UV (R8G8)
        Format::G8_B8R8_2PLANE_420_UNORM => {
            vec![Format::G8_B8R8_2PLANE_420_UNORM, Format::R8_UNORM, Format::R8G8_UNORM]
        }
        // P010: 2 planes — Y (R10X6) + UV (R10X6G10X6)
        Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16 => {
            vec![
                Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
                Format::R10X6_UNORM_PACK16,
                Format::R10X6G10X6_UNORM_2PACK16,
            ]
        }
        // RGB 8-bit: storage-compatible pair (chromium often wants
        // sampled-as-UNORM + UAV-as-UINT for raster).
        Format::B8G8R8A8_UNORM => vec![Format::B8G8R8A8_UNORM, Format::B8G8R8A8_SRGB],
        Format::R8G8B8A8_UNORM => vec![Format::R8G8B8A8_UNORM, Format::R8G8B8A8_SRGB],
        // Everything else: no extra views — chromium would still
        // declare the format itself in the list to enable MUTABLE.
        f => vec![f],
    }
}

fn drm_to_vk_format(fourcc: drm_fourcc::DrmFourcc) -> Option<vk::Format> {
    use drm_fourcc::DrmFourcc::*;
    Some(match fourcc {
        Argb8888 | Xrgb8888 => vk::Format::B8G8R8A8_UNORM,
        Abgr8888 | Xbgr8888 => vk::Format::R8G8B8A8_UNORM,
        Rgb565 => vk::Format::R5G6B5_UNORM_PACK16,
        Nv12 => vk::Format::G8_B8R8_2PLANE_420_UNORM,
        P010 => vk::Format::G10X6_B10X6R10X6_2PLANE_420_UNORM_3PACK16,
        Yuyv => vk::Format::G8B8G8R8_422_UNORM,
        _ => return None,
    })
}

/// Convert a Vulkan-style fixed-size i8 char array (e.g. PhysicalDeviceProperties.device_name)
/// to a CStr without copying.
fn cstr_from_chars(chars: &[i8]) -> &CStr {
    // Find the first NUL.
    let bytes: &[u8] = unsafe { std::slice::from_raw_parts(chars.as_ptr() as *const u8, chars.len()) };
    let nul = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len() - 1);
    // SAFETY: bytes[..=nul] ends in NUL (or we forced it by indexing the last byte).
    unsafe { CStr::from_bytes_with_nul_unchecked(&bytes[..=nul]) }
}
