mod detect;
mod crop;
mod pipeline;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Split string quartet score PDFs into one PDF per part")]
struct Args {
    /// Input score PDF
    input: PathBuf,

    /// Output directory (created if absent)
    #[arg(long, default_value = "output")]
    output_dir: PathBuf,

    /// Rasterization DPI for staff detection
    #[arg(long, default_value_t = 150)]
    dpi: u32,

    /// Write debug PNGs with detected band boundaries
    #[arg(long)]
    debug: bool,

    /// Minimum padding factor (multiples of staff line spacing) for content-aware
    /// boundary detection between instruments. Higher = more clearance.
    #[arg(long, default_value_t = 2.0)]
    min_padding_factor: f32,
}

fn main() -> Result<()> {
    let args = Args::parse();
    std::fs::create_dir_all(&args.output_dir)?;
    pipeline::run(&args.input, &args.output_dir, args.dpi, args.debug, args.min_padding_factor)
}
