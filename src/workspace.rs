//! The assistant's workspace on disk, and safe access to the memory tree.
//!
//! ```text
//! workspace/
//!   SOUL.md  IDENTITY.md  AGENTS.md     written by the operator
//!   memory/
//!     shared/MEMORY.md ...              every user's conversations may read
//!     personal/<user>/MEMORY.md ...     only that user's conversations
//! ```
//!
//! Memory paths come from a model, so they are checked here and nowhere
//! else: a path is a few plain name segments ending in `.md`, resolved under
//! one user's directory or the shared one, and no component may be a
//! symbolic link.

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Largest memory file the tools will write.
pub const MAX_MEMORY_FILE_BYTES: usize = 256 * 1024;
/// Most segments in a memory path (`topics/projects/boat.md` is three).
const MAX_PATH_SEGMENTS: usize = 4;
const MAX_SEGMENT_BYTES: usize = 64;
/// Most files one listing reports.
const MAX_LISTED_FILES: usize = 200;

/// Whose memory a path is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// One user's own memory.
    Personal(&'a str),
    /// Memory every user's conversations may read.
    Shared,
}

#[derive(Debug, Clone)]
pub struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// An operator-written file at the top of the workspace, or `None` when
    /// it does not exist.
    pub fn operator_file(&self, name: &str) -> Result<Option<String>> {
        read_optional(&self.root.join(name))
    }

    /// The directory a scope's memory lives in.
    pub fn scope_directory(&self, scope: Scope) -> PathBuf {
        let memory = self.root.join("memory");
        match scope {
            Scope::Personal(user) => memory.join("personal").join(user),
            Scope::Shared => memory.join("shared"),
        }
    }

    /// Create a user's memory directory, which is also their session's
    /// working directory.
    pub fn ensure_user(&self, user: &str) -> Result<PathBuf> {
        crate::config::validate_user_name(user)?;
        let directory = self.scope_directory(Scope::Personal(user));
        fs::create_dir_all(&directory)
            .with_context(|| format!("cannot create {}", directory.display()))?;
        Ok(directory)
    }

    /// Read a memory file, or `None` when it does not exist.
    pub fn read_memory(&self, scope: Scope, path: &str) -> Result<Option<String>> {
        let file = self.resolve(scope, path)?;
        read_optional(&file)
    }

    /// Replace a memory file, or append to it. Returns the file's new size.
    pub fn write_memory(
        &self,
        scope: Scope,
        path: &str,
        content: &str,
        append: bool,
    ) -> Result<usize> {
        let file = self.resolve(scope, path)?;
        let existing = if append {
            read_optional(&file)?.unwrap_or_default()
        } else {
            String::new()
        };
        let mut text = existing;
        if append && !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(content);
        if !text.ends_with('\n') {
            text.push('\n');
        }
        if text.len() > MAX_MEMORY_FILE_BYTES {
            bail!(
                "{path} would be {} bytes, over the limit of {MAX_MEMORY_FILE_BYTES}; \
summarize it or use another file",
                text.len()
            );
        }
        let parent = file.parent().context("memory file has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        // Created directories are checked again: one may have been swapped
        // for a link since `resolve` looked.
        refuse_links(&self.scope_directory(scope), &file)?;
        // Write beside the file and rename, so a reader never sees half a
        // file and a failed write leaves the old one.
        let temporary = file.with_extension("md.new");
        let mut handle = fs::File::create(&temporary)
            .with_context(|| format!("cannot write {}", temporary.display()))?;
        handle.write_all(text.as_bytes())?;
        handle.sync_all()?;
        fs::rename(&temporary, &file)
            .with_context(|| format!("cannot replace {}", file.display()))?;
        Ok(text.len())
    }

    /// The memory files in a scope, as paths relative to it, sorted.
    pub fn list_memory(&self, scope: Scope) -> Result<Vec<String>> {
        let root = self.scope_directory(scope);
        let mut found = Vec::new();
        let mut pending = vec![(root.clone(), 0)];
        while let Some((directory, depth)) = pending.pop() {
            let Ok(entries) = fs::read_dir(&directory) else {
                continue;
            };
            for entry in entries.flatten() {
                // `file_type` does not follow links, so a link is neither a
                // file nor a directory here and is skipped.
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if kind.is_dir() && depth + 1 < MAX_PATH_SEGMENTS {
                    pending.push((path, depth + 1));
                } else if kind.is_file()
                    && path.extension().is_some_and(|extension| extension == "md")
                    && let Ok(relative) = path.strip_prefix(&root)
                {
                    found.push(relative.to_string_lossy().into_owned());
                }
            }
        }
        found.sort();
        found.truncate(MAX_LISTED_FILES);
        Ok(found)
    }

    /// The file a model-supplied memory path names, inside `scope`.
    fn resolve(&self, scope: Scope, path: &str) -> Result<PathBuf> {
        if let Scope::Personal(user) = scope {
            crate::config::validate_user_name(user)?;
        }
        let segments: Vec<&str> = path.split('/').collect();
        if segments.len() > MAX_PATH_SEGMENTS {
            bail!("memory path {path:?} is nested too deeply");
        }
        for segment in &segments {
            validate_segment(segment).with_context(|| format!("invalid memory path {path:?}"))?;
        }
        if !path.ends_with(".md") {
            bail!("memory path {path:?} must end in .md");
        }
        let root = self.scope_directory(scope);
        let file = segments
            .iter()
            .fold(root.clone(), |file, segment| file.join(segment));
        refuse_links(&root, &file)?;
        Ok(file)
    }
}

/// A path segment is a plain name: no separators, no `..`, no leading dot.
fn validate_segment(segment: &str) -> Result<()> {
    let mut chars = segment.chars();
    let valid = segment.len() <= MAX_SEGMENT_BYTES
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !segment.contains("..");
    if !valid {
        bail!(
            "each part must be 1 to {MAX_SEGMENT_BYTES} characters of letters, digits, '.', '_' \
and '-', starting with a letter or digit"
        );
    }
    Ok(())
}

/// Refuse `file` if the scope's own directory, or any component of `file`
/// below it, is a symbolic link. Components that do not exist yet are fine.
fn refuse_links(root: &Path, file: &Path) -> Result<()> {
    let relative = file
        .strip_prefix(root)
        .context("memory path left its directory")?;
    let mut current = root.to_owned();
    let mut components = relative.components();
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("{} is a symbolic link", current.display());
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(error).with_context(|| format!("cannot check {}", current.display()));
            }
        }
        match components.next() {
            Some(component) => current.push(component),
            None => return Ok(()),
        }
    }
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(String::from_utf8_lossy(&bytes).into_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn workspace() -> (TempDir, Workspace) {
        let temporary = TempDir::new().unwrap();
        let workspace = Workspace::new(temporary.path().to_owned());
        (temporary, workspace)
    }

    #[test]
    fn writes_append_replace_and_read_back() {
        let (_temporary, workspace) = workspace();
        let alice = Scope::Personal("alice");
        assert_eq!(workspace.read_memory(alice, "MEMORY.md").unwrap(), None);
        workspace
            .write_memory(alice, "MEMORY.md", "likes tea", true)
            .unwrap();
        workspace
            .write_memory(alice, "MEMORY.md", "has a boat", true)
            .unwrap();
        assert_eq!(
            workspace.read_memory(alice, "MEMORY.md").unwrap().unwrap(),
            "likes tea\nhas a boat\n"
        );
        workspace
            .write_memory(alice, "MEMORY.md", "likes coffee", false)
            .unwrap();
        assert_eq!(
            workspace.read_memory(alice, "MEMORY.md").unwrap().unwrap(),
            "likes coffee\n"
        );
        workspace
            .write_memory(alice, "topics/boat.md", "a sloop", true)
            .unwrap();
        assert_eq!(
            workspace.list_memory(alice).unwrap(),
            ["MEMORY.md", "topics/boat.md"]
        );
    }

    #[test]
    fn one_users_memory_is_not_anothers_or_the_shared_one() {
        let (_temporary, workspace) = workspace();
        workspace
            .write_memory(
                Scope::Personal("alice"),
                "MEMORY.md",
                "alice's secret",
                true,
            )
            .unwrap();
        workspace
            .write_memory(Scope::Shared, "MEMORY.md", "bins go out on Tuesday", true)
            .unwrap();
        let bob = Scope::Personal("bob");
        assert_eq!(workspace.read_memory(bob, "MEMORY.md").unwrap(), None);
        assert!(workspace.list_memory(bob).unwrap().is_empty());
        assert_eq!(
            workspace
                .read_memory(Scope::Shared, "MEMORY.md")
                .unwrap()
                .unwrap(),
            "bins go out on Tuesday\n"
        );
    }

    #[test]
    fn paths_that_could_leave_the_scope_are_refused() {
        let (_temporary, workspace) = workspace();
        let alice = Scope::Personal("alice");
        for path in [
            "../bob/MEMORY.md",
            "..",
            "/etc/passwd.md",
            "topics/../../bob/MEMORY.md",
            ".hidden.md",
            "a//b.md",
            "",
            "notes.txt",
            "a..b.md",
            "a/b/c/d/e.md",
            "sp ace.md",
            "nul\0.md",
        ] {
            assert!(
                workspace.read_memory(alice, path).is_err(),
                "{path:?} was accepted for reading"
            );
            assert!(
                workspace.write_memory(alice, path, "x", true).is_err(),
                "{path:?} was accepted for writing"
            );
        }
        // A scope for a name that is not a valid user is refused too.
        assert!(
            workspace
                .read_memory(Scope::Personal("../shared"), "MEMORY.md")
                .is_err()
        );
    }

    #[test]
    fn symbolic_links_are_never_followed() {
        let (temporary, workspace) = workspace();
        workspace
            .write_memory(Scope::Personal("bob"), "MEMORY.md", "bob's secret", true)
            .unwrap();
        let alice_directory = workspace.ensure_user("alice").unwrap();
        let bob_directory = workspace.scope_directory(Scope::Personal("bob"));
        // A link to another user's file, and a link to their directory.
        std::os::unix::fs::symlink(
            bob_directory.join("MEMORY.md"),
            alice_directory.join("x.md"),
        )
        .unwrap();
        std::os::unix::fs::symlink(&bob_directory, alice_directory.join("topics")).unwrap();
        let alice = Scope::Personal("alice");
        for path in ["x.md", "topics/MEMORY.md"] {
            let error = workspace.read_memory(alice, path).unwrap_err();
            assert!(error.to_string().contains("symbolic link"), "{error:#}");
            assert!(workspace.write_memory(alice, path, "x", false).is_err());
        }
        assert!(workspace.list_memory(alice).unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(bob_directory.join("MEMORY.md")).unwrap(),
            "bob's secret\n"
        );
        // A user directory that is itself a link is refused as well.
        let carol = temporary.path().join("memory/personal/carol");
        std::os::unix::fs::symlink(&bob_directory, &carol).unwrap();
        assert!(
            workspace
                .read_memory(Scope::Personal("carol"), "MEMORY.md")
                .is_err()
        );
    }

    #[test]
    fn a_file_cannot_grow_past_the_limit() {
        let (_temporary, workspace) = workspace();
        let alice = Scope::Personal("alice");
        let big = "x".repeat(MAX_MEMORY_FILE_BYTES - 10);
        workspace
            .write_memory(alice, "MEMORY.md", &big, true)
            .unwrap();
        let error = workspace
            .write_memory(alice, "MEMORY.md", "twenty more bytes!!!", true)
            .unwrap_err();
        assert!(error.to_string().contains("over the limit"));
        // The refused write left the file as it was.
        assert_eq!(
            workspace
                .read_memory(alice, "MEMORY.md")
                .unwrap()
                .unwrap()
                .len(),
            big.len() + 1
        );
    }
}
