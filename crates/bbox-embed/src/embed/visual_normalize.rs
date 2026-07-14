//! Visual payload normalization (gap-48ae5495): a pure bytes-in/bytes-out
//! transform that brings an image within the multimodal provider's pixel
//! cap and (locally enforced) aspect-ratio range instead of letting it be
//! poison-dropped. Run before `voyage_multimodal::preflight_part` sees the
//! payload (`embed::queue::embed_visual_requests`), on the blocking pool
//! since decode/resize/encode is CPU-bound (concurrency-model I2).
//!
//! Two violations are corrected, each only when the source actually
//! violates it (a conformant image is returned `Unchanged`, never
//! re-encoded):
//!
//! 1. Pixel cap: downscale (preserving aspect ratio) to fit under a
//!    conservative target, safely under the provider's hard cap.
//! 2. Aspect ratio: pad the short side with a white background until the
//!    ratio is back inside the permitted range, then re-check the pixel
//!    cap (padding grows the canvas) and downscale again if needed.
//!
//! Anything this module cannot decode (corrupt bytes, an unsupported
//! format, a video mislabeled as an image) comes back as `Failed`; the
//! caller falls back to the original bytes and lets `preflight_part`
//! poison-reject them the same way it always has. Tiling (splitting one
//! oversize image into multiple embedded chunks) is out of scope: one
//! visual chunk is one embedding.

use image::{DynamicImage, ImageEncoder, ImageFormat};

use super::voyage_multimodal::{MAX_ASPECT_RATIO, MAX_IMAGE_BYTES, MAX_IMAGE_PIXELS};

/// Safety margin under `MAX_IMAGE_PIXELS` so a downscaled image doesn't
/// land right on the provider's boundary after re-encoding (JPEG/PNG
/// encoders don't change pixel count, but this leaves headroom for the
/// padding step, which can grow the canvas again after a downscale).
const TARGET_MAX_PIXELS: u64 = 15_500_000;

/// Pad to just inside `MAX_ASPECT_RATIO`, not exactly on it, so integer
/// rounding in the padding math can never tip the result back over the
/// limit.
const TARGET_ASPECT_RATIO: f64 = 18.5;

/// JPEG re-encode quality. High enough to preserve OCR/retrieval-relevant
/// detail, low enough to meaningfully shrink oversize scans.
const JPEG_QUALITY: u8 = 90;

/// Result of attempting to normalize one image payload. `bytes`/`mime` on
/// every variant (including `Failed`) so the caller never needs to hold a
/// separate copy of the original payload for the fallback path.
#[derive(Debug, Clone, PartialEq)]
pub enum NormalizeOutcome {
    /// The image already satisfies both constraints; `bytes`/`mime` are
    /// the same values passed in, untouched.
    Unchanged { bytes: Vec<u8>, mime: String },
    /// The image violated a constraint and was corrected; `bytes`/`mime`
    /// describe the re-encoded replacement.
    Normalized { bytes: Vec<u8>, mime: String },
    /// Normalization could not run (undecodable bytes, zero-sized image,
    /// re-encode failure); `bytes`/`mime` are the original input, returned
    /// unchanged so the caller can still submit them to preflight.
    Failed {
        bytes: Vec<u8>,
        mime: String,
        reason: String,
    },
}

/// The provider-constraint thresholds a normalization pass targets.
/// Parameterized (rather than reading the module consts directly) so
/// tests can exercise every branch (downscale, pad, pad-then-downscale,
/// PNG-too-big fallback) with tiny synthetic images and tiny thresholds
/// instead of needing multi-megapixel fixtures to trip the real caps.
#[derive(Debug, Clone, Copy)]
struct Limits {
    max_pixels: u64,
    target_pixels: u64,
    max_ratio: f64,
    target_ratio: f64,
    max_bytes: u64,
}

impl Limits {
    fn production() -> Self {
        Self {
            max_pixels: MAX_IMAGE_PIXELS,
            target_pixels: TARGET_MAX_PIXELS,
            max_ratio: MAX_ASPECT_RATIO,
            target_ratio: TARGET_ASPECT_RATIO,
            max_bytes: MAX_IMAGE_BYTES as u64,
        }
    }
}

/// Normalize one image payload against the provider's pixel-cap and
/// aspect-ratio constraints. Pure function: no I/O, no async — callers run
/// it on the blocking pool.
pub fn normalize_image_bytes(bytes: Vec<u8>, mime: String) -> NormalizeOutcome {
    normalize_image_bytes_with_limits(bytes, mime, Limits::production())
}

fn normalize_image_bytes_with_limits(
    bytes: Vec<u8>,
    mime: String,
    limits: Limits,
) -> NormalizeOutcome {
    let format = match image::guess_format(&bytes) {
        Ok(format) => format,
        Err(err) => {
            let reason = format!("could not detect image format: {err}");
            return NormalizeOutcome::Failed {
                bytes,
                mime,
                reason,
            };
        }
    };
    let img = match image::load_from_memory_with_format(&bytes, format) {
        Ok(img) => img,
        Err(err) => {
            let reason = format!("could not decode image: {err}");
            return NormalizeOutcome::Failed {
                bytes,
                mime,
                reason,
            };
        }
    };
    if img.width() == 0 || img.height() == 0 {
        return NormalizeOutcome::Failed {
            bytes,
            mime,
            reason: "image has a zero-length dimension".into(),
        };
    }

    let violates_pixels = pixel_count(&img) > limits.max_pixels;
    let violates_ratio = ratio_of(&img) > limits.max_ratio;
    if !violates_pixels && !violates_ratio {
        return NormalizeOutcome::Unchanged { bytes, mime };
    }

    let mut working = img;
    if pixel_count(&working) > limits.target_pixels {
        working = downscale_to_pixel_budget(working, limits.target_pixels);
    }
    if ratio_of(&working) > limits.max_ratio {
        working = pad_to_aspect_ratio(working, limits.target_ratio);
        // Padding grows the canvas; a near-cap image can cross back over
        // the pixel budget after the short side is padded out.
        if pixel_count(&working) > limits.target_pixels {
            working = downscale_to_pixel_budget(working, limits.target_pixels);
        }
    }

    let prefer_jpeg = matches!(format, ImageFormat::Jpeg);
    match encode_image(&working, prefer_jpeg, limits.max_bytes) {
        Ok((bytes, mime)) => NormalizeOutcome::Normalized { bytes, mime },
        Err(err) => {
            let reason = format!("could not re-encode normalized image: {err}");
            NormalizeOutcome::Failed {
                bytes,
                mime,
                reason,
            }
        }
    }
}

fn pixel_count(img: &DynamicImage) -> u64 {
    img.width() as u64 * img.height() as u64
}

fn ratio_of(img: &DynamicImage) -> f64 {
    let (w, h) = (img.width().max(1) as f64, img.height().max(1) as f64);
    w.max(h) / w.min(h)
}

/// Downscale (uniform scale, aspect-preserving) so the result fits within
/// `target_pixels`. A no-op if already within budget.
fn downscale_to_pixel_budget(img: DynamicImage, target_pixels: u64) -> DynamicImage {
    let (w, h) = (img.width() as f64, img.height() as f64);
    let current = w * h;
    if current <= target_pixels as f64 {
        return img;
    }
    let scale = (target_pixels as f64 / current).sqrt();
    let new_w = ((w * scale).floor() as u32).max(1);
    let new_h = ((h * scale).floor() as u32).max(1);
    img.resize_exact(new_w, new_h, image::imageops::FilterType::Lanczos3)
}

/// Pad the short side with a white background until long:short is within
/// `target_ratio`. Converts to RGB8 (alpha is flattened onto the white
/// background rather than carried through) since the padded canvas is a
/// flat background regardless of the source's original color type.
fn pad_to_aspect_ratio(img: DynamicImage, target_ratio: f64) -> DynamicImage {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return img;
    }
    let (new_w, new_h) = if w >= h {
        let min_h = ((w as f64 / target_ratio).ceil() as u32).max(1);
        (w, h.max(min_h))
    } else {
        let min_w = ((h as f64 / target_ratio).ceil() as u32).max(1);
        (w.max(min_w), h)
    };
    if new_w == w && new_h == h {
        return img;
    }
    let rgb = img.to_rgb8();
    let mut canvas = image::RgbImage::from_pixel(new_w, new_h, image::Rgb([255, 255, 255]));
    let x_off = ((new_w - w) / 2) as i64;
    let y_off = ((new_h - h) / 2) as i64;
    image::imageops::overlay(&mut canvas, &rgb, x_off, y_off);
    DynamicImage::ImageRgb8(canvas)
}

/// Re-encode the working image. JPEG when the source was JPEG (matches
/// the original format so a photo doesn't balloon in size under PNG);
/// PNG otherwise, falling back to JPEG if the PNG result would still
/// violate the provider's byte cap (a padded/downscaled scan can still be
/// large as lossless PNG). `max_bytes` is a parameter (rather than reading
/// `MAX_IMAGE_BYTES` directly) so tests can force the fallback
/// deterministically with a small fixture instead of needing a real
/// >20MB image.
fn encode_image(
    img: &DynamicImage,
    prefer_jpeg: bool,
    max_bytes: u64,
) -> image::ImageResult<(Vec<u8>, String)> {
    if prefer_jpeg {
        return encode_jpeg(img).map(|bytes| (bytes, "image/jpeg".to_string()));
    }
    let png_bytes = encode_png(img)?;
    if (png_bytes.len() as u64) <= max_bytes {
        Ok((png_bytes, "image/png".to_string()))
    } else {
        encode_jpeg(img).map(|bytes| (bytes, "image/jpeg".to_string()))
    }
}

fn encode_jpeg(img: &DynamicImage) -> image::ImageResult<Vec<u8>> {
    let rgb = img.to_rgb8();
    let mut buf = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, JPEG_QUALITY);
    encoder.write_image(
        rgb.as_raw(),
        rgb.width(),
        rgb.height(),
        image::ExtendedColorType::Rgb8,
    )?;
    Ok(buf)
}

fn encode_png(img: &DynamicImage) -> image::ImageResult<Vec<u8>> {
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, ImageFormat::Png)?;
    Ok(buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tiny thresholds so the downscale/pad/pad-then-downscale branches
    /// trip on tiny (single/double-digit pixel) synthetic images instead
    /// of needing real multi-megapixel fixtures. Chosen independent of the
    /// production constants' exact ratios so the test intent stays
    /// readable at these small numbers.
    fn tiny_limits() -> Limits {
        Limits {
            max_pixels: 200,
            target_pixels: 150,
            max_ratio: 5.0,
            target_ratio: 4.0,
            max_bytes: u64::MAX,
        }
    }

    fn encode_test_png(width: u32, height: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            width,
            height,
            image::Rgb([10, 200, 30]),
        ));
        encode_png(&img).unwrap()
    }

    fn encode_test_jpeg(width: u32, height: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            width,
            height,
            image::Rgb([200, 10, 30]),
        ));
        encode_jpeg(&img).unwrap()
    }

    fn decoded_dims(bytes: &[u8]) -> (u32, u32) {
        let decoded = image::load_from_memory(bytes).unwrap();
        (decoded.width(), decoded.height())
    }

    #[test]
    fn conformant_image_passes_through_byte_identical() {
        let bytes = encode_test_png(8, 6);
        let outcome = normalize_image_bytes(bytes.clone(), "image/png".into());
        match outcome {
            NormalizeOutcome::Unchanged { bytes: out, mime } => {
                assert_eq!(out, bytes);
                assert_eq!(mime, "image/png");
            }
            other => panic!("expected Unchanged, got {other:?}"),
        }
    }

    #[test]
    fn conformant_image_under_tiny_limits_is_unchanged() {
        // Same real code path, exercised at tiny scale: 10x10 = 100
        // pixels (under the 200 cap), ratio 1:1 (under the 5.0 cap).
        let bytes = encode_test_png(10, 10);
        let outcome =
            normalize_image_bytes_with_limits(bytes.clone(), "image/png".into(), tiny_limits());
        match outcome {
            NormalizeOutcome::Unchanged { bytes: out, mime } => {
                assert_eq!(out, bytes);
                assert_eq!(mime, "image/png");
            }
            other => panic!("expected Unchanged, got {other:?}"),
        }
    }

    #[test]
    fn oversize_image_is_downscaled_under_the_pixel_cap() {
        // 20x20 = 400 pixels, over the tiny 200-pixel cap; 1:1 ratio stays
        // well under the tiny 5.0 ratio cap, isolating the downscale path.
        let bytes = encode_test_png(20, 20);
        let outcome = normalize_image_bytes_with_limits(bytes, "image/png".into(), tiny_limits());
        match outcome {
            NormalizeOutcome::Normalized { bytes, mime } => {
                let (out_w, out_h) = decoded_dims(&bytes);
                assert!(
                    (out_w as u64) * (out_h as u64) <= tiny_limits().max_pixels,
                    "downscaled image is still over the pixel cap: {out_w}x{out_h}"
                );
                assert_eq!(
                    out_w, out_h,
                    "1:1 source should stay 1:1 after a uniform downscale"
                );
                assert_eq!(mime, "image/png");
            }
            other => panic!("expected Normalized, got {other:?}"),
        }
    }

    #[test]
    fn thin_strip_is_padded_into_the_permitted_aspect_ratio() {
        // 2 wide x 20 tall is a 10:1 ratio, over the tiny 5.0 cap; area is
        // 40 pixels, well under the tiny 200-pixel cap, isolating the pad
        // path from the downscale path.
        let bytes = encode_test_png(2, 20);
        let limits = tiny_limits();
        let outcome = normalize_image_bytes_with_limits(bytes, "image/png".into(), limits);
        match outcome {
            NormalizeOutcome::Normalized { bytes, mime } => {
                let (out_w, out_h) = decoded_dims(&bytes);
                let ratio = out_h.max(out_w) as f64 / out_w.min(out_h) as f64;
                assert!(
                    ratio <= limits.max_ratio,
                    "padded image still violates the aspect ratio cap: {out_w}x{out_h} ratio={ratio}"
                );
                assert!((out_w as u64) * (out_h as u64) <= limits.max_pixels);
                assert_eq!(mime, "image/png");
            }
            other => panic!("expected Normalized, got {other:?}"),
        }
    }

    #[test]
    fn padding_that_would_exceed_the_pixel_cap_is_downscaled_afterward() {
        // 40 wide x 2 tall: area 80 (under the 200-pixel cap, so the
        // pre-pad downscale is skipped) but a 20:1 ratio. Padding the
        // short side out to the tiny 4.0 target ratio grows the canvas to
        // 40x10 = 400 pixels, over the 200-pixel cap, forcing the
        // post-pad downscale branch.
        let bytes = encode_test_png(40, 2);
        let limits = tiny_limits();
        let outcome = normalize_image_bytes_with_limits(bytes, "image/png".into(), limits);
        match outcome {
            NormalizeOutcome::Normalized { bytes, mime } => {
                let (out_w, out_h) = decoded_dims(&bytes);
                let ratio = out_h.max(out_w) as f64 / out_w.min(out_h) as f64;
                assert!(
                    (out_w as u64) * (out_h as u64) <= limits.max_pixels,
                    "still over the pixel cap after the post-pad downscale: {out_w}x{out_h}"
                );
                assert!(
                    ratio <= limits.max_ratio,
                    "still violates the aspect ratio cap after the post-pad downscale: ratio={ratio}"
                );
                assert_eq!(mime, "image/png");
            }
            other => panic!("expected Normalized, got {other:?}"),
        }
    }

    #[test]
    fn png_result_over_the_byte_cap_falls_back_to_jpeg() {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            64,
            64,
            image::Rgb([10, 200, 30]),
        ));
        let png_bytes = encode_png(&img).unwrap();
        assert!(!png_bytes.is_empty());

        let (bytes, mime) = encode_image(&img, false, 8).unwrap();
        assert_eq!(mime, "image/jpeg");
        assert!(!bytes.is_empty());

        // Under a generous cap the same image stays PNG.
        let (bytes, mime) = encode_image(&img, false, u64::MAX).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(bytes, png_bytes);
    }

    #[test]
    fn undecodable_bytes_fail_with_original_bytes_preserved() {
        let bytes = vec![0xFFu8; 32];
        let outcome = normalize_image_bytes(bytes.clone(), "image/png".into());
        match outcome {
            NormalizeOutcome::Failed {
                bytes: out,
                mime,
                reason,
            } => {
                assert_eq!(out, bytes);
                assert_eq!(mime, "image/png");
                assert!(!reason.is_empty());
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn jpeg_source_re_encodes_as_jpeg() {
        // 25x25 = 625 pixels, over the tiny 200-pixel cap.
        let bytes = encode_test_jpeg(25, 25);
        let outcome = normalize_image_bytes_with_limits(bytes, "image/jpeg".into(), tiny_limits());
        match outcome {
            NormalizeOutcome::Normalized { bytes, mime } => {
                assert_eq!(mime, "image/jpeg");
                let (out_w, out_h) = decoded_dims(&bytes);
                assert!((out_w as u64) * (out_h as u64) <= tiny_limits().max_pixels);
            }
            other => panic!("expected Normalized, got {other:?}"),
        }
    }

    #[test]
    fn production_limits_reject_the_same_pixel_cap_as_preflight() {
        // Wiring check: the public entry point uses the real MAX_IMAGE_PIXELS/
        // MAX_ASPECT_RATIO constants shared with voyage_multimodal's
        // preflight, not a hardcoded copy. Exercised structurally (compare
        // the constants a fresh Limits::production() carries) rather than
        // through an actual multi-megapixel fixture, which would make this
        // test slow without adding coverage beyond the tiny-limits tests
        // above (identical code path).
        let limits = Limits::production();
        assert_eq!(limits.max_pixels, MAX_IMAGE_PIXELS);
        assert_eq!(limits.max_ratio, MAX_ASPECT_RATIO);
        assert_eq!(limits.max_bytes, MAX_IMAGE_BYTES as u64);
        assert!(limits.target_pixels < limits.max_pixels);
        assert!(limits.target_ratio < limits.max_ratio);
    }
}
