//! The server embeds `ui/dist`. A fresh checkout has no build of the UI yet, so write a page that says so
//! instead of failing the Rust build. `cargo xtask ui` replaces it with the real UI.
use std::path::Path;

fn main() {
    let dist = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ui/dist");
    let index = dist.join("index.html");
    println!("cargo:rerun-if-changed={}", dist.display());
    if !index.exists() {
        std::fs::create_dir_all(&dist).expect("create ui/dist");
        let page = "<!doctype html><meta charset=\"utf-8\"><title>Patina</title>\
                    <h1>Patina</h1><p>The UI is not built. Run <code>cargo xtask ui</code>, then rebuild.</p>";
        std::fs::write(&index, page).expect("write ui/dist/index.html");
    }
}
