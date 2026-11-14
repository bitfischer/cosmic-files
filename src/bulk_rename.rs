// Bulk rename: give every selected item the same new name followed by an
// ascending number, keeping the file extension. This module has no UI code.

use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

/// User input of the bulk rename dialog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Settings {
    /// New name; the number is appended to it
    pub name: String,
    /// First number, kept as a string so it can be bound to a text input
    pub start: String,
    /// Minimum number of digits, padded with leading zeros
    pub digits: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            name: String::new(),
            start: "1".to_string(),
            digits: "1".to_string(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    pub dir: bool,
}

impl Entry {
    pub fn old_name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RowError {
    Empty,
    Slashes,
    /// A file or folder outside of the batch already has this name
    Exists,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Row {
    pub old: String,
    pub new: String,
    pub error: Option<RowError>,
}

impl Row {
    pub fn changed(&self) -> bool {
        self.old != self.new
    }
}

/// Extension including the leading dot. Hidden files like `.bashrc` and
/// folders have none.
fn extension(name: &str, dir: bool) -> &str {
    if dir {
        return "";
    }
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => &name[i..],
        _ => "",
    }
}

/// Compute the new names for `entries` (in the given order) and validate them.
///
/// `exists` is used to detect collisions with files outside of the batch.
pub fn plan(entries: &[Entry], settings: &Settings, exists: impl Fn(&Path) -> bool) -> Vec<Row> {
    let start: u64 = settings.start.trim().parse().unwrap_or(1);
    let last = start.saturating_add(entries.len().saturating_sub(1) as u64);
    // At least the requested digits, and enough for the highest number
    let digits: usize = settings.digits.trim().parse().unwrap_or(1);
    let width = digits.min(32).max(last.to_string().len());

    let sources: HashSet<&Path> = entries.iter().map(|e| e.path.as_path()).collect();
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let old = entry.old_name();
            let number = start.saturating_add(index as u64);
            let new = format!(
                "{}{number:0>width$}{}",
                settings.name,
                extension(&old, entry.dir)
            );
            let error = if settings.name.is_empty() {
                Some(RowError::Empty)
            } else if settings.name.contains('/') {
                Some(RowError::Slashes)
            } else {
                let target = entry.path.with_file_name(&new);
                (new != old && !sources.contains(target.as_path()) && exists(&target))
                    .then_some(RowError::Exists)
            };
            Row { old, new, error }
        })
        .collect()
}

/// A not yet existing hidden sibling path of `path`.
fn unique_temp(path: &Path, kind: &str, i: usize) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut n = 0;
    loop {
        let candidate = path.with_file_name(format!(".{name}.cosmic-{kind}-{i}-{n}"));
        if candidate.symlink_metadata().is_err() {
            return candidate;
        }
        n += 1;
    }
}

/// Rename all `(from, to)` pairs. Renames that would overwrite another source
/// (swaps, chains) go through temporary names. On failure all completed
/// renames are rolled back.
pub fn perform(renames: &[(PathBuf, PathBuf)]) -> io::Result<()> {
    let renames: Vec<&(PathBuf, PathBuf)> = renames.iter().filter(|(f, t)| f != t).collect();
    let sources: HashSet<&Path> = renames.iter().map(|(f, _)| f.as_path()).collect();
    let needs_temp = renames.iter().any(|(_, to)| sources.contains(to.as_path()));

    // (current path, original path) for rollback
    let mut done: Vec<(PathBuf, PathBuf)> = Vec::new();

    // Undo all completed renames. Originals may be occupied by items that were
    // already moved to their new name (swaps, chains) and `rename` silently
    // replaces files, so first move everything to unique temporary names and
    // only then to the original names.
    let rollback = |done: &mut Vec<(PathBuf, PathBuf)>| {
        let mut parked: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(done.len());
        for (i, (current, original)) in done.drain(..).enumerate() {
            let temp = unique_temp(&current, "rollback", i);
            match std::fs::rename(&current, &temp) {
                Ok(()) => parked.push((temp, original)),
                Err(err) => {
                    log::error!(
                        "failed to roll back rename of {} to {}: {}",
                        current.display(),
                        original.display(),
                        err
                    );
                }
            }
        }
        for (temp, original) in parked {
            if original.symlink_metadata().is_ok() {
                // Never overwrite, keep the item under its temporary name
                log::error!(
                    "failed to roll back: {} exists, item kept as {}",
                    original.display(),
                    temp.display()
                );
            } else if let Err(err) = std::fs::rename(&temp, &original) {
                log::error!(
                    "failed to roll back rename of {} to {}: {}",
                    temp.display(),
                    original.display(),
                    err
                );
            }
        }
    };

    let rename_checked = |from: &Path, to: &Path| -> io::Result<()> {
        // `rename` silently replaces files, never allow that here
        if to.symlink_metadata().is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", to.display()),
            ));
        }
        std::fs::rename(from, to)
    };

    if !needs_temp {
        for (from, to) in &renames {
            match rename_checked(from, to) {
                Ok(()) => done.push((to.clone(), from.clone())),
                Err(err) => {
                    rollback(&mut done);
                    return Err(err);
                }
            }
        }
        return Ok(());
    }

    // Phase 1: move everything to unique temporary names
    let mut temps: Vec<(PathBuf, &PathBuf)> = Vec::with_capacity(renames.len());
    for (i, (from, to)) in renames.iter().enumerate() {
        let temp = unique_temp(from, "rename", i);
        match std::fs::rename(from, &temp) {
            Ok(()) => {
                done.push((temp.clone(), from.clone()));
                temps.push((temp, to));
            }
            Err(err) => {
                rollback(&mut done);
                return Err(err);
            }
        }
    }

    // Phase 2: move from the temporary names to the final names
    for (i, (temp, to)) in temps.iter().enumerate() {
        match rename_checked(temp, to) {
            Ok(()) => {
                // Update the rollback entry to point at the new location
                done[i].0 = (*to).clone();
            }
            Err(err) => {
                rollback(&mut done);
                return Err(err);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(names: &[&str]) -> Vec<Entry> {
        names
            .iter()
            .map(|n| Entry {
                path: PathBuf::from("/nonexistent-dir").join(n),
                dir: false,
            })
            .collect()
    }

    fn settings(name: &str, start: &str) -> Settings {
        Settings {
            name: name.into(),
            start: start.into(),
            digits: "1".into(),
        }
    }

    fn run(names: &[&str], settings: &Settings) -> Vec<String> {
        plan(&entries(names), settings, |_| false)
            .into_iter()
            .map(|r| r.new)
            .collect()
    }

    #[test]
    fn name_and_ascending_number_keep_extension() {
        assert_eq!(
            run(&["x.jpg", "y.png", "z"], &settings("Holiday-", "1")),
            ["Holiday-1.jpg", "Holiday-2.png", "Holiday-3"]
        );
    }

    #[test]
    fn numbers_are_padded_to_common_width() {
        let names: Vec<String> = (0..10).map(|i| format!("f{i}.txt")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let out = run(&refs, &settings("a", "1"));
        assert_eq!(out[0], "a01.txt");
        assert_eq!(out[9], "a10.txt");
        let out = run(&["a.txt", "b.txt"], &settings("n", "99"));
        assert_eq!(out, ["n099.txt", "n100.txt"]);
    }

    #[test]
    fn leading_zeros() {
        let s = Settings {
            digits: "3".into(),
            ..settings("n", "1")
        };
        assert_eq!(run(&["a.txt", "b.txt"], &s), ["n001.txt", "n002.txt"]);
        // Never fewer digits than the highest number needs
        let s = Settings {
            digits: "1".into(),
            ..settings("n", "9")
        };
        assert_eq!(run(&["a.txt", "b.txt"], &s), ["n09.txt", "n10.txt"]);
    }

    #[test]
    fn invalid_start_falls_back_to_one() {
        assert_eq!(run(&["a.txt"], &settings("n", "abc")), ["n1.txt"]);
    }

    #[test]
    fn extension_rules() {
        assert_eq!(extension("a.tar.gz", false), ".gz");
        assert_eq!(extension(".bashrc", false), "");
        assert_eq!(extension("a.", false), "");
        assert_eq!(extension("dir.d", true), "");
    }

    #[test]
    fn validation() {
        let rows = plan(&entries(&["a.txt"]), &settings("", "1"), |_| false);
        assert_eq!(rows[0].error, Some(RowError::Empty));
        let rows = plan(&entries(&["a.txt"]), &settings("x/y", "1"), |_| false);
        assert_eq!(rows[0].error, Some(RowError::Slashes));
        // Collision with a file outside of the batch
        let rows = plan(&entries(&["a.txt"]), &settings("n", "1"), |p| {
            p.ends_with("n1.txt")
        });
        assert_eq!(rows[0].error, Some(RowError::Exists));
        // Target that is itself renamed in the batch is fine
        let rows = plan(&entries(&["n2.txt", "n1.txt"]), &settings("n", "1"), |_| {
            true
        });
        assert!(rows.iter().all(|r| r.error.is_none()));
    }

    #[test]
    fn perform_renames_with_swap_and_rollback() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n);
        std::fs::write(p("a"), "a").unwrap();
        std::fs::write(p("b"), "b").unwrap();
        std::fs::write(p("c"), "c").unwrap();

        // Swap a and b, chain c -> a is not part of it
        perform(&[(p("a"), p("b")), (p("b"), p("a"))]).unwrap();
        assert_eq!(std::fs::read_to_string(p("a")).unwrap(), "b");
        assert_eq!(std::fs::read_to_string(p("b")).unwrap(), "a");

        // Shift chain: a -> x, b -> a
        perform(&[(p("b"), p("a2")), (p("a"), p("b"))]).unwrap();
        assert_eq!(std::fs::read_to_string(p("b")).unwrap(), "b");
        assert_eq!(std::fs::read_to_string(p("a2")).unwrap(), "a");

        // Failure (target exists outside batch) rolls everything back
        let err = perform(&[(p("a2"), p("z")), (p("b"), p("c"))]);
        assert!(err.is_err());
        assert_eq!(std::fs::read_to_string(p("a2")).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(p("b")).unwrap(), "b");
        assert!(!p("z").exists());
        assert_eq!(std::fs::read_to_string(p("c")).unwrap(), "c");
    }

    #[test]
    fn rollback_never_overwrites_swapped_items() {
        let dir = tempfile::tempdir().unwrap();
        let p = |n: &str| dir.path().join(n);
        std::fs::write(p("a"), "a").unwrap();
        std::fs::write(p("b"), "b").unwrap();
        std::fs::write(p("x"), "x").unwrap();
        // Dangling symlink: `exists()` is false, but the name is taken
        std::os::unix::fs::symlink(p("missing"), p("z")).unwrap();

        // Swap a/b, then fail on x -> z
        let err = perform(&[(p("a"), p("b")), (p("b"), p("a")), (p("x"), p("z"))]);
        assert!(err.is_err());
        assert_eq!(std::fs::read_to_string(p("a")).unwrap(), "a");
        assert_eq!(std::fs::read_to_string(p("b")).unwrap(), "b");
        assert_eq!(std::fs::read_to_string(p("x")).unwrap(), "x");
    }
}
