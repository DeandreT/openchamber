//! The six shared document projections. Formatting preserves source positions and stored values.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::BTreeMap;

use super::archive::{self, Archive};
use super::xml::{Node, Xml};
use super::{DocumentExtractionError, EmbeddedImages, TextOutput};

struct Relationship {
    target: Option<String>,
    kind: String,
    order: usize,
    image_citation: OnceCell<String>,
}

impl Relationship {
    fn citation(&self, images: &mut EmbeddedImages<'_>) -> Result<&str, DocumentExtractionError> {
        if self.image_citation.get().is_none() {
            let citation = images.citation(self.target.as_deref())?;
            let _ = self.image_citation.set(citation);
        }
        Ok(self
            .image_citation
            .get()
            .expect("relationship citation was initialized"))
    }
}

type Relationships = BTreeMap<String, Relationship>;

pub(super) fn extract(
    extension: &str,
    archive: &Archive,
    images: &mut EmbeddedImages<'_>,
    output: &mut TextOutput,
) -> Result<(), DocumentExtractionError> {
    match extension {
        "docx" => docx(archive, images, output),
        "pptx" => pptx(archive, images, output),
        "xlsx" => xlsx(archive, images, output),
        "odt" | "odp" | "ods" => odf(extension, archive, images, output),
        _ => Err(DocumentExtractionError::UnsupportedFormat),
    }
}

fn relationships(
    archive: &Archive,
    source: &str,
) -> Result<Relationships, DocumentExtractionError> {
    let path = archive::relationships_path(source);
    if archive.bytes(&path).is_none() {
        return Ok(BTreeMap::new());
    }
    let xml = archive.xml(&path)?;
    require_root(&xml, &["Relationships"])?;
    let mut result = BTreeMap::new();
    for (order, index) in xml.blocks(None, &["Relationship"]).enumerate() {
        let node = &xml.nodes[index];
        let (Some(id), Some(target)) = (node.attribute("Id"), node.attribute("Target")) else {
            continue;
        };
        let target = if node
            .attribute("TargetMode")
            .is_some_and(|mode| mode.eq_ignore_ascii_case("External"))
        {
            None
        } else {
            archive::resolve_path(source, target)
        };
        result.insert(
            id.to_owned(),
            Relationship {
                target,
                kind: node.attribute("Type").unwrap_or_default().to_owned(),
                order,
                image_citation: OnceCell::new(),
            },
        );
    }
    Ok(result)
}

fn target<'relations>(
    relationships: &'relations Relationships,
    id: Option<&str>,
) -> Option<&'relations str> {
    relationships.get(id?)?.target.as_deref()
}

fn require_root(xml: &Xml, names: &[&str]) -> Result<(), DocumentExtractionError> {
    if xml
        .nodes
        .first()
        .is_some_and(|node| names.contains(&node.name.as_str()))
    {
        Ok(())
    } else {
        Err(DocumentExtractionError::InvalidDocumentStructure)
    }
}

fn inline(
    xml: &Xml,
    index: usize,
    source: &str,
    relationships: &Relationships,
    images: &mut EmbeddedImages<'_>,
    odf: bool,
) -> Result<String, DocumentExtractionError> {
    let mut text = String::new();
    let mut next = index + 1;
    while next < xml.nodes[index].end {
        let node = &xml.nodes[next];
        match node.name.as_str() {
            "w:t" | "a:t" | "text:span" if !odf => {
                text.push_str(&xml.text(next));
                next = node.end;
                continue;
            }
            "w:tab" | "text:tab" => text.push('\t'),
            "w:br" | "a:br" | "text:line-break" => text.push('\n'),
            "text:s" if odf => text.push_str(&odf_spaces(node)),
            "a:blip" | "v:imagedata" | "draw:image" => {
                text.push('\n');
                if let Some(direct) = node.attribute("xlink:href") {
                    let path = archive::resolve_path(source, direct);
                    text.push_str(&images.citation(path.as_deref())?);
                } else {
                    let relationship = node
                        .attribute("r:embed")
                        .or_else(|| node.attribute("r:id"))
                        .and_then(|id| relationships.get(id));
                    if let Some(relationship) = relationship {
                        text.push_str(relationship.citation(images)?);
                    } else {
                        text.push_str(&images.citation(None)?);
                    }
                }
                text.push('\n');
                next = node.end;
                continue;
            }
            "" if odf => text.push_str(&node.text),
            _ => {}
        }
        next += 1;
    }
    Ok(clean_inline(&text))
}

fn clean_inline(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for piece in text.split_inclusive('\n') {
        if let Some(line) = piece.strip_suffix('\n') {
            output.push_str(line.trim_end_matches([' ', '\t']));
            output.push('\n');
        } else {
            output.push_str(piece);
        }
    }
    output.trim_matches(js_whitespace).to_owned()
}

fn js_whitespace(character: char) -> bool {
    matches!(character, '\u{0009}'..='\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}'
        | '\u{2000}'..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}'
        | '\u{3000}' | '\u{feff}')
}

fn append_paragraphs(
    archive: &Archive,
    path: &str,
    names: &[&str],
    heading: &str,
    images: &mut EmbeddedImages<'_>,
    output: &mut TextOutput,
) -> Result<(), DocumentExtractionError> {
    if output.full() {
        return Ok(());
    }
    let xml = archive.xml(path)?;
    let relationships = relationships(archive, path)?;
    let mut headed = false;
    for index in xml.blocks(None, names) {
        if output.full() {
            break;
        }
        let text = inline(&xml, index, path, &relationships, images, false)?;
        if !text.is_empty() {
            if !headed {
                output.section(heading);
                headed = true;
            }
            output.section(&text);
        }
    }
    Ok(())
}

fn numbered_part(path: &str, prefix: &str) -> Option<String> {
    let path = path.to_ascii_lowercase();
    let digits = path.strip_prefix(prefix)?.strip_suffix(".xml")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(digits.trim_start_matches('0').to_owned())
}

fn docx(
    archive: &Archive,
    images: &mut EmbeddedImages<'_>,
    output: &mut TextOutput,
) -> Result<(), DocumentExtractionError> {
    let main = archive.xml("word/document.xml")?;
    require_root(&main, &["w:document"])?;
    let relationships = relationships(archive, "word/document.xml")?;
    output.section("# Document");
    for index in main.blocks(None, &["w:p"]) {
        if output.full() {
            break;
        }
        let text = inline(
            &main,
            index,
            "word/document.xml",
            &relationships,
            images,
            false,
        )?;
        if !text.is_empty() {
            output.section(&text);
        }
    }
    // BTreeMap path order matches the shared lexicographic header/footer order.
    for path in archive.paths() {
        if output.full() {
            break;
        }
        let header = numbered_part(path, "word/header").is_some();
        if !header && numbered_part(path, "word/footer").is_none() {
            continue;
        }
        append_paragraphs(
            archive,
            path,
            &["w:p"],
            if header { "## Header" } else { "## Footer" },
            images,
            output,
        )?;
    }
    Ok(())
}

fn pptx(
    archive: &Archive,
    images: &mut EmbeddedImages<'_>,
    output: &mut TextOutput,
) -> Result<(), DocumentExtractionError> {
    let mut paths: Vec<(&str, String)> = archive
        .paths()
        .filter_map(|path| numbered_part(path, "ppt/slides/slide").map(|number| (path, number)))
        .collect();
    paths.sort_by(|left, right| left.1.len().cmp(&right.1.len()).then(left.1.cmp(&right.1)));
    if paths.is_empty() {
        return Err(DocumentExtractionError::MissingDocumentPart);
    }
    output.section("# Presentation");
    for (number, (path, _)) in paths.iter().enumerate() {
        if output.full() {
            break;
        }
        let xml = archive.xml(path)?;
        require_root(&xml, &["p:sld"])?;
        let relationships = relationships(archive, path)?;
        output.section(&format!("## Slide {}", number + 1));
        let mut has_content = false;
        for index in xml.blocks(None, &["a:p", "p:pic"]) {
            if output.full() {
                break;
            }
            let text = inline(&xml, index, path, &relationships, images, false)?;
            if !text.is_empty() {
                output.section(&text);
                has_content = true;
            }
        }
        if !has_content {
            output.section("[Empty slide]");
        }
        let notes = relationships
            .values()
            .filter(|relationship| relationship.kind.ends_with("/notesSlide"))
            .min_by_key(|relationship| relationship.order)
            .and_then(|relationship| relationship.target.as_deref());
        if let Some(path) = notes {
            append_paragraphs(
                archive,
                path,
                &["a:p"],
                &format!("### Slide {} notes", number + 1),
                images,
                output,
            )?;
        }
    }
    Ok(())
}

struct Cell<'strings> {
    reference: String,
    column: u64,
    row: u64,
    value: Cow<'strings, str>,
}

struct Row<'strings> {
    cells: Vec<Cell<'strings>>,
    first: u64,
    last: u64,
    row: u64,
}

fn coordinates(reference: &str) -> Option<(u64, u64)> {
    let split = reference.find(|character: char| !character.is_ascii_alphabetic())?;
    let (letters, digits) = reference.split_at(split);
    if letters.is_empty()
        || digits.starts_with('0')
        || !digits.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let mut column = 0_u64;
    for byte in letters.bytes() {
        column = column
            .checked_mul(26)?
            .checked_add(u64::from(byte.to_ascii_uppercase() - b'A' + 1))?;
    }
    let row: u64 = digits.parse().ok()?;
    if row == 0 {
        return None;
    }
    Some((column.checked_sub(1)?, row))
}

fn column_name(column: u64) -> String {
    let mut value = column;
    let mut reversed = Vec::new();
    loop {
        reversed.push(char::from(b'A' + (value % 26) as u8));
        if value < 26 {
            break;
        }
        value = value / 26 - 1;
    }
    reversed.iter().rev().collect()
}

fn tsv(value: &str) -> String {
    if value.contains(['\t', '\r', '\n', '"']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

fn cell_value<'strings>(
    xml: &Xml,
    index: usize,
    strings: &'strings [String],
) -> Result<Cow<'strings, str>, DocumentExtractionError> {
    let kind = xml.nodes[index].attribute("t");
    if kind == Some("inlineStr") {
        let mut text = String::new();
        for index in xml.blocks(Some(index), &["t"]) {
            text.push_str(&xml.text(index));
        }
        return Ok(Cow::Owned(text));
    }
    let value = xml
        .blocks(Some(index), &["v"])
        .next()
        .map(|index| xml.text(index))
        .unwrap_or_default();
    if value.is_empty() {
        return Ok(Cow::Borrowed(""));
    }
    Ok(match kind {
        Some("s") => Cow::Borrowed(
            value
                .trim()
                .parse::<usize>()
                .ok()
                .and_then(|index| strings.get(index))
                .map(String::as_str)
                .ok_or(DocumentExtractionError::InvalidDocumentStructure)?,
        ),
        Some("b") => Cow::Borrowed(match value.as_str() {
            "1" => "TRUE",
            "0" => "FALSE",
            _ => return Err(DocumentExtractionError::InvalidDocumentStructure),
        }),
        _ => Cow::Owned(value),
    })
}

fn rows<'strings>(
    xml: &Xml,
    strings: &'strings [String],
) -> Result<Vec<Row<'strings>>, DocumentExtractionError> {
    let mut rows = Vec::new();
    for row in xml.blocks(None, &["row"]) {
        let mut cells = Vec::new();
        for index in xml.blocks(Some(row), &["c"]) {
            let value = cell_value(xml, index, strings)?;
            if value.is_empty() {
                continue;
            }
            let reference = xml.nodes[index]
                .attribute("r")
                .ok_or(DocumentExtractionError::InvalidDocumentStructure)?;
            let (column, row) =
                coordinates(reference).ok_or(DocumentExtractionError::InvalidDocumentStructure)?;
            cells.push(Cell {
                reference: reference.to_owned(),
                column,
                row,
                value,
            });
        }
        cells.sort_by_key(|cell| cell.column);
        if let (Some(first), Some(last)) = (cells.first(), cells.last()) {
            if cells.iter().any(|cell| cell.row != first.row) {
                return Err(DocumentExtractionError::InvalidDocumentStructure);
            }
            rows.push(Row {
                first: first.column,
                last: last.column,
                row: first.row,
                cells,
            });
        }
    }
    Ok(rows)
}

fn dense(row: &Row<'_>) -> bool {
    row.last - row.first < (row.cells.len() as u64 * 4).max(32)
}

fn dense_block(rows: &[Row<'_>], output: &mut TextOutput) {
    if output.full() {
        return;
    }
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        return;
    };
    output.section(&format!(
        "Range: {}{}:{}{}",
        column_name(first.first),
        first.row,
        column_name(first.last),
        last.row
    ));
    for row in rows {
        output.push("\n");
        // Last duplicate cell wins, matching the shared Map used to serialize dense rows.
        if output.full() {
            return;
        }
        let values: BTreeMap<u64, &str> = row
            .cells
            .iter()
            .map(|cell| (cell.column, cell.value.as_ref()))
            .collect();
        for column in row.first..=row.last {
            if output.full() {
                return;
            }
            if column != row.first {
                output.push("\t");
            }
            output.push(&tsv(values.get(&column).copied().unwrap_or_default()));
        }
    }
}

fn spreadsheet_rows(rows: &[Row<'_>], output: &mut TextOutput) {
    let mut start = 0;
    for (index, row) in rows.iter().enumerate() {
        if output.full() {
            return;
        }
        if !dense(row) {
            dense_block(&rows[start..index], output);
            output.section("Cells: ");
            for (index, cell) in row.cells.iter().enumerate() {
                if output.full() {
                    return;
                }
                if index != 0 {
                    output.push(" | ");
                }
                output.push(&format!("{}\t{}", cell.reference, tsv(&cell.value)));
            }
            start = index + 1;
        } else if index > start {
            let previous = &rows[index - 1];
            if previous.row.checked_add(1) != Some(row.row)
                || row.first != previous.first
                || row.last != previous.last
            {
                dense_block(&rows[start..index], output);
                start = index;
            }
        }
    }
    dense_block(&rows[start..], output);
}

fn drawing_citations(
    archive: &Archive,
    sheet: &Xml,
    path: &str,
    images: &mut EmbeddedImages<'_>,
    output: &mut TextOutput,
) -> Result<(), DocumentExtractionError> {
    if output.full() {
        return Ok(());
    }
    let relationships = relationships(archive, path)?;
    for index in sheet.blocks(None, &["drawing"]) {
        if output.full() {
            break;
        }
        let path = target(&relationships, sheet.nodes[index].attribute("r:id"))
            .ok_or(DocumentExtractionError::InvalidDocumentStructure)?;
        let drawing = archive.xml(path)?;
        let relationships = self::relationships(archive, path)?;
        for anchor in drawing.blocks(None, &["xdr:oneCellAnchor", "xdr:twoCellAnchor"]) {
            if output.full() {
                break;
            }
            // Coordinates are mandatory here. Missing/invalid coordinates never invent A1.
            let from = drawing
                .blocks(Some(anchor), &["xdr:from"])
                .next()
                .ok_or(DocumentExtractionError::InvalidDocumentStructure)?;
            let coordinate = |name: &str| -> Result<u64, DocumentExtractionError> {
                let names = [name];
                let index = drawing
                    .blocks(Some(from), &names)
                    .next()
                    .ok_or(DocumentExtractionError::InvalidDocumentStructure)?;
                drawing
                    .text(index)
                    .parse()
                    .map_err(|_| DocumentExtractionError::InvalidDocumentStructure)
            };
            let column = coordinate("xdr:col")?;
            let row = coordinate("xdr:row")?
                .checked_add(1)
                .ok_or(DocumentExtractionError::InvalidDocumentStructure)?;
            let image = drawing.blocks(Some(anchor), &["a:blip"]).next();
            let id = image.and_then(|index| drawing.nodes[index].attribute("r:embed"));
            let citation = if let Some(relationship) = id.and_then(|id| relationships.get(id)) {
                relationship.citation(images)?
            } else {
                "[Embedded image reference could not be resolved]"
            };
            output.section(&format!(
                "Image at {}{row}: {citation}",
                column_name(column)
            ));
        }
    }
    Ok(())
}

fn xlsx(
    archive: &Archive,
    images: &mut EmbeddedImages<'_>,
    output: &mut TextOutput,
) -> Result<(), DocumentExtractionError> {
    let path = "xl/workbook.xml";
    let workbook = archive.xml(path)?;
    require_root(&workbook, &["workbook"])?;
    let relationships = relationships(archive, path)?;
    let mut strings = Vec::new();
    if archive.bytes("xl/sharedStrings.xml").is_some() {
        let xml = archive.xml("xl/sharedStrings.xml")?;
        for item in xml.blocks(None, &["si"]) {
            let mut text = String::new();
            for index in xml.blocks(Some(item), &["t"]) {
                text.push_str(&xml.text(index));
            }
            strings.push(text);
        }
    }
    output.section("# Workbook");
    for index in workbook.blocks(None, &["sheet"]) {
        if output.full() {
            break;
        }
        let node = &workbook.nodes[index];
        let relationship = node
            .attribute("r:id")
            .and_then(|id| relationships.get(id))
            .ok_or(DocumentExtractionError::InvalidDocumentStructure)?;
        let path = relationship
            .target
            .as_deref()
            .ok_or(DocumentExtractionError::InvalidDocumentStructure)?;
        let sheet = archive.xml(path)?;
        output.section(&format!(
            "## Sheet: {}",
            node.attribute("name").unwrap_or("Sheet")
        ));
        // Both the Transitional and Strict OOXML relationship namespaces identify the part kind.
        match relationship.kind.as_str() {
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/chartsheet"
            | "http://purl.oclc.org/ooxml/officeDocument/relationships/chartsheet" => {
                require_root(&sheet, &["chartsheet"])?;
                output.section("[Chart sheet content omitted by OpenChamber]");
                continue;
            }
            "http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet"
            | "http://purl.oclc.org/ooxml/officeDocument/relationships/worksheet" => {
                require_root(&sheet, &["worksheet"])?;
            }
            _ => return Err(DocumentExtractionError::InvalidDocumentStructure),
        }
        let rows = rows(&sheet, &strings)?;
        if rows.is_empty() {
            output.section("[Empty sheet]");
        }
        spreadsheet_rows(&rows, output);
        drawing_citations(archive, &sheet, path, images, output)?;
    }
    Ok(())
}

fn odf_spaces(node: &Node) -> String {
    let Some(raw) = node.attribute("text:c") else {
        return " ".to_owned();
    };
    let count: Option<u64> = raw
        .parse()
        .ok()
        .filter(|count| *count <= 9_007_199_254_740_991);
    if let Some(count) = count.filter(|count| *count > 0 && *count <= 100) {
        return " ".repeat(count as usize);
    }
    let notice = count.filter(|count| *count > 100).map_or_else(
        || "Additional spaces omitted".to_owned(),
        |count| format!("{} additional spaces omitted", count - 100),
    );
    format!("{}[{notice}]", " ".repeat(100))
}

fn odf_content<'projection>(
    xml: &'projection Xml,
    parent: Option<usize>,
    images: &'projection mut EmbeddedImages<'_>,
) -> impl Iterator<Item = Result<String, DocumentExtractionError>> + 'projection {
    xml.blocks(parent, &["text:p", "text:h", "draw:image"])
        .map(move |index| {
            let node = &xml.nodes[index];
            if node.name == "draw:image" {
                let path = node
                    .attribute("xlink:href")
                    .and_then(|path| archive::resolve_path("content.xml", path));
                images.citation(path.as_deref())
            } else {
                inline(xml, index, "content.xml", &BTreeMap::new(), images, true)
            }
        })
        .filter(|text| !matches!(text, Ok(text) if text.is_empty()))
}

fn odf(
    extension: &str,
    archive: &Archive,
    images: &mut EmbeddedImages<'_>,
    output: &mut TextOutput,
) -> Result<(), DocumentExtractionError> {
    let xml = archive.xml("content.xml")?;
    require_root(&xml, &["office:document", "office:document-content"])?;
    match extension {
        "odt" => {
            output.section("# Document");
            let mut content = odf_content(&xml, None, images);
            while !output.full() {
                let Some(text) = content.next() else {
                    break;
                };
                output.section(&text?);
            }
        }
        "odp" => {
            output.section("# Presentation");
            for (number, index) in xml.blocks(None, &["draw:page"]).enumerate() {
                if output.full() {
                    break;
                }
                let fallback = (number + 1).to_string();
                output.section(&format!(
                    "## Slide: {}",
                    xml.nodes[index].attribute("draw:name").unwrap_or(&fallback)
                ));
                let mut content = odf_content(&xml, Some(index), images);
                while !output.full() {
                    let Some(text) = content.next() else {
                        break;
                    };
                    output.section(&text?);
                }
            }
        }
        "ods" => {
            output.section("# Workbook");
            for table in xml.blocks(None, &["table:table"]) {
                if output.full() {
                    break;
                }
                output.section(&format!(
                    "## Sheet: {}",
                    xml.nodes[table].attribute("table:name").unwrap_or("Sheet")
                ));
                for shapes in xml.blocks(Some(table), &["table:shapes"]) {
                    if output.full() {
                        break;
                    }
                    let mut content = odf_content(&xml, Some(shapes), images);
                    while !output.full() {
                        let Some(text) = content.next() else {
                            break;
                        };
                        output.section(&text?);
                    }
                }
                for row in xml.blocks(Some(table), &["table:table-row"]) {
                    if output.full() {
                        break;
                    }
                    let mut row_text = TextOutput::default();
                    let mut has_value = false;
                    let mut first_cell = true;
                    // Repeated rows/cells are not expanded, as in the shared projection.
                    for cell in xml.blocks(Some(row), &["table:table-cell"]) {
                        if row_text.full() {
                            break;
                        }
                        if !first_cell {
                            row_text.push(" | ");
                        }
                        first_cell = false;
                        let mut first = true;
                        let mut content = odf_content(&xml, Some(cell), images);
                        while !row_text.full() {
                            let Some(text) = content.next() else {
                                break;
                            };
                            let text = text?;
                            if !first {
                                row_text.push(" ");
                            }
                            first = false;
                            has_value = true;
                            row_text.push(&text);
                        }
                    }
                    // A row can exhaust its budget on positional separators before reaching its
                    // first value. Preserve that partial projection so the final notice is emitted.
                    if has_value || row_text.full() {
                        output.section(&row_text.text);
                    }
                }
            }
        }
        _ => return Err(DocumentExtractionError::UnsupportedFormat),
    }
    Ok(())
}
