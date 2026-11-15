//! Font-style edition identity from `vscode/catalog.ts`.

pub(crate) fn catalog_base_name(name: &str) -> String {
    let lowercase = name.to_ascii_lowercase();
    let word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let mut output = String::new();
    let mut at = 0;
    while at < name.len() {
        let boundary = at == 0 || !name[..at].chars().next_back().is_some_and(word);
        let mut end = None;
        if boundary {
            for prefix in ["without", "non", "no", ""] {
                if !lowercase[at..].starts_with(prefix) {
                    continue;
                }
                let mut start = at + prefix.len();
                if !prefix.is_empty() {
                    for c in name[start..].chars() {
                        if c == '-' || super::whitespace(c) {
                            start += c.len_utf8();
                        } else {
                            break;
                        }
                    }
                }
                for style in ["italics", "italic", "bold"] {
                    let after = start + style.len();
                    if lowercase[start..].starts_with(style)
                        && !name[after..].chars().next().is_some_and(word)
                    {
                        end = Some(after);
                        break;
                    }
                }
                if end.is_some() {
                    break;
                }
            }
        }
        if let Some(end) = end {
            at = end;
        } else {
            let c = name[at..].chars().next().expect("character boundary");
            output.push(c);
            at += c.len_utf8();
        }
    }
    let mut cleaned = String::new();
    let mut chars = output.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '(' {
            let mut candidate = chars.clone();
            while candidate.peek().is_some_and(|c| super::whitespace(*c)) {
                candidate.next();
            }
            if candidate.next() == Some(')') {
                chars = candidate;
                continue;
            }
        }
        cleaned.push(c);
    }
    cleaned
        .split(super::whitespace)
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::catalog_base_name;

    #[test]
    fn canonical_font_edition_names_keep_the_same_identity() {
        for value in [
            "Night Owl",
            "Night Owl (No Italics)",
            "Night Owl Bold",
            "Night Owl non-italic",
            "Night  Owl WITHOUT-bold",
        ] {
            assert_eq!(catalog_base_name(value), "night owl");
        }
        assert_eq!(catalog_base_name("Boldly Lit"), "boldly lit");
        assert_eq!(catalog_base_name("Eva Dark Vivid"), "eva dark vivid");
        assert_eq!(catalog_base_name("Eva Dark Italics ( )"), "eva dark");
    }
}
