//! Byte-only HEIC/HEIF preparation for the attachment normalization owner.
//!
//! Shared attachment-files.ts requests JPEG quality 0.9, replaces a final HEIC/HEIF
//! extension and exports no EXIF/ICC metadata. It defines no pixel or output-byte
//! ceiling. The caller supplies those budgets, reads the complete input, chooses
//! the file's attachment owner and runs conversion off the foreground thread.
//!
//! Metadata counts are checked without allocating before Context::open. The
//! pinned decoder does not pass its pixel ceiling to the HEVC picture allocator,
//! so preflight also checks every selected picture's uncropped SPS dimensions.
//! max_decoded_pixels bounds their cumulative coded area. These are record-count
//! and pixel-work budgets, not an exact process-memory ceiling.

use std::io;

use heic_rs::boxes::{BoxHeader, BoxIter};
use heic_rs::context::Context;
use heic_rs::hevc::Frame;
use heic_rs::props::ItemProps;
use heic_rs::{Image, PixelLayout};
use image::ExtendedColorType;
use image::codecs::jpeg::JpegEncoder;

use crate::attachment_files::NormalizedAttachmentFile;

/// The shared quality of 0.9 on the image encoder's integer 1 through 100 scale.
const JPEG_QUALITY: u8 = 90;

/// Explicit per-file resource budgets. No shared HEIC pixel policy exists yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeicConversionLimits {
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    /// Aggregate Context::open table records/reservation slots, checked first.
    /// Includes item locations/extents, properties/associations and references.
    pub max_metadata_entries: usize,
    /// Maximum area of the primary image, before and after container transforms.
    pub max_pixels: u64,
    /// Sum of uncropped coded areas, including every selected grid tile.
    pub max_decoded_pixels: u64,
}

/// Conversion failures contain neither filenames nor input/decoder payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ImageConversionError {
    InputTooLarge,
    MetadataLimit,
    PixelLimit,
    InvalidDimensions,
    InvalidImage,
    UnsupportedImage,
    InvalidDecodedImage,
    OutputTooLarge,
    AllocationFailed,
    EncodeFailed,
}

/// Converts one complete HEIC/HEIF still image to a normalized JPEG attachment.
///
/// The decoder resolves its primary item, composes grid tiles and applies HEIF
/// clean-aperture, rotation and mirroring properties in association order. EXIF
/// orientation is not applied a second time. heic2any 0.0.4 discards HEIF alpha
/// and uses a fixed limited-range BT.601 matrix. Conversion reads YCbCr planes
/// directly to preserve that behavior; nclx never changes the matrix. The JPEG
/// contains no copied EXIF, ICC or XMP. Encoder bytes may differ from Chromium's.
///
/// Routing by extension or declared MIME belongs to the normalization caller.
/// This function validates actual bytes even when a filename has another suffix.
/// Multi-image container ordering against heic2any requires a parity fixture.
pub(crate) fn convert_heic_to_jpeg(
    filename: &str,
    content: &[u8],
    limits: HeicConversionLimits,
) -> Result<NormalizedAttachmentFile, ImageConversionError> {
    if content.len() > limits.max_input_bytes {
        return Err(ImageConversionError::InputTooLarge);
    }

    preflight_metadata(content, limits.max_metadata_entries)?;
    let context = Context::open(content).map_err(decode_error)?;
    let mut decoded_pixels = 0;
    preflight_item(&context, context.meta.primary, limits, &mut decoded_pixels)?;

    let props = context.props(context.meta.primary).map_err(decode_error)?;
    let (coded_width, coded_height) = context
        .coded_size(context.meta.primary, &props)
        .map_err(decode_error)?;
    checked_pixel_bytes(coded_width, coded_height, limits.max_pixels)?;
    let (width, height) =
        heic_rs::transform::transformed_size(coded_width, coded_height, &props.transforms)
            .map_err(decode_error)?;
    checked_jpeg_dimensions(width, height)?;
    let expected_bytes = checked_pixel_bytes(width, height, limits.max_pixels)? / 4 * 3;
    let frame = decode_primary_frame(&context, &props, limits.max_pixels)?;
    let rgb = reference_rgb(&frame, limits.max_pixels)?;
    let decoded = heic_rs::transform::apply_all(rgb, &props.transforms).map_err(decode_error)?;
    if decoded.width != width
        || decoded.height != height
        || decoded.layout != PixelLayout::Rgb8
        || decoded.data.len() != expected_bytes
    {
        return Err(ImageConversionError::InvalidDecodedImage);
    }

    let mut output = JpegOutput::new(limits.max_output_bytes);
    let encoded = JpegEncoder::new_with_quality(&mut output, JPEG_QUALITY).encode(
        &decoded.data,
        decoded.width,
        decoded.height,
        ExtendedColorType::Rgb8,
    );
    if encoded.is_err() {
        return Err(if output.limit_hit {
            ImageConversionError::OutputTooLarge
        } else if output.allocation_failed {
            ImageConversionError::AllocationFailed
        } else {
            ImageConversionError::EncodeFailed
        });
    }
    Ok(NormalizedAttachmentFile {
        filename: jpeg_filename(filename),
        media_type: "image/jpeg",
        content: output.bytes,
    })
}

impl From<heic_rs::Error> for ImageConversionError {
    fn from(error: heic_rs::Error) -> Self {
        decode_error(error)
    }
}

/// Guard the allocating container tables in heic-rs 0.1.4 before Context::open.
/// BoxIter and Reader borrow input and allocate nothing. This checks counts and
/// field widths only; the pinned parser still validates each record's contents.
fn preflight_metadata(content: &[u8], max_entries: usize) -> Result<(), ImageConversionError> {
    let meta = BoxIter::new(content).require(b"meta", "meta")?;
    let (_, _, reader) = meta.full_box("meta")?;
    let mut budget = MetadataBudget {
        remaining: max_entries,
    };
    for child in BoxIter::new(reader.rest()) {
        let child = child?;
        match &child.boxtype {
            b"iloc" => preflight_iloc(&child, &mut budget)?,
            b"iinf" => preflight_iinf(&child, &mut budget)?,
            b"iref" => preflight_iref(&child, &mut budget)?,
            b"iprp" => preflight_iprp(&child, &mut budget)?,
            _ => {}
        }
    }
    Ok(())
}

struct MetadataBudget {
    remaining: usize,
}

impl MetadataBudget {
    fn take(&mut self, entries: usize) -> Result<(), ImageConversionError> {
        self.remaining = self
            .remaining
            .checked_sub(entries)
            .ok_or(ImageConversionError::MetadataLimit)?;
        Ok(())
    }

    // Dynamic vectors can double their capacity and start with 4 slots. Charge
    // conservatively, including temporary lists and tables a later box replaces.
    fn dynamic(&mut self, entries: usize) -> Result<(), ImageConversionError> {
        let slots = entries
            .checked_mul(2)
            .and_then(|n| n.checked_add(4))
            .ok_or(ImageConversionError::MetadataLimit)?;
        self.take(slots)
    }
}

fn preflight_iloc(
    box_header: &BoxHeader<'_>,
    budget: &mut MetadataBudget,
) -> Result<(), ImageConversionError> {
    let (version, _, mut reader) = box_header.full_box("iloc")?;
    if version > 2 {
        return Err(ImageConversionError::UnsupportedImage);
    }
    let sizes = reader.u8("iloc sizes")?;
    let sizes2 = reader.u8("iloc sizes")?;
    let offset = sizes >> 4;
    let length = sizes & 15;
    let base = sizes2 >> 4;
    let index = if version == 0 { 0 } else { sizes2 & 15 };
    if [offset, length, base, index]
        .iter()
        .any(|&n| !matches!(n, 0 | 4 | 8))
    {
        return Err(ImageConversionError::InvalidImage);
    }
    let items = if version < 2 {
        usize::from(reader.u16("iloc count")?)
    } else {
        reader.u32("iloc count")? as usize
    };
    let id_width = if version < 2 { 2 } else { 4 };
    let method_width = if version == 0 { 0 } else { 2 };
    let record_width = id_width + method_width + 2 + usize::from(base) + 2;
    if items > reader.remaining() / record_width {
        return Err(ImageConversionError::InvalidImage);
    }
    budget.dynamic(items)?;
    let extent_width = usize::from(offset) + usize::from(length) + usize::from(index);
    for _ in 0..items {
        reader.skip(record_width - 2, "iloc item header")?;
        let count = usize::from(reader.u16("iloc extents")?);
        // Multiple zero-width extents repeat the same entire object without
        // encoding any data. The pinned parser otherwise allocates count slots.
        if extent_width == 0 && count > 1 {
            return Err(ImageConversionError::InvalidImage);
        }
        budget.take(count)?;
        let bytes = count
            .checked_mul(extent_width)
            .ok_or(ImageConversionError::MetadataLimit)?;
        reader.skip(bytes, "iloc extent records")?;
    }
    Ok(())
}

fn preflight_iinf(
    box_header: &BoxHeader<'_>,
    budget: &mut MetadataBudget,
) -> Result<(), ImageConversionError> {
    let (version, _, mut reader) = box_header.full_box("iinf")?;
    let count = if version == 0 {
        usize::from(reader.u16("iinf count")?)
    } else {
        reader.u32("iinf count")? as usize
    };
    if count > reader.remaining() / 8 {
        return Err(ImageConversionError::InvalidImage);
    }
    budget.take(count)?;
    budget.take(4)?;
    // The pinned parser reserves count but pushes every actual infe child.
    for child in BoxIter::new(reader.rest()) {
        if child?.is(b"infe") {
            budget.take(2)?;
        }
    }
    Ok(())
}

fn preflight_iref(
    box_header: &BoxHeader<'_>,
    budget: &mut MetadataBudget,
) -> Result<(), ImageConversionError> {
    let (version, _, reader) = box_header.full_box("iref")?;
    let width = match version {
        0 => 2,
        1 => 4,
        _ => return Err(ImageConversionError::UnsupportedImage),
    };
    budget.take(4)?;
    for child in BoxIter::new(reader.rest()) {
        let child = child?;
        let mut record = child.reader();
        record.skip(width, "iref source")?;
        let count = usize::from(record.u16("iref count")?);
        budget.take(2)?;
        budget.take(count)?;
        record.skip(count * width, "iref target records")?;
    }
    Ok(())
}

fn preflight_iprp(
    box_header: &BoxHeader<'_>,
    budget: &mut MetadataBudget,
) -> Result<(), ImageConversionError> {
    budget.take(8)?; // Initial capacity of the properties and associations vectors.
    for child in box_header.children() {
        let child = child?;
        if child.is(b"ipco") {
            for property in child.children() {
                property?;
                budget.take(2)?;
            }
        } else if child.is(b"ipma") {
            let (version, flags, mut reader) = child.full_box("ipma")?;
            let count = reader.u32("ipma count")? as usize;
            let id_width = if version == 0 { 2 } else { 4 };
            if count > reader.remaining() / (id_width + 1) {
                return Err(ImageConversionError::InvalidImage);
            }
            budget.dynamic(count)?;
            let width = if flags & 1 == 0 { 1 } else { 2 };
            for _ in 0..count {
                reader.skip(id_width, "ipma item")?;
                let associations = usize::from(reader.u8("ipma associations")?);
                budget.dynamic(associations)?;
                reader.skip(associations * width, "ipma association records")?;
            }
        }
    }
    Ok(())
}

fn decode_error(error: heic_rs::Error) -> ImageConversionError {
    match error {
        heic_rs::Error::PixelLimit { .. } => ImageConversionError::PixelLimit,
        heic_rs::Error::Unsupported(_) => ImageConversionError::UnsupportedImage,
        _ => ImageConversionError::InvalidImage,
    }
}

fn preflight_item(
    context: &Context<'_>,
    item: u32,
    limits: HeicConversionLimits,
    decoded_pixels: &mut u64,
) -> Result<(), ImageConversionError> {
    require_supported_properties(context, item)?;
    let props = context.props(item).map_err(decode_error)?;
    if let Some(size) = props.ispe {
        checked_pixel_bytes(size.width, size.height, limits.max_decoded_pixels)?;
    }
    if let Some((grid, tiles)) = context.grid(item).map_err(decode_error)? {
        checked_pixel_bytes(
            grid.output_width,
            grid.output_height,
            limits.max_decoded_pixels,
        )?;
        // Context::grid rejects nested and cyclic grid derivations before here.
        // Repeated tile references count repeatedly: the decoder also decodes each.
        for tile in tiles {
            require_supported_properties(context, tile)?;
            let props = context.props(tile).map_err(decode_error)?;
            preflight_coded(&props, limits, decoded_pixels)?;
        }
    } else {
        preflight_coded(&props, limits, decoded_pixels)?;
    }
    Ok(())
}

fn require_supported_properties(
    context: &Context<'_>,
    item: u32,
) -> Result<(), ImageConversionError> {
    // This is the high-level decoder's strict=true check, applied to tiles too.
    if context.meta.props.unknown_essential(item).is_some() {
        return Err(ImageConversionError::UnsupportedImage);
    }
    if context
        .meta
        .props
        .associations(item)
        .iter()
        .any(|association| {
            association.essential
                && association.index != 0
                && context.meta.props.resolve(*association).is_none()
        })
    {
        return Err(ImageConversionError::InvalidImage);
    }
    Ok(())
}

/// Use the public codec and compositor APIs to choose color before RGB exists.
/// The high-level decode API has no nclx override and its color path upsamples
/// chroma differently from heic2any's nearest-neighbor 4:2:0 display loop.
fn decode_primary_frame(
    context: &Context<'_>,
    props: &ItemProps<'_>,
    max_pixels: u64,
) -> Result<Frame, ImageConversionError> {
    if let Some((grid, ids)) = context.grid(context.meta.primary)? {
        let mut tiles = Vec::new();
        tiles
            .try_reserve_exact(ids.len())
            .map_err(|_| ImageConversionError::AllocationFailed)?;
        for id in ids {
            tiles.push(decode_coded_frame(context, id, &context.props(id)?)?);
        }
        Ok(heic_rs::grid::compose(&grid, &tiles, max_pixels)?)
    } else {
        decode_coded_frame(context, context.meta.primary, props)
    }
}

fn decode_coded_frame(
    context: &Context<'_>,
    id: u32,
    props: &ItemProps<'_>,
) -> Result<Frame, ImageConversionError> {
    let config = props
        .hvcc
        .as_ref()
        .ok_or(ImageConversionError::InvalidImage)?;
    let data = context.item_data(id)?;
    let slices = config.split_nals(&data)?;
    let frame = heic_rs::hevc::decode_still(&config.parameter_sets(), &slices)?;
    frame.validate()?;
    Ok(frame)
}

/// heic2any@0.0.4 HeifImage.display's coefficients and Uint8ClampedArray rounding.
/// For 8-bit 4:2:0, chroma samples cover the same 2x2 pixels as the reference.
/// Other depths and sampling retain the codec's native planes; qualification
/// against libheif's preceding 8-bit/4:2:0 conversion is still required for those.
fn reference_rgb(frame: &Frame, max_pixels: u64) -> Result<Image, ImageConversionError> {
    frame.validate()?;
    if !(8..=16).contains(&frame.bit_depth) {
        return Err(ImageConversionError::UnsupportedImage);
    }
    let bytes = checked_pixel_bytes(frame.width, frame.height, max_pixels)? / 4 * 3;
    let mut data = Vec::new();
    data.try_reserve_exact(bytes)
        .map_err(|_| ImageConversionError::AllocationFailed)?;
    let step = f64::from(1u16 << (frame.bit_depth - 8));
    for y in 0..frame.height as usize {
        for x in 0..frame.width as usize {
            let luma = f64::from(frame.y[y * frame.y_stride as usize + x]) / step;
            let (cb, cr) = if frame.chroma == heic_rs::ChromaFormat::Monochrome {
                (0.0, 0.0)
            } else {
                let index = (y >> frame.chroma.y_shift()) * frame.c_stride as usize
                    + (x >> frame.chroma.x_shift());
                (
                    f64::from(frame.cb[index]) / step - 128.0,
                    f64::from(frame.cr[index]) / step - 128.0,
                )
            };
            let luma = 1.164 * (luma - 16.0);
            for value in [
                luma + 1.596 * cr,
                luma - 0.813 * cr - 0.391 * cb,
                luma + 2.018 * cb,
            ] {
                data.push(value.clamp(0.0, 255.0).round_ties_even() as u8);
            }
        }
    }
    Ok(Image {
        data,
        width: frame.width,
        height: frame.height,
        layout: PixelLayout::Rgb8,
    })
}

fn preflight_coded(
    props: &ItemProps<'_>,
    limits: HeicConversionLimits,
    decoded_pixels: &mut u64,
) -> Result<(), ImageConversionError> {
    let config = props
        .hvcc
        .as_ref()
        .ok_or(ImageConversionError::InvalidImage)?;
    let mut largest = 0;
    // The decoder dispatches by each NAL's actual header, including a NAL stored
    // in a differently labelled hvcC parameter-set array. Check that same set.
    for nal in config.parameter_sets() {
        if nal.len() < 2 {
            return Err(ImageConversionError::InvalidImage);
        }
        let kind = (nal[0] >> 1) & 0x3f;
        let layer = ((nal[0] & 1) << 5) | (nal[1] >> 3);
        if kind != 33 || layer != 0 {
            continue;
        }
        let (width, height) = sps_dimensions(nal)?;
        checked_pixel_bytes(width, height, limits.max_decoded_pixels)?;
        largest = largest.max(u64::from(width) * u64::from(height));
    }
    if largest == 0 {
        return Err(ImageConversionError::InvalidImage);
    }
    *decoded_pixels = decoded_pixels
        .checked_add(largest)
        .filter(|&total| total <= limits.max_decoded_pixels)
        .ok_or(ImageConversionError::PixelLimit)?;
    Ok(())
}

fn checked_pixel_bytes(
    width: u32,
    height: u32,
    max_pixels: u64,
) -> Result<usize, ImageConversionError> {
    if width == 0 || height == 0 {
        return Err(ImageConversionError::InvalidDimensions);
    }
    u64::from(width)
        .checked_mul(u64::from(height))
        .filter(|&pixels| pixels <= max_pixels)
        .and_then(|pixels| pixels.checked_mul(4))
        .and_then(|bytes| usize::try_from(bytes).ok())
        .filter(|&bytes| bytes <= isize::MAX as usize)
        .ok_or(ImageConversionError::PixelLimit)
}

fn checked_jpeg_dimensions(width: u32, height: u32) -> Result<(), ImageConversionError> {
    if width == 0 || height == 0 || width > u32::from(u16::MAX) || height > u32::from(u16::MAX) {
        return Err(ImageConversionError::InvalidDimensions);
    }
    Ok(())
}

pub(crate) fn jpeg_filename(filename: &str) -> String {
    match filename.rsplit_once('.') {
        Some((stem, extension))
            if extension.eq_ignore_ascii_case("heic") || extension.eq_ignore_ascii_case("heif") =>
        {
            format!("{stem}.jpg")
        }
        _ => filename.to_owned(),
    }
}

/// The SPS geometry prefix from HEVC clauses 7.3.2.2 and 7.3.3.
///
/// The public heic_rs::hevc::probe returns cropped dimensions from only the
/// first referenced SPS. The allocator uses uncropped dimensions from the SPS
/// selected by a slice, so every SPS must pass this allocation-free check.
fn sps_dimensions(nal: &[u8]) -> Result<(u32, u32), ImageConversionError> {
    if nal.len() < 2 || nal[0] & 0x80 != 0 || (nal[0] >> 1) & 0x3f != 33 || nal[1] & 7 == 0 {
        return Err(ImageConversionError::InvalidImage);
    }
    let mut bits = NalBits::new(&nal[2..]);
    bits.skip(4)?;
    let sublayers = bits.read(3)? as usize;
    bits.skip(1 + 96)?;
    let mut profiles = [false; 7];
    let mut levels = [false; 7];
    for index in 0..sublayers {
        profiles[index] = bits.read(1)? != 0;
        levels[index] = bits.read(1)? != 0;
    }
    if sublayers > 0 {
        bits.skip((8 - sublayers) * 2)?;
    }
    for index in 0..sublayers {
        if profiles[index] {
            bits.skip(88)?;
        }
        if levels[index] {
            bits.skip(8)?;
        }
    }
    bits.unsigned_exp_golomb()?; // sps_seq_parameter_set_id
    let chroma = bits.unsigned_exp_golomb()?;
    if chroma > 3 {
        return Err(ImageConversionError::InvalidImage);
    }
    if chroma == 3 {
        bits.skip(1)?; // separate_colour_plane_flag
    }
    let width = bits.unsigned_exp_golomb()?;
    let height = bits.unsigned_exp_golomb()?;
    // These are the pinned decoder's own valid coded-dimension bounds.
    checked_jpeg_dimensions(width, height)?;
    Ok((width, height))
}

/// Reads RBSP bits without copying a NAL or allocating an emulation-byte index.
struct NalBits<'a> {
    bytes: &'a [u8],
    offset: usize,
    zeros: u8,
    current: u8,
    remaining: u8,
}

impl<'a> NalBits<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            offset: 0,
            zeros: 0,
            current: 0,
            remaining: 0,
        }
    }

    fn bit(&mut self) -> Result<u32, ImageConversionError> {
        if self.remaining == 0 {
            loop {
                let byte = *self
                    .bytes
                    .get(self.offset)
                    .ok_or(ImageConversionError::InvalidImage)?;
                self.offset += 1;
                if self.zeros >= 2 && byte == 3 {
                    self.zeros = 0;
                    continue;
                }
                self.zeros = if byte == 0 {
                    self.zeros.saturating_add(1)
                } else {
                    0
                };
                self.current = byte;
                self.remaining = 8;
                break;
            }
        }
        self.remaining -= 1;
        Ok(u32::from((self.current >> self.remaining) & 1))
    }

    fn read(&mut self, count: usize) -> Result<u32, ImageConversionError> {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Ok(value)
    }

    fn skip(&mut self, count: usize) -> Result<(), ImageConversionError> {
        for _ in 0..count {
            self.bit()?;
        }
        Ok(())
    }

    fn unsigned_exp_golomb(&mut self) -> Result<u32, ImageConversionError> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros >= 32 {
                return Err(ImageConversionError::InvalidImage);
            }
        }
        Ok(((1u32 << zeros) - 1) + self.read(zeros)?)
    }
}

/// Refuses a JPEG write before extending its byte buffer beyond the budget.
struct JpegOutput {
    bytes: Vec<u8>,
    max_bytes: usize,
    limit_hit: bool,
    allocation_failed: bool,
}

impl JpegOutput {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::new(),
            max_bytes,
            limit_hit: false,
            allocation_failed: false,
        }
    }
}

impl io::Write for JpegOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let Some(length) = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .filter(|&n| n <= self.max_bytes)
        else {
            self.limit_hit = true;
            return Err(io::ErrorKind::FileTooLarge.into());
        };
        if length > self.bytes.capacity() {
            let capacity = self
                .bytes
                .capacity()
                .saturating_mul(2)
                .max(1024)
                .max(length);
            let capacity = capacity.min(self.max_bytes);
            if self
                .bytes
                .try_reserve_exact(capacity - self.bytes.len())
                .is_err()
            {
                self.allocation_failed = true;
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
pub(crate) fn heic_fixture_for_worker_test() -> Vec<u8> {
    tests::white_heic()
}

#[cfg(test)]
mod tests {
    use super::*;

    // The 485-byte flat-white-16.heic fixture is generated synthetic imagery.
    // Source: https://github.com/tbraun96/heic-rs/tree/v0.1.4/tests/fixtures
    // Its README explicitly licenses the fixtures MIT OR Apache-2.0.
    //
    // MIT License
    // Copyright (c) 2026 Thomas Braun
    //
    // Permission is hereby granted, free of charge, to any person obtaining a copy
    // of this software and associated documentation files (the "Software"), to deal
    // in the Software without restriction, including without limitation the rights
    // to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
    // copies of the Software, and to permit persons to whom the Software is
    // furnished to do so, subject to the following conditions:
    //
    // The above copyright notice and this permission notice shall be included in all
    // copies or substantial portions of the Software.
    //
    // THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
    // IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
    // FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
    // AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
    // LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
    // OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
    // SOFTWARE.
    const WHITE_HEIC: &str = concat!(
        "000000246674797068656963000000006d6966314d6950726d6961664d69484268656963000001866d65746100000000",
        "0000002168646c72000000000000000070696374000000000000000000000000000000002464696e660000001c647265",
        "6600000000000000010000000c75726c20000000010000000e7069746d0000000000010000002369696e660000000000",
        "0100000015696e666502000000000100006876633100000000e669707270000000c56970636f00000013636f6c726e63",
        "6c78000200020006800000000c636c6c6900cb004000000014697370650000000000000010000000100000000969726f",
        "7400000000107069786900000000030808080000007168766343010370000000b000000000001ef000fcfdf8f800000b",
        "03a00001001740010c01ffff037000000300b0000003000003001e7024a100010023420101037000000300b000000300",
        "0003001ea0142041c0930ce21ee45954dc08081802a2000100094401c06172c84453640000001969706d610000000000",
        "0000010001068102030586840000001e696c6f630000000044000001000100000001000001ba0000002b000000016d64",
        "6174000000000000003b000000272801afa2c647ec65d5bf2e44da21fffe9ed6a8e3995fd0223ad87ffd88b266d44690",
        "f8f408fe6c",
    );

    pub(super) fn white_heic() -> Vec<u8> {
        WHITE_HEIC
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let pair = std::str::from_utf8(pair).expect("fixture hex is ASCII");
                u8::from_str_radix(pair, 16).expect("fixture contains hexadecimal bytes")
            })
            .collect()
    }

    fn limits() -> HeicConversionLimits {
        // Synthetic test budgets are not application defaults.
        HeicConversionLimits {
            max_input_bytes: 4096,
            max_output_bytes: 4096,
            max_metadata_entries: 4096,
            max_pixels: 4096,
            // Apple's tiny visible fixture has a cropped 160x64 coded picture.
            max_decoded_pixels: 16_384,
        }
    }

    fn box_bytes(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let length = u32::try_from(body.len() + 8).expect("synthetic box fits u32");
        let mut bytes = length.to_be_bytes().to_vec();
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(body);
        bytes
    }

    fn full_box_bytes(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![version];
        bytes.extend_from_slice(&flags.to_be_bytes()[1..]);
        bytes.extend_from_slice(body);
        box_bytes(kind, &bytes)
    }

    fn info_entry(id: u16, kind: &[u8; 4]) -> Vec<u8> {
        let mut body = id.to_be_bytes().to_vec();
        body.extend_from_slice(&0u16.to_be_bytes());
        body.extend_from_slice(kind);
        body.push(0);
        full_box_bytes(b"infe", 2, 0, &body)
    }

    // Construct a real one-tile grid from the licensed fixture's HEVC payload.
    // The optional alpha link names an intentionally unusable auxiliary item.
    // That item must never be parsed or decoded by reference-compatible display.
    fn grid_fixture(unknown_essential: Option<u16>, linked_unusable_alpha: bool) -> Vec<u8> {
        let input = white_heic();
        let context = Context::open(&input).expect("licensed fixture container");
        let coded = context
            .item_data(context.meta.primary)
            .expect("licensed HEVC payload");
        let mut properties = Vec::new();
        for property in &context.meta.props.boxes {
            let kind = if property.is(b"clli") && unknown_essential.is_some() {
                b"zzzz"
            } else {
                &property.boxtype
            };
            properties.extend_from_slice(&box_bytes(kind, property.payload));
        }
        if linked_unusable_alpha {
            properties.extend_from_slice(&full_box_bytes(
                b"auxC",
                0,
                0,
                b"urn:mpeg:hevc:2015:auxid:1\0",
            ));
            properties.extend_from_slice(&box_bytes(b"hvcC", &[255]));
        }
        let count = if linked_unusable_alpha { 3u16 } else { 2u16 };
        let mut entries = count.to_be_bytes().to_vec();
        entries.extend_from_slice(&info_entry(1, b"grid"));
        entries.extend_from_slice(&info_entry(2, b"hvc1"));
        if linked_unusable_alpha {
            entries.extend_from_slice(&info_entry(3, b"hvc1"));
        }
        let mut associations = u32::from(count).to_be_bytes().to_vec();
        associations.extend_from_slice(&1u16.to_be_bytes());
        if unknown_essential == Some(1) {
            associations.extend_from_slice(&[3, 0x83, 0x84, 0x82]);
        } else {
            associations.extend_from_slice(&[2, 0x83, 0x84]);
        }
        associations.extend_from_slice(&2u16.to_be_bytes());
        associations.extend_from_slice(&[
            6,
            0x81,
            if unknown_essential == Some(2) {
                0x82
            } else {
                2
            },
            3,
            5,
            0x86,
            0x84,
        ]);
        if linked_unusable_alpha {
            associations.extend_from_slice(&3u16.to_be_bytes());
            associations.extend_from_slice(&[2, 0x87, 0x88]);
        }
        let mut property_box = box_bytes(b"ipco", &properties);
        property_box.extend_from_slice(&full_box_bytes(b"ipma", 0, 0, &associations));
        let mut references = box_bytes(b"dimg", &[0, 1, 0, 1, 0, 2]);
        if linked_unusable_alpha {
            references.extend_from_slice(&box_bytes(b"auxl", &[0, 3, 0, 1, 0, 1]));
        }
        let mut locations = vec![0x44, 0, 0, 2];
        for (id, offset, length) in [(1u16, 0u32, 8u32), (2, 8, coded.len() as u32)] {
            locations.extend_from_slice(&id.to_be_bytes());
            locations.extend_from_slice(&[0, 1, 0, 0, 0, 1]);
            locations.extend_from_slice(&offset.to_be_bytes());
            locations.extend_from_slice(&length.to_be_bytes());
        }
        let mut data = vec![0, 0, 0, 0, 0, 16, 0, 16];
        data.extend_from_slice(&coded);
        let mut handler = vec![0; 4];
        handler.extend_from_slice(b"pict");
        handler.extend_from_slice(&[0; 13]);
        let mut meta = full_box_bytes(b"hdlr", 0, 0, &handler);
        meta.extend_from_slice(&full_box_bytes(b"pitm", 0, 0, &[0, 1]));
        meta.extend_from_slice(&full_box_bytes(b"iinf", 0, 0, &entries));
        meta.extend_from_slice(&box_bytes(b"iprp", &property_box));
        meta.extend_from_slice(&full_box_bytes(b"iref", 0, 0, &references));
        meta.extend_from_slice(&full_box_bytes(b"iloc", 1, 0, &locations));
        meta.extend_from_slice(&box_bytes(b"idat", &data));
        let ftyp = BoxIter::new(&input)
            .require(b"ftyp", "ftyp")
            .expect("fixture brand");
        let mut file = box_bytes(b"ftyp", ftyp.payload);
        file.extend_from_slice(&full_box_bytes(b"meta", 0, 0, &meta));
        file
    }

    #[test]
    fn zero_width_iloc_extent_amplification_is_refused_before_context_open() {
        let mut locations = vec![0, 0, 0, 2];
        for id in [1u16, 2] {
            locations.extend_from_slice(&id.to_be_bytes());
            locations.extend_from_slice(&[0, 0, 255, 255]);
        }
        let iloc = full_box_bytes(b"iloc", 0, 0, &locations);
        let mut meta = full_box_bytes(b"hdlr", 0, 0, &[0, 0, 0, 0, b'p', b'i', b'c', b't']);
        meta.extend_from_slice(&full_box_bytes(b"pitm", 0, 0, &[0, 1]));
        meta.extend_from_slice(&iloc);
        let fixture = white_heic();
        let ftyp = BoxIter::new(&fixture)
            .require(b"ftyp", "ftyp")
            .expect("fixture brand");
        let mut content = box_bytes(b"ftyp", ftyp.payload);
        content.extend_from_slice(&full_box_bytes(b"meta", 0, 0, &meta));
        assert!(
            content.len() < 256,
            "the allocation-amplifying container is tiny"
        );
        assert_eq!(
            preflight_metadata(&content, 4096),
            Err(ImageConversionError::InvalidImage),
            "zero-width fields cannot request 65535 Extent allocations per item"
        );
        assert_eq!(
            convert_heic_to_jpeg("a.heic", &content, limits()),
            Err(ImageConversionError::InvalidImage),
            "the guard runs before Context::open"
        );
    }

    #[test]
    fn container_table_budgets_cover_all_context_open_vector_families() {
        let input = grid_fixture(None, false);
        assert!(
            preflight_metadata(&input, 4096).is_ok(),
            "complete normal tables pass"
        );
        assert_eq!(
            preflight_metadata(&input, 0),
            Err(ImageConversionError::MetadataLimit),
            "an exhausted metadata budget fails before allocating parsed tables"
        );
        for (kind, version, body) in [
            (b"iinf", 0, vec![0, 1, 0, 0, 0, 8, b'i', b'n', b'f', b'e']),
            (b"iloc", 0, vec![0, 0, 0, 1, 0, 1, 0, 0, 0, 1]),
            (b"iref", 0, box_bytes(b"dimg", &[0, 1, 0, 1, 0, 2])),
            (b"iprp", 0, box_bytes(b"ipco", &box_bytes(b"zzzz", &[]))),
        ] {
            let child = if kind == b"iprp" {
                box_bytes(kind, &body)
            } else {
                full_box_bytes(kind, version, 0, &body)
            };
            let content = full_box_bytes(b"meta", 0, 0, &child);
            assert_eq!(
                preflight_metadata(&content, 0),
                Err(ImageConversionError::MetadataLimit),
                "each allocating container-table family requires budget before parsing"
            );
        }
        let associations = full_box_bytes(b"ipma", 0, 0, &[0, 0, 0, 1, 0, 1, 255]);
        let properties = box_bytes(b"iprp", &associations);
        let content = full_box_bytes(b"meta", 0, 0, &properties);
        assert_eq!(
            preflight_metadata(&content, 32),
            Err(ImageConversionError::MetadataLimit),
            "association reservation is checked even when its payload is missing"
        );
    }

    #[test]
    fn actual_grid_decode_ignores_linked_auxiliary_alpha() {
        let plain = convert_heic_to_jpeg("a.heic", &grid_fixture(None, false), limits())
            .expect("the real one-tile grid decodes");
        let with_alpha = convert_heic_to_jpeg("a.heic", &grid_fixture(None, true), limits())
            .expect("unselected alpha properties and data are never interpreted");
        assert_eq!(
            plain, with_alpha,
            "heic2any's opaque display never decodes auxiliary alpha"
        );
    }

    #[test]
    fn primary_and_selected_grid_tile_unknown_essential_properties_are_refused() {
        for item in [1, 2] {
            assert_eq!(
                convert_heic_to_jpeg("a.heic", &grid_fixture(Some(item), false), limits()),
                Err(ImageConversionError::UnsupportedImage),
                "strict essential-property handling covers both the primary grid and its tiles"
            );
        }
    }

    #[test]
    fn real_heic_converts_to_a_complete_white_jpeg() {
        let input = white_heic();
        assert_eq!(input.len(), 485, "the upstream fixture remains complete");
        let prepared = convert_heic_to_jpeg("photo.HEIF", &input, limits())
            .expect("licensed synthetic HEIC decodes and encodes");
        assert_eq!(
            prepared.filename, "photo.jpg",
            "conversion replaces the final suffix"
        );
        assert_eq!(
            prepared.media_type, "image/jpeg",
            "output MIME is normalized"
        );
        assert!(
            prepared.content.ends_with(&[0xff, 0xd9]),
            "the JPEG has an end marker"
        );
        let jpeg = image::load_from_memory_with_format(&prepared.content, image::ImageFormat::Jpeg)
            .expect("the actual JPEG decoder accepts the result");
        assert_eq!(
            (jpeg.width(), jpeg.height()),
            (16, 16),
            "dimensions survive conversion"
        );
        assert!(
            jpeg.to_rgb8()
                .as_raw()
                .iter()
                .all(|&channel| channel >= 253),
            "the synthetic white image remains white after lossy encoding"
        );
    }

    #[test]
    fn filename_rules_match_the_shared_final_extension_replacement() {
        for (input, expected) in [
            ("a.heic", "a.jpg"),
            ("a.hEiF", "a.jpg"),
            ("a.b.HEIC", "a.b.jpg"),
            (".heif", ".jpg"),
            ("a.heic.txt", "a.heic.txt"),
            ("a.jpg", "a.jpg"),
            ("no-suffix", "no-suffix"),
            ("", ""),
        ] {
            assert_eq!(
                jpeg_filename(input),
                expected,
                "only the final HEIC/HEIF suffix changes"
            );
        }
        let prepared = convert_heic_to_jpeg("mime-routed.bin", &white_heic(), limits())
            .expect("a declared MIME can route bytes without a HEIC suffix");
        assert_eq!(
            prepared.filename, "mime-routed.bin",
            "MIME routing does not append a suffix"
        );
    }

    #[test]
    fn input_pixel_and_output_limits_are_independent_and_inclusive() {
        let input = white_heic();
        let prepared = convert_heic_to_jpeg("a.heic", &input, limits()).expect("valid fixture");
        let exact = HeicConversionLimits {
            max_input_bytes: input.len(),
            max_output_bytes: prepared.content.len(),
            ..limits()
        };
        assert!(
            convert_heic_to_jpeg("a.heic", &input, exact).is_ok(),
            "exact byte budgets pass"
        );
        for (limited, expected) in [
            (
                HeicConversionLimits {
                    max_input_bytes: input.len() - 1,
                    ..exact
                },
                ImageConversionError::InputTooLarge,
            ),
            (
                HeicConversionLimits {
                    max_output_bytes: prepared.content.len() - 1,
                    ..exact
                },
                ImageConversionError::OutputTooLarge,
            ),
            (
                HeicConversionLimits {
                    max_output_bytes: 0,
                    ..exact
                },
                ImageConversionError::OutputTooLarge,
            ),
            (
                HeicConversionLimits {
                    max_metadata_entries: 0,
                    ..exact
                },
                ImageConversionError::MetadataLimit,
            ),
            (
                HeicConversionLimits {
                    max_pixels: 255,
                    ..exact
                },
                ImageConversionError::PixelLimit,
            ),
            (
                HeicConversionLimits {
                    max_decoded_pixels: 1,
                    ..exact
                },
                ImageConversionError::PixelLimit,
            ),
        ] {
            assert_eq!(
                convert_heic_to_jpeg("a.heic", &input, limited),
                Err(expected),
                "one budget cannot be replaced by another"
            );
        }
    }

    #[test]
    fn every_truncated_prefix_and_malformed_content_returns_an_error() {
        let input = white_heic();
        for length in 0..input.len() {
            assert!(
                convert_heic_to_jpeg("a.heic", &input[..length], limits()).is_err(),
                "an incomplete input cannot become a successful attachment"
            );
        }
        for bytes in [b"not an image".as_slice(), &[0; 64], b"\xff\xd8\xff\xd9"] {
            assert!(
                convert_heic_to_jpeg("a.heic", bytes, limits()).is_err(),
                "a HEIC suffix grants no decoding authority"
            );
        }
    }

    #[test]
    fn declared_zero_huge_and_over_budget_dimensions_fail_before_decode() {
        let input = white_heic();
        let ispe = input
            .windows(4)
            .position(|bytes| bytes == b"ispe")
            .expect("the licensed fixture contains spatial extents");
        for (width, height, expected) in [
            // The pinned property parser rejects zero ispe dimensions before
            // Context::open can expose them to the wrapper's pixel preflight.
            (0u32, 16u32, ImageConversionError::InvalidImage),
            (u32::MAX, u32::MAX, ImageConversionError::PixelLimit),
            (65, 65, ImageConversionError::PixelLimit),
        ] {
            let mut changed = input.clone();
            changed[ispe + 8..ispe + 12].copy_from_slice(&width.to_be_bytes());
            changed[ispe + 12..ispe + 16].copy_from_slice(&height.to_be_bytes());
            assert_eq!(
                convert_heic_to_jpeg("a.heic", &changed, limits()),
                Err(expected),
                "container dimensions are checked before allocating decoded pixels"
            );
        }
        assert_eq!(
            checked_pixel_bytes(u32::MAX, u32::MAX, u64::MAX),
            Err(ImageConversionError::PixelLimit),
            "RGBA byte arithmetic cannot overflow"
        );
    }

    #[test]
    fn sps_coded_budget_cannot_be_bypassed_by_small_container_dimensions() {
        let input = white_heic();
        let context = Context::open(&input).expect("valid fixture container");
        let props = context
            .props(context.meta.primary)
            .expect("primary properties");
        let config = props.hvcc.as_ref().expect("HEVC configuration");
        let sps = config
            .parameter_sets()
            .into_iter()
            .find(|nal| (nal[0] >> 1) & 0x3f == 33)
            .expect("the real fixture carries a sequence parameter set");
        let (width, height) = sps_dimensions(sps).expect("the actual SPS prefix parses");
        let area = u64::from(width) * u64::from(height);
        let mut decoded_pixels = 0;
        let exact = HeicConversionLimits {
            max_decoded_pixels: area,
            ..limits()
        };
        preflight_coded(&props, exact, &mut decoded_pixels).expect("exact coded area passes");
        assert_eq!(
            decoded_pixels, area,
            "uncropped coded area consumes the budget"
        );
        assert_eq!(
            preflight_coded(&props, exact, &mut decoded_pixels),
            Err(ImageConversionError::PixelLimit),
            "repeated tile decodes consume separate area"
        );
        let short = HeicConversionLimits {
            max_decoded_pixels: area - 1,
            ..limits()
        };
        assert_eq!(
            preflight_coded(&props, short, &mut 0),
            Err(ImageConversionError::PixelLimit),
            "the container's small ispe cannot hide coded area"
        );
        for length in 0..2 {
            assert_eq!(
                sps_dimensions(&sps[..length]),
                Err(ImageConversionError::InvalidImage),
                "short NAL headers cannot panic"
            );
        }
    }

    #[test]
    fn ycbcr_uses_the_reference_matrix_and_nearest_chroma_before_jpeg_encoding() {
        let frame = Frame {
            width: 4,
            height: 2,
            bit_depth: 8,
            chroma: heic_rs::ChromaFormat::Yuv420,
            y: vec![81; 8],
            cb: vec![90, 128],
            cr: vec![240, 128],
            y_stride: 4,
            c_stride: 2,
        };
        let rgb = reference_rgb(&frame, 8).expect("valid synthetic YCbCr planes");
        assert_eq!(
            rgb.layout,
            PixelLayout::Rgb8,
            "the reference discards auxiliary alpha"
        );
        assert_eq!(
            rgb.data,
            [
                254, 0, 0, 254, 0, 0, 76, 76, 76, 76, 76, 76, 254, 0, 0, 254, 0, 0, 76, 76, 76, 76,
                76, 76,
            ],
            "BT.601 and nearest 2x2 chroma match heic2any 0.0.4's display loop"
        );
    }

    #[test]
    fn nal_bits_remove_emulation_bytes_and_reject_truncation_and_overlong_codes() {
        let mut bits = NalBits::new(&[0, 0, 3, 1, 0xff]);
        assert_eq!(
            bits.read(16),
            Ok(0),
            "the first 2 zero bytes remain payload"
        );
        assert_eq!(
            bits.read(8),
            Ok(1),
            "emulation prevention is removed before reading bits"
        );
        assert_eq!(
            bits.read(8),
            Ok(255),
            "the next payload byte remains intact"
        );
        assert_eq!(
            bits.bit(),
            Err(ImageConversionError::InvalidImage),
            "reading beyond the NAL returns an error"
        );
        let mut bits = NalBits::new(&[0; 4]);
        assert_eq!(
            bits.unsigned_exp_golomb(),
            Err(ImageConversionError::InvalidImage),
            "an overlong dimension code cannot overflow its shift"
        );
    }

    #[test]
    fn bounded_jpeg_writer_never_returns_a_truncated_success() {
        use std::io::Write;
        let mut output = JpegOutput::new(3);
        output
            .write_all(b"abc")
            .expect("the exact output budget is accepted");
        assert!(output.write_all(b"d").is_err(), "the next byte is rejected");
        assert!(
            output.limit_hit,
            "the caller can distinguish the output limit"
        );
        assert_eq!(
            output.bytes, b"abc",
            "a failed write does not append a partial chunk"
        );
    }

    #[test]
    fn failures_and_output_debug_never_include_input_or_filename_content() {
        let filename = "private-sensitive-name.heic";
        let error = convert_heic_to_jpeg(filename, b"private-input-payload", limits())
            .expect_err("malformed bytes fail");
        assert_eq!(
            format!("{error:?}"),
            "InvalidImage",
            "failure diagnostics contain only a kind"
        );
        let prepared =
            convert_heic_to_jpeg(filename, &white_heic(), limits()).expect("valid fixture");
        let debug = format!("{prepared:?}");
        assert!(
            !debug.contains("private"),
            "normalized debug omits the filename"
        );
        assert!(
            !debug.contains("255, 216"),
            "normalized debug omits JPEG bytes"
        );
    }
}
