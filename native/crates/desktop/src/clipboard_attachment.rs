//! Byte-only preparation of complete GPUI clipboard image files.
//!
//! The caller runs this inside attachment_intake's serialized background preparation and owns
//! filename admission, draft identity, error presentation and storage. PNG/JPEG/GIF/WebP/SVG keep
//! their encoded bytes. BMP/TIFF/ICO/PNM become static PNGs without copied EXIF, ICC or XMP.
//! GPUI's platform.rs decode_static_image_from_decoder applies the decoder's orientation and
//! decodes one image. This boundary follows that contract, including the first TIFF image.
//!
//! Raster buffer checks use document_attachments' existing 100 MiB image-work allowance. The
//! encoded input and output have separate established intake/transport caps. image::Limits is
//! best-effort for decoder-internal allocations, not process memory isolation. HEIC is never
//! decoded here; its existing isolated worker remains the only HEIC conversion boundary.

use std::io::{self, Cursor};

use gpui_kit::ImageFormat;
use image::codecs::png::PngEncoder;
use image::metadata::Orientation;
use image::{
    ColorType, DynamicImage, ExtendedColorType, ImageError, ImageReader, Limits, RgbaImage,
};
use openchamber_native_transport::attachment::MAX_PREPARED_ATTACHMENT_BYTES;

use crate::attachment_files::{MAX_INLINE_ATTACHMENT_BYTES, NormalizedAttachmentFile};

// document_attachments::MAX_IMAGE_VALIDATION_BYTES is private. Keep the same work allowance;
// width/height follow from fitting two RGBA buffers, without a separate arbitrary axis policy.
const MAX_RASTER_WORK_BYTES: usize = 100 * 1024 * 1024;
const MAX_IMAGE_DIMENSION: u32 = (MAX_RASTER_WORK_BYTES / 8) as u32;

/// Content-free failures for one clipboard image. Decoder diagnostics are discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClipboardImageError {
    InputTooLarge,
    Unsupported,
    Invalid,
    Limit,
    OutputTooLarge,
    Allocation,
    Encode,
}

/// Prepares a complete encoded clipboard image without reading files or spawning work.
///
/// `bytes` must be the complete GPUI Image::bytes(), never a truncated prefix or raw pixels.
/// `filename` must be an admitted display basename. Accepted encoded formats retain that name;
/// conversion replaces its final extension with `.png`. Intake must also admit the result name
/// and must call this off the foreground thread under its existing preparation gate.
pub(crate) fn prepare_clipboard_image(
    bytes: &[u8],
    format: ImageFormat,
    filename: String,
) -> Result<NormalizedAttachmentFile, ClipboardImageError> {
    use image::ImageDecoder;

    if bytes.len() > MAX_INLINE_ATTACHMENT_BYTES {
        return Err(ClipboardImageError::InputTooLarge);
    }

    let raster_format = match format {
        ImageFormat::Png => image::ImageFormat::Png,
        ImageFormat::Jpeg => image::ImageFormat::Jpeg,
        ImageFormat::Gif => image::ImageFormat::Gif,
        ImageFormat::Webp => image::ImageFormat::WebP,
        ImageFormat::Svg => return unchanged(bytes, format.mime_type(), filename),
        ImageFormat::Bmp => image::ImageFormat::Bmp,
        ImageFormat::Tiff => image::ImageFormat::Tiff,
        ImageFormat::Ico => image::ImageFormat::Ico,
        ImageFormat::Pnm => image::ImageFormat::Pnm,
    };
    if image::guess_format(bytes).ok() != Some(raster_format) {
        return Err(ClipboardImageError::Invalid);
    }
    if matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::Webp
    ) {
        return unchanged(bytes, format.mime_type(), filename);
    }

    // image 0.25.10's ICO decoder creates an unlimited nested PNG decoder before set_limits.
    // Select the same entry without allocation and route embedded PNG through ImageReader so
    // its metadata inflation receives limits before decoder construction.
    let ico_png = if format == ImageFormat::Ico {
        selected_ico_png(bytes)?
    } else {
        None
    };
    let (encoded, decode_format) = ico_png.as_ref().map_or((bytes, raster_format), |entry| {
        (entry.bytes, image::ImageFormat::Png)
    });
    let mut reader = ImageReader::with_format(Cursor::new(encoded), decode_format);
    reader.limits(raster_limits());
    let mut decoder = reader.into_decoder().map_err(decode_error)?;
    let (width, height) = decoder.dimensions();
    let rgba_bytes = checked_pixel_bytes(width, height)?;
    if let Some(entry) = ico_png
        && ((width.min(256), height.min(256)) != entry.dimensions
            || decoder.color_type() != ColorType::Rgba8)
    {
        return Err(ClipboardImageError::Invalid);
    }
    let decoded_bytes =
        usize::try_from(decoder.total_bytes()).map_err(|_| ClipboardImageError::Limit)?;
    if decoded_bytes == 0 {
        return Err(ClipboardImageError::Invalid);
    }
    // into_rgba8 reuses RGBA8 storage; other sample types can overlap one RGBA allocation.
    decoded_bytes
        .checked_add(rgba_bytes)
        .filter(|&total| total <= MAX_RASTER_WORK_BYTES)
        .ok_or(ClipboardImageError::Limit)?;
    let mut limits = raster_limits();
    // Match ImageReader::decode's reservation for DynamicImage's caller-owned decode buffer.
    limits
        .reserve(decoder.total_bytes())
        .map_err(decode_error)?;
    decoder.set_limits(limits).map_err(decode_error)?;
    // GPUI's ICO wrapper exposes no EXIF orientation, including for nested PNG payloads.
    let orientation = if format == ImageFormat::Ico {
        Orientation::NoTransforms
    } else {
        decoder.orientation().map_err(decode_error)?
    };
    let decoded = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    if (decoded.width(), decoded.height()) != (width, height)
        || decoded.as_bytes().len() != decoded_bytes
    {
        return Err(ClipboardImageError::Invalid);
    }
    let mut image = DynamicImage::ImageRgba8(decoded.into_rgba8());
    // Transposed orientations can overlap two RGBA buffers, admitted before pixel decoding.
    image.apply_orientation(orientation);
    let image = image.into_rgba8();
    let content = encode_png(&image, MAX_PREPARED_ATTACHMENT_BYTES)?;
    Ok(NormalizedAttachmentFile {
        filename: png_filename(&filename),
        media_type: "image/png",
        content,
    })
}

fn unchanged(
    bytes: &[u8],
    media_type: &'static str,
    filename: String,
) -> Result<NormalizedAttachmentFile, ClipboardImageError> {
    let mut content = Vec::new();
    content
        .try_reserve_exact(bytes.len())
        .map_err(|_| ClipboardImageError::Allocation)?;
    content.extend_from_slice(bytes);
    Ok(NormalizedAttachmentFile {
        filename,
        media_type,
        content,
    })
}

fn raster_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_RASTER_WORK_BYTES as u64);
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits
}

fn checked_pixel_bytes(width: u32, height: u32) -> Result<usize, ClipboardImageError> {
    if width == 0 || height == 0 {
        return Err(ClipboardImageError::Invalid);
    }
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(ClipboardImageError::Limit);
    }
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .filter(|&bytes| bytes <= MAX_RASTER_WORK_BYTES / 2 && bytes <= isize::MAX as usize)
        .ok_or(ClipboardImageError::Limit)
}

fn decode_error(error: ImageError) -> ClipboardImageError {
    match error {
        ImageError::Unsupported(_) => ClipboardImageError::Unsupported,
        ImageError::Limits(_) => ClipboardImageError::Limit,
        ImageError::IoError(error) if error.kind() == io::ErrorKind::OutOfMemory => {
            ClipboardImageError::Allocation
        }
        _ => ClipboardImageError::Invalid,
    }
}

fn png_filename(filename: &str) -> String {
    let stem = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
    format!("{stem}.png")
}

struct IcoPng<'a> {
    bytes: &'a [u8],
    dimensions: (u32, u32),
}

/// image 0.25.10 chooses maximum bit depth then area, with the last entry winning ties.
fn selected_ico_png(bytes: &[u8]) -> Result<Option<IcoPng<'_>>, ClipboardImageError> {
    let header = bytes.get(..6).ok_or(ClipboardImageError::Invalid)?;
    let count = usize::from(u16::from_le_bytes([header[4], header[5]]));
    if count == 0 {
        return Err(ClipboardImageError::Invalid);
    }
    let directory_end = 6 + count * 16; // A u16 count fits usize on every supported target.
    let directory = bytes
        .get(6..directory_end)
        .ok_or(ClipboardImageError::Invalid)?;
    let mut best = None;
    for entry in directory.chunks_exact(16) {
        let width = if entry[0] == 0 {
            256
        } else {
            u32::from(entry[0])
        };
        let height = if entry[1] == 0 {
            256
        } else {
            u32::from(entry[1])
        };
        let planes = u16::from_le_bytes([entry[4], entry[5]]);
        let depth = u16::from_le_bytes([entry[6], entry[7]]);
        if planes > 256 || depth > 256 {
            return Err(ClipboardImageError::Invalid);
        }
        let score = (depth, width * height);
        if best
            .as_ref()
            .is_none_or(|(previous, _, _)| score >= *previous)
        {
            best = Some((score, entry, (width, height)));
        }
    }
    let (_, entry, dimensions) = best.ok_or(ClipboardImageError::Invalid)?;
    let length = u32::from_le_bytes(
        entry[8..12]
            .try_into()
            .expect("ICO length field is 4 bytes"),
    );
    let offset = u32::from_le_bytes(
        entry[12..16]
            .try_into()
            .expect("ICO offset field is 4 bytes"),
    );
    let offset = usize::try_from(offset).map_err(|_| ClipboardImageError::Invalid)?;
    let length = usize::try_from(length).map_err(|_| ClipboardImageError::Invalid)?;
    let end = offset
        .checked_add(length)
        .ok_or(ClipboardImageError::Invalid)?;
    if offset < directory_end {
        return Err(ClipboardImageError::Invalid);
    }
    let payload = bytes.get(offset..end).ok_or(ClipboardImageError::Invalid)?;
    Ok(payload.starts_with(b"\x89PNG\r\n\x1a\n").then_some(IcoPng {
        bytes: payload,
        dimensions,
    }))
}

fn encode_png(image: &RgbaImage, max_bytes: usize) -> Result<Vec<u8>, ClipboardImageError> {
    use image::ImageEncoder;

    let mut output = PngOutput::new(max_bytes);
    let encoded = PngEncoder::new(&mut output).write_image(
        image.as_raw(),
        image.width(),
        image.height(),
        ExtendedColorType::Rgba8,
    );
    if let Some(error) = output.error {
        return Err(error);
    }
    encoded.map_err(|_| ClipboardImageError::Encode)?;
    Ok(output.bytes)
}

/// Append-only output gives the PNG encoder no seek or backfill operation that can bypass the cap.
struct PngOutput {
    bytes: Vec<u8>,
    max_bytes: usize,
    error: Option<ClipboardImageError>,
}

impl PngOutput {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
            error: None,
        }
    }
}

impl io::Write for PngOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(error) = self.error {
            return Err(match error {
                ClipboardImageError::Allocation => io::ErrorKind::OutOfMemory,
                _ => io::ErrorKind::FileTooLarge,
            }
            .into());
        }
        let Some(length) = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|&length| length <= self.max_bytes)
        else {
            self.error = Some(ClipboardImageError::OutputTooLarge);
            return Err(io::ErrorKind::FileTooLarge.into());
        };
        if length > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(1024)
                .max(length)
                .min(self.max_bytes);
            if self
                .bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .is_err()
            {
                self.error = Some(ClipboardImageError::Allocation);
                return Err(io::ErrorKind::OutOfMemory.into());
            }
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[path = "clipboard_attachment_tests.rs"]
mod tests;
