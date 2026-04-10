use image::GrayImage;
use realfft::RealFftPlanner;

pub const PARTS: [&str; 4] = ["violin1", "violin2", "viola", "cello"];

/// One instrument's band within one system, in PDF points (origin bottom-left).
#[derive(Clone, Copy, Debug)]
pub struct Band {
    pub y_top: f32,
    pub y_bot: f32,
}

/// Per-page detection result: a list of systems, each system has 4 bands.
pub enum PageBands {
    /// Systems detected: each inner array is [violin1, violin2, viola, cello] bands.
    /// `first_staff_y_top`: PDF-point Y of the top of the first staff on this page
    /// (used to define the header region above system 1).
    Systems { systems: Vec<[Band; 4]>, first_staff_y_top: f32 },
    /// No staves — copy full page to all outputs unchanged
    FullPage,
}

pub fn detect(img: &GrayImage, page_height_pts: f32, page_width_pts: f32, dpi: u32) -> PageBands {
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
    let stave_tops = non_max_suppress(&conv, staff_span * 1.8);

    if stave_tops.len() < 4 {
        eprintln!("  warning: only {} staves detected, using full page", stave_tops.len());
        return PageBands::FullPage;
    }

    let n = (stave_tops.len() / 4) * 4;
    let stave_tops = &stave_tops[..n];
    let num_systems = n / 4;

    // Group staves into systems: each consecutive group of 4 is one system
    let mut systems: Vec<[Band; 4]> = Vec::with_capacity(num_systems);

    for sys in 0..num_systems {
        let sys_staves = &stave_tops[sys * 4..(sys + 1) * 4];

        // Per-instrument bounds within this system
        let mut inst_tops = [0f32; 4];
        let mut inst_bots = [0f32; 4];
        for inst in 0..4 {
            let top = sys_staves[inst];
            let bot = top + staff_span;
            // Top instrument gets extra room for tempo/text markings above the staff
            let top_pad = if inst == 0 { 4.5 } else { 3.0 };
            inst_tops[inst] = top - top_pad * line_spacing;
            inst_bots[inst] = bot + 3.0 * line_spacing;
        }

        // Band boundaries: midpoint between adjacent instruments
        let mut band_top_px = [0f32; 4];
        let mut band_bot_px = [h as f32; 4];

        // System top: for first system, top of first instrument's staff region (header is above).
        // For subsequent systems, midpoint between previous system bottom and this system top.
        if sys == 0 {
            band_top_px[0] = inst_tops[0];
        } else {
            let prev_last_bot = stave_tops[(sys - 1) * 4 + 3] + staff_span + 3.0 * line_spacing;
            band_top_px[0] = (prev_last_bot + inst_tops[0]) / 2.0;
        }

        // System bottom: cap at inst_bots[3] for last system (don't extend to page edge)
        if sys == num_systems - 1 {
            band_bot_px[3] = inst_bots[3];
        } else {
            let next_first_top = stave_tops[(sys + 1) * 4] - 3.0 * line_spacing;
            let this_last_bot = inst_bots[3];
            band_bot_px[3] = (this_last_bot + next_first_top) / 2.0;
        }

        // Internal boundaries between instruments in this system
        for i in 0..3 {
            let boundary = (inst_bots[i] + inst_tops[i + 1]) / 2.0;
            band_bot_px[i] = boundary;
            band_top_px[i + 1] = boundary;
        }

        let mut bands = [Band { y_top: 0.0, y_bot: 0.0 }; 4];
        for i in 0..4 {
            let top_px = band_top_px[i].clamp(0.0, h as f32);
            let bot_px = band_bot_px[i].clamp(0.0, h as f32);
            bands[i] = Band {
                y_top: (page_height_pts - top_px * pts_per_px).clamp(0.0, page_height_pts),
                y_bot: (page_height_pts - bot_px * pts_per_px).clamp(0.0, page_height_pts),
            };
        }
        systems.push(bands);
    }

    let _ = page_width_pts;
    // first_staff_y_top: top of first instrument's staff region on this page (in PDF pts)
    let first_staff_top_px = stave_tops[0] - 3.0 * line_spacing;
    let first_staff_y_top = (page_height_pts - first_staff_top_px * pts_per_px).clamp(0.0, page_height_pts);
    PageBands::Systems { systems, first_staff_y_top }
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
