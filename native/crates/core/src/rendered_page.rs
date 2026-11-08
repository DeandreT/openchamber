//! The page an `openchamber_render` call showed and where it stands on the
//! user's answer (`renderedPage` in `packages/ui/src/lib/opencode/tools.ts`).

use crate::transcript::ToolStatus;

/// The tool agents show pages with.
pub const RENDER_TOOL: &str = "openchamber_render";
const MAX_TITLE_LENGTH: usize = 120;

/// The page fields of a render call's input. The call may put them inside
/// `parameters` or beside `action`; the transport takes `parameters` first,
/// field by field, as `renderPageInputSchema` does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageInput {
    pub html: Option<String>,
    /// A page in the UI block language, drawn by the chat's own components.
    pub ui: Option<String>,
    /// Trimmed and non-empty.
    pub title: Option<String>,
    pub await_answer: Option<bool>,
}

/// What the user answered on a page that waited, as the server recorded it
/// in the call's metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PageAnswer {
    /// `data` is the answer's JSON text.
    Answered {
        summary: String,
        data: String,
    },
    NotAnswered {
        reason: Option<String>,
    },
}

/// The `openchamber` result in a render call's metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageResult {
    pub ok: Option<bool>,
    pub answer: Option<PageAnswer>,
}

/// Where a page stands on the user's answer (`RenderedPageAsk`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ask {
    Show,
    Waiting,
    /// `data` is the answer's JSON text.
    Answered {
        summary: String,
        data: String,
    },
    AnsweredInChat,
    /// The user had written in the chat before the page appeared.
    ChatSentEarlier,
    /// The chat could not draw the page's UI blocks.
    NotDrawn,
    Unanswered,
}

/// A page the chat shows in place of the call's row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenderedPage {
    /// Empty for a UI block page.
    pub html: String,
    pub ui: Option<String>,
    /// Empty when neither the call nor the page names it; the chat shows its
    /// own word then.
    pub title: String,
    pub ask: Ask,
}

/// The page a call showed, once OpenChamber accepted the call. A page that
/// waits for an answer shows while its call runs and after a call that
/// failed while waiting; any other refused or unfinished call keeps its row.
pub fn rendered_page(
    tool: &str,
    status: ToolStatus,
    input: &PageInput,
    result: &PageResult,
) -> Option<RenderedPage> {
    if tool != RENDER_TOOL {
        return None;
    }
    let html = input.html.clone().unwrap_or_default();
    let ui = input.ui.clone().filter(|ui| !ui.trim().is_empty());
    if ui.is_none() && html.trim().is_empty() {
        return None;
    }
    let title = input
        .title
        .clone()
        .unwrap_or_else(|| title_from_html(&html));
    let page = |ask| {
        Some(RenderedPage {
            html: if ui.is_some() {
                String::new()
            } else {
                html.clone()
            },
            ui: ui.clone(),
            title: title.clone(),
            ask,
        })
    };
    if input.await_answer == Some(true) {
        match status {
            ToolStatus::Running => return page(Ask::Waiting),
            ToolStatus::Error => return page(Ask::Unanswered),
            _ => {}
        }
    }
    if status != ToolStatus::Completed || result.ok != Some(true) {
        return None;
    }
    match &result.answer {
        Some(PageAnswer::Answered { summary, data }) => page(Ask::Answered {
            summary: summary.clone(),
            data: data.clone(),
        }),
        Some(PageAnswer::NotAnswered { reason }) => page(match reason.as_deref() {
            Some("chat-sent-earlier") => Ask::ChatSentEarlier,
            Some("ui-not-drawn") => Ask::NotDrawn,
            _ => Ask::AnsweredInChat,
        }),
        None => page(Ask::Show),
    }
}

/// The byte range of the first `<name ...>...</close>` content, where `close`
/// is any of `closers`, matched without regard to ASCII case.
fn element_content<'a>(html: &'a str, name: &str, closers: &[&str]) -> Option<&'a str> {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{name}");
    let mut from = 0;
    while let Some(offset) = lower[from..].find(&open) {
        let start = from + offset;
        let after = start + open.len();
        // `\b`: the name must end here.
        let boundary = lower[after..]
            .chars()
            .next()
            .is_none_or(|next| !(next.is_ascii_alphanumeric() || next == '_'));
        if boundary && let Some(close) = lower[after..].find('>') {
            let content_start = after + close + 1;
            let end = closers
                .iter()
                .filter_map(|closer| lower[content_start..].find(&format!("</{closer}>")))
                .min()?;
            return Some(&html[content_start..content_start + end]);
        }
        from = after;
    }
    None
}

/// An element's text: tags become spaces, a few entities decode, and
/// whitespace collapses (`elementText`).
fn element_text(content: &str) -> String {
    let mut text = String::with_capacity(content.len());
    let mut in_tag = false;
    for character in content.chars() {
        match character {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(character),
            _ => {}
        }
    }
    if in_tag {
        // An unclosed `<` stays text, as `<[^>]*>` cannot match it.
        let cut = content.rfind('<').unwrap_or(content.len());
        text.push_str(&content[cut..]);
    }
    let decoded = [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&quot;", "\""),
        ("&#39;", "'"),
        ("&apos;", "'"),
        ("&nbsp;", " "),
    ];
    let mut output = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(index) = rest.find('&') {
        output.push_str(&rest[..index]);
        let tail = &rest[index..];
        match decoded.iter().find(|(entity, _)| tail.starts_with(entity)) {
            Some((entity, value)) => {
                output.push_str(value);
                rest = &tail[entity.len()..];
            }
            None => {
                output.push('&');
                rest = &tail[1..];
            }
        }
    }
    output.push_str(rest);
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A page's own name: its `<title>`, else its first h1 to h3, cut at 120
/// UTF-16 units (`pageTitleFromHtml`).
pub fn title_from_html(html: &str) -> String {
    let title = element_content(html, "title", &["title"])
        .map(element_text)
        .filter(|text| !text.is_empty())
        .or_else(|| {
            ["h1", "h2", "h3"]
                .iter()
                .filter_map(|name| {
                    let start = html.to_ascii_lowercase().find(&format!("<{name}"))?;
                    let content = element_content(&html[start..], name, &["h1", "h2", "h3"])?;
                    Some((start, element_text(content)))
                })
                .min_by_key(|(start, _)| *start)
                .map(|(_, text)| text)
        })
        .unwrap_or_default();
    let mut units = 0;
    let cut: String = title
        .chars()
        .take_while(|character| {
            units += character.len_utf16();
            units <= MAX_TITLE_LENGTH
        })
        .collect();
    cut.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(html: &str) -> PageInput {
        PageInput {
            html: Some(html.to_owned()),
            ..PageInput::default()
        }
    }

    fn accepted(answer: Option<PageAnswer>) -> PageResult {
        PageResult {
            ok: Some(true),
            answer,
        }
    }

    #[test]
    fn only_accepted_render_calls_show_pages() {
        let page = input("<p>x</p>");
        assert!(rendered_page("bash", ToolStatus::Completed, &page, &accepted(None)).is_none());
        assert!(rendered_page(RENDER_TOOL, ToolStatus::Running, &page, &accepted(None)).is_none());
        assert!(
            rendered_page(
                RENDER_TOOL,
                ToolStatus::Completed,
                &page,
                &PageResult::default()
            )
            .is_none()
        );
        assert!(
            rendered_page(
                RENDER_TOOL,
                ToolStatus::Completed,
                &input("  "),
                &accepted(None)
            )
            .is_none()
        );
        let shown = rendered_page(RENDER_TOOL, ToolStatus::Completed, &page, &accepted(None))
            .expect("shown");
        assert_eq!(shown.ask, Ask::Show);
    }

    #[test]
    fn waiting_pages_show_while_their_call_runs() {
        let mut page = input("<button>Pick</button>");
        page.await_answer = Some(true);
        let none = PageResult::default();
        assert_eq!(
            rendered_page(RENDER_TOOL, ToolStatus::Running, &page, &none).map(|page| page.ask),
            Some(Ask::Waiting)
        );
        assert_eq!(
            rendered_page(RENDER_TOOL, ToolStatus::Error, &page, &none).map(|page| page.ask),
            Some(Ask::Unanswered)
        );
        let answered = accepted(Some(PageAnswer::Answered {
            summary: "A".to_owned(),
            data: "{\"pick\":1}".to_owned(),
        }));
        assert_eq!(
            rendered_page(RENDER_TOOL, ToolStatus::Completed, &page, &answered)
                .map(|page| page.ask),
            Some(Ask::Answered {
                summary: "A".to_owned(),
                data: "{\"pick\":1}".to_owned()
            })
        );
        for (reason, ask) in [
            (Some("chat-sent-earlier"), Ask::ChatSentEarlier),
            (Some("ui-not-drawn"), Ask::NotDrawn),
            (None, Ask::AnsweredInChat),
        ] {
            let result = accepted(Some(PageAnswer::NotAnswered {
                reason: reason.map(str::to_owned),
            }));
            assert_eq!(
                rendered_page(RENDER_TOOL, ToolStatus::Completed, &page, &result)
                    .map(|page| page.ask),
                Some(ask)
            );
        }
    }

    #[test]
    fn titles_fall_back_to_the_page() {
        assert_eq!(
            title_from_html("<html><head><TITLE>A &amp; <b>B</b></title></head></html>"),
            "A & B"
        );
        assert_eq!(
            title_from_html("<body><h2 class=x>Second</h2><h1>First</h1>"),
            "Second"
        );
        assert_eq!(title_from_html("<header>no</header>"), "");
        let long = format!("<title>{}</title>", "é".repeat(200));
        assert_eq!(title_from_html(&long).chars().count(), 120);
        let mut named = input("<title>Inner</title>");
        named.title = Some("Outer".to_owned());
        assert_eq!(
            rendered_page(RENDER_TOOL, ToolStatus::Completed, &named, &accepted(None))
                .map(|page| page.title),
            Some("Outer".to_owned())
        );
    }

    #[test]
    fn ui_pages_carry_no_html() {
        let page = PageInput {
            html: Some("<p>x</p>".to_owned()),
            ui: Some("Card()".to_owned()),
            ..PageInput::default()
        };
        let shown = rendered_page(RENDER_TOOL, ToolStatus::Completed, &page, &accepted(None))
            .expect("shown");
        assert_eq!(shown.html, "");
        assert_eq!(shown.ui.as_deref(), Some("Card()"));
    }
}
