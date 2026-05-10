use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into()))
        .join("binaries");
    let target_triple = "x86_64-pc-windows-msvc";
    let binary_path = out_dir.join(format!("hermes_agent-{}.exe", target_triple));

    let _ = std::fs::create_dir_all(&out_dir);

    // In dev, tauri-build needs the sidecar binary to exist (externalBin check).
    // We copy a real Windows executable so cargo build succeeds.
    // The sidecar spawn in lib.rs detects this is a placeholder by checking
    // the path and falls back to `app.shell().command("python")`.
    if !binary_path.exists() {
        // Try to find python.exe on PATH first (most useful as placeholder)
        let mut found = false;
        if let Ok(path) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path) {
                let py = dir.join("python.exe");
                if py.exists() {
                    let _ = std::fs::copy(&py, &binary_path);
                    println!(
                        "cargo:warning=DEV placeholder: python.exe -> {:?}",
                        binary_path
                    );
                    found = true;
                    break;
                }
            }
        }
        if !found {
            // Fallback: copy hostname.exe (tiny system exe, always present)
            let system32 =
                PathBuf::from(std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into()))
                    .join("system32");
            let hostname = system32.join("hostname.exe");
            if hostname.exists() {
                let _ = std::fs::copy(&hostname, &binary_path);
                println!(
                    "cargo:warning=DEV placeholder: hostname.exe -> {:?}",
                    binary_path
                );
            }
        }
        println!("cargo:warning=Replace with real PyInstaller build before release.");
    }

    tauri_build::build()
}
