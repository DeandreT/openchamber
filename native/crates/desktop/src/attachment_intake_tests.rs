//! Intake tests use private temporary files and small synthetic document containers.

use std::fs;
use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use image::{DynamicImage, ImageFormat};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use super::*;

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        #[cfg(target_os = "linux")]
        let root = PathBuf::from("/tmp/opencode");
        #[cfg(not(target_os = "linux"))]
        let root = std::env::temp_dir();
        let path = root.join(format!(
            "openchamber-attachment-intake-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path).expect("create unique test directory");
        Self(path)
    }

    fn file(&self, bytes: &[u8]) -> PathBuf {
        let path = self.0.join("selected-file");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options
            .open(&path)
            .expect("create private selected file")
            .write_all(bytes)
            .expect("write selected bytes");
        path
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn memory(filename: &str, content: &[u8]) -> AttachmentIntake {
    AttachmentIntake {
        source: AttachmentInputSource::Bytes(content.to_vec()),
        filename: filename.to_owned(),
        declared_media_type: None,
        reserved_filenames: Vec::new(),
    }
}

fn from_path(path: PathBuf, filename: &str) -> AttachmentIntake {
    AttachmentIntake {
        source: AttachmentInputSource::LocalPath(path),
        ..memory(filename, b"")
    }
}

fn text(file: &NormalizedAttachmentFile) -> &str {
    std::str::from_utf8(&file.content).expect("normalized text is UTF-8")
}

fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    for &(path, content) in entries {
        archive
            .start_file(path, SimpleFileOptions::default())
            .expect("start small synthetic ZIP part");
        archive.write_all(content).expect("write synthetic part");
    }
    archive.finish().expect("finish synthetic ZIP").into_inner()
}

#[test]
fn encoded_clipboard_bmp_converts_before_publication_and_svg_stays_source_text() {
    let mut bytes = Vec::new();
    image::codecs::bmp::BmpEncoder::new(&mut bytes)
        .encode(&[3, 5, 7], 1, 1, image::ExtendedColorType::Rgb8)
        .expect("synthetic clipboard BMP");
    let output = prepare_attachment(AttachmentIntake {
        source: AttachmentInputSource::ClipboardImage {
            bytes,
            format: gpui_kit::ImageFormat::Bmp,
        },
        ..memory("pasted-image-1.bmp", b"")
    })
    .expect("complete encoded clipboard bytes convert under intake");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].filename, "pasted-image-1.png");
    assert_eq!(output[0].media_type, "image/png");
    let decoded = image::load_from_memory_with_format(&output[0].content, ImageFormat::Png)
        .expect("converted PNG is complete");
    assert_eq!(decoded.into_rgb8().as_raw(), &[3, 5, 7]);

    let svg = b"<svg xmlns=\"http://www.w3.org/2000/svg\"><script>synthetic()</script></svg>";
    let output = prepare_attachment(memory("pasted-image-2.svg", svg))
        .expect("SVG follows text normalization");
    assert_eq!(output[0].media_type, "text/plain");
    assert_eq!(output[0].content, svg);
    assert_eq!(
        prepare_attachment(AttachmentIntake {
            source: AttachmentInputSource::ClipboardImage {
                bytes: svg.to_vec(),
                format: gpui_kit::ImageFormat::Svg
            },
            ..memory("pasted-image-2.svg", b"")
        }),
        Err(AttachmentIntakeError::Unsupported),
        "the raster conversion boundary cannot expose SVG as an image"
    );
}

#[test]
fn path_and_owned_handle_read_complete_sources_with_the_captured_display_name() {
    let scratch = Scratch::new();
    let path = scratch.file(b"\xef\xbb\xbfcomplete\xff\r\n");
    let files = prepare_attachment(from_path(path.clone(), "captured.txt"))
        .expect("selected regular file prepares");
    assert_eq!(files.len(), 1, "one input text file emits one output");
    assert_eq!(
        files[0].filename, "captured.txt",
        "capture supplies the name"
    );
    assert_eq!(files[0].media_type, "text/plain");
    assert_eq!(text(&files[0]), "complete\u{fffd}\r\n");

    let mut handle = File::open(&path).expect("open owned selection");
    handle
        .read_exact(&mut [0; 4])
        .expect("advance handle cursor");
    let files = prepare_attachment(AttachmentIntake {
        source: AttachmentInputSource::LocalFile(handle),
        ..memory("handle.txt", b"")
    })
    .expect("owned handle rewinds before bounded read");
    assert_eq!(text(&files[0]), "complete\u{fffd}\r\n");
    assert_eq!(files[0].filename, "handle.txt");
}

#[test]
fn raw_admission_is_ten_mib_for_paths_handles_and_owned_memory() {
    assert_eq!(MAX_INLINE_ATTACHMENT_BYTES, 10 * 1024 * 1024);
    let scratch = Scratch::new();
    let path = scratch.file(b"");
    let file = OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("test file");
    file.set_len(MAX_INLINE_ATTACHMENT_BYTES as u64 + 1)
        .expect("grow sparse oversized source");
    assert_eq!(
        prepare_attachment(from_path(path.clone(), "oversized.pdf")),
        Err(AttachmentIntakeError::InputTooLarge),
        "source metadata cannot admit a 10 MiB plus 1 file"
    );
    assert_eq!(
        prepare_attachment(AttachmentIntake {
            source: AttachmentInputSource::LocalFile(File::open(&path).expect("selected handle")),
            ..memory("oversized.docx", b"")
        }),
        Err(AttachmentIntakeError::InputTooLarge),
        "the document parser's 20 MiB limit cannot widen raw admission"
    );
    assert_eq!(
        prepare_attachment(AttachmentIntake {
            source: AttachmentInputSource::Bytes(vec![0; MAX_INLINE_ATTACHMENT_BYTES + 1]),
            ..memory("oversized.heic", b"")
        }),
        Err(AttachmentIntakeError::InputTooLarge),
        "oversized HEIC bytes fail before any worker launch"
    );

    file.set_len(MAX_INLINE_ATTACHMENT_BYTES as u64)
        .expect("exact raw bound");
    let files = prepare_attachment(from_path(path, "exact.pdf")).expect("exact limit fits");
    assert_eq!(files[0].content.len(), MAX_INLINE_ATTACHMENT_BYTES);
    assert_eq!(files[0].media_type, "application/pdf");
}

#[test]
fn growth_after_metadata_is_bounded_and_never_becomes_a_truncated_success() {
    struct GrowOnRead {
        reader: File,
        writer: File,
        grown: bool,
        bytes_read: usize,
    }
    impl Read for GrowOnRead {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if !self.grown {
                self.writer
                    .set_len(MAX_INLINE_ATTACHMENT_BYTES as u64 + 8192)?;
                self.grown = true;
            }
            let count = self.reader.read(bytes)?;
            self.bytes_read += count;
            Ok(count)
        }
    }
    let scratch = Scratch::new();
    let path = scratch.file(b"initial");
    let reader = File::open(&path).expect("selected reader");
    admit_metadata(&reader.metadata().expect("initial handle metadata"))
        .expect("initial length is admissible");
    let mut growing = GrowOnRead {
        reader,
        writer: OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("test writer"),
        grown: false,
        bytes_read: 0,
    };
    assert_eq!(
        read_bounded(&mut growing),
        Err(AttachmentIntakeError::InputTooLarge),
        "stale short metadata cannot turn an oversized source into success"
    );
    assert_eq!(
        growing.bytes_read,
        MAX_INLINE_ATTACHMENT_BYTES + 1,
        "the reader stops at exactly the admission bound plus the detection byte"
    );
}

#[test]
fn failed_reads_discard_partial_bytes_and_diagnostic_strings() {
    struct FailedAfterPrefix(bool);
    impl Read for FailedAfterPrefix {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            if self.0 {
                return Err(io::Error::other("private source path"));
            }
            self.0 = true;
            let prefix = b"partial";
            let count = bytes.len().min(prefix.len());
            bytes[..count].copy_from_slice(&prefix[..count]);
            Ok(count)
        }
    }
    assert_eq!(
        read_bounded(&mut FailedAfterPrefix(false)),
        Err(AttachmentIntakeError::Read(io::ErrorKind::Other)),
        "a failed complete read never publishes its successful prefix"
    );
}

#[test]
fn normalized_growth_must_fit_the_prepared_transport_limit_before_returning() {
    let bytes = vec![0xff; MAX_PREPARED_ATTACHMENT_BYTES / 3 + 1];
    assert!(
        bytes.len() <= MAX_INLINE_ATTACHMENT_BYTES,
        "the complete input fits raw admission"
    );
    assert_eq!(
        prepare_attachment(AttachmentIntake {
            source: AttachmentInputSource::Bytes(bytes),
            declared_media_type: Some("text/plain".to_owned()),
            ..memory("expanded.txt", b"")
        }),
        Err(AttachmentIntakeError::OutputTooLarge),
        "lossy UTF-8 expansion cannot publish an unsendable prepared file"
    );
}

#[test]
fn directories_and_nonregular_owned_handles_are_rejected() {
    let scratch = Scratch::new();
    assert_eq!(
        prepare_attachment(from_path(scratch.path().to_owned(), "folder.txt")),
        Err(AttachmentIntakeError::NotRegular)
    );
    #[cfg(unix)]
    {
        assert_eq!(
            prepare_attachment(AttachmentIntake {
                source: AttachmentInputSource::LocalFile(
                    File::open(scratch.path()).expect("Unix permits opening a directory handle"),
                ),
                ..memory("folder.txt", b"")
            }),
            Err(AttachmentIntakeError::NotRegular),
            "the opened handle's metadata is authoritative"
        );
        assert_eq!(
            prepare_attachment(from_path(PathBuf::from("/dev/null"), "device.txt")),
            Err(AttachmentIntakeError::NotRegular),
            "device selections are never read"
        );
        assert_eq!(
            prepare_attachment(AttachmentIntake {
                source: AttachmentInputSource::LocalFile(
                    File::open("/dev/null").expect("open nonregular test handle"),
                ),
                ..memory("device.txt", b"")
            }),
            Err(AttachmentIntakeError::NotRegular),
            "an already-open device cannot bypass the regular-file check"
        );
    }
}

#[cfg(unix)]
#[test]
fn explicit_symlink_selections_keep_normal_resolution_and_still_require_a_regular_target() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let target = scratch.file(b"selected target");
    let selected = scratch.path().join("explicit-selection");
    symlink(&target, &selected).expect("create selected symlink");
    let files = prepare_attachment(from_path(selected.clone(), "selected.txt"))
        .expect("explicitly selected link to a regular file is supported");
    assert_eq!(files[0].content, b"selected target");
    fs::remove_file(&selected).expect("remove test link");
    symlink(scratch.path(), &selected).expect("link to directory");
    assert_eq!(
        prepare_attachment(from_path(selected, "selected.txt")),
        Err(AttachmentIntakeError::NotRegular),
        "symlink resolution cannot admit a directory"
    );
}

#[test]
fn binary_text_notebook_and_har_inputs_use_the_actual_normalizer() {
    for (filename, declared, media_type) in [
        ("photo.PNG", None, "image/png"),
        ("photo.jpeg", None, "image/jpeg"),
        ("photo.gif", None, "image/gif"),
        ("photo.webp", None, "image/webp"),
        ("document.pdf", None, "application/pdf"),
        ("mime-only.bin", Some("IMAGE/JPEG; ignored=1"), "image/jpeg"),
    ] {
        let files = prepare_attachment(AttachmentIntake {
            declared_media_type: declared.map(str::to_owned),
            ..memory(filename, b"\0complete\xffbinary")
        })
        .expect("normalizer accepts the declared binary role");
        assert_eq!(files[0].filename, filename);
        assert_eq!(files[0].media_type, media_type);
        assert_eq!(files[0].content, b"\0complete\xffbinary");
    }
    let notebook = serde_json::to_vec(&serde_json::json!({
        "metadata": { "kernelspec": { "language": "rust" } },
        "cells": [{
            "cell_type": "code",
            "source": "42",
            "outputs": [{ "data": { "text/plain": "42", "image/png": "private binary output" } }],
        }],
    }))
    .expect("synthetic notebook serializes");
    let files = prepare_attachment(memory("calculation.ipynb", &notebook))
        .expect("notebook normalization succeeds");
    assert_eq!(files[0].filename, "calculation.ipynb");
    assert_eq!(files[0].media_type, "text/plain");
    assert_eq!(
        text(&files[0]),
        "# Notebook: calculation.ipynb\n\n## Code cell 1\n\n```rust\n42\n```\n\n\
         ### Output\n\n42\n\n[Non-text output omitted: image/png]\n"
    );
    let har = serde_json::to_vec(&serde_json::json!({
        "log": { "entries": [{ "request": {
            "headers": [{ "name": "Authorization", "value": "private credential" }],
            "postData": { "text": "private body" },
        } }] },
    }))
    .expect("synthetic HAR serializes");
    let files =
        prepare_attachment(memory("capture.har", &har)).expect("HAR normalization succeeds");
    let sanitized: serde_json::Value =
        serde_json::from_slice(&files[0].content).expect("sanitized HAR remains JSON");
    assert_eq!(
        sanitized["log"]["entries"][0]["request"]["headers"][0]["value"],
        "[REDACTED]"
    );
    assert_eq!(
        sanitized["log"]["entries"][0]["request"]["postData"]["text"],
        "[OMITTED BY OPENCHAMBER]"
    );
    assert!(
        !text(&files[0]).contains("private"),
        "HAR secrets are absent"
    );
    let empty = prepare_attachment(memory("empty.txt", b"")).expect("empty text is supported");
    assert_eq!(empty.len(), 1, "empty source text still emits one file");
    assert!(empty[0].content.is_empty());
}

#[test]
fn all_six_document_extensions_dispatch_to_real_extraction_and_keep_the_original_name() {
    let cases = [
        (
            "docx",
            archive(&[(
                "word/document.xml",
                b"<w:document><w:p><w:t>intake</w:t></w:p></w:document>",
            )]),
        ),
        (
            "pptx",
            archive(&[(
                "ppt/slides/slide1.xml",
                b"<p:sld><a:p><a:t>intake</a:t></a:p></p:sld>",
            )]),
        ),
        (
            "xlsx",
            archive(&[
                (
                    "xl/workbook.xml",
                    br#"<workbook><sheets><sheet name="intake" r:id="one"/></sheets></workbook>"#,
                ),
                (
                    "xl/_rels/workbook.xml.rels",
                    b"<Relationships><Relationship Id=\"one\" Target=\"worksheets/sheet1.xml\" \
                      Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\"/>\
                      </Relationships>",
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    b"<worksheet><sheetData><row><c r=\"A1\" t=\"inlineStr\"><is><t>intake</t></is></c>\
                      </row></sheetData></worksheet>",
                ),
            ]),
        ),
        (
            "odt",
            archive(&[(
                "content.xml",
                b"<office:document><text:p>intake</text:p></office:document>",
            )]),
        ),
        (
            "odp",
            archive(&[(
                "content.xml",
                b"<office:document><draw:page><text:p>intake</text:p></draw:page></office:document>",
            )]),
        ),
        (
            "ods",
            archive(&[(
                "content.xml",
                b"<office:document><table:table><table:table-row><table:table-cell>\
                  <text:p>intake</text:p></table:table-cell></table:table-row>\
                  </table:table></office:document>",
            )]),
        ),
    ];
    for (extension, source) in cases {
        let filename = format!("captured.{}", extension.to_ascii_uppercase());
        let files = prepare_attachment(memory(&filename, &source))
            .expect("supported document reaches its format extractor");
        assert_eq!(files.len(), 1, "text-only document has one output");
        assert_eq!(
            files[0].filename, filename,
            "original document name is retained"
        );
        assert_eq!(
            files[0].media_type, "text/plain",
            "ZIP bytes become extracted text"
        );
        assert!(
            text(&files[0]).contains("intake"),
            "actual XML content is extracted"
        );
        assert_eq!(
            prepare_attachment(memory(&filename, b"invalid archive")),
            Err(AttachmentIntakeError::Document(
                DocumentExtractionError::InvalidArchive
            )),
            "document errors never fall back to attaching binary or plain source"
        );
    }
}

#[test]
fn document_images_keep_exact_bytes_citation_order_and_reserved_names() {
    let mut png = Cursor::new(Vec::new());
    DynamicImage::new_rgb8(1, 1)
        .write_to(&mut png, ImageFormat::Png)
        .expect("encode small synthetic embedded image");
    let png = png.into_inner();
    let source = archive(&[
        (
            "content.xml",
            b"<office:document><text:p>start</text:p><draw:image xlink:href=\"Pictures/one.png\"/>\
              <text:p>end</text:p></office:document>",
        ),
        ("Pictures/one.png", &png),
    ]);
    let files = prepare_attachment(AttachmentIntake {
        reserved_filenames: vec!["NOTES-IMAGE-1.PNG".to_owned()],
        ..memory("notes.odt", &source)
    })
    .expect("document and cited image prepare atomically");
    assert_eq!(
        files.len(),
        2,
        "multiple files identify one document bundle"
    );
    assert_eq!(files[0].filename, "notes.odt");
    assert_eq!(files[0].media_type, "text/plain");
    assert_eq!(files[1].filename, "notes-image-2.png");
    assert_eq!(files[1].media_type, "image/png");
    assert_eq!(files[1].content, png, "embedded bytes stay exact");
    assert!(
        text(&files[0]).contains("start\n\n[notes-image-2.png]\n\nend"),
        "the converter's reserved filename remains in its original citation position"
    );
}

#[test]
fn unsafe_original_and_generated_names_fail_transport_admission_before_storage() {
    for filename in [
        "",
        "../private.txt",
        "private\\file.txt",
        " NUL.txt",
        "CON.txt",
        "file?.txt",
        "trail.",
    ] {
        assert_eq!(
            prepare_attachment(memory(filename, b"source")),
            Err(AttachmentIntakeError::InvalidFilename),
            "unsafe captured names are rejected without renaming"
        );
    }
    let filename = format!("{}.odt", "a".repeat(251));
    assert!(
        AttachmentUpload::new(&filename, Vec::new()).is_ok(),
        "original name fits"
    );
    let mut png = Cursor::new(Vec::new());
    DynamicImage::new_rgb8(1, 1)
        .write_to(&mut png, ImageFormat::Png)
        .expect("synthetic image");
    let source = archive(&[
        (
            "content.xml",
            br#"<office:document><draw:image xlink:href="Pictures/one.png"/></office:document>"#,
        ),
        ("Pictures/one.png", png.get_ref()),
    ]);
    assert_eq!(
        prepare_attachment(memory(&filename, &source)),
        Err(AttachmentIntakeError::InvalidFilename),
        "a cited image with an unsendable generated name fails the entire source bundle"
    );
}

#[test]
fn unsupported_and_malformed_files_do_not_clear_or_block_successful_siblings() {
    let first = prepare_attachment(memory("first.txt", b"first")).expect("first sibling");
    for input in [
        memory("binary.bin", b"\0unsupported"),
        AttachmentIntake {
            declared_media_type: Some("application/msword".to_owned()),
            ..memory("legacy.doc", b"document bytes")
        },
    ] {
        assert_eq!(
            prepare_attachment(input),
            Err(AttachmentIntakeError::Unsupported)
        );
    }
    assert_eq!(
        prepare_attachment(memory("broken.har", b"invalid HAR")),
        Err(AttachmentIntakeError::MalformedHar),
        "a malformed HAR cannot return unsanitized source or empty success"
    );
    let last = prepare_attachment(memory("last.txt", b"last")).expect("last sibling");
    assert_eq!(first[0].content, b"first", "earlier sibling is unchanged");
    assert_eq!(last[0].content, b"last", "later sibling still prepares");
}

#[test]
fn heic_uses_the_workers_actual_production_limits_without_launching_a_test_executable() {
    let limits = heic_limits();
    assert_eq!(limits.max_input_bytes, MAX_INLINE_ATTACHMENT_BYTES);
    assert_eq!(limits.max_output_bytes, heic_worker::MAX_OUTPUT_BYTES);
    assert_eq!(
        limits.max_metadata_entries,
        heic_worker::MAX_METADATA_ENTRIES
    );
    assert_eq!(limits.max_pixels, heic_worker::MAX_PIXELS);
    assert_eq!(limits.max_decoded_pixels, heic_worker::MAX_PIXELS);
    assert_eq!(
        prepare_attachment(memory("invalid?.heic", b"HEIC source")),
        Err(AttachmentIntakeError::InvalidFilename),
        "unsafe names fail without launching or decoding"
    );
    // Successful contained HEIC intake requires the built desktop's early main worker branch.
    // The unit-test executable does not have it and must never be launched as that worker.
}

#[test]
fn debug_values_never_expose_paths_names_mime_payloads_or_attachment_content() {
    let input = AttachmentIntake {
        source: AttachmentInputSource::LocalPath(PathBuf::from("/private/path/selection")),
        filename: "private-name.txt".to_owned(),
        declared_media_type: Some("private-mime".to_owned()),
        reserved_filenames: vec!["private-reservation".to_owned()],
    };
    assert!(!format!("{input:?}").contains("private"));
    let input = memory("private-name.txt", b"private content");
    assert!(!format!("{input:?}").contains("private"));
    let prepared = prepare_attachment(input).expect("private source prepares");
    assert!(!format!("{prepared:?}").contains("private"));
    let scratch = Scratch::new();
    let error = prepare_attachment(from_path(
        scratch.path().join("private-missing-file"),
        "missing.txt",
    ))
    .expect_err("missing file fails");
    assert_eq!(error, AttachmentIntakeError::Read(io::ErrorKind::NotFound));
    assert!(!format!("{error:?}").contains("private"));
}
