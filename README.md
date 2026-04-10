# sheet-music-splitter

Splits a string quartet score PDF into four individual part PDFs (violin1, violin2, viola, cello).

```
cargo run --release -- score.pdf --output-dir output/
```

## How it works

- Rasterizes each page and collapses pixel darkness horizontally into a 1D signal
- FFT on the signal finds the staff line spacing; a comb filter locates each staff's position
- Staves are grouped into systems of 4 and each instrument's region is cropped per system
- PDF CropBox + Form XObjects preserve vector quality (no re-rasterization)
- `libpdfium.dylib` is downloaded automatically on first `cargo build`
