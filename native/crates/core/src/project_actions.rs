//! A project's actions as the work status card's action row shows them
//! (`useProjectActionsRunner.ts`, `lib/projectActions.ts`).

/// Action icon keys and the sprite glyphs they name (`PROJECT_ACTION_ICONS`).
const ICONS: [(&str, &str); 18] = [
    ("play", "play"),
    ("build", "hammer"),
    ("lint", "checkbox-circle"),
    ("terminal", "terminal-box"),
    ("tools", "tools"),
    ("bug", "bug"),
    ("flask", "flask"),
    ("rocket", "rocket"),
    ("code", "code"),
    ("server", "server"),
    ("branch", "git-branch"),
    ("search", "search"),
    ("settings", "settings-3"),
    ("brain", "brain-ai-3"),
    ("stack", "stack"),
    ("robot", "robot-2"),
    ("command", "command"),
    ("file", "file-text"),
];

/// A configured action of a project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectAction {
    pub id: String,
    pub name: String,
    /// An icon key; an unknown or absent one shows `play`.
    pub icon: Option<String>,
}

/// What the action row shows with nothing picked yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShownAction<'a> {
    /// The project's first action.
    Configured(&'a ProjectAction),
    /// Dev server detection, when the project configures no action.
    AutoDiscover,
}

impl ShownAction<'_> {
    /// The project's own actions come first, so the row offers the first of
    /// them; auto-discovery is the fallback.
    pub fn of(actions: &[ProjectAction]) -> ShownAction<'_> {
        actions
            .first()
            .map_or(ShownAction::AutoDiscover, ShownAction::Configured)
    }

    /// The sprite glyph of the action (`resolveProjectActionIconName`).
    pub fn icon(&self) -> &'static str {
        match self {
            ShownAction::AutoDiscover => "scan-2",
            ShownAction::Configured(action) => action
                .icon
                .as_deref()
                .and_then(|key| ICONS.iter().find(|(known, _)| *known == key))
                .map_or("play", |(_, glyph)| glyph),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(id: &str, icon: Option<&str>) -> ProjectAction {
        ProjectAction {
            id: id.to_owned(),
            name: id.to_owned(),
            icon: icon.map(str::to_owned),
        }
    }

    #[test]
    fn the_first_configured_action_comes_before_auto_discovery() {
        assert_eq!(ShownAction::of(&[]), ShownAction::AutoDiscover);
        let actions = [action("dev", Some("rocket")), action("test", None)];
        assert_eq!(
            ShownAction::of(&actions),
            ShownAction::Configured(&actions[0])
        );
    }

    #[test]
    fn icons_resolve_by_key_and_default_to_play() {
        assert_eq!(ShownAction::AutoDiscover.icon(), "scan-2");
        assert_eq!(
            ShownAction::Configured(&action("a", Some("build"))).icon(),
            "hammer"
        );
        assert_eq!(
            ShownAction::Configured(&action("a", Some("nope"))).icon(),
            "play"
        );
        assert_eq!(ShownAction::Configured(&action("a", None)).icon(), "play");
    }
}
