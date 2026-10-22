//! Embedded assets: OpenChamber's sprite icons and GPUI Kit's defaults.
//!
//! `resources/icons.json` holds the web UI's own glyphs, exported from its
//! icon sprite by `bun run native:resources`, so both clients draw identical
//! icons.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

use gpui_kit::{AssetSource, Result, SharedString};

const ICONS_JSON: &str = include_str!("../resources/icons.json");
const ICON_PREFIX: &str = "openchamber/icons/";

/// Serves OpenChamber icons under `openchamber/icons/` and Kit assets elsewhere.
#[derive(Clone, Copy, Debug, Default)]
pub struct AppAssets;

fn icons() -> &'static HashMap<String, String> {
    static ICONS: OnceLock<HashMap<String, String>> = OnceLock::new();
    ICONS.get_or_init(|| {
        serde_json::from_str(ICONS_JSON).expect("resources/icons.json is generated JSON")
    })
}

/// The asset path of a sprite icon, such as `arrow-right-s`.
pub fn icon_path(name: &str) -> SharedString {
    format!("{ICON_PREFIX}{name}.svg").into()
}

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(name) = path
            .strip_prefix(ICON_PREFIX)
            .and_then(|rest| rest.strip_suffix(".svg"))
        {
            return Ok(icons().get(name).map(|svg| Cow::Borrowed(svg.as_bytes())));
        }
        gpui_kit::assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut entries = gpui_kit::assets::Assets.list(path)?;
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
}
