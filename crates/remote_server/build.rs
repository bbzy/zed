#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]
use std::process::Command;

const ZED_MANIFEST: &str = include_str!("../zed/Cargo.toml");

fn main() {
    let zed_cargo_toml =
        toml_edit::ImDocument::parse(ZED_MANIFEST).expect("failed to parse zed Cargo.toml");
    let version = zed_cargo_toml
        .get("package")
        .and_then(|package| package.get("version"))
        .and_then(toml_edit::Item::as_str)
        .expect("zed Cargo.toml must declare a package version string");
    println!("cargo:rustc-env=ZED_PKG_VERSION={version}");
    println!(
        "cargo:rustc-env=TARGET={}",
        std::env::var("TARGET").unwrap()
    );

    // Read the source revision without snapshotting the working copy.
    println!("cargo:rerun-if-env-changed=ZED_COMMIT_SHA");
    let commit_sha = std::env::var("ZED_COMMIT_SHA").ok().or_else(|| {
        Command::new("kk")
            .args([
                "--ignore-working-copy",
                "log",
                "--no-graph",
                "-r",
                "@",
                "-T",
                "commit_id",
            ])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    });
    if let Some(commit_sha) = commit_sha {
        println!("cargo:rustc-env=ZED_COMMIT_SHA={commit_sha}");
    }
    if let Some(build_identifier) = option_env!("GITHUB_RUN_NUMBER") {
        println!("cargo:rustc-env=ZED_BUILD_ID={build_identifier}");
    }
}
