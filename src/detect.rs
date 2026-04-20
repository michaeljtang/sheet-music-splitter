use image::GrayImage;
use realfft::RealFftPlanner;

pub const PARTS: [&str; 4] = ["violin1", "violin2", "viola", "cello"];

/// One instrument's band within one system, in PDF points (origin bottom-left).
#[derive(Clone, Copy, Debug)]
pub struct Band {
    pub y_top: f32,
    pub y_bot: f32,
}

/// A rectangular protrusion extending beyond the base band, in PDF points.
#[derive(Clone, Debug)]
pub struct Protrusion {
    pub x_left: f32,
    pub x_right: f32,
    pub y_top: f32,
    pub y_bot: f32,
}

/// Full clip shape for one instrument strip: base rectangle + optional protrusions.
#[derive(Clone, Debug)]
pub struct StripShape {
    pub base: Band,
    pub protrusions: Vec<Protrusion>,
}

/// Per-page detection result: a list of systems, each system has 4 strip shapes.
pub enum PageBands {
    /// Systems detected: each inner array is [violin1, violin2, viola, cello] strip shapes.
    /// `header`: StripShape for the title/composer/tempo area above system 1.
    Systems { systems: Vec<[StripShape; 4]>, header: StripShape },
    /// No staves — copy full page to all outputs unchanged
    FullPage,
}

pub fn detect(img: &GrayImage, page_height_pts: f32, page_width_pts: f32, dpi: u32, min_padding_factor: f32) -> PageBands {
    let pts_per_px = 72.0 / dpi as f32;
    let (w, h) = img.dimensions();

    let x0 = w / 5;
    let x1 = w * 4 / 5;
    let signal: Vec<f32> = (0..h)
        .map(|y| {
            (x0..x1)
                .map(|x| 255.0 - img.get_pixel(x, y).0[0] as f32)
                .sum::<f32>()
                / (x1 - x0) as f32
        })
        .collect();

    let line_spacing = match staff_line_spacing(&signal) {
        Some(s) => s,
        None => return PageBands::FullPage,
    };

    let staff_span = 4.0 * line_spacing;
    let conv = comb_convolve(&signal, line_spacing);
    let mut stave_tops = non_max_suppress(&conv, staff_span * 1.8);

    // ── False-stave removal (only when count isn't divisible by 4) ────
    // When we have extra staves (e.g. 9 instead of 8, 13 instead of 12),
    // identify and remove false detections caused by 8va lines, title text,
    // or other non-stave horizontal content.
    while stave_tops.len() > 4 && stave_tops.len() % 4 != 0 {
        let gaps: Vec<f32> = stave_tops.windows(2).map(|w| w[1] - w[0]).collect();
        let mut sorted = gaps.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = sorted[sorted.len() / 2];

        // Find the most anomalous gap (smallest or largest relative to median)
        let (min_idx, &min_gap) = gaps.iter().enumerate()
            .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap()).unwrap();
        let first_gap = gaps[0];
        let last_gap = *gaps.last().unwrap();

        let small_gap_ratio = min_gap / median;
        let first_large_ratio = first_gap / median;
        let last_large_ratio = last_gap / median;

        // Prefer removing small-gap false staves (ratio < 0.75),
        // then low-score outliers at front/back (score < 50% of median score),
        // then large-gap outliers at front/back (ratio > 1.6).
        if small_gap_ratio < 0.75 {
            // Two staves too close — remove the one with lower convolution score
            let idx_a = stave_tops[min_idx] as usize;
            let idx_b = stave_tops[min_idx + 1] as usize;
            let score_a = if idx_a < conv.len() { conv[idx_a] } else { 0.0 };
            let score_b = if idx_b < conv.len() { conv[idx_b] } else { 0.0 };
            if score_a <= score_b {
                stave_tops.remove(min_idx);
            } else {
                stave_tops.remove(min_idx + 1);
            }
        } else {
            // Check convolution scores at front/back for weak false detections
            let scores: Vec<f32> = stave_tops.iter()
                .map(|&y| { let i = y as usize; if i < conv.len() { conv[i] } else { 0.0 } })
                .collect();
            let mut sorted_scores = scores.clone();
            sorted_scores.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median_score = sorted_scores[sorted_scores.len() / 2];
            let first_score = scores[0];
            let last_score = *scores.last().unwrap();

            if first_score < median_score * 0.5 && first_score <= last_score {
                stave_tops.remove(0);
            } else if last_score < median_score * 0.5 {
                stave_tops.pop();
            } else if first_large_ratio > 1.6 && first_gap >= last_gap {
                stave_tops.remove(0);
            } else if last_large_ratio > 1.6 {
                stave_tops.pop();
            } else {
                break; // no clear outlier found
            }
        }
    }

    if stave_tops.len() < 4 {
        eprintln!("  warning: only {} staves detected, using full page", stave_tops.len());
        return PageBands::FullPage;
    }

    let n = (stave_tops.len() / 4) * 4;
    let stave_tops = &stave_tops[..n];
    let num_systems = n / 4;

    let min_pad = min_padding_factor * line_spacing;
    // Column width for per-column scanning: ~1 staff space
    let col_width = (line_spacing.round() as u32).max(4);

    // Pre-compute system-to-system gap edges (between cello of sys N and violin1 of sys N+1)
    let mut sys_gap_top: Vec<f32> = Vec::new(); // indexed by gap index (0 = between sys 0 and 1)
    let mut sys_gap_bot: Vec<f32> = Vec::new();
    for s in 0..num_systems.saturating_sub(1) {
        let cello_bot = stave_tops[s * 4 + 3] + staff_span;
        let next_vln1_top = stave_tops[(s + 1) * 4];
        let (gt, gb) = find_gap_edges(img, cello_bot, next_vln1_top);
        sys_gap_top.push(gt);
        sys_gap_bot.push(gb);
    }

    let mut systems: Vec<[StripShape; 4]> = Vec::with_capacity(num_systems);

    for sys in 0..num_systems {
        let sys_staves = &stave_tops[sys * 4..(sys + 1) * 4];

        // Per-instrument staff edges (pixel coordinates, no padding)
        let mut staff_top_px = [0f32; 4];
        let mut staff_bot_px = [0f32; 4];
        for inst in 0..4 {
            staff_top_px[inst] = sys_staves[inst];
            staff_bot_px[inst] = sys_staves[inst] + staff_span;
        }

        // Base band boundaries (with fixed padding) — same as before
        let mut band_top_px = [0f32; 4];
        let mut band_bot_px = [h as f32; 4];

        // Top of system boundary
        if sys == 0 {
            let top_pad = 4.5 * line_spacing;
            band_top_px[0] = (staff_top_px[0] - top_pad).max(0.0);
        } else {
            // Use midpoint of the system-to-system gap
            let gap_mid = (sys_gap_top[sys - 1] + sys_gap_bot[sys - 1]) / 2.0;
            band_top_px[0] = gap_mid;
        }

        // Bottom of system boundary
        if sys == num_systems - 1 {
            band_bot_px[3] = (staff_bot_px[3] + 3.0 * line_spacing).min(h as f32);
        } else {
            // Use midpoint of the system-to-system gap
            let gap_mid = (sys_gap_top[sys] + sys_gap_bot[sys]) / 2.0;
            band_bot_px[3] = gap_mid;
        }

        // Inter-instrument boundaries: find widest empty gap, split at its midpoint
        // This places boundaries in actual whitespace rather than between staves.
        let mut gap_top = [0f32; 3]; // top of gap between inst i and i+1
        let mut gap_bot = [0f32; 3]; // bottom of gap
        let mut gap_mid_px = [0f32; 3]; // original gap midpoints (used as protrusion scan limits)
        for i in 0..3 {
            let (g_top, g_bot) = find_gap_edges(img, staff_bot_px[i], staff_top_px[i + 1]);
            gap_top[i] = g_top;
            gap_bot[i] = g_bot;
            let gap_mid = (g_top + g_bot) / 2.0;
            gap_mid_px[i] = gap_mid;
            band_bot_px[i] = gap_mid;
            band_top_px[i + 1] = gap_mid;
        }

        // Tighten base bands: cap to 2.5× line_spacing above/below each staff.
        // This keeps lines compact; protrusions extend where content actually needs more.
        let max_pad = 2.5 * line_spacing;
        for inst in 0..4 {
            let tight_top = staff_top_px[inst] - max_pad;
            if band_top_px[inst] < tight_top {
                band_top_px[inst] = tight_top;
            }
            let tight_bot = staff_bot_px[inst] + max_pad;
            if band_bot_px[inst] > tight_bot {
                band_bot_px[inst] = tight_bot;
            }
        }

        // Now scan for protrusions per instrument
        let shapes: [StripShape; 4] = std::array::from_fn(|inst| {
            let base_top_px = band_top_px[inst];
            let base_bot_px = band_bot_px[inst];

            let mut protrusions_px: Vec<(u32, u32, f32, f32)> = Vec::new(); // (x_left, x_right, y_top, y_bot) in pixels

            // Scan upward: from base_top toward content above
            let scan_up_limit = if inst == 0 {
                if sys == 0 {
                    base_top_px // no scan into title/header area
                } else {
                    // Use system gap midpoint, not the far edge
                    let gap_mid = (sys_gap_top[sys - 1] + sys_gap_bot[sys - 1]) / 2.0;
                    gap_mid.min(base_top_px)
                }
            } else {
                // Scan up to the original gap midpoint (before tightening),
                // so protrusions can reclaim space where content actually exists.
                gap_mid_px[inst - 1].min(base_top_px)
            };

            if base_top_px > scan_up_limit {
                let up_protrusions = scan_protrusions_in_region(
                    img, scan_up_limit, base_top_px, col_width, min_pad, true,
                );
                protrusions_px.extend(up_protrusions);
            }

            // Scan downward: from base_bot toward content below
            let scan_down_limit = if inst == 3 {
                if sys == num_systems - 1 {
                    (staff_bot_px[3] + 3.0 * line_spacing).min(h as f32)
                } else {
                    // Use system gap midpoint, not the far edge — prevents scanning
                    // deep into the next system's territory
                    let gap_mid = (sys_gap_top[sys] + sys_gap_bot[sys]) / 2.0;
                    gap_mid.max(base_bot_px)
                }
            } else {
                // Scan down to the original gap midpoint (before tightening)
                gap_mid_px[inst].max(base_bot_px)
            };

            if scan_down_limit > base_bot_px {
                let down_protrusions = scan_protrusions_in_region(
                    img, base_bot_px, scan_down_limit, col_width, min_pad, false,
                );
                protrusions_px.extend(down_protrusions);
            }

            // Convert everything to PDF coordinates
            let base = Band {
                y_top: (page_height_pts - base_top_px * pts_per_px).clamp(0.0, page_height_pts),
                y_bot: (page_height_pts - base_bot_px * pts_per_px).clamp(0.0, page_height_pts),
            };

            let protrusions: Vec<Protrusion> = protrusions_px
                .iter()
                .map(|&(xl, xr, yt, yb)| Protrusion {
                    x_left: xl as f32 * pts_per_px,
                    x_right: xr as f32 * pts_per_px,
                    y_top: (page_height_pts - yt * pts_per_px).clamp(0.0, page_height_pts),
                    y_bot: (page_height_pts - yb * pts_per_px).clamp(0.0, page_height_pts),
                })
                .collect();

            StripShape { base, protrusions }
        });

        systems.push(shapes);
    }

    let _ = page_width_pts;
    // Header: base captures title/composer area; protrusions reach down to
    // capture tempo markings and rehearsal letters that sit just above the stave.
    // Base bottom = 4.5 * line_spacing above the first stave (matches violin 1's band top).
    let header_base_bot_px = (stave_tops[0] - 4.5 * line_spacing).max(0.0);
    // Scan for content between header base and just above the first stave.
    // Stop 1.5× line_spacing above stave top — close enough to capture the full
    // tempo marking without clipping into the staff itself.
    let scan_top = header_base_bot_px;
    let scan_bot = (stave_tops[0] - 1.5 * line_spacing).max(scan_top);
    let mut header_protrusions_px: Vec<(u32, u32, f32, f32)> = Vec::new();
    if scan_bot > scan_top + 2.0 {
        let prots = scan_protrusions_in_region(
            img, scan_top, scan_bot, col_width, min_pad, false,
        );
        for (xl, xr, yt, yb) in prots {
            // Clamp to scan region so padding doesn't push into violin 1's staff
            header_protrusions_px.push((xl, xr, yt, yb.min(scan_bot)));
        }
    }

    let header_base = Band {
        y_top: page_height_pts,
        y_bot: (page_height_pts - header_base_bot_px * pts_per_px).clamp(0.0, page_height_pts),
    };
    let header_protrusions: Vec<Protrusion> = header_protrusions_px
        .iter()
        .map(|&(xl, xr, yt, yb)| Protrusion {
            x_left: xl as f32 * pts_per_px,
            x_right: xr as f32 * pts_per_px,
            y_top: (page_height_pts - yt * pts_per_px).clamp(0.0, page_height_pts),
            y_bot: (page_height_pts - yb * pts_per_px).clamp(0.0, page_height_pts),
        })
        .collect();
    let header = StripShape { base: header_base, protrusions: header_protrusions };

    PageBands::Systems { systems, header }
}

/// Find the widest empty gap between two adjacent staves.
/// Returns (gap_top, gap_bot) in pixel rows — the top and bottom edges of the
/// widest run of empty rows. If no gap is found, returns the midpoint.
fn find_gap_edges(img: &GrayImage, staff_bot: f32, next_staff_top: f32) -> (f32, f32) {
    let (w, h) = img.dimensions();
    let gap_start = (staff_bot.ceil() as u32).min(h - 1);
    let gap_end = (next_staff_top.floor() as u32).min(h - 1);
    let midpoint = (staff_bot + next_staff_top) / 2.0;

    if gap_start >= gap_end {
        return (midpoint, midpoint);
    }

    let x_margin = w / 20;
    let x0 = x_margin;
    let x1 = w - x_margin;
    let dark_threshold: u8 = 200;

    let row_counts: Vec<u32> = (gap_start..=gap_end)
        .map(|y| {
            (x0..x1)
                .filter(|&x| img.get_pixel(x, y).0[0] < dark_threshold)
                .count() as u32
        })
        .collect();

    let n = row_counts.len();
    let smoothed: Vec<f32> = (0..n)
        .map(|i| {
            let lo = i.saturating_sub(2);
            let hi = (i + 3).min(n);
            row_counts[lo..hi].iter().sum::<u32>() as f32 / (hi - lo) as f32
        })
        .collect();

    let ink_threshold = 2.0;
    let has_ink: Vec<bool> = smoothed.iter().map(|&v| v > ink_threshold).collect();

    // Find widest run of empty rows
    let mut best_start = 0usize;
    let mut best_len = 0usize;
    let mut run_start = 0usize;
    let mut run_len = 0usize;
    let mut in_run = false;

    for (i, &has) in has_ink.iter().enumerate() {
        if !has {
            if !in_run {
                run_start = i;
                run_len = 0;
                in_run = true;
            }
            run_len += 1;
        } else {
            if in_run && run_len > best_len {
                best_start = run_start;
                best_len = run_len;
            }
            in_run = false;
        }
    }
    if in_run && run_len > best_len {
        best_start = run_start;
        best_len = run_len;
    }

    if best_len == 0 {
        return (midpoint, midpoint);
    }

    let g_top = gap_start as f32 + best_start as f32;
    let g_bot = g_top + best_len as f32;
    (g_top, g_bot)
}

/// Scan a region beyond a base band edge for ink connected to the current instrument.
/// Works outward from the band edge, stopping at whitespace gaps to avoid capturing
/// the neighboring instrument's content.
///
/// `region_top_px` < `region_bot_px` (pixel coords, y increases downward).
/// `extending_up`: if true, scans upward from region_bot_px (the base band's top edge);
///   if false, scans downward from region_top_px (the base band's bottom edge).
fn scan_protrusions_in_region(
    img: &GrayImage,
    region_top_px: f32,
    region_bot_px: f32,
    col_width: u32,
    pad_px: f32,
    extending_up: bool,
) -> Vec<(u32, u32, f32, f32)> {
    let (w, h) = img.dimensions();
    let dark_threshold: u8 = 200;
    let ink_count_threshold: u32 = 2;
    // Max gap of empty rows before we stop — prevents jumping to neighboring instrument
    let max_gap = (pad_px * 0.75).max(4.0) as u32;

    let r_top = (region_top_px.ceil() as u32).min(h - 1);
    let r_bot = (region_bot_px.floor() as u32).min(h - 1);
    if r_top >= r_bot {
        return Vec::new();
    }

    let x_margin = w / 20;
    let x_start = x_margin;
    let x_end = w - x_margin;

    struct ColInk {
        x_left: u32,
        x_right: u32,
        ink_edge: f32,
    }

    let mut col_inks: Vec<ColInk> = Vec::new();

    let mut col_x = x_start;
    while col_x < x_end {
        let col_right = (col_x + col_width).min(x_end);

        // Scan outward from the band edge, tracking connected ink
        let ink_edge = if extending_up {
            // Scan from r_bot-1 upward (decreasing y) toward r_top
            let mut farthest: Option<u32> = None;
            let mut gap_count = 0u32;

            let start_y = r_bot.saturating_sub(1);
            if start_y >= r_top {
                for y in (r_top..=start_y).rev() {
                    let has_ink = (col_x..col_right)
                        .filter(|&x| img.get_pixel(x, y).0[0] < dark_threshold)
                        .count() as u32 >= ink_count_threshold;

                    if has_ink {
                        farthest = Some(farthest.map_or(y, |f: u32| f.min(y)));
                        gap_count = 0;
                    } else {
                        gap_count += 1;
                        if farthest.is_some() && gap_count > max_gap {
                            break; // hit a large gap — stop, rest belongs to neighbor
                        }
                    }
                }
            }
            farthest
        } else {
            // Scan from r_top downward (increasing y) toward r_bot
            let mut farthest: Option<u32> = None;
            let mut gap_count = 0u32;

            let start_y = r_top;
            if start_y < r_bot {
                for y in start_y..r_bot {
                    let has_ink = (col_x..col_right)
                        .filter(|&x| img.get_pixel(x, y).0[0] < dark_threshold)
                        .count() as u32 >= ink_count_threshold;

                    if has_ink {
                        farthest = Some(farthest.map_or(y, |f: u32| f.max(y)));
                        gap_count = 0;
                    } else {
                        gap_count += 1;
                        if farthest.is_some() && gap_count > max_gap {
                            break;
                        }
                    }
                }
            }
            farthest
        };

        if let Some(edge) = ink_edge {
            col_inks.push(ColInk {
                x_left: col_x,
                x_right: col_right,
                ink_edge: edge as f32,
            });
        }

        col_x = col_right;
    }

    if col_inks.is_empty() {
        return Vec::new();
    }

    // Merge adjacent columns with ink into wider protrusion rectangles
    let mut protrusions: Vec<(u32, u32, f32, f32)> = Vec::new();
    let mut group_start = 0usize;

    while group_start < col_inks.len() {
        let mut group_end = group_start;
        let mut farthest = col_inks[group_start].ink_edge;

        // Merge adjacent columns (allow a 1-column gap for robustness)
        while group_end + 1 < col_inks.len() {
            let gap = col_inks[group_end + 1].x_left - col_inks[group_end].x_right;
            if gap <= col_width {
                group_end += 1;
                if extending_up {
                    farthest = farthest.min(col_inks[group_end].ink_edge);
                } else {
                    farthest = farthest.max(col_inks[group_end].ink_edge);
                }
            } else {
                break;
            }
        }

        // Add padding around the ink
        let padded_edge = if extending_up {
            (farthest - pad_px).max(0.0)
        } else {
            (farthest + pad_px).min(h as f32)
        };

        // Protrusion extends from the base band edge to the padded ink edge
        let x_left = col_inks[group_start].x_left.saturating_sub(col_width / 2);
        let x_right = (col_inks[group_end].x_right + col_width / 2).min(w);

        let (y_top, y_bot) = if extending_up {
            (padded_edge, region_bot_px)
        } else {
            (region_top_px, padded_edge)
        };

        protrusions.push((x_left, x_right, y_top, y_bot));

        group_start = group_end + 1;
    }

    protrusions
}

fn staff_line_spacing(signal: &[f32]) -> Option<f32> {
    let n = signal.len();
    let mut buf = signal.to_vec();
    let mut planner = RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(n);
    let mut spectrum = fft.make_output_vec();
    fft.process(&mut buf, &mut spectrum).ok()?;

    // Staff line spacing at 150 DPI: 8–20px
    let min_bin = (n / 20).max(1);
    let max_bin = (n / 8).min(spectrum.len() - 1);
    if min_bin >= max_bin {
        return None;
    }

    let mags: Vec<f32> = spectrum.iter().map(|c| c.norm()).collect();

    let (best_i, best_mag) = mags[min_bin..=max_bin]
        .iter()
        .enumerate()
        .fold((0, 0f32), |acc, (i, &m)| if m > acc.1 { (i, m) } else { acc });

    let mean_mag = mags[min_bin..=max_bin].iter().sum::<f32>()
        / (max_bin - min_bin + 1) as f32;

    if best_mag < mean_mag * 3.0 {
        return None;
    }

    let best_bin = best_i + min_bin;
    let spacing = n as f32 / best_bin as f32;

    // Correct for 2× harmonic: if the 2× frequency bin is dominant,
    // the detected spacing may be a sub-harmonic — double it to get the true spacing.
    // Only apply if the doubled spacing stays within the valid staff-line range (8–20px).
    let harmonic_bin = best_bin * 2;
    if harmonic_bin < mags.len() && mags[harmonic_bin] > best_mag * 0.8 {
        let doubled = spacing * 2.0;
        if doubled <= 20.0 {
            return Some(doubled);
        }
    }

    Some(spacing)
}

fn comb_convolve(signal: &[f32], line_spacing: f32) -> Vec<f32> {
    let n = signal.len();
    let s = line_spacing.round() as usize;
    (0..n)
        .map(|i| {
            [0, s, 2 * s, 3 * s, 4 * s]
                .iter()
                .map(|&t| if i + t < n { signal[i + t] } else { 0.0 })
                .sum()
        })
        .collect()
}

fn non_max_suppress(conv: &[f32], radius: f32) -> Vec<f32> {
    let n = conv.len();
    let r = radius.round() as usize;
    let mut buf = conv.to_vec();
    let mut peaks = vec![];

    let mean = buf.iter().sum::<f32>() / n as f32;
    let threshold = mean * 2.0;

    loop {
        let (idx, &val) = buf
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .unwrap();

        if val < threshold {
            break;
        }

        peaks.push(idx as f32);
        let lo = idx.saturating_sub(r);
        let hi = (idx + r).min(n);
        for v in buf[lo..hi].iter_mut() {
            *v = 0.0;
        }
    }

    peaks.sort_by(|a, b| a.partial_cmp(b).unwrap());
    peaks
}
