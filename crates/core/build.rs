//! Forward selected build-time secrets from `.env` at the workspace root
//! into the compiler as env vars, so `option_env!("WC_*")` in core's
//! source picks them up.
//!
//! Only the keys listed in `WHITELIST` are forwarded — random env-style
//! lines in `.env` (Docker tokens, etc.) won't leak into the binary.
//!
//! Format: minimal KEY=VALUE / KEY="VALUE" / KEY='VALUE' parser. Lines
//! starting with `#` and blank lines are ignored. No expansions, no
//! escapes — keep `.env` simple.

use std::io::BufRead;
use std::path::PathBuf;

const WHITELIST: &[(&str, &str)] = &[
    // (key in .env, env name forwarded to rustc)
    ("SSID", "WC_WIFI_SSID"),
    ("PASSWORD", "WC_WIFI_PASSWORD"),
];

fn main() {
    let manifest = PathBuf::from(env_or_panic("CARGO_MANIFEST_DIR"));
    // workspace root is the manifest's parent's parent (crates/core/.. -> crates/.. -> workspace)
    let workspace = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(PathBuf::from)
        .unwrap_or(manifest.clone());
    let env_path = workspace.join(".env");

    println!("cargo:rerun-if-changed={}", env_path.display());
    for (_, rust_key) in WHITELIST {
        // Track external env so existing exports (CI etc.) override the file.
        println!("cargo:rerun-if-env-changed={rust_key}");
    }

    if !env_path.exists() {
        return;
    }

    let f = match std::fs::File::open(&env_path) {
        Ok(f) => f,
        Err(e) => {
            println!("cargo:warning=could not read {}: {e}", env_path.display());
            return;
        }
    };
    let reader = std::io::BufReader::new(f);
    for line in reader.lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, raw)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let val = raw.trim().trim_matches(|c| c == '"' || c == '\'');
        for (env_key, rust_key) in WHITELIST {
            if key == *env_key {
                // External env var wins (lets CI override .env without editing).
                if std::env::var_os(rust_key).is_none() {
                    println!("cargo:rustc-env={rust_key}={val}");
                }
            }
        }
    }
}

fn env_or_panic(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| panic!("missing env var: {key}"))
}
