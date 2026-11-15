//! Build helpers, run as `cargo xtask <task>`. Not part of the Patina binary.
use std::path::Path;
use std::process::{Command, exit};

fn run(cmd: &mut Command) {
    println!("> {cmd:?}");
    match cmd.status() {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("command failed: {s}");
            exit(1);
        }
        Err(e) => {
            eprintln!("cannot run command: {e}");
            exit(1);
        }
    }
}

/// On Windows npm is a `.cmd` script, which `Command` finds only by its full name.
fn npm() -> Command {
    Command::new(if cfg!(windows) { "npm.cmd" } else { "npm" })
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let ui = root.join("ui");
    match std::env::args().nth(1).as_deref() {
        // Export the API types first, so the UI is always built against the current Rust types.
        Some("ui") => {
            types(&root);
            let lock = if ui.join("node_modules").exists() { "install" } else { "ci" };
            run(npm().arg(lock).arg("--no-audit").arg("--no-fund").current_dir(&ui));
            run(npm().args(["run", "build"]).current_dir(&ui));
        }
        Some("types") => types(&root),
        _ => {
            eprintln!(
                "usage: cargo xtask <ui|types>\n  ui     export API types, then build ui/dist\n  types  export API types to ui/src/api/generated"
            );
            exit(2);
        }
    }
}

fn types(root: &Path) {
    let out = root.join("ui/src/api/generated");
    // Start clean so a removed Rust type also disappears from the UI.
    let _ = std::fs::remove_dir_all(&out);
    run(Command::new(env!("CARGO"))
        .args(["test", "-p", "patina-core", "--lib", "export_bindings"])
        .current_dir(root));
}
