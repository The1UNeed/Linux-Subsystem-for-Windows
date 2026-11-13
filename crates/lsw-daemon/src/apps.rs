//! Windows apps as Linux apps: one `.desktop` launcher per installed
//! Windows app, named after the app's Linux `app_id` so desktops match its
//! windows to it (dock/taskbar grouping, icons).

use lsw_proto::{AppEntry, linux_app_id};
use std::path::{Path, PathBuf};

/// Desktop category shared by all LSW launchers (the "Windows Apps"
/// folder collects it).
pub const CATEGORY: &str = "X-LSW-Windows";

/// Longest app id or name accepted from the guest for a launcher.
const MAX_FIELD: usize = 512;

/// Whether a guest-provided string may appear in a launcher at all: no
/// control characters (a newline would start a new key, e.g. a second
/// `Exec=`), bounded length. Such apps are skipped, never "cleaned".
pub fn launcher_safe(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_FIELD && !s.chars().any(char::is_control)
}

/// Quote one argument for a desktop entry `Exec` key (Desktop Entry
/// Specification, "The Exec key"), including the second level of escaping
/// that applies to every string value. Returns `None` for arguments that
/// cannot be represented safely (control characters).
pub fn exec_arg(arg: &str) -> Option<String> {
    if arg.chars().any(char::is_control) {
        return None;
    }
    const RESERVED: &[char] = &[
        ' ', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(', ')', '`',
    ];
    // Field codes: a literal % is written %%.
    let arg = arg.replace('%', "%%");
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
        arg
    };
    // String-level escaping: backslashes are doubled once more.
    Some(quoted.replace('\\', "\\\\"))
}

/// Escape a plain string value (`Name=`, `Icon=`): backslashes doubled.
/// Callers have already refused control characters.
fn value(s: &str) -> String {
    s.replace('\\', "\\\\")
}

/// The desktop entry for `app`, or `None` if any guest-provided field is
/// not [`launcher_safe`].
pub fn desktop_entry(app: &AppEntry, lsw: &Path, vm: &str, icon: Option<&Path>) -> Option<String> {
    if !launcher_safe(&app.launch) || !launcher_safe(&app.name) {
        return None;
    }
    let lsw = lsw.to_string_lossy();
    let icon = icon.map(|i| i.to_string_lossy().into_owned());
    if [lsw.as_ref(), vm]
        .into_iter()
        .chain(icon.as_deref())
        .any(|s| s.chars().any(char::is_control))
    {
        return None;
    }
    let id = linux_app_id(&app.app);
    let exec = [
        exec_arg(&lsw)?,
        "--name".to_owned(),
        exec_arg(vm)?,
        "launch".to_owned(),
        exec_arg(&app.launch)?,
    ]
    .join(" ");
    let mut out = format!(
        "[Desktop Entry]\nType=Application\nName={}\nComment=Windows app\nExec={exec}\nCategories=Utility;{CATEGORY};\nStartupWMClass={id}\nX-LSW-VM={}\n",
        value(&app.name),
        value(vm),
    );
    if let Some(icon) = icon {
        out.push_str(&format!("Icon={}\n", value(&icon)));
    }
    Some(out)
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
        if !launcher_safe(&app.launch) || !launcher_safe(&app.name) {
            tracing::warn!(
                "skipping a Windows app with an unsafe name or id: {:?}",
                app.name
            );
            continue;
        }
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
        let Some(entry) = desktop_entry(app, lsw, vm, icon.as_deref()) else {
            tracing::warn!(
                "skipping a Windows app with an unsafe name or id: {:?}",
                app.name
            );
            continue;
        };
        let tmp = file.with_extension("desktop.tmp");
        std::fs::write(&tmp, entry)?;
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

    /// A Desktop Entry `Exec` parser written from the specification: the
    /// string-level unescape, then quoting, then `%%`.
    fn parse_exec(file: &str) -> Vec<String> {
        let line = file
            .lines()
            .find_map(|l| l.strip_prefix("Exec="))
            .expect("Exec key");
        // String level: \s \n \t \r \\ .
        let mut v = String::new();
        let mut it = line.chars();
        while let Some(c) = it.next() {
            if c == '\\' {
                match it.next() {
                    Some('s') => v.push(' '),
                    Some('n') => v.push('\n'),
                    Some('t') => v.push('\t'),
                    Some('r') => v.push('\r'),
                    Some('\\') => v.push('\\'),
                    Some(o) => {
                        v.push('\\');
                        v.push(o)
                    }
                    None => v.push('\\'),
                }
            } else {
                v.push(c);
            }
        }
        // Exec level: whitespace-separated, double-quoted args with \" \` \$ \\.
        let mut args = Vec::new();
        let mut cur = String::new();
        let (mut inq, mut started) = (false, false);
        let mut it = v.chars().peekable();
        while let Some(c) = it.next() {
            match c {
                '"' => {
                    inq = !inq;
                    started = true;
                }
                '\\' if inq => cur.push(it.next().expect("escaped char")),
                ' ' if !inq => {
                    if started || !cur.is_empty() {
                        args.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                _ => cur.push(c),
            }
        }
        if started || !cur.is_empty() {
            args.push(cur);
        }
        args.into_iter().map(|a| a.replace("%%", "%")).collect()
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
    fn adversarial_ids_roundtrip_exactly() {
        for id in [
            "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App",
            r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\cmd.exe",
            "a b\"c'd$e`f\\g;h|i&j*k?l#m(n)o<p>q~r",
            "100% %f %U %%",
            r#"\"; rm -rf ~ #"#,
        ] {
            let e =
                desktop_entry(&app(id, "X"), Path::new("/usr/bin/lsw"), "windows", None).unwrap();
            assert_eq!(
                parse_exec(&e),
                ["/usr/bin/lsw", "--name", "windows", "launch", id],
                "{id}"
            );
            // Every line is one of ours: guest data never starts a key.
            for line in e.lines() {
                let key = line.split('=').next().unwrap();
                assert!(
                    [
                        "[Desktop Entry]",
                        "Type",
                        "Name",
                        "Comment",
                        "Exec",
                        "Categories",
                        "StartupWMClass",
                        "X-LSW-VM",
                        "Icon"
                    ]
                    .contains(&key),
                    "unexpected line {line:?}"
                );
            }
        }
    }

    #[test]
    fn control_characters_are_refused_not_cleaned() {
        let bad = [
            app("calculator\nExec=/usr/bin/false\n#", "Calculator"),
            app("ok!App", "Paint\nExec=/bin/sh"),
            app("x\r", "y"),
            app("", "empty"),
            app(&"a".repeat(MAX_FIELD + 1), "long"),
        ];
        for a in &bad {
            assert!(
                desktop_entry(a, Path::new("/usr/bin/lsw"), "windows", None).is_none(),
                "{a:?}"
            );
        }
        let tmp = tempfile::tempdir().unwrap();
        let n = sync(
            &bad,
            &tmp.path().join("a"),
            &tmp.path().join("i"),
            Path::new("/usr/bin/lsw"),
            "windows",
        )
        .unwrap();
        assert_eq!(n, 0);
        assert_eq!(
            std::fs::read_dir(tmp.path().join("i")).unwrap().count(),
            0,
            "no icons for skipped apps"
        );
    }

    #[test]
    fn entries_match_window_app_ids_and_sync_removes_stale_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let (apps_dir, icons) = (tmp.path().join("applications"), tmp.path().join("icons"));
        let calc = app(
            "Microsoft.WindowsCalculator_8wekyb3d8bbwe!App",
            "Calculator",
        );
        let paint = app("Microsoft.Paint_8wekyb3d8bbwe!App", "Paint");
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
        assert!(text.contains(&format!(
            "Icon={}",
            icons.join(format!("{id}.png")).display()
        )));
        let paint_id = linux_app_id("Microsoft.Paint_8wekyb3d8bbwe!App");
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

    /// Writes launchers for hostile ids into `$LSW_DESKTOP_SAMPLES` so an
    /// external parser (GLib) can check them: `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn write_samples_for_external_parsers() {
        let Some(dir) = std::env::var_os("LSW_DESKTOP_SAMPLES") else {
            return;
        };
        let apps: Vec<AppEntry> = [
            "calculator\nExec=/usr/bin/false\n#",
            r"{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\cmd.exe",
            "a b\"c'd$e`f\\g;h|i&j*k?l#m(n)o<p>q~r",
            "100% %f %U %%",
        ]
        .iter()
        .enumerate()
        .map(|(i, id)| AppEntry {
            launch: id.to_string(),
            name: format!("Sample {i}"),
            app: format!("sample-{i}"),
            icon_png: vec![],
        })
        .collect();
        let dir = std::path::PathBuf::from(dir);
        let n = sync(
            &apps,
            &dir.join("applications"),
            &dir.join("icons"),
            Path::new("/usr/bin/lsw"),
            "windows",
        )
        .unwrap();
        println!("{n} launchers written");
    }
}
