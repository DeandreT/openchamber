//! Reachable native Settings pages and their explicit, localized search targets.

use crate::locale::Strings;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Page {
    #[default]
    General,
    Appearance,
    Hosts,
}

impl Page {
    pub(crate) const ALL: [Self; 3] = [Self::General, Self::Appearance, Self::Hosts];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Appearance => "appearance",
            Self::Hosts => "remote-instances",
        }
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::General => "settings.page.general.title",
            Self::Appearance => "settings.page.appearance.title",
            Self::Hosts => "settings.page.remoteInstances.title",
        }
    }

    pub(crate) fn group(self) -> &'static str {
        match self {
            Self::General | Self::Appearance => "settings.view.nav.group.general",
            Self::Hosts => "settings.view.nav.group.projects",
        }
    }

    pub(crate) fn description(self) -> &'static str {
        match self {
            Self::General => "settings.page.general.description",
            Self::Appearance => "settings.page.appearance.description",
            Self::Hosts => "settings.remoteInstances.direct.description",
        }
    }

    pub(crate) fn icon(self) -> &'static str {
        match self {
            Self::General => "settings-3",
            Self::Appearance => "palette",
            Self::Hosts => "computer",
        }
    }
}

/// IDs, labels and synonyms follow the canonical Settings search registry.
/// Only controls with an implemented native destination belong here.
pub(crate) struct SearchItem {
    pub(crate) id: &'static str,
    pub(crate) page: Page,
    pub(crate) title: &'static str,
    pub(crate) description: Option<&'static str>,
    keywords: &'static str,
}

pub(crate) const SEARCH_ITEMS: &[SearchItem] = &[
    SearchItem {
        id: "appearance.language",
        page: Page::Appearance,
        title: "settings.appearance.language.label",
        description: Some("settings.appearance.language.description"),
        keywords: "locale translation ui language",
    },
    SearchItem {
        id: "appearance.light-theme",
        page: Page::Appearance,
        title: "settings.openchamber.visual.field.lightTheme",
        description: None,
        keywords: "theme color light mode",
    },
    SearchItem {
        id: "appearance.dark-theme",
        page: Page::Appearance,
        title: "settings.openchamber.visual.field.darkTheme",
        description: None,
        keywords: "theme color dark mode",
    },
    SearchItem {
        id: "appearance.interface-font",
        page: Page::Appearance,
        title: "settings.openchamber.visual.field.interfaceFont",
        description: None,
        keywords: "font family ui typeface",
    },
    SearchItem {
        id: "appearance.code-font",
        page: Page::Appearance,
        title: "settings.openchamber.visual.field.codeFont",
        description: None,
        keywords: "font family mono monospace code",
    },
    SearchItem {
        id: "appearance.interface-font-size",
        page: Page::Appearance,
        title: "settings.openchamber.visual.field.interfaceFontSize",
        description: None,
        keywords: "font text size ui scale",
    },
    SearchItem {
        id: "appearance.auto-save-enabled",
        page: Page::General,
        title: "settings.openchamber.visual.field.autoSaveEnabled",
        description: Some("settings.openchamber.visual.field.autoSaveEnabledInfo"),
        keywords: "editor autosave auto-save files save",
    },
    SearchItem {
        id: "appearance.terminal-close-on-exit",
        page: Page::General,
        title: "settings.openchamber.visual.field.terminalCloseOnExit",
        description: Some("settings.openchamber.visual.field.terminalCloseOnExitInfo"),
        keywords: "terminal exit close tab shell",
    },
    SearchItem {
        id: "appearance.terminal-shell",
        page: Page::General,
        title: "settings.openchamber.visual.field.terminalShell",
        description: Some("settings.openchamber.visual.field.terminalShellHint"),
        keywords: "terminal shell bash zsh fish pwsh powershell",
    },
    SearchItem {
        id: "remote-instances.direct-hosts",
        page: Page::Hosts,
        title: "settings.remoteInstances.direct.title",
        description: Some("settings.remoteInstances.direct.description"),
        keywords: "remote server host instance connect url",
    },
];

pub(crate) fn search(query: &str, strings: &Strings) -> Vec<&'static SearchItem> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    SEARCH_ITEMS
        .iter()
        .filter(|item| {
            let text = format!(
                "{} {} {} {}",
                strings.get(item.title),
                item.description
                    .map(|key| strings.get(key))
                    .unwrap_or_default(),
                strings.get(item.page.title()),
                item.keywords,
            )
            .to_lowercase();
            query.split_whitespace().all(|term| text.contains(term))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_matches_all_terms_and_localized_labels() {
        let en = Strings::load("en");
        assert!(search("  ", &en).is_empty());
        assert_eq!(
            search("TRANSLATION language", &en)[0].id,
            "appearance.language"
        );
        assert_eq!(search("auto-save files", &en)[0].page, Page::General);
        assert!(search("font nonexistent", &en).is_empty());
        for locale in ["ar", "he", "de"] {
            let strings = Strings::load(locale);
            let label = strings.get("settings.appearance.language.label");
            assert!(
                search(&label, &strings)
                    .iter()
                    .any(|item| item.id == "appearance.language")
            );
        }
    }

    #[test]
    fn destinations_and_ids_are_unambiguous() {
        let mut ids = std::collections::HashSet::new();
        for item in SEARCH_ITEMS {
            assert!(ids.insert(item.id), "each search target has one identity");
            assert!(
                Page::ALL.contains(&item.page),
                "every result opens a real page"
            );
        }
        assert_eq!(Page::default(), Page::General);
    }
}
