//! `data:` URLs, as a prompt's inline file arrives.

/// A decoded `data:` URL: its media type and bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataUrl {
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Decodes `data:<mime>[;params];base64,<data>`; percent-encoded data URLs
/// and malformed base64 answer `None`.
pub fn decode(url: &str) -> Option<DataUrl> {
    let rest = url.strip_prefix("data:")?;
    let (header, data) = rest.split_once(',')?;
    let mut fields = header.split(';');
    let mime = fields
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if !fields.any(|field| field.trim().eq_ignore_ascii_case("base64")) {
        return None;
    }
    Some(DataUrl {
        mime,
        bytes: decode_base64(data)?,
    })
}

/// Standard base64 with optional padding; whitespace is skipped.
fn decode_base64(data: &str) -> Option<Vec<u8>> {
    let value = |byte: u8| -> Option<u32> {
        Some(match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32)
    };
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0;
    let mut padding = false;
    for byte in data.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte == b'=' {
            padding = true;
            continue;
        }
        if padding {
            return None;
        }
        buffer = (buffer << 6) | value(byte)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    // Leftover bits must be zero padding of the last byte.
    (bits < 6 && buffer == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64_data_urls() {
        assert_eq!(
            decode("data:text/plain;base64,aGk="),
            Some(DataUrl {
                mime: "text/plain".to_owned(),
                bytes: b"hi".to_vec(),
            })
        );
        assert_eq!(
            decode("data:image/PNG;charset=x;base64,aGVsbG8gd29ybGQ").map(|url| url.bytes),
            Some(b"hello world".to_vec())
        );
    }

    #[test]
    fn rejects_what_it_cannot_read() {
        assert_eq!(decode("file:///a.png"), None);
        assert_eq!(
            decode("data:text/plain,hi"),
            None,
            "percent-encoded data is not read"
        );
        assert_eq!(decode("data:text/plain;base64,a*Gk="), None);
        assert_eq!(decode("data:text/plain;base64,aGk=aGk="), None);
    }
}
