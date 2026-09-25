use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd, IntoRawFd},
    path::{Component, Path},
};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const OUTPUT_LIMIT: usize = 262_144;
const SCAN_LIMIT: usize = 4 * 1024 * 1024;
const MAX_DEPTH: usize = 32;
const DEFAULT_EXCLUDES: [&str; 3] = [".git", ".env", ".asyntalc"];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub root: String,
    #[serde(default = "default_operations")]
    pub operations: Vec<String>,
    #[serde(default = "default_exclude")]
    pub exclude: Vec<String>,
    #[serde(default = "default_file_bytes")]
    pub max_file_bytes: u64,
    #[serde(default = "default_entries")]
    pub max_entries: usize,
    #[serde(default = "default_results")]
    pub max_results: usize,
}

fn default_operations() -> Vec<String> {
    ["read_file", "list_files", "search"]
        .map(str::to_owned)
        .into()
}
fn default_exclude() -> Vec<String> {
    DEFAULT_EXCLUDES.map(str::to_owned).into()
}
fn default_file_bytes() -> u64 {
    65_536
}
fn default_entries() -> usize {
    1000
}
fn default_results() -> usize {
    100
}

impl WorkspaceConfig {
    pub fn validate(&self) -> Result<()> {
        if !Path::new(&self.root).is_absolute()
            || self.root.len() > 4096
            || self.root.split('/').any(|part| part == "." || part == "..")
            || Path::new(&self.root)
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
        {
            bail!("workspace root must be an absolute directory without traversal");
        }
        if self.operations.is_empty()
            || self.operations.len() > 3
            || self
                .operations
                .iter()
                .enumerate()
                .any(|(index, op)| self.operations[..index].contains(op))
            || self
                .operations
                .iter()
                .any(|op| !["read_file", "list_files", "search"].contains(&op.as_str()))
        {
            bail!("workspace operations must contain read_file, list_files, or search");
        }
        if !(1..=262_144).contains(&self.max_file_bytes)
            || !(1..=10_000).contains(&self.max_entries)
            || !(1..=1000).contains(&self.max_results)
        {
            bail!("workspace limits are outside the supported bounds");
        }
        if self.exclude.len() > 128
            || self
                .exclude
                .iter()
                .any(|p| relative(p).is_err() || p == ".")
        {
            bail!("workspace exclusions must be relative component names or path prefixes");
        }
        open_root(&self.root).map_err(|_| {
            anyhow::anyhow!(
                "workspace root cannot be opened without symlinks (Linux openat2 required)"
            )
        })?;
        Ok(())
    }

    pub fn permits(&self, name: &str) -> bool {
        name.strip_prefix("workspace_")
            .is_some_and(|op| self.operations.iter().any(|allowed| allowed == op))
    }

    pub fn tool_definitions(&self) -> Vec<Value> {
        self.operations.iter().map(|op| {
            let mut properties = json!({"path":{"type":"string","description":"Relative workspace path; use . for the root. Symlinks and excluded paths are unavailable."}});
            let mut required = vec!["path"];
            if op == "search" {
                properties["query"] = json!({"type":"string","description":"Case-sensitive literal substring (not a regular expression)."});
                required.push("query");
            }
            let description = match op.as_str() {
                "read_file" => "Read one bounded UTF-8 file and return its SHA-256 hash. Files are live, not a snapshot.",
                "list_files" => "Recursively list permitted workspace files and directories, within configured limits.",
                _ => "Search permitted UTF-8 files recursively for a literal substring. Return matching paths, line numbers, and SHA-256 hashes.",
            };
            json!({"type":"function","function":{"name":format!("workspace_{op}"),"description":description,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})
        }).collect()
    }

    fn excluded(&self, path: &str) -> bool {
        self.exclude
            .iter()
            .map(String::as_str)
            .chain(DEFAULT_EXCLUDES)
            .any(|prefix| {
                if prefix.contains('/') {
                    path == prefix
                        || path
                            .strip_prefix(prefix)
                            .is_some_and(|suffix| suffix.starts_with('/'))
                } else {
                    path.split('/').any(|part| part == prefix)
                }
            })
    }
}

pub struct Workspace {
    config: WorkspaceConfig,
    root: File,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    path: String,
    query: String,
}

impl Workspace {
    pub fn open(config: WorkspaceConfig) -> Result<Self> {
        config.validate()?;
        let root = open_root(&config.root)
            .map_err(|_| anyhow::anyhow!("workspace root is unavailable"))?;
        Ok(Self { config, root })
    }

    pub fn execute(&self, name: &str, arguments: &str) -> Value {
        if !self.config.permits(name) {
            return error("operation_denied");
        }
        if arguments.len() > 16_384 {
            return error("invalid_arguments");
        }
        let result = if name == "workspace_search" {
            match serde_json::from_str::<SearchArgs>(arguments) {
                Ok(args) if !args.query.is_empty() && args.query.len() <= 1024 => {
                    self.walk(&args.path, Some(&args.query))
                }
                _ => Err("invalid_arguments"),
            }
        } else {
            match serde_json::from_str::<PathArgs>(arguments) {
                Ok(args) if name == "workspace_read_file" => self.read_result(&args.path),
                Ok(args) => self.walk(&args.path, None),
                Err(_) => Err("invalid_arguments"),
            }
        };
        let value = result.unwrap_or_else(error);
        if serde_json::to_vec(&value).is_ok_and(|bytes| bytes.len() <= OUTPUT_LIMIT) {
            value
        } else {
            error("output_limit")
        }
    }

    fn handle(&self, path: &str) -> Result<File, &'static str> {
        relative(path)?;
        if self.config.excluded(path) {
            return Err("path_excluded");
        }
        open_beneath(&self.root, path, libc::O_PATH).map_err(|_| "path_unavailable")
    }

    fn read(&self, handle: File) -> Result<String, &'static str> {
        let metadata = handle.metadata().map_err(|_| "path_unavailable")?;
        if !metadata.is_file() {
            return Err("not_regular_file");
        }
        if metadata.len() > self.config.max_file_bytes {
            return Err("file_limit");
        }
        // This trusted procfs path reopens the already verified inode, not user input.
        let file = File::open(format!("/proc/self/fd/{}", handle.as_raw_fd()))
            .map_err(|_| "path_unavailable")?;
        let mut bytes = Vec::new();
        file.take(self.config.max_file_bytes + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "read_failed")?;
        if bytes.len() as u64 > self.config.max_file_bytes {
            return Err("file_limit");
        }
        String::from_utf8(bytes).map_err(|_| "not_utf8")
    }

    fn read_result(&self, path: &str) -> Result<Value, &'static str> {
        let text = self.read(self.handle(path)?)?;
        Ok(
            json!({"ok":true,"path":path,"bytes":text.len(),"sha256":hash(&text),"content":text,"live":true}),
        )
    }

    fn walk(&self, path: &str, query: Option<&str>) -> Result<Value, &'static str> {
        let initial = self.handle(path)?;
        if !initial.metadata().map_err(|_| "path_unavailable")?.is_dir() {
            return Err("not_directory");
        }
        let mut state = Walk {
            entries: Vec::new(),
            visited: 0,
            scanned: 0,
            read_budget: 0,
            truncated: false,
            skipped: 0,
        };
        self.visit(path, query, 0, &mut state)?;
        Ok(
            json!({"ok":true,"path":path,"entries":state.entries,"visited":state.visited,"scanned_bytes":state.scanned,"truncated":state.truncated,"skipped":state.skipped,"live":true}),
        )
    }

    fn visit(
        &self,
        path: &str,
        query: Option<&str>,
        depth: usize,
        state: &mut Walk,
    ) -> Result<(), &'static str> {
        if depth >= MAX_DEPTH {
            state.truncated = true;
            return Ok(());
        }
        let file = open_beneath(&self.root, path, libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(|_| "path_unavailable")?;
        let directory = Directory::new(file).map_err(|_| "path_unavailable")?;
        loop {
            if state.visited >= self.config.max_entries
                || state.entries.len() >= self.config.max_results
                || state.read_budget >= SCAN_LIMIT
            {
                state.truncated = true;
                break;
            }
            let Some(name) = directory.next().map_err(|_| "read_failed")? else {
                break;
            };
            if name.as_bytes() == b"." || name.as_bytes() == b".." {
                continue;
            }
            state.visited += 1;
            let name = match name.into_string() {
                Ok(name) => name,
                Err(_) => {
                    state.skipped += 1;
                    continue;
                }
            };
            let child = if path == "." {
                name
            } else {
                format!("{path}/{name}")
            };
            let handle = match self.handle(&child) {
                Ok(handle) => handle,
                Err(_) => {
                    state.skipped += 1;
                    continue;
                }
            };
            let metadata = match handle.metadata() {
                Ok(metadata) => metadata,
                Err(_) => {
                    state.skipped += 1;
                    continue;
                }
            };
            if metadata.is_dir() {
                if query.is_none() {
                    state.entries.push(json!({"path":child,"type":"directory"}));
                }
                if self.visit(&child, query, depth + 1, state).is_err() {
                    state.skipped += 1;
                }
            } else if metadata.is_file() {
                if let Some(query) = query {
                    if metadata.len() > self.config.max_file_bytes {
                        state.skipped += 1;
                        continue;
                    }
                    let reserve = self.config.max_file_bytes as usize + 1;
                    if reserve > SCAN_LIMIT - state.read_budget {
                        state.truncated = true;
                        break;
                    }
                    state.read_budget += reserve;
                    match self.read(handle) {
                        Ok(text) => {
                            state.read_budget -= reserve - text.len();
                            state.scanned += text.len();
                            let mut lines = text
                                .lines()
                                .enumerate()
                                .filter(|(_, line)| line.contains(query))
                                .map(|(line, _)| line + 1);
                            let matches: Vec<_> = lines.by_ref().take(100).collect();
                            let more_matches = lines.next().is_some();
                            if !matches.is_empty() {
                                state.entries.push(json!({"path":child,"sha256":hash(&text),"lines":matches,"more_matches":more_matches}));
                            }
                        }
                        Err(_) => {
                            state.skipped += 1;
                        }
                    }
                } else {
                    state
                        .entries
                        .push(json!({"path":child,"type":"file","bytes":metadata.len()}));
                }
            } else {
                state.skipped += 1;
            }
        }
        Ok(())
    }
}

struct Walk {
    entries: Vec<Value>,
    visited: usize,
    scanned: usize,
    read_budget: usize,
    truncated: bool,
    skipped: usize,
}
fn error(code: &str) -> Value {
    json!({"ok":false,"error":{"code":code}})
}
fn hash(text: &str) -> String {
    ring::digest::digest(&ring::digest::SHA256, text.as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn relative(path: &str) -> Result<(), &'static str> {
    if path == "." {
        return Ok(());
    }
    if path.is_empty()
        || path.len() > 4096
        || path.as_bytes().contains(&0)
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("invalid_path");
    }
    Ok(())
}

#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

fn open_root(path: &str) -> io::Result<File> {
    open_at(libc::AT_FDCWD, path, libc::O_PATH | libc::O_DIRECTORY, 0x04)
}

fn open_beneath(root: &File, path: &str, flags: i32) -> io::Result<File> {
    open_at(root.as_raw_fd(), path, flags, 0x08 | 0x04 | 0x01)
}

fn open_at(dir: i32, path: &str, flags: i32, resolve: u64) -> io::Result<File> {
    let path = CString::new(path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let how = OpenHow {
        flags: (flags | libc::O_CLOEXEC) as u64,
        mode: 0,
        resolve,
    };
    // The kernel reads the NUL-terminated path and fully initialized open_how.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir,
            path.as_ptr(),
            &how,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd as i32) })
    }
}

struct Directory(*mut libc::DIR);
impl Directory {
    fn new(file: File) -> io::Result<Self> {
        let fd = file.into_raw_fd();
        let dir = unsafe { libc::fdopendir(fd) };
        if dir.is_null() {
            let error = io::Error::last_os_error();
            unsafe {
                libc::close(fd);
            }
            Err(error)
        } else {
            Ok(Self(dir))
        }
    }
    fn next(&self) -> io::Result<Option<CString>> {
        unsafe {
            *libc::__errno_location() = 0;
            let entry = libc::readdir(self.0);
            if entry.is_null() {
                let code = *libc::__errno_location();
                if code == 0 {
                    Ok(None)
                } else {
                    Err(io::Error::from_raw_os_error(code))
                }
            } else {
                Ok(Some(CStr::from_ptr((*entry).d_name.as_ptr()).to_owned()))
            }
        }
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    fn config(root: &Path) -> WorkspaceConfig {
        serde_json::from_value(json!({"root":root.to_str().unwrap()})).unwrap()
    }
    fn call(workspace: &Workspace, op: &str, path: &str) -> Value {
        workspace.execute(op, &json!({"path":path}).to_string())
    }

    #[test]
    fn reads_hashes_and_searches_live_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("hello.txt"), "hello\nworld").unwrap();
        let workspace = Workspace::open(config(dir.path())).unwrap();
        let read = call(&workspace, "workspace_read_file", "hello.txt");
        assert_eq!(read["content"], "hello\nworld");
        assert_eq!(
            read["sha256"],
            "26c60a61d01db5836ca70fefd44a6a016620413c8ef5f259a6c5612d4f79d3b8"
        );
        let search = workspace.execute("workspace_search", r#"{"path":".","query":"world"}"#);
        assert_eq!(search["entries"][0]["lines"], json!([2]));
        fs::write(dir.path().join("hello.txt"), "changed").unwrap();
        assert_eq!(
            call(&workspace, "workspace_read_file", "hello.txt")["content"],
            "changed"
        );
    }

    #[test]
    fn confines_paths_and_rejects_symlinks_special_files_and_exclusions() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "secret").unwrap();
        symlink(outside.path(), dir.path().join("link")).unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/.env"), "secret").unwrap();
        fs::create_dir(dir.path().join("private")).unwrap();
        fs::write(dir.path().join("private/secret"), "secret").unwrap();
        let fifo = CString::new(dir.path().join("fifo").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let mut cfg = config(dir.path());
        cfg.exclude = vec!["private/secret".into()];
        let workspace = Workspace::open(cfg).unwrap();
        for path in [
            "../secret",
            "/etc/passwd",
            "link/secret",
            "nested/.env",
            "private/secret",
            "fifo",
            "nested/../private/secret",
        ] {
            assert_eq!(
                call(&workspace, "workspace_read_file", path)["ok"],
                false,
                "{path}"
            );
        }
        let listing = call(&workspace, "workspace_list_files", ".").to_string();
        assert!(!listing.contains("secret"));
        assert!(!listing.contains("fifo"));
    }

    #[test]
    fn enforces_arguments_capabilities_and_resource_bounds() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a"), "abcd").unwrap();
        fs::write(dir.path().join("b"), [255]).unwrap();
        let mut cfg = config(dir.path());
        cfg.max_file_bytes = 3;
        cfg.max_results = 1;
        cfg.operations = vec!["read_file".into(), "list_files".into()];
        let workspace = Workspace::open(cfg).unwrap();
        assert_eq!(
            call(&workspace, "workspace_read_file", "a")["error"]["code"],
            "file_limit"
        );
        assert_eq!(
            call(&workspace, "workspace_read_file", "b")["error"]["code"],
            "not_utf8"
        );
        assert_eq!(
            workspace.execute("workspace_search", r#"{"path":".","query":"a"}"#)["error"]["code"],
            "operation_denied"
        );
        assert_eq!(
            workspace.execute("workspace_read_file", r#"{"path":"a","extra":true}"#)["error"]["code"],
            "invalid_arguments"
        );
        let listing = call(&workspace, "workspace_list_files", ".");
        assert_eq!(listing["entries"].as_array().unwrap().len(), 1);
        assert_eq!(listing["truncated"], true);
        let mut duplicate = config(dir.path());
        duplicate.operations = vec!["read_file".into(), "read_file".into()];
        assert!(duplicate.validate().is_err());
        let mut dotted = config(dir.path());
        dotted.root.push_str("/.");
        assert!(dotted.validate().is_err());
    }

    #[test]
    fn held_root_survives_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a"), "original").unwrap();
        let workspace = Workspace::open(config(&root)).unwrap();
        fs::rename(&root, dir.path().join("old")).unwrap();
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a"), "replacement").unwrap();
        assert_eq!(
            call(&workspace, "workspace_read_file", "a")["content"],
            "original"
        );
    }

    #[test]
    fn rejects_symlink_roots_and_never_reads_a_swapped_symlink() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret"), "outside secret").unwrap();
        symlink(outside.path(), dir.path().join("root-link")).unwrap();
        assert!(Workspace::open(config(&dir.path().join("root-link"))).is_err());
        fs::write(dir.path().join("target"), "allowed").unwrap();
        let workspace = Workspace::open(config(dir.path())).unwrap();
        let stopping = Arc::new(AtomicBool::new(false));
        let stopped = stopping.clone();
        let root = dir.path().to_owned();
        let secret = outside.path().join("secret");
        let swapper = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                fs::write(root.join("replacement"), "allowed").unwrap();
                fs::rename(root.join("replacement"), root.join("target")).unwrap();
                symlink(&secret, root.join("replacement")).unwrap();
                fs::rename(root.join("replacement"), root.join("target")).unwrap();
            }
        });
        let mut leaked = false;
        for _ in 0..500 {
            let value = call(&workspace, "workspace_read_file", "target");
            leaked |= value["ok"] == true && value["content"] != "allowed";
        }
        stopping.store(true, Ordering::Relaxed);
        swapper.join().unwrap();
        assert!(!leaked);
    }

    #[test]
    fn bounds_escaped_output_and_recursive_scanning() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("escaped"), vec![0; 65_536]).unwrap();
        let workspace = Workspace::open(config(dir.path())).unwrap();
        assert_eq!(
            call(&workspace, "workspace_read_file", "escaped")["error"]["code"],
            "output_limit"
        );
        fs::remove_file(dir.path().join("escaped")).unwrap();
        for index in 0..100 {
            fs::write(dir.path().join(format!("file{index}")), vec![255; 65_536]).unwrap();
        }
        let result = workspace.execute("workspace_search", r#"{"path":".","query":"text"}"#);
        assert_eq!(result["truncated"], true);
        assert!(result["visited"].as_u64().unwrap() < 100);
        assert!(result["entries"].as_array().unwrap().is_empty());
    }
}
