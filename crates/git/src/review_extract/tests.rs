use super::*;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("not-configured"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim_end().into()
}

fn fixture() -> Scratch {
    let root = Scratch::new_outside(&std::env::current_exe().unwrap()).unwrap();
    git(&root.0, &["init", "--template="]);
    git(&root.0, &["config", "core.autocrlf", "false"]);
    root
}

fn set_fixture_head(root: &Path) {
    let tree = git(root, &["write-tree"]);
    fs::write(root.join("fixture-commit"), format!("tree {tree}\nauthor Test <test@example.invalid> 1 +0000\ncommitter Test <test@example.invalid> 1 +0000\n\nfixture\n")).unwrap();
    let oid = git(
        root,
        &["hash-object", "-w", "-t", "commit", "fixture-commit"],
    );
    fs::write(root.join(".git/HEAD"), format!("{oid}\n")).unwrap();
}

#[test]
fn refuses_nonliteral_and_ambiguous_paths() {
    for path in [
        "",
        ".",
        "..",
        "/file",
        "a//b",
        "a/../b",
        "a\\b",
        "a/",
        ":(glob)*",
        "a[1]",
        "C:/secret",
        "file:stream",
        ".GiT/config",
        "GIT~1/index",
        "CON.txt",
        "LPT¹",
        "a. ",
        "a\nb",
    ] {
        assert!(!valid_path(path), "{path:?}");
    }
    assert!(valid_path("nested/文 件.txt"));
}

#[test]
fn stages_are_separate_and_repository_helpers_are_never_used() {
    let root = fixture();
    let file = root.0.join("note");
    fs::write(&file, "base\n").unwrap();
    git(&root.0, &["add", "note"]);
    set_fixture_head(&root.0);
    fs::write(&file, "index\n").unwrap();
    git(&root.0, &["add", "note"]);
    fs::write(&file, "work\n").unwrap();
    fs::write(root.0.join("secret"), "unselected marker").unwrap();
    for key in [
        "diff.external",
        "diff.tripwire.textconv",
        "core.fsmonitor",
        "filter.tripwire.clean",
        "filter.tripwire.smudge",
    ] {
        git(&root.0, &["config", key, "false"]);
    }
    git(&root.0, &["config", "filter.tripwire.required", "true"]);
    fs::write(
        root.0.join(".gitattributes"),
        "* diff=tripwire filter=tripwire\n",
    )
    .unwrap();
    let patches = extract(&root.0, [("note", true), ("note", false)].into_iter()).unwrap();
    fs::write(file, "later\n").unwrap();
    assert!(patches[0].contains("-base\n+index\n"));
    assert!(patches[1].contains("-index\n+work\n"));
    assert!(!patches.join("").contains("unselected marker"));
    assert!(extract(&root.0, [("secret", false)].into_iter()).is_err());
}

#[test]
fn autocrlf_checkout_does_not_create_a_whole_file_patch() {
    let root = fixture();
    git(&root.0, &["config", "core.autocrlf", "true"]);
    let file = root.0.join("note");
    fs::write(&file, "one\ntwo\nthree\n").unwrap();
    git(&root.0, &["add", "note"]);
    fs::remove_file(&file).unwrap();
    git(&root.0, &["checkout-index", "--force", "--", "note"]);
    assert_eq!(fs::read(&file).unwrap(), b"one\r\ntwo\r\nthree\r\n");
    let review = || extract(&root.0, [("note", false)].into_iter());
    assert!(review().is_err());
    fs::write(&file, "one\r\nchanged\r\nthree\r\n").unwrap();
    assert!(review().unwrap()[0].contains(" one\n-two\n+changed\n three\n"));
    fs::write(&file, "one\nchanged\r\nthree\n").unwrap();
    assert!(review().is_err());
}

#[test]
fn large_source_with_a_small_edit_fits_the_snapshot() {
    let root = fixture();
    let before: String = (0..6000)
        .map(|line| format!("source line {line:04}\n"))
        .collect();
    assert!(before.len() > MAX_BYTES);
    let file = root.0.join("large.rs");
    fs::write(&file, &before).unwrap();
    git(&root.0, &["add", "--", "large.rs"]);
    fs::write(file, before.replace("line 3000", "line edited")).unwrap();
    let patches = extract(&root.0, [("large.rs", false)].into_iter()).unwrap();
    let snapshot = crate::review_snapshot::ReviewSnapshot::freeze(
        "workspace",
        [crate::review_snapshot::Part {
            workspace: "workspace",
            path: "large.rs",
            staged: false,
            diff: &patches[0],
            truncated: false,
        }],
    )
    .unwrap();
    assert!(snapshot.text().len() < 512);
    assert!(
        snapshot
            .text()
            .contains("-source line 3000\n+source line edited\n")
    );
    assert!(!snapshot.text().contains("line 0000"));
}

#[test]
fn rejects_staged_link_deletion_binary_oversize_and_directory() {
    let root = fixture();
    fs::create_dir(root.0.join("nested")).unwrap();
    fs::write(root.0.join("nested/file"), "link target\n").unwrap();
    git(&root.0, &["add", "nested/file"]);
    let oid = git(&root.0, &["hash-object", "-w", "nested/file"]);
    git(
        &root.0,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{oid},link"),
        ],
    );
    set_fixture_head(&root.0);
    git(&root.0, &["update-index", "--force-remove", "link"]);
    assert!(extract(&root.0, [("link", true)].into_iter()).is_err());
    assert!(extract(&root.0, [("nested", true)].into_iter()).is_err());
    for content in [
        b"bad\0data".to_vec(),
        vec![b'x'; MAX_BYTES + 1],
        vec![b'x'; MAX_FILE + 1],
    ] {
        fs::write(root.0.join("nested/file"), content).unwrap();
        assert!(extract(&root.0, [("nested/file", false)].into_iter()).is_err());
    }
    fs::remove_file(root.0.join("nested/file")).unwrap();
    fs::hard_link(root.0.join("fixture-commit"), root.0.join("nested/file")).unwrap();
    assert!(extract(&root.0, [("nested/file", false)].into_iter()).is_err());
}

#[test]
fn refuses_executable_additions_and_executable_to_regular_edits() {
    let root = fixture();
    let file = root.0.join("note");
    fs::write(&file, "first\n").unwrap();
    git(&root.0, &["add", "--chmod=+x", "note"]);
    assert_eq!(
        extract(&root.0, [("note", true)].into_iter()),
        Err(UNSUPPORTED)
    );
    set_fixture_head(&root.0);
    fs::write(&file, "second\n").unwrap();
    assert_eq!(
        extract(&root.0, [("note", false)].into_iter()),
        Err(UNSUPPORTED)
    );
    git(&root.0, &["add", "--chmod=-x", "note"]);
    assert_eq!(
        extract(&root.0, [("note", true)].into_iter()),
        Err(UNSUPPORTED)
    );
}

#[test]
fn git_pointer_is_reported_without_following_it() {
    let root = Scratch::new_outside(&std::env::current_exe().unwrap()).unwrap();
    fs::write(root.0.join(".git"), "gitdir: ../other\n").unwrap();
    assert!(
        extract(&root.0, [("note", true)].into_iter())
            .unwrap_err()
            .contains("linked worktrees")
    );
}

#[test]
fn working_file_under_a_replaced_junction_is_not_opened() {
    let root = fixture();
    fs::create_dir(root.0.join("dir")).unwrap();
    fs::write(root.0.join("dir/note"), "original\n").unwrap();
    git(&root.0, &["add", "dir/note"]);
    fs::rename(root.0.join("dir"), root.0.join("elsewhere")).unwrap();
    fs::write(root.0.join("elsewhere/note"), "wrong file\n").unwrap();
    let made = Command::new("cmd.exe")
        .current_dir(&root.0)
        .args(["/d", "/c", "mklink", "/J", "dir", "elsewhere"])
        .output()
        .unwrap();
    assert!(made.status.success());
    assert_eq!(
        extract(&root.0, [("dir/note", false)].into_iter()),
        Err(UNSAFE)
    );
}

fn packed_fixture() -> Scratch {
    let root = fixture();
    fs::write(root.0.join("note"), "base\n").unwrap();
    fs::write(root.0.join("other"), "unselected packed content\n").unwrap();
    git(&root.0, &["add", "note", "other"]);
    set_fixture_head(&root.0);
    git(&root.0, &["gc", "--prune=now"]);
    let oid = git(&root.0, &["rev-parse", ":note"]);
    assert!(
        !root
            .0
            .join(format!(".git/objects/{}/{}", &oid[..2], &oid[2..]))
            .exists()
    );
    fs::write(root.0.join("note"), "work\n").unwrap();
    root
}

#[test]
fn gc_packed_index_and_head_produce_only_the_selected_patch() {
    let root = packed_fixture();
    let patches = extract(&root.0, [("note", false)].into_iter()).unwrap();
    assert!(patches[0].contains("-base\n+work\n"));
    assert!(!patches[0].contains("unselected packed content"));
    git(&root.0, &["add", "note"]);
    fs::write(root.0.join("note"), "later\n").unwrap();
    let patches = extract(&root.0, [("note", true), ("note", false)].into_iter()).unwrap();
    assert!(patches[0].contains("-base\n+work\n"));
    assert!(patches[1].contains("-work\n+later\n"));
}

#[test]
fn incomplete_and_oversized_pack_sets_are_rejected() {
    let root = packed_fixture();
    let dir = root.0.join(PACK_DIR);
    let stem = format!("pack-{}", "1".repeat(40));
    File::create_new(dir.join(format!("{stem}.idx"))).unwrap();
    let review = || extract(&root.0, [("note", false)].into_iter());
    assert_eq!(review(), Err(MISSING_PACK));
    File::create_new(dir.join(format!("{stem}.pack")))
        .unwrap()
        .set_len(MAX_PACK_BYTES)
        .unwrap();
    assert_eq!(review(), Err(PACK_LIMIT));
}

#[test]
fn packed_objects_do_not_follow_alternates_or_a_pack_directory_junction() {
    let root = packed_fixture();
    let pack_dir = root.0.join(PACK_DIR);
    let elsewhere = root.0.join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::rename(&pack_dir, elsewhere.join("pack")).unwrap();
    fs::create_dir(&pack_dir).unwrap();
    fs::write(
        root.0.join(".git/objects/info/alternates"),
        b"../../elsewhere\n",
    )
    .unwrap();
    assert_eq!(git(&root.0, &["cat-file", "blob", ":note"]), "base");
    let review = || extract(&root.0, [("note", false)].into_iter());
    assert_eq!(review(), Err(MISSING_PACK));
    fs::remove_dir(&pack_dir).unwrap();
    let made = Command::new("cmd.exe")
        .current_dir(&root.0)
        .args([
            "/d",
            "/c",
            "mklink",
            "/J",
            ".git\\objects\\pack",
            "elsewhere\\pack",
        ])
        .output()
        .unwrap();
    assert!(made.status.success());
    assert_eq!(review(), Err(UNSAFE));
}
