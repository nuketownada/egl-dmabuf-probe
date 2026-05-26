//! Pretty-printing of probe results.

use owo_colors::OwoColorize;
use tabled::{
    settings::{object::Columns, Modify, Style, Width},
    Table, Tabled,
};

use crate::probe::{AllocResult, ImportResult, MatrixCell, ModifierSpec, FormatSpec, Probe};
use crate::vulkan_import::{VulkanImportResult, VulkanProbe, VulkanQueryResult};

/// Keys we highlight in the EGL extension dumps.
const RELEVANT_EXTENSIONS: &[(&str, &str)] = &[
    ("EGL_KHR_image_pixmap",
     "Wrap X11 pixmaps directly as EGL images (Chromium ozone-x11 wants this)."),
    ("EGL_EXT_image_dma_buf_import",
     "Import dma-bufs as EGL images (the fallback Chromium uses on NVIDIA)."),
    ("EGL_EXT_image_dma_buf_import_modifiers",
     "Specify DRM format modifiers when importing dma-bufs."),
    ("EGL_MESA_image_dma_buf_export",
     "Export EGL images as dma-bufs (Mesa-side interop)."),
    ("EGL_KHR_image",
     "Generic EGLImage creation; prerequisite for the others."),
    ("EGL_KHR_image_base",
     "Foundation extension required by KHR_image variants."),
];

pub fn print_device_info(p: &Probe) {
    println!("{}", "Device".bold().underline());
    println!("  Path:   {}", p.device_path);
    println!("  Driver: {}", p.driver_name.cyan());
}

/// Show what the driver *claims* to support via
/// `eglQueryDmaBufFormatsEXT` / `eglQueryDmaBufModifiersEXT`. The
/// probe matrix then verifies whether the claims hold.
pub fn print_driver_claims(p: &Probe) {
    println!("\n{}", "Driver-claimed dma-buf support".bold().underline());
    let formats = match p.query_supported_formats() {
        Ok(f) => f,
        Err(e) => {
            println!("  {} {}", "✗".red(), format!("eglQueryDmaBufFormatsEXT: {e}").red());
            return;
        }
    };
    if formats.is_empty() {
        println!("  {} driver claims zero supported formats", "✗".red());
        return;
    }
    println!(
        "  Driver advertises {} importable formats via eglQueryDmaBufFormatsEXT.",
        formats.len().to_string().bold()
    );

    // Highlight the ones we care about for browser / video pipelines.
    let interesting = [
        ("ARGB8888", drm_fourcc::DrmFourcc::Argb8888 as i32),
        ("XRGB8888", drm_fourcc::DrmFourcc::Xrgb8888 as i32),
        ("ABGR8888", drm_fourcc::DrmFourcc::Abgr8888 as i32),
        ("XBGR8888", drm_fourcc::DrmFourcc::Xbgr8888 as i32),
        ("NV12",     drm_fourcc::DrmFourcc::Nv12 as i32),
        ("P010",     drm_fourcc::DrmFourcc::P010 as i32),
        ("YUYV",     drm_fourcc::DrmFourcc::Yuyv as i32),
        // YV12 fourcc: 'Y','V','1','2' = 0x32315659
        ("YV12",     0x32315659),
    ];
    println!("  {}", "Browser / video pipeline formats:".dimmed());
    for (name, fourcc) in interesting {
        let present = formats.contains(&fourcc);
        let marker = if present { "✓".green().to_string() } else { "✗".red().to_string() };
        let name_styled = if present { name.green().to_string() } else { name.red().to_string() };
        if present {
            // Query modifiers for this format.
            match p.query_supported_modifiers(fourcc) {
                Ok(mods) if !mods.is_empty() => {
                    let mods_str = mods
                        .iter()
                        .take(6)
                        .map(|(m, ext)| {
                            if *m == 0 {
                                "LINEAR".to_string()
                            } else if *m == (1u64 << 56) - 1 {
                                "INVALID".to_string()
                            } else {
                                let suffix = if *ext { " [ext-only]" } else { "" };
                                format!("0x{:x}{}", m, suffix)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let trailing = if mods.len() > 6 { format!(" (+{} more)", mods.len() - 6) } else { String::new() };
                    println!(
                        "    {} {:10} modifiers: {}{}",
                        marker, name_styled, mods_str.dimmed(), trailing.dimmed()
                    );
                }
                Ok(_) => {
                    println!(
                        "    {} {:10} {}",
                        marker,
                        name_styled,
                        "format advertised but no modifiers".yellow()
                    );
                }
                Err(e) => {
                    println!(
                        "    {} {:10} modifier query failed: {}",
                        marker,
                        name_styled,
                        e.red()
                    );
                }
            }
        } else {
            println!("    {} {} {}", marker, name_styled, "not advertised".dimmed());
        }
    }
}

pub fn print_vulkan_info(vp: &VulkanProbe) {
    println!("\n{}", "Vulkan probe".bold().underline());
    println!("  Device:         {}", vp.device_name.cyan());
    println!("  API version:    {}", vp.api_version);
    println!("  Driver version: {}", vp.driver_version);
    if vp.missing_extensions.is_empty() {
        println!(
            "  Required extensions: {}",
            "all present".green()
        );
    } else {
        println!(
            "  {} Missing required extensions: {}",
            "✗".red(),
            vp.missing_extensions.join(", ").red()
        );
        println!(
            "{}",
            "  → Vulkan import attempts will be marked 'skipped' below.".dimmed()
        );
    }
}

pub fn print_extension_summary(p: &Probe) {
    println!("\n{}", "EGL Extensions".bold().underline());

    let client = p.client_extensions.split_whitespace().collect::<Vec<_>>();
    let display = p.display_extensions.split_whitespace().collect::<Vec<_>>();

    println!("  Client extensions: {}", client.len());
    println!("  Display extensions: {}", display.len());

    println!("\n  {}", "Key extensions relevant to Chromium pixmap/dma-buf import:".dimmed());
    for (name, desc) in RELEVANT_EXTENSIONS {
        let present = client.contains(name) || display.contains(name);
        let marker = if present {
            "✓".green().to_string()
        } else {
            "✗".red().to_string()
        };
        let name_styled = if present {
            name.green().to_string()
        } else {
            name.red().to_string()
        };
        println!("    {} {:48} {}", marker, name_styled, desc.dimmed());
    }
}

#[derive(Tabled)]
struct Row {
    #[tabled(rename = "Format")]
    format: String,
    #[tabled(rename = "Modifier")]
    modifier: String,
    #[tabled(rename = "GBM alloc")]
    alloc: String,
    /// EGL import using the modifier the caller asked for (none for INVALID rows).
    #[tabled(rename = "EGL (as req)")]
    import_req: String,
    /// EGL import using the driver-chosen modifier (only for INVALID rows
    /// that allocated with a concrete modifier).
    #[tabled(rename = "EGL (w/ mod)")]
    import_actual: String,
    /// Vulkan (simple profile) — explicit-modifier path.
    #[tabled(rename = "VK simple\n(exp)")]
    vulkan_explicit: String,
    /// Vulkan (simple profile) — list-modifier path.
    #[tabled(rename = "VK simple\n(list)")]
    vulkan_list: String,
    /// Vulkan (chromium-like profile: MUTABLE_FORMAT + multi-usage +
    /// FormatList) — explicit-modifier path. This is the path that
    /// matches ANGLE's actual vkCreateImage chain.
    #[tabled(rename = "VK chromium\n(exp)")]
    vulkan_chromium_explicit: String,
    /// Vulkan (chromium-like profile) — list-modifier path.
    #[tabled(rename = "VK chromium\n(list)")]
    vulkan_chromium_list: String,
    /// vkGetPhysicalDeviceImageFormatProperties2 result — the *query*
    /// chromium does BEFORE attempting vkCreateImage. Shown as
    /// `simple/chromium` for the two profiles.
    #[tabled(rename = "VK query\n(s/c)")]
    vulkan_query: String,
    #[tabled(rename = "Notes")]
    notes: String,
}

pub fn print_matrix(cells: &[MatrixCell], formats: &[FormatSpec], modifiers: &[ModifierSpec]) {
    let _ = (formats, modifiers); // currently unused; kept for future filtering

    let rows: Vec<Row> = cells
        .iter()
        .map(|c| {
            let alloc = match &c.alloc {
                AllocResult::Ok { actual_modifier } => {
                    if *actual_modifier == c.modifier.value {
                        "ok".green().to_string()
                    } else {
                        format!("ok (mod=0x{:x})", actual_modifier).yellow().to_string()
                    }
                }
                AllocResult::Failed(_) => "fail".red().to_string(),
                AllocResult::Unsupported => "n/a".dimmed().to_string(),
            };
            let import_req = fmt_import(&c.import_as_requested);
            let import_actual = match &c.import_with_actual_modifier {
                None => "—".dimmed().to_string(),
                Some(r) => fmt_import(r),
            };
            let vulkan_explicit = match &c.vulkan_import_explicit {
                None => "—".dimmed().to_string(),
                Some(v) => fmt_vk_import(v),
            };
            let vulkan_list = match &c.vulkan_import_list {
                None => "—".dimmed().to_string(),
                Some(v) => fmt_vk_import(v),
            };
            let vulkan_chromium_explicit = match &c.vulkan_import_chromium_explicit {
                None => "—".dimmed().to_string(),
                Some(v) => fmt_vk_import(v),
            };
            let vulkan_chromium_list = match &c.vulkan_import_chromium_list {
                None => "—".dimmed().to_string(),
                Some(v) => fmt_vk_import(v),
            };
            let vulkan_query = format!(
                "{}/{}",
                fmt_vk_query(&c.vulkan_query_simple),
                fmt_vk_query(&c.vulkan_query_chromium)
            );
            // Notes priority: alloc failure → chromium-like failure
            // (most relevant to the chromium bug) → simple Vulkan
            // failure → EGL failure.
            let notes = match (
                &c.alloc,
                &c.vulkan_import_chromium_list,
                &c.vulkan_import_chromium_explicit,
                &c.vulkan_import_explicit,
                &c.import_as_requested,
            ) {
                (AllocResult::Failed(e), _, _, _, _) => truncate(e, 50),
                (_, Some(VulkanImportResult::Failed(e)), _, _, _) => truncate(e, 50),
                (_, _, Some(VulkanImportResult::Failed(e)), _, _) => truncate(e, 50),
                (_, _, _, Some(VulkanImportResult::Failed(e)), _) => truncate(e, 50),
                (_, _, _, _, ImportResult::Failed(e)) => truncate(e, 50),
                _ => String::new(),
            };
            Row {
                format: c.format.name.to_string(),
                modifier: c.modifier.name.to_string(),
                alloc,
                import_req,
                import_actual,
                vulkan_explicit,
                vulkan_list,
                vulkan_chromium_explicit,
                vulkan_chromium_list,
                vulkan_query,
                notes,
            }
        })
        .collect();

    let mut table = Table::new(rows);
    table
        .with(Style::rounded())
        .with(Modify::new(Columns::single(10)).with(Width::wrap(50).keep_words(true)));
    println!("\n{}", "DMA-BUF format × modifier matrix".bold().underline());
    println!("{table}");
    println!(
        "{}",
        "  \"Import (w/ actual mod)\" = INVALID-modifier alloc, then retry with driver-chosen modifier"
            .dimmed()
    );
}

fn fmt_import(r: &ImportResult) -> String {
    match r {
        ImportResult::Ok => "ok".green().to_string(),
        ImportResult::Failed(_) => "fail".red().to_string(),
        ImportResult::Skipped => "—".dimmed().to_string(),
    }
}

fn fmt_vk_import(r: &VulkanImportResult) -> String {
    match r {
        VulkanImportResult::Ok => "ok".green().to_string(),
        VulkanImportResult::Failed(_) => "fail".red().to_string(),
        VulkanImportResult::Skipped(_) => "skip".dimmed().to_string(),
    }
}

fn fmt_vk_query(r: &Option<VulkanQueryResult>) -> String {
    match r {
        None => "—".dimmed().to_string(),
        Some(VulkanQueryResult::Supported) => "ok".green().to_string(),
        Some(VulkanQueryResult::NotSupported) => "n/a".yellow().to_string(),
        Some(VulkanQueryResult::Error(_)) => "err".red().to_string(),
        Some(VulkanQueryResult::Skipped(_)) => "skip".dimmed().to_string(),
    }
}

pub fn print_summary(cells: &[MatrixCell]) {
    let total = cells.len();
    let alloc_ok = cells
        .iter()
        .filter(|c| matches!(c.alloc, AllocResult::Ok { .. }))
        .count();
    let import_req_ok = cells
        .iter()
        .filter(|c| matches!(c.import_as_requested, ImportResult::Ok))
        .count();
    let retried = cells
        .iter()
        .filter(|c| c.import_with_actual_modifier.is_some())
        .count();
    let import_actual_ok = cells
        .iter()
        .filter(|c| matches!(c.import_with_actual_modifier, Some(ImportResult::Ok)))
        .count();
    let recovered_by_actual = cells
        .iter()
        .filter(|c| {
            matches!(c.import_as_requested, ImportResult::Failed(_))
                && matches!(c.import_with_actual_modifier, Some(ImportResult::Ok))
        })
        .count();

    println!("\n{}", "Summary".bold().underline());
    println!("  Combinations tested:        {}", total);
    println!(
        "  GBM allocations succeeded:  {} ({}%)",
        alloc_ok.green(),
        pct(alloc_ok, total)
    );
    println!(
        "  EGL imports (as requested): {} ({}%)",
        import_req_ok.green(),
        pct(import_req_ok, total)
    );
    if retried > 0 {
        println!(
            "  EGL imports (w/ actual mod): {} of {} retried ({}% of retries)",
            import_actual_ok.green(),
            retried,
            pct(import_actual_ok, retried)
        );
        if recovered_by_actual > 0 {
            println!(
                "  {} Recovered by passing the driver's chosen modifier explicitly: {}",
                "★".yellow().bold(),
                recovered_by_actual.green()
            );
            println!(
                "{}",
                "  → Likely fix for chromium: pass modifier on every import, even for INVALID alloc.".dimmed()
            );
        }
    }

    let vk_exp_attempted = cells
        .iter()
        .filter(|c| matches!(&c.vulkan_import_explicit, Some(VulkanImportResult::Ok | VulkanImportResult::Failed(_))))
        .count();
    let vk_exp_ok = cells
        .iter()
        .filter(|c| matches!(&c.vulkan_import_explicit, Some(VulkanImportResult::Ok)))
        .count();
    let vk_list_attempted = cells
        .iter()
        .filter(|c| matches!(&c.vulkan_import_list, Some(VulkanImportResult::Ok | VulkanImportResult::Failed(_))))
        .count();
    let vk_list_ok = cells
        .iter()
        .filter(|c| matches!(&c.vulkan_import_list, Some(VulkanImportResult::Ok)))
        .count();

    if vk_exp_attempted > 0 || vk_list_attempted > 0 {
        println!(
            "  Vulkan import (explicit):   {} of {} attempted ({}%)",
            vk_exp_ok.green(),
            vk_exp_attempted,
            pct(vk_exp_ok, vk_exp_attempted)
        );
        println!(
            "  Vulkan import (list):       {} of {} attempted ({}%)",
            vk_list_ok.green(),
            vk_list_attempted,
            pct(vk_list_ok, vk_list_attempted)
        );

        // The most diagnostically interesting case: explicit path
        // fails but list path succeeds on the same buffer. Confirms
        // the chromium / ANGLE failure is recoverable by switching
        // strategies — and identifies the driver-side bug as being
        // in the explicit path validation specifically.
        let exp_fail_list_ok = cells
            .iter()
            .filter(|c| {
                let exp_fail = matches!(&c.vulkan_import_explicit, Some(VulkanImportResult::Failed(_)));
                let list_ok = matches!(&c.vulkan_import_list, Some(VulkanImportResult::Ok));
                exp_fail && list_ok
            })
            .count();
        if exp_fail_list_ok > 0 {
            println!();
            println!(
                "  {} {} buffers: explicit-layout import FAILS, list-based import SUCCEEDS",
                "★".yellow().bold(),
                exp_fail_list_ok.to_string().red().bold()
            );
            println!(
                "{}",
                "  → Driver-side bug is scoped to VkImageDrmFormatModifierExplicitCreateInfoEXT.".dimmed()
            );
            println!(
                "{}",
                "  → ANGLE / Chromium use the explicit path; libplacebo (mpv) uses the list path".dimmed()
            );
            println!(
                "{}",
                "    and works on the same buffers. Workaround: switch caller to list path.".dimmed()
            );
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

fn pct(n: usize, d: usize) -> u32 {
    if d == 0 { 0 } else { ((n as f64 / d as f64) * 100.0).round() as u32 }
}
