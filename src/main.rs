//! egl-dmabuf-probe — characterize NVIDIA / Mesa EGL support for the
//! pixmap-import and DMA-BUF-import code paths that Chromium and
//! Wayland compositors exercise.
//!
//! Outputs a (format × modifier) matrix showing which combinations
//! the driver can allocate via GBM and import via
//! `eglCreateImage(EGL_LINUX_DMA_BUF_EXT, ...)`. Distinguishes
//! allocation failure (GBM rejected it) from import failure (allocated
//! fine but EGL refused).

mod egl_ffi;
mod probe;
mod report;
mod vulkan_import;

use std::path::PathBuf;

use clap::Parser;
use owo_colors::OwoColorize;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// DRM render node to test. Defaults to /dev/dri/renderD128.
    #[arg(short = 'd', long, default_value = "/dev/dri/renderD128")]
    device: PathBuf,

    /// Test only these comma-separated formats (e.g. NV12,P010,XRGB8888).
    /// Default: a curated set covering RGB, YUV 4:2:0, and 10-bit YUV.
    #[arg(long, value_delimiter = ',')]
    formats: Option<Vec<String>>,

    /// Test only these comma-separated modifiers
    /// (e.g. LINEAR,INVALID,I915_Y_TILED,NVIDIA_BLOCK_LINEAR_2D).
    #[arg(long, value_delimiter = ',')]
    modifiers: Option<Vec<String>>,

    /// Print the EGL error code/string for every failed attempt.
    #[arg(short, long)]
    verbose: bool,

    /// Skip the format × modifier matrix; only print extension info.
    #[arg(long)]
    extensions_only: bool,

    /// Skip the Vulkan dma-buf import probe (useful if libvulkan is
    /// unavailable or the GPU has no Vulkan support).
    #[arg(long)]
    skip_vulkan: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    eprintln!(
        "{} {} on render node {}\n",
        "egl-dmabuf-probe".bold(),
        env!("CARGO_PKG_VERSION").dimmed(),
        args.device.display().to_string().cyan(),
    );

    let probe = probe::Probe::new(&args.device)?;

    let vulkan_probe = if args.skip_vulkan {
        None
    } else {
        match vulkan_import::VulkanProbe::new() {
            Ok(vp) => Some(vp),
            Err(e) => {
                eprintln!(
                    "{} {}",
                    "Vulkan probe init failed:".yellow(),
                    e
                );
                eprintln!(
                    "{}",
                    "  Continuing with EGL-only probe. Pass --skip-vulkan to silence.".dimmed()
                );
                None
            }
        }
    };

    report::print_device_info(&probe);
    report::print_extension_summary(&probe);
    report::print_driver_claims(&probe);
    if let Some(vp) = &vulkan_probe {
        report::print_vulkan_info(vp);
    }

    if args.extensions_only {
        return Ok(());
    }

    let formats = probe::formats_to_test(args.formats.as_deref());
    let modifiers = probe::modifiers_to_test(args.modifiers.as_deref());

    eprintln!(
        "\nProbing {} × {} = {} combinations...\n",
        formats.len(),
        modifiers.len(),
        formats.len() * modifiers.len()
    );

    let results = probe.run_matrix_with_vulkan(
        &formats,
        &modifiers,
        vulkan_probe.as_ref(),
        args.verbose,
    );
    report::print_matrix(&results, &formats, &modifiers);
    report::print_summary(&results);

    Ok(())
}
