use std::io::Cursor;
use std::time::Instant;

use image::{DynamicImage, ImageReader, Limits};
use libwebp_sys::WebPImageHint;
use webp::{Encoder as WebPEncoder, WebPConfig};

/// Maximum input image dimensions accepted for decode.
/// Prevents decompression-bomb attacks that could exhaust memory/CPU.
const MAX_INPUT_WIDTH: u32 = 16_384;
const MAX_INPUT_HEIGHT: u32 = 16_384;
/// Maximum memory budget for a single decoded image (512 MiB).
const MAX_DECODE_BYTES: u64 = 512 * 1024 * 1024;

pub struct ConvertParams {
    pub max_width: u32,
    pub max_height: u32,
    pub quality: u32,
    pub effort: u32,
}

pub struct ConvertResult {
    pub output_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug)]
pub enum ConvertError {
    Decode(String),
    Encode(String),
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConvertError::Decode(msg) => write!(f, "{msg}"),
            ConvertError::Encode(msg) => write!(f, "{msg}"),
        }
    }
}

pub fn convert(image_data: &[u8], params: &ConvertParams) -> Result<ConvertResult, ConvertError> {
    // Instant::now() is a syscall; skip it when DEBUG logging is disabled.
    let debug = tracing::enabled!(tracing::Level::DEBUG);
    let t0 = debug.then(Instant::now);

    let mut reader = ImageReader::new(Cursor::new(image_data))
        .with_guessed_format()
        .map_err(|e| ConvertError::Decode(e.to_string()))?;
    // Limits is non-exhaustive; mutate fields on the default value.
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_INPUT_WIDTH);
    limits.max_image_height = Some(MAX_INPUT_HEIGHT);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let image = reader
        .decode()
        .map_err(|e| ConvertError::Decode(e.to_string()))?;

    let decode_ms = t0.map(|t| t.elapsed().as_millis()).unwrap_or(0);
    let src_w = image.width();
    let src_h = image.height();

    let t1 = debug.then(Instant::now);
    let resized = scale_down(image, params.max_width, params.max_height);
    let resize_ms = t1.map(|t| t.elapsed().as_millis()).unwrap_or(0);

    let width = resized.width();
    let height = resized.height();

    let t2 = debug.then(Instant::now);
    let encoder =
        WebPEncoder::from_image(&resized).map_err(|e| ConvertError::Encode(e.to_string()))?;
    let config = build_webp_config(params)?;
    let webp = encoder
        .encode_advanced(&config)
        .map_err(|e| ConvertError::Encode(format!("{e:?}")))?;
    let encode_ms = t2.map(|t| t.elapsed().as_millis()).unwrap_or(0);

    tracing::debug!(
        src_w,
        src_h,
        dst_w = width,
        dst_h = height,
        decode_ms,
        resize_ms,
        encode_ms,
        "convert breakdown"
    );

    Ok(ConvertResult {
        output_data: webp.to_vec(),
        width,
        height,
    })
}

/// SCALE_DOWN: fit within (max_w, max_h), no upscaling.
/// Uses image::thumbnail — fast integer algorithm, suitable for thumbnailing.
fn scale_down(image: DynamicImage, max_w: u32, max_h: u32) -> DynamicImage {
    if image.width() <= max_w && image.height() <= max_h {
        return image;
    }
    image.thumbnail(max_w, max_h)
}

fn build_webp_config(params: &ConvertParams) -> Result<WebPConfig, ConvertError> {
    let mut config = WebPConfig::new()
        .map_err(|_| ConvertError::Encode("failed to create WebPConfig".to_string()))?;

    config.quality = params.quality as f32;
    config.method = params.effort as i32;
    config.image_hint = WebPImageHint::WEBP_HINT_PHOTO;
    config.sns_strength = 70;
    config.filter_sharpness = 2;
    config.filter_strength = 25;
    config.low_memory = 1;

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_down_no_op_when_fits() {
        let img = DynamicImage::new_rgb8(100, 100);
        let result = scale_down(img, 200, 200);
        assert_eq!(result.width(), 100);
        assert_eq!(result.height(), 100);
    }

    #[test]
    fn scale_down_respects_max_width() {
        let img = DynamicImage::new_rgb8(400, 200);
        let result = scale_down(img, 200, 200);
        assert_eq!(result.width(), 200);
        assert_eq!(result.height(), 100);
    }

    #[test]
    fn scale_down_respects_max_height() {
        let img = DynamicImage::new_rgb8(200, 400);
        let result = scale_down(img, 200, 200);
        assert_eq!(result.width(), 100);
        assert_eq!(result.height(), 200);
    }
}
