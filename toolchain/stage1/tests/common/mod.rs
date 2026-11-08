//! Helpers shared by the integration tests. Each test binary uses only
//! some of them.
#![allow(dead_code)]

/// One test case from a tree-sitter corpus file.
pub struct Case {
    pub name: String,
    pub rejected: bool,
    pub input: String,
    pub tree: String,
}

/// Splits a corpus file into cases. A header is a name between two `===`
/// lines, with `:error` marking input the grammar must reject. The input
/// runs to a `---` line, and the expected tree to the next header.
pub fn corpus_cases(text: &str) -> Vec<Case> {
    let lines: Vec<&str> = text.lines().collect();
    let is_header = |l: &str| l.starts_with("===");
    let mut cases = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !is_header(lines[i]) {
            i += 1;
            continue;
        }
        let header_end = i
            + 1
            + lines[i + 1..]
                .iter()
                .position(|l| is_header(l))
                .expect("header is closed");
        let header = &lines[i + 1..header_end];
        let body_end = header_end
            + 1
            + lines[header_end + 1..]
                .iter()
                .position(|l| l.starts_with("---"))
                .expect("input ends with a --- line");
        let tree_end = lines[body_end + 1..]
            .iter()
            .position(|l| is_header(l))
            .map_or(lines.len(), |p| body_end + 1 + p);
        cases.push(Case {
            name: header[0].to_owned(),
            rejected: header.contains(&":error"),
            input: lines[header_end + 1..body_end].join("\n"),
            tree: lines[body_end + 1..tree_end].join("\n"),
        });
        i = tree_end;
    }
    cases
}
