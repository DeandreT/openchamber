//! Reachable native Settings pages and their explicit, localized search targets.

use crate::locale::Strings;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Page {
    #[default]
    General,
    Appearance,
    Chat,
    Notifications,
    Sessions,
    Shortcuts,
    Voice,
    Usage,
    Hosts,
    Behavior,
    Snippets,
}

impl Page {
    /// `pageOrder` in `SettingsView.tsx`, limited to implemented pages.
    pub(crate) const ALL: [Self; 11] = [
        Self::General,
        Self::Appearance,
        Self::Chat,
        Self::Notifications,
        Self::Sessions,
        Self::Shortcuts,
        Self::Voice,
        Self::Usage,
        Self::Hosts,
        Self::Behavior,
        Self::Snippets,
    ];

    pub(crate) fn id(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Appearance => "appearance",
            Self::Chat => "chat",
            Self::Notifications => "notifications",
            Self::Voice => "voice",
            Self::Usage => "usage",
            Self::Sessions => "sessions",
            Self::Shortcuts => "shortcuts",
            Self::Hosts => "remote-instances",
            Self::Behavior => "behavior",
            Self::Snippets => "snippets",
        }
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::General => "settings.page.general.title",
            Self::Appearance => "settings.page.appearance.title",
            Self::Chat => "settings.page.chat.title",
            Self::Notifications => "settings.page.notifications.title",
            Self::Voice => "settings.page.voice.title",
            Self::Usage => "settings.page.usage.title",
            Self::Sessions => "settings.page.sessions.title",
            Self::Shortcuts => "settings.page.shortcuts.title",
            Self::Hosts => "settings.page.remoteInstances.title",
            Self::Behavior => "settings.page.behavior.title",
            Self::Snippets => "settings.page.snippets.title",
        }
    }

    pub(crate) fn group(self) -> &'static str {
        match self {
            Self::General
            | Self::Appearance
            | Self::Chat
            | Self::Notifications
            | Self::Sessions
            | Self::Shortcuts
            | Self::Voice
            | Self::Usage => "settings.view.nav.group.general",
            Self::Hosts => "settings.view.nav.group.projects",
            Self::Behavior => "settings.view.nav.group.opencode",
            Self::Snippets => "settings.view.nav.group.content",
        }
    }

    /// The header's description; split pages such as Snippets have none.
    pub(crate) fn description(self) -> Option<&'static str> {
        Some(match self {
            Self::General => "settings.page.general.description",
            Self::Appearance => "settings.page.appearance.description",
            Self::Chat => "settings.page.chat.description",
            Self::Notifications => "settings.page.notifications.description",
            Self::Voice => "settings.page.voice.description",
            // A split page: its list stands in for a description.
            Self::Usage => "settings.usage.page.empty.selectProvider",
            Self::Sessions => "settings.page.sessions.description",
            Self::Shortcuts => "settings.page.shortcuts.description",
            Self::Hosts => "settings.remoteInstances.direct.description",
            Self::Behavior => "settings.page.behavior.description",
            Self::Snippets => return None,
        })
    }

    pub(crate) fn icon(self) -> &'static str {
        match self {
            Self::General => "settings-3",
            Self::Appearance => "palette",
            Self::Chat => "chat-ai-3",
            Self::Notifications => "notification-3",
            Self::Voice => "mic",
            Self::Usage => "bar-chart-2",
            Self::Sessions => "chat-history",
            Self::Shortcuts => "command",
            Self::Hosts => "computer",
            Self::Behavior => "brain",
            Self::Snippets => "chat-thread",
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
        id: "chat.prompt-navigator",
        page: Page::Chat,
        title: "settings.openchamber.visual.field.promptNavigatorEnabled",
        description: None,
        keywords: "prompt navigator navigation timeline scroll",
    },
    SearchItem {
        id: "chat.code-block-line-wrap",
        page: Page::Chat,
        title: "settings.openchamber.visual.field.codeBlockLineWrap",
        description: None,
        keywords: "code wrap line wrap markdown",
    },
    SearchItem {
        id: "chat.draft-starters-visible",
        page: Page::Chat,
        title: "settings.openchamber.visual.field.draftStartersVisible",
        description: None,
        keywords: "starter starters new session welcome suggestions",
    },
    SearchItem {
        id: "chat.subagent-read-only-banner",
        page: Page::Chat,
        title: "settings.openchamber.visual.field.allowPromptingSubagentSessions",
        description: None,
        keywords: "subagent read only prompt banner",
    },
    SearchItem {
        id: "chat.follow-up-behavior",
        page: Page::Chat,
        title: "settings.openchamber.visual.section.followUpBehavior",
        description: Some("settings.openchamber.visual.field.followUpBehaviorDescription"),
        keywords: "follow up queue steer send immediately",
    },
    SearchItem {
        id: "sessions.deletion-dialog",
        page: Page::Sessions,
        title: "settings.openchamber.defaults.field.showDeletionDialog",
        description: None,
        keywords: "delete confirmation",
    },
    SearchItem {
        id: "sessions.permission-default",
        page: Page::Sessions,
        title: "settings.sessions.permissions.defaultMode",
        description: Some("settings.sessions.permissions.defaultModeInfo"),
        keywords: "permissions auto-accept accept safety net ask shield approval new session",
    },
    SearchItem {
        id: "sessions.show-chats",
        page: Page::Sessions,
        title: "settings.openchamber.defaults.field.showChats",
        description: None,
        keywords: "chats sidebar hide show section",
    },
    SearchItem {
        id: "sessions.work-enabled",
        page: Page::Sessions,
        title: "settings.openchamber.sessionWork.field.enabled",
        description: None,
        keywords: "in work in progress track done sidebar",
    },
    SearchItem {
        id: "sessions.warming",
        page: Page::Sessions,
        title: "settings.openchamber.defaults.field.sessionWarming",
        description: Some("settings.openchamber.defaults.field.sessionWarmingInfo"),
        keywords: "warming warm cache prompt cache keep-alive idle per session keep warm duration how long hour",
    },
    SearchItem {
        id: "sessions.compaction-auto",
        page: Page::Sessions,
        title: "settings.openchamber.defaults.compaction.auto",
        description: Some("settings.openchamber.defaults.compaction.autoInfo"),
        keywords: "compaction compact auto automatic context limit summary overflow",
    },
    SearchItem {
        id: "sessions.compaction-buffer",
        page: Page::Sessions,
        title: "settings.openchamber.defaults.compaction.buffer",
        description: Some("settings.openchamber.defaults.compaction.bufferInfo"),
        keywords: "compaction compact buffer threshold context limit context window summary tokens",
    },
    SearchItem {
        id: "sessions.compaction-keep",
        page: Page::Sessions,
        title: "settings.openchamber.defaults.compaction.keep",
        description: Some("settings.openchamber.defaults.compaction.keepInfo"),
        keywords: "compaction compact keep recent tail summary tokens",
    },
    SearchItem {
        id: "sessions.background-compaction",
        page: Page::Sessions,
        title: "settings.openchamber.defaults.compaction.background",
        description: Some("settings.openchamber.defaults.compaction.backgroundInfo"),
        keywords: "compaction compact background experimental precompute summary background_compaction",
    },
    SearchItem {
        id: "sessions.worktree-archive-on-done",
        page: Page::Sessions,
        title: "settings.openchamber.finishedWorktrees.field.archiveOnDone",
        description: Some("settings.openchamber.finishedWorktrees.info"),
        keywords: "done finish in work worktree archive cleanup remove",
    },
    SearchItem {
        id: "sessions.merged-worktree-cleanup",
        page: Page::Sessions,
        title: "settings.openchamber.mergedWorktreeCleanup.field.enable",
        description: Some("settings.openchamber.finishedWorktrees.info"),
        keywords: "pr pull request merged worktree branch archive cleanup remove",
    },
    SearchItem {
        id: "shortcuts.keyboard-shortcuts",
        page: Page::Shortcuts,
        title: "settings.openchamber.keyboardShortcuts.title",
        description: Some("settings.openchamber.keyboardShortcuts.tooltip"),
        keywords: "keyboard hotkeys bindings",
    },
    SearchItem {
        id: "notifications.delivery",
        page: Page::Notifications,
        title: "settings.notifications.page.delivery.title",
        description: None,
        keywords: "desktop notifications system notifications",
    },
    SearchItem {
        id: "notifications.events",
        page: Page::Notifications,
        title: "settings.notifications.page.events.title",
        description: None,
        keywords: "completion subtasks errors questions",
    },
    SearchItem {
        id: "notifications.pullRequests",
        page: Page::Notifications,
        title: "settings.notifications.pullRequests.label",
        description: Some("settings.notifications.pullRequests.info"),
        keywords: "pull requests merge queue auto-merge checks github",
    },
    SearchItem {
        id: "voice.speech-recognition",
        page: Page::Voice,
        title: "settings.voice.page.section.speechRecognition",
        description: None,
        keywords: "stt dictation voice input transcribe whisper parakeet qwen microphone misheard correct fix small model",
    },
    SearchItem {
        id: "voice.dictation-auto-send",
        page: Page::Voice,
        title: "settings.voice.page.field.dictationAutoSend",
        description: Some("settings.voice.page.field.dictationAutoSendInfo"),
        keywords: "dictation voice input send submit automatic",
    },
    SearchItem {
        id: "usage.model-quotas",
        page: Page::Usage,
        title: "settings.usage.page.section.modelQuotas",
        description: None,
        keywords: "models quota limits tokens billing usage",
    },
    SearchItem {
        id: "behavior.system-prompt",
        page: Page::Behavior,
        title: "settings.behavior.page.section.systemPrompt",
        description: Some("settings.behavior.page.warning.title"),
        keywords: "agents.md global instructions system prompt",
    },
    SearchItem {
        id: "behavior.response-style",
        page: Page::Behavior,
        title: "settings.behavior.page.section.responseStyle",
        description: Some("settings.behavior.page.responseStyle.tooltip"),
        keywords: "tone concise detailed custom instructions",
    },
    SearchItem {
        id: "snippets.create",
        page: Page::Snippets,
        title: "settings.snippets.sidebar.actions.create",
        description: None,
        keywords: "add new snippet",
    },
    SearchItem {
        id: "snippets.content",
        page: Page::Snippets,
        title: "settings.snippets.page.field.content",
        description: None,
        keywords: "markdown prompt template",
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
        assert_eq!(search("steer", &en)[0].id, "chat.follow-up-behavior");
        assert_eq!(search("safety net", &en)[0].page, Page::Sessions);
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
        let groups: Vec<_> = Page::ALL.iter().map(|page| page.group()).collect();
        assert!(
            groups.windows(2).filter(|pair| pair[0] != pair[1]).count() + 1
                == groups
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
            "each navigation group is contiguous"
        );
    }
}
