use std::{env, process::Command};

fn write_placeholder(dist: &str) {
    std::fs::create_dir_all(dist).ok();
    std::fs::write(
        format!("{dist}/index.html"),
        "<!DOCTYPE html><html><body><h1>rustnzb</h1><p>Frontend not built. Run: cd frontend && npx ng build</p></body></html>",
    )
    .ok();
}

fn main() {
    println!("cargo:rerun-if-changed=frontend/src/");
    println!("cargo:rerun-if-changed=frontend/angular.json");
    println!("cargo:rerun-if-env-changed=RUSTNZB_SKIP_FRONTEND_BUILD");

    let dist = "frontend/dist/frontend/browser";

    // If dist already exists (e.g. CI pre-built it or previous build), skip
    if std::path::Path::new(dist).join("index.html").exists() {
        return;
    }

    if env::var_os("RUSTNZB_SKIP_FRONTEND_BUILD").is_some() {
        write_placeholder(dist);
        return;
    }

    // Try to run ng build if frontend exists
    if std::path::Path::new("frontend/package.json").exists() {
        match Command::new("npx")
            .args(["ng", "build", "--configuration=production"])
            .current_dir("frontend")
            .status()
        {
            Ok(status) if status.success() => return,
            Ok(status) => {
                println!(
                    "cargo:warning=Angular build failed with exit code {:?}",
                    status.code()
                );
            }
            Err(e) => {
                println!("cargo:warning=Could not run ng build: {e}");
            }
        }
    }

    // Create minimal placeholder so rust-embed has something to embed
    write_placeholder(dist);
}
