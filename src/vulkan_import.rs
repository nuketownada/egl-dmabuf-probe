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
    physical_device_handle: vk::PhysicalDevice,
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
    queue_family: u32,
    queue: vk::Queue,
}

#[derive(Debug)]
pub enum VulkanImportResult {
    Ok,
    Failed(String),
    /// Skipped because the DRM format has no Vulkan equivalent, or the
    /// device doesn't expose the required extensions.
    Skipped(String),
}

/// Outcome of `vkGetPhysicalDeviceImageFormatProperties2` for a
/// `(format, modifier)` combination. Mirrors chromium's
/// `FindSupportedFlagsForFormat` / `IsFormatSupported` query before
/// it ever attempts `vkCreateImage`.
#[derive(Debug)]
pub enum VulkanQueryResult {
    /// Query returned VK_SUCCESS — driver supports this combination.
    Supported,
    /// Query returned VK_ERROR_FORMAT_NOT_SUPPORTED — driver
    /// affirmatively rejected this combination.
    NotSupported,
    /// Query returned a different error (e.g.
    /// VK_ERROR_FEATURE_NOT_PRESENT). Reports the raw code.
    Error(String),
    /// Skipped because no Vulkan equivalent for the format / device
    /// extensions missing.
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
                    let queue = unsafe { device.get_device_queue(queue_family, 0) };
                    Some(ImportState { device, external_memory_fd, queue_family, queue })
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
            physical_device_handle: physical_device,
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

    /// Replicate Chromium's `IsFormatSupported` /
    /// `FindSupportedFlagsForFormat` query: call
    /// `vkGetPhysicalDeviceImageFormatProperties2` with the same
    /// pNext chain Chromium builds (ImageFormatInfo2 → FormatList →
    /// ExternalImageFormatInfo → DrmFormatModifierInfoEXT). Reports
    /// whether NVIDIA accepts this *capability query* for the
    /// `(format, modifier)` pair — independent of any image
    /// creation. If this returns VK_ERROR_FEATURE_NOT_PRESENT, the
    /// real chromium bug is here, not in vkCreateImage.
    pub fn query_image_format_properties(
        &self,
        format: &FormatSpec,
        modifier: u64,
        chromium_like: bool,
        verbose: bool,
    ) -> VulkanQueryResult {
        let state = match &self.state {
            Some(s) => s,
            None => {
                return VulkanQueryResult::Skipped(format!(
                    "missing device extensions: {}",
                    self.missing_extensions.join(", ")
                ));
            }
        };

        let vk_format = match drm_to_vk_format(format.fourcc) {
            Some(f) => f,
            None => {
                return VulkanQueryResult::Skipped(format!(
                    "no Vulkan format for {:?}",
                    format.fourcc
                ));
            }
        };

        let (usage, flags) = if chromium_like {
            (
                vk::ImageUsageFlags::SAMPLED
                    | vk::ImageUsageFlags::TRANSFER_SRC
                    | vk::ImageUsageFlags::TRANSFER_DST,
                vk::ImageCreateFlags::MUTABLE_FORMAT | vk::ImageCreateFlags::EXTENDED_USAGE,
            )
        } else {
            (vk::ImageUsageFlags::SAMPLED, vk::ImageCreateFlags::empty())
        };

        let view_formats: Vec<vk::Format> = if chromium_like {
            chromium_like_view_formats(vk_format)
        } else {
            Vec::new()
        };
        let mut format_list_info =
            vk::ImageFormatListCreateInfo::default().view_formats(&view_formats);
        let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut modifier_info = vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default()
            .drm_format_modifier(modifier)
            .sharing_mode(vk::SharingMode::EXCLUSIVE);

        // Build the chain: ImageFormatInfo2 → FormatList → ExternalImageFormatInfo → DrmModifierInfo.
        // ash's push_next wires successive nodes by appending; chromium's
        // chain order is ImageFormatInfo2 .pNext → FormatList .pNext →
        // External .pNext → DrmModifier. push_next on the builder
        // produces the same effective order.
        let mut info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(vk_format)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(usage)
            .flags(flags)
            .push_next(&mut external_info)
            .push_next(&mut modifier_info);
        if chromium_like && !view_formats.is_empty() {
            info = info.push_next(&mut format_list_info);
        }

        let mut props = vk::ImageFormatProperties2::default();
        let result = unsafe {
            (self.instance.fp_v1_1().get_physical_device_image_format_properties2)(
                self.physical_device_handle,
                &info as *const _,
                &mut props,
            )
        };
        // Reference state so the unused-let pattern doesn't pin
        // unused symbols; state is only kept alive so the device
        // outlives the instance's destructor ordering.
        let _ = state;

        match result {
            vk::Result::SUCCESS => VulkanQueryResult::Supported,
            vk::Result::ERROR_FORMAT_NOT_SUPPORTED => VulkanQueryResult::NotSupported,
            other => {
                if verbose {
                    eprintln!(
                        "vk_query {} mod=0x{:x} chromium_like={}: {:?}",
                        format.name, modifier, chromium_like, other
                    );
                }
                VulkanQueryResult::Error(format!("{:?}", other))
            }
        }
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

impl VulkanProbe {
    /// Create (but don't bind) an image for `bo` via the explicit or
    /// list path and report what the driver thinks the image looks
    /// like: `vkGetImageMemoryRequirements` size, the modifier it
    /// settled on, and `vkGetImageSubresourceLayout` per memory plane.
    /// Lets the explicit path's idea of the layout be compared with
    /// GBM's and with the list path's.
    pub fn layout_report<U>(
        &self,
        bo: &gbm::BufferObject<U>,
        format: &FormatSpec,
        width: u32,
        height: u32,
        explicit: bool,
    ) -> Result<LayoutReport, String> {
        let state = self.state.as_ref().ok_or_else(|| {
            format!("missing device extensions: {}", self.missing_extensions.join(", "))
        })?;
        let vk_format = drm_to_vk_format(format.fourcc)
            .ok_or_else(|| format!("no Vulkan format for {:?}", format.fourcc))?;

        let plane_count = bo.plane_count();
        let modifier: u64 = u64::from(bo.modifier());
        let plane_layouts: Vec<vk::SubresourceLayout> = (0..plane_count)
            .map(|i| vk::SubresourceLayout {
                offset: bo.offset(i as i32) as u64,
                size: 0,
                row_pitch: bo.stride_for_plane(i as i32) as u64,
                array_pitch: 0,
                depth_pitch: 0,
            })
            .collect();

        let mut ext_mem_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut explicit_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&plane_layouts);
        let modifier_list = [modifier];
        let mut list_info = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
            .drm_format_modifiers(&modifier_list);

        let mut image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk_format)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::SAMPLED)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext_mem_info);
        image_ci = if explicit {
            image_ci.push_next(&mut explicit_info)
        } else {
            image_ci.push_next(&mut list_info)
        };

        let image = unsafe { state.device.create_image(&image_ci, None) }
            .map_err(|e| format!("vkCreateImage: {e:?}"))?;

        let mem_size = unsafe { state.device.get_image_memory_requirements(image) }.size;

        let drm_mod = ash::ext::image_drm_format_modifier::Device::new(&self.instance, &state.device);
        let mut mod_props = vk::ImageDrmFormatModifierPropertiesEXT::default();
        let chosen_modifier = unsafe {
            drm_mod.get_image_drm_format_modifier_properties(image, &mut mod_props)
        }
        .ok()
        .map(|_| mod_props.drm_format_modifier);

        const PLANE_ASPECTS: [vk::ImageAspectFlags; 4] = [
            vk::ImageAspectFlags::MEMORY_PLANE_0_EXT,
            vk::ImageAspectFlags::MEMORY_PLANE_1_EXT,
            vk::ImageAspectFlags::MEMORY_PLANE_2_EXT,
            vk::ImageAspectFlags::MEMORY_PLANE_3_EXT,
        ];
        let planes = PLANE_ASPECTS[..plane_count as usize]
            .iter()
            .map(|&aspect| unsafe {
                state.device.get_image_subresource_layout(
                    image,
                    vk::ImageSubresource { aspect_mask: aspect, mip_level: 0, array_layer: 0 },
                )
            })
            .collect();

        unsafe { state.device.destroy_image(image, None) };
        Ok(LayoutReport { mem_size, chosen_modifier, planes })
    }

    /// Import `bo` as a two-plane image (explicit or list path), bound
    /// to the full dma-buf, write a pattern into both planes with
    /// vkCmdCopyBufferToImage, copy it back out and compare. Mirrors
    /// the reproducer in NVIDIA forum thread 387942. Returns the
    /// fraction of mismatched bytes per plane (Y, CbCr).
    pub fn readback_test<U>(
        &self,
        bo: &gbm::BufferObject<U>,
        width: u32,
        height: u32,
        explicit: bool,
    ) -> Result<[f64; 2], String> {
        let state = self.state.as_ref().ok_or_else(|| {
            format!("missing device extensions: {}", self.missing_extensions.join(", "))
        })?;
        let dev = &state.device;
        if bo.plane_count() != 2 {
            return Err(format!("expected 2 planes, bo has {}", bo.plane_count()));
        }
        let modifier: u64 = u64::from(bo.modifier());
        let plane_layouts: Vec<vk::SubresourceLayout> = (0..2)
            .map(|i| vk::SubresourceLayout {
                offset: bo.offset(i) as u64,
                size: 0,
                row_pitch: bo.stride_for_plane(i) as u64,
                array_pitch: 0,
                depth_pitch: 0,
            })
            .collect();

        let mut ext_mem_info = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
        let mut explicit_info = vk::ImageDrmFormatModifierExplicitCreateInfoEXT::default()
            .drm_format_modifier(modifier)
            .plane_layouts(&plane_layouts);
        let modifier_list = [modifier];
        let mut list_info = vk::ImageDrmFormatModifierListCreateInfoEXT::default()
            .drm_format_modifiers(&modifier_list);
        let mut image_ci = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(vk::Format::G8_B8R8_2PLANE_420_UNORM)
            .extent(vk::Extent3D { width, height, depth: 1 })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
            .usage(vk::ImageUsageFlags::TRANSFER_SRC | vk::ImageUsageFlags::TRANSFER_DST)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut ext_mem_info);
        image_ci = if explicit {
            image_ci.push_next(&mut explicit_info)
        } else {
            image_ci.push_next(&mut list_info)
        };

        // Everything created below is torn down by `Cleanup` on any
        // return path.
        let mut c = Cleanup {
            dev,
            image: vk::Image::null(),
            image_memory: vk::DeviceMemory::null(),
            buffers: Vec::new(),
            pool: vk::CommandPool::null(),
        };
        c.image = unsafe { dev.create_image(&image_ci, None) }
            .map_err(|e| format!("vkCreateImage: {e:?}"))?;

        let fd = bo.fd_for_plane(0).map_err(|e| format!("gbm_bo_get_fd: {e}"))?.into_raw_fd();
        let dmabuf_size = unsafe { libc::lseek(fd, 0, libc::SEEK_END) } as u64;
        let mem_reqs = unsafe { dev.get_image_memory_requirements(c.image) };
        let mut fd_props = vk::MemoryFdPropertiesKHR::default();
        let usable = unsafe {
            state.external_memory_fd.get_memory_fd_properties(
                vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT,
                fd,
                &mut fd_props,
            )
        }
        .map(|_| mem_reqs.memory_type_bits & fd_props.memory_type_bits)
        .unwrap_or(0);
        if usable == 0 {
            unsafe { libc::close(fd) };
            return Err("no usable memory type for the dma-buf".into());
        }
        // Bind the whole dma-buf, as the 387942 reproducer does, so an
        // undersized memreq can't be what truncates the CbCr plane.
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(c.image);
        let mut import_info = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT)
            .fd(fd);
        let alloc_info = vk::MemoryAllocateInfo::default()
            .allocation_size(dmabuf_size.max(mem_reqs.size))
            .memory_type_index(usable.trailing_zeros())
            .push_next(&mut import_info)
            .push_next(&mut dedicated);
        c.image_memory = match unsafe { dev.allocate_memory(&alloc_info, None) } {
            Ok(m) => m,
            Err(e) => {
                unsafe { libc::close(fd) };
                return Err(format!("vkAllocateMemory(import): {e:?}"));
            }
        };
        unsafe { dev.bind_image_memory(c.image, c.image_memory, 0) }
            .map_err(|e| format!("vkBindImageMemory: {e:?}"))?;

        // Tightly packed staging layout: Y (w×h R8), then CbCr
        // (w/2×h/2 R8G8 = w×h/2 bytes).
        let (cw, ch) = (width.div_ceil(2), height.div_ceil(2));
        let y_bytes = (width * height) as u64;
        let c_bytes = (cw * ch * 2) as u64;
        let total = y_bytes + c_bytes;
        let pattern: Vec<u8> = (0..total)
            .map(|i| {
                let i = i as u32;
                (i.wrapping_mul(2654435761) >> 24) as u8 ^ (i / width) as u8
            })
            .collect();

        let (upload, upload_mem) = self.host_buffer(dev, total)?;
        c.buffers.push((upload, upload_mem));
        let (download, download_mem) = self.host_buffer(dev, total)?;
        c.buffers.push((download, download_mem));
        unsafe {
            let p = dev
                .map_memory(upload_mem, 0, total, vk::MemoryMapFlags::empty())
                .map_err(|e| format!("vkMapMemory: {e:?}"))?;
            std::ptr::copy_nonoverlapping(pattern.as_ptr(), p as *mut u8, total as usize);
            dev.unmap_memory(upload_mem);
        }

        let regions = [
            (vk::ImageAspectFlags::PLANE_0, 0, width, height),
            (vk::ImageAspectFlags::PLANE_1, y_bytes, cw, ch),
        ]
        .map(|(aspect, offset, w, h)| {
            vk::BufferImageCopy::default()
                .buffer_offset(offset)
                .image_subresource(vk::ImageSubresourceLayers {
                    aspect_mask: aspect,
                    mip_level: 0,
                    base_array_layer: 0,
                    layer_count: 1,
                })
                .image_extent(vk::Extent3D { width: w, height: h, depth: 1 })
        });

        c.pool = unsafe {
            dev.create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(state.queue_family),
                None,
            )
        }
        .map_err(|e| format!("vkCreateCommandPool: {e:?}"))?;
        let cmd = unsafe {
            dev.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(c.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }
        .map_err(|e| format!("vkAllocateCommandBuffers: {e:?}"))?[0];

        let range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let barrier = |old, new, src, dst| {
            vk::ImageMemoryBarrier::default()
                .old_layout(old)
                .new_layout(new)
                .src_access_mask(src)
                .dst_access_mask(dst)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(c.image)
                .subresource_range(range)
        };
        unsafe {
            dev.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                .map_err(|e| format!("vkBeginCommandBuffer: {e:?}"))?;
            dev.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TOP_OF_PIPE,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    vk::ImageLayout::UNDEFINED,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::AccessFlags::empty(),
                    vk::AccessFlags::TRANSFER_WRITE,
                )],
            );
            dev.cmd_copy_buffer_to_image(
                cmd,
                upload,
                c.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &regions,
            );
            dev.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[barrier(
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    vk::AccessFlags::TRANSFER_WRITE,
                    vk::AccessFlags::TRANSFER_READ,
                )],
            );
            dev.cmd_copy_image_to_buffer(
                cmd,
                c.image,
                vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                download,
                &regions,
            );
            dev.end_command_buffer(cmd)
                .map_err(|e| format!("vkEndCommandBuffer: {e:?}"))?;
            let cmds = [cmd];
            dev.queue_submit(
                state.queue,
                &[vk::SubmitInfo::default().command_buffers(&cmds)],
                vk::Fence::null(),
            )
            .map_err(|e| format!("vkQueueSubmit: {e:?}"))?;
            dev.queue_wait_idle(state.queue)
                .map_err(|e| format!("vkQueueWaitIdle: {e:?}"))?;
        }

        let readback = unsafe {
            let p = dev
                .map_memory(download_mem, 0, total, vk::MemoryMapFlags::empty())
                .map_err(|e| format!("vkMapMemory: {e:?}"))?;
            let v = std::slice::from_raw_parts(p as *const u8, total as usize).to_vec();
            dev.unmap_memory(download_mem);
            v
        };
        let wrong = |r: std::ops::Range<usize>| {
            let n = r.len();
            r.filter(|&i| readback[i] != pattern[i]).count() as f64 / n as f64
        };
        let y = y_bytes as usize;
        Ok([wrong(0..y), wrong(y..total as usize)])
    }

    fn host_buffer(&self, dev: &ash::Device, size: u64) -> Result<(vk::Buffer, vk::DeviceMemory), String> {
        let buffer = unsafe {
            dev.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC | vk::BufferUsageFlags::TRANSFER_DST),
                None,
            )
        }
        .map_err(|e| format!("vkCreateBuffer: {e:?}"))?;
        let reqs = unsafe { dev.get_buffer_memory_requirements(buffer) };
        let props = unsafe {
            self.instance.get_physical_device_memory_properties(self.physical_device_handle)
        };
        let want = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT;
        let Some(type_index) = (0..props.memory_type_count).find(|&i| {
            reqs.memory_type_bits & (1 << i) != 0
                && props.memory_types[i as usize].property_flags.contains(want)
        }) else {
            unsafe { dev.destroy_buffer(buffer, None) };
            return Err("no host-visible memory type".into());
        };
        let memory = match unsafe {
            dev.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(type_index),
                None,
            )
        } {
            Ok(m) => m,
            Err(e) => {
                unsafe { dev.destroy_buffer(buffer, None) };
                return Err(format!("vkAllocateMemory(host): {e:?}"));
            }
        };
        if let Err(e) = unsafe { dev.bind_buffer_memory(buffer, memory, 0) } {
            unsafe {
                dev.free_memory(memory, None);
                dev.destroy_buffer(buffer, None);
            }
            return Err(format!("vkBindBufferMemory: {e:?}"));
        }
        Ok((buffer, memory))
    }
}

/// Destroys whatever `readback_test` managed to create.
struct Cleanup<'a> {
    dev: &'a ash::Device,
    image: vk::Image,
    image_memory: vk::DeviceMemory,
    buffers: Vec<(vk::Buffer, vk::DeviceMemory)>,
    pool: vk::CommandPool,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let dev = self.dev;
        unsafe {
            if self.pool != vk::CommandPool::null() {
                dev.destroy_command_pool(self.pool, None);
            }
            for &(b, m) in &self.buffers {
                dev.destroy_buffer(b, None);
                dev.free_memory(m, None);
            }
            if self.image != vk::Image::null() {
                dev.destroy_image(self.image, None);
            }
            if self.image_memory != vk::DeviceMemory::null() {
                dev.free_memory(self.image_memory, None);
            }
        }
    }
}

pub struct LayoutReport {
    pub mem_size: u64,
    pub chosen_modifier: Option<u64>,
    pub planes: Vec<vk::SubresourceLayout>,
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
