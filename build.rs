use std::{env, process::Command};

const PLACEHOLDER_HTML: &str = "<!DOCTYPE html><html><body><h1>rustnzb</h1><p>Frontend not built. Run: cd frontend && npm ci && npm run build -- --configuration=production</p></body></html>";

fn write_placeholder(dist: &str) {
    std::fs::create_dir_all(dist).ok();
    std::fs::write(format!("{dist}/index.html"), PLACEHOLDER_HTML).ok();
}

fn main() {
    println!("cargo:rerun-if-env-changed=RUSTNZB_BUILD_REF");
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-changed=frontend/src/");
    println!("cargo:rerun-if-changed=frontend/angular.json");
    println!("cargo:rerun-if-env-changed=RUSTNZB_SKIP_FRONTEND_BUILD");

    let package_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let build_ref = env::var("RUSTNZB_BUILD_REF")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| ci_ref("CI_COMMIT_TAG"))
        .or_else(|| ci_ref("CI_COMMIT_SHA"))
        .or_else(git_head_ref);
    let build_version = build_ref
        .map(|build_ref| format!("{package_version}+{build_ref}"))
        .unwrap_or(package_version);
    println!("cargo:rustc-env=RUSTNZB_BUILD_VERSION={build_version}");

    let dist = "frontend/dist/frontend/browser";

    // If a real dist already exists (e.g. CI pre-built it or a prior build
    // succeeded), skip rebuilding. Placeholder output should not suppress a
    // later retry once npm connectivity is restored.
    if let Ok(existing) = std::fs::read_to_string(std::path::Path::new(dist).join("index.html"))
        && existing != PLACEHOLDER_HTML
    {
        return;
    }

    if env::var_os("RUSTNZB_SKIP_FRONTEND_BUILD").is_some() {
        write_placeholder(dist);
        return;
    }

    // Try to run ng build if frontend exists
    if std::path::Path::new("frontend/package.json").exists() {
        let frontend_dir = "frontend";
        let ng_bin = std::path::Path::new(frontend_dir).join("node_modules/.bin/ng");

        if !ng_bin.exists() {
            match Command::new("npm")
                .args(["ci", "--no-audit", "--no-fund"])
                .current_dir(frontend_dir)
                .status()
            {
                Ok(status) if status.success() => {}
                Ok(status) => {
                    println!(
                        "cargo:warning=Frontend dependency install failed with exit code {:?}",
                        status.code()
                    );
                    write_placeholder(dist);
                    return;
                }
                Err(e) => {
                    println!("cargo:warning=Could not run npm ci: {e}");
                    write_placeholder(dist);
                    return;
                }
            }
        }

        match Command::new("npm")
            .args(["run", "build", "--", "--configuration=production"])
            .current_dir(frontend_dir)
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
                println!("cargo:warning=Could not run npm build: {e}");
            }
        }
    }

    // Create minimal placeholder so rust-embed has something to embed
    write_placeholder(dist);
}

fn ci_ref(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn git_head_ref() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}
