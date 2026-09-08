//! End-to-end contract tests. Set RRG_REQUIRE_RG=1 to require the oracle in CI.
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

fn fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::create_dir(root.join("src")).unwrap();
    for (path, bytes) in [
        (
            "a.txt",
            b"before\nneedle needle\nNeedle\nafter\n\nlast needle".as_slice(),
        ),
        ("b.txt", b"no match\nother text\n"),
        ("src/lib.rs", b"pub fn needle() {}\n// NEEDLE\n"),
        ("src/readme.md", b"needle in markdown\n"),
        (".hidden", b"needle hidden\n"),
        ("ignored.txt", b"needle ignored\n"),
        (".gitignore", b"ignored.txt\n"),
        ("bytes.bin", b"\0needle binary\n"),
    ] {
        std::fs::write(root.join(path), bytes).unwrap();
    }
    dir
}

fn run(exe: &str, root: &Path, args: &[&str], input: Option<&[u8]>) -> Output {
    let mut child = Command::new(exe)
        .args(args)
        .current_dir(root)
        .env_remove("RIPGREP_CONFIG_PATH")
        .env_remove("RRG_NO_DAEMON")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(bytes) = input {
        child.stdin.take().unwrap().write_all(bytes).unwrap();
    }
    child.wait_with_output().unwrap()
}

fn rg_available() -> bool {
    if Command::new("rg").arg("--version").output().is_ok() {
        return true;
    }
    assert!(
        std::env::var_os("RRG_REQUIRE_RG").is_none(),
        "rg is required for differential tests"
    );
    eprintln!("skipping rg differential test: rg is not installed");
    false
}

#[test]
fn everyday_search_matches_rg_output_and_exit_status() {
    if !rg_available() {
        return;
    }
    let dir = fixture();
    let cases: &[&[&str]] = &[
        &["needle", "."],
        &["needle", "a.txt"],
        &["needle", "a.txt", "src"],
        &["missing", "."],
        &["-c", "needle", "."],
        &["--count-matches", "needle", "."],
        &["-co", "needle", "."],
        &["-c", "--include-zero", "needle", "."],
        &["-l", "needle", "."],
        &["--files-without-match", "needle", "."],
        &["-q", "needle", "."],
        &["-q", "missing", "."],
        &["-n", "needle", "."],
        &["-N", "needle", "."],
        &["-H", "needle", "a.txt"],
        &["-I", "needle", "."],
        &["--heading", "needle", "."],
        &["--column", "needle", "a.txt"],
        &["-b", "needle", "a.txt"],
        &["-o", "needle", "a.txt"],
        &["-r", "found", "needle", "a.txt"],
        &["-or", "found", "needle", "a.txt"],
        &["-C1", "needle", "."],
        &["-B1", "-A2", "needle", "a.txt"],
        &["-nC1", "--context-separator=...", "needle", "."],
        &["-m1", "needle", "."],
        &["-m0", "needle", "."],
        &["-i", "needle", "."],
        &["-S", "needle", "."],
        &["-S", "Needle", "."],
        &["-is", "needle", "."],
        &["-si", "needle", "."],
        &["-w", "needle", "."],
        &["-x", "needle needle", "."],
        &["-v", "needle", "a.txt"],
        &["-F", "fn needle()", "."],
        &["-e", "needle", "-e", "other", "."],
        &["-e", "needle\nother", "."],
        &["--hidden", "needle", "."],
        &["-u", "needle", "."],
        &["-uu", "needle", "."],
        &["-uuu", "needle", "."],
        &["-a", "needle", "bytes.bin"],
        &["needle", "bytes.bin"],
        &["-g", "*.rs", "needle", "."],
        &["-g", "!*.txt", "needle", "."],
        &["--iglob", "*.RS", "needle", "."],
        &["-t", "rust", "needle", "."],
        &["-T", "rust", "needle", "."],
        &["--type-add", "custom:*.txt", "-tcustom", "needle", "."],
        &["--files", "."],
        &["--files", "-q"],
        &["-cv", "needle", "."],
        &["-F", "--", "-needle", "."],
        &["-e", "-needle", "."],
        &["--files", "-0", "."],
        &["-l0", "needle", "."],
        &["--max-depth=1", "needle", "."],
        &["--max-filesize=20", "needle", "."],
        &["--color=always", "needle", "a.txt"],
        &[
            "--colors=match:fg:blue",
            "--color=always",
            "needle",
            "a.txt",
        ],
        &["-M5", "needle", "a.txt"],
        &["--trim", "needle", "."],
        &["-C1", "--passthru", "needle", "."],
        &["--passthru", "-C1", "needle", "."],
        &["--heading", "--passthru", "NEEDLE", "."],
        &["(", "."],
        &["needle", "does-not-exist"],
        &["--no-messages", "needle", "does-not-exist"],
    ];
    let mut failures = Vec::new();
    for case in cases {
        let mut args = vec!["--no-config", "--no-ignore-global", "--sort=path"];
        args.extend_from_slice(case);
        let expected = run("rg", dir.path(), &args, None);
        let actual = run(env!("CARGO_BIN_EXE_rrg"), dir.path(), &args, None);
        if actual.stdout != expected.stdout || actual.status.code() != expected.status.code() {
            failures.push(format!("{case:?}\nexpected status {:?}, output {:?}\nactual status {:?}, output {:?}\nstderr {}", expected.status.code(), String::from_utf8_lossy(&expected.stdout), actual.status.code(), String::from_utf8_lossy(&actual.stdout), String::from_utf8_lossy(&actual.stderr)));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn stdin_and_pattern_files_match_rg() {
    if !rg_available() {
        return;
    }
    let dir = fixture();
    std::fs::write(dir.path().join("patterns"), "needle\nother\n").unwrap();
    std::fs::write(dir.path().join("empty-patterns"), "").unwrap();
    for (args, input) in [
        (vec!["needle"], Some(b"needle\nother\n".as_slice())),
        (
            vec!["-n", "needle", "-"],
            Some(b"needle\nother\n".as_slice()),
        ),
        (vec!["needle", "-", "a.txt"], Some(b"needle\n".as_slice())),
        (vec!["-f", "patterns", "."], None),
        (vec!["-f", "empty-patterns", "."], None),
        (vec!["-f", "-", "."], Some(b"needle\nother\n".as_slice())),
        (vec!["needle"], None),
    ] {
        let mut all = vec!["--no-config", "--no-ignore-global", "--sort=path"];
        all.extend(args);
        let expected = run("rg", dir.path(), &all, input);
        let actual = run(env!("CARGO_BIN_EXE_rrg"), dir.path(), &all, input);
        assert_eq!(
            actual.stdout,
            expected.stdout,
            "{all:?}: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(actual.status.code(), expected.status.code(), "{all:?}");
    }
}

#[test]
fn indexed_output_obeys_the_same_contract_and_sees_changed_and_new_files() {
    let dir = fixture();
    let build = run(
        env!("CARGO_BIN_EXE_rrg"),
        dir.path(),
        &["--build-index", "."],
        None,
    );
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    std::fs::write(dir.path().join("new.txt"), "needle new file\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "needle changed file\n").unwrap();
    for flags in [
        vec![],
        vec!["-c"],
        vec!["-l"],
        vec!["-nC1"],
        vec!["-v"],
        vec!["--files-without-match"],
        vec!["--hidden"],
    ] {
        let mut args = vec!["--sort=path", "-F"];
        args.extend(flags);
        args.extend(["needle", "."]);
        let expected = run(env!("CARGO_BIN_EXE_rrg"), dir.path(), &args, None);
        args.insert(0, "--use-index");
        let actual = run(env!("CARGO_BIN_EXE_rrg"), dir.path(), &args, None);
        assert_eq!(
            actual.stdout,
            expected.stdout,
            "{args:?}: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(actual.status.code(), expected.status.code(), "{args:?}");
    }
}

#[test]
fn configuration_is_loaded_and_can_be_disabled_or_overridden() {
    let dir = fixture();
    let config = dir.path().join("config");
    std::fs::write(&config, "# options\n--ignore-case\n--line-number\n").unwrap();
    for (flags, expected) in [
        (vec![], "2:needle needle\n3:Needle\n6:last needle\n"),
        (vec!["--no-config"], "needle needle\nlast needle\n"),
        (vec!["-sN"], "needle needle\nlast needle\n"),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_rrg"))
            .args(flags)
            .args(["needle", "a.txt"])
            .current_dir(dir.path())
            .env("RIPGREP_CONFIG_PATH", &config)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, expected.as_bytes());
    }
}

#[test]
fn non_utf8_paths_and_file_contents_are_preserved() {
    #[cfg(not(target_os = "macos"))]
    use std::os::unix::ffi::OsStringExt;
    let dir = fixture();
    #[cfg(not(target_os = "macos"))]
    let path = std::ffi::OsString::from_vec(b"invalid-\xff.txt".to_vec());
    #[cfg(target_os = "macos")]
    let path = std::ffi::OsString::from("invalid.txt");
    std::fs::write(dir.path().join(path), b"needle \xff\n").unwrap();
    let out = run(
        env!("CARGO_BIN_EXE_rrg"),
        dir.path(),
        &["--sort=path", "needle", "."],
        None,
    );
    assert!(
        out.stdout
            .windows(b"needle \xff\n".len())
            .any(|s| s == b"needle \xff\n")
    );
}

#[test]
fn json_events_match_rg_except_elapsed_timings() {
    if !rg_available() {
        return;
    }
    let dir = fixture();
    fn normalize(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                map.remove("elapsed");
                map.remove("elapsed_total");
                for value in map.values_mut() {
                    normalize(value);
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    normalize(value);
                }
            }
            _ => {}
        }
    }
    for pattern in ["needle", "no-such-pattern"] {
        let args = ["--no-config", "--sort=path", "--json", "-C1", pattern, "."];
        let expected = run("rg", dir.path(), &args, None);
        let actual = run(env!("CARGO_BIN_EXE_rrg"), dir.path(), &args, None);
        let parse = |bytes: &[u8]| {
            bytes
                .split(|&b| b == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| {
                    let mut value: serde_json::Value = serde_json::from_slice(line).unwrap();
                    normalize(&mut value);
                    value
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(parse(&actual.stdout), parse(&expected.stdout), "{args:?}");
        assert_eq!(actual.status.code(), expected.status.code());
    }
}

#[test]
fn multiline_crlf_null_data_and_editor_output_match_rg() {
    if !rg_available() {
        return;
    }
    let dir = fixture();
    std::fs::write(dir.path().join("crlf.txt"), b"needle\r\nother\r\n").unwrap();
    std::fs::write(dir.path().join("nul.txt"), b"needle\0other\0needle\0").unwrap();
    for args in [
        vec!["-U", "needle\\nNeedle", "a.txt"],
        vec!["-U", "--multiline-dotall", "needle.*after", "a.txt"],
        vec!["-n", "--crlf", "needle$", "crlf.txt"],
        vec!["--null-data", "-n", "needle", "nul.txt"],
        vec!["--vimgrep", "needle", "a.txt"],
        vec!["--passthru", "-n", "needle", "a.txt"],
        vec!["-p", "needle", "a.txt"],
        vec!["--no-unicode", "\\w+", "a.txt"],
    ] {
        let expected = run("rg", dir.path(), &args, None);
        let actual = run(env!("CARGO_BIN_EXE_rrg"), dir.path(), &args, None);
        assert_eq!(
            actual.stdout,
            expected.stdout,
            "{args:?}: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(actual.status.code(), expected.status.code(), "{args:?}");
    }
}

#[test]
fn symlinks_and_error_exit_codes_match_rg() {
    if !rg_available() {
        return;
    }
    let dir = fixture();
    std::os::unix::fs::symlink("a.txt", dir.path().join("link.txt")).unwrap();
    for args in [
        vec!["needle", "link.txt"],
        vec!["--sort=path", "-L", "needle", "."],
        vec!["--sort=path", "needle", "a.txt", "missing.txt"],
        vec!["--sort=path", "-q", "needle", "a.txt", "missing.txt"],
        vec!["--sort=path", "-q", "needle", "missing.txt", "a.txt"],
    ] {
        let expected = run("rg", dir.path(), &args, None);
        let actual = run(env!("CARGO_BIN_EXE_rrg"), dir.path(), &args, None);
        assert_eq!(
            actual.stdout,
            expected.stdout,
            "{args:?}: {}",
            String::from_utf8_lossy(&actual.stderr)
        );
        assert_eq!(actual.status.code(), expected.status.code(), "{args:?}");
    }
}
