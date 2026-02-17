fn main() {
    // Only build frontend if dist/ is missing index.html or in release mode.
    // During dev, run `cd frontend && npm run dev` separately for HMR.
    let dist_index = std::path::Path::new("dist/index.html");
    if !dist_index.exists() {
        // Check if frontend/ directory exists with package.json
        let frontend_pkg = std::path::Path::new("frontend/package.json");
        if frontend_pkg.exists() {
            // Check if node_modules exists, install if not
            let node_modules = std::path::Path::new("frontend/node_modules");
            if !node_modules.exists() {
                let status = std::process::Command::new("npm")
                    .args(["install"])
                    .current_dir("frontend")
                    .status();
                if let Err(e) = status {
                    println!("cargo:warning=Failed to run npm install: {}", e);
                    println!("cargo:warning=SPA will not be embedded. Run `cd frontend && npm install && npm run build` manually.");
                    return;
                }
            }

            let status = std::process::Command::new("npm")
                .args(["run", "build"])
                .current_dir("frontend")
                .status();
            match status {
                Ok(s) if s.success() => {
                    println!("cargo:warning=Frontend built successfully into dist/");
                }
                Ok(s) => {
                    println!(
                        "cargo:warning=Frontend build failed with status: {}. Dashboard will use legacy HTML fallback.",
                        s
                    );
                }
                Err(e) => {
                    println!("cargo:warning=Failed to run npm build: {}. Dashboard will use legacy HTML fallback.", e);
                }
            }
        }
    }

    // Rebuild if frontend source changes
    println!("cargo:rerun-if-changed=frontend/src");
    println!("cargo:rerun-if-changed=frontend/index.html");
    println!("cargo:rerun-if-changed=frontend/vite.config.ts");
    println!("cargo:rerun-if-changed=dist");
}
