//! The header's Open In control: the file manager, which opens the project
//! folder (`OpenInAppButton.tsx`).

use std::path::Path;

/// The app's label, as `OPEN_IN_APPS` names the file manager per platform.
#[cfg(target_os = "windows")]
pub(crate) const LABEL: &str = "Explorer";
#[cfg(target_os = "macos")]
pub(crate) const LABEL: &str = "Finder";
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
pub(crate) const LABEL: &str = "File Manager";

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
