//! Path resolution against the driver's root.
//!
//! This is **scoping, not a security boundary** — that line was retired, and
//! pretending otherwise would be worse than not having it. What it buys is
//! ordinary engineering: paths in tool calls stay short (which keeps context
//! small), a driver instance means one tree (the same shape the remote driver
//! gives a node), and a confused model reaching for `~/.ssh` gets an error
//! instead of a file.
//!
//! Where it deliberately stops: a symlink inside the root may point outside
//! it and this does not follow the link to find out. Stated here so nobody
//! mistakes the fence for a wall.

use std::path::{Component, Path, PathBuf};

pub struct Scope {
    root: PathBuf,
}

impl Scope {
    pub fn new(root: PathBuf) -> Scope {
        Scope { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a caller-supplied path. Absolute paths and `..` that climbs
    /// past the root are refused.
    ///
    /// Normalisation is lexical on purpose: `fs::write` names a file that does
    /// not exist yet, and `canonicalize` cannot resolve those.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let p = Path::new(path);
        if p.is_absolute() {
            return Err(format!(
                "path must be relative to the driver root: {path:?}"
            ));
        }
        let mut out = PathBuf::new();
        for c in p.components() {
            match c {
                Component::Normal(seg) => out.push(seg),
                Component::CurDir => {}
                Component::ParentDir => {
                    if !out.pop() {
                        return Err(format!("path climbs out of the driver root: {path:?}"));
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!("path must be relative: {path:?}"));
                }
            }
        }
        Ok(self.root.join(out))
    }

    /// The name a resolved path goes back to the model under: relative, so a
    /// listing of a thousand files does not spend a thousand prefixes of
    /// context on saying the same thing.
    pub fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_what_would_leave_the_tree() {
        let s = Scope::new(PathBuf::from("/srv/work"));
        assert!(s.resolve("/etc/passwd").is_err());
        assert!(s.resolve("../../etc/passwd").is_err());
        assert!(s.resolve("a/../../..").is_err());
        assert_eq!(
            s.resolve("src/main.rs").unwrap(),
            Path::new("/srv/work/src/main.rs")
        );
        // Climbing back down to where it started is fine.
        assert_eq!(s.resolve("a/../b").unwrap(), Path::new("/srv/work/b"));
        assert_eq!(s.resolve("./x").unwrap(), Path::new("/srv/work/x"));
    }

    #[test]
    fn names_go_back_relative() {
        let s = Scope::new(PathBuf::from("/srv/work"));
        assert_eq!(s.relative(Path::new("/srv/work/src/a.rs")), "src/a.rs");
    }
}
