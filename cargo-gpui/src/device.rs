//! The phone: adb, pushing libraries, reloads (A2) and installs.

use std::{
    env,
    path::{Path, PathBuf},
    io::Write,
    process::{Command, Stdio},
    sync::OnceLock,
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
    let out = adb_output(args, None)?;
    ensure!(
        out.status.success(),
        "adb {args:?}: {}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs `adb shell -T <script>` with `input` on stdin; returns its exit
/// code (adb's shell protocol passes it on) and output.
fn shell_with_input(script: &str, input: Option<&[u8]>) -> anyhow::Result<(i32, String)> {
    let out = adb_output(&["shell", "-T", script], input)?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok((out.status.code().unwrap_or(-1), text))
}

fn adb_output(args: &[&str], input: Option<&[u8]>) -> anyhow::Result<std::process::Output> {
    let custom = env::var("CARGO_GPUI_ADB").ok();
    let mut words = custom.as_deref().unwrap_or("adb").split_whitespace();
    let mut child = Command::new(words.next().context("empty CARGO_GPUI_ADB")?)
        .args(words)
        .args(args)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    std::thread::scope(|scope| {
        if let Some(input) = input {
            let mut stdin = child.stdin.take().unwrap();
            // A write error means adb exited early; its status says why.
            scope.spawn(move || _ = stdin.write_all(input));
        }
        Ok(child.wait_with_output()?)
    })
}

/// Exit code of a device script whose delta could not be applied (no base,
/// or not the base it was made from): the caller sends the whole file.
const DELTA_FAILED: i32 = 42;

/// Waits on the phone, in one adb call, for the app to write `files/dev/aslr`
/// for its current process; up to `timeout`, polling every 100 ms.
pub fn wait_for_aslr(timeout: Duration) -> anyhow::Result<u64> {
    let package = Project::get()?.package()?;
    // `<hex> <pid>`; a file left by an earlier process doesn't count.
    let script = poll_script(
        timeout,
        0.1,
        &format!(
            "p=$(pidof {package}) && a=$(run-as {package} cat files/dev/aslr 2>/dev/null) \
             && [ \"${{a#* }}\" = \"${{p%% *}}\" ] && echo \"$a\""
        ),
    );
    let (code, out) = shell_with_input(&script, None)?;
    if code != 0 {
        bail!("{package} did not report its aslr_reference (is it running a debug build with gpui_hot::init?)");
    }
    let hex = out.split_whitespace().next().unwrap_or_default();
    Ok(u64::from_str_radix(hex.trim_start_matches("0x"), 16)?)
}

/// A device shell loop that runs `check` every `every` seconds until it
/// succeeds (exit 0) or `timeout` runs out (exit 1).
fn poll_script(timeout: Duration, every: f64, check: &str) -> String {
    let tries = (timeout.as_secs_f64() / every).ceil().max(1.0) as u32;
    format!("i=0; until {check}; do i=$((i+1)); [ $i -ge {tries} ] && exit 1; sleep {every}; done")
}

/// Waits on the phone for the app to acknowledge patch `generation`, so the
/// answer costs no extra adb round trips when appended to the push.
fn wait_applied_script(generation: u32, timeout: Duration) -> anyhow::Result<String> {
    let package = Project::get()?.package()?;
    Ok(poll_script(
        timeout,
        0.05,
        &format!(
            "a=$(run-as {package} cat files/dev/applied 2>/dev/null) \
             && case $a in \"{generation} \"*) echo \"applied: $a\";; *) false;; esac"
        ),
    ))
}

/// `Ok(())` if the app applied patch `generation`, from the output of
/// [`wait_applied_script`].
fn check_applied(generation: u32, timeout: Duration, code: i32, out: &str) -> anyhow::Result<()> {
    let line = out.lines().find_map(|l| l.strip_prefix("applied: "));
    match line.map(|l| l.trim().strip_prefix(&format!("{generation} ")).unwrap_or(l)) {
        Some("ok") if code == 0 => Ok(()),
        Some(error) => bail!("the app could not apply patch {generation}: {error}"),
        None if code == 1 => bail!(
            "the app did not acknowledge patch {generation} within {timeout:?}{}",
            recent_log().map(|l| format!("; its log:\n{l}")).unwrap_or_default()
        ),
        None => bail!("pushing patch {generation} failed ({code}): {out}"),
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

/// Puts patch library `lib` at `device_lib` and its table at `patch.json`
/// (last: the app picks the patch up when the table appears), then waits for
/// the app to apply it. Sends a delta against `base` (the host's copy and the
/// phone's path of an earlier patch or library) when it can, applied by
/// `unpatch` in the same adb call. Returns what was sent.
pub fn push_patch(
    lib: &Path,
    table: &Path,
    device_lib: &str,
    base: Option<&(PathBuf, String)>,
    generation: u32,
    timeout: Duration,
) -> anyhow::Result<String> {
    let dir = Project::get()?.device_patch_dir()?;
    let wait = wait_applied_script(generation, timeout)?;
    let mut fallback = "full patch".to_string();
    if let Some((host_base, device_base)) = base {
        // -9: 0.1 MB in 0.2 s, against 0.3 MB in 0.1 s at -3.
        match delta(host_base, lib, 9).and_then(|delta| {
            let mut input = (delta.len() as u64).to_le_bytes().to_vec();
            input.extend_from_slice(&delta);
            input.extend(std::fs::read(table)?);
            let unpatch = unpatch_on_device()?;
            let script = format!(
                "{unpatch} {device_base} - {device_lib} {dir}/patch.json || exit {DELTA_FAILED}; {wait}"
            );
            Ok((delta.len(), shell_with_input(&script, Some(&input))?))
        }) {
            Ok((_, (DELTA_FAILED, out))) => fallback = format!("full patch: delta failed: {}", out.trim()),
            Ok((bytes, (code, out))) => {
                check_applied(generation, timeout, code, &out)?;
                return Ok(format!("{:.2} MB delta", bytes as f64 / 1e6));
            }
            Err(err) => fallback = format!("full patch: {err:#}"),
        }
    }
    // One push for both files: they go in argument order, the table last.
    adb(&["push", "-z", "zstd", lib.to_str().unwrap(), table.to_str().unwrap(), &format!("{dir}/")])?;
    let (code, out) = shell_with_input(&wait, None)?;
    check_applied(generation, timeout, code, &out)?;
    Ok(fallback)
}

/// Clears the patches of earlier processes and their acknowledgement:
/// generations restart at 1 with each patcher.
pub fn reset_patches() -> anyhow::Result<()> {
    adb(&["shell", &reset_patches_script()?])?;
    Ok(())
}

fn reset_patches_script() -> anyhow::Result<String> {
    let project = Project::get()?;
    let (dir, package) = (project.device_patch_dir()?, project.package()?);
    Ok(format!(
        "rm -rf {dir} && mkdir -p {dir} && chmod 755 {dir}/.. {dir} && run-as {package} rm -f files/dev/applied"
    ))
}

/// A2: run a new build without reinstalling. Puts the stripped library in
/// the app's files/dev/ (the debug activity loads it from there) and
/// restarts the app with the intent extras it remembered
/// (`gpui_hot::remember`), clearing the patches of the old process. With a
/// delta, all of that is one adb call.
pub fn reload(build_args: &[String]) -> anyhow::Result<()> {
    let project = Project::get()?;
    let mut times = vec![];
    let mut t = Instant::now();
    let mut lap = |name: &'static str, t: &mut Instant| {
        times.push((name, t.elapsed()));
        *t = Instant::now();
    };
    let lib = library::build(build_args)?;
    lap("build", &mut t);

    let dir = project.device_stage_dir()?;
    // The staged copy stays on the phone as the base of the next delta;
    // `pushed` is the host's copy of it.
    let staged = format!("{dir}/lib.so");
    let pushed = project.state_dir().join("device-lib.so");
    let restart = restart_script(&staged)?;
    let delta = (|| {
        ensure!(pushed.exists(), "no library staged on the phone yet");
        let delta = delta(&pushed, &lib, 3)?;
        let mut input = (delta.len() as u64).to_le_bytes().to_vec();
        input.extend_from_slice(&delta);
        let unpatch = unpatch_on_device()?;
        let script = format!(
            "{{ {unpatch} {staged} - {staged}.new && mv {staged}.new {staged}; }} || exit {DELTA_FAILED}; {restart}"
        );
        Ok((delta.len(), shell_with_input(&script, Some(&input))?))
    })();
    let (sent, (code, out)) = match delta {
        Ok((bytes, (code, out))) if code != DELTA_FAILED => {
            lap("delta+restart", &mut t);
            (format!("{:.1} MB delta", bytes as f64 / 1e6), (code, out))
        }
        delta => {
            let why = match delta {
                Ok((_, (_, out))) => out.trim().to_string(),
                Err(err) => format!("{err:#}"),
            };
            println!("cargo gpui: full push ({why})");
            _ = std::fs::remove_file(&pushed);
            adb(&["shell", &format!("rm -rf {dir} && mkdir -p {dir} && chmod 755 {DEVICE_TOOLS} {dir}")])?;
            push_chunked(&lib, &staged)?;
            lap("push", &mut t);
            let result = shell_with_input(&restart, None)?;
            lap("restart", &mut t);
            ("full library".to_string(), result)
        }
    };
    std::fs::copy(&lib, &pushed)?;
    ensure!(code == 0 && out.contains("Status: ok"), "restarting the app failed ({code}): {out}");

    let total: Duration = times.iter().map(|(_, d)| *d).sum();
    let detail: Vec<String> = times.iter().map(|(n, d)| format!("{n} {} ms", d.as_millis())).collect();
    let extras: Vec<&str> = out.lines().filter_map(|l| l.strip_prefix("extra: ")).collect();
    println!(
        "cargo gpui: reloaded {:.0} MB ({sent}){} in {} ms ({})",
        std::fs::metadata(&lib)?.len() as f64 / 1e6,
        if extras.is_empty() { String::new() } else { format!(" with {}", extras.join(", ")) },
        total.as_millis(),
        detail.join(", ")
    );
    Ok(())
}

/// Device shell: copy the staged library where the debug activity loads it
/// (read-only: loaded code must be, for apps targeting Android 14), stop the
/// app, clear its patches, and start it again with the extras it remembered
/// (`key=value` lines, passed as `--es key value`).
fn restart_script(staged: &str) -> anyhow::Result<String> {
    let project = Project::get()?;
    let package = project.package()?;
    let name = format!("lib{}.so", project.tip);
    Ok(format!(
        "run-as {package} sh -c 'mkdir -p files/dev && rm -f files/dev/{name} \
         && cp {staged} files/dev/{name} && chmod 444 files/dev/{name}' \
         && am force-stop {package} && run-as {package} rm -f files/dev/aslr && {reset} \
         && run-as {package} cat files/dev/extras 2>/dev/null | {{ set --; \
         while IFS= read -r l; do case $l in *=*) echo \"extra: $l\"; set -- \"$@\" --es \"${{l%%=*}}\" \"${{l#*=}}\";; esac; done; \
         am start -W -n {package}/{activity} \"$@\"; }}",
        reset = reset_patches_script()?,
        activity = project.activity,
    ))
}

/// A zstd `--patch-from` delta of `new` against `base`: a reload's library
/// against the last one (~1.4 MB instead of ~21 MB compressed), or a patch
/// against the last patch. zstd's frame checksum makes `unpatch` fail on the
/// wrong base.
fn delta(base: &Path, new: &Path, level: u8) -> anyhow::Result<Vec<u8>> {
    let out = Command::new("zstd")
        .args(["-q", "-c", &format!("-{level}"), "--patch-from"])
        .arg(base)
        .arg(new)
        .output()?;
    ensure!(out.status.success(), "zstd --patch-from failed: {}", String::from_utf8_lossy(&out.stderr));
    Ok(out.stdout)
}

/// `unpatch`'s path on the phone, pushed once per run (`--sync` skips the
/// copy when the phone's is current).
fn unpatch_on_device() -> anyhow::Result<String> {
    static PUSHED: OnceLock<()> = OnceLock::new();
    let device = format!("{DEVICE_TOOLS}/unpatch");
    if PUSHED.get().is_none() {
        adb(&["push", "--sync", unpatch_binary()?.to_str().unwrap(), &device])?;
        adb(&["shell", &format!("chmod 755 {DEVICE_TOOLS} {device}")])?;
        _ = PUSHED.set(());
    }
    Ok(device)
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
