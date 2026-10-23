//! The work status card's open-in-app control: the file manager, which
//! opens the project folder, with the icon the desktop shows for it
//! (`OpenInAppButton.tsx`).
//!
//! On Linux the icon is found as `packages/electron/linux-app-discovery.mjs`
//! finds it: the default handler of `inode/directory` (else a known file
//! manager), its desktop entry's icon, then the generic file-manager icons,
//! each looked up in the icon themes and pixmaps. Only PNG files are used, as
//! the web shows only those; without one the control shows the label's
//! initial. Other platforms show the initial until their lookups are ported.

use std::path::{Path, PathBuf};

/// The app's label, as `OPEN_IN_APPS` names the file manager per platform.
#[cfg(target_os = "windows")]
pub(crate) const LABEL: &str = "Explorer";
#[cfg(target_os = "macos")]
pub(crate) const LABEL: &str = "Finder";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) const LABEL: &str = "File Manager";

/// The label's initial, which stands in for a missing icon.
pub(crate) fn initial() -> String {
    LABEL
        .trim()
        .chars()
        .next()
        .map_or_else(|| "?".to_owned(), |first| first.to_uppercase().collect())
}

/// The file manager's icon file, when the desktop has a PNG for it.
#[cfg(target_os = "linux")]
pub(crate) fn file_manager_icon() -> Option<PathBuf> {
    let locations = linux::Locations::from_env();
    let entries = linux::desktop_entries(&locations);
    let default_id = linux::default_file_manager_id();
    let entry = linux::file_manager_entry(&entries, default_id.as_deref());
    let mut names: Vec<&str> = entry
        .and_then(|entry| entry.icon.as_deref())
        .into_iter()
        .collect();
    names.extend(linux::FILE_MANAGER_ICON_FALLBACKS);
    linux::png_icon(&names, &locations)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn file_manager_icon() -> Option<PathBuf> {
    None
}

/// Opens `directory` in the file manager, as Electron opens a path with the
/// platform's default handler.
pub(crate) fn open_directory(directory: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    let program = "xdg-open";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "windows")]
    let program = "explorer";
    std::process::Command::new(program)
        .arg(directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashSet;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    const DEFAULT_DATA_DIRS: [&str; 2] = ["/usr/local/share", "/usr/share"];
    const FILE_MANAGER_IDS: [&str; 10] = [
        "org.gnome.Nautilus",
        "org.xfce.thunar",
        "thunar",
        "nemo",
        "org.kde.dolphin",
        "dolphin",
        "pcmanfm",
        "caja",
        "nautilus",
        "xfce4-file-manager",
    ];
    pub(super) const FILE_MANAGER_ICON_FALLBACKS: [&str; 4] = [
        "system-file-manager",
        "org.xfce.thunar",
        "org.gnome.Nautilus",
        "folder",
    ];
    /// Mid sizes first: the icon is drawn at 16 px from a PNG.
    const ICON_SIZES: [&str; 17] = [
        "48x48", "48", "32x32", "32", "64x64", "64", "24x24", "24", "22x22", "22", "16x16", "16",
        "128x128", "128", "256x256", "256", "scalable",
    ];
    const ICON_CATEGORIES: [&str; 6] =
        ["apps", "places", "status", "devices", "mimetypes", "legacy"];
    const XDG_MIME_TIMEOUT: Duration = Duration::from_millis(1500);

    /// The XDG base directories the lookups search.
    pub(super) struct Locations {
        data_home: PathBuf,
        data_dirs: Vec<PathBuf>,
        home: PathBuf,
    }

    impl Locations {
        pub(super) fn from_env() -> Self {
            let variable = |name: &str| {
                std::env::var(name)
                    .ok()
                    .map(|value| value.trim().to_owned())
                    .filter(|value| !value.is_empty())
            };
            let home = PathBuf::from(variable("HOME").unwrap_or_default());
            let data_home = variable("XDG_DATA_HOME")
                .map_or_else(|| home.join(".local").join("share"), PathBuf::from);
            let data_dirs = variable("XDG_DATA_DIRS").map_or_else(
                || DEFAULT_DATA_DIRS.iter().map(PathBuf::from).collect(),
                |dirs| {
                    dirs.split(':')
                        .filter(|dir| !dir.is_empty())
                        .map(PathBuf::from)
                        .collect()
                },
            );
            Self {
                data_home,
                data_dirs,
                home,
            }
        }

        fn joined(&self, leaf: &str, extra: &[PathBuf]) -> Vec<PathBuf> {
            let mut dirs = vec![self.data_home.join(leaf)];
            dirs.extend(extra.iter().cloned());
            dirs.extend(self.data_dirs.iter().map(|dir| dir.join(leaf)));
            unique(dirs)
        }
    }

    fn unique(paths: Vec<PathBuf>) -> Vec<PathBuf> {
        let mut seen = HashSet::new();
        paths
            .into_iter()
            .filter(|path| !path.as_os_str().is_empty() && seen.insert(path.clone()))
            .collect()
    }

    /// The fields of a desktop entry the lookup reads.
    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct DesktopEntry {
        pub(super) id: String,
        pub(super) name: String,
        pub(super) icon: Option<String>,
        categories: Vec<String>,
    }

    /// The `[Desktop Entry]` group's unlocalized values; `None` for an entry
    /// that is not a shown application with a name and a command.
    pub(super) fn parse_desktop_entry(content: &str, id: &str) -> Option<DesktopEntry> {
        let mut values: Vec<(String, String)> = Vec::new();
        let mut group = "";
        for line in content.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(name) = line
                .strip_prefix('[')
                .and_then(|line| line.strip_suffix(']'))
            {
                group = name.trim();
                continue;
            }
            if group != "Desktop Entry" {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let key = key.trim();
            if key.is_empty() || key.contains('[') || values.iter().any(|(known, _)| known == key) {
                continue;
            }
            values.push((key.to_owned(), unescape(value)));
        }
        let value = |key: &str| {
            values
                .iter()
                .find(|(known, _)| known == key)
                .map(|(_, value)| value.trim())
        };
        let flag = |key: &str| value(key).is_some_and(|value| value.eq_ignore_ascii_case("true"));
        if value("Type").unwrap_or("Application") != "Application"
            || flag("NoDisplay")
            || flag("Hidden")
        {
            return None;
        }
        let name = value("Name").filter(|name| !name.is_empty())?;
        let exec = value("Exec").filter(|exec| !exec.is_empty())?;
        if strip_field_codes(exec).is_empty() {
            return None;
        }
        Some(DesktopEntry {
            id: if id.is_empty() {
                name.to_owned()
            } else {
                id.to_owned()
            },
            name: name.to_owned(),
            icon: value("Icon")
                .filter(|icon| !icon.is_empty())
                .map(str::to_owned),
            categories: value("Categories")
                .unwrap_or_default()
                .split(';')
                .map(str::trim)
                .filter(|category| !category.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }

    fn unescape(value: &str) -> String {
        let mut output = String::with_capacity(value.len());
        let mut characters = value.chars();
        while let Some(character) = characters.next() {
            if character != '\\' {
                output.push(character);
                continue;
            }
            match characters.next() {
                Some('s') => output.push(' '),
                Some('n') => output.push('\n'),
                Some('t') => output.push('\t'),
                Some('r') => output.push('\r'),
                Some('\\') => output.push('\\'),
                Some(other) => {
                    output.push('\\');
                    output.push(other);
                }
                None => output.push('\\'),
            }
        }
        output
    }

    /// `Exec` without its `%` field codes; `%%` stays a percent sign.
    fn strip_field_codes(exec: &str) -> String {
        let mut output = String::with_capacity(exec.len());
        let mut characters = exec.chars();
        while let Some(character) = characters.next() {
            if character != '%' {
                output.push(character);
                continue;
            }
            if let Some('%') = characters.next() {
                output.push('%');
            }
        }
        output.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// Every shown application, the first entry of an ID winning, sorted by
    /// name.
    pub(super) fn desktop_entries(locations: &Locations) -> Vec<DesktopEntry> {
        let mut dirs = locations.joined("applications", &[]);
        dirs.extend([
            PathBuf::from("/usr/local/share/applications"),
            PathBuf::from("/usr/share/applications"),
        ]);
        let mut files = Vec::new();
        for dir in unique(dirs) {
            collect_desktop_files(&dir, &mut files);
        }
        let mut seen = HashSet::new();
        let mut entries = Vec::new();
        for file in files {
            let id = file
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| strip_desktop_suffix(name).to_owned())
                .unwrap_or_default();
            let Some(entry) = fs::read_to_string(&file)
                .ok()
                .and_then(|content| parse_desktop_entry(&content, &id))
            else {
                continue;
            };
            if seen.insert(entry.id.clone()) {
                entries.push(entry);
            }
        }
        entries.sort_by_key(|entry| entry.name.to_lowercase());
        entries
    }

    fn collect_desktop_files(dir: &Path, files: &mut Vec<PathBuf>) {
        let Ok(read) = fs::read_dir(dir) else {
            return;
        };
        let mut children: Vec<_> = read.flatten().collect();
        children.sort_by_key(|child| child.file_name());
        for child in children {
            let path = child.path();
            match child.file_type() {
                Ok(kind) if kind.is_dir() => collect_desktop_files(&path, files),
                Ok(kind)
                    if kind.is_file()
                        && path
                            .extension()
                            .is_some_and(|extension| extension.eq_ignore_ascii_case("desktop")) =>
                {
                    files.push(path);
                }
                _ => {}
            }
        }
    }

    fn strip_desktop_suffix(name: &str) -> &str {
        let lowered = name.to_ascii_lowercase();
        if lowered.ends_with(".desktop") {
            &name[..name.len() - ".desktop".len()]
        } else {
            name
        }
    }

    /// Lowercased, without `.desktop`, runs of other characters as one space.
    fn comparable(value: &str) -> String {
        let lowered = value.to_lowercase();
        let stripped = strip_desktop_suffix(&lowered);
        let mut output = String::new();
        let mut gap = false;
        for character in stripped.chars() {
            if character.is_ascii_lowercase() || character.is_ascii_digit() {
                if gap && !output.is_empty() {
                    output.push(' ');
                }
                gap = false;
                output.push(character);
            } else {
                gap = true;
            }
        }
        output
    }

    /// The handler `xdg-mime` names for folders, without `.desktop`.
    pub(super) fn default_file_manager_id() -> Option<String> {
        let mut child = Command::new("xdg-mime")
            .args(["query", "default", "inode/directory"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + XDG_MIME_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        let output = child.wait_with_output().ok()?;
        let id = String::from_utf8(output.stdout).ok()?;
        let id = strip_desktop_suffix(id.trim());
        (!id.is_empty()).then(|| id.to_owned())
    }

    /// The default file manager's entry, else a known one, else any entry in
    /// a file manager category.
    pub(super) fn file_manager_entry<'a>(
        entries: &'a [DesktopEntry],
        default_id: Option<&str>,
    ) -> Option<&'a DesktopEntry> {
        let by_id = |id: &str| {
            entries
                .iter()
                .find(|entry| entry.id == id || comparable(&entry.id) == comparable(id))
        };
        default_id
            .and_then(by_id)
            .or_else(|| FILE_MANAGER_IDS.iter().find_map(|id| by_id(id)))
            .or_else(|| {
                entries.iter().find(|entry| {
                    entry
                        .categories
                        .iter()
                        .any(|category| category == "FileManager" || category == "FileTools")
                })
            })
    }

    /// The first of `names` whose icon file is a PNG.
    pub(super) fn png_icon(names: &[&str], locations: &Locations) -> Option<PathBuf> {
        let mut seen = HashSet::new();
        names
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty() && seen.insert(*name))
            .filter_map(|name| icon_file(name, locations))
            .find(|file| {
                file.extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
            })
    }

    /// The icon file an icon name resolves to: a path as given, else the
    /// first theme that has it (`hicolor` first), else a pixmap.
    fn icon_file(name: &str, locations: &Locations) -> Option<PathBuf> {
        let given = Path::new(name);
        if (given.is_absolute() || name.contains('/')) && readable(given) {
            return Some(given.to_path_buf());
        }
        let base = match name.rsplit_once('.') {
            Some((stem, extension))
                if ["png", "svg", "xpm", "ico"]
                    .iter()
                    .any(|known| extension.eq_ignore_ascii_case(known)) =>
            {
                stem
            }
            _ => name,
        };
        let mut roots = locations.joined("icons", &[locations.home.join(".icons")]);
        roots.extend([
            PathBuf::from("/usr/local/share/icons"),
            PathBuf::from("/usr/share/icons"),
        ]);
        for root in unique(roots) {
            for theme in theme_names(&root) {
                if let Some(file) = icon_in_theme(&root.join(theme), base) {
                    return Some(file);
                }
            }
        }
        let mut pixmaps = locations.joined("pixmaps", &[]);
        pixmaps.extend([
            PathBuf::from("/usr/share/pixmaps"),
            PathBuf::from("/usr/local/share/pixmaps"),
        ]);
        for dir in unique(pixmaps) {
            for extension in ["png", "svg", "xpm"] {
                let candidate = dir.join(format!("{base}.{extension}"));
                if readable(&candidate) {
                    return Some(candidate);
                }
            }
        }
        None
    }

    fn theme_names(root: &Path) -> Vec<String> {
        let Ok(read) = fs::read_dir(root) else {
            return Vec::new();
        };
        let mut themes: Vec<String> = read
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .collect();
        themes.sort_by(|left, right| match (left.as_str(), right.as_str()) {
            ("hicolor", _) => std::cmp::Ordering::Less,
            (_, "hicolor") => std::cmp::Ordering::Greater,
            _ => left.to_lowercase().cmp(&right.to_lowercase()),
        });
        themes
    }

    /// A mid-size PNG first; else any PNG, scalable included; else an SVG.
    fn icon_in_theme(theme: &Path, name: &str) -> Option<PathBuf> {
        let mut png = None;
        let mut svg = None;
        for size in ICON_SIZES {
            for category in ICON_CATEGORIES {
                let dir = theme.join(size).join(category);
                let candidate = dir.join(format!("{name}.png"));
                if readable(&candidate) {
                    if size != "scalable" {
                        return Some(candidate);
                    }
                    png.get_or_insert(candidate);
                }
                let candidate = dir.join(format!("{name}.svg"));
                if svg.is_none() && readable(&candidate) {
                    svg = Some(candidate);
                }
            }
        }
        png.or(svg)
    }

    fn readable(path: &Path) -> bool {
        fs::File::open(path).is_ok()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn entries_read_the_unlocalized_desktop_group() {
            let entry = parse_desktop_entry(
                "[Desktop Entry]\nType=Application\nName[de]=Dateien\nName=Files\nExec=nautilus %U\nIcon=org.gnome.Nautilus\nCategories=GNOME;FileManager;\n[Desktop Action new]\nName=New\n",
                "org.gnome.Nautilus",
            )
            .expect("a shown application");
            assert_eq!(entry.name, "Files");
            assert_eq!(entry.icon.as_deref(), Some("org.gnome.Nautilus"));
            assert_eq!(entry.categories, ["GNOME", "FileManager"]);
            for hidden in [
                "[Desktop Entry]\nName=A\nExec=a\nNoDisplay=true\n",
                "[Desktop Entry]\nType=Link\nName=A\nExec=a\n",
                "[Desktop Entry]\nName=A\nExec=%U\n",
            ] {
                assert_eq!(parse_desktop_entry(hidden, "a"), None, "{hidden}");
            }
        }

        #[test]
        fn the_default_handler_wins_then_known_managers_then_categories() {
            let entry = |id: &str, categories: &[&str]| DesktopEntry {
                id: id.to_owned(),
                name: id.to_owned(),
                icon: None,
                categories: categories
                    .iter()
                    .map(|&category| category.to_owned())
                    .collect(),
            };
            let entries = [
                entry("custom", &["FileManager"]),
                entry("org.kde.dolphin", &[]),
                entry("org.gnome.Nautilus", &[]),
            ];
            let found = |default: Option<&str>| {
                file_manager_entry(&entries, default).map(|entry| entry.id.as_str())
            };
            assert_eq!(found(Some("org.kde.dolphin")), Some("org.kde.dolphin"));
            assert_eq!(found(Some("ORG.KDE.Dolphin")), Some("org.kde.dolphin"));
            assert_eq!(found(None), Some("org.gnome.Nautilus"));
            assert_eq!(found(Some("missing")), Some("org.gnome.Nautilus"));
            assert_eq!(
                file_manager_entry(&entries[..1], None).map(|entry| entry.id.as_str()),
                Some("custom")
            );
        }

        #[test]
        fn only_png_icons_are_used() {
            let root = std::env::temp_dir().join(format!("oc-icons-{}", std::process::id()));
            let theme = root.join("data").join("icons").join("hicolor");
            fs::create_dir_all(theme.join("48x48").join("apps")).expect("writable");
            fs::create_dir_all(theme.join("scalable").join("apps")).expect("writable");
            fs::write(
                theme.join("scalable").join("apps").join("vector.svg"),
                "<svg/>",
            )
            .expect("writable");
            fs::write(theme.join("48x48").join("apps").join("raster.png"), "png")
                .expect("writable");
            let locations = Locations {
                data_home: root.join("data"),
                data_dirs: Vec::new(),
                home: root.join("home"),
            };
            assert_eq!(
                png_icon(&["vector", "raster"], &locations),
                Some(theme.join("48x48").join("apps").join("raster.png")),
                "an SVG-only name is passed over"
            );
            assert_eq!(png_icon(&["vector"], &locations), None);
            let _ = fs::remove_dir_all(root);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_initial_stands_for_the_label() {
        assert_eq!(initial(), LABEL[..1].to_uppercase());
    }
}
