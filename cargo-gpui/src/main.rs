//! `cargo gpui`: the dev loop for GPUI apps on Android
//! (the GPUI Mobile Lab's docs/hot-patching-plan.md, A1 and A2). Debug arm64
//! builds only.
//!
//!   cargo gpui dev [cargo args]      reload, then patch on every save; reload
//!                                    when a patch fails, or on `r` + Enter
//!   cargo gpui reload [cargo args]   rebuild, push the library, restart the
//!                                    app where it was (no Gradle, no reinstall)
//!   cargo gpui watch                 patch the running app on every save
//!   cargo gpui build [cargo args]    the app library only
//!   cargo gpui apk [cargo args]      a debug APK in dist/
//!   cargo gpui install [apk]         install it, ready for delta reloads
//!
//! The app depends on the `gpui-hot` crate and names its Android package in
//! `[package.metadata.gpui]` (see project.rs). During builds this binary is
//! also cargo's `RUSTC_WORKSPACE_WRAPPER` and the app's linker. The patch
//! steps follow the Dioxus CLI's `dx serve --hotpatch`; credits in
//! ../../README.md.

mod dev;
mod device;
mod library;
mod patch;
mod project;
mod stub;

use std::{
    env,
    path::Path,
    process::{Command, ExitCode},
};

use anyhow::Context;
use serde::{Deserialize, Serialize};

const USAGE: &str = "usage: cargo gpui <dev|reload|watch|build|apk|install> [args]";

fn main() -> ExitCode {
    let mut args: Vec<String> = env::args().skip(1).collect();
    let result = if env::var_os("CARGO_GPUI_SHIM").is_some() {
        library::link_shim(&args)
    } else if args.first().is_some_and(|a| a.ends_with("rustc")) {
        library::wrap_rustc(&args)
    } else {
        // `cargo gpui x` runs `cargo-gpui gpui x`.
        if args.first().is_some_and(|a| a == "gpui") {
            args.remove(0);
        }
        let rest = args.get(1..).unwrap_or_default();
        match args.first().map(String::as_str) {
            Some("dev") => dev::dev(rest),
            Some("reload") => device::reload(rest).map(|()| ExitCode::SUCCESS),
            Some("watch") => dev::watch(),
            Some("build") => library::build(rest).map(|_| ExitCode::SUCCESS),
            Some("apk") => library::apk(rest).map(|_| ExitCode::SUCCESS),
            Some("install") => device::install(rest.first().map(Path::new)).map(|()| ExitCode::SUCCESS),
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        }
    };
    match result {
        Ok(code) => code,
        Err(err) => {
            eprintln!("cargo gpui: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_status(cmd: &mut Command) -> anyhow::Result<ExitCode> {
    let status = cmd.status()?;
    Ok(ExitCode::from(status.code().unwrap_or(1) as u8))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> anyhow::Result<T> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> anyhow::Result<()> {
    std::fs::create_dir_all(path.parent().unwrap())?;
    Ok(std::fs::write(path, serde_json::to_vec(value)?)?)
}
