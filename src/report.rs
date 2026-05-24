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
    /// EGL import using the modifier the caller asked for (none for INVALID rows).
    #[tabled(rename = "Import (as req)")]
    import_req: String,
    /// EGL import using the driver-chosen modifier (only for INVALID rows
    /// that allocated with a concrete modifier).
    #[tabled(rename = "Import (w/ actual mod)")]
    import_actual: String,
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
            // Notes priority: alloc failure first, then whichever
            // import attempt failed most informatively.
            let notes = match (&c.alloc, &c.import_as_requested, &c.import_with_actual_modifier) {
                (AllocResult::Failed(e), _, _) => truncate(e, 50),
                (_, _, Some(ImportResult::Failed(e))) if matches!(&c.import_as_requested, ImportResult::Failed(_)) => {
                    // Both failed — the actual-modifier one usually has the more interesting error.
                    truncate(e, 50)
                }
                (_, ImportResult::Failed(e), _) => truncate(e, 50),
                _ => String::new(),
            };
            Row {
                format: c.format.name.to_string(),
                modifier: c.modifier.name.to_string(),
                alloc,
                import_req,
                import_actual,
                notes,
            }
        })
        .collect();

    let mut table = Table::new(rows);
    table
        .with(Style::rounded())
        .with(Modify::new(Columns::single(5)).with(Width::wrap(50).keep_words(true)));
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
