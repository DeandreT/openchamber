//! Embedded assets: OpenChamber's sprite icons, brand logo and Kit defaults.
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
pub(crate) const APP_LOGO: &str = "openchamber/logo.svg";
const APP_LOGO_SVG: &str = include_str!("../../../../packages/web/public/logo-dark-512x512.svg");

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
