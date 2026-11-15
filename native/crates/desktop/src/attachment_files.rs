//! Pure preparation of one complete local attachment, matching shared attachment-files.ts.
//!
//! The caller owns bounded reads, filename/path safety, draft identity and transport. This
//! module never reads a path or stages bytes. A failure affects only the supplied file.
//! Display filenames retain their spelling, and all text becomes UTF-8 through File.text's
//! lossy decoding rules. Document extraction and HEIC conversion require separate converters.

use std::collections::BTreeSet;
use std::fmt;

use openchamber_native_transport::endpoint::Endpoint;
use serde_json::{Map, Value};

/// The shared input-store.ts ceiling for reading one local file into an inline prompt.
///
/// This is an input limit. It does not establish upload, normalized-output or journal limits.
pub(crate) const MAX_INLINE_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;

const SAMPLE_BYTES: usize = 4096;
const REDACTED: &str = "[REDACTED]";
const OMITTED: &str = "[OMITTED BY OPENCHAMBER]";

/// Complete normalized bytes and display metadata, with no path or upload authority.
#[derive(PartialEq, Eq)]
pub(crate) struct NormalizedAttachmentFile {
    pub filename: String,
    pub media_type: &'static str,
    pub content: Vec<u8>,
}

impl fmt::Debug for NormalizedAttachmentFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NormalizedAttachmentFile")
            .field("media_type", &self.media_type)
            .field("content_bytes", &self.content.len())
            .finish_non_exhaustive()
    }
}

/// A document/image conversion unavailable in this basic native port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequiredConversion {
    OfficeDocument,
    OpenDocument,
    HeicImage,
}

/// Content-free preparation failures for the caller to report per selected file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NormalizationError {
    InputTooLarge,
    UnsupportedConversion(RequiredConversion),
    UnsupportedContent,
    MalformedHar,
}

/// Prepares one complete input without truncating it or changing any sibling attachment.
///
/// `content` must contain the entire input, not a prefix from a capped read. The caller must
/// reject incomplete reads. Use `MAX_INLINE_ATTACHMENT_BYTES` for the shared inline policy;
/// another staging owner must supply its own established input limit explicitly. Normalized
/// output can grow through UTF-8 replacement, HAR formatting or notebook rendering. Its
/// storage and transport limits remain the caller's responsibility.
pub(crate) fn normalize_attachment_file(
    filename: &str,
    declared_media_type: &str,
    content: &[u8],
    max_input_bytes: usize,
) -> Result<NormalizedAttachmentFile, NormalizationError> {
    if content.len() > max_input_bytes {
        return Err(NormalizationError::InputTooLarge);
    }

    let extension = filename.rsplit_once('.').map_or("", |(_, ext)| ext);
    let extension = extension.to_ascii_lowercase();
    let media_type = declared_media_type.split(';').next().unwrap_or_default();
    let media_type = media_type.trim_matches(js_whitespace).to_ascii_lowercase();
    if let Some(conversion) = required_conversion(&extension, &media_type) {
        return Err(NormalizationError::UnsupportedConversion(conversion));
    }

    let (media_type, content) = match extension.as_str() {
        "har" => {
            let text = file_text(content);
            let mut value: Value =
                serde_json::from_str(&text).map_err(|_| NormalizationError::MalformedHar)?;
            sanitize_har_value(&mut value, None);
            let mut sanitized =
                serde_json::to_vec_pretty(&value).map_err(|_| NormalizationError::MalformedHar)?;
            sanitized.push(b'\n');
            ("text/plain", sanitized)
        }
        "ipynb" => {
            let text = file_text(content);
            let rendered = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|value| notebook_text(&value, filename));
            ("text/plain", rendered.unwrap_or(text).into_bytes())
        }
        _ => {
            let media_type = attachment_media_type(&extension, &media_type, content)
                .ok_or(NormalizationError::UnsupportedContent)?;
            let content = if media_type == "text/plain" {
                file_text(content).into_bytes()
            } else {
                content.to_vec()
            };
            (media_type, content)
        }
    };

    Ok(NormalizedAttachmentFile {
        filename: filename.to_owned(),
        media_type,
        content,
    })
}

fn required_conversion(extension: &str, media_type: &str) -> Option<RequiredConversion> {
    if matches!(extension, "heic" | "heif") || matches!(media_type, "image/heic" | "image/heif") {
        return Some(RequiredConversion::HeicImage);
    }
    if matches!(extension, "docx" | "pptx" | "xlsx")
        || media_type.starts_with("application/vnd.openxmlformats-officedocument.")
        || matches!(
            media_type,
            "application/msword" | "application/vnd.ms-excel" | "application/vnd.ms-powerpoint"
        )
    {
        return Some(RequiredConversion::OfficeDocument);
    }
    if matches!(extension, "odt" | "odp" | "ods")
        || media_type.starts_with("application/vnd.oasis.opendocument.")
    {
        return Some(RequiredConversion::OpenDocument);
    }
    None
}

fn binary_media_type(media_type: &str) -> Option<&'static str> {
    match media_type {
        "image/png" => Some("image/png"),
        "image/jpeg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        "application/pdf" => Some("application/pdf"),
        _ => None,
    }
}

fn attachment_media_type(extension: &str, declared: &str, content: &[u8]) -> Option<&'static str> {
    if let Some(media_type) = binary_media_type(declared) {
        return Some(media_type);
    }
    if declared.is_empty() || declared == "application/octet-stream" {
        let fallback = match extension {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            "pdf" => Some("application/pdf"),
            _ => None,
        };
        if fallback.is_some() {
            return fallback;
        }
    }
    // Picker extensions are not an allowlist for decoding arbitrary binary content.
    if declared.starts_with("text/")
        || matches!(
            declared,
            "application/json"
                | "application/ld+json"
                | "application/toml"
                | "application/x-toml"
                | "application/x-yaml"
                | "application/xml"
                | "application/yaml"
                | "image/svg+xml"
        )
        || declared.ends_with("+json")
        || declared.ends_with("+xml")
    {
        return Some("text/plain");
    }
    let sample = &content[..content.len().min(SAMPLE_BYTES)];
    if sample.contains(&0) {
        return None;
    }
    let controls = sample
        .iter()
        .filter(|&&byte| byte < 9 || (byte > 13 && byte < 32))
        .count();
    (controls * 10 <= sample.len() * 3).then_some("text/plain")
}

fn file_text(content: &[u8]) -> String {
    // Blob.text decodes UTF-8 without consulting MIME charset and consumes one UTF-8 BOM.
    let content = content.strip_prefix(b"\xef\xbb\xbf").unwrap_or(content);
    String::from_utf8_lossy(content).into_owned()
}

fn js_whitespace(character: char) -> bool {
    matches!(character, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}'
        | '\u{3000}' | '\u{feff}')
}

fn source_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(lines)) => {
            let mut text = String::new();
            for line in lines {
                if let Some(line) = line.as_str() {
                    text.push_str(line);
                }
            }
            text
        }
        _ => String::new(),
    }
}

fn notebook_text(value: &Value, filename: &str) -> Option<String> {
    let cells = value.get("cells")?.as_array()?;
    let language = value
        .pointer("/metadata/kernelspec/language")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut sections = vec![format!("# Notebook: {filename}")];
    for (index, cell) in cells.iter().enumerate() {
        let source = source_text(cell.get("source"));
        let source = source.trim_end_matches(js_whitespace);
        match cell.get("cell_type").and_then(Value::as_str) {
            Some("markdown") => {
                sections.push(format!("## Markdown cell {}\n\n{source}", index + 1));
                continue;
            }
            Some("code") => {
                sections.push(format!(
                    "## Code cell {}\n\n```{language}\n{source}\n```",
                    index + 1
                ));
            }
            _ => continue,
        }
        let Some(raw_outputs) = cell.get("outputs").and_then(Value::as_array) else {
            continue;
        };
        let mut outputs = Vec::new();
        for output in raw_outputs {
            let mut text = source_text(output.get("text"));
            if text.is_empty() {
                text = source_text(output.get("traceback"));
            }
            if !text.is_empty() {
                outputs.push(text.trim_end_matches(js_whitespace).to_owned());
                continue;
            }
            if let Some(data) = output
                .get("data")
                .filter(|data| data.is_object() || data.is_array())
            {
                let plain = source_text(data.get("text/plain"));
                if !plain.is_empty() {
                    outputs.push(plain.trim_end_matches(js_whitespace).to_owned());
                }
                // GPUI already enables serde_json's preserve_order feature in this desktop crate.
                let omitted: Vec<String> = match data {
                    Value::Object(record) => record
                        .keys()
                        .filter(|key| key.as_str() != "text/plain")
                        .cloned()
                        .collect(),
                    Value::Array(items) => {
                        (0..items.len()).map(|index| index.to_string()).collect()
                    }
                    _ => unreachable!("only object or array data reaches output omission"),
                };
                if !omitted.is_empty() {
                    outputs.push(format!("[Non-text output omitted: {}]", omitted.join(", ")));
                }
                continue;
            }
            let name = output.get("ename");
            let value = output.get("evalue");
            if name.is_some_and(Value::is_string) || value.is_some_and(Value::is_string) {
                let name = name.filter(|value| !value.is_null()).map(js_string);
                let value = value.filter(|value| !value.is_null()).map(js_string);
                let text = format!(
                    "{}: {}",
                    name.as_deref().unwrap_or("Error"),
                    value.as_deref().unwrap_or_default()
                );
                outputs.push(text.trim_end_matches(js_whitespace).to_owned());
            }
        }
        if !outputs.is_empty() {
            sections.push(format!("### Output\n\n{}", outputs.join("\n\n")));
        }
    }
    Some(format!("{}\n", sections.join("\n\n")))
}

fn js_string(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(_) => "[object Object]".to_owned(),
        Value::Array(items) => {
            let items: Vec<String> = items
                .iter()
                .map(|item| {
                    if item.is_null() {
                        String::new()
                    } else {
                        js_string(item)
                    }
                })
                .collect();
            items.join(",")
        }
        _ => value.to_string(),
    }
}

fn sensitive_name(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization"
            | "proxy-authorization"
            | "cookie"
            | "set-cookie"
            | "x-api-key"
            | "apikey"
            | "api-key"
            | "api_key"
            | "clientsecret"
            | "client-secret"
            | "client_secret"
            | "password"
            | "secret"
            | "accesstoken"
            | "access-token"
            | "access_token"
            | "refreshtoken"
            | "refresh-token"
            | "refresh_token"
            | "idtoken"
            | "id-token"
            | "id_token"
            | "token"
    )
}

fn redact_url(value: &str) -> String {
    // Match URL parsing's removal of surrounding C0/space and embedded ASCII newlines/tabs.
    let candidate: String = value
        .trim_matches(|character: char| character <= ' ')
        .chars()
        .filter(|character| !matches!(character, '\t' | '\n' | '\r'))
        .collect();
    let Some((scheme, _)) = candidate.split_once(':') else {
        return value.to_owned();
    };
    if !scheme.starts_with(|character: char| character.is_ascii_alphabetic())
        || !scheme
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        return value.to_owned();
    }
    // Reuse the URL parser exposed by Endpoint, as markdown_links.rs does. An opaque
    // base makes absolute inputs such as "https:example.com" independent of that base.
    let endpoint = Endpoint::parse("https://native.invalid/").expect("constant base URL");
    let base = endpoint
        .url("")
        .join("data:,")
        .expect("constant opaque URL");
    let Ok(mut url) = base.join(&candidate) else {
        return value.to_owned();
    };
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut redacted_names = BTreeSet::new();
    let mut changed = false;
    for (name, value) in url.query_pairs() {
        if sensitive_name(&name) {
            changed = true;
            // URLSearchParams.set collapses duplicate names, comparing their exact case.
            if !redacted_names.insert(name.to_string()) {
                continue;
            }
            pairs.push((name.into_owned(), REDACTED.to_owned()));
        } else {
            pairs.push((name.into_owned(), value.into_owned()));
        }
    }
    if changed {
        url.query_pairs_mut().clear().extend_pairs(pairs);
    }
    url.into()
}

fn sanitize_har_value(value: &mut Value, key: Option<&str>) {
    if key.is_some_and(sensitive_name) {
        *value = Value::String(REDACTED.to_owned());
        return;
    }
    if key == Some("cookies")
        && let Value::Array(cookies) = value
    {
        // The canonical cookies branch replaces each value shallowly and returns.
        for cookie in cookies {
            match cookie {
                Value::Object(record) => {
                    record.insert("value".to_owned(), Value::String(REDACTED.to_owned()));
                }
                Value::Array(items) => {
                    let mut record = Map::new();
                    for (index, item) in std::mem::take(items).into_iter().enumerate() {
                        record.insert(index.to_string(), item);
                    }
                    record.insert("value".to_owned(), Value::String(REDACTED.to_owned()));
                    *cookie = Value::Object(record);
                }
                _ => {}
            }
        }
        return;
    }
    if matches!(key, Some("text" | "encoding")) {
        *value = Value::String(OMITTED.to_owned());
        return;
    }
    match value {
        Value::String(text) if key == Some("url") => *text = redact_url(text),
        Value::Array(items) => {
            for item in items {
                sanitize_har_value(item, None);
            }
        }
        Value::Object(record) => {
            let sensitive_entry = record
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(sensitive_name);
            for (name, value) in record {
                if sensitive_entry && name == "value" {
                    *value = Value::String(REDACTED.to_owned());
                } else {
                    sanitize_har_value(value, Some(name));
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepare(filename: &str, media_type: &str, content: &[u8]) -> NormalizedAttachmentFile {
        normalize_attachment_file(filename, media_type, content, MAX_INLINE_ATTACHMENT_BYTES)
            .expect("synthetic fixture is supported")
    }

    fn text(prepared: &NormalizedAttachmentFile) -> &str {
        std::str::from_utf8(&prepared.content).expect("prepared text is UTF-8")
    }

    #[test]
    fn supported_binary_mimes_and_fallback_extensions_preserve_complete_bytes() {
        let bytes = b"\0\xff\x01synthetic-binary";
        for (extension, media_type) in [
            ("png", "image/png"),
            ("jpg", "image/jpeg"),
            ("jpeg", "image/jpeg"),
            ("gif", "image/gif"),
            ("webp", "image/webp"),
            ("pdf", "application/pdf"),
        ] {
            for declared in ["", "application/octet-stream", media_type] {
                let filename = format!("FILE.{}", extension.to_ascii_uppercase());
                let prepared = prepare(&filename, declared, bytes);
                assert_eq!(
                    prepared.filename, filename,
                    "display name remains unchanged"
                );
                assert_eq!(
                    prepared.media_type, media_type,
                    "binary MIME matches its kind"
                );
                assert_eq!(
                    prepared.content, bytes,
                    "binary bytes are never decoded or truncated"
                );
            }
            let prepared = prepare("unknown.data", &media_type.to_ascii_uppercase(), bytes);
            assert_eq!(
                prepared.media_type, media_type,
                "declared MIME supports missing extensions"
            );
        }
        assert_eq!(
            prepare("photo.png", " IMAGE/JPEG ; ignored=1", bytes).media_type,
            "image/jpeg",
            "supported declared MIME takes precedence over a conflicting extension"
        );
    }

    #[test]
    fn textual_mime_families_decode_as_utf8_and_ignore_charset() {
        for declared in [
            "text/plain",
            "text/x-python",
            "application/json",
            "application/ld+json",
            "application/toml",
            "application/x-toml",
            "application/x-yaml",
            "application/xml",
            "application/yaml",
            "image/svg+xml",
            "application/example+json",
            "application/example+xml",
            " TEXT/PLAIN ; charset=iso-8859-1",
        ] {
            let prepared = prepare("source.unknown", declared, b"\xef\xbb\xbfhello\xff\r\n");
            assert_eq!(prepared.media_type, "text/plain", "text MIME is normalized");
            assert_eq!(
                text(&prepared),
                "hello\u{fffd}\r\n",
                "File.text uses lossy UTF-8"
            );
        }
        let prepared = prepare("source.txt", "", b"\xef\xbb\xbf\xef\xbb\xbftext");
        assert_eq!(
            text(&prepared),
            "\u{feff}text",
            "only the leading UTF-8 BOM is consumed"
        );
    }

    #[test]
    fn canonical_plain_code_and_diagram_extensions_use_text_sniffing() {
        let extensions = "bat c cc cjs cmd conf cpp cs css csv cts dart diff drawio excalidraw env \
            erl ex exs fs fsx go gql graphql h hh hcl hpp hrl htm html ini java jl js json jsonl \
            jsx kt kts log lua md mdx mjs mts ndjson patch php proto ps1 py r rb rs sass scala \
            scss sh sol sql svelte svg swift tf toml ts tsx txt vue xml yaml yml zig zsh text \
            markdown jsonld unlisted";
        for extension in extensions.split_whitespace() {
            let prepared = prepare(&format!("source.{extension}"), "", b"source\n");
            assert_eq!(prepared.media_type, "text/plain", "text source is accepted");
            assert_eq!(text(&prepared), "source\n", "source remains readable");
        }
        for (filename, declared, source) in [
            ("diagram.SVG", "image/svg+xml", "<svg></svg>"),
            ("diagram.DRAWIO", "application/xml", "<mxfile></mxfile>"),
        ] {
            let prepared = prepare(filename, declared, source.as_bytes());
            assert_eq!(
                prepared.media_type, "text/plain",
                "diagram is attached as source"
            );
            assert_eq!(text(&prepared), source, "diagram source is not rendered");
        }
    }

    #[test]
    fn binary_sniff_rejects_nuls_and_more_than_thirty_percent_controls() {
        for bytes in [b"normal\0source".as_slice(), b"\x01\x02\x03\x04abcdef"] {
            assert_eq!(
                normalize_attachment_file("source.rs", "", bytes, MAX_INLINE_ATTACHMENT_BYTES),
                Err(NormalizationError::UnsupportedContent),
                "a code extension alone cannot admit binary bytes"
            );
        }
        assert_eq!(
            prepare("unknown", "", b"\x01\x02\x03abcdefg").media_type,
            "text/plain",
            "exactly thirty percent control bytes are accepted by the shared sniff"
        );
        assert_eq!(
            prepare("empty", "", b"").content,
            b"",
            "empty unknown input is text"
        );
        assert_eq!(
            prepare("unknown", "", b"\t\n\x0b\x0c\r\x7f").media_type,
            "text/plain",
            "the canonical control-byte predicate excludes whitespace and DEL"
        );
        let mut bytes = vec![b'a'; SAMPLE_BYTES];
        bytes.push(0);
        let prepared = prepare("unknown", "application/octet-stream", &bytes);
        assert_eq!(
            prepared.content, bytes,
            "sniffing inspects only the first 4 KiB, without truncation"
        );
        assert_eq!(
            normalize_attachment_file("file.png", "application/unknown", b"\0binary", 100),
            Err(NormalizationError::UnsupportedContent),
            "binary extension fallback applies only to empty or octet-stream MIME"
        );
    }

    #[test]
    fn unavailable_converters_have_distinct_content_free_results() {
        for (extensions, media_types, conversion) in [
            (
                ["docx", "pptx", "xlsx"].as_slice(),
                [
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation",
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                    "application/msword",
                    "application/vnd.ms-excel",
                    "application/vnd.ms-powerpoint",
                ]
                .as_slice(),
                RequiredConversion::OfficeDocument,
            ),
            (
                ["odt", "odp", "ods"].as_slice(),
                [
                    "application/vnd.oasis.opendocument.text",
                    "application/vnd.oasis.opendocument.presentation",
                    "application/vnd.oasis.opendocument.spreadsheet",
                ]
                .as_slice(),
                RequiredConversion::OpenDocument,
            ),
            (
                ["heic", "heif"].as_slice(),
                ["image/heic", "image/heif"].as_slice(),
                RequiredConversion::HeicImage,
            ),
        ] {
            for extension in extensions {
                let filename = format!("FILE.{}", extension.to_ascii_uppercase());
                assert_eq!(
                    normalize_attachment_file(&filename, "text/plain", b"source", 100),
                    Err(NormalizationError::UnsupportedConversion(conversion)),
                    "converter-required extensions never pass through as text"
                );
            }
            for media_type in media_types {
                assert_eq!(
                    normalize_attachment_file("unknown.bin", media_type, b"source", 100),
                    Err(NormalizationError::UnsupportedConversion(conversion)),
                    "declared converter-required types are rejected even with misleading names"
                );
            }
        }
    }

    #[test]
    fn notebook_fixture_renders_source_text_and_omits_nontext_outputs() {
        // Derived from attachment-files.test.ts with additional synthetic output variants.
        let notebook = serde_json::json!({
            "metadata": { "kernelspec": { "language": "python" } },
            "cells": [
                { "cell_type": "markdown", "source": ["# Analysis\n", 42, "Notes\n"] },
                {
                    "cell_type": "code", "source": ["print('ok')\n"],
                    "outputs": [
                        { "text": ["ok\n"] },
                        { "data": { "text/plain": ["result"], "image/png": "base64-image" } },
                        { "traceback": ["Trace\n", "Failure\n"] },
                        { "ename": "ValueError", "evalue": "bad input" },
                        { "evalue": "default name" },
                        { "text": "stream wins", "data": { "image/jpeg": "hidden-image" } },
                        null,
                    ]
                },
                { "cell_type": "raw", "source": "ignored raw cell" },
            ]
        });
        let bytes = serde_json::to_vec(&notebook).expect("synthetic notebook serializes");
        let prepared = prepare("analysis.IPYNB", "application/json", &bytes);
        assert_eq!(prepared.media_type, "text/plain", "notebook becomes text");
        assert_eq!(
            text(&prepared),
            "# Notebook: analysis.IPYNB\n\n## Markdown cell 1\n\n# Analysis\nNotes\n\n\
             ## Code cell 2\n\n```python\nprint('ok')\n```\n\n### Output\n\nok\n\nresult\n\n\
             [Non-text output omitted: image/png]\n\nTrace\nFailure\n\nValueError: bad input\n\n\
             Error: default name\n\nstream wins\n",
            "notebook rendering matches the canonical cell and output algorithm"
        );
        assert!(
            !text(&prepared).contains("base64-image"),
            "binary output data is omitted"
        );
        assert!(
            !text(&prepared).contains("hidden-image"),
            "stream output has precedence over data"
        );
    }

    #[test]
    fn notebook_output_order_and_javascript_trim_rules_are_preserved() {
        let bytes = br#"{"cells":[null,{"cell_type":"code","source":"x\u0085\ufeff","outputs":[{"data":{"text/html":"b","image/png":"a","text/plain":"plain"}}]}]}"#;
        let prepared = prepare("ordered.ipynb", "", bytes);
        assert!(
            text(&prepared).contains("```\nx\u{0085}\n```"),
            "trimEnd keeps NEL and removes BOM whitespace"
        );
        assert!(
            text(&prepared).contains("plain\n\n[Non-text output omitted: text/html, image/png]"),
            "nontext MIME names keep source order using the desktop's existing preserve_order feature"
        );
        assert_eq!(
            text(&prepare("empty.ipynb", "", br#"{"cells":[]}"#)),
            "# Notebook: empty.ipynb\n",
            "an empty notebook still renders its heading"
        );
    }

    #[test]
    fn malformed_notebooks_fall_back_to_decoded_source_but_hars_are_rejected() {
        for bytes in [
            b"not valid JSON".as_slice(),
            b"{\"cells\":true}",
            b"null",
            b"[]",
            b"\xff",
        ] {
            let prepared = prepare("broken.ipynb", "application/json", bytes);
            assert_eq!(
                text(&prepared),
                file_text(bytes),
                "unrenderable notebooks retain decoded source"
            );
        }
        assert_eq!(
            normalize_attachment_file("network.HAR", "text/plain", b"not valid HAR JSON", 100),
            Err(NormalizationError::MalformedHar),
            "a malformed HAR must never fall back to unsanitized source"
        );
        assert_eq!(
            text(&prepare("network.har", "", b"null")),
            "null\n",
            "the canonical HAR sanitizer accepts any valid JSON root"
        );
    }

    #[test]
    fn har_fixture_redacts_nested_credentials_cookies_queries_and_bodies() {
        // Derived from attachment-files.test.ts. All payload values are synthetic.
        let har = serde_json::json!({
            "log": { "entries": [{
                "request": {
                    "url": "https://example.com/api?token=secret&query=visible",
                    "headers": [
                        { "name": "Authorization", "value": "Bearer secret" },
                        { "name": "Accept", "value": "application/json" }
                    ],
                    "cookies": [{ "name": "session", "value": "cookie-secret", "path": "/" }],
                    "postData": { "mimeType": "application/json", "text": "{\"password\":\"secret\"}" },
                    "queryString": [{ "name": "client_secret", "value": "nested-secret" }],
                    "extra": [{ "authorization": { "nested": "credential" }, "password": "password-secret" }]
                },
                "response": {
                    "headers": [{ "name": "Set-Cookie", "value": "session=secret" }],
                    "content": { "mimeType": "application/json", "encoding": "base64", "text": "secret response" }
                }
            }] }
        });
        let bytes = serde_json::to_vec(&har).expect("synthetic HAR serializes");
        let prepared = prepare("network.har", "application/json", &bytes);
        let sanitized: Value =
            serde_json::from_slice(&prepared.content).expect("sanitized HAR is JSON");
        let entry = &sanitized["log"]["entries"][0];
        assert_eq!(
            entry["request"]["url"],
            "https://example.com/api?token=%5BREDACTED%5D&query=visible"
        );
        assert_eq!(entry["request"]["headers"][0]["value"], REDACTED);
        assert_eq!(entry["request"]["headers"][1]["value"], "application/json");
        assert_eq!(entry["request"]["cookies"][0]["value"], REDACTED);
        assert_eq!(entry["request"]["cookies"][0]["path"], "/");
        assert_eq!(entry["request"]["postData"]["text"], OMITTED);
        assert_eq!(entry["request"]["queryString"][0]["value"], REDACTED);
        assert_eq!(entry["request"]["extra"][0]["authorization"], REDACTED);
        assert_eq!(entry["request"]["extra"][0]["password"], REDACTED);
        assert_eq!(entry["response"]["headers"][0]["value"], REDACTED);
        assert_eq!(entry["response"]["content"]["text"], OMITTED);
        assert_eq!(entry["response"]["content"]["encoding"], OMITTED);
        for omitted in [
            "Bearer secret",
            "cookie-secret",
            "nested-secret",
            "password-secret",
            "secret response",
        ] {
            assert!(
                !text(&prepared).contains(omitted),
                "synthetic secret is absent from prepared HAR"
            );
        }
        assert!(
            prepared.content.ends_with(b"\n"),
            "pretty HAR output ends with a newline"
        );
    }

    #[test]
    fn har_redaction_uses_exact_sensitive_names_and_urlsearchparams_set_semantics() {
        let names = "Authorization PROXY-AUTHORIZATION Cookie Set-Cookie X-Api-Key apikey api-key \
            api_key clientsecret client-secret client_secret Password Secret accesstoken access-token \
            access_token refreshtoken refresh-token refresh_token idtoken id-token id_token Token";
        let mut record = Map::new();
        for name in names.split_whitespace() {
            record.insert(
                name.to_owned(),
                Value::String("synthetic credential".to_owned()),
            );
        }
        record.insert(
            "not-a-token".to_owned(),
            Value::String("visible".to_owned()),
        );
        record.insert("text".to_owned(), serde_json::json!({ "body": "hidden" }));
        record.insert("encoding".to_owned(), Value::Null);
        record.insert("Text".to_owned(), Value::String("visible".to_owned()));
        let bytes = serde_json::to_vec(&record).expect("synthetic HAR object serializes");
        let prepared = prepare("network.har", "", &bytes);
        let value: Value = serde_json::from_slice(&prepared.content).expect("prepared HAR parses");
        for name in names.split_whitespace() {
            assert_eq!(
                value[name], REDACTED,
                "the exact sensitive-name set is case insensitive"
            );
        }
        assert_eq!(
            value["not-a-token"], "visible",
            "partial sensitive-name matches are retained"
        );
        assert_eq!(
            value["text"], OMITTED,
            "body omission precedes recursion for every value kind"
        );
        assert_eq!(
            value["encoding"], OMITTED,
            "encoding omission applies even to null"
        );
        assert_eq!(
            value["Text"], "visible",
            "body-key comparisons are case sensitive"
        );
        assert_eq!(
            redact_url(
                "https://EXAMPLE.com?%74oken=one&token=two&TOKEN=three&safe=a%20b&safe=c#hash"
            ),
            "https://example.com/?token=%5BREDACTED%5D&TOKEN=%5BREDACTED%5D&safe=a+b&safe=c#hash",
            "query mutation decodes names, collapses exact duplicates and reencodes all pairs"
        );
        assert_eq!(
            redact_url("https:example.com?token=secret"),
            "https://example.com/?token=%5BREDACTED%5D"
        );
        assert_eq!(
            redact_url("relative?token=secret"),
            "relative?token=secret",
            "invalid absolute URLs retain the canonical fallback"
        );
        assert_eq!(
            redact_url("http://[invalid?token=secret"),
            "http://[invalid?token=secret"
        );
    }

    #[test]
    fn input_limits_reject_whole_files_and_do_not_limit_normalized_output() {
        assert_eq!(
            MAX_INLINE_ATTACHMENT_BYTES,
            10 * 1024 * 1024,
            "shared input policy remains 10 MiB"
        );
        assert_eq!(
            normalize_attachment_file("file.txt", "", b"abcd", 3),
            Err(NormalizationError::InputTooLarge),
            "over-limit input is refused without passing a truncated prefix"
        );
        let exact =
            normalize_attachment_file("file.txt", "", b"abc", 3).expect("exact limit is allowed");
        assert_eq!(
            exact.content, b"abc",
            "all input bytes are retained at the exact limit"
        );
        let decoded =
            normalize_attachment_file("file.txt", "text/plain", b"\xff", 1).expect("input fits");
        assert_eq!(
            decoded.content.len(),
            3,
            "UTF-8 replacement can grow beyond the input limit"
        );
        let empty = normalize_attachment_file("empty", "", b"", 0).expect("zero-sized input fits");
        assert!(
            empty.content.is_empty(),
            "zero limit does not invent a nonzero minimum"
        );
        assert_eq!(
            normalize_attachment_file("network.har", "", b"bad JSON", 1),
            Err(NormalizationError::InputTooLarge),
            "input bounds are enforced before structured parsing"
        );
        let oversized = vec![b'a'; MAX_INLINE_ATTACHMENT_BYTES + 1];
        assert_eq!(
            normalize_attachment_file("large.txt", "", &oversized, MAX_INLINE_ATTACHMENT_BYTES),
            Err(NormalizationError::InputTooLarge),
            "the established shared inline ceiling rejects a whole oversized input"
        );
    }

    #[test]
    fn a_failed_file_leaves_successful_siblings_unchanged() {
        let first = prepare("first.txt", "", b"first");
        let failed = normalize_attachment_file("failed.har", "", b"invalid HAR", 100);
        let last = prepare("last.txt", "", b"last");
        assert_eq!(failed, Err(NormalizationError::MalformedHar));
        assert_eq!(
            first.content, b"first",
            "a later failure cannot erase an earlier result"
        );
        assert_eq!(
            last.content, b"last",
            "a failed input cannot prevent later preparation"
        );
    }

    #[test]
    fn debug_output_contains_no_filename_or_content() {
        let prepared = prepare("private-filename.txt", "text/plain", b"private-content");
        let debug = format!("{prepared:?}");
        assert!(
            !debug.contains("private-filename"),
            "debug omits the filename"
        );
        assert!(
            !debug.contains("private-content"),
            "debug omits attachment bytes"
        );
        assert_eq!(
            format!("{:?}", NormalizationError::MalformedHar),
            "MalformedHar",
            "errors contain only typed reasons"
        );
    }
}
