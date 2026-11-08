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
`npm test` also parses them. Nothing type-checks them yet.

The flake provides the tree-sitter CLI. Generating the parser also needs
Node.js on the `PATH`.

```
nix develop
cd tree-sitter
npm test
```

## Compiler

[toolchain/](toolchain) is a Cargo workspace. `stage1` is the bootstrap
compiler, written in Rust. So far it lexes and parses:

```
cd toolchain
cargo test
cargo run -- parse ../examples/uart/main.tern
```

`parse` prints the tree in tree-sitter's S-expression form. The tests
parse every program in `examples/`, compare the tree for every corpus
input the grammar accepts with tree-sitter's, and check that every input
the grammar rejects gets an error.
