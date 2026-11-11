//! A finished `websearch` call's text result as a list of hits
//! (`parseWebSearchOutput` in `lib/opencode/websearch.ts`).
//!
//! OpenCode writes one `## [title](url)` block per hit, an optional
//! `Published: <ISO>` line, then the snippet. A result in any other shape is
//! not parsed, and the row prints it as text.
//!
//! Every hit is drawn as a link, so a result is hits only when each address
//! is one the client opens: an address with credentials, a backslash or a
//! control character is refused when clicked, and a result holding one stays
//! text.

use crate::tool_names::normalize;

/// One hit of a web search.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebSearchResult {
    pub url: String,
    /// The address's host without a leading `www.`.
    pub host: String,
    /// `None` when OpenCode had no title and repeated the URL instead.
    pub title: Option<String>,
    /// The publish time as the result wrote it, an ISO timestamp.
    pub published: Option<String>,
    pub snippet: Option<String>,
}

/// What a finished search found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebSearchOutput {
    Results(Vec<WebSearchResult>),
    /// OpenCode's "No search results found." answer.
    Empty,
}

const NO_RESULTS_PREFIX: &str = "No search results found.";
const PUBLISHED_PREFIX: &str = "Published: ";

/// Whether `tool` is OpenCode's web search.
#[must_use]
pub fn is_web_search(tool: &str) -> bool {
    normalize(tool) == "websearch"
}

/// The hits of a search result; `None` for text in another shape.
#[must_use]
pub fn parse(content: &str) -> Option<WebSearchOutput> {
    let text = content.trim();
    if text.starts_with(NO_RESULTS_PREFIX) {
        return Some(WebSearchOutput::Empty);
    }
    let mut results = Vec::new();
    let mut current: Option<Block<'_>> = None;
    for line in text.split('\n') {
        if let Some((title, url)) = heading(line) {
            let host = host_of(url)?;
            if let Some(block) = current.take() {
                results.push(block.finish());
            }
            current = Some(Block {
                url,
                host,
                title: (!title.is_empty() && title != url).then_some(title),
                published: None,
                body: Vec::new(),
            });
            continue;
        }
        // Text before the first heading is not a search result.
        let block = current.as_mut()?;
        // The date line comes right after the heading, before the blank line.
        if block.body.is_empty()
            && block.published.is_none()
            && let Some(published) = published(line)
        {
            block.published = Some(published);
            continue;
        }
        block.body.push(line);
    }
    results.push(current?.finish());
    Some(WebSearchOutput::Results(results))
}

struct Block<'a> {
    url: &'a str,
    host: String,
    title: Option<&'a str>,
    published: Option<&'a str>,
    body: Vec<&'a str>,
}

impl Block<'_> {
    fn finish(self) -> WebSearchResult {
        let snippet = self.body.join("\n");
        let snippet = snippet.trim();
        WebSearchResult {
            url: self.url.to_owned(),
            host: self.host,
            title: self.title.map(str::to_owned),
            published: self.published.map(str::to_owned),
            snippet: (!snippet.is_empty()).then(|| snippet.to_owned()),
        }
    }
}

/// The title and address of a `## [title](url)` line. The title runs to the
/// last `](` that an HTTP(S) address without whitespace follows.
fn heading(line: &str) -> Option<(&str, &str)> {
    let inner = line.strip_prefix("## [")?.strip_suffix(')')?;
    let mut end = inner.len();
    while let Some(split) = inner[..end].rfind("](") {
        let url = &inner[split + 2..];
        let address = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"));
        if address.is_some_and(|address| !address.is_empty())
            && !url.chars().any(char::is_whitespace)
        {
            return Some((&inner[..split], url));
        }
        end = split;
    }
    None
}

/// The timestamp of a `Published: <timestamp>` line.
fn published(line: &str) -> Option<&str> {
    let value = line.strip_prefix(PUBLISHED_PREFIX)?;
    (!value.is_empty() && !value.chars().any(char::is_whitespace)).then_some(value)
}

/// The host of an HTTP(S) address the client opens, lowercased, without its
/// port or a leading `www.`.
///
/// `None` when the address names no host, and for an address link routing
/// denies: one with a user name or password before the host, a backslash,
/// or a control character.
fn host_of(url: &str) -> Option<String> {
    if url.contains('\\') || url.chars().any(char::is_control) {
        return None;
    }
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if host.contains('@') {
        return None;
    }
    let host = if host.starts_with('[') {
        // An IPv6 literal keeps its brackets, as `URL.hostname` does.
        &host[..=host.find(']')?]
    } else {
        host.split_once(':').map_or(host, |(name, _)| name)
    };
    let host = host.to_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    (!host.is_empty()).then(|| host.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn results(output: Option<WebSearchOutput>) -> Vec<WebSearchResult> {
        match output {
            Some(WebSearchOutput::Results(results)) => results,
            other => panic!("expected search results, got {other:?}"),
        }
    }

    #[test]
    fn reads_each_hit_with_its_date_and_snippet() {
        let parsed = results(parse(
            "## [Rust Book](https://www.rust-lang.org/book)\nPublished: 2026-01-05T10:00:00.000Z\n\nLearn Rust.\nSecond line.\n\n## [https://example.org/a](https://example.org/a)\n\n## [Bare [docs] (v2)](http://Docs.Example.org:8080/x?y=(1))\nPublished: later text\n",
        ));
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].title.as_deref(), Some("Rust Book"));
        assert_eq!(parsed[0].host, "rust-lang.org");
        assert_eq!(
            parsed[0].published.as_deref(),
            Some("2026-01-05T10:00:00.000Z")
        );
        assert_eq!(
            parsed[0].snippet.as_deref(),
            Some("Learn Rust.\nSecond line.")
        );
        assert_eq!(parsed[1].title, None, "a repeated URL is no title");
        assert_eq!(parsed[1].published, None);
        assert_eq!(parsed[1].snippet, None);
        assert_eq!(parsed[2].title.as_deref(), Some("Bare [docs] (v2)"));
        assert_eq!(parsed[2].url, "http://Docs.Example.org:8080/x?y=(1)");
        assert_eq!(parsed[2].host, "docs.example.org");
        assert_eq!(
            parsed[2].snippet.as_deref(),
            Some("Published: later text"),
            "a date line with spaces is part of the snippet"
        );
    }

    #[test]
    fn the_date_line_only_follows_the_heading() {
        let parsed = results(parse(
            "## [A](https://a.example)\n\nPublished: 2026-01-05\nPublished: 2026-01-06",
        ));
        assert_eq!(parsed[0].published, None);
        assert_eq!(
            parsed[0].snippet.as_deref(),
            Some("Published: 2026-01-05\nPublished: 2026-01-06")
        );
    }

    #[test]
    fn an_empty_search_is_not_a_parse_failure() {
        assert_eq!(
            parse("  No search results found. Try another query.\n"),
            Some(WebSearchOutput::Empty)
        );
    }

    #[test]
    fn other_shapes_stay_text() {
        assert_eq!(parse(""), None);
        assert_eq!(parse("Search failed: rate limited"), None);
        assert_eq!(
            parse("Intro\n## [A](https://a.example)"),
            None,
            "text before the first heading"
        );
        assert_eq!(
            parse("## [A](ftp://a.example)"),
            None,
            "only web addresses are hits"
        );
        assert_eq!(
            parse("## [A](https://a.example)\n\n## [B](https://)"),
            Some(WebSearchOutput::Results(vec![WebSearchResult {
                url: "https://a.example".to_owned(),
                host: "a.example".to_owned(),
                title: Some("A".to_owned()),
                published: None,
                snippet: Some("## [B](https://)".to_owned()),
            }])),
            "a heading without an address is snippet text"
        );
        assert_eq!(
            parse("## [A](https://a.example)\n## [B](https://:80/)"),
            None,
            "one hit without a host leaves the whole result as text"
        );
    }

    #[test]
    fn a_hit_the_client_would_not_open_leaves_the_result_as_text() {
        let good = "## [A](https://a.example/)\n\nFine.";
        for denied in [
            "https://user:pw@b.example/",
            "https://user@b.example/",
            "https://b.example\\@c.example/",
            "https://b.example/a\\b",
            "https://b.example/\u{7}",
        ] {
            assert_eq!(host_of(denied), None, "{denied:?}");
            assert_eq!(
                parse(&format!("{good}\n\n## [B]({denied})\n\nText.")),
                None,
                "{denied:?} is not drawn as a link beside the good hit"
            );
            assert_eq!(parse(&format!("## [B]({denied})")), None, "{denied:?}");
        }
        assert_eq!(
            host_of("https://b.example/profile/@name?mail=a@b.example").as_deref(),
            Some("b.example"),
            "an @ after the host is part of the page's address"
        );
        assert!(parse(good).is_some());
    }

    #[test]
    fn hosts_drop_ports_and_www() {
        assert_eq!(
            host_of("https://WWW.Example.com:8443/p").as_deref(),
            Some("example.com")
        );
        assert_eq!(host_of("http://[::1]:3000/").as_deref(), Some("[::1]"));
        assert_eq!(host_of("https://?q"), None);
        assert!(is_web_search("WebSearch"));
        assert!(!is_web_search("webfetch"));
    }
}
