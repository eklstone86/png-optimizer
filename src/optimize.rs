//! The optimization pipeline for a single file:
//! decode (PNG, or JPG/GIF/WebP/… converted to PNG) → quantize (libimagequant) → repair transparency
//! → recompress (oxipng) → verify → write.

use std::fs::{self, FileTimes, OpenOptions};
use std::io::Cursor;
use std::os::windows::fs::FileTimesExt;
use std::path::{Path, PathBuf};

use oxipng::{Deflater, StripChunks, ZopfliOptions};

use crate::alpha;

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Level {
    Fast,
    Maximum,
    // Settings saved while an ECT-based "Ultra" level existed load as Extreme.
    #[serde(alias = "Ultra")]
    Extreme,
}

impl Level {
    pub const ALL: [Level; 3] = [Level::Fast, Level::Maximum, Level::Extreme];

    pub fn label(self) -> &'static str {
        match self {
            Level::Fast => "Fast",
            Level::Maximum => "Maximum",
            Level::Extreme => "Extreme (Zopfli, very slow)",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Settings {
    /// 0 = lossless only; 2..=256 = fixed palette size; `AUTO_BASE + q` = auto (fewest colors reaching quality q).
    pub colors: u32,
    pub dither: bool,
    pub level: Level,
    pub keep_date: bool,
    /// false: write an optimized copy next to the original (name_opt.png).
    pub overwrite: bool,
}

/// `Settings::colors` values from here up mean "auto": the quality target is `colors - AUTO_BASE` (0..=100).
pub const AUTO_BASE: u32 = 1000;

pub fn auto_quality(colors: u32) -> Option<u8> {
    colors.checked_sub(AUTO_BASE).map(|q| q.min(100) as u8)
}

pub enum Outcome {
    /// `converted`: the input was not a PNG and a new .png was written next to it.
    /// `first_frame`: the input was animated; only its first frame was kept.
    /// `colors`: distinct colors actually present in the kept file (fully transparent pixels count as one);
    /// `quantized`: the palette was reduced (otherwise the result is lossless).
    Optimized { before: u64, after: u64, note: &'static str, output: PathBuf, converted: bool, first_frame: bool, colors: usize, quantized: bool },
    AlreadyOptimal { size: u64, colors: usize },
    Skipped { size: u64, colors: usize },
}

struct Decoded {
    rgba: Vec<imagequant::RGBA>,
    width: usize,
    height: usize,
}

impl Decoded {
    fn alpha(&self) -> Vec<u8> {
        self.rgba.iter().map(|p| p.a).collect()
    }
}

/// File extensions the app accepts. Everything but PNG is converted to PNG.
pub const EXTENSIONS: [&str; 12] = ["png", "jpg", "jpeg", "jfif", "gif", "webp", "bmp", "tif", "tiff", "tga", "ico", "qoi"];

pub fn is_png(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"))
}

fn decode(data: &[u8]) -> Result<Decoded, String> {
    let bmp = lodepng::decode32(data).map_err(|e| format!("decode: {e}"))?;
    Ok(Decoded { rgba: bmp.buffer, width: bmp.width, height: bmp.height })
}

/// Decodes any supported non-PNG format to 8-bit RGBA, applying the EXIF orientation (camera JPEGs).
/// Returns whether the input is animated (only the first frame is decoded).
fn decode_other(path: &Path, data: &[u8]) -> Result<(Decoded, bool), String> {
    use image::{AnimationDecoder, ImageDecoder, ImageFormat};
    let e = |e: image::ImageError| format!("decode: {e}");
    let mut reader = image::ImageReader::new(Cursor::new(data)).with_guessed_format().map_err(|e| format!("decode: {e}"))?;
    if reader.format().is_none() {
        // Formats without a signature (TGA) are identified by their extension.
        if let Ok(f) = ImageFormat::from_path(path) {
            reader.set_format(f);
        }
    }
    let format = reader.format();
    let mut decoder = reader.into_decoder().map_err(e)?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(e)?;
    img.apply_orientation(orientation);
    let rgba = img.to_rgba8();
    let (width, height) = (rgba.width() as usize, rgba.height() as usize);
    let rgba = rgba.pixels().map(|p| imagequant::RGBA::new(p[0], p[1], p[2], p[3])).collect();

    let animated = match format {
        Some(ImageFormat::Gif) => image::codecs::gif::GifDecoder::new(Cursor::new(data))
            .map(|d| d.into_frames().take(2).count() > 1)
            .unwrap_or(false),
        Some(ImageFormat::WebP) => image::codecs::webp::WebPDecoder::new(Cursor::new(data))
            .map(|d| d.has_animation())
            .unwrap_or(false),
        _ => false,
    };
    Ok((Decoded { rgba, width, height }, animated))
}

pub fn process(path: &Path, s: &Settings) -> Result<Outcome, String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let meta = fs::metadata(path).map_err(|e| e.to_string())?;
    let before = data.len() as u64;

    // Non-PNG input is converted: the source stays, a new .png is written next to it.
    let converted = !is_png(path);
    let (original, first_frame) = if converted { decode_other(path, &data)? } else { (decode(&data)?, false) };
    let alpha = original.alpha();

    // Lossy first; if transparency can't be guaranteed, fall back to lossless; else leave the file alone.
    let mut result: Option<(Vec<u8>, &'static str, usize)> = None;
    let mut quantized = false;
    if s.colors > 0 {
        if let Some((png, fixed, _)) = quantize(&original, &alpha, s)? {
            let png = recompress(&png, s.level)?;
            if let Some(colors) = verified(&alpha, &png)? {
                quantized = true;
                result = Some((png, if fixed { " · alpha fixed" } else { "" }, colors));
            }
        }
    }
    if result.is_none() {
        let source_png = if converted {
            lodepng::encode32(&original.rgba, original.width, original.height).map_err(|e| format!("encode: {e}"))?
        } else {
            data.clone()
        };
        let png = recompress(&source_png, s.level)?;
        if let Some(colors) = verified(&alpha, &png)? {
            result = Some((png, if s.colors > 0 { " · lossless (alpha guard)" } else { "" }, colors));
        }
    }
    let Some((png, note, colors)) = result else {
        return Ok(Outcome::Skipped { size: before, colors: count_colors(&original.rgba) });
    };

    let after = png.len() as u64;
    if converted {
        // Always written, even if bigger than the source (a PNG of a photo usually is).
        let output = reserve_png_path(path).map_err(|e| e.to_string())?;
        if let Err(e) = write_replace(&output, &png, s.keep_date.then_some(&meta)) {
            let _ = fs::remove_file(&output);
            return Err(e);
        }
        return Ok(Outcome::Optimized { before, after, note, output, converted, first_frame, colors, quantized });
    }
    if after >= before {
        return Ok(Outcome::AlreadyOptimal { size: before, colors: count_colors(&original.rgba) });
    }
    let output = if s.overwrite { path.to_path_buf() } else { copy_path(path) };
    write_replace(&output, &png, s.keep_date.then_some(&meta))?;
    Ok(Outcome::Optimized { before, after, note, output, converted, first_frame, colors, quantized })
}

/// Returns an indexed PNG, whether transparency had to be repaired, and the palette size.
/// None if the palette had no room for a transparent entry.
fn quantize(img: &Decoded, alpha: &[u8], s: &Settings) -> Result<Option<(Vec<u8>, bool, usize)>, String> {
    let has_transparent = alpha.contains(&0);
    let e = |e: imagequant::Error| format!("quantize: {e}");

    let mut attr = imagequant::new();
    attr.set_speed(1).map_err(e)?;
    match auto_quality(s.colors) {
        // Auto: libimagequant uses the fewest colors (up to 256) that still reach the target quality;
        // min 0 means it never gives up, it just uses all 256 when the target is out of reach.
        Some(q) => {
            attr.set_max_colors(256).map_err(e)?;
            attr.set_quality(0, q).map_err(e)?;
        }
        None => attr.set_max_colors(s.colors).map_err(e)?,
    }
    let mut image = attr.new_image_borrowed(&img.rgba, img.width, img.height, 0.0).map_err(e)?;
    if has_transparent {
        // Reserve a fully transparent palette slot so the repair step always has a target.
        image.add_fixed_color(imagequant::RGBA::new(0, 0, 0, 0)).map_err(e)?;
    }
    let mut res = attr.quantize(&mut image).map_err(e)?;
    res.set_dithering_level(if s.dither { 1.0 } else { 0.0 }).map_err(e)?;
    let (mut palette, mut indices) = res.remapped(&mut image).map_err(e)?;

    let (fixed, ok) = alpha::fix_palette(alpha, &mut palette, &mut indices);
    if !ok {
        return Ok(None);
    }

    let mut enc = lodepng::Encoder::new();
    enc.set_auto_convert(false);
    let set_palette_mode = |mode: &mut lodepng::ColorMode| -> Result<(), String> {
        mode.set_colortype(lodepng::ColorType::PALETTE);
        mode.set_bitdepth(8);
        mode.set_palette(&palette).map_err(|e| format!("encode: {e}"))
    };
    set_palette_mode(enc.info_raw_mut())?;
    set_palette_mode(&mut enc.info_png_mut().color)?;
    let png = enc.encode(&indices, img.width, img.height).map_err(|e| format!("encode: {e}"))?;
    Ok(Some((png, fixed, palette.len())))
}

/// Lossless recompression. Each level is never worse than the one below it:
/// Zopfli loses to libdeflate on some images (different filter choice), so Extreme keeps the smaller.
fn recompress(png: &[u8], level: Level) -> Result<Vec<u8>, String> {
    let run = |preset: u8, zopfli: bool| {
        let mut o = oxipng::Options::from_preset(preset);
        o.strip = StripChunks::Safe;
        o.optimize_alpha = true; // only rewrites the RGB of fully transparent pixels
        if zopfli {
            o.deflater = Deflater::Zopfli(ZopfliOptions::default());
        }
        oxipng::optimize_from_memory(png, &o).map_err(|e| format!("oxipng: {e}"))
    };
    let smaller = |a: Vec<u8>, b: Vec<u8>| if b.len() < a.len() { b } else { a };

    if level == Level::Fast {
        return run(2, false);
    }
    let best = run(6, false)?;
    if level == Level::Maximum {
        return Ok(best);
    }
    Ok(smaller(best, run(6, true)?))
}

/// Checks transparency against the original; on success returns the result's color count.
fn verified(original_alpha: &[u8], png: &[u8]) -> Result<Option<usize>, String> {
    let img = decode(png)?;
    Ok(alpha::verify(original_alpha, &img.alpha()).then(|| count_colors(&img.rgba)))
}

/// Distinct RGBA colors; every fully transparent pixel counts as the same color.
pub fn count_colors(rgba: &[imagequant::RGBA]) -> usize {
    let mut seen = std::collections::HashSet::with_capacity(1024);
    for p in rgba {
        let key = if p.a == 0 { 0 } else { u32::from_le_bytes([p.r, p.g, p.b, p.a]) };
        seen.insert(key);
    }
    seen.len()
}

/// Claims a new, unused `name.png` next to a converted source (`name (2).png`, … if taken).
/// `create_new` makes the claim atomic, so parallel conversions of photo.jpg and photo.gif
/// can't pick the same name, and an existing file is never overwritten.
fn reserve_png_path(src: &Path) -> std::io::Result<PathBuf> {
    let stem = src.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    for n in 1.. {
        let name = if n == 1 { format!("{stem}.png") } else { format!("{stem} ({n}).png") };
        let candidate = src.with_file_name(name);
        match OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    unreachable!()
}

/// Where the optimized copy goes when originals are kept: picture.png → picture_opt.png.
pub fn copy_path(path: &Path) -> PathBuf {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!("{stem}_opt.png"))
}

/// Writes to a temp file beside the target, then atomically moves it into place.
fn write_replace(path: &Path, png: &[u8], keep_times: Option<&fs::Metadata>) -> Result<(), String> {
    let tmp = path.with_extension("png.pngopt-tmp");
    let io = |e: std::io::Error| e.to_string();
    fs::write(&tmp, png).map_err(io)?;

    let finish = || -> std::io::Result<()> {
        if let Some(meta) = keep_times {
            let mut times = FileTimes::new().set_modified(meta.modified()?);
            if let Ok(created) = meta.created() {
                times = times.set_created(created);
            }
            OpenOptions::new().write(true).open(&tmp)?.set_times(times)?;
        }
        if let Ok(meta) = fs::metadata(path) {
            let mut perms = meta.permissions();
            if perms.readonly() {
                #[allow(clippy::permissions_set_readonly_false)]
                perms.set_readonly(false);
                fs::set_permissions(path, perms)?;
            }
        }
        fs::rename(&tmp, path)
    };
    finish().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Transparent background with garbage RGB, a soft-edged disc and barely visible specks (alpha 1–5):
    /// raw libimagequant merges those specks with alpha 0 and makes the whole background alpha 1.
    fn trap_png() -> Vec<u8> {
        let (w, h) = (256usize, 256usize);
        let mut seed = 7u32;
        let mut rnd = |n: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) % n
        };
        let mut px = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                let d = (((x as f64 - 128.0).powi(2) + (y as f64 - 128.0).powi(2)) as f64).sqrt();
                let mut a = if d < 70.0 { 255 } else if d < 110.0 { (255.0 * (110.0 - d) / 40.0) as u8 } else { 0 };
                if a == 0 && rnd(20) == 0 {
                    a = 1 + rnd(5) as u8;
                }
                px.push(imagequant::RGBA::new(rnd(256) as u8, x as u8, y as u8, a));
            }
        }
        lodepng::encode32(&px, w, h).unwrap()
    }

    #[test]
    fn transparency_survives_every_setting() {
        let dir = std::env::temp_dir().join("pngopt_rust_test");
        fs::create_dir_all(&dir).unwrap();
        let source = trap_png();
        let original_alpha = decode(&source).unwrap().alpha();

        for colors in [256, 64, 16, 4, 2] {
            for dither in [true, false] {
                let path = dir.join(format!("trap_{colors}_{dither}.png"));
                fs::write(&path, &source).unwrap();
                let s = Settings { colors, dither, level: Level::Fast, keep_date: false, overwrite: true };
                let outcome = process(&path, &s).unwrap();
                let result = fs::read(&path).unwrap();
                let alpha = decode(&result).unwrap().alpha();
                let leaks = original_alpha.iter().zip(&alpha).filter(|&(&a, &b)| a == 0 && b != 0).count();
                let note = match outcome {
                    Outcome::Optimized { note, .. } => note,
                    Outcome::AlreadyOptimal { .. } => "already optimal",
                    Outcome::Skipped { .. } => "skipped",
                };
                println!("{colors:>3} colors dither={dither:<5} {:>7} -> {:>6} bytes  leaks={leaks}  {note}", source.len(), result.len());
                assert_eq!(leaks, 0, "{colors} colors, dither={dither}");
                assert!(result.len() < source.len());
            }
        }
    }

    #[test]
    fn converts_other_formats() {
        use image::{ImageFormat, RgbaImage};
        let dir = std::env::temp_dir().join(format!("pngopt_conv_{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        // Opaque gradient with a transparent hole: alpha-capable formats must keep the hole transparent.
        let (w, h) = (96u32, 64u32);
        let img = RgbaImage::from_fn(w, h, |x, y| {
            let hole = (40..56).contains(&x) && (20..44).contains(&y);
            image::Rgba([(x * 2) as u8, (y * 3) as u8, 128, if hole { 0 } else { 255 }])
        });
        let formats = [
            ("jpg", ImageFormat::Jpeg, false),
            ("gif", ImageFormat::Gif, true),
            ("webp", ImageFormat::WebP, true),
            ("bmp", ImageFormat::Bmp, true),
            ("tiff", ImageFormat::Tiff, true),
            ("tga", ImageFormat::Tga, true),
            ("ico", ImageFormat::Ico, true),
            ("qoi", ImageFormat::Qoi, true),
        ];
        let s = Settings { colors: 256, dither: true, level: Level::Fast, keep_date: false, overwrite: true };
        for (ext, format, has_alpha) in formats {
            let src = dir.join(format!("pic_{ext}.{ext}"));
            if has_alpha {
                img.save_with_format(&src, format).unwrap();
            } else {
                image::DynamicImage::ImageRgba8(img.clone()).to_rgb8().save_with_format(&src, format).unwrap();
            }
            let _ = fs::remove_file(dir.join(format!("pic_{ext}.png")));
            let Outcome::Optimized { output, converted, first_frame, .. } = process(&src, &s).unwrap() else {
                panic!("{ext}: not converted");
            };
            assert!(converted && !first_frame, "{ext}");
            assert!(src.exists(), "{ext}: source must be kept");
            assert_eq!(output, dir.join(format!("pic_{ext}.png")), "{ext}");
            let out = decode(&fs::read(&output).unwrap()).unwrap();
            assert_eq!((out.width, out.height), (w as usize, h as usize), "{ext}");
            if has_alpha {
                let src_alpha: Vec<u8> = img.pixels().map(|p| p[3]).collect();
                assert!(alpha::verify(&src_alpha, &out.alpha()), "{ext}: transparency leaked");
            }
            println!("{ext:>4}: {} -> {} bytes", fs::metadata(&src).unwrap().len(), fs::metadata(&output).unwrap().len());
        }

        // An existing PNG with the same name is never overwritten.
        let src = dir.join("clash.bmp");
        img.save_with_format(&src, ImageFormat::Bmp).unwrap();
        fs::write(dir.join("clash.png"), b"not ours").unwrap();
        let _ = fs::remove_file(dir.join("clash (2).png"));
        let Outcome::Optimized { output, .. } = process(&src, &s).unwrap() else { panic!() };
        assert_eq!(output, dir.join("clash (2).png"));
        assert_eq!(fs::read(dir.join("clash.png")).unwrap(), b"not ours");

        // Animated GIF: first frame, flagged.
        let src = dir.join("anim.gif");
        {
            use image::codecs::gif::GifEncoder;
            let mut enc = GifEncoder::new(fs::File::create(&src).unwrap());
            let frames = (0..2).map(|_| image::Frame::new(img.clone()));
            enc.encode_frames(frames).unwrap();
        }
        let _ = fs::remove_file(dir.join("anim.png"));
        let Outcome::Optimized { first_frame, .. } = process(&src, &s).unwrap() else { panic!() };
        assert!(first_frame, "animated gif must be flagged");
    }
    /// Size experiments over a folder: PNGOPT_EXP=<dir> cargo test --release experiment -- --ignored --nocapture
    #[test]
    #[ignore]
    fn experiment() {
        let dir = std::env::var("PNGOPT_EXP").expect("set PNGOPT_EXP");
        let files: Vec<Vec<u8>> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "png"))
            .map(|e| fs::read(e.path()).unwrap())
            .collect();
        let total_in: usize = files.iter().map(|d| d.len()).sum();
        let opts = |strip: StripChunks, zopfli: Option<u64>| {
            let mut o = oxipng::Options::from_preset(6);
            o.strip = strip;
            o.optimize_alpha = true;
            if let Some(i) = zopfli {
                o.deflater = Deflater::Zopfli(ZopfliOptions { iteration_count: std::num::NonZeroU64::new(i).unwrap(), ..Default::default() });
            }
            o
        };
        let run = |label: &str, f: &dyn Fn(&[u8]) -> Vec<u8>| {
            let t = std::time::Instant::now();
            let out: usize = files.iter().map(|d| f(d).len().min(d.len())).sum();
            println!("{label:<44} {out:>9} bytes  {:>6.1}%  {:>6.1}s", 100.0 * out as f64 / total_in as f64, t.elapsed().as_secs_f64());
        };
        println!("{} files, {total_in} bytes", files.len());
        let lossless = |o: oxipng::Options| move |d: &[u8]| oxipng::optimize_from_memory(d, &o).unwrap();
        run("lossless Maximum (libdeflate)", &lossless(opts(StripChunks::Safe, None)));
        run("lossless Extreme (Zopfli 15 it.)", &lossless(opts(StripChunks::Safe, Some(15))));
        run("lossless Extreme + strip ALL metadata", &lossless(opts(StripChunks::All, Some(15))));
        let twice = opts(StripChunks::Safe, Some(15));
        run("lossless Extreme, second pass on its output", &|d: &[u8]| {
            let once = oxipng::optimize_from_memory(d, &twice).unwrap();
            oxipng::optimize_from_memory(&once, &twice).unwrap()
        });
        for (colors, dither) in [(256, true)] {
            let s = Settings { colors, dither, level: Level::Extreme, keep_date: false, overwrite: true };
            let label = match auto_quality(colors) {
                Some(q) => format!("auto quality {q}, dither={dither}"),
                None => format!("{colors} colors, dither={dither}"),
            };
            run(&label, &|d: &[u8]| {
                let img = decode(d).unwrap();
                let a = img.alpha();
                match quantize(&img, &a, &s).unwrap() {
                    Some((png, _, n)) => {
                        println!("    palette: {n} colors");
                        recompress(&png, Level::Fast).unwrap()
                    }
                    None => d.to_vec(),
                }
            });
        }
        // Quantizing an already-quantized result again.
        let s = Settings { colors: 256, dither: true, level: Level::Extreme, keep_date: false, overwrite: true };
        run("256 colors, then quantized a second time", &|d: &[u8]| {
            let q = |d: &[u8]| {
                let img = decode(d).unwrap();
                let a = img.alpha();
                quantize(&img, &a, &s).unwrap().map(|(p, _, _)| recompress(&p, Level::Extreme).unwrap()).unwrap_or(d.to_vec())
            };
            q(&q(d))
        });
    }
    #[test]
    fn auto_picks_fewer_colors_for_simple_images() {
        // A flat 6-color image: auto must not spend anywhere near 256 colors on it.
        let (w, h) = (64usize, 64usize);
        let flat: Vec<imagequant::RGBA> = (0..w * h)
            .map(|i| {
                let k = ((i % w) / 11) as u8;
                imagequant::RGBA::new(k * 40, 255 - k * 40, 100, 255)
            })
            .collect();
        let img = Decoded { rgba: flat, width: w, height: h };
        let a = img.alpha();
        for q in [90, 75, 60] {
            let s = Settings { colors: AUTO_BASE + q, dither: true, level: Level::Fast, keep_date: false, overwrite: true };
            let (_, _, colors) = quantize(&img, &a, &s).unwrap().unwrap();
            println!("auto quality {q}: {colors} colors");
            assert!(colors <= 16, "quality {q}: {colors} colors for a 6-color image");
        }
        // A smooth gradient needs many more colors at high quality than at low quality.
        let grad: Vec<imagequant::RGBA> = (0..256 * 64).map(|i| imagequant::RGBA::new((i % 256) as u8, (i / 256 * 4) as u8, 128, 255)).collect();
        let img = Decoded { rgba: grad, width: 256, height: 64 };
        let a = img.alpha();
        let used = |q: u32| {
            let s = Settings { colors: AUTO_BASE + q, dither: true, level: Level::Fast, keep_date: false, overwrite: true };
            quantize(&img, &a, &s).unwrap().unwrap().2
        };
        let (hi, lo) = (used(90), used(60));
        println!("gradient: quality 90 -> {hi} colors, quality 60 -> {lo} colors");
        assert!(lo < hi);
    }

    #[test]
    fn extreme_is_lossless_and_not_worse() {
        let source = trap_png();
        let lossless = recompress(&source, Level::Maximum).unwrap();
        let extreme = recompress(&source, Level::Extreme).unwrap();
        assert!(extreme.len() <= lossless.len());
        let (a, b) = (decode(&source).unwrap(), decode(&extreme).unwrap());
        assert_eq!((a.width, a.height), (b.width, b.height));
        // Every visible pixel identical; fully transparent ones only need to stay transparent.
        for (p, q) in a.rgba.iter().zip(&b.rgba) {
            if p.a == 0 {
                assert_eq!(q.a, 0);
            } else {
                assert_eq!(p, q);
            }
        }
        println!("trap: source {} -> maximum {} -> extreme {}", source.len(), lossless.len(), extreme.len());
    }

    /// Manual diagnosis: PNGOPT_DIAG=<file.png> cargo test --release diagnose -- --ignored --nocapture
    #[test]
    #[ignore]
    fn diagnose() {
        let path = std::env::var("PNGOPT_DIAG").expect("set PNGOPT_DIAG");
        let data = fs::read(&path).unwrap();
        let img = decode(&data).unwrap();
        let alpha = img.alpha();
        let mut hist = [0usize; 5];
        for &a in &alpha {
            hist[match a { 0 => 0, 1..=15 => 1, 16..=127 => 2, 128..=254 => 3, _ => 4 }] += 1;
        }
        println!("{}x{}  alpha: 0={} 1-15={} 16-127={} 128-254={} 255={}", img.width, img.height, hist[0], hist[1], hist[2], hist[3], hist[4]);
        for colors in [256, 64, 16, 8, 4] {
            for dither in [true, false] {
                let s = Settings { colors, dither, level: Level::Fast, keep_date: false, overwrite: true };
                let Some((png, fixed, _)) = quantize(&img, &alpha, &s).unwrap() else {
                    println!("{colors:>3} dither={dither:<5} no room for transparent entry");
                    continue;
                };
                let out = decode(&png).unwrap().alpha();
                let (mut leak, mut lost, mut max_lost) = (0, 0, 0u8);
                for (&a, &b) in alpha.iter().zip(&out) {
                    if a == 0 && b != 0 { leak += 1; }
                    if a >= 128 && b == 0 { lost += 1; max_lost = max_lost.max(a); }
                }
                println!("{colors:>3} dither={dither:<5} fixed={fixed:<5} transparent->visible={leak}  solid->transparent={lost} (max orig alpha {max_lost})  size={}", png.len());
            }
        }
    }
}