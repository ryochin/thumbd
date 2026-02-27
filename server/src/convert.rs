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
    pub image_type: i32,
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
    UnsupportedFormat(i32),
}

impl std::fmt::Display for ConvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConvertError::Decode(msg) => write!(f, "{msg}"),
            ConvertError::Encode(msg) => write!(f, "{msg}"),
            ConvertError::UnsupportedFormat(n) => write!(f, "unsupported image type: {n}"),
        }
    }
}

pub fn convert(image_data: &[u8], params: &ConvertParams) -> Result<ConvertResult, ConvertError> {
    // Instant::now() is a syscall; skip it when DEBUG logging is disabled.
    let debug = tracing::enabled!(tracing::Level::DEBUG);
    let t0 = debug.then(Instant::now);

    let is_jpeg = image::guess_format(image_data)
        .map(|fmt| fmt == image::ImageFormat::Jpeg)
        .unwrap_or(false);

    let mut dct_scale: Option<turbojpeg::ScalingFactor> = None;
    let image = if is_jpeg {
        let (img, scale) = jpeg_decode_scaled(image_data, params.max_width, params.max_height)?;
        dct_scale = Some(scale);
        img
    } else {
        let mut reader = ImageReader::new(Cursor::new(image_data))
            .with_guessed_format()
            .map_err(|e| ConvertError::Decode(e.to_string()))?;
        // Limits is non-exhaustive; mutate fields on the default value.
        let mut limits = Limits::default();
        limits.max_image_width = Some(MAX_INPUT_WIDTH);
        limits.max_image_height = Some(MAX_INPUT_HEIGHT);
        limits.max_alloc = Some(MAX_DECODE_BYTES);
        reader.limits(limits);
        reader
            .decode()
            .map_err(|e| ConvertError::Decode(e.to_string()))?
    };

    let decode_ms = t0.map(|t| t.elapsed().as_millis()).unwrap_or(0);
    let src_w = image.width();
    let src_h = image.height();

    let t1 = debug.then(Instant::now);
    let resized = scale_down(image, params.max_width, params.max_height);
    let resize_ms = t1.map(|t| t.elapsed().as_millis()).unwrap_or(0);

    let width = resized.width();
    let height = resized.height();

    let t2 = debug.then(Instant::now);
    // IMAGE_TYPE_UNSPECIFIED (0) falls through to WebP as the default.
    let (output_data, encode_ms) = match params.image_type {
        0 | 1 => {
            let encoder = WebPEncoder::from_image(&resized)
                .map_err(|e| ConvertError::Encode(e.to_string()))?;
            let config = build_webp_config(params)?;
            let webp = encoder
                .encode_advanced(&config)
                .map_err(|e| ConvertError::Encode(format!("{e:?}")))?;
            let encode_ms = t2.map(|t| t.elapsed().as_millis()).unwrap_or(0);
            (webp.to_vec(), encode_ms)
        }
        n => return Err(ConvertError::UnsupportedFormat(n)),
    };

    tracing::debug!(
        src_w,
        src_h,
        dst_w = width,
        dst_h = height,
        dct_scale = dct_scale.map(|s| s.to_string()),
        decode_ms,
        resize_ms,
        encode_ms,
        "convert breakdown"
    );

    Ok(ConvertResult {
        output_data,
        width,
        height,
    })
}

/// デコード後 thumbnail() が有効に機能できる最大の DCT スケール率を選ぶ。
/// src のどちらか一方が max を超えていれば thumbnail が縮小できる。
/// 1/8 → 1/4 → 1/2 → 1/1 の順に試し、条件を満たす最大縮小を返す。
fn choose_scale(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> turbojpeg::ScalingFactor {
    for factor in [
        turbojpeg::ScalingFactor::ONE_EIGHTH,
        turbojpeg::ScalingFactor::ONE_QUARTER,
        turbojpeg::ScalingFactor::ONE_HALF,
        turbojpeg::ScalingFactor::ONE,
    ] {
        let sw = factor.scale(src_w as usize) as u32;
        let sh = factor.scale(src_h as usize) as u32;
        if sw >= max_w || sh >= max_h {
            return factor;
        }
    }
    turbojpeg::ScalingFactor::ONE
}

/// turbojpeg で JPEG を DCT スケーリング付きでデコードする。
/// max_w / max_h から最適なスケール率を自動選択する。
fn jpeg_decode_scaled(
    data: &[u8],
    max_w: u32,
    max_h: u32,
) -> Result<(DynamicImage, turbojpeg::ScalingFactor), ConvertError> {
    let mut dec =
        turbojpeg::Decompressor::new().map_err(|e| ConvertError::Decode(e.to_string()))?;

    let header = dec
        .read_header(data)
        .map_err(|e| ConvertError::Decode(e.to_string()))?;

    // 入力サイズ上限チェック（セキュリティ用）
    if header.width as u32 > MAX_INPUT_WIDTH || header.height as u32 > MAX_INPUT_HEIGHT {
        return Err(ConvertError::Decode(format!(
            "image too large: {}x{}",
            header.width, header.height
        )));
    }

    let scale = choose_scale(header.width as u32, header.height as u32, max_w, max_h);
    dec.set_scaling_factor(scale)
        .map_err(|e| ConvertError::Decode(e.to_string()))?;

    let sw = scale.scale(header.width);
    let sh = scale.scale(header.height);
    let pitch = sw * 3; // RGB, no padding
    let mut buf = vec![0u8; pitch * sh];

    let output = turbojpeg::Image {
        pixels: buf.as_mut_slice(),
        width: sw,
        pitch,
        height: sh,
        format: turbojpeg::PixelFormat::RGB,
    };
    dec.decompress(data, output)
        .map_err(|e| ConvertError::Decode(e.to_string()))?;

    let img_buf = image::ImageBuffer::<image::Rgb<u8>, _>::from_raw(sw as u32, sh as u32, buf)
        .ok_or_else(|| ConvertError::Decode("ImageBuffer::from_raw failed".into()))?;

    Ok((DynamicImage::ImageRgb8(img_buf), scale))
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
    fn convert_jpeg_uses_scaled_decode() {
        // 4000x3000 JPEG を max 640x480 に変換 → 出力が 640x480 以内であること
        let jpeg = make_test_jpeg(4000, 3000);
        let params = ConvertParams {
            image_type: 1,
            max_width: 640,
            max_height: 480,
            quality: 80,
            effort: 3,
        };
        let result = convert(&jpeg, &params).expect("convert failed");
        assert!(result.width <= 640, "width {} > 640", result.width);
        assert!(result.height <= 480, "height {} > 480", result.height);
        assert!(!result.output_data.is_empty());
    }

    #[test]
    fn convert_non_jpeg_still_works() {
        // PNG 入力が引き続き動くこと
        let img = DynamicImage::new_rgb8(200, 150);
        let mut buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let params = ConvertParams {
            image_type: 1,
            max_width: 640,
            max_height: 480,
            quality: 80,
            effort: 3,
        };
        let result = convert(&buf, &params).expect("PNG convert failed");
        assert!(!result.output_data.is_empty());
    }

    /// テスト用の最小 JPEG バイト列を生成するヘルパー
    fn make_test_jpeg(width: u32, height: u32) -> Vec<u8> {
        let img = DynamicImage::new_rgb8(width, height);
        let mut buf = Vec::new();
        img.write_to(
            &mut std::io::Cursor::new(&mut buf),
            image::ImageFormat::Jpeg,
        )
        .expect("failed to encode test JPEG");
        buf
    }

    #[test]
    fn jpeg_decode_scaled_uses_quarter_scale() {
        let jpeg = make_test_jpeg(4000, 3000);
        let (img, _scale) = jpeg_decode_scaled(&jpeg, 640, 480).expect("decode failed");
        // 1/4 scale: 1000x750
        assert_eq!(img.width(), 1000);
        assert_eq!(img.height(), 750);
    }

    #[test]
    fn jpeg_decode_scaled_no_upscale_for_small_source() {
        let jpeg = make_test_jpeg(400, 300);
        let (img, _scale) = jpeg_decode_scaled(&jpeg, 640, 480).expect("decode failed");
        // source is smaller than target → scale 1/1 → 400x300
        assert_eq!(img.width(), 400);
        assert_eq!(img.height(), 300);
    }

    #[test]
    fn choose_scale_quarter_for_large_image() {
        // 4000x3000 → max 640x480: 1/4=1000x750 ≥ 640, OK
        let f = choose_scale(4000, 3000, 640, 480);
        assert_eq!(f, turbojpeg::ScalingFactor::ONE_QUARTER);
    }

    #[test]
    fn choose_scale_eighth_for_very_small_target() {
        // 4000x3000 → max 200x150: 1/8=500x375 ≥ 200, OK
        let f = choose_scale(4000, 3000, 200, 150);
        assert_eq!(f, turbojpeg::ScalingFactor::ONE_EIGHTH);
    }

    #[test]
    fn choose_scale_no_scale_when_source_is_small() {
        // 400x300 → max 640x480: source already fits, 1/1
        let f = choose_scale(400, 300, 640, 480);
        assert_eq!(f, turbojpeg::ScalingFactor::ONE);
    }

    #[test]
    fn choose_scale_half_for_medium_image() {
        // 1280x960 → max 640x480: 1/4=320x240 < 640 and < 480, but 1/2=640x480 ≥ 640, OK
        let f = choose_scale(1280, 960, 640, 480);
        assert_eq!(f, turbojpeg::ScalingFactor::ONE_HALF);
    }

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
