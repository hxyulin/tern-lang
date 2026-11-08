# tern

A design for a small systems language that compiles fast and is easy for
people to review. "tern" is a working name.

[DESIGN.md](DESIGN.md) records the goals, each
decision with where the idea came from, what was rejected, and the open
questions.

## Grammar

[tree-sitter/](tree-sitter) holds a tree-sitter grammar for the parts of
the syntax designed so far. The grammar uses no precedence or conflict
declarations, so a clean `tree-sitter generate` shows the syntax has no
LR(1) conflicts. The tests in `tree-sitter/test/corpus` include code the
grammar must reject.

[examples/](examples) holds small programs written in the designed syntax.
`npm test` also parses them, and the compiler's tests type-check them.

The flake provides the tree-sitter CLI. Generating the parser also needs
Node.js on the `PATH`.

```
nix develop
cd tree-sitter
npm test
```

## Compiler

[toolchain/](toolchain) is a Cargo workspace. `stage1` is the bootstrap
compiler, written in Rust. It checks a program and compiles it through
[keel](https://github.com/hxyulin/keel) for aarch64 macOS or x86-64
Linux. On macOS, keel assembles and links in memory against the SDK's
libSystem, so a build needs the Command Line Tools but not `clang`. On
Linux, `clang` on the `PATH` assembles and links.

```
cd toolchain
cargo test
cargo run -- build ../examples/ini -o inicheck
./inicheck settings.ini
cargo run -- check ../examples/uart
```

- `build DIR` compiles the package in `DIR`, every package it imports
  and the runtime into an executable. `-O` optimizes, `-S` writes
  assembly and `--ir` writes keel's IR.
- `check DIR` loads the same packages and reports parse, import, name and
  type errors.
- `parse FILE` prints the syntax tree in tree-sitter's S-expression form.

The tests parse every program in `examples/`, compare the tree for every
corpus input the grammar accepts with tree-sitter's, check that every
input the grammar rejects gets an error, type-check the examples, and
build and run the programs in `toolchain/stage1/tests/run` and
`examples/ini`, with and without optimization. `examples/uart` targets a
microcontroller keel does not support, so it is only checked.

[toolchain/std](toolchain/std) is the standard library, written in tern
over the C library.
