//! 图片附件的解码与缩略图（V2 P2-1）。
//! 解码在 spawn_blocking 里执行；任何失败由调用方降级为「无尺寸/无缩略图」，附件照常入库。

use image::codecs::jpeg::JpegEncoder;
use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageReader, Limits};

/// 缩略图最长边（px）
pub const THUMB_EDGE: u32 = 480;
/// 解码上限：最大宽高 12,000px、最大分配 512MB（防解压炸弹）
const MAX_DIM: u32 = 12_000;
const MAX_ALLOC: u64 = 512 * 1024 * 1024;

pub struct ImageMeta {
    /// EXIF 方向纠正后的像素尺寸
    pub width: i32,
    pub height: i32,
    /// JPEG q80 缩略图字节；最长边 ≤480 或编码失败时为 None
    pub thumb_jpeg: Option<Vec<u8>>,
}

/// 解出图片并应用 EXIF 方向；超出尺寸/内存上限或格式不支持即失败
fn decode(data: &[u8]) -> image::ImageResult<DynamicImage> {
    let mut reader = ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .map_err(image::ImageError::IoError)?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DIM);
    limits.max_image_height = Some(MAX_DIM);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// 最长边 > THUMB_EDGE 时等比缩到 480 并编码 JPEG q80
fn encode_thumb(img: &DynamicImage) -> image::ImageResult<Vec<u8>> {
    let thumb = img.thumbnail(THUMB_EDGE, THUMB_EDGE);
    // JPEG 不支持 alpha：透明图先铺白底再编码，避免透明区变黑
    let rgb = if thumb.color().has_alpha() {
        let rgba = thumb.to_rgba8();
        let mut bg = image::RgbaImage::from_pixel(
            thumb.width(),
            thumb.height(),
            image::Rgba([255, 255, 255, 255]),
        );
        image::imageops::overlay(&mut bg, &rgba, 0, 0);
        DynamicImage::ImageRgba8(bg).into_rgb8()
    } else {
        thumb.to_rgb8()
    };
    let mut out = Vec::new();
    DynamicImage::ImageRgb8(rgb).write_with_encoder(JpegEncoder::new_with_quality(&mut out, 80))?;
    Ok(out)
}

/// jpeg/png/webp：记录宽高，最长边 >480 附缩略图；gif 只取首帧宽高（保留动图）；其余 None。
/// 输入 bytes 已是完整文件，格式以内容嗅探为准（mime 只决定要不要处理）。
pub fn inspect_image(mime: &str, data: &[u8]) -> Option<ImageMeta> {
    let want_thumb = matches!(mime, "image/jpeg" | "image/png" | "image/webp");
    let dims_only = mime == "image/gif";
    if !want_thumb && !dims_only {
        return None;
    }
    let img = decode(data).ok()?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }
    let thumb_jpeg = if want_thumb && w.max(h) > THUMB_EDGE {
        encode_thumb(&img).ok()
    } else {
        None
    };
    Some(ImageMeta {
        width: w as i32,
        height: h as i32,
        thumb_jpeg,
    })
}
