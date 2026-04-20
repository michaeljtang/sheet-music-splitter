use anyhow::{Context, Result};
use image::{ImageBuffer, Luma, Rgb};
use pdfium_render::prelude::*;
use std::path::Path;

use crate::detect::{self, Band, StripShape, PageBands, PARTS};
use crate::crop;

pub fn run(input: &Path, output_dir: &Path, dpi: u32, debug: bool, min_padding_factor: f32) -> Result<()> {
    let pdfium = Pdfium::new(
        Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path("./"))
            .or_else(|_| Pdfium::bind_to_system_library())?,
    );

    // Create a per-piece subfolder: output_dir/{stem}/
    let piece_name = input
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let piece_dir = output_dir.join(&piece_name);
    std::fs::create_dir_all(&piece_dir)?;

    let doc = pdfium.load_pdf_from_file(input, None)?;
    let page_count = doc.pages().len() as usize;
    let input_str = input.to_string_lossy().to_string();
    println!("Processing '{}' ({} pages)...", piece_name, page_count);

    let mut entries: [Vec<(String, u32, StripShape)>; 4] = Default::default();
    let mut header: Option<StripShape> = None;

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

        let bands = detect::detect(&gray, height_pts, width_pts, dpi, min_padding_factor);

        print!("  page {}/{}: ", page_idx + 1, page_count);
        match &bands {
            PageBands::Systems { systems, header: page_header } => {
                println!("{} systems", systems.len());
                if debug {
                    save_debug_image(&gray, systems, &piece_dir, page_idx, height_pts, dpi)?;
                }
                // Capture header from the very first page's first system
                if page_idx == 0 && header.is_none() {
                    header = Some(page_header.clone());
                }
                for system in systems {
                    for inst in 0..4 {
                        entries[inst].push((input_str.clone(), page_idx as u32, system[inst].clone()));
                    }
                }
            }
            PageBands::FullPage => {
                println!("full page");
                let full_shape = StripShape {
                    base: Band { y_top: height_pts, y_bot: 0.0 },
                    protrusions: Vec::new(),
                };
                for inst in 0..4 {
                    entries[inst].push((input_str.clone(), page_idx as u32, full_shape.clone()));
                }
            }
        }
    }

    let entries_by_inst: [Vec<(u32, StripShape)>; 4] = std::array::from_fn(|inst| {
        entries[inst].iter().map(|(_, pg, shape)| (*pg, shape.clone())).collect()
    });

    crop::write_parts(input, &entries_by_inst, header.as_ref(), &PARTS, &piece_dir)?;

    Ok(())
}

fn save_debug_image(
    gray: &ImageBuffer<Luma<u8>, Vec<u8>>,
    systems: &[[StripShape; 4]],
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

    // Dimmer versions for protrusion outlines
    let prot_colors = [
        Rgb([180u8, 80, 80]),
        Rgb([80, 150, 80]),
        Rgb([80, 100, 180]),
        Rgb([150, 80, 150]),
    ];

    for system in systems {
        for (inst, shape) in system.iter().enumerate() {
            // Draw base band boundaries (full-width horizontal lines)
            for &y_pts in &[shape.base.y_top, shape.base.y_bot] {
                let y_px = ((page_height_pts - y_pts) / pts_per_px).round() as u32;
                let y_px = y_px.min(h - 1);
                for dy in 0..3u32 {
                    let row = (y_px + dy).min(h - 1);
                    for x in 0..w {
                        rgb.put_pixel(x, row, colors[inst]);
                    }
                }
            }

            // Draw protrusion rectangles as outlines
            for prot in &shape.protrusions {
                let p_top = ((page_height_pts - prot.y_top) / pts_per_px).round() as u32;
                let p_bot = ((page_height_pts - prot.y_bot) / pts_per_px).round() as u32;
                let p_left = (prot.x_left / pts_per_px).round() as u32;
                let p_right = (prot.x_right / pts_per_px).round() as u32;
                let p_top = p_top.min(h - 1);
                let p_bot = p_bot.min(h - 1);
                let p_left = p_left.min(w - 1);
                let p_right = p_right.min(w - 1);

                // Top and bottom edges
                for &row in &[p_top, p_bot] {
                    for dy in 0..2u32 {
                        let r = (row + dy).min(h - 1);
                        for x in p_left..=p_right.min(w - 1) {
                            rgb.put_pixel(x, r, prot_colors[inst]);
                        }
                    }
                }
                // Left and right edges
                for y in p_top..=p_bot {
                    for &col in &[p_left, p_right] {
                        for dx in 0..2u32 {
                            let c = (col + dx).min(w - 1);
                            rgb.put_pixel(c, y, prot_colors[inst]);
                        }
                    }
                }
            }
        }
    }

    let path = output_dir.join(format!("page_{:03}_debug.png", page_idx + 1));
    rgb.save(&path)?;
    Ok(())
}
