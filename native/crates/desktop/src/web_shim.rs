//! The frame shim: the desktop's own top document around untrusted content.
//!
//! Electron draws an agent's HTML page, an HTML file preview and an
//! extension panel in a sandboxed `<iframe>` of the app's document, and talks
//! to it with `postMessage`. The desktop gives each of them a WebKit view
//! whose top document is this shim: one sandboxed iframe filling the view,
//! with the same sandbox attributes as the reference. The shim has no script
//! of its own. A relay in an isolated script world of the top frame passes
//! the frame's messages to Rust and Rust's messages to the frame; the frame's
//! own world, and every subframe, sees no message handler at all.

/// The isolated world the relay runs in.
pub(crate) const RELAY_WORLD: &str = "openchamber-shim";
/// The script message handler, registered in [`RELAY_WORLD`] only.
pub(crate) const RELAY_HANDLER: &str = "openchamberShim";
/// The longest message the relay passes on, in UTF-16 units.
const RELAY_MESSAGE_MAX: usize = 4 * 1024 * 1024;
const FRAME_ID: &str = "openchamber-frame";

/// What the shim's iframe loads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FrameSource {
    /// A document given inline (`srcdoc`).
    Inline(String),
    /// A document at an address.
    Address(String),
}

/// The sandbox of an agent's page and of an HTML preview: scripts and
/// forms, no same origin, no top navigation, no popups.
pub(crate) const PAGE_SANDBOX: &str = "allow-scripts allow-forms";
/// The sandbox of an extension panel (`PluginPane`).
pub(crate) const GUEST_SANDBOX: &str = "allow-scripts";

/// Escapes text for a double-quoted HTML attribute.
fn attribute(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '"' => escaped.push_str("&quot;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// The shim document holding `source` in a frame with `sandbox`.
pub(crate) fn shim_document(source: &FrameSource, sandbox: &str, title: &str) -> String {
    let content = match source {
        FrameSource::Inline(html) => format!("srcdoc=\"{}\"", attribute(html)),
        FrameSource::Address(url) => format!("src=\"{}\"", attribute(url)),
    };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title>\
         <style>html,body{{margin:0;height:100%;overflow:hidden;background:transparent}}\
         iframe{{display:block;border:0;width:100%;height:100%;background:transparent}}</style>\
         </head><body><iframe id=\"{FRAME_ID}\" title=\"{}\" sandbox=\"{}\" {content}></iframe>\
         </body></html>",
        attribute(title),
        attribute(title),
        attribute(sandbox),
    )
}

/// The relay, injected at document start into [`RELAY_WORLD`] of the top
/// frame only. It forwards messages whose source is the shim's frame and
/// defines the one function Rust calls to post into the frame.
pub(crate) fn relay_script() -> String {
    format!(
        r#"(() => {{
  const handlers = window.webkit && window.webkit.messageHandlers;
  const handler = handlers && handlers.{RELAY_HANDLER};
  if (!handler) return;
  const frame = () => document.getElementById("{FRAME_ID}");
  window.addEventListener("message", (event) => {{
    const target = frame();
    if (!target || event.source !== target.contentWindow) return;
    let text;
    try {{ text = JSON.stringify(event.data); }} catch (error) {{ return; }}
    if (typeof text === "string" && text.length <= {RELAY_MESSAGE_MAX}) handler.postMessage(text);
  }});
  Object.defineProperty(window, "__openchamberShimPost", {{
    value: (message) => {{
      const target = frame();
      if (target && target.contentWindow) target.contentWindow.postMessage(message, "*");
    }},
  }});
}})();"#
    )
}

/// The script Rust evaluates in [`RELAY_WORLD`] to post `message` into the
/// frame. JSON is valid JavaScript, so the value needs no further quoting;
/// `<` is escaped anyway so the text never reads as markup.
pub(crate) fn post_script(message: &serde_json::Value) -> String {
    let json = message.to_string().replace('<', "\\u003c");
    format!("window.__openchamberShimPost && window.__openchamberShimPost({json});")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_documents_cannot_leave_their_attribute() {
        let document = shim_document(
            &FrameSource::Inline("<p title=\"x\">a & b</p>\"><script>alert(1)</script>".to_owned()),
            PAGE_SANDBOX,
            "T\"<",
        );
        assert!(document.contains("sandbox=\"allow-scripts allow-forms\""));
        assert!(!document.contains("<script>"));
        assert_eq!(document.matches("<iframe").count(), 1);
        assert!(document.contains("srcdoc=\"&lt;p title=&quot;x&quot;&gt;a &amp; b"));
        assert!(!document.contains("allow-same-origin"));
    }

    #[test]
    fn addressed_frames_keep_their_sandbox() {
        let document = shim_document(
            &FrameSource::Address(
                "http://127.0.0.1:1/api/guest-files/g/a.html?x=1&y=\"".to_owned(),
            ),
            GUEST_SANDBOX,
            "Panel",
        );
        assert!(document.contains("sandbox=\"allow-scripts\""));
        assert!(
            document
                .contains("src=\"http://127.0.0.1:1/api/guest-files/g/a.html?x=1&amp;y=&quot;\"")
        );
    }

    #[test]
    fn posted_messages_stay_one_expression() {
        let script = post_script(&serde_json::json!({ "text": "</script><b>" }));
        assert!(!script.contains('<'));
        assert!(script.starts_with("window.__openchamberShimPost"));
    }
}
