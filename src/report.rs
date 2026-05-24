//! Pretty-printing of probe results.

use owo_colors::OwoColorize;
use tabled::{
    settings::{object::Columns, Modify, Style, Width},
    Table, Tabled,
};

use crate::probe::{AllocResult, ImportResult, MatrixCell, ModifierSpec, FormatSpec, Probe};

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
    #[tabled(rename = "EGL import")]
    import: String,
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
            let import = match &c.import {
                ImportResult::Ok => "ok".green().to_string(),
                ImportResult::Failed(_) => "fail".red().to_string(),
                ImportResult::Skipped => "—".dimmed().to_string(),
            };
            let notes = match (&c.alloc, &c.import) {
                (AllocResult::Failed(e), _) => truncate(e, 60),
                (_, ImportResult::Failed(e)) => truncate(e, 60),
                _ => String::new(),
            };
            Row {
                format: c.format.name.to_string(),
                modifier: c.modifier.name.to_string(),
                alloc,
                import,
                notes,
            }
        })
        .collect();

    let mut table = Table::new(rows);
    table
        .with(Style::rounded())
        .with(Modify::new(Columns::single(4)).with(Width::wrap(60).keep_words(true)));
    println!("\n{}", "DMA-BUF format × modifier matrix".bold().underline());
    println!("{table}");
}

pub fn print_summary(cells: &[MatrixCell]) {
    let total = cells.len();
    let alloc_ok = cells
        .iter()
        .filter(|c| matches!(c.alloc, AllocResult::Ok { .. }))
        .count();
    let import_ok = cells
        .iter()
        .filter(|c| matches!(c.import, ImportResult::Ok))
        .count();

    println!("\n{}", "Summary".bold().underline());
    println!(
        "  Combinations tested:  {}",
        total
    );
    println!(
        "  Allocations succeeded: {} ({}%)",
        alloc_ok.green(),
        pct(alloc_ok, total)
    );
    println!(
        "  Imports succeeded:     {} ({}%)",
        import_ok.green(),
        pct(import_ok, total)
    );

    let import_fail = total - import_ok - cells
        .iter()
        .filter(|c| matches!(c.import, ImportResult::Skipped))
        .count();
    if import_fail > 0 {
        println!(
            "  Imports failed:        {}",
            import_fail.red()
        );
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
