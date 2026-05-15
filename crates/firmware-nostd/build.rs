//! Build script: load the workspace-root `.env` and re-export every
//! `KEY=VALUE` line as a `rustc-env` var, so the firmware can read
//! WiFi + MQTT credentials via `env!(...)` without them ever living
//! in source (`.env` is gitignored).

use std::{env, fs, path::PathBuf};

fn main() {
    let env_path = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("..")
        .join("..")
        .join(".env");
    println!("cargo:rerun-if-changed={}", env_path.display());

    let contents = fs::read_to_string(&env_path).unwrap_or_else(|_| {
        panic!(
            "missing {} — copy .env.example and fill WiFi + MQTT credentials",
            env_path.display()
        )
    });
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            println!("cargo:rustc-env={}={}", k.trim(), v.trim());
        }
    }
}
