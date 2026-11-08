//! Building a program: check it, lower it to keel, and link it with the C
//! library through `clang`.

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
    let assembly = keel_codegen::compile(&module, host())
        .map_err(|e| format!("internal error: code generation failed: {e}"))?;
    if options.assembly_only {
        return std::fs::write(out, assembly)
            .map_err(|e| format!("cannot write {}: {e}", out.display()));
    }
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
