//! Rasterize a monospace TTF into a reusable Lux grayscale atlas.
//!
//! This is the host-side half of Alacritty-style text: FreeType-quality
//! coverage (via fontdue) baked into an 8-bit atlas that the Lux blit
//! library alpha-composites onto a B8G8R8X8 framebuffer.

use std::path::{Path, PathBuf};

fn main() {
    let mut font_path = PathBuf::from("/usr/share/fonts/Adwaita/AdwaitaMono-Regular.ttf");
    let mut px = 20.0f32;
    let mut out = PathBuf::from("lib/font_atlas.lux");
    let mut preview = PathBuf::from("lib/font_preview.ppm");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--font" => font_path = PathBuf::from(args.next().expect("--font needs a path")),
            "--px" => px = args.next().expect("--px needs a value").parse().unwrap(),
            "--out" => out = PathBuf::from(args.next().expect("--out needs a path")),
            "--preview" => preview = PathBuf::from(args.next().expect("--preview needs a path")),
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let font_bytes = std::fs::read(&font_path).unwrap_or_else(|e| {
        panic!("reading {}: {e}", font_path.display());
    });
    let font = fontdue::Font::from_bytes(font_bytes.as_slice(), fontdue::FontSettings::default())
        .expect("parse font");
    let line = font
        .horizontal_line_metrics(px)
        .expect("font has horizontal metrics");
    let ascent = line.ascent.ceil() as i32;
    let descent = line.descent.floor() as i32;
    let gap = line.line_gap.round() as i32;
    let mut cell_w = 1i32;
    let mut cell_h = (ascent - descent + gap).max(1);

    const FIRST: u8 = 32;
    const LAST: u8 = 126;
    let mut glyphs: Vec<(u8, fontdue::Metrics, Vec<u8>)> = Vec::new();
    for ch in FIRST..=LAST {
        let (metrics, bitmap) = font.rasterize(ch as char, px);
        cell_w = cell_w.max(metrics.advance_width.ceil() as i32);
        let top = ascent - metrics.height as i32 - metrics.ymin;
        let bottom = top + metrics.height as i32;
        cell_h = cell_h.max(bottom.max(ascent - descent));
        glyphs.push((ch, metrics, bitmap));
    }

    let cell_w = cell_w as usize;
    let cell_h = cell_h as usize;
    let glyph_bytes = cell_w * cell_h;
    let mut atlas = vec![0u8; glyph_bytes * 96];

    for (slot, (_ch, metrics, bitmap)) in glyphs.iter().enumerate() {
        blit_glyph(
            &mut atlas[slot * glyph_bytes..(slot + 1) * glyph_bytes],
            cell_w,
            cell_h,
            ascent,
            metrics,
            bitmap,
        );
    }
    blit_missing(&mut atlas[95 * glyph_bytes..], cell_w, cell_h);

    let mut ink = 0usize;
    let mut ink_cov = 0u8;
    let a = &atlas[glyph_bytes * (b'A' - FIRST) as usize..][..glyph_bytes];
    for (i, &c) in a.iter().enumerate() {
        if c > ink_cov {
            ink_cov = c;
            ink = i;
        }
    }

    write_lux(
        &out,
        &font_path,
        px,
        cell_w,
        cell_h,
        ascent,
        ink,
        &atlas,
    );
    let tables = out
        .parent()
        .map(|p| p.join("gpu_text_tables.lux"))
        .unwrap_or_else(|| PathBuf::from("lib/gpu_text_tables.lux"));
    write_gpu_tables(&tables, cell_w, cell_h, 40, 25, &atlas);
    write_preview(&preview, cell_w, cell_h, &atlas);
    println!(
        "font-atlas: {} px={} cell={}x{} ascent={} atlas={} bytes ink@{}={}",
        font_path.display(),
        px,
        cell_w,
        cell_h,
        ascent,
        atlas.len(),
        ink,
        ink_cov
    );
    println!("wrote {} and {}", out.display(), preview.display());
}

fn blit_glyph(
    dest: &mut [u8],
    cell_w: usize,
    cell_h: usize,
    ascent: i32,
    metrics: &fontdue::Metrics,
    bitmap: &[u8],
) {
    let dst_x = metrics.xmin;
    let dst_y = ascent - metrics.height as i32 - metrics.ymin;
    for gy in 0..metrics.height {
        for gx in 0..metrics.width {
            let x = dst_x + gx as i32;
            let y = dst_y + gy as i32;
            if x < 0 || y < 0 || x >= cell_w as i32 || y >= cell_h as i32 {
                continue;
            }
            let cov = bitmap[gy * metrics.width + gx];
            let i = y as usize * cell_w + x as usize;
            dest[i] = dest[i].max(cov);
        }
    }
}

fn blit_missing(dest: &mut [u8], cell_w: usize, cell_h: usize) {
    if cell_w < 4 || cell_h < 4 {
        return;
    }
    for y in 1..cell_h - 1 {
        for x in 1..cell_w - 1 {
            let edge = y == 1 || y == cell_h - 2 || x == 1 || x == cell_w - 2;
            dest[y * cell_w + x] = if edge { 200 } else { 0 };
        }
    }
}

fn write_lux(
    path: &Path,
    font_path: &Path,
    px: f32,
    cell_w: usize,
    cell_h: usize,
    ascent: i32,
    ink: usize,
    atlas: &[u8],
) {
    let mut padded = atlas.to_vec();
    while padded.len() % 4 != 0 {
        padded.push(0);
    }
    let mut body = String::new();
    body.push_str("// Generated by tools/font-atlas. Do not edit by hand.\n");
    body.push_str("// 8-bit grayscale coverage, one glyph per cell, ASCII 32..=126 then a replacement box.\n");
    body.push_str(&format!(
        "// source {} at {}px\n",
        font_path.display(),
        px
    ));
    body.push_str("mod font_atlas\n\n");
    body.push_str(&format!("fn font_cell_width() -> Int {{ {cell_w} }}\n\n"));
    body.push_str(&format!("fn font_cell_height() -> Int {{ {cell_h} }}\n\n"));
    body.push_str(&format!("fn font_ascent() -> Int {{ {ascent} }}\n\n"));
    body.push_str(&format!(
        "fn font_px() -> Int {{ {} }}\n\n",
        px.round() as i32
    ));
    body.push_str(&format!("fn font_ink_sample() -> Int {{ {ink} }}\n\n"));
    body.push_str("fn font_atlas() -> String {\n    <<\n");
    let words: Vec<u32> = padded
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    for (i, word) in words.iter().enumerate() {
        if i % 8 == 0 {
            if i > 0 {
                body.push('\n');
            }
            body.push_str("        ");
        } else {
            body.push(' ');
        }
        body.push_str(&format!("0x{word:08X}:32"));
        if i + 1 != words.len() {
            body.push(',');
        }
    }
    body.push_str("\n    >>\n}\n");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn write_words(body: &mut String, bytes: &[u8]) {
    let mut padded = bytes.to_vec();
    while padded.len() % 4 != 0 {
        padded.push(0);
    }
    let words: Vec<u32> = padded
        .chunks_exact(4)
        .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    for (i, word) in words.iter().enumerate() {
        if i % 8 == 0 {
            if i > 0 {
                body.push('\n');
            }
            body.push_str("        ");
        } else {
            body.push(' ');
        }
        body.push_str(&format!("0x{word:08X}:32"));
        if i + 1 != words.len() {
            body.push(',');
        }
    }
}

fn pack_atlas_2d(cell_w: usize, cell_h: usize, atlas: &[u8]) -> Vec<u8> {
    let width = 96 * cell_w;
    let mut dest = vec![0u8; width * cell_h];
    for glyph in 0..96 {
        let src = &atlas[glyph * cell_w * cell_h..];
        for y in 0..cell_h {
            let src_off = y * cell_w;
            let dst_off = y * width + glyph * cell_w;
            dest[dst_off..dst_off + cell_w].copy_from_slice(&src[src_off..src_off + cell_w]);
        }
    }
    dest
}

fn write_gpu_tables(path: &Path, cell_w: usize, cell_h: usize, cols: i32, rows: i32, atlas: &[u8]) {
    let width = cols * cell_w as i32;
    let height = rows * cell_h as i32;
    let mut ndc_x = Vec::new();
    for col in 0..=cols {
        let px = (col * cell_w as i32) as f32;
        ndc_x.extend_from_slice(&((px / width as f32) * 2.0 - 1.0).to_bits().to_le_bytes());
    }
    let mut ndc_y = Vec::new();
    for row in 0..=rows {
        let py = (row * cell_h as i32) as f32;
        ndc_y.extend_from_slice(&(1.0 - (py / height as f32) * 2.0).to_bits().to_le_bytes());
    }
    let mut u_table = Vec::new();
    for slot in 0..=96 {
        u_table.extend_from_slice(&((slot as f32) / 96.0).to_bits().to_le_bytes());
    }
    const VGA: [[u8; 3]; 16] = [
        [0, 0, 0],
        [170, 0, 0],
        [0, 170, 0],
        [170, 170, 0],
        [0, 0, 170],
        [170, 0, 170],
        [0, 170, 170],
        [170, 170, 170],
        [85, 85, 85],
        [255, 85, 85],
        [85, 255, 85],
        [255, 255, 85],
        [85, 85, 255],
        [255, 85, 255],
        [85, 255, 255],
        [255, 255, 255],
    ];
    let mut colors = Vec::new();
    for rgb in VGA {
        for channel in rgb {
            colors.extend_from_slice(&((channel as f32) / 255.0).to_bits().to_le_bytes());
        }
        colors.extend_from_slice(&1.0f32.to_bits().to_le_bytes());
    }
    let atlas_2d = pack_atlas_2d(cell_w, cell_h, atlas);
    let mut atlas_bgra = Vec::with_capacity(atlas_2d.len() * 4);
    for &cov in &atlas_2d {
        atlas_bgra.extend_from_slice(&[cov, cov, cov, 255]);
    }

    let mut body = String::new();
    body.push_str("// Generated by tools/font-atlas. Do not edit by hand.\n");
    body.push_str("// GPU compositor tables: 2D coverage atlas + IEEE-754 clip/UV/color bits.\n");
    body.push_str("mod gpu_text_tables\n\n");
    body.push_str(&format!(
        "fn font_atlas_2d_width() -> Int {{ {} }}\n\n",
        96 * cell_w
    ));
    body.push_str(&format!("fn font_atlas_2d_height() -> Int {{ {cell_h} }}\n\n"));
    body.push_str("fn font_atlas_2d() -> String {\n    <<\n");
    write_words(&mut body, &atlas_bgra);
    body.push_str("\n    >>\n}\n\n");
    body.push_str("fn gpu_ndc_x_table() -> String {\n    <<\n");
    write_words(&mut body, &ndc_x);
    body.push_str("\n    >>\n}\n\n");
    body.push_str("fn gpu_ndc_y_table() -> String {\n    <<\n");
    write_words(&mut body, &ndc_y);
    body.push_str("\n    >>\n}\n\n");
    body.push_str("fn gpu_u_table() -> String {\n    <<\n");
    write_words(&mut body, &u_table);
    body.push_str("\n    >>\n}\n\n");
    body.push_str("fn gpu_color_table() -> String {\n    <<\n");
    write_words(&mut body, &colors);
    body.push_str("\n    >>\n}\n");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

fn write_preview(path: &Path, cell_w: usize, cell_h: usize, atlas: &[u8]) {
    const SAMPLE: &str = "Yggdrasil Lux Terminal 0123456789\nThe quick brown fox jumps over the lazy dog.\nABCDEFGHIJKLMNOPQRSTUVWXYZ\nabcdefghijklmnopqrstuvwxyz\n{}[]()<>+-*/=.,:;!?@#$%^&*|\\~`\"'";
    let lines: Vec<&str> = SAMPLE.lines().collect();
    let cols = lines.iter().map(|l| l.len()).max().unwrap_or(1);
    let width = cols * cell_w;
    let height = lines.len() * cell_h;
    let mut rgb = vec![0u8; width * height * 3];
    let fg = [236u8, 236, 236];
    let bg = [18u8, 18, 22];
    for (row, line) in lines.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            let slot = if (32..=126).contains(&(ch as u8)) {
                (ch as u8 - 32) as usize
            } else {
                95
            };
            let glyph = &atlas[slot * cell_w * cell_h..];
            let ox = col * cell_w;
            let oy = row * cell_h;
            for y in 0..cell_h {
                for x in 0..cell_w {
                    let cov = glyph[y * cell_w + x] as u32;
                    let mix = |f: u8, b: u8| -> u8 {
                        let v = f as u32 * cov + b as u32 * (255 - cov);
                        ((v + 1 + (v >> 8)) >> 8) as u8
                    };
                    let i = ((oy + y) * width + (ox + x)) * 3;
                    rgb[i] = mix(fg[0], bg[0]);
                    rgb[i + 1] = mix(fg[1], bg[1]);
                    rgb[i + 2] = mix(fg[2], bg[2]);
                }
            }
        }
    }
    let mut ppm = format!("P6\n{width} {height}\n255\n").into_bytes();
    ppm.extend_from_slice(&rgb);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, ppm).unwrap();
}
