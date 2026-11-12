use std::path::PathBuf;
use std::process::Command;

/// macOS: the key patch library loaded into the game copy (app/macos/keypatch.c), universal so that it fits the
/// game whether it runs native or under Rosetta. Embedded in the launcher with include_bytes!.
fn build_keypatch() {
    let src = "../macos/keypatch.c";
    println!("cargo:rerun-if-changed={src}");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("libforever_keypatch.dylib");
    let status = Command::new("clang")
        .args(["-dynamiclib", "-O2", "-Wall", "-arch", "arm64", "-arch", "x86_64", "-mmacosx-version-min=11.0", "-o"])
        .arg(&out)
        .arg(src)
        .status()
        .expect("clang not found (xcode-select --install)");
    assert!(status.success(), "building {src} failed");
}

/// The public certificate of your server's CA (scripts/gen-certs.sh writes it to certs/ca/forever_root_ca.crt):
/// the FOREVER_CA_CERT variable (its PEM text, as GitHub Actions passes it), else resources/forever_root_ca.crt.
fn embed_ca() {
    let file = "resources/forever_root_ca.crt";
    println!("cargo:rerun-if-env-changed=FOREVER_CA_CERT");
    println!("cargo:rerun-if-changed={file}");
    let pem = match std::env::var("FOREVER_CA_CERT") {
        Ok(text) if !text.trim().is_empty() => text,
        _ => std::fs::read_to_string(file).unwrap_or_else(|_| {
            panic!("no server CA: set FOREVER_CA_CERT or copy certs/ca/forever_root_ca.crt (never the .key) to app/src-tauri/{file}")
        }),
    };
    assert!(pem.contains("-----BEGIN CERTIFICATE-----"), "the server CA is not a PEM certificate");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("forever_root_ca.crt");
    std::fs::write(out, pem.trim().to_string() + "\n").unwrap();
}

/// Linux/SteamOS: PE64 helper runs in the game's own Wine/Proton prefix.
fn build_keypatch_linux() {
    let src = "../linux/keypatch.c";
    println!("cargo:rerun-if-changed={src}");
    println!("cargo:rerun-if-changed=../linux/keystore.h");
    println!("cargo:rerun-if-env-changed=FOREVER_MINGW_CC");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("forever-keypatch.exe");
    let compiler = std::env::var("FOREVER_MINGW_CC").unwrap_or_else(|_| "x86_64-w64-mingw32-gcc".into());
    let status = Command::new(compiler)
        .args(["-std=c11", "-municode", "-O2", "-Wall", "-Wextra", "-Werror", "-static", "-o"])
        .arg(&out)
        .arg(src)
        .status()
        .expect("MinGW compiler not found (install gcc-mingw-w64-x86-64)");
    assert!(status.success(), "building {src} failed");
}

fn main() {
    embed_ca();
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        build_keypatch();
    }
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        build_keypatch_linux();
    }
    // FOREVER_CHECK_ONLY=1: lets `cargo check --target x86_64-pc-windows-gnu` run on Linux without the Windows
    // resource compiler (real builds never set it)
    if std::env::var_os("FOREVER_CHECK_ONLY").is_some() {
        println!("cargo:rustc-cfg=desktop");
        println!("cargo:rustc-check-cfg=cfg(desktop, mobile)");
        return;
    }
    tauri_build::build()
}
