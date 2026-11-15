//! Interface and code fonts, resolved from the web client's default stacks.
//!
//! The default `system` and `system-mono` options in
//! `packages/ui/src/lib/fontOptions.ts` use
//! `-apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif` and
//! `ui-monospace, "SFMono-Regular", "Menlo", "Cascadia Mono", "Segoe UI Mono",
//! monospace`. Chromium takes the first installed family by its exact name,
//! then the platform's generic family, so both clients draw text with the same
//! face. On Linux the generic families come from fontconfig, which is slow
//! enough to read on a thread while the application starts.
//!
//! The interface stack ends in `sans-serif`, which Chromium maps to its
//! default sans-serif family, Arial, and fontconfig to Arial or a metric twin
//! such as Liberation Sans. Text the interface face lacks a glyph for (such as
//! `↑`) comes from that face before any per-character fallback, so it is the
//! interface font's first fallback here too.

#[cfg(target_os = "linux")]
use std::process::Command;
use std::thread::{self, JoinHandle};

use gpui_kit::component::Theme;
use gpui_kit::{App, FontFallbacks, Global, SharedString};

/// Named families of the interface stack after the macOS-only keywords.
const UI_FAMILIES: &[&str] = &["Segoe UI"];
/// Named families of the code stack after `ui-monospace`.
const MONO_FAMILIES: &[&str] = &["SFMono-Regular", "Menlo", "Cascadia Mono", "Segoe UI Mono"];
/// Chromium's default `sans-serif` family, and the families Skia accepts in
/// its place when fontconfig substitutes one.
#[cfg(target_os = "linux")]
const SANS_SERIF_FAMILY: &str = "Arial";
#[cfg(target_os = "linux")]
const SANS_SERIF_EQUIVALENTS: &[&str] = &["Arial", "Liberation Sans", "Arimo", "Albany AMT"];

/// The platform's `system-ui`, `sans-serif` and `monospace` families.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GenericFamilies {
    system_ui: Option<String>,
    sans_serif: Option<String>,
    monospace: Option<String>,
}

/// Starts reading the generic families; join the handle before the first frame.
pub fn spawn_generic_lookup() -> JoinHandle<GenericFamilies> {
    thread::spawn(generic_families)
}

#[cfg(target_os = "linux")]
fn generic_families() -> GenericFamilies {
    GenericFamilies {
        system_ui: fontconfig_family("system-ui").or_else(|| fontconfig_family("sans-serif")),
        sans_serif: fontconfig_family(SANS_SERIF_FAMILY)
            .filter(|family| SANS_SERIF_EQUIVALENTS.contains(&family.as_str())),
        monospace: fontconfig_family("monospace"),
    }
}

#[cfg(target_os = "windows")]
fn generic_families() -> GenericFamilies {
    GenericFamilies {
        system_ui: Some("Segoe UI".to_owned()),
        sans_serif: Some("Arial".to_owned()),
        monospace: Some("Consolas".to_owned()),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn generic_families() -> GenericFamilies {
    GenericFamilies {
        system_ui: Some(".SystemUIFont".to_owned()),
        sans_serif: Some("Helvetica".to_owned()),
        monospace: Some("Menlo".to_owned()),
    }
}

/// The family fontconfig substitutes for `pattern`.
#[cfg(target_os = "linux")]
fn fontconfig_family(pattern: &str) -> Option<String> {
    let output = Command::new("fc-match")
        .args(["--format=%{family[0]}", pattern])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let family = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!family.is_empty()).then_some(family)
}

/// The resolved interface and code families.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FontFamilies {
    pub ui: SharedString,
    pub mono: SharedString,
}

/// The resolved system stacks survive user font choices and Kit mode resets.
#[derive(Clone)]
struct SystemFonts(FontFamilies);
impl Global for SystemFonts {}

/// The interface font's fallbacks: the stack's `sans-serif` family when it
/// differs from the interface family.
#[derive(Clone, Debug, Default)]
pub struct UiFontFallbacks(pub Option<FontFallbacks>);

impl Global for UiFontFallbacks {}

/// Installed counterparts of the Electron font options. Native never downloads fonts.
pub(crate) struct FontOption {
    pub(crate) id: &'static str,
    pub(crate) label: &'static str,
}

const UI_OPTIONS: &[(&str, &str)] = &[
    ("inter", "Inter"),
    ("fixel", "Fixel Text"),
    ("geist-sans", "Geist Sans"),
    ("atkinson-hyperlegible", "Atkinson Hyperlegible"),
    ("source-sans-3", "Source Sans 3"),
    ("roboto", "Roboto"),
    ("noto-sans", "Noto Sans"),
    ("dm-sans", "DM Sans"),
    ("manrope", "Manrope"),
];
const MONO_OPTIONS: &[(&str, &str)] = &[
    ("jetbrains-mono", "JetBrains Mono"),
    ("fira-code", "Fira Code"),
    ("geist-mono", "Geist Mono"),
    ("commit-mono", "Commit Mono"),
    ("source-code-pro", "Source Code Pro"),
    ("cascadia-code", "Cascadia Code"),
    ("roboto-mono", "Roboto Mono"),
    ("iosevka", "Iosevka"),
];

pub(crate) fn valid_ui_font(id: &str) -> bool {
    id == "system" || UI_OPTIONS.iter().any(|option| option.0 == id)
}

pub(crate) fn valid_mono_font(id: &str) -> bool {
    id == "system-mono" || MONO_OPTIONS.iter().any(|option| option.0 == id)
}

fn available_fonts(options: &'static [(&'static str, &'static str)], cx: &App) -> Vec<FontOption> {
    let installed = cx.text_system().all_font_names();
    options
        .iter()
        .filter(|(_, family)| installed.iter().any(|name| name == family))
        .map(|(id, label)| FontOption { id, label })
        .collect()
}

/// Named installed fonts only. The caller supplies a localized System option.
pub(crate) fn available_ui_fonts(cx: &App) -> Vec<FontOption> {
    available_fonts(UI_OPTIONS, cx)
}
pub(crate) fn available_mono_fonts(cx: &App) -> Vec<FontOption> {
    available_fonts(MONO_OPTIONS, cx)
}

/// Applies the saved IDs, falling back to system stacks if a font was uninstalled.
pub(crate) fn apply_preferences(generic: &GenericFamilies, ui: &str, mono: &str, cx: &mut App) {
    if cx.try_global::<SystemFonts>().is_none() {
        apply(generic, cx);
    }
    let system = cx.global::<SystemFonts>().0.clone();
    let installed = cx.text_system().all_font_names();
    let pick = |options: &[(&str, &str)], id: &str| {
        options
            .iter()
            .find(|(candidate, family)| {
                *candidate == id && installed.iter().any(|name| name == family)
            })
            .map(|(_, family)| SharedString::from((*family).to_owned()))
    };
    let ui = pick(UI_OPTIONS, ui).unwrap_or(system.ui);
    let mono = pick(MONO_OPTIONS, mono).unwrap_or(system.mono);
    cx.set_global(UiFontFallbacks(ui_fallbacks(generic, &ui)));
    Theme::update(cx, |theme| {
        theme.font_family = ui;
        theme.mono_font_family = mono;
    });
}

/// The `sans-serif` family as the interface font's fallback, unless it is
/// the interface family itself.
pub fn ui_fallbacks(generic: &GenericFamilies, ui: &SharedString) -> Option<FontFallbacks> {
    generic
        .sans_serif
        .as_ref()
        .filter(|family| family.as_str() != ui.as_ref())
        .map(|family| FontFallbacks::from_fonts(vec![family.clone()]))
}

/// Resolves both stacks against the installed fonts. A stack with nothing
/// installed and no generic family keeps `fallback`.
pub fn resolve(
    generic: &GenericFamilies,
    installed: impl Fn(&str) -> bool,
    fallback: &FontFamilies,
) -> FontFamilies {
    let pick = |named: &[&str], generic: Option<&String>, fallback: &SharedString| {
        named
            .iter()
            .find(|family| installed(family))
            .map(|family| SharedString::from(family.to_string()))
            .or_else(|| generic.map(|family| SharedString::from(family.clone())))
            .unwrap_or_else(|| fallback.clone())
    };
    FontFamilies {
        ui: pick(UI_FAMILIES, generic.system_ui.as_ref(), &fallback.ui),
        mono: pick(MONO_FAMILIES, generic.monospace.as_ref(), &fallback.mono),
    }
}

/// Installs the families for native views and Kit components.
pub fn apply(generic: &GenericFamilies, cx: &mut App) {
    let theme = Theme::global(cx);
    let fallback = FontFamilies {
        ui: theme.font_family.clone(),
        mono: theme.mono_font_family.clone(),
    };
    // Sorted and deduplicated by the text system.
    let installed = cx.text_system().all_font_names();
    let families = resolve(
        generic,
        |family| {
            installed
                .binary_search_by(|name| name.as_str().cmp(family))
                .is_ok()
        },
        &fallback,
    );
    let fallbacks = ui_fallbacks(generic, &families.ui);
    tracing::info!(ui = %families.ui, mono = %families.mono, ?fallbacks, "fonts selected");
    cx.set_global(SystemFonts(families.clone()));
    cx.set_global(UiFontFallbacks(fallbacks));
    Theme::update(cx, |theme| {
        theme.font_family = families.ui.clone();
        theme.mono_font_family = families.mono.clone();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui_kit::test]
    fn system_choices_restore_the_original_stacks_after_a_named_choice(
        cx: &mut gpui_kit::TestAppContext,
    ) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            let generic = GenericFamilies::default();
            apply(&generic, cx);
            let system = cx.global::<SystemFonts>().0.clone();
            Theme::update(cx, |theme| {
                theme.font_family = "Named UI".into();
                theme.mono_font_family = "Named Mono".into();
            });
            apply_preferences(&generic, "system", "system-mono", cx);
            assert_eq!(Theme::global(cx).font_family, system.ui);
            assert_eq!(Theme::global(cx).mono_font_family, system.mono);
        });
    }

    #[test]
    fn named_families_win_over_generic_ones_only_when_installed() {
        let generic = GenericFamilies {
            system_ui: Some("Noto Sans".to_owned()),
            sans_serif: Some("Liberation Sans".to_owned()),
            monospace: Some("Noto Sans Mono".to_owned()),
        };
        let fallback = FontFamilies {
            ui: "Kit Sans".into(),
            mono: "Kit Mono".into(),
        };
        let none = resolve(&generic, |_| false, &fallback);
        assert_eq!(none.ui.as_ref(), "Noto Sans");
        assert_eq!(none.mono.as_ref(), "Noto Sans Mono");
        let windows = resolve(
            &generic,
            |family| matches!(family, "Segoe UI" | "Cascadia Mono" | "Segoe UI Mono"),
            &fallback,
        );
        assert_eq!(windows.ui.as_ref(), "Segoe UI");
        assert_eq!(
            windows.mono.as_ref(),
            "Cascadia Mono",
            "the stack's order decides"
        );
        let bare = resolve(&GenericFamilies::default(), |_| false, &fallback);
        assert_eq!(bare, fallback);
    }

    #[test]
    fn the_sans_serif_family_falls_back_only_when_it_differs() {
        let generic = GenericFamilies {
            system_ui: Some("Noto Sans".to_owned()),
            sans_serif: Some("Liberation Sans".to_owned()),
            monospace: None,
        };
        assert_eq!(
            ui_fallbacks(&generic, &"Noto Sans".into())
                .map(|fallbacks| fallbacks.fallback_list().to_vec()),
            Some(vec!["Liberation Sans".to_owned()])
        );
        assert!(ui_fallbacks(&generic, &"Liberation Sans".into()).is_none());
        assert!(ui_fallbacks(&GenericFamilies::default(), &"Noto Sans".into()).is_none());
    }
}
