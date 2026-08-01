//! Turn a TrueType font into the bitmaps the UEFI application draws with.
//!
//! The firmware side has no filesystem it can rely on and no font engine,
//! so glyphs have to be baked into the binary. This tool does the baking on
//! the host and writes a generated Rust source file; the result is
//! committed, so building the application needs neither this tool nor a
//! font installed.
//!
//! Two cell sizes come out of one run. 16x32 is what a Steam Deck gets:
//! 1280x800 of usable screen divided by that cell is exactly 80x25, the
//! geometry the menus were already written against. 8x16 covers the smaller
//! framebuffers everything else offers, OVMF's 800x600 included.
//!
//! Coverage is stored at 8 bits per pixel, and each glyph is trimmed to its
//! own bounding box. Trimming matters more than it sounds: a monospace cell
//! is mostly empty, and the boxes cut the baked data roughly in half.
//!
//!     cargo run --release -- \
//!         /usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf \
//!         ../../crates/efigptfix/src/gfx/font_data.rs

use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont};
use std::fmt::Write as _;
use std::path::Path;

/// Printable ASCII. Every string the application renders is ASCII by
/// construction — the report writers are checked for it — so nothing else
/// needs to be carried.
const FIRST: u8 = 0x20;
const LAST: u8 = 0x7E;

/// Applied to coverage before it is stored.
///
/// Below 1.0 this thickens strokes. `ab_glyph` does not hint, so stems land
/// wherever the outline puts them and a 1-pixel stem routinely comes out as
/// two half-lit pixels; at 8x16 that reads as grey mush. Darkening the
/// partial coverage buys back most of what hinting would have given.
const GAMMA: f32 = 0.72;

struct Baked {
    cell_w: usize,
    cell_h: usize,
    glyphs: Vec<Box_>,
    coverage: Vec<u8>,
}

/// One glyph's trimmed extent within its cell, and where its rows live.
struct Box_ {
    x: u8,
    y: u8,
    w: u8,
    h: u8,
    off: u32,
}

fn bake(font: &FontVec, cell_w: usize, cell_h: usize) -> Baked {
    // Scale so the advance width is exactly one cell. Deriving the scale
    // from the advance rather than from the em size is what makes the grid
    // come out right for any monospace face: the cell is the advance.
    let unit = font.as_scaled(PxScale::from(1.0));
    let advance = unit.h_advance(font.glyph_id('M'));
    let px = cell_w as f32 / advance;
    let scaled = font.as_scaled(PxScale::from(px));

    // Centre the ascent-to-descent band in the cell, so the same text sits
    // at the same height whatever cell size is in use.
    let ink = scaled.ascent() - scaled.descent();
    let baseline = (cell_h as f32 - ink) / 2.0 + scaled.ascent();

    let mut glyphs = Vec::new();
    let mut coverage = Vec::new();
    let mut cell = vec![0u8; cell_w * cell_h];

    for code in FIRST..=LAST {
        cell.iter_mut().for_each(|p| *p = 0);
        let ch = code as char;
        let glyph = font.glyph_id(ch).with_scale_and_position(px, point(0.0, baseline));

        if let Some(outline) = font.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, c| {
                let x = bounds.min.x as i32 + gx as i32;
                let y = bounds.min.y as i32 + gy as i32;
                // Clip rather than trusting the outline to stay inside the
                // advance: several glyphs legitimately overhang it.
                if x < 0 || y < 0 || x >= cell_w as i32 || y >= cell_h as i32 {
                    return;
                }
                let v = (c.clamp(0.0, 1.0).powf(GAMMA) * 255.0).round() as u8;
                let slot = &mut cell[y as usize * cell_w + x as usize];
                *slot = (*slot).max(v);
            });
        }

        glyphs.push(trim(&cell, cell_w, cell_h, &mut coverage));
    }

    Baked { cell_w, cell_h, glyphs, coverage }
}

/// Find the lit part of a cell and append just that to the pool.
fn trim(cell: &[u8], cell_w: usize, cell_h: usize, pool: &mut Vec<u8>) -> Box_ {
    let (mut x0, mut y0, mut x1, mut y1) = (cell_w, cell_h, 0usize, 0usize);
    for y in 0..cell_h {
        for x in 0..cell_w {
            if cell[y * cell_w + x] != 0 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x + 1);
                y1 = y1.max(y + 1);
            }
        }
    }
    if x1 == 0 {
        // Blank, space being the obvious one. Nothing to store.
        return Box_ { x: 0, y: 0, w: 0, h: 0, off: 0 };
    }

    let off = pool.len() as u32;
    for y in y0..y1 {
        pool.extend_from_slice(&cell[y * cell_w + x0..y * cell_w + x1]);
    }
    Box_ { x: x0 as u8, y: y0 as u8, w: (x1 - x0) as u8, h: (y1 - y0) as u8, off }
}

fn emit(out: &mut String, name: &str, baked: &Baked) {
    let Baked { cell_w, cell_h, glyphs, coverage } = baked;

    writeln!(out, "pub static {name}: Font = Font {{").unwrap();
    writeln!(out, "    cell_w: {cell_w},").unwrap();
    writeln!(out, "    cell_h: {cell_h},").unwrap();
    writeln!(out, "    first: {FIRST:#04x},").unwrap();
    writeln!(out, "    glyphs: {name}_GLYPHS,").unwrap();
    writeln!(out, "    coverage: {name}_COVERAGE,").unwrap();
    writeln!(out, "}};\n").unwrap();

    writeln!(out, "static {name}_GLYPHS: &[Glyph] = &[").unwrap();
    for (i, g) in glyphs.iter().enumerate() {
        let ch = (FIRST + i as u8) as char;
        let shown = if ch == '\'' || ch == '\\' { format!("{ch:?}") } else { format!("'{ch}'") };
        writeln!(
            out,
            "    Glyph {{ x: {}, y: {}, w: {}, h: {}, off: {} }}, // {shown}",
            g.x, g.y, g.w, g.h, g.off
        )
        .unwrap();
    }
    writeln!(out, "];\n").unwrap();

    // A byte string rather than an array literal: same bytes, a quarter of
    // the source, and it still diffs line by line.
    writeln!(out, "static {name}_COVERAGE: &[u8] = b\"\\").unwrap();
    for chunk in coverage.chunks(24) {
        out.push_str("    ");
        for b in chunk {
            write!(out, "\\x{b:02x}").unwrap();
        }
        out.push_str("\\\n");
    }
    writeln!(out, "    \";\n").unwrap();
}

/// Render a specimen to a PGM, so a font change can be looked at rather
/// than guessed at. Grey on white, since that is easier to judge on a
/// desktop than the white-on-black the application actually draws.
fn preview(baked: &Baked, dest: &str) {
    let specimen = [
        "efigptfix  Check GPT (read only)",
        "ABCDEFGHIJKLMNOPQRSTUVWXYZ 0123456789",
        "abcdefghijklmnopqrstuvwxyz !\"#$%&'()*+",
        ",-./:;<=>?@[\\]^_`{|}~  1101 => 34.4 GiB",
    ];
    let (cw, ch) = (baked.cell_w, baked.cell_h);
    let cols = specimen.iter().map(|l| l.chars().count()).max().unwrap();
    let (w, h) = (cols * cw, specimen.len() * ch);
    let mut img = vec![255u8; w * h];

    for (row, text) in specimen.iter().enumerate() {
        for (col, c) in text.chars().enumerate() {
            let g = &baked.glyphs[(c as u8 - FIRST) as usize];
            for gy in 0..usize::from(g.h) {
                for gx in 0..usize::from(g.w) {
                    let cov = baked.coverage[g.off as usize + gy * usize::from(g.w) + gx];
                    let x = col * cw + usize::from(g.x) + gx;
                    let y = row * ch + usize::from(g.y) + gy;
                    img[y * w + x] = 255 - cov;
                }
            }
        }
    }

    let mut out = format!("P5\n{w} {h}\n255\n").into_bytes();
    out.extend_from_slice(&img);
    std::fs::write(dest, out).expect("cannot write preview");
    eprintln!("wrote {dest}");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(ttf), Some(dest)) = (args.next(), args.next()) else {
        eprintln!("usage: mkfont <font.ttf> <out.rs> [preview-prefix]");
        std::process::exit(2);
    };
    let specimen_prefix = args.next();

    let bytes = std::fs::read(&ttf).unwrap_or_else(|e| {
        eprintln!("cannot read {ttf}: {e}");
        std::process::exit(1);
    });
    let font = FontVec::try_from_vec(bytes).expect("not a usable TrueType font");

    let small = bake(&font, 8, 16);
    let large = bake(&font, 16, 32);

    let source = Path::new(&ttf).file_name().unwrap().to_string_lossy().into_owned();
    let mut out = String::new();
    writeln!(
        out,
        "//! Baked glyph bitmaps. GENERATED — do not edit by hand.\n\
         //!\n\
         //! Produced by `tools/mkfont` from {source}, gamma {GAMMA}. Regenerate with\n\
         //! `make font`. The licence of the source face is in `docs/FONT-LICENSE`.\n\
         \n\
         use super::font::{{Font, Glyph}};\n"
    )
    .unwrap();
    emit(&mut out, "SMALL", &small);
    emit(&mut out, "LARGE", &large);

    std::fs::write(&dest, out).unwrap_or_else(|e| {
        eprintln!("cannot write {dest}: {e}");
        std::process::exit(1);
    });

    for (name, b) in [("8x16", &small), ("16x32", &large)] {
        if let Some(prefix) = &specimen_prefix {
            preview(b, &format!("{prefix}{name}.pgm"));
        }
        let cells = b.glyphs.len() * b.cell_w * b.cell_h;
        eprintln!(
            "{name}: {} glyphs, {} bytes of coverage ({}% of {cells} untrimmed)",
            b.glyphs.len(),
            b.coverage.len(),
            b.coverage.len() * 100 / cells
        );
    }
    eprintln!("wrote {dest}");
}
