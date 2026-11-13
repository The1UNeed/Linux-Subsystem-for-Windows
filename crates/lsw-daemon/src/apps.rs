//! Windows apps as Linux apps: one `.desktop` launcher per installed
//! Windows app, named after the app's Linux `app_id` so desktops match its
//! windows to it (dock/taskbar grouping, icons).

use lsw_proto::{AppEntry, linux_app_id};
use std::path::{Path, PathBuf};

/// Desktop category shared by all LSW launchers (the "Windows Apps"
/// folder collects it).
pub const CATEGORY: &str = "X-LSW-Windows";

/// Quote one argument for a desktop entry `Exec` key (Desktop Entry
/// Specification, "The Exec key"), including the second level of escaping
/// that applies to every string value.
pub fn exec_arg(arg: &str) -> String {
    const RESERVED: &[char] = &[
        ' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(',
        ')', '`',
    ];
    let quoted = if arg.is_empty() || arg.contains(RESERVED) {
        let mut q = String::from("\"");
        for c in arg.chars() {
            if matches!(c, '"' | '`' | '$' | '\\') {
                q.push('\\');
            }
            q.push(c);
        }
        q.push('"');
        q
    } else {
        arg.to_owned()
    };
    // String-level escaping: backslashes are doubled once more.
    quoted.replace('\\', "\\\\")
}

/// Keep a value on one line (desktop entry values cannot contain newlines).
fn one_line(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// The desktop entry for `app`.
pub fn desktop_entry(app: &AppEntry, lsw: &Path, vm: &str, icon: Option<&Path>) -> String {
    let id = linux_app_id(&app.app);
    let exec = [
        exec_arg(&lsw.to_string_lossy()),
        "--name".into(),
        exec_arg(vm),
        "launch".into(),
        exec_arg(&app.launch),
    ]
    .join(" ");
    let mut out = format!(
        "[Desktop Entry]\nType=Application\nName={}\nComment=Windows app\nExec={exec}\nCategories=Utility;{CATEGORY};\nStartupWMClass={id}\nX-LSW-VM={}\n",
        one_line(&app.name),
        one_line(vm),
    );
    if let Some(icon) = icon {
        out.push_str(&format!("Icon={}\n", one_line(&icon.to_string_lossy())));
    }
    out
}

/// Write launchers for `apps` (and their icons) into `applications_dir`,
/// removing LSW launchers of apps no longer installed. Returns how many
/// launchers exist afterwards.
pub fn sync(
    apps: &[AppEntry],
    applications_dir: &Path,
    icons_dir: &Path,
    lsw: &Path,
    vm: &str,
) -> std::io::Result<usize> {
    std::fs::create_dir_all(applications_dir)?;
    std::fs::create_dir_all(icons_dir)?;
    let mut keep: Vec<PathBuf> = Vec::new();
    for app in apps {
        let id = linux_app_id(&app.app);
        let icon = if app.icon_png.starts_with(b"\x89PNG") {
            let p = icons_dir.join(format!("{id}.png"));
            std::fs::write(&p, &app.icon_png)?;
            Some(p)
        } else {
            None
        };
        let file = applications_dir.join(format!("{id}.desktop"));
        if keep.contains(&file) {
            continue; // several Start entries for one app: the first wins
        }
        let tmp = file.with_extension("desktop.tmp");
        std::fs::write(&tmp, desktop_entry(app, lsw, vm, icon.as_deref()))?;
        std::fs::rename(&tmp, &file)?;
        keep.push(file);
    }
    for entry in std::fs::read_dir(applications_dir)?.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let ours = name.starts_with("lsw.")
            && name.ends_with(".desktop")
            && std::fs::read_to_string(&path)
                .is_ok_and(|s| s.contains(&format!("X-LSW-VM={vm}\n")));
        if ours && !keep.contains(&path) {
            let _ = std::fs::remove_file(&path);
        }
    }
    Ok(keep.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_quoting_follows_the_spec() {
        assert_eq!(exec_arg("launch"), "launch");
        assert_eq!(exec_arg("a b"), "\"a b\"");
        // A backslash: escaped inside the quotes, then doubled for the
        // string level, so the file holds four.
        assert_eq!(
            exec_arg(r"{GUID}\notepad.exe"),
            r#""{GUID}\\\\notepad.exe""#
        );
        assert_eq!(exec_arg("x\"$`"), "\"x\\\\\"\\\\$\\\\`\"");
        assert_eq!(
            exec_arg("Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"),
            "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App"
        );
    }

    fn app(launch: &str, name: &str) -> AppEntry {
        AppEntry {
            launch: launch.into(),
            name: name.into(),
            app: launch.into(),
            icon_png: b"\x89PNG fake".to_vec(),
        }
    }

    #[test]
    fn entries_match_window_app_ids_and_sync_removes_stale_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let (apps_dir, icons) = (tmp.path().join("applications"), tmp.path().join("icons"));
        let calc = app(
            "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App",
            "Calculator",
        );
        let paint = app("Microsoft.Paint_8wekyb3d8bbwe!App", "Paint\nInjected=1");
        let n = sync(
            &[calc.clone(), paint],
            &apps_dir,
            &icons,
            Path::new("/usr/bin/lsw"),
            "windows",
        )
        .unwrap();
        assert_eq!(n, 2);
        let id = linux_app_id(&calc.app);
        let text = std::fs::read_to_string(apps_dir.join(format!("{id}.desktop"))).unwrap();
        assert!(text.contains("Name=Calculator\n"));
        assert!(text.contains(&format!("StartupWMClass={id}\n")));
        assert!(text.contains("Exec=/usr/bin/lsw --name windows launch Microsoft.WindowsCalculator_8wekyb3d8bbwe!App\n"));
        assert!(text.contains(&format!(
            "Icon={}",
            icons.join(format!("{id}.png")).display()
        )));
        let paint_id = linux_app_id("Microsoft.Paint_8wekyb3d8bbwe!App");
        let paint_text =
            std::fs::read_to_string(apps_dir.join(format!("{paint_id}.desktop"))).unwrap();
        assert!(
            paint_text.contains("Name=PaintInjected=1\n"),
            "newlines stripped"
        );
        // A foreign launcher in the same directory is left alone.
        std::fs::write(apps_dir.join("org.gnome.Nautilus.desktop"), "x").unwrap();
        sync(
            &[calc],
            &apps_dir,
            &icons,
            Path::new("/usr/bin/lsw"),
            "windows",
        )
        .unwrap();
        assert!(!apps_dir.join(format!("{paint_id}.desktop")).exists());
        assert!(apps_dir.join("org.gnome.Nautilus.desktop").exists());
    }
}
