//! Guard for the process rule: every external program is started through
//! `tools::tool_command` / `tools::blocking_tool_command` (no console window on Windows, stdin
//! closed), so no product code outside `src/tools.rs` may call `Command::new` itself.
//! Integration tests under `tests/` are exempt: they run from a console anyway.

use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_direct_process_spawns_outside_tools() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    rust_files(&root.join("src-tauri").join("src"), &mut files);
    let allowed = root.join("src").join("tools.rs");
    assert!(files.contains(&allowed), "scan found the source tree");

    let mut violations = Vec::new();
    for file in files.iter().filter(|f| **f != allowed) {
        let text = std::fs::read_to_string(file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains("Command::new(") {
                violations.push(format!("{}:{}: {}", file.strip_prefix(root).unwrap().display(), n + 1, line.trim()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "start processes via tools::tool_command / blocking_tool_command:\n{}",
        violations.join("\n")
    );
}
