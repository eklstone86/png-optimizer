//! Transparency guard: fully transparent pixels of the original must stay fully transparent.
//!
//! Only that direction is enforced. Visible pixels turning transparent at low color counts
//! (alpha dithering) is ordinary quantization loss the user opted into by picking few colors.

/// True if no fully transparent pixel became visible.
pub fn verify(original: &[u8], optimized: &[u8]) -> bool {
    original.len() == optimized.len() && original.iter().zip(optimized).all(|(&a, &b)| a != 0 || b == 0)
}

/// Points every pixel that was fully transparent in the original at a fully transparent
/// palette entry, adding one if needed. Returns (fixed anything, success).
pub fn fix_palette(
    original_alpha: &[u8],
    palette: &mut Vec<imagequant::RGBA>,
    indices: &mut [u8],
) -> (bool, bool) {
    let leaks = |palette: &[imagequant::RGBA], alpha: u8, index: u8| alpha == 0 && palette[index as usize].a != 0;
    if !original_alpha.iter().zip(indices.iter()).any(|(&a, &ix)| leaks(palette, a, ix)) {
        return (false, true);
    }

    let transparent = match palette.iter().position(|c| c.a == 0) {
        Some(t) => t,
        None if palette.len() < 256 => {
            palette.push(imagequant::RGBA::new(0, 0, 0, 0));
            palette.len() - 1
        }
        None => return (false, false),
    };
    for (ix, &a) in indices.iter_mut().zip(original_alpha) {
        if leaks(palette, a, *ix) {
            *ix = transparent as u8;
        }
    }
    (true, true)
}
