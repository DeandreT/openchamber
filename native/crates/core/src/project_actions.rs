//! A project's actions as the header's action button runs them
//! (`useProjectActionsRunner.ts`, `lib/projectActions.ts`,
//! `lib/detectDevServer.ts`, `lib/projectActionTerminal.ts`).

/// The ID of the built-in action that detects the dev server command.
pub const AUTO_DISCOVER_ID: &str = "__openchamber_auto_discover_preview__";

/// Action icon keys and the sprite glyphs they name (`PROJECT_ACTION_ICONS`).
const ICONS: [(&str, &str); 44] = [
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
    ("deploy", "upload-cloud-2"),
    ("download", "download-cloud-2"),
    ("test", "test-tube"),
    ("database", "database-2"),
    ("mobile", "smartphone"),
    ("desktop", "computer"),
    ("web", "global"),
    ("package", "box-3"),
    ("cloud", "cloud"),
    ("security", "shield-check"),
    ("speed", "speed-up"),
    ("notify", "notification-3"),
    ("format", "brush"),
    ("clean", "delete-bin-2"),
    ("watch", "eye"),
    ("loop", "loop-left"),
    ("timer", "timer"),
    ("merge", "git-merge"),
    ("lock", "lock"),
    ("archive", "archive"),
    ("sparkle", "sparkling-2"),
    ("npm", "npmjs"),
    ("apple", "apple"),
    ("android", "android"),
    ("windows", "windows"),
    ("linux", "ubuntu"),
];

/// Where a project's setup came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionSource {
    /// The user's own file on this instance.
    Personal,
    /// The repository's `.openchamber/project.json`, which runs only after the
    /// shared-setup trust prompt.
    Shared,
}

/// A configured action of a project, or the auto-discovery entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectAction {
    pub id: String,
    pub name: String,
    /// An icon key; an unknown or absent one shows `play`.
    pub icon: Option<String>,
    pub command: String,
    /// `runIn: 'parent'`: the project's own checkout, not the session's
    /// worktree.
    pub run_in_parent: bool,
    pub source: ActionSource,
}

impl ProjectAction {
    /// The auto-discovery entry the button and menu offer after the
    /// project's own actions; its command is found when it runs.
    pub fn auto_discover(name: String) -> Self {
        Self {
            id: AUTO_DISCOVER_ID.to_owned(),
            name,
            icon: Some("scan-2".to_owned()),
            command: String::new(),
            run_in_parent: false,
            source: ActionSource::Personal,
        }
    }

    pub fn is_auto_discover(&self) -> bool {
        self.id == AUTO_DISCOVER_ID
    }

    /// The sprite glyph of the action (`resolveProjectActionIconName`).
    pub fn glyph(&self) -> &'static str {
        if self.is_auto_discover() {
            return "scan-2";
        }
        self.icon
            .as_deref()
            .and_then(|key| ICONS.iter().find(|(known, _)| *known == key))
            .map_or("play", |(_, glyph)| glyph)
    }
}

/// A project's merged setup as the action button needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectSetup {
    /// Shared actions first, then personal ones, in the server's order.
    pub actions: Vec<ProjectAction>,
    /// The repository's shared commands when this instance has not trusted
    /// their current version.
    pub untrusted: Option<SharedTrust>,
}

/// What the shared-setup trust prompt shows and records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedTrust {
    /// The hash of the shared commands; trusting records it.
    pub hash: String,
    /// Where the shared file lives in the repository.
    pub path: String,
    pub setup_commands: Vec<String>,
    /// Each shared action's name and command.
    pub actions: Vec<(String, String)>,
}

/// The action the header button shows: a running one wins over the stored
/// pick, so the button is where it gets stopped; then the pick, then the
/// first action. A pick that names no action falls back the same way.
pub fn shown<'a>(
    actions: &'a [ProjectAction],
    picked: Option<&str>,
    is_running: impl Fn(&ProjectAction) -> bool,
) -> Option<&'a ProjectAction> {
    let picked = picked.and_then(|id| actions.iter().find(|action| action.id == id));
    picked
        .filter(|action| is_running(action))
        .or_else(|| actions.iter().find(|action| is_running(action)))
        .or(picked)
        .or_else(|| actions.first())
}

/// The command as the terminal receives it (`normalizeProjectActionCommand`):
/// trimmed, CRLF and CR turned into LF, and C0 controls other than tab, LF
/// and CR removed, as DEL is.
pub fn normalize_command(command: &str) -> String {
    command
        .trim()
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|&character| {
            let code = u32::from(character);
            !(code <= 8 || code == 11 || code == 12 || (14..=31).contains(&code) || code == 127)
        })
        .collect()
}

/// The package manager a project's scripts run with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackageManager {
    Npm,
    Yarn,
    Pnpm,
    Bun,
}

impl PackageManager {
    /// `packageManager` in `package.json` names it first.
    pub fn from_package_field(value: &str) -> Option<Self> {
        let value = value.to_ascii_lowercase();
        [
            ("bun@", Self::Bun),
            ("pnpm@", Self::Pnpm),
            ("yarn@", Self::Yarn),
            ("npm@", Self::Npm),
        ]
        .into_iter()
        .find_map(|(prefix, manager)| value.starts_with(prefix).then_some(manager))
    }

    /// Then the first lock file present, in this order; else npm.
    pub const LOCK_FILES: [(&'static str, Self); 5] = [
        ("bun.lock", Self::Bun),
        ("bun.lockb", Self::Bun),
        ("pnpm-lock.yaml", Self::Pnpm),
        ("yarn.lock", Self::Yarn),
        ("package-lock.json", Self::Npm),
    ];

    fn run_prefix(self) -> &'static str {
        match self {
            Self::Npm => "npm run",
            Self::Yarn => "yarn",
            Self::Pnpm => "pnpm",
            Self::Bun => "bun run --shell=bun",
        }
    }
}

/// What auto-discovery runs: a dev-server action, or a dev script with the
/// project's package manager.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevServer {
    pub command: String,
    /// The configured action it reuses, which keeps that action's trust.
    pub action: Option<ProjectAction>,
}

/// Whether a script name is a dev server's (`DEV_COMMAND_PATTERNS`), and how
/// early it ranks: `dev`, `start`, `start:dev|preview|serve`, `preview`,
/// `serve`, `develop`, each with an optional `:suffix` where the web allows one.
fn dev_script_rank(name: &str) -> Option<usize> {
    let name = name.to_ascii_lowercase();
    let with_suffix = |base: &str| {
        name == base
            || name
                .strip_prefix(base)
                .is_some_and(|rest| rest.starts_with(':'))
    };
    if with_suffix("dev") {
        return Some(0);
    }
    if name == "start" {
        return Some(1);
    }
    if ["start:dev", "start:preview", "start:serve"]
        .iter()
        .any(|base| with_suffix(base))
    {
        return Some(2);
    }
    ["preview", "serve", "develop"]
        .iter()
        .position(|base| with_suffix(base))
        .map(|index| index + 3)
}

/// The script an action's command runs through a package manager
/// (`DEV_SCRIPT_COMMAND_PATTERN`), after an optional `cd <dir> &&`.
fn script_of(command: &str) -> Option<&str> {
    let mut rest = command.trim();
    if let Some(after) = strip_word(rest, "cd") {
        let after = after.trim_start();
        let directory_end = match after.chars().next()? {
            quote @ ('"' | '\'') => after[1..].find(quote)? + 2,
            _ => after
                .find(|character: char| character.is_whitespace() || ";&|".contains(character))
                .unwrap_or(after.len()),
        };
        rest = after[directory_end..]
            .trim_start()
            .strip_prefix("&&")?
            .trim_start();
    }
    let manager = ["npm", "yarn", "pnpm", "bun"]
        .into_iter()
        .find_map(|manager| strip_word(rest, manager))?;
    let mut rest = manager.trim_start();
    if let Some(after) = strip_word(rest, "run") {
        rest = after.trim_start();
    }
    if let Some(after) = strip_word(rest, "--shell=bun") {
        rest = after.trim_start();
    }
    let script = rest.split(char::is_whitespace).next()?;
    (!script.is_empty() && !script.contains([';', '&', '|'])).then_some(script)
}

/// `word` at the start of `text`, case-insensitive, followed by whitespace.
fn strip_word<'a>(text: &'a str, word: &str) -> Option<&'a str> {
    let head = text.get(..word.len())?;
    let rest = &text[word.len()..];
    (head.eq_ignore_ascii_case(word) && rest.starts_with(char::is_whitespace)).then_some(rest)
}

/// `DEV_ACTION_NAME_PATTERN`: "dev", "development", "preview" or "serve",
/// with an optional " server"; or "start"/"run" [dev|development] then
/// "server" or "preview"; each with an optional " (…)" suffix.
fn is_dev_action_name(name: &str) -> bool {
    let mut name = name.trim().to_ascii_lowercase();
    if name.ends_with(')')
        && let Some(open) = name.rfind(" (")
        && name.len() > open + 3
        && !name[open + 2..name.len() - 1].contains(')')
    {
        name.truncate(open);
    }
    let words: Vec<&str> = name.split_whitespace().collect();
    let dev = |word: &str| word == "dev" || word == "development";
    match words.as_slice() {
        ["start" | "run", "server" | "preview"] => true,
        ["start" | "run", middle, "server" | "preview"] => dev(middle),
        [first] | [first, "server"] => dev(first) || *first == "preview" || *first == "serve",
        _ => false,
    }
}

/// `detectDevServerCommand` up to the static-site fallback: a configured
/// action that runs a dev script, or a personal one named like a dev server,
/// else the first dev script by pattern order.
pub fn dev_server(
    actions: &[ProjectAction],
    scripts: &[String],
    manager: PackageManager,
) -> Option<DevServer> {
    let action = actions.iter().find(|action| {
        script_of(&action.command).is_some_and(|script| dev_script_rank(script).is_some())
            || (action.source != ActionSource::Shared && is_dev_action_name(&action.name))
    });
    if let Some(action) = action {
        return Some(DevServer {
            command: action.command.clone(),
            action: Some(action.clone()),
        });
    }
    let script = scripts
        .iter()
        .filter_map(|script| dev_script_rank(script).map(|rank| (rank, script)))
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, script)| script)?;
    Some(DevServer {
        command: format!("{} {script}", manager.run_prefix()),
        action: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(id: &str, icon: Option<&str>) -> ProjectAction {
        ProjectAction {
            id: id.to_owned(),
            name: id.to_owned(),
            icon: icon.map(str::to_owned),
            command: format!("echo {id}"),
            run_in_parent: false,
            source: ActionSource::Personal,
        }
    }

    #[test]
    fn a_running_action_wins_then_the_pick_then_the_first() {
        let actions = [
            action("dev", None),
            action("test", None),
            action("lint", None),
        ];
        let none = |_: &ProjectAction| false;
        assert_eq!(
            shown(&actions, None, none).map(|a| a.id.as_str()),
            Some("dev")
        );
        assert_eq!(
            shown(&actions, Some("lint"), none).map(|a| a.id.as_str()),
            Some("lint")
        );
        assert_eq!(
            shown(&actions, Some("gone"), none).map(|a| a.id.as_str()),
            Some("dev"),
            "a stale pick falls back to the first action"
        );
        let test_runs = |a: &ProjectAction| a.id == "test";
        assert_eq!(
            shown(&actions, Some("lint"), test_runs).map(|a| a.id.as_str()),
            Some("test")
        );
        let both_run = |a: &ProjectAction| a.id != "dev";
        assert_eq!(
            shown(&actions, Some("lint"), both_run).map(|a| a.id.as_str()),
            Some("lint"),
            "a running pick stays shown"
        );
        assert_eq!(shown(&[], None, none), None);
    }

    #[test]
    fn icons_resolve_by_key_and_default_to_play() {
        assert_eq!(
            ProjectAction::auto_discover("Auto".into()).glyph(),
            "scan-2"
        );
        assert_eq!(action("a", Some("build")).glyph(), "hammer");
        assert_eq!(action("a", Some("linux")).glyph(), "ubuntu");
        assert_eq!(action("a", Some("nope")).glyph(), "play");
        assert_eq!(action("a", None).glyph(), "play");
    }

    #[test]
    fn commands_lose_control_characters_and_carriage_returns() {
        assert_eq!(
            normalize_command("  bun dev\r\necho\u{7}\tok\rdone\u{7f}\n "),
            "bun dev\necho\tok\ndone"
        );
        assert_eq!(normalize_command(" \u{1b}\n"), "");
    }

    #[test]
    fn dev_scripts_rank_by_pattern_order() {
        let scripts = ["build", "serve", "start:watch", "start", "dev:web"]
            .map(str::to_owned)
            .to_vec();
        let found = dev_server(&[], &scripts, PackageManager::Bun).expect("a dev script");
        assert_eq!(found.command, "bun run --shell=bun dev:web");
        assert_eq!(found.action, None);
        let start = ["start:watch", "start"].map(str::to_owned).to_vec();
        assert_eq!(
            dev_server(&[], &start, PackageManager::Npm).map(|found| found.command),
            Some("npm run start".to_owned()),
            "unrelated start:* scripts never launch"
        );
        assert!(dev_server(&[], &["start:watch".to_owned()], PackageManager::Npm).is_none());
    }

    #[test]
    fn actions_that_run_dev_scripts_or_are_named_so_are_reused() {
        let mut shared = action("shared", None);
        shared.source = ActionSource::Shared;
        shared.name = "Dev server".to_owned();
        shared.command = "make".to_owned();
        let mut scripted = action("web", None);
        scripted.command = "cd \"apps/web\" && pnpm run dev --port 3000".to_owned();
        let mut named = action("named", None);
        named.name = "Start dev server (web)".to_owned();
        named.command = "./run.sh".to_owned();

        assert_eq!(
            dev_server(std::slice::from_ref(&shared), &[], PackageManager::Npm),
            None,
            "a shared action needs a matching script command"
        );
        let found = dev_server(
            &[shared.clone(), scripted.clone()],
            &[],
            PackageManager::Npm,
        )
        .expect("the scripted action");
        assert_eq!(found.action.map(|a| a.id), Some("web".to_owned()));
        assert_eq!(found.command, scripted.command);
        assert_eq!(
            dev_server(&[named], &[], PackageManager::Npm).and_then(|f| f.action.map(|a| a.id)),
            Some("named".to_owned())
        );
        for name in [
            "dev",
            "Development server",
            "preview",
            "run preview",
            "Start server",
            "Serve (docs)",
        ] {
            assert!(is_dev_action_name(name), "{name}");
        }
        for name in ["start", "deploy server", "dev tools", "start the server"] {
            assert!(!is_dev_action_name(name), "{name}");
        }
        assert_eq!(script_of("bun --shell=bun preview:x"), Some("preview:x"));
        assert_eq!(script_of("yarn start"), Some("start"));
        assert_eq!(script_of("npx start"), None);
    }

    #[test]
    fn the_package_field_names_the_manager() {
        assert_eq!(
            PackageManager::from_package_field("pnpm@9.1.0"),
            Some(PackageManager::Pnpm)
        );
        assert_eq!(PackageManager::from_package_field("deno@1"), None);
    }
}
