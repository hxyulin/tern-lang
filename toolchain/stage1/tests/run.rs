//! End to end: each directory in `tests/run` is a program, built with
//! keel without and with optimization, then run. Its `stdout` file holds
//! the expected output, `exit` the exit status (0 if absent), and
//! `stderr`, if present, text the error output must start with. A
//! directory with `.c` files is linked with a static archive of them, and
//! with libm.
//!
//! Each program is built twice: by stage 1, and by ternc, the compiler in
//! tern, which stage 1 builds.
//!
//! Needs `cc` and `ar` for those archives; on Linux, also `clang` to
//! assemble and link.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use stage1::build::{Options, build};

mod common;
use common::ternc_compilers;

/// Builds a program with or without optimization into the given path.
type Builder<'a> = dyn Fn(bool, &Path) -> Result<(), String> + 'a;

fn std_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../std")
}

/// A builder that uses stage 1 with `options`, optimization aside.
fn stage1(root: &Path, options: &Options) -> impl Fn(bool, &Path) -> Result<(), String> {
    move |optimize, out| {
        let options = Options {
            optimize,
            link: options.link.clone(),
            libraries: options.libraries.clone(),
            ..Options::new(std_dir())
        };
        build(root, out, &options)
    }
}

/// A builder that runs ternc's `build` command with `extra` arguments.
fn ternc<'a>(
    root: &'a Path,
    extra: &'a [String],
) -> impl Fn(bool, &Path) -> Result<(), String> + 'a {
    move |optimize, out| {
        let (compiler, _) = &ternc_compilers()[0];
        let output = Command::new(compiler)
            .arg("build")
            .arg(root)
            .arg("--std")
            .arg(std_dir())
            .args(extra)
            .args(optimize.then_some("-O"))
            .arg("-o")
            .arg(out)
            .output()
            .expect("can run ternc");
        if output.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).into_owned())
        }
    }
}

/// Builds `root` with `builder`, without and with optimization, runs it
/// with `args`, and returns the failures.
fn run(name: &str, builder: &Builder, args: &[&str], expected: &Path) -> Vec<String> {
    let mut failures = Vec::new();
    let read = |file: &str| fs::read_to_string(expected.join(file)).ok();
    let want_stdout = read("stdout").unwrap_or_default();
    let want_exit: i32 = read("exit").map_or(0, |s| s.trim().parse().expect("a number"));
    let want_stderr = read("stderr");
    for optimize in [false, true] {
        let label = if optimize { "-O" } else { "plain" };
        let out = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("run")
            .join(format!("{name}{}", if optimize { "-O" } else { "" }));
        fs::create_dir_all(out.parent().expect("a parent")).expect("can create directory");
        if let Err(e) = builder(optimize, &out) {
            failures.push(format!("{name} ({label}): build failed:\n{e}"));
            continue;
        }
        let output = Command::new(&out).args(args).output().expect("can run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        // A signal shows as 128 plus its number, as in a shell.
        let code = output.status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            128 + output.status.signal().unwrap_or(0)
        });
        if stdout != want_stdout {
            failures.push(format!(
                "{name} ({label}): stdout\n--- want\n{want_stdout}--- got\n{stdout}"
            ));
        }
        if code != want_exit {
            failures.push(format!(
                "{name} ({label}): exit {code}, want {want_exit}; stderr:\n{stderr}"
            ));
        }
        if let Some(want) = &want_stderr
            && !stderr.starts_with(want.as_str())
        {
            failures.push(format!(
                "{name} ({label}): stderr\n--- want a start of\n{want}\n--- got\n{stderr}"
            ));
        }
    }
    failures
}

/// A static archive of the `.c` files in `root`, if it has any.
fn c_archive(name: &str, root: &Path) -> Option<PathBuf> {
    let mut sources: Vec<PathBuf> = fs::read_dir(root)
        .expect("can read directory")
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "c"))
        .collect();
    if sources.is_empty() {
        return None;
    }
    sources.sort();
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("c").join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("can create directory");
    let mut objects = Vec::new();
    for source in &sources {
        let stem = source.file_stem().expect("a file name");
        let object = dir.join(stem).with_extension("o");
        let status = Command::new("cc")
            .args(["-c", "-O2", "-o"])
            .arg(&object)
            .arg(source)
            .status()
            .expect("can run cc");
        assert!(status.success(), "cc failed on {}", source.display());
        objects.push(object);
    }
    let archive = dir.join(format!("lib{name}.a"));
    let status = Command::new("ar")
        .arg("rcs")
        .arg(&archive)
        .args(&objects)
        .status()
        .expect("can run ar");
    assert!(status.success(), "ar failed for {name}");
    Some(archive)
}

fn run_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/run")
}

/// The programs in `tests/run`, sorted.
fn run_names() -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(run_dir())
        .expect("tests/run exists")
        .filter_map(Result::ok)
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert!(names.len() >= 8, "found only {names:?}");
    names
}

#[test]
fn programs_run() {
    let mut failures = Vec::new();
    for name in &run_names() {
        let root = run_dir().join(name);
        let mut options = Options::new(std_dir());
        if let Some(archive) = c_archive(name, &root) {
            options.link.push(archive);
            options.libraries.push("m".into());
        }
        failures.extend(run(name, &stage1(&root, &options), &[], &root));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn programs_built_by_ternc_run() {
    let mut failures = Vec::new();
    for name in &run_names() {
        let root = run_dir().join(name);
        let mut extra = Vec::new();
        if let Some(archive) = c_archive(&format!("{name}-ternc"), &root) {
            extra.push(archive.to_string_lossy().into_owned());
            extra.push("-lm".to_owned());
        }
        let label = format!("{name}-ternc");
        failures.extend(run(&label, &ternc(&root, &extra), &[], &root));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn ini_example_runs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/ini");
    let tmp = Path::new(env!("CARGO_TARGET_TMPDIR")).join("ini-input");
    fs::create_dir_all(&tmp).expect("can create directory");
    let good = tmp.join("good.ini");
    fs::write(
        &good,
        "; settings\n[server]\nhost = example.org\nport=8080   # the port\n\n[client]\nname = tern\n",
    )
    .expect("can write");
    let bad = tmp.join("bad.ini");
    fs::write(&bad, "[a]\nkey value\n").expect("can write");
    let expect = |name: &str, stdout: &str, exit: i32, stderr: &str| {
        let dir = tmp.join(name);
        fs::create_dir_all(&dir).expect("can create directory");
        fs::write(dir.join("stdout"), stdout).expect("can write");
        fs::write(dir.join("exit"), exit.to_string()).expect("can write");
        fs::write(dir.join("stderr"), stderr).expect("can write");
        dir
    };
    let good_dir = expect(
        "good",
        "[server]\n  host = example.org\n  port = 8080\n[client]\n  name = tern\n",
        0,
        "",
    );
    let bad_dir = expect("bad", "[a]\n", 1, "line 2: expected key = value\n");
    let usage_dir = expect("usage", "", 1, "usage: inicheck FILE\n");
    let mut failures = Vec::new();
    let good = good.to_string_lossy().into_owned();
    let bad = bad.to_string_lossy().into_owned();
    let options = Options::new(std_dir());
    let by_stage1 = stage1(&root, &options);
    let by_ternc = ternc(&root, &[]);
    for (compiler, builder) in [("", &by_stage1 as &Builder), ("-ternc", &by_ternc)] {
        failures.extend(run(
            &format!("ini-good{compiler}"),
            builder,
            &[&good],
            &good_dir,
        ));
        failures.extend(run(
            &format!("ini-bad{compiler}"),
            builder,
            &[&bad],
            &bad_dir,
        ));
        failures.extend(run(
            &format!("ini-usage{compiler}"),
            builder,
            &[],
            &usage_dir,
        ));
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
