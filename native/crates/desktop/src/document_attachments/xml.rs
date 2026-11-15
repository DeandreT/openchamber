//! A bounded flat XML tree. Traversal and destruction never recurse through document depth.

use quick_xml::XmlVersion;
use quick_xml::events::{BytesStart, Event};
use quick_xml::reader::Reader;

use super::DocumentExtractionError;

const MAX_XML_NODES: usize = 250_000;
const MAX_XML_DEPTH: usize = 128;
const MAX_ATTRIBUTES: usize = 128;

pub(super) struct Node {
    pub name: String,
    pub text: String,
    attributes: Vec<(String, String)>,
    /// Exclusive end of this node's subtree in pre-order, including this node.
    pub end: usize,
}

impl Node {
    pub(super) fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .filter(|value| !value.is_empty())
    }
}

pub(super) struct Xml {
    pub nodes: Vec<Node>,
}

impl Xml {
    pub(super) fn parse(bytes: &[u8]) -> Result<Self, DocumentExtractionError> {
        let source = std::str::from_utf8(bytes).map_err(|_| DocumentExtractionError::InvalidXml)?;
        let source = source.strip_prefix('\u{feff}').unwrap_or(source);
        if source.chars().any(|character| !legal_character(character)) {
            return Err(DocumentExtractionError::InvalidXml);
        }
        let mut reader = Reader::from_str(source);
        reader.config_mut().check_comments = true;
        let mut nodes: Vec<Node> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        let mut roots = 0;
        let mut declared = false;
        loop {
            let before = reader.buffer_position();
            let event = reader
                .read_event()
                .map_err(|_| DocumentExtractionError::InvalidXml)?;
            match event {
                Event::Start(tag) | Event::Empty(tag) => {
                    // Empty events are identified from the already-consumed source. This keeps
                    // the common construction code independent of quick-xml event ownership.
                    let position = reader.buffer_position() as usize;
                    let empty = source[..position].ends_with("/>");
                    if stack.is_empty() {
                        roots += 1;
                        if roots != 1 {
                            return Err(DocumentExtractionError::InvalidXml);
                        }
                    }
                    if stack.len() >= MAX_XML_DEPTH {
                        return Err(DocumentExtractionError::XmlComplexityLimit);
                    }
                    let index = nodes.len();
                    nodes.push(node_from_tag(&tag, &reader, index + 1)?);
                    if !empty {
                        stack.push(index);
                    }
                }
                Event::End(tag) => {
                    let index = stack.pop().ok_or(DocumentExtractionError::InvalidXml)?;
                    if nodes[index].name.as_bytes() != tag.name().as_ref() {
                        return Err(DocumentExtractionError::InvalidXml);
                    }
                    nodes[index].end = nodes.len();
                }
                Event::Text(text) => {
                    let text = text
                        .decode()
                        .map_err(|_| DocumentExtractionError::InvalidXml)?;
                    if text.contains("]]>") {
                        return Err(DocumentExtractionError::InvalidXml);
                    }
                    push_text(&mut nodes, &stack, &text)?;
                }
                Event::CData(text) => {
                    if stack.is_empty() {
                        return Err(DocumentExtractionError::InvalidXml);
                    }
                    let text = text
                        .decode()
                        .map_err(|_| DocumentExtractionError::InvalidXml)?;
                    push_text(&mut nodes, &stack, &text)?;
                }
                Event::GeneralRef(reference) => {
                    if stack.is_empty() {
                        return Err(DocumentExtractionError::InvalidXml);
                    }
                    let character = reference
                        .resolve_char_ref()
                        .map_err(|_| DocumentExtractionError::InvalidXml)?;
                    let text = if let Some(character) = character {
                        if !legal_character(character) {
                            return Err(DocumentExtractionError::InvalidXml);
                        }
                        character.to_string()
                    } else {
                        let name = reference
                            .decode()
                            .map_err(|_| DocumentExtractionError::InvalidXml)?;
                        match name.as_ref() {
                            "lt" => "<",
                            "gt" => ">",
                            "amp" => "&",
                            "apos" => "'",
                            "quot" => "\"",
                            _ => return Err(DocumentExtractionError::InvalidXml),
                        }
                        .to_owned()
                    };
                    push_text(&mut nodes, &stack, &text)?;
                }
                Event::DocType(_) => return Err(DocumentExtractionError::InvalidXml),
                Event::Decl(declaration) => {
                    if before != 0 || roots != 0 || !nodes.is_empty() || declared {
                        return Err(DocumentExtractionError::InvalidXml);
                    }
                    declared = true;
                    let version = declaration
                        .version()
                        .map_err(|_| DocumentExtractionError::InvalidXml)?;
                    if version.as_ref() != b"1.0" {
                        return Err(DocumentExtractionError::InvalidXml);
                    }
                    if let Some(encoding) = declaration.encoding() {
                        let encoding = encoding.map_err(|_| DocumentExtractionError::InvalidXml)?;
                        if !encoding.eq_ignore_ascii_case(b"utf-8") {
                            return Err(DocumentExtractionError::InvalidXml);
                        }
                    }
                }
                Event::Eof => break,
                Event::Comment(_) | Event::PI(_) => {}
            }
            if nodes.len() > MAX_XML_NODES {
                return Err(DocumentExtractionError::XmlComplexityLimit);
            }
        }
        if roots != 1 || !stack.is_empty() {
            return Err(DocumentExtractionError::InvalidXml);
        }
        Ok(Self { nodes })
    }

    /// Outermost matching descendants, without selecting a matched node twice through nesting.
    pub(super) fn blocks<'xml>(
        &'xml self,
        parent: Option<usize>,
        names: &'xml [&str],
    ) -> impl Iterator<Item = usize> + 'xml {
        let (mut next, end) = parent.map_or((0, self.nodes.len()), |index| {
            (index + 1, self.nodes[index].end)
        });
        std::iter::from_fn(move || {
            while next < end {
                let index = next;
                next += 1;
                if names.contains(&self.nodes[index].name.as_str()) {
                    next = self.nodes[index].end;
                    return Some(index);
                }
            }
            None
        })
    }

    pub(super) fn text(&self, index: usize) -> String {
        let mut result = String::new();
        for node in &self.nodes[index + 1..self.nodes[index].end] {
            result.push_str(&node.text);
        }
        result
    }
}

fn node_from_tag(
    tag: &BytesStart<'_>,
    reader: &Reader<&[u8]>,
    end: usize,
) -> Result<Node, DocumentExtractionError> {
    let name = std::str::from_utf8(tag.name().as_ref())
        .map_err(|_| DocumentExtractionError::InvalidXml)?
        .to_owned();
    if !legal_name(&name) {
        return Err(DocumentExtractionError::InvalidXml);
    }
    let mut attributes = Vec::new();
    for attribute in tag.attributes() {
        if attributes.len() >= MAX_ATTRIBUTES {
            return Err(DocumentExtractionError::XmlComplexityLimit);
        }
        let attribute = attribute.map_err(|_| DocumentExtractionError::InvalidXml)?;
        if attribute.value.contains(&b'<') {
            return Err(DocumentExtractionError::InvalidXml);
        }
        let name = std::str::from_utf8(attribute.key.as_ref())
            .map_err(|_| DocumentExtractionError::InvalidXml)?
            .to_owned();
        if !legal_name(&name) {
            return Err(DocumentExtractionError::InvalidXml);
        }
        let value = attribute
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
            .map_err(|_| DocumentExtractionError::InvalidXml)?
            .into_owned();
        if value.chars().any(|character| !legal_character(character)) {
            return Err(DocumentExtractionError::InvalidXml);
        }
        attributes.push((name, value));
    }
    Ok(Node {
        name,
        text: String::new(),
        attributes,
        end,
    })
}

fn push_text(
    nodes: &mut Vec<Node>,
    stack: &[usize],
    text: &str,
) -> Result<(), DocumentExtractionError> {
    if stack.is_empty() {
        if !text.trim_matches([' ', '\t', '\r', '\n']).is_empty() {
            return Err(DocumentExtractionError::InvalidXml);
        }
        return Ok(());
    }
    if !text.is_empty() {
        nodes.push(Node {
            name: String::new(),
            text: text.to_owned(),
            attributes: Vec::new(),
            end: nodes.len() + 1,
        });
    }
    Ok(())
}

fn legal_character(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\r' | '\u{0020}'..='\u{d7ff}'
        | '\u{e000}'..='\u{fffd}' | '\u{10000}'..='\u{10ffff}')
}

fn legal_name(name: &str) -> bool {
    let mut characters = name.chars();
    let Some(first) = characters.next() else {
        return false;
    };
    name_start(first)
        && characters.all(|character| {
            name_start(character)
                || matches!(character, '-' | '.' | '0'..='9' | '\u{00b7}' | '\u{0300}'..='\u{036f}'
            | '\u{203f}'..='\u{2040}')
        })
}

fn name_start(character: char) -> bool {
    matches!(character, ':' | '_' | 'A'..='Z' | 'a'..='z' | '\u{00c0}'..='\u{00d6}'
        | '\u{00d8}'..='\u{00f6}' | '\u{00f8}'..='\u{02ff}' | '\u{0370}'..='\u{037d}'
        | '\u{037f}'..='\u{1fff}' | '\u{200c}'..='\u{200d}' | '\u{2070}'..='\u{218f}'
        | '\u{2c00}'..='\u{2fef}' | '\u{3001}'..='\u{d7ff}' | '\u{f900}'..='\u{fdcf}'
        | '\u{fdf0}'..='\u{fffd}' | '\u{10000}'..='\u{effff}')
}
