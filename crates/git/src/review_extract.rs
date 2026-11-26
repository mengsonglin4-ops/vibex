use super::review_snapshot::{MAX_BYTES, git_header_path};
use std::cell::Cell;
use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[path = "review_extract/guarded.rs"]
mod guarded;

type Result<T> = std::result::Result<T, &'static str>;
const UNSAFE: &str = "Cannot establish a safe, exact regular-file selection.";
const UNSUPPORTED: &str =
    "This Git layout, file type or staging state is not supported for review.";
const LIMIT: &str = "The selection exceeds the model review size or processing limit.";
const GIT: &str = "Git could not verify the selected file side locally.";
const MAX_INDEX: usize = 8 * 1024 * 1024;
const MAX_FILE: usize = 8 * 1024 * 1024;
const MAX_INPUT: usize = 32 * 1024 * 1024;
const PACK_DIR: &str = ".git/objects/pack";
const MAX_PACK_BYTES: u64 = 96 * 1024 * 1024;
const MAX_PACK_PAIRS: usize = 8;
const PACK_LIMIT: &str =
    "Local Git packs exceed the review limit (8 pairs, 96 MiB total, 128 directory entries).";
const MISSING_PACK: &str = "The requested Git object or its local pack/index pair is missing or unreadable; nothing was fetched.";

pub(super) fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && path.split('/').count() <= 32
        && !path
            .chars()
            .any(|ch| ch.is_control() || "\\:*?[]<>|\"~".contains(ch))
        && path.split('/').all(|part| {
            let stem = part
                .split('.')
                .next()
                .unwrap_or("")
                .trim_end()
                .to_ascii_uppercase();
            !part.is_empty()
                && !part.ends_with([' ', '.'])
                && !part.eq_ignore_ascii_case(".git")
                && !matches!(
                    stem.as_str(),
                    "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
                )
                && !["COM", "LPT"].iter().any(|prefix| {
                    stem.strip_prefix(prefix).is_some_and(|suffix| {
                        matches!(
                            suffix,
                            "0" | "1"
                                | "2"
                                | "3"
                                | "4"
                                | "5"
                                | "6"
                                | "7"
                                | "8"
                                | "9"
                                | "¹"
                                | "²"
                                | "³"
                        )
                    })
                })
        })
}

fn bounded(file: File, limit: usize) -> Result<Vec<u8>> {
    if file.metadata().map_err(|_| UNSAFE)?.len() > limit as u64 {
        return Err(LIMIT);
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| UNSAFE)?;
    if bytes.len() > limit {
        Err(LIMIT)
    } else {
        Ok(bytes)
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new_outside(root: &Path) -> Result<Self> {
        let base = std::env::temp_dir();
        if !base.is_absolute()
            || base
                .canonicalize()
                .map_err(|_| GIT)?
                .starts_with(root.canonicalize().map_err(|_| UNSAFE)?)
        {
            return Err(UNSAFE);
        }
        let builder = fs::DirBuilder::new();
        for sequence in 0..8 {
            let path = base.join(format!(
                "vibex-review-{}-{}-{sequence}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| GIT)?
                    .as_nanos()
            ));
            match builder.create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(GIT),
            }
        }
        Err(GIT)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Repo<'a> {
    root: &'a Path,
    git: PathBuf,
    scratch: Scratch,
    deadline: Instant,
    remaining: Cell<usize>,
    packs_loaded: Cell<bool>,
}

impl Repo<'_> {
    fn charge(&self, bytes: usize) -> Result<()> {
        self.remaining
            .set(self.remaining.get().checked_sub(bytes).ok_or(LIMIT)?);
        Ok(())
    }

    fn run(&self, args: &[&str], limit: usize, expected: i32) -> Result<Vec<u8>> {
        self.run_with_input(args, limit, expected, Stdio::null())
    }

    fn run_with_input(
        &self,
        args: &[&str],
        limit: usize,
        expected: i32,
        input: Stdio,
    ) -> Result<Vec<u8>> {
        let mut command = Command::new(&self.git);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let scratch = &self.scratch.0;
        command.env_clear();
        for key in ["SystemRoot", "WINDIR", "PATH", "TMP", "TEMP", "TMPDIR"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .current_dir(scratch)
            .env("GIT_DIR", scratch.join("meta"))
            .env("GIT_WORK_TREE", scratch.join("work"))
            .env("HOME", scratch)
            .env("XDG_CONFIG_HOME", scratch)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", scratch.join("empty"))
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_ALLOW_PROTOCOL", "")
            .env("GIT_ALLOC_LIMIT", "100663296")
            .args([
                "--no-pager",
                "--no-lazy-fetch",
                "--no-replace-objects",
                "--literal-pathspecs",
            ])
            .args(args)
            .stdin(input)
            .stderr(Stdio::null())
            .stdout(Stdio::piped());
        if Instant::now() >= self.deadline {
            return Err(LIMIT);
        }
        let mut child = command.spawn().map_err(|_| GIT)?;
        let stdout = child.stdout.take().ok_or(GIT)?;
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout
                .take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        });
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < self.deadline => {
                    std::thread::sleep(Duration::from_millis(3))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let bytes = reader.join().map_err(|_| GIT)?.map_err(|_| GIT)?;
        if bytes.len() > limit || status.is_none() {
            return Err(LIMIT);
        }
        if status.and_then(|value| value.code()) != Some(expected) {
            return Err(GIT);
        }
        Ok(bytes)
    }

    fn load_packs(&self) -> Result<()> {
        if self.packs_loaded.get() {
            return Ok(());
        }
        let _pins = guarded::pin_directory(self.root, PACK_DIR)?.ok_or(MISSING_PACK)?;
        let mut pairs = std::collections::BTreeMap::new();
        for (index, entry) in fs::read_dir(self.root.join(PACK_DIR))
            .map_err(|_| UNSAFE)?
            .enumerate()
        {
            if Instant::now() >= self.deadline {
                return Err(LIMIT);
            }
            if index >= 128 {
                return Err(PACK_LIMIT);
            }
            let name = entry.map_err(|_| UNSAFE)?.file_name();
            let Some((stem, ext)) = name.to_str().and_then(|name| name.rsplit_once('.')) else {
                continue;
            };
            let bit = match ext {
                "idx" => 1,
                "pack" => 2,
                _ => continue,
            };
            if !stem.strip_prefix("pack-").is_some_and(|oid| {
                valid_oid(oid) && !oid.bytes().any(|byte| byte.is_ascii_uppercase())
            }) {
                continue;
            }
            *pairs.entry(stem.to_owned()).or_insert(0u8) |= bit;
            if pairs.len() > MAX_PACK_PAIRS {
                return Err(PACK_LIMIT);
            }
        }
        if pairs.is_empty() || pairs.values().any(|bits| *bits != 3) {
            return Err(MISSING_PACK);
        }
        let mut files = Vec::new();
        let mut remaining = MAX_PACK_BYTES;
        for stem in pairs.into_keys() {
            for ext in ["idx", "pack"] {
                let name = format!("{stem}.{ext}");
                let file =
                    guarded::open(self.root, &format!("{PACK_DIR}/{name}"))?.ok_or(MISSING_PACK)?;
                let size = file.metadata().map_err(|_| UNSAFE)?.len();
                remaining = remaining.checked_sub(size).ok_or(PACK_LIMIT)?;
                files.push((name, file, size));
            }
        }
        let dest = self.scratch.0.join("meta/objects/pack");
        fs::create_dir(&dest).map_err(|_| GIT)?;
        let mut buffer = vec![0u8; 128 * 1024];
        for (name, mut file, mut left) in files {
            let mut output = File::create_new(dest.join(name)).map_err(|_| GIT)?;
            while left > 0 {
                if Instant::now() >= self.deadline {
                    return Err(LIMIT);
                }
                let chunk = left.min(buffer.len() as u64) as usize;
                file.read_exact(&mut buffer[..chunk]).map_err(|_| UNSAFE)?;
                output.write_all(&buffer[..chunk]).map_err(|_| GIT)?;
                left -= chunk as u64;
            }
        }
        self.packs_loaded.set(true);
        Ok(())
    }

    fn object(&self, oid: &str, kind: &str, limit: usize) -> Result<Vec<u8>> {
        let limit = limit.min(self.remaining.get());
        if !valid_oid(oid) {
            return Err(GIT);
        }
        let loose = format!("objects/{}/{}", &oid[..2], &oid[2..]);
        let dest = self.scratch.0.join("meta").join(&loose);
        if !dest.exists() {
            if let Some(source) = guarded::open(self.root, &format!(".git/{loose}"))? {
                fs::create_dir_all(dest.parent().ok_or(GIT)?).map_err(|_| GIT)?;
                fs::write(&dest, bounded(source, limit + 64 * 1024)?).map_err(|_| GIT)?;
            } else {
                self.load_packs()?;
            }
        }
        let size = self.run(&["cat-file", "-s", oid], 32, 0).map_err(|error| {
            if error == GIT && self.packs_loaded.get() {
                MISSING_PACK
            } else {
                error
            }
        })?;
        let size: usize = std::str::from_utf8(&size)
            .ok()
            .and_then(|s| s.trim_end().parse().ok())
            .ok_or(GIT)?;
        if size > limit {
            return Err(LIMIT);
        }
        self.charge(size)?;
        if self.run(&["cat-file", "-t", oid], 16, 0)? != format!("{kind}\n").as_bytes() {
            return Err(GIT);
        }
        let bytes = self.run(&["cat-file", kind, oid], size, 0)?;
        if bytes.len() != size {
            return Err(GIT);
        }
        fs::write(self.scratch.0.join("verify"), &bytes).map_err(|_| GIT)?;
        if self.run(
            &["hash-object", "--no-filters", "-t", kind, "--", "verify"],
            42,
            0,
        )? != format!("{oid}\n").as_bytes()
        {
            return Err(GIT);
        }
        Ok(bytes)
    }

    fn blob(&self, oid: Option<&str>) -> Result<Option<Vec<u8>>> {
        oid.map(|oid| self.object(oid, "blob", MAX_FILE))
            .transpose()
    }

    fn head_entry(&self, tree: Option<&str>, path: &str) -> Result<Option<String>> {
        let Some(tree) = tree else {
            return Ok(None);
        };
        let mut tree = tree.to_owned();
        let mut components = path.split('/').peekable();
        while let Some(name) = components.next() {
            self.object(&tree, "tree", 256 * 1024)?;
            let data = self.run(&["ls-tree", "-z", &tree, "--", name], name.len() + 128, 0)?;
            if data.is_empty() {
                return Ok(None);
            }
            let (header, entry) = std::str::from_utf8(&data)
                .ok()
                .and_then(|s| s.strip_suffix('\0'))
                .and_then(|s| s.split_once('\t'))
                .ok_or(GIT)?;
            if entry != name {
                return Err(UNSAFE);
            }
            let fields: Vec<_> = header.split(' ').collect();
            match (components.peek(), fields.as_slice()) {
                (None, ["100644", "blob", oid]) if valid_oid(oid) => {
                    return Ok(Some((*oid).to_owned()));
                }
                (Some(_), ["040000", "tree", oid]) if valid_oid(oid) => tree = (*oid).to_owned(),
                _ => return Err(UNSUPPORTED),
            }
        }
        Err(UNSAFE)
    }

    fn patch(&self, path: &str, old: Option<&[u8]>, new: Option<&[u8]>) -> Result<String> {
        if old == new {
            return Err(UNSUPPORTED);
        }
        for bytes in [old, new].into_iter().flatten() {
            if std::str::from_utf8(bytes).is_err()
                || bytes
                    .iter()
                    .any(|byte| byte.is_ascii_control() && !matches!(byte, b'\n' | b'\r' | b'\t'))
            {
                return Err(UNSUPPORTED);
            }
        }
        for (name, bytes) in [("before", old), ("after", new)] {
            fs::write(self.scratch.0.join(name), bytes.unwrap_or_default()).map_err(|_| GIT)?;
        }
        let patch = self.run(
            &[
                "diff",
                "--no-index",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--no-renames",
                "--",
                "before",
                "after",
            ],
            MAX_BYTES,
            1,
        )?;
        let patch = std::str::from_utf8(&patch).map_err(|_| GIT)?;
        let (_, hunks) = patch
            .split_once("\n--- a/before\n+++ b/after\n")
            .ok_or(UNSUPPORTED)?;
        if !hunks.starts_with("@@ -") {
            return Err(UNSUPPORTED);
        }
        let first = git_header_path("a", path, false);
        let second = git_header_path("b", path, false);
        Ok(format!(
            "diff --git {first} {second}\n--- {}\n+++ {}\n{hunks}",
            if old.is_some() { &first } else { "/dev/null" },
            if new.is_some() { &second } else { "/dev/null" }
        ))
    }
}

fn valid_oid(value: &str) -> bool {
    value.len() == 40
        && value.bytes().all(|byte| byte.is_ascii_hexdigit())
        && !value.bytes().all(|byte| byte == b'0')
}

fn index_entries(root: &Path, selection: &[(&str, bool)]) -> Result<Vec<Option<String>>> {
    let mut entries = vec![None; selection.len()];
    let Some(file) = guarded::open(root, ".git/index")? else {
        return Ok(entries);
    };
    let bytes = bounded(file, MAX_INDEX)?;
    let (data, _) = bytes
        .split_at_checked(bytes.len().checked_sub(20).ok_or(GIT)?)
        .ok_or(GIT)?;
    let (header, mut rest) = data.split_at_checked(12).ok_or(GIT)?;
    if &header[..8] != b"DIRC\0\0\0\x02" {
        return Err(UNSUPPORTED);
    }
    let count = u32::from_be_bytes(header[8..12].try_into().map_err(|_| GIT)?) as usize;
    if count > rest.len() / 64 {
        return Err(GIT);
    }
    for _ in 0..count {
        let (fields, tail) = rest.split_at_checked(62).ok_or(GIT)?;
        let flags = u16::from_be_bytes([fields[60], fields[61]]);
        if flags & 0x4000 != 0 {
            return Err(UNSUPPORTED);
        }
        let length = tail.iter().position(|b| *b == 0).ok_or(GIT)?;
        if length.min(0xfff) != usize::from(flags & 0xfff) {
            return Err(GIT);
        }
        let name = &tail[..length];
        for ((path, _), entry) in selection.iter().zip(&mut entries) {
            let selected = path.as_bytes();
            if name
                .strip_prefix(selected)
                .is_some_and(|suffix| suffix.starts_with(b"/"))
                || selected
                    .strip_prefix(name)
                    .is_some_and(|suffix| suffix.starts_with(b"/"))
            {
                return Err(UNSAFE);
            }
            if name.eq_ignore_ascii_case(selected) {
                let mode = u32::from_be_bytes(fields[24..28].try_into().map_err(|_| GIT)?);
                if name != selected || mode != 0o100644 || flags & 0xf000 != 0 || entry.is_some() {
                    return Err(UNSUPPORTED);
                }
                *entry = Some(
                    fields[40..60]
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                );
            }
        }
        rest = rest.get((63 + length).next_multiple_of(8)..).ok_or(GIT)?;
    }
    while !rest.is_empty() {
        let (header, tail) = rest.split_at_checked(8).ok_or(GIT)?;
        if !header[0].is_ascii_uppercase() {
            return Err(UNSUPPORTED);
        }
        rest = tail
            .get(u32::from_be_bytes(header[4..8].try_into().map_err(|_| GIT)?) as usize..)
            .ok_or(GIT)?;
    }
    Ok(entries)
}

fn normalize_checkout(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut normalized = Vec::with_capacity(bytes.len());
    let mut saw_crlf = false;
    let mut saw_lf = false;
    let mut iter = bytes.iter().copied();
    while let Some(byte) = iter.next() {
        if byte == b'\r' {
            if iter.next() != Some(b'\n') {
                return Err(UNSUPPORTED);
            }
            saw_crlf = true;
            normalized.push(b'\n');
        } else {
            saw_lf |= byte == b'\n';
            normalized.push(byte);
        }
    }
    if saw_crlf && saw_lf {
        Err(UNSUPPORTED)
    } else {
        Ok(normalized)
    }
}

fn head(root: &Path) -> Result<Option<String>> {
    let read = |path: &str, limit| -> Result<Option<String>> {
        guarded::open(root, path)?
            .map(|file| String::from_utf8(bounded(file, limit)?).map_err(|_| GIT))
            .transpose()
    };
    let head = read(".git/HEAD", 1024)?.ok_or(GIT)?;
    let head = head.trim_end();
    if valid_oid(head) {
        return Ok(Some(head.to_owned()));
    }
    let reference = head
        .strip_prefix("ref: ")
        .filter(|name| name.starts_with("refs/heads/") && valid_path(name))
        .ok_or(UNSUPPORTED)?;
    if let Some(oid) = read(&format!(".git/{reference}"), 128)? {
        return valid_oid(oid.trim_end())
            .then(|| Some(oid.trim_end().to_owned()))
            .ok_or(GIT);
    }
    if let Some(packed) = read(".git/packed-refs", MAX_INDEX)? {
        for line in packed.lines() {
            if let Some((oid, name)) = line.split_once(' ')
                && name == reference
            {
                return valid_oid(oid).then(|| Some(oid.to_owned())).ok_or(GIT);
            }
        }
    }
    Ok(None)
}

pub(super) fn extract<'a>(
    root: &Path,
    selection: impl Iterator<Item = (&'a str, bool)>,
) -> Result<Vec<String>> {
    if !root.is_absolute()
        || root
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(UNSAFE);
    }
    let selection: Vec<_> = selection.collect();
    if fs::symlink_metadata(root.join(".git")).is_ok_and(|metadata| metadata.is_file()) {
        return Err(
            "Model review requires a .git directory; linked worktrees and other .git pointer files are not supported.",
        );
    }
    let canonical = root.canonicalize().map_err(|_| UNSAFE)?;
    let git = std::env::split_paths(&std::env::var_os("PATH").ok_or(GIT)?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(if cfg!(windows) { "git.exe" } else { "git" }))
        .filter_map(|file| file.canonicalize().ok())
        .find(|file| file.is_file() && !file.starts_with(&canonical))
        .ok_or(GIT)?;
    let repo = Repo {
        root,
        git,
        scratch: Scratch::new_outside(root)?,
        deadline: Instant::now() + Duration::from_secs(15),
        remaining: Cell::new(MAX_INPUT),
        packs_loaded: Cell::new(false),
    };
    let scratch = &repo.scratch.0;
    for dir in ["meta/refs", "meta/objects", "work"] {
        fs::create_dir_all(scratch.join(dir)).map_err(|_| GIT)?;
    }
    fs::write(scratch.join("meta/HEAD"), b"ref: refs/heads/review\n").map_err(|_| GIT)?;
    if guarded::open(root, ".git/commondir")?.is_some()
        || guarded::open(root, ".git/reftable")?.is_some()
    {
        return Err(UNSUPPORTED);
    }
    let config = guarded::open(root, ".git/config")?.ok_or(UNSUPPORTED)?;
    if config.metadata().map_err(|_| GIT)?.len() > MAX_BYTES as u64 {
        return Err(LIMIT);
    }
    let layout = repo.run_with_input(
        &[
            "config",
            "--no-includes",
            "--file",
            "-",
            "--null",
            "--get-regexp",
            r"^(core\.(bare|worktree|repositoryformatversion)|extensions\..*|include(if)?\..*)$",
        ],
        1024,
        0,
        config.into(),
    )?;
    let mut worktree = false;
    for key in layout.strip_suffix(b"\0").ok_or(GIT)?.split(|b| *b == 0) {
        match key {
            b"core.bare\nfalse" => worktree = true,
            b"core.repositoryformatversion\n0" | b"core.repositoryformatversion\n1" => (),
            _ => return Err(UNSUPPORTED),
        }
    }
    if !worktree {
        return Err(UNSUPPORTED);
    }
    let entries = index_entries(root, &selection)?;
    let head = head(root)?;
    let tree = if selection.iter().any(|(_, staged)| *staged) {
        head.as_deref()
            .map(|oid| {
                let commit = repo.object(oid, "commit", MAX_BYTES)?;
                std::str::from_utf8(commit.split(|byte| *byte == b'\n').next().ok_or(GIT)?)
                    .ok()
                    .and_then(|line| line.strip_prefix("tree "))
                    .filter(|oid| valid_oid(oid))
                    .map(str::to_owned)
                    .ok_or(GIT)
            })
            .transpose()?
    } else {
        None
    };
    let mut patches = Vec::new();
    let mut total = 0;
    for ((path, staged), index) in selection.into_iter().zip(entries) {
        let (mut old, mut new) = if staged {
            let previous = repo.head_entry(tree.as_deref(), path)?;
            (
                repo.blob(previous.as_deref())?,
                repo.blob(index.as_deref())?,
            )
        } else {
            if index.is_none() {
                return Err(
                    "Stage this untracked file before review; its ignore and repository boundaries cannot be verified.",
                );
            }
            let file = guarded::open(root, path)?;
            let old = repo.blob(index.as_deref())?;
            let new = file
                .map(|file| bounded(file, MAX_FILE.min(repo.remaining.get())))
                .transpose()?;
            if let Some(bytes) = &new {
                repo.charge(bytes.len())?;
            }
            (old, new)
        };
        if !staged {
            for side in [&mut old, &mut new].into_iter().flatten() {
                *side = normalize_checkout(side)?;
            }
        }
        let patch = repo.patch(path, old.as_deref(), new.as_deref())?;
        total += patch.len();
        if total > MAX_BYTES {
            return Err(LIMIT);
        }
        patches.push(patch);
    }
    Ok(patches)
}

#[cfg(test)]
#[path = "review_extract/tests.rs"]
mod tests;
