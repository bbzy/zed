#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]
use std::process::Command;

fn main() {
    if std::env::var("ZED_UPDATE_EXPLANATION").is_ok() {
        println!(r#"cargo:rustc-cfg=feature="no-bundled-uninstall""#);
    }

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-env=MACOSX_DEPLOYMENT_TARGET=12.0");
    }

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

    #[cfg(all(windows, feature = "cli"))]
    {
        println!("cargo:rerun-if-env-changed=RELEASE_CHANNEL");
        println!("cargo:rerun-if-env-changed=GITHUB_RUN_NUMBER");

        windows_resources::compile(false).expect("failed to compile Windows resources");
    }
}
