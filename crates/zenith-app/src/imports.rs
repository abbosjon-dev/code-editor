//! Built-in "organize imports" for when the language server has none: each
//! run of consecutive single-line import statements is sorted and
//! de-duplicated (blank lines keep separating groups). Multi-line imports
//! are left alone.

/// Whether a line is a complete one-line import in `language`.
fn is_import(line: &str, language: &str) -> bool {
    let t = line.trim_start();
    let lang = language.to_lowercase();
    let one_stmt = |prefixes: &[&str]| prefixes.iter().any(|p| t.starts_with(p)) && t.trim_end().ends_with(';');
    match lang.as_str() {
        "rust" => one_stmt(&["use ", "pub use ", "pub(crate) use "]),
        "python" => (t.starts_with("import ") || t.starts_with("from ")) && !t.trim_end().ends_with(['(', '\\']),
        "javascript" | "typescript" | "tsx" | "jsx" | "javascript (babel)" | "typescriptreact" => {
            t.starts_with("import ")
                && (t.trim_end().ends_with(';') || t.contains(" from ") || t.contains('\''))
                && !t.trim_end().ends_with('{')
        }
        "java" | "kotlin" | "scala" => t.starts_with("import ") && !t.trim_end().ends_with('{'),
        "c#" => one_stmt(&["using "]) && !t.contains('('),
        _ => false,
    }
}

/// Sort key: ignore leading `pub ` etc. so `pub use a` sits next to `use a`,
/// and compare case-sensitively the way rustfmt / isort do roughly.
fn key(line: &str) -> String {
    line.trim().to_string()
}

/// Organize the imports of `text`; `None` when nothing changes.
pub fn organize(text: &str, language: &str) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let go = language.eq_ignore_ascii_case("go");
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    let mut in_go_block = false;
    while i < lines.len() {
        let l = lines[i];
        // Go: `import (` … `)` — sort the lines inside.
        if go && l.trim() == "import (" {
            out.push(l.to_string());
            in_go_block = true;
            i += 1;
            continue;
        }
        let member = if in_go_block {
            let t = l.trim();
            if t == ")" {
                in_go_block = false;
                false
            } else {
                !t.is_empty() && !t.starts_with("//")
            }
        } else {
            is_import(l, language)
        };
        if !member {
            out.push(l.to_string());
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len()
            && (if in_go_block {
                let t = lines[i].trim();
                !t.is_empty() && t != ")" && !t.starts_with("//")
            } else {
                is_import(lines[i], language)
            })
        {
            i += 1;
        }
        let mut block: Vec<&str> = lines[start..i].to_vec();
        block.sort_by_key(|l| key(l));
        block.dedup_by(|a, b| a.trim() == b.trim());
        out.extend(block.into_iter().map(str::to_string));
    }
    let new = out.join("\n");
    (new != text).then_some(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_rust_blocks_and_keeps_groups() {
        let src = "use std::sync::Arc;\nuse std::collections::HashMap;\nuse std::sync::Arc;\n\nuse crate::b;\nuse crate::a;\nuse foo::{\n    x,\n};\nfn main() {}\n";
        let out = organize(src, "Rust").unwrap();
        assert_eq!(
            out,
            "use std::collections::HashMap;\nuse std::sync::Arc;\n\nuse crate::a;\nuse crate::b;\nuse foo::{\n    x,\n};\nfn main() {}\n"
        );
        assert_eq!(organize(&out, "Rust"), None);
    }

    #[test]
    fn python_js_go() {
        assert_eq!(organize("import sys\nimport os\n\nx = 1\n", "Python").unwrap(), "import os\nimport sys\n\nx = 1\n");
        assert_eq!(
            organize("import b from './b';\nimport a from './a';\nconst x = 1;\n", "TypeScript").unwrap(),
            "import a from './a';\nimport b from './b';\nconst x = 1;\n"
        );
        assert_eq!(organize("import (\n\t\"os\"\n\t\"fmt\"\n)\n", "Go").unwrap(), "import (\n\t\"fmt\"\n\t\"os\"\n)\n");
        assert_eq!(organize("let x = 1;\n", "Rust"), None);
    }
}
