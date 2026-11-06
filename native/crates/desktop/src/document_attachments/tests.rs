//! Synthetic ZIP fixtures only. No documents, image files or paths leave this module.

use std::io::{Cursor, Read, Seek, Write};

use image::codecs::png::PngEncoder;
use image::{DynamicImage, ExtendedColorType, ImageEncoder, ImageFormat};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::*;

fn zipped(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    for (name, bytes) in entries {
        writer
            .start_file(*name, options)
            .expect("synthetic archive entry starts");
        writer
            .write_all(bytes)
            .expect("synthetic entry bytes write");
    }
    writer
        .finish()
        .expect("synthetic ZIP finishes")
        .into_inner()
}

fn image_bytes(format: ImageFormat) -> Vec<u8> {
    let mut bytes = Cursor::new(Vec::new());
    DynamicImage::new_rgb8(1, 1)
        .write_to(&mut bytes, format)
        .expect("synthetic 1x1 image encodes");
    bytes.into_inner()
}

fn text(result: &DocumentAttachments) -> &str {
    std::str::from_utf8(&result.text_file.content).expect("extraction emits valid UTF-8")
}

fn extract(name: &str, entries: &[(&str, &[u8])]) -> DocumentAttachments {
    extract_document_attachments(name, &zipped(entries), &[])
        .expect("valid synthetic document extracts")
}

fn rels(items: &[(&str, &str, &str)]) -> String {
    let mut xml = String::from("<Relationships>");
    for (id, target, kind) in items {
        // Use the real worksheet relationship kind while retaining concise fixture declarations.
        let kind = if *kind == "sheet" {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet"
        } else {
            *kind
        };
        xml.push_str(&format!(
            "<Relationship Id=\"{id}\" Target=\"{target}\" Type=\"{kind}\"/>"
        ));
    }
    xml.push_str("</Relationships>");
    xml
}

#[test]
fn docx_keeps_paragraph_order_references_headers_and_filename() {
    let png = image_bytes(ImageFormat::Png);
    let xml = br#"<w:document><w:body><w:p><w:r><w:t>Before &amp; &lt;literal&gt;</w:t></w:r></w:p><w:p><a:blip r:embed="image"/></w:p><w:p><w:t>After</w:t><w:tab/><w:t>tab</w:t><w:br/><w:t>line</w:t></w:p><w:p><a:blip r:embed="image"/></w:p></w:body></w:document>"#;
    let relationships = rels(&[("image", "./media/../media/photo.png", "image")]);
    let entries = [
        ("word/document.xml", xml.as_slice()),
        ("word/_rels/document.xml.rels", relationships.as_bytes()),
        ("word/media/photo.png", png.as_slice()),
        (
            "word/header1.xml",
            b"<w:hdr><w:p><w:t>Header</w:t></w:p></w:hdr>",
        ),
        (
            "word/footer1.xml",
            b"<w:ftr><w:p><w:t>Footer</w:t></w:p></w:ftr>",
        ),
    ];
    let result = extract("report.DOCX", &entries);
    assert_eq!(
        result.text_file.filename, "report.DOCX",
        "the text attachment retains the source name"
    );
    assert_eq!(
        result.text_file.media_type, "text/plain",
        "document bytes become a text attachment"
    );
    assert!(
        text(&result).contains("Before & <literal>\n\n[report-image-1.png]\n\nAfter\ttab\nline"),
        "text and images retain paragraph positions and XML escapes are decoded once"
    );
    assert!(
        text(&result).contains("## Footer\n\nFooter\n\n## Header\n\nHeader"),
        "extras use shared path ordering"
    );
    assert_eq!(
        result.images.len(),
        1,
        "repeated references share one attached image"
    );
    assert_eq!(result.images[0].content, png, "image bytes stay unchanged");
    let reserved = vec!["REPORT-IMAGE-1.PNG".to_owned()];
    let result = extract_document_attachments("report.docx", &zipped(&entries), &reserved)
        .expect("reserved output names are skipped");
    assert_eq!(
        result.images[0].filename, "report-image-2.png",
        "name reservations ignore case"
    );
}

#[test]
fn pptx_sorts_slide_parts_keeps_picture_positions_and_notes() {
    let jpeg = image_bytes(ImageFormat::Jpeg);
    let relationships = rels(&[
        ("picture", "../media/photo.jpeg", "image"),
        (
            "notes",
            "../notesSlides/notesSlide1.xml",
            "http://example/notesSlide",
        ),
    ]);
    let result = extract("deck.pptx", &[
        ("ppt/slides/slide10.xml", b"<p:sld/>"),
        ("ppt/slides/slide2.xml", br#"<p:sld><a:p><a:t>Title</a:t></a:p><p:pic><a:blip r:embed="picture"/></p:pic></p:sld>"#),
        ("ppt/slides/_rels/slide2.xml.rels", relationships.as_bytes()),
        ("ppt/notesSlides/notesSlide1.xml", b"<p:notes><a:p><a:t>Speaker note</a:t></a:p></p:notes>"),
        ("ppt/media/photo.jpeg", &jpeg),
    ]);
    assert!(
        text(&result).contains(
            "## Slide 1\n\nTitle\n\n[deck-image-1.jpg]\n\n### Slide 1 notes\n\nSpeaker note"
        ),
        "numbered slides, pictures and notes match the shared projection"
    );
    assert!(
        text(&result).contains("## Slide 2\n\n[Empty slide]"),
        "empty slides remain explicit"
    );
    assert_eq!(
        result.images[0].media_type, "image/jpeg",
        "JPEG extension normalizes to jpg"
    );
}

#[test]
fn xlsx_uses_rich_shared_strings_quoted_tsv_sparse_coordinates_and_anchors() {
    let webp = image_bytes(ImageFormat::WebP);
    let workbook_rels = rels(&[("sheet", "worksheets/sheet1.xml", "sheet")]);
    let sheet_rels = rels(&[("drawing", "../drawings/drawing1.xml", "drawing")]);
    let drawing_rels = rels(&[("photo", "../media/photo.webp", "image")]);
    let result = extract("budget.xlsx", &[
        ("xl/workbook.xml", br#"<workbook><sheets><sheet name="Data &amp; details" r:id="sheet"/></sheets></workbook>"#),
        ("xl/_rels/workbook.xml.rels", workbook_rels.as_bytes()),
        ("xl/sharedStrings.xml", b"<sst><si><r><t>Reve</t></r><r><t>nue</t></r></si></sst>"),
        ("xl/worksheets/sheet1.xml", br#"<worksheet><sheetData><row><c r="A1" t="s"><v>0</v></c><c r="B1"><v>42</v></c></row><row><c r="A2" t="inlineStr"><is><t>North</t></is></c><c r="B2"><v>17</v></c></row><row><c r="A4" t="inlineStr"><is><t>line 1&#10;line 2</t></is></c><c r="B4" t="inlineStr"><is><t>say &quot;hi&quot;</t></is></c></row><row><c r="A5" t="b"><v>1</v></c><c r="XFD5" t="b"><v>0</v></c></row></sheetData><drawing r:id="drawing"/></worksheet>"#),
        ("xl/worksheets/_rels/sheet1.xml.rels", sheet_rels.as_bytes()),
        ("xl/drawings/drawing1.xml", br#"<xdr:wsDr><xdr:twoCellAnchor><xdr:from><xdr:col>1</xdr:col><xdr:row>2</xdr:row></xdr:from><a:blip r:embed="photo"/></xdr:twoCellAnchor></xdr:wsDr>"#),
        ("xl/drawings/_rels/drawing1.xml.rels", drawing_rels.as_bytes()),
        ("xl/media/photo.webp", &webp),
    ]);
    for expected in [
        "## Sheet: Data & details",
        "Range: A1:B2\nRevenue\t42\nNorth\t17",
        "Range: A4:B4\n\"line 1\nline 2\"\t\"say \"\"hi\"\"\"",
        "Cells: A5\tTRUE | XFD5\tFALSE",
        "Image at B3: [budget-image-1.webp]",
    ] {
        assert!(
            text(&result).contains(expected),
            "worksheet extraction preserves the expected values and positions"
        );
    }
    assert_eq!(result.images.len(), 1, "the anchored image is attached");
}

#[test]
fn odf_extracts_all_three_kinds_and_caps_space_expansion() {
    let png = image_bytes(ImageFormat::Png);
    let odt = extract("notes.odt", &[
        ("content.xml", br#"<office:document-content><text:h>Heading</text:h><text:p>Hello <text:span>world</text:span><text:tab/>tab<text:line-break/>line</text:p><draw:frame><draw:image xlink:href="Pictures/photo.png"/></draw:frame><text:p>After image</text:p><text:p>Before<text:s text:c="999999999999999999999"/>After</text:p></office:document-content>"#),
        ("Pictures/photo.png", &png),
    ]);
    assert!(
        text(&odt)
            .contains("Heading\n\nHello world\ttab\nline\n\n[notes-image-1.png]\n\nAfter image"),
        "ODT paragraphs and standalone images retain order"
    );
    assert!(
        text(&odt).contains("[Additional spaces omitted]After"),
        "invalid huge repeat counts remain bounded and explicit"
    );
    let odp = extract("slides.odp", &[("content.xml", br#"<office:document><draw:page draw:name="Intro"><text:p>Welcome</text:p></draw:page></office:document>"#)]);
    assert_eq!(
        text(&odp),
        "# Presentation\n\n## Slide: Intro\n\nWelcome\n",
        "ODP retains page names"
    );
    let ods = extract("table.ods", &[
        ("content.xml", br#"<office:document><table:table table:name="Data"><table:shapes><draw:image xlink:href="Pictures/photo.png"/></table:shapes><table:table-row><table:table-cell><text:p>Name</text:p></table:table-cell><table:table-cell><text:p>Value</text:p></table:table-cell></table:table-row></table:table></office:document>"#),
        ("Pictures/photo.png", &png),
    ]);
    assert!(
        text(&ods).contains("## Sheet: Data\n\n[table-image-1.png]\n\nName | Value"),
        "ODS retains shapes before table rows"
    );
}

#[test]
fn canonical_path_admission_rejects_traversal_absolute_aliases_and_controls() {
    for path in [
        "../word/document.xml",
        "/word/document.xml",
        "word/../word/document.xml",
        "word//document.xml",
        "word/./document.xml",
        "word\\document.xml",
        "C:/word/document.xml",
        "word/\u{007f}document.xml",
    ] {
        let bytes = zipped(&[(path, b"<w:document/>")]);
        assert_eq!(
            extract_document_attachments("unsafe.docx", &bytes, &[]),
            Err(DocumentExtractionError::UnsafeArchivePath),
            "noncanonical archive names are refused before extraction"
        );
    }
    assert_eq!(
        archive::resolve_path("ppt/slides/slide1.xml", "../media/photo.png").as_deref(),
        Some("ppt/media/photo.png"),
        "internal parent segments resolve relative to their source part"
    );
    assert_eq!(
        archive::resolve_path("word/document.xml", "../../outside.png"),
        None,
        "relationship traversal cannot escape the archive root"
    );
}

#[test]
fn invalid_xml_is_a_per_file_failure_without_binary_fallback() {
    for xml in [
        b"<w:document><w:p></w:document>".as_slice(),
        b"<w:document>",
        b"<w:document/ ><extra/>",
        b"<w:document>&undefined;</w:document>",
        b"<!DOCTYPE x [<!ENTITY x 'expanded'>]><w:document>&x;</w:document>",
        b"<w:document><w:t>\xff</w:t></w:document>",
        b"<w:document a='one' a='two'/>",
    ] {
        assert!(
            extract_document_attachments(
                "invalid.docx",
                &zipped(&[("word/document.xml", xml)]),
                &[]
            )
            .is_err(),
            "malformed XML never becomes a successful empty document or raw text attachment"
        );
    }
    let first = extract(
        "first.odt",
        &[(
            "content.xml",
            b"<office:document><text:p>first</text:p></office:document>",
        )],
    );
    let failed = extract_document_attachments("failed.docx", b"not a ZIP", &[]);
    let last = extract(
        "last.odt",
        &[(
            "content.xml",
            b"<office:document><text:p>last</text:p></office:document>",
        )],
    );
    assert!(failed.is_err(), "a failure is explicit");
    assert!(
        text(&first).contains("first") && text(&last).contains("last"),
        "independent successful files remain intact"
    );
}

#[test]
fn flat_xml_keeps_text_on_the_correct_parent_and_decodes_entities_once() {
    let result = extract("text.odt", &[("content.xml", b"<office:document><text:p>a<text:span>b</text:span>c &amp;lt; &#x1f642; <![CDATA[<literal>]]></text:p></office:document>")]);
    assert!(
        text(&result).contains("abc &lt; \u{1f642} <literal>"),
        "nested text, CDATA and character references are neither dropped nor decoded twice"
    );
    let depth = format!("{}{}", "<x>".repeat(129), "</x>".repeat(129));
    assert!(
        matches!(
            xml::Xml::parse(depth.as_bytes()),
            Err(DocumentExtractionError::XmlComplexityLimit)
        ),
        "deep trees cannot overflow a recursive stack"
    );
}

#[test]
fn signatures_without_dimensions_and_external_links_do_not_create_images() {
    let relationships = br#"<Relationships><Relationship Id="external" Target="https://example.invalid/private.png" TargetMode="External"/><Relationship Id="unsafe" Target="../../outside.png"/><Relationship Id="spoof" Target="media/fake.png"/><Relationship Id="svg" Target="media/picture.svg"/></Relationships>"#;
    let result = extract("bad-images.docx", &[
        ("word/document.xml", br#"<w:document><w:p><a:blip r:embed="external"/></w:p><w:p><a:blip r:embed="unsafe"/></w:p><w:p><a:blip r:embed="spoof"/></w:p><w:p><a:blip r:embed="svg"/></w:p></w:document>"#),
        ("word/_rels/document.xml.rels", relationships),
        ("word/media/fake.png", b"\x89PNG\r\n\x1a\n"),
        ("word/media/picture.svg", b"<svg/>"),
    ]);
    assert!(
        result.images.is_empty(),
        "unresolved, external and invalid image references yield no binary attachments"
    );
    assert!(
        text(&result).contains("[Invalid embedded image omitted: fake.png]"),
        "signature-only bytes fail dimension validation"
    );
    assert!(
        text(&result).contains("[Unsupported embedded image omitted: picture.svg]"),
        "unsupported image kinds stay explicit"
    );
}

#[test]
fn every_supported_image_kind_needs_matching_signature_and_dimensions() {
    for (extension, format) in [
        ("png", ImageFormat::Png),
        ("jpg", ImageFormat::Jpeg),
        ("gif", ImageFormat::Gif),
        ("webp", ImageFormat::WebP),
    ] {
        let image = image_bytes(format);
        let path = format!("Pictures/photo.{extension}");
        let xml = format!("<office:document><draw:image xlink:href=\"{path}\"/></office:document>");
        let result = extract(
            "image.odt",
            &[("content.xml", xml.as_bytes()), (&path, &image)],
        );
        assert_eq!(
            result.images.len(),
            1,
            "a real header and matching extension are admitted"
        );
        assert_eq!(
            result.images[0].content, image,
            "validated images preserve their exact bytes"
        );
    }
}

#[test]
fn image_count_limits_keep_first_citation_order_and_explicit_omissions() {
    let image = image_bytes(ImageFormat::Png);
    let mut xml = String::from("<office:document>");
    let mut owned = Vec::new();
    for index in 0..51 {
        let path = format!("Pictures/photo{index}.png");
        xml.push_str(&format!("<draw:image xlink:href=\"{path}\"/>"));
        owned.push((path, image.clone()));
    }
    xml.push_str("</office:document>");
    let mut entries = vec![("content.xml", xml.as_bytes())];
    for (path, image) in &owned {
        entries.push((path.as_str(), image.as_slice()));
    }
    let result = extract("gallery.odt", &entries);
    assert_eq!(
        result.images.len(),
        50,
        "only the first 50 distinct images are attached"
    );
    assert!(
        text(&result).contains("[Embedded image omitted by attachment limits: photo50.png]"),
        "omitted positions stay visible in the text attachment"
    );
    assert_eq!(
        result.images[49].filename, "gallery-image-50.png",
        "images retain their citation order"
    );
}

#[test]
fn image_byte_limits_admit_exactly_twenty_and_forty_mib() {
    let png = image_bytes(ImageFormat::Png);
    // Valid headers with synthetic trailing padding exercise compressed-byte attachment bounds
    // without allocating or decoding large pixel buffers.
    let mut padded = png.clone();
    padded.resize(MAX_EMBEDDED_IMAGE_BYTES, 0);
    let result = extract("sizes.odt", &[
        ("content.xml", br#"<office:document><draw:image xlink:href="Pictures/first.png"/><draw:image xlink:href="Pictures/second.png"/><draw:image xlink:href="Pictures/third.png"/></office:document>"#),
        ("Pictures/first.png", &padded), ("Pictures/second.png", &padded),
        ("Pictures/third.png", &png),
    ]);
    assert_eq!(
        result.images.len(),
        2,
        "two exact-limit images fit the forty MiB total"
    );
    assert_eq!(
        result
            .images
            .iter()
            .map(|image| image.content.len())
            .sum::<usize>(),
        MAX_EMBEDDED_IMAGES_BYTES,
        "the retained total can equal the ceiling"
    );
    assert!(
        text(&result).contains("[Embedded image omitted by attachment limits: third.png]"),
        "a further image exceeds the total"
    );
    drop(result);
    padded.push(0);
    let result = extract("large-image.odt", &[
        ("content.xml", br#"<office:document><draw:image xlink:href="Pictures/large.png"/></office:document>"#),
        ("Pictures/large.png", &padded),
    ]);
    assert!(
        result.images.is_empty(),
        "one byte over the per-image ceiling is refused"
    );
    assert!(
        text(&result).contains("[Embedded image omitted by attachment limits: large.png]"),
        "per-image omission is explicit"
    );
}

#[test]
fn text_truncation_preserves_utf8_utf16_bounds_and_removes_uncited_images() {
    let image = image_bytes(ImageFormat::Png);
    let xml = format!(
        "<office:document><text:p>{}</text:p><draw:image xlink:href=\"Pictures/image.png\"/></office:document>",
        "\u{1f642}".repeat(250_100)
    );
    let result = extract(
        "long.odt",
        &[
            ("content.xml", xml.as_bytes()),
            ("Pictures/image.png", &image),
        ],
    );
    assert!(
        text(&result).encode_utf16().count() <= 500_000,
        "the ceiling uses UTF-16 units and never splits a Unicode scalar"
    );
    assert!(
        text(&result).ends_with(TRUNCATION_NOTICE),
        "actual truncation is explicit"
    );
    assert!(
        result.images.is_empty(),
        "images beyond the final text cutoff are not returned"
    );
    let mut buffer = TextOutput::default();
    let citation = "[long-image-1.png]";
    let budget = MAX_TEXT_UNITS - TRUNCATION_NOTICE.len();
    buffer.push(&"x".repeat(budget - 3));
    buffer.push(citation);
    buffer.push(&"x".repeat(100));
    let filenames = BTreeMap::from([("path".to_owned(), "long-image-1.png".to_owned())]);
    let bounded = buffer.finish(&filenames);
    assert!(
        !bounded.contains("[lon"),
        "a citation crossing the cutoff is removed whole"
    );
    assert!(
        bounded.ends_with(TRUNCATION_NOTICE),
        "removing a partial citation keeps the notice"
    );
    let prefix = "x".repeat(budget - 20);
    let xml = format!(
        "<office:document><text:p>{prefix}</text:p><draw:image xlink:href=\"Pictures/image.png\"/><text:p>{}</text:p></office:document>",
        "x".repeat(100)
    );
    let result = extract(
        "long.odt",
        &[
            ("content.xml", xml.as_bytes()),
            ("Pictures/image.png", &image),
        ],
    );
    assert!(
        result.images.is_empty(),
        "an actual image whose citation crosses the final cutoff is removed"
    );
    assert!(
        !text(&result).contains("[long"),
        "the final attachment contains no partial image filename"
    );
}

#[test]
fn shared_string_reuse_does_not_expand_a_dense_sheet_in_memory() {
    let shared = format!("<sst><si><t>{}</t></si></sst>", "value".repeat(100_000));
    let mut sheet = String::from("<worksheet><sheetData>");
    for row in 1..=1_000 {
        sheet.push_str(&format!("<row><c r=\"A{row}\" t=\"s\"><v>0</v></c></row>"));
    }
    sheet.push_str("</sheetData></worksheet>");
    let relationships = rels(&[("sheet", "worksheets/sheet1.xml", "sheet")]);
    let result = extract(
        "reused.xlsx",
        &[
            (
                "xl/workbook.xml",
                br#"<workbook><sheet name="Data" r:id="sheet"/></workbook>"#,
            ),
            ("xl/_rels/workbook.xml.rels", relationships.as_bytes()),
            ("xl/sharedStrings.xml", shared.as_bytes()),
            ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
        ],
    );
    assert!(
        text(&result).ends_with(TRUNCATION_NOTICE),
        "a repeatedly referenced large string still yields bounded output"
    );
    assert!(
        text(&result).encode_utf16().count() <= MAX_TEXT_UNITS,
        "dense serialization stops at the text ceiling"
    );
}

#[test]
fn malformed_worksheet_values_and_positions_fail_without_inventing_data() {
    let relationships = rels(&[("sheet", "worksheets/sheet1.xml", "sheet")]);
    for cell in [
        r#"<c r="A1" t="s"><v>99</v></c>"#,
        r#"<c r="A1" t="b"><v>2</v></c>"#,
        r#"<c r="BAD0"><v>1</v></c>"#,
        r#"<c r="A1"><v>1</v></c><c r="B2"><v>2</v></c>"#,
    ] {
        let sheet = format!("<worksheet><row>{cell}</row></worksheet>");
        let bytes = zipped(&[
            (
                "xl/workbook.xml",
                br#"<workbook><sheet r:id="sheet"/></workbook>"#,
            ),
            ("xl/_rels/workbook.xml.rels", relationships.as_bytes()),
            ("xl/worksheets/sheet1.xml", sheet.as_bytes()),
        ]);
        assert_eq!(
            extract_document_attachments("invalid.xlsx", &bytes, &[]),
            Err(DocumentExtractionError::InvalidDocumentStructure),
            "invalid cells never invent a string, boolean or range"
        );
    }
}

fn patch_declared_size(bytes: &mut [u8], name: &str, size: u32) {
    let mut cursor = 0;
    let mut patched = false;
    while cursor + 46 <= bytes.len() {
        if bytes[cursor..].starts_with(b"PK\x01\x02") {
            let length = u16::from_le_bytes([bytes[cursor + 28], bytes[cursor + 29]]) as usize;
            if bytes.get(cursor + 46..cursor + 46 + length) == Some(name.as_bytes()) {
                bytes[cursor + 24..cursor + 28].copy_from_slice(&size.to_le_bytes());
                patched = true;
            }
        }
        cursor += 1;
    }
    assert!(patched, "synthetic central directory metadata is found");
}

#[test]
fn metadata_limits_cover_unselected_binaries_before_any_decompression() {
    let mut bytes = zipped(&[("unused.bin", b"x")]);
    patch_declared_size(&mut bytes, "unused.bin", (MAX_ENTRY_BYTES + 1) as u32);
    assert_eq!(
        extract_document_attachments("large.docx", &bytes, &[]),
        Err(DocumentExtractionError::EntryTooLarge),
        "unselected entry size is still admitted"
    );
    let mut bytes = zipped(&[("unused.xml", b"<x/>")]);
    patch_declared_size(&mut bytes, "unused.xml", (MAX_XML_ENTRY_BYTES + 1) as u32);
    assert_eq!(
        extract_document_attachments("large.docx", &bytes, &[]),
        Err(DocumentExtractionError::XmlTooLarge),
        "all XML metadata observes the narrower ceiling"
    );
    let names: Vec<String> = (0..5).map(|index| format!("unused{index}.bin")).collect();
    let entries: Vec<(&str, &[u8])> = names
        .iter()
        .map(|name| (name.as_str(), b"x".as_slice()))
        .collect();
    let mut bytes = zipped(&entries);
    for name in &names {
        patch_declared_size(&mut bytes, name, 21 * 1024 * 1024);
    }
    assert_eq!(
        extract_document_attachments("large.docx", &bytes, &[]),
        Err(DocumentExtractionError::ExpandedArchiveTooLarge),
        "unselected bytes count toward the total expansion ceiling"
    );
    assert_eq!(
        extract_document_attachments("large.docx", &vec![0; MAX_ARCHIVE_BYTES + 1], &[]),
        Err(DocumentExtractionError::InputTooLarge),
        "compressed input is refused before ZIP parsing"
    );
}

#[test]
fn oversized_and_underreported_deflate_streams_never_escape_read_caps() {
    let expanded = vec![b'x'; MAX_XML_ENTRY_BYTES as usize + 1];
    let mut bytes = zipped(&[("word/document.xml", &expanded)]);
    assert!(
        bytes.len() < MAX_ARCHIVE_BYTES,
        "highly compressible fixture passes the compressed ceiling"
    );
    assert_eq!(
        extract_document_attachments("bomb.docx", &bytes, &[]),
        Err(DocumentExtractionError::XmlTooLarge),
        "honest oversized metadata is refused first"
    );
    patch_declared_size(&mut bytes, "word/document.xml", 1);
    assert!(
        extract_document_attachments("bomb.docx", &bytes, &[]).is_err(),
        "lying metadata cannot bypass capped decompression and size verification"
    );
}

#[test]
fn entry_count_is_bounded_for_metadata_only_entries() {
    let names: Vec<String> = (0..5_001)
        .map(|index| format!("metadata/{index}.bin"))
        .collect();
    let entries: Vec<(&str, &[u8])> = names
        .iter()
        .map(|name| (name.as_str(), b"x".as_slice()))
        .collect();
    assert_eq!(
        extract_document_attachments("entries.docx", &zipped(&entries), &[]),
        Err(DocumentExtractionError::TooManyEntries),
        "metadata-only entries count toward the 5000-entry ceiling"
    );
}

#[test]
fn debug_and_failures_contain_no_source_names_paths_or_content() {
    let result = extract(
        "private-name.odt",
        &[(
            "content.xml",
            b"<office:document><text:p>private-content</text:p></office:document>",
        )],
    );
    let debug = format!("{result:?}");
    assert!(
        !debug.contains("private-name") && !debug.contains("private-content"),
        "result Debug exposes counts only"
    );
    assert_eq!(
        format!("{:?}", DocumentExtractionError::InvalidXml),
        "InvalidXml",
        "errors retain no parser diagnostics or private bytes"
    );
    assert_eq!(
        extract_document_attachments("old.doc", b"binary", &[]),
        Err(DocumentExtractionError::UnsupportedFormat),
        "legacy binary formats have no raw-as-text fallback"
    );
}

#[test]
fn repeated_large_missing_image_targets_resolve_once_and_projection_stops_when_full() {
    let missing = format!("{}.png", "x".repeat(4 * 1024 * 1024));
    let relationships = rels(&[("image", &missing, "image")]);
    let paragraphs = "<w:p><a:blip r:embed=\"image\"/></w:p>".repeat(100_000);
    for leading_text in ["", &"x".repeat(MAX_TEXT_UNITS + 1)] {
        let xml =
            format!("<w:document><w:p><w:t>{leading_text}</w:t></w:p>{paragraphs}</w:document>");
        let bytes = zipped(&[
            ("word/document.xml", xml.as_bytes()),
            ("word/_rels/document.xml.rels", relationships.as_bytes()),
        ]);
        let archive =
            Archive::read(&bytes).expect("large references remain within archive admission limits");
        let mut images = EmbeddedImages::new(&archive, "references.docx", &[]);
        let mut output = TextOutput::default();
        formats::extract("docx", &archive, &mut images, &mut output)
            .expect("one repeated missing relationship is bounded and explicit");
        assert!(output.full(), "the fixture crosses the text ceiling");
        if leading_text.is_empty() {
            assert_eq!(
                images.citations.len(),
                1,
                "missing-image omission outcomes are cached too"
            );
            assert_eq!(
                images.reference_work_bytes,
                missing.len() + "word/".len(),
                "a canonical multi-megabyte target is inspected once, independent of reference count"
            );
        } else {
            assert_eq!(
                images.reference_work_bytes, 0,
                "a full text projection never resolves later images"
            );
        }
        assert!(
            images.files.is_empty(),
            "missing references cannot invent image attachments"
        );
    }
}

#[test]
fn aggregate_reference_work_is_charged_before_long_cache_key_comparisons() {
    let bytes = zipped(&[("content.xml", b"<office:document/>")]);
    let archive = Archive::read(&bytes).expect("synthetic empty document is admitted");
    let mut images = EmbeddedImages::new(&archive, "references.odt", &[]);
    let path = format!("Pictures/{}.png", "x".repeat(4 * 1024 * 1024));
    let first = images
        .citation(Some(&path))
        .expect("first bounded reference resolves to an omission");
    let admitted_calls = MAX_REFERENCE_WORK_BYTES / path.len();
    for _ in 1..admitted_calls {
        assert_eq!(
            images
                .citation(Some(&path))
                .expect("bounded cache lookup is allowed"),
            first,
            "all repeated omissions use the same result"
        );
    }
    assert_eq!(
        images.citation(Some(&path)),
        Err(DocumentExtractionError::ReferenceWorkLimit),
        "repeated long direct-reference comparisons cannot grow beyond the aggregate work budget"
    );
}

fn invalid_png_with_large_compressed_profile() -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = PngEncoder::new(&mut bytes);
    encoder
        .set_icc_profile(vec![0; 16 * 1024 * 1024])
        .expect("the image library accepts a synthetic profile fixture");
    encoder
        .write_image(&[0, 0, 0], 1, 1, ExtendedColorType::Rgb8)
        .expect("the image library encodes the synthetic profile");
    // Remove the actual image data after the library-generated iCCP metadata. Construction must
    // fail after inspecting that metadata. No custom image codec or decompressor is involved.
    let idat = bytes
        .windows(4)
        .position(|bytes| bytes == b"IDAT")
        .expect("library-generated PNG contains image data");
    bytes.truncate(idat - 4);
    bytes
}

#[test]
fn failed_png_metadata_inspections_consume_an_aggregate_budget() {
    let bad = invalid_png_with_large_compressed_profile();
    assert!(
        bad.len() < 1024 * 1024,
        "the large profile is compressed in the encoded fixture"
    );
    let good = image_bytes(ImageFormat::Png);
    let mut xml = String::from("<office:document><draw:image xlink:href=\"Pictures/good.png\"/>");
    let names: Vec<String> = (0..128)
        .map(|index| format!("Pictures/bad{index}.png"))
        .collect();
    for name in &names {
        xml.push_str(&format!("<draw:image xlink:href=\"{name}\"/>"));
    }
    xml.push_str("</office:document>");
    let mut entries = vec![
        ("content.xml", xml.as_bytes()),
        ("Pictures/good.png", good.as_slice()),
    ];
    for name in &names {
        entries.push((name.as_str(), bad.as_slice()));
    }
    let bytes = zipped(&entries);
    let archive =
        Archive::read(&bytes).expect("small encoded profiles fit document admission limits");
    let mut images = EmbeddedImages::new(&archive, "profiles.odt", &[]);
    let mut output = TextOutput::default();
    formats::extract("odt", &archive, &mut images, &mut output)
        .expect("failed image validations remain explicit per-image omissions");
    assert!(
        output
            .text
            .contains("[Invalid embedded image omitted: bad0.png]"),
        "the profile fixture reaches a failed decoder construction"
    );
    assert!(
        output
            .text
            .contains("[Embedded image omitted by validation limits: bad127.png]"),
        "invalid images cannot repeatedly inflate metadata after the aggregate allowance is spent"
    );
    assert!(
        images.validation_work_bytes <= MAX_IMAGE_VALIDATION_BYTES,
        "every attempted inspection is charged before construction"
    );
    assert_eq!(
        images.files.len(),
        1,
        "failed validations preserve an earlier valid image"
    );
}

fn central_directory_fixture(actual: usize, declared: u16) -> Vec<u8> {
    // Directory-only malformed fixtures exercise admission before any entry parser/index can run.
    let mut header = [0_u8; 46];
    header[..4].copy_from_slice(b"PK\x01\x02");
    header[28..30].copy_from_slice(&1_u16.to_le_bytes());
    let mut bytes = Vec::new();
    for _ in 0..actual {
        bytes.extend_from_slice(&header);
        bytes.push(b'x');
    }
    let size = bytes.len() as u32;
    bytes.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    bytes.extend_from_slice(&declared.to_le_bytes());
    bytes.extend_from_slice(&declared.to_le_bytes());
    bytes.extend_from_slice(&size.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes
}

fn with_zip64_footer(mut bytes: Vec<u8>) -> Vec<u8> {
    let offset = bytes.len() - 22;
    let mut end = bytes.split_off(offset);
    assert!(
        end.starts_with(b"PK\x05\x06"),
        "synthetic ZIP has an uncommented footer"
    );
    let count = u16::from_le_bytes(
        end[10..12]
            .try_into()
            .expect("synthetic footer has a count"),
    );
    let size = u32::from_le_bytes(
        end[12..16]
            .try_into()
            .expect("synthetic footer has a directory size"),
    );
    let start = u32::from_le_bytes(
        end[16..20]
            .try_into()
            .expect("synthetic footer has a directory offset"),
    );
    bytes.extend_from_slice(b"PK\x06\x06");
    bytes.extend_from_slice(&44_u64.to_le_bytes());
    bytes.extend_from_slice(&45_u16.to_le_bytes());
    bytes.extend_from_slice(&45_u16.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&u64::from(count).to_le_bytes());
    bytes.extend_from_slice(&u64::from(count).to_le_bytes());
    bytes.extend_from_slice(&u64::from(size).to_le_bytes());
    bytes.extend_from_slice(&u64::from(start).to_le_bytes());
    bytes.extend_from_slice(b"PK\x06\x07");
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&(offset as u64).to_le_bytes());
    bytes.extend_from_slice(&1_u32.to_le_bytes());
    end[8..12].fill(0xff);
    end[12..20].fill(0xff);
    bytes.extend_from_slice(&end);
    bytes
}

#[test]
fn pre_index_admission_counts_actual_records_even_when_footers_underreport() {
    let exact = central_directory_fixture(5_000, 5_000);
    let admitted =
        super::zip_admission::admit(&exact).expect("the physical entry ceiling is inclusive");
    assert_eq!(
        admitted.entries, 5_000,
        "exactly 5000 records are admitted before metadata parsing"
    );
    let many = central_directory_fixture(100_000, 1);
    assert!(
        many.len() < MAX_ARCHIVE_BYTES,
        "many tiny central records fit the compressed input ceiling"
    );
    assert_eq!(
        extract_document_attachments("many.docx", &many, &[]),
        Err(DocumentExtractionError::TooManyEntries),
        "actual central records stop at 5001 before metadata allocation despite a claimed count of one"
    );
    let wide = with_zip64_footer(many);
    assert_eq!(
        extract_document_attachments("many.docx", &wide, &[]),
        Err(DocumentExtractionError::TooManyEntries),
        "ZIP64 cannot hide actual records behind its declared count either"
    );
    for bytes in [
        central_directory_fixture(2, 1),
        central_directory_fixture(1, 2),
    ] {
        assert_eq!(
            extract_document_attachments("counts.docx", &bytes, &[]),
            Err(DocumentExtractionError::InvalidArchive),
            "actual and declared counts must agree below the entry ceiling"
        );
    }
}

#[test]
fn zip64_admission_preserves_valid_documents_and_rejects_invalid_footers() {
    let normal = zipped(&[(
        "word/document.xml",
        b"<w:document><w:p><w:t>Valid</w:t></w:p></w:document>",
    )]);
    let wide_start = normal.len() - 22;
    let wide = with_zip64_footer(normal);
    let result = extract_document_attachments("wide.docx", &wide, &[])
        .expect("a valid ZIP64 footer remains supported");
    assert!(
        text(&result).contains("Valid"),
        "ZIP64 admission retains the ordinary document path"
    );
    let mut forged_count = wide.clone();
    for offset in [wide_start + 24, wide_start + 32] {
        forged_count[offset..offset + 8].copy_from_slice(&5_001_u64.to_le_bytes());
    }
    assert_eq!(
        extract_document_attachments("wide.docx", &forged_count, &[]),
        Err(DocumentExtractionError::TooManyEntries),
        "ZIP64 count admission precedes entry allocation"
    );
    let mut invalid_size = wide.clone();
    invalid_size[wide_start + 4..wide_start + 12].copy_from_slice(&43_u64.to_le_bytes());
    assert_eq!(
        extract_document_attachments("wide.docx", &invalid_size, &[]),
        Err(DocumentExtractionError::InvalidArchive),
        "the ZIP64 record must have its complete fixed-size payload"
    );
    let mut invalid_locator = wide;
    let locator = invalid_locator.len() - 22 - 20;
    invalid_locator[locator + 8..locator + 16].copy_from_slice(&u64::MAX.to_le_bytes());
    assert_eq!(
        extract_document_attachments("wide.docx", &invalid_locator, &[]),
        Err(DocumentExtractionError::InvalidArchive),
        "ZIP64 locator offsets are validated without slicing or allocation panics"
    );
}

#[test]
fn mixed_workbooks_retain_worksheets_on_both_sides_of_an_explicit_chart_omission() {
    let worksheet_kind =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet";
    let chart_kind =
        "http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet";
    let relationships = rels(&[
        ("before", "worksheets/first.xml", worksheet_kind),
        ("chart", "chartsheets/chart1.xml", chart_kind),
        ("after", "worksheets/last.xml", worksheet_kind),
    ]);
    let workbook = br#"<workbook><sheets><sheet name="Before" r:id="before"/><sheet name="Chart" r:id="chart"/><sheet name="After" r:id="after"/></sheets></workbook>"#;
    let entries = [
        ("xl/workbook.xml", workbook.as_slice()),
        ("xl/_rels/workbook.xml.rels", relationships.as_bytes()),
        (
            "xl/worksheets/first.xml",
            b"<worksheet><row><c r='A1'><v>17</v></c></row></worksheet>",
        ),
        (
            "xl/chartsheets/chart1.xml",
            b"<chartsheet><drawing r:id='chartDrawing'/></chartsheet>",
        ),
        (
            "xl/worksheets/last.xml",
            b"<worksheet><row><c r='B2'><v>42</v></c></row></worksheet>",
        ),
    ];
    let result = extract("mixed.xlsx", &entries);
    assert!(
        text(&result).contains("## Sheet: Before\n\nRange: A1:A1\n17"),
        "the first ordinary worksheet is retained"
    );
    assert!(
        text(&result).contains("## Sheet: Chart\n\n[Chart sheet content omitted by OpenChamber]"),
        "unsupported chart content has an explicit omission rather than invented empty-sheet data"
    );
    assert!(
        text(&result).contains("## Sheet: After\n\nRange: B2:B2\n42"),
        "an unsupported sheet does not erase later supported sheets"
    );
    let mut wrong_kind = entries;
    wrong_kind[3] = ("xl/chartsheets/chart1.xml", b"<worksheet/>");
    assert_eq!(
        extract_document_attachments("invalid.xlsx", &zipped(&wrong_kind), &[]),
        Err(DocumentExtractionError::InvalidDocumentStructure),
        "a verified chartsheet relationship must still name a chartsheet root"
    );
}

fn overlapping_deflate_aliases() -> Vec<u8> {
    let one = zipped(&[(
        "word/document.xml",
        b"<w:document><w:p><w:t>Valid</w:t></w:p></w:document>",
    )]);
    let footer = one.len() - 22;
    let central = u32::from_le_bytes(
        one[footer + 16..footer + 20]
            .try_into()
            .expect("synthetic central offset"),
    ) as usize;
    let template: [u8; 46] = one[central..central + 46]
        .try_into()
        .expect("synthetic central header");
    let encoded =
        u32::from_le_bytes(template[20..24].try_into().expect("synthetic encoded size")) as usize;
    let local_name =
        u16::from_le_bytes(one[26..28].try_into().expect("synthetic local name length")) as usize;
    let local_extra = u16::from_le_bytes(
        one[28..30]
            .try_into()
            .expect("synthetic local extra length"),
    ) as usize;
    let data_start = 30 + local_name + local_extra;
    let claimed = 16 * 1024 * 1024_u32;
    let mut bytes = one[..data_start + encoded].to_vec();
    // The real Deflate stream ends immediately. Every alias advertises the same large raw
    // range, which owned ZipFile::drop will drain after successfully decoding the short stream.
    bytes[18..22].copy_from_slice(&claimed.to_le_bytes());
    bytes.resize(data_start + claimed as usize, 0);
    let directory = bytes.len();
    for index in 0..5_000 {
        let name = if index == 0 {
            "word/document.xml".to_owned()
        } else {
            format!("metadata/alias{index}.xml")
        };
        let mut header = template;
        header[20..24].copy_from_slice(&claimed.to_le_bytes());
        header[28..30].copy_from_slice(&(name.len() as u16).to_le_bytes());
        header[30..34].fill(0); // No extra field or comment in the alias records.
        header[42..46].copy_from_slice(&0_u32.to_le_bytes());
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(name.as_bytes());
    }
    let directory_size = bytes.len() - directory;
    bytes.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    bytes.extend_from_slice(&5_000_u16.to_le_bytes());
    bytes.extend_from_slice(&5_000_u16.to_le_bytes());
    bytes.extend_from_slice(&(directory_size as u32).to_le_bytes());
    bytes.extend_from_slice(&(directory as u32).to_le_bytes());
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    bytes
}

#[test]
fn owned_zip_entry_drop_cannot_drain_overlapping_claims_beyond_the_input_budget() {
    use zip::ZipReadOptions;
    use zip::read::{ArchiveOffset, Config};
    use zip::unstable::read::ZipIterable;

    use super::compressed_reads::{CompressedReadBudget, CompressedReader};

    let input = overlapping_deflate_aliases();
    assert!(
        input.len() < MAX_ARCHIVE_BYTES,
        "5000 large overlapping claims fit the physical input ceiling"
    );
    let admitted =
        super::zip_admission::admit(&input).expect("all 5000 actual central records fit admission");
    assert_eq!(
        admitted.entries, 5_000,
        "entry count alone does not reject this hostile fixture"
    );
    let mut zip = ZipIterable::try_new(
        Cursor::new(input.as_slice()),
        Config {
            archive_offset: ArchiveOffset::Known(0),
        },
    )
    .expect("the exact library accepts the fixture's central framing");
    let files = zip.files().expect("central metadata iteration starts");
    let budget = CompressedReadBudget::new(input.len());
    let mut dropped = 0;
    while let Some(metadata) = files.next() {
        let metadata = metadata.expect("alias metadata parses without decoding");
        let mut source = CompressedReader::new(files.reader_mut(), &budget);
        let mut entry = metadata
            .with_reader(&mut source, ZipReadOptions::default())
            .expect("the local header is readable before the shared quota is spent");
        let mut decoded = Vec::new();
        entry
            .by_ref()
            .take(MAX_XML_ENTRY_BYTES + 1)
            .read_to_end(&mut decoded)
            .expect("the short Deflate stream decodes before cleanup reaches the quota");
        assert!(
            decoded.starts_with(b"<w:document>"),
            "this reaches a real successful Deflate decode"
        );
        let before = budget.bytes_read();
        drop(entry); // Exercises zip 9.2.2's actual owned-metadata destructor.
        dropped += 1;
        if dropped == 1 {
            assert!(
                budget.bytes_read() > before + 1024 * 1024,
                "positive control proves that the actual ZipFile destructor drains extra raw data"
            );
        }
        assert!(
            budget.bytes_read() <= input.len(),
            "decoder and destructor reads share one physical byte ceiling"
        );
        if budget.rejected() {
            break;
        }
    }
    assert!(
        budget.rejected(),
        "cleanup's swallowed read error remains visible to the owning caller"
    );
    assert!(
        dropped <= 2,
        "the first repeated range exhausts the remaining quota before thousands of aliases run"
    );
    assert_eq!(
        extract_document_attachments("aliases.docx", &input, &[]),
        Err(DocumentExtractionError::InvalidArchive),
        "production extraction reports a whole-file failure after a rejected cleanup read"
    );
    let wide = with_zip64_footer(input);
    assert_eq!(
        extract_document_attachments("aliases.docx", &wide, &[]),
        Err(DocumentExtractionError::InvalidArchive),
        "ZIP64 footer handling cannot replenish the shared compressed-read budget"
    );
}

fn write_complete_document<W: Write + Seek>(mut writer: ZipWriter<W>, method: CompressionMethod) {
    let options = SimpleFileOptions::default()
        .compression_method(method)
        .large_file(true);
    writer
        .start_file("word/document.xml", options)
        .expect("normal document entry starts");
    writer
        .write_all(b"<w:document><w:p><w:t>Complete</w:t></w:p></w:document>")
        .expect("normal document bytes write");
    writer
        .start_file("word/footer1.xml", options)
        .expect("normal footer entry starts");
    writer
        .write_all(b"<w:ftr><w:p><w:t>Footer</w:t></w:p></w:ftr>")
        .expect("normal footer bytes write");
    writer.finish().expect("normal ZIP finishes");
}

#[test]
fn compressed_read_quota_preserves_stored_deflate_zip64_and_data_descriptors() {
    for method in [CompressionMethod::Stored, CompressionMethod::Deflated] {
        for descriptors in [false, true] {
            let mut bytes = Vec::new();
            if descriptors {
                write_complete_document(ZipWriter::new_stream(&mut bytes), method);
            } else {
                write_complete_document(ZipWriter::new(Cursor::new(&mut bytes)), method);
            }
            for input in [bytes.clone(), with_zip64_footer(bytes)] {
                let result = extract_document_attachments("complete.docx", &input, &[])
                    .expect("complete non-overlapping entries fit their physical input quota");
                assert_eq!(
                    text(&result),
                    "# Document\n\nComplete\n\n## Footer\n\nFooter\n",
                    "normal stored/Deflate entries, ZIP64 local sizes, descriptors and footer data remain complete"
                );
                let mut commented = input;
                let footer = commented.len() - 22;
                let comment = b"Unused ZIP comment PK\x05\x06!";
                commented[footer + 20..footer + 22]
                    .copy_from_slice(&(comment.len() as u16).to_le_bytes());
                commented.extend_from_slice(comment);
                let result = extract_document_attachments("complete.docx", &commented, &[])
                    .expect("comments do not change complete document extraction");
                assert_eq!(
                    text(&result),
                    "# Document\n\nComplete\n\n## Footer\n\nFooter\n",
                    "removing an unused ZIP comment preserves stored/Deflate, ZIP64 and descriptor cases"
                );
            }
        }
    }
}

#[test]
fn ods_empty_cell_separators_retain_a_truncation_notice_before_the_first_value() {
    let empties = "<table:table-cell/>".repeat(166_668);
    let xml = format!(
        "<office:document><table:table table:name=\"Data\"><table:table-row>{empties}<table:table-cell><text:p>Unreached value</text:p></table:table-cell></table:table-row><table:table-row><table:table-cell><text:p>Later row</text:p></table:table-cell></table:table-row></table:table></office:document>"
    );
    let result = extract("empty-prefix.ods", &[("content.xml", xml.as_bytes())]);
    assert!(
        text(&result).encode_utf16().count() <= MAX_TEXT_UNITS,
        "dense empty-cell positions obey the text ceiling"
    );
    assert!(
        text(&result).ends_with(TRUNCATION_NOTICE),
        "a row truncated on separators cannot masquerade as a completed empty row"
    );
    assert!(
        !text(&result).contains("Unreached value") && !text(&result).contains("Later row"),
        "projection stops at the cap without inventing unobserved cell data"
    );
    let complete = extract("empty-row.ods", &[("content.xml", b"<office:document><table:table table:name='Data'><table:table-row><table:table-cell/><table:table-cell/></table:table-row><table:table-row><table:table-cell><text:p>Observed</text:p></table:table-cell></table:table-row></table:table></office:document>")]);
    assert_eq!(
        text(&complete),
        "# Workbook\n\n## Sheet: Data\n\nObserved\n",
        "a genuinely complete empty row remains omitted without a false truncation notice"
    );
}

struct FooterDiscoveryFixture {
    bytes: Vec<u8>,
    wide_offset: usize,
    footer_offset: usize,
    extension_bytes: usize,
}

fn commented_zip64_discovery_fixture() -> FooterDiscoveryFixture {
    let normal = zipped(&[(
        "word/document.xml",
        b"<w:document><w:p><w:t>Discovery</w:t></w:p></w:document>",
    )]);
    let wide_offset = normal.len() - 22;
    let mut bytes = with_zip64_footer(normal);
    let tail = bytes.split_off(wide_offset + 56);
    let extension_bytes = 16 * 1024 * 1024;
    bytes.resize(bytes.len() + extension_bytes, 0);
    bytes[wide_offset + 4..wide_offset + 12]
        .copy_from_slice(&(44 + extension_bytes as u64).to_le_bytes());
    bytes.extend_from_slice(&tail);
    let footer_offset = bytes.len() - 22;
    let locator = bytes[footer_offset - 20..footer_offset].to_vec();
    let fake_footer = bytes[footer_offset..].to_vec();
    let mut comment = Vec::new();
    for _ in 0..1_000 {
        comment.extend_from_slice(&locator);
        comment.extend_from_slice(&fake_footer);
    }
    // Fake comment lengths are zero, so none terminates at the actual input EOF. Admission
    // identifies the real footer, while zip's relaxed discovery accepts each fake candidate.
    comment.push(b'!');
    bytes[footer_offset + 20..footer_offset + 22]
        .copy_from_slice(&(comment.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&comment);
    FooterDiscoveryFixture {
        bytes,
        wide_offset,
        footer_offset,
        extension_bytes,
    }
}

#[test]
fn verified_footer_view_bounds_discovery_before_repeated_zip64_allocation() {
    use std::borrow::Cow;
    use std::cell::Cell;
    use std::io::{self, SeekFrom};

    use zip::ZipReadOptions;
    use zip::read::{ArchiveOffset, Config};
    use zip::unstable::read::ZipIterable;

    use super::compressed_reads::{CompressedReadBudget, CompressedReader};

    #[derive(Default)]
    struct DiscoveryCounters {
        read_bytes: Cell<usize>,
        wide_probes: Cell<usize>,
        sector_allocation_reads: Cell<usize>,
        retry_stopped: Cell<bool>,
    }

    struct CountedDiscovery<'input> {
        source: Cursor<&'input [u8]>,
        counters: &'input DiscoveryCounters,
        wide_offset: u64,
        extension_bytes: usize,
    }

    impl Read for CountedDiscovery<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.source.position() == self.wide_offset && buffer.len() == 4 {
                let probes = self.counters.wide_probes.get() + 1;
                self.counters.wide_probes.set(probes);
                if probes > 1 {
                    // Negative control: stop before another ZIP64 fixed header is decoded, so
                    // the unsafe original path cannot repeatedly allocate 16 MiB in this test.
                    self.counters.retry_stopped.set(true);
                    return Err(io::Error::other(
                        "Repeated ZIP64 discovery stopped by negative control",
                    ));
                }
            }
            if buffer.len() >= self.extension_bytes {
                // Pinned blocks.rs allocates the extensible-data Vec immediately before this
                // read_exact call. This observes those allocation requests without a global
                // allocator hook, and the negative-control guard precedes any repeated one.
                self.counters
                    .sector_allocation_reads
                    .set(self.counters.sector_allocation_reads.get() + 1);
            }
            let read = self.source.read(buffer)?;
            self.counters
                .read_bytes
                .set(self.counters.read_bytes.get() + read);
            Ok(read)
        }
    }

    impl Seek for CountedDiscovery<'_> {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.source.seek(position)
        }
    }

    let fixture = commented_zip64_discovery_fixture();
    assert!(
        fixture.bytes.len() < MAX_ARCHIVE_BYTES,
        "the hostile comment and large ZIP64 sector fit input admission"
    );
    let admitted = super::zip_admission::admit(&fixture.bytes)
        .expect("the real footer and central record are valid");
    let control = DiscoveryCounters::default();
    let raw = CountedDiscovery {
        source: Cursor::new(fixture.bytes.as_slice()),
        counters: &control,
        wide_offset: fixture.wide_offset as u64,
        extension_bytes: fixture.extension_bytes,
    };
    let unprepared = ZipIterable::try_new(
        raw,
        Config {
            archive_offset: ArchiveOffset::Known(0),
        },
    );
    assert!(
        unprepared.is_err() && control.retry_stopped.get(),
        "negative control proves the original input retries footer discovery after a rejected large allocation"
    );
    assert_eq!(
        control.sector_allocation_reads.get(),
        1,
        "the negative control stops before the second metadata-sector allocation request"
    );
    assert_eq!(
        control.wide_probes.get(),
        2,
        "the unsafe retry is observed before it can decode another allocating header"
    );

    let view = admitted
        .reader_view(&fixture.bytes)
        .expect("verified footer view is bounded");
    assert!(
        matches!(&view, Cow::Owned(_)),
        "a commented input uses one bounded owned view"
    );
    assert_eq!(
        view.len(),
        fixture.footer_offset + 22,
        "only the unused comment is removed"
    );
    assert_eq!(
        &view[..view.len() - 2],
        &fixture.bytes[..view.len() - 2],
        "payload, central offsets and the ZIP64 record are byte-for-byte unchanged"
    );
    assert_eq!(
        &view[view.len() - 2..],
        &[0, 0],
        "only the verified comment-length field is zeroed"
    );
    let counters = DiscoveryCounters::default();
    let source = CountedDiscovery {
        source: Cursor::new(view.as_ref()),
        counters: &counters,
        wide_offset: fixture.wide_offset as u64,
        extension_bytes: fixture.extension_bytes,
    };
    let mut zip = ZipIterable::try_new(
        source,
        Config {
            archive_offset: ArchiveOffset::Known(0),
        },
    )
    .expect("verified footer is the first accepted discovery candidate");
    assert_eq!(
        zip.len(),
        admitted.entries,
        "the same central directory is selected"
    );
    let compressed_budget = CompressedReadBudget::new(fixture.bytes.len());
    for pass in 0..2 {
        let files = zip.files().expect("bounded central directory pass starts");
        let metadata = files
            .next()
            .expect("one admitted entry exists")
            .expect("bounded central metadata parses");
        if pass == 1 {
            let mut source = CompressedReader::new(files.reader_mut(), &compressed_budget);
            let mut entry = metadata
                .with_reader(&mut source, ZipReadOptions::default())
                .expect("the admitted local entry header remains unchanged");
            let mut decoded = Vec::new();
            entry
                .by_ref()
                .take(MAX_XML_ENTRY_BYTES + 1)
                .read_to_end(&mut decoded)
                .expect("the original Deflate bytes still decode");
            drop(entry);
            assert!(
                decoded
                    .windows(b"Discovery".len())
                    .any(|bytes| bytes == b"Discovery"),
                "both metadata passes and entry decoding retain the original document"
            );
        }
    }
    assert!(
        !compressed_budget.rejected(),
        "the existing entry and Drop quota remains active and unspent"
    );
    assert!(
        !counters.retry_stopped.get(),
        "the safe view never retries a ZIP64 candidate"
    );
    assert_eq!(
        counters.wide_probes.get(),
        1,
        "Known(0) reaches the verified ZIP64 record once"
    );
    assert_eq!(
        counters.sector_allocation_reads.get(),
        1,
        "only one input-bounded ZIP64 metadata allocation is requested"
    );
    assert!(
        counters.read_bytes.get() <= view.len() + 4 * 1024,
        "actual constructor, two metadata passes and entry reads stay within one sector plus bounded framing work"
    );
    let result = extract_document_attachments("discovery.docx", &fixture.bytes, &[])
        .expect("production extraction uses the verified footer view");
    assert_eq!(
        text(&result),
        "# Document\n\nDiscovery\n",
        "the unused hostile comment never changes attachment content"
    );
}
