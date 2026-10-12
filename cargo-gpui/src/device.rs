//! The phone: adb, pushing libraries, reloads (A2) and installs.

use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use anyhow::{Context, bail, ensure};

use crate::{
    library,
    project::{DEVICE_TOOLS, Project},
};

/// Runs adb. `CARGO_GPUI_ADB` replaces the command, e.g. `phone adb` for a
/// shared phone behind a lock.
pub fn adb(args: &[&str]) -> anyhow::Result<String> {
    let custom = env::var("CARGO_GPUI_ADB").ok();
    let mut words = custom.as_deref().unwrap_or("adb").split_whitespace();
    let out = Command::new(words.next().context("empty CARGO_GPUI_ADB")?)
        .args(words)
        .args(args)
        .output()?;
    ensure!(
        out.status.success(),
        "adb {args:?}: {}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Quotes `s` for the device shell.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A file from the app's `files/dev` (written by the `gpui-hot` crate), or
/// `None` if it doesn't exist.
pub fn app_file(name: &str) -> anyhow::Result<Option<String>> {
    let package = Project::get()?.package()?;
    let out = adb(&[
        "shell",
        &format!("run-as {package} cat files/dev/{name} 2>/dev/null || true"),
    ])?;
    Ok(Some(out).filter(|s| !s.is_empty()))
}

/// The running app's `aslr_reference`, waiting up to `timeout` for it to start.
pub fn wait_for_aslr(timeout: Duration) -> anyhow::Result<u64> {
    let package = Project::get()?.package()?;
    let start = Instant::now();
    loop {
        let pid = adb(&["shell", &format!("pidof {package} || true")])?;
        let pid = pid.split_whitespace().next();
        if let (Some(pid), Some(file)) = (pid, app_file("aslr")?) {
            // `<hex> <pid>`; a file left by an earlier process doesn't count.
            let mut words = file.split_whitespace();
            if let (Some(hex), Some(file_pid)) = (words.next(), words.next()) {
                if file_pid == pid {
                    return Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?);
                }
            }
        }
        if start.elapsed() > timeout {
            bail!("{package} did not report its aslr_reference (is it running a debug build with gpui_hot::init?)");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Waits for the app to acknowledge patch `generation`: `Ok(())` once
/// applied, `Err` if it failed there or didn't answer.
pub fn wait_applied(generation: u32, timeout: Duration) -> anyhow::Result<()> {
    let start = Instant::now();
    loop {
        if let Some(line) = app_file("applied")? {
            if let Some(rest) = line.trim().strip_prefix(&format!("{generation} ")) {
                return match rest {
                    "ok" => Ok(()),
                    error => bail!("the app could not apply patch {generation}: {error}"),
                };
            }
        }
        if start.elapsed() > timeout {
            bail!(
                "the app did not acknowledge patch {generation} within {timeout:?}{}",
                recent_log().map(|l| format!("; its log:\n{l}")).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The app's recent hot-patching lines and any crash, for error messages.
fn recent_log() -> Option<String> {
    let out = adb(&["logcat", "-d", "-b", "main,crash", "-t", "2000", "-v", "brief"]).ok()?;
    let lines: Vec<&str> = out
        .lines()
        .filter(|l| {
            l.contains("hot: ") || l.starts_with("F/") || l.contains("panicked") || l.contains("FATAL")
        })
        .collect();
    let tail = &lines[lines.len().saturating_sub(30)..];
    (!tail.is_empty()).then(|| tail.join("\n"))
}

/// A2: run a new build without reinstalling. Pushes the stripped library to
/// the app's files/dev/ (the debug activity loads it from there) and restarts
/// the app with the intent extras it remembered (`gpui_hot::remember`).
pub fn reload(build_args: &[String]) -> anyhow::Result<()> {
    let project = Project::get()?;
    let package = project.package()?;
    let mut times = vec![];
    let mut t = Instant::now();
    let mut lap = |name: &'static str, t: &mut Instant| {
        times.push((name, t.elapsed()));
        *t = Instant::now();
    };
    let lib = library::build(build_args)?;
    lap("build", &mut t);

    let sent = push_library(&lib)?;
    lap("push", &mut t);

    let extras = app_file("extras")?.unwrap_or_default();
    adb(&["shell", &format!("am force-stop {package}; run-as {package} rm -f files/dev/aslr")])?;
    let mut start = format!("am start -W -n {package}/{}", project.activity);
    for (key, value) in extras.lines().filter_map(|l| l.split_once('=')) {
        start.push_str(&format!(" --es {} {}", quote(key), quote(value)));
    }
    let out = adb(&["shell", &start])?;
    ensure!(out.contains("Status: ok"), "am start failed: {out}");
    lap("restart", &mut t);

    let total: Duration = times.iter().map(|(_, d)| *d).sum();
    let detail: Vec<String> = times.iter().map(|(n, d)| format!("{n} {} ms", d.as_millis())).collect();
    let extras: Vec<&str> = extras.lines().collect();
    println!(
        "cargo gpui: reloaded {:.0} MB ({sent}){} in {} ms ({})",
        std::fs::metadata(&lib)?.len() as f64 / 1e6,
        if extras.is_empty() { String::new() } else { format!(" with {}", extras.join(", ")) },
        total.as_millis(),
        detail.join(", ")
    );
    Ok(())
}

/// Puts `lib` where the debug activity loads it, sending a delta when the
/// phone has the previous one. Returns what was sent.
fn push_library(lib: &Path) -> anyhow::Result<String> {
    let project = Project::get()?;
    let package = project.package()?;
    let dir = project.device_stage_dir()?;
    // The staged copy stays on the phone as the base of the next delta;
    // `pushed` is the host's copy of it.
    let staged = format!("{dir}/lib.so");
    let pushed = project.state_dir().join("device-lib.so");
    let sent = match push_delta(lib, &pushed, &staged) {
        Ok(bytes) => format!("{:.1} MB delta", bytes as f64 / 1e6),
        Err(err) => {
            println!("cargo gpui: full push ({err:#})");
            _ = std::fs::remove_file(&pushed);
            adb(&["shell", &format!("rm -rf {dir} && mkdir -p {dir} && chmod 755 {DEVICE_TOOLS} {dir}")])?;
            push_chunked(lib, &staged)?;
            "full library".to_string()
        }
    };
    std::fs::copy(lib, &pushed)?;
    // Loaded code must be read-only for apps targeting Android 14.
    let name = format!("lib{}.so", project.tip);
    adb(&[
        "shell",
        &format!(
            "run-as {package} sh -c 'mkdir -p files/dev && rm -f files/dev/{name} \
             && cp {staged} files/dev/{name} && chmod 444 files/dev/{name}'"
        ),
    ])?;
    Ok(sent)
}

/// Sends only a zstd `--patch-from` delta against the library staged by the
/// last reload (~1.4 MB instead of ~21 MB compressed), which `unpatch`
/// applies on the phone. Errs, leaving the staged copy alone, when there is
/// no base or it isn't the one the delta was made from (zstd's checksum).
fn push_delta(lib: &Path, pushed: &Path, staged: &str) -> anyhow::Result<u64> {
    ensure!(pushed.exists(), "no library staged on the phone yet");
    let delta = Project::get()?.state_dir().join("lib.delta.zst");
    let status = Command::new("zstd")
        .args(["-q", "-f", "-3", "--patch-from"])
        .arg(pushed)
        .arg(lib)
        .arg("-o")
        .arg(&delta)
        .status()?;
    ensure!(status.success(), "zstd --patch-from failed");
    let bytes = std::fs::metadata(&delta)?.len();
    let unpatch = unpatch_binary()?;
    let result = (|| {
        let d = DEVICE_TOOLS;
        // --sync skips unpatch when the phone's copy is current.
        adb(&["push", "--sync", unpatch.to_str().unwrap(), &format!("{d}/unpatch")])?;
        adb(&["push", delta.to_str().unwrap(), &format!("{staged}.delta")])?;
        adb(&[
            "shell",
            &format!(
                "test -f {staged} && chmod 755 {d}/unpatch && {d}/unpatch {staged} {staged}.delta {staged}.new \
                 && mv {staged}.new {staged}; status=$?; rm -f {staged}.delta; exit $status"
            ),
        ])
    })();
    _ = std::fs::remove_file(&delta);
    result?;
    Ok(bytes)
}

/// The phone has no zstd binary: builds `unpatch` (../unpatch) for it.
fn unpatch_binary() -> anyhow::Result<PathBuf> {
    let dir = Project::get()?.state_dir().join("unpatch");
    std::fs::create_dir_all(dir.join("src"))?;
    for (path, contents) in [
        ("Cargo.toml", include_str!("../unpatch/Cargo.toml")),
        ("Cargo.lock", include_str!("../unpatch/Cargo.lock")),
        ("src/main.rs", include_str!("../unpatch/src/main.rs")),
    ] {
        // Unchanged files keep their mtime, so cargo has nothing to do.
        if std::fs::read_to_string(dir.join(path)).ok().as_deref() != Some(contents) {
            std::fs::write(dir.join(path), contents)?;
        }
    }
    let status = Command::new("cargo")
        .args(["ndk", "-t", "arm64-v8a", "--platform", "26", "build", "-q"])
        .current_dir(&dir)
        .env_remove("RUSTC_WORKSPACE_WRAPPER")
        .env_remove("CARGO_TARGET_DIR")
        .status()?;
    ensure!(status.success(), "building unpatch failed");
    Ok(dir.join("target/aarch64-linux-android/debug/unpatch"))
}

/// `adb push` in 16 MB parts: one call for a large file can outlast a
/// per-call timeout on a slow link. Libraries compress ~4x with zstd, which
/// adb does not use unless asked.
fn push_chunked(local: &Path, remote: &str) -> anyhow::Result<()> {
    let bytes = std::fs::read(local)?;
    let dir = Project::get()?.state_dir().join("chunks");
    _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;
    let mut parts = vec![];
    for (i, chunk) in bytes.chunks(16 << 20).enumerate() {
        let part = dir.join(format!("part{i:03}"));
        std::fs::write(&part, chunk)?;
        adb(&["push", "-z", "zstd", part.to_str().unwrap(), &format!("{remote}.part{i:03}")])?;
        parts.push(format!("{remote}.part{i:03}"));
    }
    adb(&["shell", &format!("cat {} > {remote} && rm {}", parts.join(" "), parts.join(" "))])?;
    _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// `cargo gpui install [apk]`: installs the debug APK (pushed in parts, then
/// `pm install`), drops any reloaded library, which would otherwise shadow
/// the APK's, and stages the APK's library as the base of the next delta.
pub fn install(apk: Option<&Path>) -> anyhow::Result<()> {
    let project = Project::get()?;
    let package = project.package()?;
    let apk = apk.map(Path::to_path_buf).unwrap_or_else(|| library::debug_apk(project));
    ensure!(apk.exists(), "{} not found: run `cargo gpui apk`", apk.display());
    let t = Instant::now();
    let dir = project.device_stage_dir()?;
    adb(&["shell", &format!("rm -rf {dir} && mkdir -p {dir} && chmod 755 {DEVICE_TOOLS} {dir}")])?;
    let remote = format!("{dir}/app.apk");
    push_chunked(&apk, &remote)?;
    let out = adb(&["shell", &format!("pm install -r {remote}")])?;
    ensure!(out.contains("Success"), "pm install failed: {out}");
    adb(&["shell", &format!("run-as {package} rm -rf files/dev")])?;

    // Both sides keep the APK's library as the base of the next delta.
    let pushed = project.state_dir().join("device-lib.so");
    _ = std::fs::remove_file(&pushed);
    let entry = format!("lib/arm64-v8a/lib{}.so", project.tip);
    let staged = adb(&["shell", &format!("unzip -p {remote} {entry} > {dir}/lib.so")]).and_then(|_| {
        let out = Command::new("unzip").arg("-p").arg(&apk).arg(&entry).output()?;
        ensure!(out.status.success() && !out.stdout.is_empty(), "no {entry} in the APK");
        Ok(std::fs::write(&pushed, out.stdout)?)
    });
    if let Err(err) = staged {
        _ = std::fs::remove_file(&pushed);
        println!("cargo gpui: the first reload will push the full library ({err:#})");
    }
    adb(&["shell", &format!("rm -f {remote}")])?;
    println!("cargo gpui: installed {} in {} ms", apk.display(), t.elapsed().as_millis());
    Ok(())
}
