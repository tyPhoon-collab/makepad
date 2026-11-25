//! Read-only discovery of an explicitly installed CEF distribution.
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static WORKSPACE: OnceLock<(&'static Path, &'static str)> = OnceLock::new();

/// Register the embedding workspace and its CEF setup command before starting the host.
pub fn set_distribution_workspace(root: &'static Path, setup_command: &'static str) {
    WORKSPACE
        .set((root, setup_command))
        .expect("CEF distribution workspace already set");
}

pub(crate) fn platform() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("macosarm64"),
        ("macos", "x86_64") => Some("macosx64"),
        ("linux", "x86_64") => Some("linux64"),
        ("linux", "aarch64") => Some("linuxarm64"),
        ("windows", "x86_64") => Some("windows64"),
        ("windows", "aarch64") => Some("windowsarm64"),
        _ => None,
    }
}

fn pointer(prebuilt: &Path, platform: &str) -> Option<PathBuf> {
    if let Ok(text) = std::fs::read_to_string(prebuilt.join(format!("current-{platform}.txt"))) {
        let name = text.lines().next().unwrap_or("").trim();
        if !name.is_empty() {
            let dist = prebuilt.join(name);
            if dist.join("include").is_dir() {
                return Some(dist);
            }
        }
    }
    // Keep accepting the older symlink/directory pointer.
    let dist = prebuilt.join(format!("current-{platform}"));
    dist.join("include").is_dir().then_some(dist)
}

fn from_ancestors(start: &Path, platform: &str) -> Option<PathBuf> {
    for root in start.ancestors() {
        if let Some(dist) = pointer(&root.join("local/cef-prebuilt"), platform) {
            return Some(dist);
        }
        // Worktrees share the main checkout's installed distribution. Resolve
        // relative gitdir paths against the directory containing the .git file.
        if let Ok(text) = std::fs::read_to_string(root.join(".git")) {
            if let Some(gitdir) = text.trim().strip_prefix("gitdir:") {
                if let Ok(gitdir) = root.join(gitdir.trim()).canonicalize() {
                    if let Some(main) = gitdir
                        .ancestors()
                        .find(|path| path.file_name().is_some_and(|name| name == ".git"))
                        .and_then(Path::parent)
                    {
                        if let Some(dist) = pointer(&main.join("local/cef-prebuilt"), platform) {
                            return Some(dist);
                        }
                    }
                }
            }
        }
    }
    None
}

pub(crate) fn find() -> Result<PathBuf, String> {
    let platform = platform().ok_or("CEF distribution is unsupported on this platform")?;
    let setup_command = WORKSPACE.get().map_or_else(
        || format!("cargo run --release -p makepad-cef-fetch -- --fetch --platform {platform}"),
        |(_, command)| (*command).to_owned(),
    );
    let hint = format!(
        "Set MAKEPAD_CEF_DIST_DIR to an extracted distribution, or run `{setup_command}`."
    );
    let dist = if let Some(dir) = std::env::var_os("MAKEPAD_CEF_DIST_DIR") {
        let dir = PathBuf::from(dir);
        if !dir.join("include").is_dir() {
            return Err(format!(
                "{} is not a CEF distribution (missing include/). {hint}",
                dir.display()
            ));
        }
        dir
    } else {
        // Prefer the executable's installation when moved to another checkout.
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().and_then(|dir| from_ancestors(dir, platform)))
            .or_else(|| {
                std::env::var_os("MAKEPAD_WM_ROOT")
                    .map(PathBuf::from)
                    .and_then(|root| from_ancestors(&root, platform))
            })
            .or_else(|| WORKSPACE.get().and_then(|(root, _)| from_ancestors(root, platform)))
            .or_else(|| from_ancestors(Path::new(env!("CARGO_MANIFEST_DIR")), platform))
            .or_else(|| from_cargo_git_checkouts(platform))
            .ok_or_else(|| format!("No installed CEF distribution for {platform}. {hint}"))?
    };
    dist.canonicalize()
        .map_err(|err| format!("Cannot resolve {}: {err}. {hint}", dist.display()))
}

pub(crate) fn version() -> Option<String> {
    let header = std::fs::read_to_string(find().ok()?.join("include/cef_version.h")).ok()?;
    header.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == "#define" && fields.next()? == "CEF_VERSION").then(|| {
            fields
                .next()
                .map(|value| value.trim_matches('"').to_owned())
        })?
    })
}

fn cargo_home() -> Option<PathBuf> {
    std::env::var_os("CARGO_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".cargo"))
        })
}

fn parse_makepad_lock_rev(text: &str) -> Option<String> {
    let mut in_pkg = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            in_pkg = false;
        } else if line == r#"name = "makepad-widgets""# {
            in_pkg = true;
        } else if in_pkg {
            if let Some(src) = line.strip_prefix("source = ") {
                let hash = src.trim_matches('"').split('#').nth(1)?;
                return hash.get(..7).map(str::to_owned);
            }
        }
    }
    None
}

fn from_cargo_git_checkouts(platform: &str) -> Option<PathBuf> {
    let checkouts = cargo_home()?.join("git/checkouts");
    let target_rev = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .map(|p| p.join("Cargo.lock"))
        .find(|p| p.is_file())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|text| parse_makepad_lock_rev(&text));
    let entries = std::fs::read_dir(checkouts).ok()?;
    let mut fallback = None;
    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("makepad-") {
            continue;
        }
        let Ok(revs) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for rev_entry in revs.flatten() {
            let rev_path = rev_entry.path();
            if let Some(dist) = pointer(&rev_path.join("local/cef-prebuilt"), platform) {
                if target_rev.as_deref() == rev_entry.file_name().to_str() {
                    return Some(dist);
                }
                fallback.get_or_insert(dist);
            }
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::parse_makepad_lock_rev;

    #[test]
    fn parses_makepad_widgets_rev_from_cargo_lock() {
        let lock = r#"
[[package]]
name = "other"
source = "git+https://example.com/other#1234567890"

[[package]]
name = "makepad-widgets"
version = "1.0.0"
source = "git+https://github.com/makepad/makepad?branch=main#506ec051abcdef"
"#;
        assert_eq!(parse_makepad_lock_rev(lock).as_deref(), Some("506ec05"));
    }
}
