//! Building a program: check it, lower it to keel, then assemble and link
//! it with the C library. On macOS keel does both in memory; elsewhere
//! `clang` assembles and links keel's assembly.

use std::path::{Path, PathBuf};
use std::process::Command;

use keel_codegen::Target;

use crate::{check, codegen, package, resolve};

pub struct Options {
    pub std_dir: PathBuf,
    /// Run keel's optimizer.
    pub optimize: bool,
    /// Stop after writing the assembly file.
    pub assembly_only: bool,
    /// Write keel's IR instead, before optimization.
    pub ir_only: bool,
}

/// The target this compiler runs on.
pub fn host() -> Target {
    if cfg!(target_os = "macos") {
        Target::Aarch64AppleDarwin
    } else if cfg!(target_arch = "aarch64") {
        Target::Aarch64LinuxGnu
    } else {
        Target::X86_64LinuxGnu
    }
}

/// Builds the program rooted at `root` into the executable `out`, or the
/// assembly file `out` with `assembly_only`. Errors are rendered text.
pub fn build(root: &Path, out: &Path, options: &Options) -> Result<(), String> {
    let program = package::load_hosted(root, &options.std_dir);
    if program.has_errors() {
        return Err(program.render_errors());
    }
    let names = resolve::resolve(&program);
    let mut diagnostics = names.diagnostics.clone();
    let checked = if diagnostics.is_empty() {
        let checked = check::check(&program, &names);
        diagnostics.clone_from(&checked.diagnostics);
        Some(checked)
    } else {
        None
    };
    if !diagnostics.is_empty() {
        let mut text = String::new();
        for (file, d) in &diagnostics {
            text.push_str(&d.render(&program.file(*file).source));
        }
        return Err(text);
    }
    let checked = checked.expect("checked without errors");
    let mut module = codegen::generate(&program, &names, checked).map_err(|e| e.to_string())?;
    if let Err(errors) = keel_ir::verify_module(&module) {
        let lines: Vec<String> = errors.iter().map(ToString::to_string).collect();
        return Err(format!(
            "internal error: keel rejected the generated code:\n{}",
            lines.join("\n")
        ));
    }
    if options.ir_only {
        return std::fs::write(out, module.to_string())
            .map_err(|e| format!("cannot write {}: {e}", out.display()));
    }
    if options.optimize {
        keel_opt::optimize(&mut module).map_err(|e| format!("internal error: {e}"))?;
    }
    if options.assembly_only {
        let assembly = keel_codegen::compile(&module, host())
            .map_err(|e| format!("internal error: code generation failed: {e}"))?;
        return std::fs::write(out, assembly)
            .map_err(|e| format!("cannot write {}: {e}", out.display()));
    }
    if host() == Target::Aarch64AppleDarwin {
        let object = assemble(&module)?;
        return link_darwin(&object, out);
    }
    link_with_clang(&module, out)
}

/// Assembles `module` for Apple arm64 in memory: code generation hands each
/// function's instructions straight to keel's assembler, with no text.
fn assemble(module: &keel_ir::Module) -> Result<Vec<u8>, String> {
    let codegen_error =
        |e: keel_codegen::CodegenError| format!("internal error: code generation failed: {e}");
    let selector = keel_codegen::OptionsSelector(keel_codegen::SelectionOptions::default());
    let pipeline = keel_codegen::CodegenPipeline {
        selector: &selector,
        allocator: &keel_codegen::regalloc::AllocationTier::Full,
    };
    let request = keel_codegen::Request::new(Target::Aarch64AppleDarwin, pipeline);
    // Every Apple arm64 CPU has the SHA-256 extension.
    let features = keel_asm::TargetFeatures::default().with_sha256();
    let mut assembler = keel_asm::ObjectAssembler::with_features(
        keel_asm::Target::Aarch64AppleDarwin,
        features,
        keel_asm::x86::Features::default(),
    )
    .map_err(|e| format!("internal error: {e}"))?;
    request
        .program_chunks(module, &mut |program| assembler.push(program))
        .map_err(codegen_error)?;
    assembler
        .finish()
        .map_err(|e| format!("internal error: assembly failed: {e}"))
}

/// Links one object with libSystem into the signed executable `out`.
fn link_darwin(object: &[u8], out: &Path) -> Result<(), String> {
    let sdk = macos_sdk().ok_or("cannot find the macOS SDK; set SDKROOT")?;
    let libsystem = keel_link::Dylib::from_tbd(&sdk.join("usr/lib/libSystem.tbd"), &sdk)
        .map_err(|e| format!("cannot read libSystem from {}: {e}", sdk.display()))?;
    let inputs = [keel_link::Input {
        name: "tern.o",
        bytes: object,
    }];
    let link_options = keel_link::Options {
        dylibs: vec![libsystem],
        ..keel_link::Options::default()
    };
    let warnings = keel_link::link_signed(&inputs, link_options, out)
        .map_err(|e| format!("cannot link {}: {e}", out.display()))?;
    for warning in warnings {
        eprintln!("warning: {warning}");
    }
    Ok(())
}

/// The macOS SDK: `SDKROOT`, the Command Line Tools' SDK, or what `xcrun`
/// reports. Reading the directory first skips running xcrun.
fn macos_sdk() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("SDKROOT").filter(|s| !s.is_empty()) {
        return Some(PathBuf::from(root));
    }
    let tools = Path::new("/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk");
    let developer = std::fs::read_link("/var/db/xcode_select_link").ok();
    if developer.as_deref() == Some(Path::new("/Library/Developer/CommandLineTools"))
        && tools.is_dir()
    {
        return Some(tools.to_path_buf());
    }
    let output = Command::new("xcrun")
        .args(["--sdk", "macosx", "--show-sdk-path"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    Some(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

/// Writes assembly next to `out` and has clang assemble and link it.
fn link_with_clang(module: &keel_ir::Module, out: &Path) -> Result<(), String> {
    let assembly = keel_codegen::compile(module, host())
        .map_err(|e| format!("internal error: code generation failed: {e}"))?;
    let asm_path = out.with_extension("s");
    std::fs::write(&asm_path, assembly)
        .map_err(|e| format!("cannot write {}: {e}", asm_path.display()))?;
    let status = Command::new("clang")
        .arg("-o")
        .arg(out)
        .arg(&asm_path)
        .status()
        .map_err(|e| format!("cannot run clang: {e}"))?;
    let _ = std::fs::remove_file(&asm_path);
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "clang failed to assemble or link {}",
            out.display()
        ))
    }
}
