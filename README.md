# tern

A design for a small systems language that compiles fast and is easy for
people to review. "tern" is a working name.

There is no code yet. [DESIGN.md](DESIGN.md) records the goals, each
decision with where the idea came from, what was rejected, and the open
questions.

## Grammar

[tree-sitter/](tree-sitter) holds a tree-sitter grammar for the parts of
the syntax designed so far. The grammar uses no precedence or conflict
declarations, so a clean `tree-sitter generate` shows the syntax has no
LR(1) conflicts. The tests in `tree-sitter/test/corpus` include code the
grammar must reject.

The flake provides the tree-sitter CLI. Generating the parser also needs
Node.js on the `PATH`.

```
nix develop
cd tree-sitter
npm test
```
