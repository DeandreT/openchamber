use super::*;

use image::codecs::bmp::BmpEncoder;
use image::codecs::ico::IcoEncoder;
use image::codecs::pnm::PnmEncoder;
use image::codecs::tiff::TiffEncoder;
use image::{ImageDecoder, ImageEncoder};

// Distinct rows and columns expose BMP row order and TIFF orientation mistakes.
const RGB: [u8; 18] = [
    255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0, 255, 0, 255, 0, 255, 255,
];

fn fixture(format: ImageFormat) -> Vec<u8> {
    let mut bytes = Vec::new();
    match format {
        ImageFormat::Bmp => BmpEncoder::new(&mut bytes)
            .encode(&RGB, 2, 3, ExtendedColorType::Rgb8)
            .expect("the synthetic BMP encodes"),
        ImageFormat::Tiff => {
            let mut encoder = TiffEncoder::new(Cursor::new(&mut bytes));
            encoder
                .set_icc_profile(b"private-test-profile".to_vec())
                .expect("TIFF accepts the synthetic ICC field");
            encoder
                .write_image(&RGB, 2, 3, ExtendedColorType::Rgb8)
                .expect("the synthetic TIFF encodes");
        }
        ImageFormat::Pnm => PnmEncoder::new(&mut bytes)
            .write_image(&RGB, 2, 3, ExtendedColorType::Rgb8)
            .expect("the synthetic PNM encodes"),
        ImageFormat::Ico => {
            let rgb = image::RgbImage::from_raw(2, 3, RGB.to_vec())
                .expect("RGB fixture dimensions match");
            let rgba = DynamicImage::ImageRgb8(rgb).into_rgba8();
            IcoEncoder::new(&mut bytes)
                .write_image(rgba.as_raw(), 2, 3, ExtendedColorType::Rgba8)
                .expect("the synthetic ICO encodes");
        }
        _ => panic!("the fixture supports converted clipboard formats only"),
    }
    bytes
}

fn prepared_png(bytes: &[u8], format: ImageFormat) -> NormalizedAttachmentFile {
    prepare_clipboard_image(
        bytes,
        format,
        format!("pasted-image-7.{}", format.extension()),
    )
    .expect("a complete bounded image converts")
}

fn decode_png(bytes: &[u8]) -> DynamicImage {
    image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .expect("the actual PNG decoder accepts the complete result")
}

#[test]
fn actual_bmp_tiff_ico_and_pnm_become_complete_pngs() {
    for format in [
        ImageFormat::Bmp,
        ImageFormat::Tiff,
        ImageFormat::Ico,
        ImageFormat::Pnm,
    ] {
        let input = fixture(format);
        let output = prepared_png(&input, format);
        assert_eq!(
            output.filename, "pasted-image-7.png",
            "the basename survives conversion"
        );
        assert_eq!(
            output.media_type, "image/png",
            "converted bytes have PNG MIME"
        );
        assert_ne!(
            output.content, input,
            "conversion must produce new encoded bytes"
        );
        let decoded = decode_png(&output.content);
        assert_eq!(
            (decoded.width(), decoded.height()),
            (2, 3),
            "geometry survives"
        );
        assert_eq!(
            decoded.into_rgb8().as_raw(),
            &RGB,
            "pixel values and row order survive"
        );
    }
}

#[test]
fn accepted_encodings_are_retained_without_pixel_decoding() {
    // Intentionally header-only raster samples prove this path performs no pixel decode.
    for (format, bytes) in [
        (ImageFormat::Png, b"\x89PNG\r\n\x1a\n".as_slice()),
        (ImageFormat::Jpeg, b"\xff\xd8\xff".as_slice()),
        (ImageFormat::Gif, b"GIF89a".as_slice()),
        (ImageFormat::Webp, b"RIFF\x00\x00\x00\x00WEBP".as_slice()),
        (
            ImageFormat::Svg,
            b"<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>".as_slice(),
        ),
    ] {
        let filename = format!("pasted-image-8.{}", format.extension());
        let output = prepare_clipboard_image(bytes, format, filename.clone())
            .expect("existing accepted encodings require no raster conversion");
        assert_eq!(
            output.filename, filename,
            "pass-through names stay unchanged"
        );
        assert_eq!(
            output.media_type,
            format.mime_type(),
            "the encoded type remains exact"
        );
        assert_eq!(
            output.content, bytes,
            "pass-through does not re-encode or rewrite bytes"
        );
    }
}

#[test]
fn png_bytes_and_metadata_are_kept_exactly() {
    let rgba = image::RgbaImage::from_pixel(1, 1, image::Rgba([3, 5, 7, 11]));
    let mut bytes = Vec::new();
    let mut encoder = PngEncoder::new(&mut bytes);
    encoder
        .set_exif_metadata(b"private-test-metadata".to_vec())
        .expect("PNG accepts EXIF bytes");
    encoder
        .write_image(rgba.as_raw(), 1, 1, ExtendedColorType::Rgba8)
        .expect("the synthetic PNG encodes");
    let output = prepare_clipboard_image(&bytes, ImageFormat::Png, "original.PNG".to_owned())
        .expect("an already accepted PNG passes through");
    assert_eq!(
        output.content, bytes,
        "existing PNG metadata is not rewritten"
    );
    assert_eq!(
        output.filename, "original.PNG",
        "an existing filename retains its spelling"
    );
}

#[test]
fn actual_input_length_is_checked_before_every_format_branch() {
    let mut bytes = vec![0; MAX_INLINE_ATTACHMENT_BYTES + 1];
    bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    for format in [
        ImageFormat::Png,
        ImageFormat::Svg,
        ImageFormat::Bmp,
        ImageFormat::Tiff,
    ] {
        assert_eq!(
            prepare_clipboard_image(&bytes, format, "image.bin".to_owned()),
            Err(ClipboardImageError::InputTooLarge),
            "a reported format cannot bypass the complete-input ceiling"
        );
    }
    bytes.pop();
    let output = prepare_clipboard_image(&bytes, ImageFormat::Png, "image.png".to_owned())
        .expect("the exact input ceiling is inclusive");
    assert_eq!(output.content, bytes, "admission does not truncate bytes");
    assert_eq!(
        MAX_PREPARED_ATTACHMENT_BYTES,
        20 * 1024 * 1024,
        "transport owns the output cap"
    );
}

#[test]
fn invalid_truncated_and_misreported_conversion_inputs_fail() {
    for format in [
        ImageFormat::Bmp,
        ImageFormat::Tiff,
        ImageFormat::Ico,
        ImageFormat::Pnm,
    ] {
        let bytes = fixture(format);
        for truncated in [&bytes[..0], &bytes[..1], &bytes[..bytes.len() / 2]] {
            assert!(
                prepare_clipboard_image(truncated, format, "image.bin".to_owned()).is_err(),
                "missing pixel or container data cannot become a PNG"
            );
        }
        for invalid in [b"private-invalid-content".as_slice(), &[0; 64]] {
            assert_eq!(
                prepare_clipboard_image(invalid, format, "private-name.bin".to_owned()),
                Err(ClipboardImageError::Invalid),
                "a format label never grants decoding authority to unrelated bytes"
            );
        }
        let other = if format == ImageFormat::Bmp {
            ImageFormat::Tiff
        } else {
            ImageFormat::Bmp
        };
        assert_eq!(
            prepare_clipboard_image(&bytes, other, "image.bin".to_owned()),
            Err(ClipboardImageError::Invalid),
            "actual encoded bytes must match the reported format"
        );
    }
    assert_eq!(
        prepare_clipboard_image(
            &fixture(ImageFormat::Bmp),
            ImageFormat::Png,
            "image.png".to_owned(),
        ),
        Err(ClipboardImageError::Invalid),
        "a pass-through label cannot disguise a conversion format"
    );
    assert_eq!(
        prepare_clipboard_image(
            b"\0\0\0\x18ftypheic",
            ImageFormat::Tiff,
            "image.tiff".to_owned(),
        ),
        Err(ClipboardImageError::Invalid),
        "HEIC bytes never enter an in-process clipboard decoder"
    );
}

#[test]
fn huge_bmp_and_pnm_headers_are_refused_before_missing_pixels_are_read() {
    let mut bmp = fixture(ImageFormat::Bmp);
    bmp[18..22].copy_from_slice(&65_535u32.to_le_bytes());
    bmp[22..26].copy_from_slice(&65_535u32.to_le_bytes());
    for (format, bytes) in [
        (ImageFormat::Bmp, bmp.as_slice()),
        (ImageFormat::Pnm, b"P6\n65535 65535\n255\n".as_slice()),
        (ImageFormat::Pnm, b"P6\n4294967295 1\n255\n".as_slice()),
    ] {
        assert_eq!(
            prepare_clipboard_image(bytes, format, "image.bin".to_owned()),
            Err(ClipboardImageError::Limit),
            "the header's dimensions fail before truncated pixel data is decoded"
        );
    }
    assert_eq!(
        checked_pixel_bytes(0, 1),
        Err(ClipboardImageError::Invalid),
        "zero area is invalid"
    );
    assert_eq!(
        checked_pixel_bytes(u32::MAX, u32::MAX),
        Err(ClipboardImageError::Limit),
        "arithmetic stays checked"
    );
    assert_eq!(
        checked_pixel_bytes(MAX_IMAGE_DIMENSION, 1),
        Ok(MAX_RASTER_WORK_BYTES / 2),
        "the derived axis ceiling is inclusive"
    );
}

fn tiff_u16(bytes: &[u8], little: bool) -> u16 {
    let field = bytes[..2]
        .try_into()
        .expect("fixture TIFF field has 2 bytes");
    if little {
        u16::from_le_bytes(field)
    } else {
        u16::from_be_bytes(field)
    }
}

fn tiff_u32(bytes: &[u8], little: bool) -> u32 {
    let field = bytes[..4]
        .try_into()
        .expect("fixture TIFF field has 4 bytes");
    if little {
        u32::from_le_bytes(field)
    } else {
        u32::from_be_bytes(field)
    }
}

fn tiff_put_u16(bytes: &mut [u8], value: u16, little: bool) {
    let encoded = if little {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    bytes[..2].copy_from_slice(&encoded);
}

fn tiff_put_u32(bytes: &mut [u8], value: u32, little: bool) {
    let encoded = if little {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    bytes[..4].copy_from_slice(&encoded);
}

#[test]
fn huge_tiff_dimensions_fail_before_pixel_decoding() {
    let mut bytes = fixture(ImageFormat::Tiff);
    let little = &bytes[..2] == b"II";
    let directory = tiff_u32(&bytes[4..8], little) as usize;
    let count = usize::from(tiff_u16(&bytes[directory..], little));
    for entry in bytes[directory + 2..directory + 2 + count * 12].chunks_exact_mut(12) {
        // Retain one strip for the larger declared height so constructor consistency checks
        // pass and the wrapper's pre-pixel allocation checks are the reason for refusal.
        if matches!(tiff_u16(entry, little), 256 | 257 | 278) {
            assert_eq!(
                tiff_u16(&entry[2..], little),
                4,
                "fixture dimensions and rows per strip are LONG fields"
            );
            tiff_put_u32(&mut entry[8..], 65_535, little);
        }
    }
    assert_eq!(
        prepare_clipboard_image(&bytes, ImageFormat::Tiff, "image.tiff".to_owned()),
        Err(ClipboardImageError::Limit),
        "huge TIFF geometry cannot allocate a decoded image"
    );
}

#[test]
fn tiff_orientation_is_applied_without_exporting_source_metadata() {
    let mut bytes = fixture(ImageFormat::Tiff);
    let little = &bytes[..2] == b"II";
    let directory = tiff_u32(&bytes[4..8], little) as usize;
    let count = usize::from(tiff_u16(&bytes[directory..], little));
    let mut entries: Vec<[u8; 12]> = bytes[directory + 2..directory + 2 + count * 12]
        .chunks_exact(12)
        .map(|entry| {
            entry
                .try_into()
                .expect("fixture directory entries have 12 bytes")
        })
        .collect();
    let mut orientation = [0; 12];
    tiff_put_u16(&mut orientation, 274, little);
    tiff_put_u16(&mut orientation[2..], 3, little);
    tiff_put_u32(&mut orientation[4..], 1, little);
    tiff_put_u16(&mut orientation[8..], 6, little); // EXIF 6 is clockwise 90 degrees.
    entries.push(orientation);
    entries.sort_by_key(|entry| tiff_u16(entry, little));
    // Append a replacement IFD so existing pixel offsets need no rewriting.
    let replacement = bytes.len();
    bytes.resize(replacement + 2 + (count + 1) * 12 + 4, 0);
    tiff_put_u32(&mut bytes[4..8], replacement as u32, little);
    tiff_put_u16(&mut bytes[replacement..], (count + 1) as u16, little);
    let targets = bytes[replacement + 2..replacement + 2 + entries.len() * 12]
        .chunks_exact_mut(12)
        .zip(entries);
    for (target, entry) in targets {
        target.copy_from_slice(&entry);
    }
    let output = prepared_png(&bytes, ImageFormat::Tiff);
    let decoded = decode_png(&output.content);
    let expected = image::imageops::rotate90(
        &image::RgbImage::from_raw(2, 3, RGB.to_vec()).expect("fixture dimensions match"),
    );
    assert_eq!(
        decoded.into_rgb8(),
        expected,
        "the PNG matches GPUI's oriented static display"
    );
    let mut decoder =
        ImageReader::with_format(Cursor::new(&output.content), image::ImageFormat::Png)
            .into_decoder()
            .expect("converted PNG headers are valid");
    assert_eq!(
        decoder.exif_metadata().expect("EXIF inspection succeeds"),
        None,
        "orientation is not exported again"
    );
    assert_eq!(
        decoder.icc_profile().expect("ICC inspection succeeds"),
        None,
        "source profiles are not exported"
    );
}

#[test]
fn png_filename_replaces_only_the_final_extension() {
    for (input, expected) in [
        ("pasted-image-1.BMP", "pasted-image-1.png"),
        ("a.b.tiff", "a.b.png"),
        ("image.ico", "image.png"),
        ("image.pnm", "image.png"),
        ("no-extension", "no-extension.png"),
    ] {
        assert_eq!(
            png_filename(input),
            expected,
            "the display stem is retained"
        );
    }
}

fn png_crc(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[test]
fn ico_nested_png_receives_dimension_limits_before_decoding() {
    let mut bytes = fixture(ImageFormat::Ico);
    let offset = u32::from_le_bytes(bytes[18..22].try_into().expect("ICO offset is 4 bytes"));
    let png = &mut bytes[offset as usize..];
    png[16..20].copy_from_slice(&(MAX_IMAGE_DIMENSION + 1).to_be_bytes());
    let crc = png_crc(&png[12..29]);
    png[29..33].copy_from_slice(&crc.to_be_bytes());
    assert_eq!(
        prepare_clipboard_image(&bytes, ImageFormat::Ico, "image.ico".to_owned()),
        Err(ClipboardImageError::Limit),
        "the inner PNG is checked before constructing an unlimited ICO decoder"
    );
}

#[test]
fn ico_directory_dimensions_and_complete_selected_payload_are_required() {
    let mut bytes = fixture(ImageFormat::Ico);
    bytes[6] = 1; // Actual PNG width is 2.
    assert_eq!(
        prepare_clipboard_image(&bytes, ImageFormat::Ico, "image.ico".to_owned()),
        Err(ClipboardImageError::Invalid),
        "a nested PNG must match the selected directory entry"
    );
    let mut bytes = fixture(ImageFormat::Ico);
    bytes[14..18].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
        prepare_clipboard_image(&bytes, ImageFormat::Ico, "image.ico".to_owned()),
        Err(ClipboardImageError::Invalid),
        "declared payload lengths cannot authorize a truncated selected image"
    );
}

#[test]
fn append_only_png_writer_enforces_the_inclusive_cap_and_sticky_failure() {
    use std::io::Write;

    let mut output = PngOutput::new(3);
    output
        .write_all(b"abc")
        .expect("the exact output ceiling is inclusive");
    assert!(
        output.write_all(b"de").is_err(),
        "a write crossing the cap is refused wholly"
    );
    assert_eq!(
        output.error,
        Some(ClipboardImageError::OutputTooLarge),
        "the failure has a typed kind"
    );
    assert_eq!(
        output.bytes, b"abc",
        "failed writes leave all existing bytes unchanged"
    );
    assert!(
        output.write_all(b"x").is_err(),
        "later writes cannot recover a failed output"
    );
    assert_eq!(
        output.bytes, b"abc",
        "there is no seek or backfill operation"
    );

    let image = image::RgbaImage::from_pixel(2, 3, image::Rgba([1, 2, 3, 4]));
    let complete = encode_png(&image, MAX_PREPARED_ATTACHMENT_BYTES).expect("tiny PNG encodes");
    assert_eq!(
        encode_png(&image, complete.len()).expect("exact encoder output cap passes"),
        complete,
        "the output is complete"
    );
    assert_eq!(
        encode_png(&image, complete.len() - 1),
        Err(ClipboardImageError::OutputTooLarge),
        "a capped encoder cannot return truncated success"
    );
    assert_eq!(
        encode_png(&image, 0),
        Err(ClipboardImageError::OutputTooLarge),
        "zero output capacity fails"
    );
}

#[test]
fn clipboard_failures_do_not_contain_source_or_filename_content() {
    let error = prepare_clipboard_image(
        b"private-clipboard-content",
        ImageFormat::Bmp,
        "private-filename.bmp".to_owned(),
    )
    .expect_err("malformed content fails");
    assert_eq!(
        format!("{error:?}"),
        "Invalid",
        "decoder and clipboard content never enter diagnostics"
    );
}
