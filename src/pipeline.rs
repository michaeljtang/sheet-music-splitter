use anyhow::{Context, Result};
use image::{ImageBuffer, Luma, Rgb};
use pdfium_render::prelude::*;
use std::path::Path;

use crate::detect::{self, Band, PageBands, PARTS};
use crate::crop;

pub fn run(input: &Path, output_dir: &Path, dpi: u32, debug: bool) -> Result<()> {
    let pdfium = Pdfium::new(
        Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path("./"))
            .or_else(|_| Pdfium::bind_to_system_library())?,
    );

    let doc = pdfium.load_pdf_from_file(input, None)?;
    let page_count = doc.pages().len() as usize;
    let input_str = input.to_string_lossy().to_string();
    println!("Processing {} pages...", page_count);

    let mut entries: [Vec<(String, u32, Band)>; 4] = Default::default();
    let mut header: Option<Band> = None;

    for page_idx in 0..page_count {
        let page = doc.pages().get(page_idx as u16).context("page access")?;
        let width_pts = page.width().value;
        let height_pts = page.height().value;

        let render_cfg = PdfRenderConfig::new()
            .set_target_width((width_pts * dpi as f32 / 72.0) as i32)
            .set_target_height((height_pts * dpi as f32 / 72.0) as i32);

        let bitmap = page.render_with_config(&render_cfg)?;
        let rgb_img = bitmap.as_image().into_rgb8();
        let (w, h) = rgb_img.dimensions();
        let gray: ImageBuffer<Luma<u8>, Vec<u8>> = ImageBuffer::from_fn(w, h, |x, y| {
            let p = rgb_img.get_pixel(x, y);
            let v = (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) as u8;
            Luma([v])
        });

        let bands = detect::detect(&gray, height_pts, width_pts, dpi);

        print!("  page {}/{}: ", page_idx + 1, page_count);
        match &bands {
            PageBands::Systems { systems, first_staff_y_top } => {
                println!("{} systems", systems.len());
                if debug {
                    save_debug_image(&gray, systems, output_dir, page_idx, height_pts, dpi)?;
                }
                // Capture header from the very first page's first system
                if page_idx == 0 && header.is_none() {
                    header = Some(Band { y_top: height_pts, y_bot: *first_staff_y_top });
                }
                for system in systems {
                    for inst in 0..4 {
                        entries[inst].push((input_str.clone(), page_idx as u32, system[inst]));
                    }
                }
            }
            PageBands::FullPage => {
                println!("full page");
                let full_band = Band { y_top: height_pts, y_bot: 0.0 };
                for inst in 0..4 {
                    entries[inst].push((input_str.clone(), page_idx as u32, full_band));
                }
            }
        }
    }

    let entries_by_inst: [Vec<(u32, detect::Band)>; 4] = std::array::from_fn(|inst| {
        entries[inst].iter().map(|(_, pg, band)| (*pg, *band)).collect()
    });

    crop::write_parts(input, &entries_by_inst, header.as_ref(), &PARTS, output_dir)?;

    Ok(())
}

fn save_debug_image(
    gray: &ImageBuffer<Luma<u8>, Vec<u8>>,
    systems: &[[Band; 4]],
    output_dir: &Path,
    page_idx: usize,
    page_height_pts: f32,
    dpi: u32,
) -> Result<()> {
    let pts_per_px = 72.0 / dpi as f32;
    let (w, h) = gray.dimensions();
    let mut rgb: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_fn(w, h, |x, y| {
        let v = gray.get_pixel(x, y).0[0];
        Rgb([v, v, v])
    });

    let colors = [
        Rgb([255u8, 50, 50]),
        Rgb([50, 200, 50]),
        Rgb([50, 100, 255]),
        Rgb([200, 50, 200]),
    ];

    for system in systems {
        for (inst, band) in system.iter().enumerate() {
            for &y_pts in &[band.y_top, band.y_bot] {
                let y_px = ((page_height_pts - y_pts) / pts_per_px).round() as u32;
                let y_px = y_px.min(h - 1);
                for dy in 0..3u32 {
                    let row = (y_px + dy).min(h - 1);
                    for x in 0..w {
                        rgb.put_pixel(x, row, colors[inst]);
                    }
                }
            }
        }
    }

    let path = output_dir.join(format!("page_{:03}_debug.png", page_idx + 1));
    rgb.save(&path)?;
    Ok(())
}
