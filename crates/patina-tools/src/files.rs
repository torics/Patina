//! The five file tools. Each resolves its path through the workspace before touching the disk.
use crate::confine::Workspace;
use async_trait::async_trait;
use ignore::WalkBuilder;
use patina_core::provider::ToolSpec;
use patina_core::tool::{Tool, ToolKind, ToolOutput};
use serde_json::{Value, json};
use std::sync::Arc;

pub const MAX_LIST_ENTRIES: usize = 500;
pub const MAX_READ_LINES: usize = 2000;
pub const MAX_READ_BYTES: usize = 100 * 1024;
const MAX_SEARCH_LINE: usize = 400;

/// The built-in file tools for one workspace, by name.
pub fn file_tools(ws: Arc<Workspace>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(ListFiles(ws.clone())),
        Arc::new(ReadFile(ws.clone())),
        Arc::new(Search(ws.clone())),
        Arc::new(EditFile(ws.clone())),
        Arc::new(WriteFile(ws)),
    ]
}

fn spec(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        schema: json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false}),
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or("")
}

/// Run blocking file work off the async runtime.
async fn blocking(f: impl FnOnce() -> ToolOutput + Send + 'static) -> ToolOutput {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|e| ToolOutput::error(format!("tool crashed: {e}")))
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0) || std::str::from_utf8(bytes).is_err()
}

fn walker(root: &std::path::Path, depth: Option<usize>) -> WalkBuilder {
    let mut b = WalkBuilder::new(root);
    // Honor .gitignore even where git metadata is not visible, show dotfiles, never descend into .git.
    b.hidden(false)
        .require_git(false)
        .max_depth(depth)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|e| e.file_name() != ".git");
    b
}

pub struct ListFiles(Arc<Workspace>);

#[async_trait]
impl Tool for ListFiles {
    fn spec(&self) -> ToolSpec {
        spec(
            "list_files",
            "List files and folders under a path as a tree. Respects .gitignore. Returns at most 500 entries.",
            json!({"path": {"type": "string", "description": "Folder relative to the workspace root; \".\" for the root"},
                   "depth": {"type": "integer", "minimum": 1, "maximum": 20, "description": "How many levels to descend (default 3)"}}),
            &["path"],
        )
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    async fn call(&self, args: Value) -> ToolOutput {
        let ws = self.0.clone();
        blocking(move || {
            let dir = match ws.resolve(str_arg(&args, "path")) {
                Ok(d) => d,
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            if !dir.is_dir() {
                return ToolOutput::error(format!("{:?} is not a folder", str_arg(&args, "path")));
            }
            let depth = args.get("depth").and_then(Value::as_u64).unwrap_or(3) as usize;
            let mut lines = Vec::new();
            let mut truncated = false;
            for entry in walker(&dir, Some(depth)).build().flatten().skip(1) {
                if lines.len() == MAX_LIST_ENTRIES {
                    truncated = true;
                    break;
                }
                let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
                lines.push(format!("{}{}", ws.display(entry.path()), if is_dir { "/" } else { "" }));
            }
            let mut out = if lines.is_empty() {
                "(empty)".to_string()
            } else {
                lines.join("\n")
            };
            if truncated {
                out.push_str(&format!(
                    "\n[stopped at {MAX_LIST_ENTRIES} entries; list a subfolder or lower the depth]"
                ));
            }
            ToolOutput::ok(out)
        })
        .await
    }
}

pub struct ReadFile(Arc<Workspace>);

#[async_trait]
impl Tool for ReadFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "read_file",
            "Read a text file with line numbers. Returns up to 2000 lines or 100 KB per call; use offset and limit for more. Refuses binary files.",
            json!({"path": {"type": "string"},
                   "offset": {"type": "integer", "minimum": 1, "description": "First line to return, starting at 1"},
                   "limit": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "Number of lines to return"}}),
            &["path"],
        )
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    async fn call(&self, args: Value) -> ToolOutput {
        let ws = self.0.clone();
        blocking(move || {
            let rel = str_arg(&args, "path");
            let path = match ws.resolve(rel) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(e) => return ToolOutput::error(format!("cannot read {rel:?}: {e}")),
            };
            if is_binary(&bytes) {
                return ToolOutput::error(format!("{rel:?} is a binary file"));
            }
            ws.remember(&path, &bytes);
            let text = String::from_utf8_lossy(&bytes);
            let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1) as usize;
            let limit = (args.get("limit").and_then(Value::as_u64).unwrap_or(MAX_READ_LINES as u64) as usize).min(MAX_READ_LINES);
            let total = text.lines().count();
            let mut out = String::new();
            let mut last = offset - 1;
            for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
                let numbered = format!("{:>6}\t{line}\n", i + 1);
                if out.len() + numbered.len() > MAX_READ_BYTES {
                    break;
                }
                out.push_str(&numbered);
                last = i + 1;
            }
            if total == 0 {
                out.push_str("(empty file)");
            } else if offset > total {
                return ToolOutput::error(format!("offset {offset} is past the end; the file has {total} lines"));
            } else if last < total {
                out.push_str(&format!(
                    "[showing lines {offset} to {last} of {total}; continue with offset {}]",
                    last + 1
                ));
            }
            ToolOutput::ok(out)
        })
        .await
    }
}

pub struct Search(Arc<Workspace>);

#[async_trait]
impl Tool for Search {
    fn spec(&self) -> ToolSpec {
        spec(
            "search",
            "Search file contents with a regular expression. Respects .gitignore. Returns file, line number and line.",
            json!({"pattern": {"type": "string", "description": "Regular expression"},
                   "path": {"type": "string", "description": "Folder or file to search (default: the workspace root)"},
                   "glob": {"type": "string", "description": "Only search files matching this glob, for example *.rs"},
                   "max_results": {"type": "integer", "minimum": 1, "maximum": 1000, "description": "Default 100"}}),
            &["pattern"],
        )
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Read
    }
    async fn call(&self, args: Value) -> ToolOutput {
        let ws = self.0.clone();
        blocking(move || {
            let re = match regex::Regex::new(str_arg(&args, "pattern")) {
                Ok(r) => r,
                Err(e) => return ToolOutput::error(format!("invalid pattern: {e}")),
            };
            let start = match ws.resolve(args.get("path").and_then(Value::as_str).unwrap_or(".")) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            let max = args.get("max_results").and_then(Value::as_u64).unwrap_or(100) as usize;
            let mut w = walker(&start, None);
            if let Some(glob) = args.get("glob").and_then(Value::as_str) {
                let mut ob = ignore::overrides::OverrideBuilder::new(&start);
                if let Err(e) = ob.add(glob) {
                    return ToolOutput::error(format!("invalid glob: {e}"));
                }
                match ob.build() {
                    Ok(o) => {
                        w.overrides(o);
                    }
                    Err(e) => return ToolOutput::error(format!("invalid glob: {e}")),
                }
            }
            let mut hits = Vec::new();
            'files: for entry in w.build().flatten() {
                if !entry.file_type().is_some_and(|t| t.is_file()) {
                    continue;
                }
                // The walker does not follow symlinks, so every file here is really inside the workspace.
                let Ok(bytes) = std::fs::read(entry.path()) else { continue };
                if is_binary(&bytes) {
                    continue;
                }
                for (i, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
                    if re.is_match(line) {
                        if hits.len() == max {
                            hits.push(format!("[stopped at {max} results; narrow the pattern, path or glob]"));
                            break 'files;
                        }
                        let shown: String = line.chars().take(MAX_SEARCH_LINE).collect();
                        hits.push(format!("{}:{}: {}", ws.display(entry.path()), i + 1, shown));
                    }
                }
            }
            ToolOutput::ok(if hits.is_empty() {
                "no matches".to_string()
            } else {
                hits.join("\n")
            })
        })
        .await
    }
}

/// A unified diff between two versions of one file, as shown on approval cards.
pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

/// What an edit would produce: `(full path, old text, new text)`, or the reason it cannot apply.
fn plan_edit(ws: &Workspace, args: &Value) -> Result<(std::path::PathBuf, String, String), String> {
    let rel = str_arg(args, "path");
    let (old, new) = (str_arg(args, "old_string"), str_arg(args, "new_string"));
    let path = ws.resolve(rel).map_err(|e| e.to_string())?;
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read {rel:?}: {e}"))?;
    if is_binary(&bytes) {
        return Err(format!("{rel:?} is a binary file"));
    }
    match ws.is_fresh(&path, &bytes) {
        None => return Err(format!("read {rel:?} with read_file before editing it")),
        Some(false) => return Err(format!("{rel:?} changed since you last read it; read it again before editing")),
        Some(true) => {}
    }
    if old.is_empty() {
        return Err("old_string is empty; use write_file to create or replace a whole file".into());
    }
    if old == new {
        return Err("old_string and new_string are the same".into());
    }
    let text = String::from_utf8_lossy(&bytes).to_string();
    let count = text.matches(old).count();
    let replace_all = args.get("replace_all").and_then(Value::as_bool).unwrap_or(false);
    let updated = match (count, replace_all) {
        (0, _) => return Err(format!("old_string was not found in {rel:?}")),
        (1, _) | (_, true) => text.replace(old, new),
        (n, false) => {
            return Err(format!(
                "old_string matches {n} times in {rel:?}; add context to make it unique, or set replace_all"
            ));
        }
    };
    Ok((path, text, updated))
}

pub struct EditFile(Arc<Workspace>);

#[async_trait]
impl Tool for EditFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "edit_file",
            "Replace an exact string in a file. old_string must match exactly once unless replace_all is true. Read the file first.",
            json!({"path": {"type": "string"}, "old_string": {"type": "string"}, "new_string": {"type": "string"},
                   "replace_all": {"type": "boolean"}}),
            &["path", "old_string", "new_string"],
        )
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    async fn preview(&self, args: &Value) -> Option<String> {
        let (ws, args) = (self.0.clone(), args.clone());
        tokio::task::spawn_blocking(move || {
            plan_edit(&ws, &args)
                .ok()
                .map(|(_, old, new)| unified_diff(str_arg(&args, "path"), &old, &new))
        })
        .await
        .ok()
        .flatten()
    }
    async fn call(&self, args: Value) -> ToolOutput {
        let ws = self.0.clone();
        blocking(move || {
            let (path, old, new) = match plan_edit(&ws, &args) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e),
            };
            if let Err(e) = std::fs::write(&path, &new) {
                return ToolOutput::error(format!("cannot write {:?}: {e}", str_arg(&args, "path")));
            }
            ws.remember(&path, new.as_bytes());
            let n = old.matches(str_arg(&args, "old_string")).count();
            ToolOutput {
                content: format!("edited {} ({n} replacement{})", ws.display(&path), if n == 1 { "" } else { "s" }),
                is_error: false,
                changed_files: true,
            }
        })
        .await
    }
}

pub struct WriteFile(Arc<Workspace>);

#[async_trait]
impl Tool for WriteFile {
    fn spec(&self) -> ToolSpec {
        spec(
            "write_file",
            "Create a file or overwrite it with the given content. Creates parent folders.",
            json!({"path": {"type": "string"}, "content": {"type": "string"}}),
            &["path", "content"],
        )
    }
    fn kind(&self) -> ToolKind {
        ToolKind::Edit
    }
    async fn preview(&self, args: &Value) -> Option<String> {
        let (ws, args) = (self.0.clone(), args.clone());
        tokio::task::spawn_blocking(move || {
            let rel = str_arg(&args, "path");
            let path = ws.resolve(rel).ok()?;
            let old = std::fs::read(&path).unwrap_or_default();
            if is_binary(&old) {
                return Some(format!("(overwrites binary file {rel})"));
            }
            Some(unified_diff(rel, &String::from_utf8_lossy(&old), str_arg(&args, "content")))
        })
        .await
        .ok()
        .flatten()
    }
    async fn call(&self, args: Value) -> ToolOutput {
        let ws = self.0.clone();
        blocking(move || {
            let rel = str_arg(&args, "path");
            let content = str_arg(&args, "content");
            let path = match ws.resolve(rel) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e.to_string()),
            };
            if path == ws.root() || path.is_dir() {
                return ToolOutput::error(format!("{rel:?} is a folder"));
            }
            let existed = path.exists();
            if let Some(parent) = path.parent()
                && let Err(e) = std::fs::create_dir_all(parent)
            {
                return ToolOutput::error(format!("cannot create folders for {rel:?}: {e}"));
            }
            if let Err(e) = std::fs::write(&path, content) {
                return ToolOutput::error(format!("cannot write {rel:?}: {e}"));
            }
            ws.remember(&path, content.as_bytes());
            ToolOutput {
                content: format!(
                    "{} {} ({} bytes)",
                    if existed { "overwrote" } else { "created" },
                    ws.display(&path),
                    content.len()
                ),
                is_error: false,
                changed_files: true,
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _t: tempfile::TempDir,
        ws: Arc<Workspace>,
        outside: std::path::PathBuf,
    }

    fn fixture() -> Fixture {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("work");
        std::fs::create_dir_all(root.join("src/deep")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    println!(\"hello\");\n}\n").unwrap();
        std::fs::write(root.join("src/deep/notes.txt"), "alpha\nbeta\nalpha\n").unwrap();
        std::fs::write(root.join("target/built.txt"), "hello ignored\n").unwrap();
        std::fs::write(root.join(".git/config"), "hello git\n").unwrap();
        std::fs::write(root.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        let outside = t.path().join("secret.txt");
        std::fs::write(&outside, "outside secret\n").unwrap();
        let ws = Arc::new(Workspace::new(&root).unwrap());
        Fixture { _t: t, ws, outside }
    }

    async fn call(f: &Fixture, name: &str, args: Value) -> ToolOutput {
        let tool = file_tools(f.ws.clone()).into_iter().find(|t| t.spec().name == name).unwrap();
        patina_core::tool::validate(&tool.spec().schema, &args).unwrap();
        tool.call(args).await
    }

    #[tokio::test]
    async fn list_respects_gitignore_and_hides_git() {
        let f = fixture();
        let out = call(&f, "list_files", json!({"path": "."})).await;
        assert!(!out.is_error);
        assert!(
            out.content.contains("src/main.rs") && out.content.contains("src/deep/"),
            "{}",
            out.content
        );
        assert!(
            !out.content.contains("built.txt") && !out.content.contains(".git/"),
            "{}",
            out.content
        );
        let shallow = call(&f, "list_files", json!({"path": ".", "depth": 1})).await;
        assert!(!shallow.content.contains("main.rs"));
        assert!(call(&f, "list_files", json!({"path": "src/main.rs"})).await.is_error);
    }

    #[tokio::test]
    async fn list_stops_at_500_entries() {
        let f = fixture();
        std::fs::create_dir_all(f.ws.root().join("many")).unwrap();
        for i in 0..600 {
            std::fs::write(f.ws.root().join(format!("many/f{i:04}.txt")), "").unwrap();
        }
        let out = call(&f, "list_files", json!({"path": "many"})).await;
        assert_eq!(out.content.lines().filter(|l| l.starts_with("many/")).count(), MAX_LIST_ENTRIES);
        assert!(out.content.contains("stopped at 500"));
    }

    #[tokio::test]
    async fn read_numbers_lines_pages_and_refuses_binary() {
        let f = fixture();
        let out = call(&f, "read_file", json!({"path": "src/main.rs"})).await;
        assert_eq!(out.content, "     1\tfn main() {\n     2\t    println!(\"hello\");\n     3\t}\n");
        let page = call(&f, "read_file", json!({"path": "src/main.rs", "offset": 2, "limit": 1})).await;
        assert!(
            page.content.starts_with("     2\t") && page.content.contains("continue with offset 3"),
            "{}",
            page.content
        );
        assert!(call(&f, "read_file", json!({"path": "bin.dat"})).await.content.contains("binary"));
        assert!(call(&f, "read_file", json!({"path": "missing.txt"})).await.is_error);
        assert!(call(&f, "read_file", json!({"path": "src/main.rs", "offset": 99})).await.is_error);

        let big: String = (0..5000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(f.ws.root().join("big.txt"), &big).unwrap();
        let out = call(&f, "read_file", json!({"path": "big.txt"})).await;
        assert_eq!(out.content.lines().filter(|l| l.contains('\t')).count(), MAX_READ_LINES);
        assert!(out.content.len() <= MAX_READ_BYTES + 200);
    }

    #[tokio::test]
    async fn search_finds_lines_and_respects_ignore_and_glob() {
        let f = fixture();
        let out = call(&f, "search", json!({"pattern": "hel+o"})).await;
        assert_eq!(out.content, "src/main.rs:2:     println!(\"hello\");");
        let out = call(&f, "search", json!({"pattern": "alpha", "glob": "*.txt"})).await;
        assert_eq!(out.content, "src/deep/notes.txt:1: alpha\nsrc/deep/notes.txt:3: alpha");
        assert_eq!(
            call(&f, "search", json!({"pattern": "alpha", "glob": "*.rs"})).await.content,
            "no matches"
        );
        assert!(
            call(&f, "search", json!({"pattern": "alpha", "max_results": 1}))
                .await
                .content
                .contains("stopped at 1")
        );
        assert!(call(&f, "search", json!({"pattern": "("})).await.is_error);
    }

    #[tokio::test]
    async fn edit_requires_read_unique_match_and_fresh_content() {
        let f = fixture();
        let edit = |old: &str, new: &str, all: bool| json!({"path": "src/deep/notes.txt", "old_string": old, "new_string": new, "replace_all": all});
        let out = call(&f, "edit_file", edit("beta", "gamma", false)).await;
        assert!(out.is_error && out.content.contains("read_file"), "{}", out.content);

        call(&f, "read_file", json!({"path": "src/deep/notes.txt"})).await;
        let out = call(&f, "edit_file", edit("alpha", "x", false)).await;
        assert!(out.is_error && out.content.contains("matches 2 times"), "{}", out.content);
        assert!(call(&f, "edit_file", edit("nope", "x", false)).await.content.contains("not found"));

        let out = call(&f, "edit_file", edit("beta", "gamma", false)).await;
        assert!(!out.is_error && out.changed_files);
        // The tool's own write counts as seen, so a second edit works without another read.
        let out = call(&f, "edit_file", edit("alpha", "omega", true)).await;
        assert!(out.content.contains("2 replacements"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(f.ws.root().join("src/deep/notes.txt")).unwrap(),
            "omega\ngamma\nomega\n"
        );

        // Someone else changes the file: the next edit is refused until the agent reads again.
        std::fs::write(f.ws.root().join("src/deep/notes.txt"), "changed elsewhere\n").unwrap();
        let out = call(&f, "edit_file", edit("changed", "x", false)).await;
        assert!(out.is_error && out.content.contains("changed since"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(f.ws.root().join("src/deep/notes.txt")).unwrap(),
            "changed elsewhere\n"
        );
    }

    #[tokio::test]
    async fn write_creates_parents_and_previews_a_diff() {
        let f = fixture();
        let tool = file_tools(f.ws.clone())
            .into_iter()
            .find(|t| t.spec().name == "write_file")
            .unwrap();
        let args = json!({"path": "docs/new/readme.md", "content": "# Title\n"});
        let diff = tool.preview(&args).await.unwrap();
        assert!(diff.contains("+# Title") && diff.contains("b/docs/new/readme.md"), "{diff}");
        assert!(!f.ws.root().join("docs").exists(), "a preview writes nothing");
        let out = tool.call(args).await;
        assert!(out.content.starts_with("created docs/new/readme.md") && out.changed_files);
        let over = call(&f, "write_file", json!({"path": "docs/new/readme.md", "content": "x"})).await;
        assert!(over.content.starts_with("overwrote"));
        assert!(call(&f, "write_file", json!({"path": "src", "content": "x"})).await.is_error);

        let edit = file_tools(f.ws.clone()).into_iter().find(|t| t.spec().name == "edit_file").unwrap();
        call(&f, "read_file", json!({"path": "src/main.rs"})).await;
        let diff = edit
            .preview(&json!({"path": "src/main.rs", "old_string": "hello", "new_string": "bye"}))
            .await
            .unwrap();
        assert!(
            diff.contains("-    println!(\"hello\");") && diff.contains("+    println!(\"bye\");"),
            "{diff}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_tool_can_leave_the_workspace() {
        use std::os::unix::fs::symlink;
        let f = fixture();
        symlink(&f.outside, f.ws.root().join("link")).unwrap();
        symlink(f.outside.parent().unwrap(), f.ws.root().join("up")).unwrap();
        let abs = f.outside.to_str().unwrap().to_string();
        for path in ["../secret.txt", abs.as_str(), "link", "up/secret.txt", ".git/config"] {
            for (tool, args) in [
                ("read_file", json!({"path": path})),
                ("write_file", json!({"path": path, "content": "pwned"})),
                ("edit_file", json!({"path": path, "old_string": "outside", "new_string": "pwned"})),
                ("list_files", json!({"path": path})),
                ("search", json!({"pattern": "secret|hello", "path": path})),
            ] {
                let out = call(&f, tool, args).await;
                assert!(out.is_error, "{tool} on {path} was allowed: {}", out.content);
                assert!(!out.content.contains("outside secret"), "{tool} on {path} leaked content");
            }
        }
        assert_eq!(std::fs::read_to_string(&f.outside).unwrap(), "outside secret\n");
        assert_eq!(std::fs::read_to_string(f.ws.root().join(".git/config")).unwrap(), "hello git\n");
        // Walking tools do not follow links out either.
        let all = call(&f, "search", json!({"pattern": "secret"})).await;
        assert_eq!(all.content, "no matches");
        let listing = call(&f, "list_files", json!({"path": ".", "depth": 5})).await;
        assert!(!listing.content.contains("secret.txt"), "{}", listing.content);
    }
}
