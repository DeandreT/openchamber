//! Embedded assets: OpenChamber's sprite icons, brand logo and Kit defaults.
//!
//! `resources/icons.json` holds the web UI's own glyphs, exported from its
//! icon sprite by `bun run native:resources`, so both clients draw identical
//! icons.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

use gpui_kit::{AssetSource, Result, SharedString};
use quick_xml::events::Event;
use quick_xml::reader::Reader;

use crate::locale::Direction;

const ICONS_JSON: &str = include_str!("../resources/icons.json");
const ICON_PREFIX: &str = "openchamber/icons/";
const DIRECTIONAL_ICONS: &[&str] = &[
    "arrow-left",
    "arrow-right",
    "arrow-left-s",
    "arrow-right-s",
    "arrow-go-back",
    "layout-left",
    "layout-right",
    "panel-left",
    "panel-card",
    "send-plane",
    "send-plane-2",
];
pub(crate) const APP_LOGO: &str = "openchamber/logo.svg";
const APP_LOGO_SVG: &str = include_str!("../../../../packages/web/public/logo-dark-512x512.svg");
/// The name of the MCP mark, which the icon sprite does not carry
/// (`components/icons/McpIcon.tsx`). It is served like a sprite icon.
pub(crate) const MCP_ICON: &str = "mcp";
const MCP_ICON_SVG: &str = concat!(
    r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="-0.5 -0.5 25 25" fill="currentColor" "#,
    r#"stroke="currentColor" stroke-width="0.5" fill-rule="evenodd">"#,
    r#"<path d="M15.688 2.343a2.588 2.588 0 00-3.61 0l-9.626 9.44a.863.863 0 01-1.203 0 .823.823 0 010-1.18l9.626-9.44a4.313 4.313 0 016.016 0 4.116 4.116 0 011.204 3.54 4.3 4.3 0 013.609 1.18l.05.05a4.115 4.115 0 010 5.9l-8.706 8.537a.274.274 0 000 .393l1.788 1.754a.823.823 0 010 1.18.863.863 0 01-1.203 0l-1.788-1.753a1.92 1.92 0 010-2.754l8.706-8.538a2.47 2.47 0 000-3.54l-.05-.049a2.588 2.588 0 00-3.607-.003l-7.172 7.034-.002.002-.098.097a.863.863 0 01-1.204 0 .823.823 0 010-1.18l7.273-7.133a2.47 2.47 0 00-.003-3.537z"/>"#,
    r#"<path d="M14.485 4.703a.823.823 0 000-1.18.863.863 0 00-1.204 0l-7.119 6.982a4.115 4.115 0 000 5.9 4.314 4.314 0 006.016 0l7.12-6.982a.823.823 0 000-1.18.863.863 0 00-1.204 0l-7.119 6.982a2.588 2.588 0 01-3.61 0 2.47 2.47 0 010-3.54l7.12-6.982z"/>"#,
    "</svg>"
);
/// The composer's Stop mark, which the icon sprite does not carry
/// (`components/icons/StopIcon.tsx`): an outlined square over its own fill at
/// a fifth of the color. It is served like a sprite icon.
pub(crate) const STOP_ICON: &str = "stop-duotone";
const STOP_ICON_SVG: &str = concat!(
    r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256" fill="currentColor">"#,
    r#"<path d="M208,56V200a8,8,0,0,1-8,8H56a8,8,0,0,1-8-8V56a8,8,0,0,1,8-8H200A8,8,0,0,1,208,56Z" opacity="0.2"/>"#,
    r#"<path d="M200,40H56A16,16,0,0,0,40,56V200a16,16,0,0,0,16,16H200a16,16,0,0,0,16-16V56A16,16,0,0,0,200,40Zm0,160H56V56H200V200Z"/>"#,
    "</svg>"
);

/// Serves OpenChamber's embedded icons and logo, then Kit assets.
#[derive(Clone, Copy, Debug, Default)]
pub struct AppAssets;

fn icons() -> &'static HashMap<String, String> {
    static ICONS: OnceLock<HashMap<String, String>> = OnceLock::new();
    ICONS.get_or_init(|| {
        serde_json::from_str(ICONS_JSON).expect("resources/icons.json is generated JSON")
    })
}

/// The exported icon named `name`, when the sprite export carries it.
pub(crate) fn icon_name(name: &str) -> Option<&'static str> {
    icons().get_key_value(name).map(|(name, _)| name.as_str())
}

/// The asset path of a sprite icon, such as `arrow-right-s`.
pub fn icon_path(name: &str) -> SharedString {
    format!("{ICON_PREFIX}{name}.svg").into()
}

/// The sprite path in the layout's direction.
pub fn icon_path_in(name: &str, direction: Direction) -> SharedString {
    if direction == Direction::RightToLeft && DIRECTIONAL_ICONS.contains(&name) {
        format!("{ICON_PREFIX}{name}.rtl.svg").into()
    } else {
        icon_path(name)
    }
}

fn mirror_icon(svg: &str) -> Option<String> {
    let mut reader = Reader::from_str(svg);
    let Event::Start(root) = reader.read_event().ok()? else {
        return None;
    };
    if root.name().as_ref() != b"svg" {
        return None;
    }
    let view_box = root
        .attributes()
        .filter_map(std::result::Result::ok)
        .find(|attribute| attribute.key.as_ref() == b"viewBox")?;
    let values = std::str::from_utf8(view_box.value.as_ref())
        .ok()?
        .split(|character: char| character == ',' || character.is_ascii_whitespace())
        .filter(|value| !value.is_empty())
        .map(str::parse::<f64>)
        .collect::<std::result::Result<Vec<_>, _>>()
        .ok()?;
    let [min_x, min_y, width, height] = values.as_slice() else {
        return None;
    };
    if ![min_x, min_y, width, height]
        .iter()
        .all(|value| value.is_finite())
        || *width <= 0.0
        || *height <= 0.0
    {
        return None;
    }
    let translate = 2.0 * min_x + width;
    if !translate.is_finite() {
        return None;
    }
    let start = reader.buffer_position() as usize;
    let end = svg.rfind("</svg>")?;
    if end < start {
        return None;
    }
    Some(format!(
        "{}<g transform=\"matrix(-1 0 0 1 {translate} 0)\">{}</g>{}",
        &svg[..start],
        &svg[start..end],
        &svg[end..]
    ))
}

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if path == APP_LOGO {
            return Ok(Some(Cow::Owned(
                APP_LOGO_SVG.replace("#f5f5f7", "currentColor").into_bytes(),
            )));
        }
        if let Some(name) = path
            .strip_prefix(ICON_PREFIX)
            .and_then(|rest| rest.strip_suffix(".svg"))
        {
            if let Some(name) = name.strip_suffix(".rtl") {
                return Ok(DIRECTIONAL_ICONS
                    .contains(&name)
                    .then(|| icons().get(name).and_then(|svg| mirror_icon(svg)))
                    .flatten()
                    .map(|svg| Cow::Owned(svg.into_bytes())));
            }
            if name == MCP_ICON {
                return Ok(Some(Cow::Borrowed(MCP_ICON_SVG.as_bytes())));
            }
            if name == STOP_ICON {
                return Ok(Some(Cow::Borrowed(STOP_ICON_SVG.as_bytes())));
            }
            return Ok(icons().get(name).map(|svg| Cow::Borrowed(svg.as_bytes())));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut entries = gpui_kit::assets::Assets.list(path)?;
        if APP_LOGO.starts_with(path) {
            entries.push(APP_LOGO.into());
        }
        for name in icons().keys() {
            let icon = icon_path(name);
            if icon.starts_with(path) {
                entries.push(icon);
            }
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_sprite_icons_by_name() {
        let svg = AppAssets
            .load(&icon_path("arrow-right-s"))
            .expect("loading never fails for sprite icons")
            .expect("the icon is exported");
        assert!(svg.starts_with(b"<svg"));
        assert!(
            AppAssets
                .load("openchamber/icons/not-exported.svg")
                .expect("unknown sprite icons are absent, not errors")
                .is_none()
        );
    }

    #[test]
    fn mirrors_directional_sprite_icons_for_right_to_left() {
        let path = icon_path_in("send-plane-2", Direction::RightToLeft);
        assert_eq!(path.as_ref(), "openchamber/icons/send-plane-2.rtl.svg");
        let svg = AppAssets
            .load(&path)
            .expect("asset source")
            .expect("directional sprite icon");
        let text = std::str::from_utf8(&svg).expect("SVG is UTF-8");
        assert!(text.contains("<g transform=\"matrix(-1 0 0 1 24 0)\">"));
        assert!(text.ends_with("</g></svg>"));
        assert_eq!(
            icon_path_in("send-plane-2", Direction::LeftToRight),
            icon_path("send-plane-2")
        );
        assert_eq!(
            icon_path_in("add", Direction::RightToLeft),
            icon_path("add")
        );
        assert!(
            AppAssets
                .load("openchamber/icons/add.rtl.svg")
                .expect("asset source")
                .is_none()
        );
        assert!(
            !AppAssets
                .list(ICON_PREFIX)
                .expect("asset source")
                .iter()
                .any(|path| path.ends_with(".rtl.svg"))
        );
    }

    #[test]
    fn mirror_requires_a_valid_view_box() {
        let svg = r#"<svg viewBox="-2, 3, 10, 12"><path/></svg>"#;
        assert!(
            mirror_icon(svg)
                .expect("valid viewBox")
                .contains("matrix(-1 0 0 1 6 0)")
        );
        for invalid in [
            "<svg><path/></svg>",
            r#"<svg viewBox="0 0 nope 24"><path/></svg>"#,
            r#"<svg viewBox="0 0 NaN 24"><path/></svg>"#,
            r#"<svg viewBox="0 0 0 24"><path/></svg>"#,
        ] {
            assert!(mirror_icon(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn directional_icon_set_matches_the_web() {
        let web = include_str!("../../../../packages/ui/src/components/icon/Icon.tsx");
        let literal = web
            .split_once("const DIRECTIONAL_ICONS:")
            .expect("the web directional set")
            .1
            .split_once("([")
            .expect("the set's opening")
            .1
            .split_once("])")
            .expect("the set's closing")
            .0;
        let names: Vec<_> = literal
            .lines()
            .filter_map(|line| line.trim().strip_prefix('"')?.strip_suffix("\","))
            .collect();
        let web: std::collections::HashSet<_> = names.iter().copied().collect();
        let native: std::collections::HashSet<_> = DIRECTIONAL_ICONS.iter().copied().collect();
        assert_eq!(web.len(), names.len(), "duplicate names in the web set");
        assert_eq!(
            native.len(),
            DIRECTIONAL_ICONS.len(),
            "duplicate native names"
        );
        assert_eq!(native, web);
    }

    #[test]
    fn the_mcp_mark_is_served_beside_the_sprite_icons() {
        let svg = AppAssets
            .load(&icon_path(MCP_ICON))
            .expect("asset source")
            .expect("the MCP mark is embedded");
        let text = std::str::from_utf8(&svg).expect("SVG is UTF-8");
        assert!(text.starts_with("<svg"));
        assert!(text.contains("viewBox=\"-0.5 -0.5 25 25\""));
        assert!(text.contains("currentColor"));
        assert!(
            icons().get(MCP_ICON).is_none(),
            "the sprite export carries no icon of this name to shadow"
        );
    }

    #[test]
    fn the_stop_mark_is_served_beside_the_sprite_icons() {
        let svg = AppAssets
            .load(&icon_path(STOP_ICON))
            .expect("asset source")
            .expect("the Stop mark is embedded");
        let text = std::str::from_utf8(&svg).expect("SVG is UTF-8");
        assert!(text.starts_with("<svg"));
        assert!(text.contains("viewBox=\"0 0 256 256\""));
        assert!(
            text.contains("opacity=\"0.2\""),
            "the fill under the outline"
        );
        assert!(
            icons().get(STOP_ICON).is_none(),
            "the sprite export carries no icon of this name to shadow"
        );
    }

    #[test]
    fn app_logo_uses_the_canonical_brand_geometry_and_theme_color() {
        let logo = AppAssets
            .load(APP_LOGO)
            .expect("asset source")
            .expect("brand asset");
        let text = std::str::from_utf8(&logo).expect("SVG is UTF-8");
        assert!(text.contains("viewBox=\"-50 -50 100 100\""));
        assert!(text.contains("currentColor"));
        assert!(!text.contains("#f5f5f7"));
    }
}
