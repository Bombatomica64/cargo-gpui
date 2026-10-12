//! The base build: the app's library, compiled with this tool as cargo's
//! `RUSTC_WORKSPACE_WRAPPER` and the app's linker, so that patches can replay
//! the app crate's rustc command and link against the result. The approach is
//! that of the Dioxus CLI's fat builds (`dx serve --hotpatch`).

use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
    time::{Instant, SystemTime},
};

use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};

use crate::{
    project::{Project, ROOT_VAR},
    read_json, run_status, write_json,
};

/// The app crate's rustc invocation, recorded during the base build and
/// replayed per patch.
#[derive(Serialize, Deserialize)]
pub struct Rustc {
    pub rustc: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub linker: String,
}

pub fn ndk_bin(tool: &str) -> anyhow::Result<PathBuf> {
    let sdk = env::var("ANDROID_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env::var("HOME").unwrap()).join("Android/Sdk"));
    let ndk = match env::var("ANDROID_NDK_HOME") {
        Ok(ndk) => PathBuf::from(ndk),
        Err(_) => {
            let mut all: Vec<_> = std::fs::read_dir(sdk.join("ndk"))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect();
            all.sort();
            all.pop().context("no NDK under $ANDROID_HOME/ndk")?
        }
    };
    Ok(ndk.join("toolchains/llvm/prebuilt/linux-x86_64/bin").join(tool))
}

/// When [`build`] last touched src/lib.rs: not a change the user made.
static LAST_TOUCH: std::sync::Mutex<Option<SystemTime>> = std::sync::Mutex::new(None);

pub fn last_touch() -> Option<SystemTime> {
    *LAST_TOUCH.lock().unwrap()
}

/// Builds the app library (arm64 debug), records how, keeps it as the base
/// for patches and leaves a copy without DWARF where Gradle packages it.
/// Returns that copy.
pub fn build(build_args: &[String]) -> anyhow::Result<PathBuf> {
    let project = Project::get()?;
    let state = project.state_dir();
    std::fs::create_dir_all(&state)?;
    // Make cargo re-run rustc (and so the linker) for the app crate.
    let now = SystemTime::now();
    std::fs::File::options()
        .append(true)
        .open(project.root.join("src/lib.rs"))?
        .set_modified(now)?;
    *LAST_TOUCH.lock().unwrap() = Some(now);
    let t = Instant::now();
    let status = Command::new("cargo")
        .args(["ndk", "-t", "arm64-v8a", "--platform", "26", "build"])
        .args(build_args)
        .current_dir(&project.root)
        .env("RUSTC_WORKSPACE_WRAPPER", env::current_exe()?)
        .env("CARGO_GPUI_MODE", "fat")
        .env(ROOT_VAR, &project.root)
        .status()?;
    ensure!(status.success(), "cargo ndk failed");
    let cargo_ms = t.elapsed().as_millis();
    let rustc_ms: u128 = read_json(&state.join("rustc-ms.json"))?;
    let link_ms: u128 = read_json(&state.join("link-ms.json"))?;
    let t = Instant::now();
    save_base()?;
    let save_ms = t.elapsed().as_millis();

    // The phone needs no DWARF (patches link against base.so, Subsecond
    // finds `main` via the dynamic symbol table). Only non-loaded sections
    // go: addresses are unchanged. Stripping from cargo's output also skips
    // cargo-ndk's copy (`-o`) of the full library.
    let packaged = project.jni_lib();
    let dir = packaged.parent().unwrap();
    _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir)?;
    let status = Command::new(ndk_bin("llvm-strip")?)
        .arg("--strip-debug")
        .arg(state.join("base.so"))
        .arg("-o")
        .arg(&packaged)
        .status()?;
    ensure!(status.success(), "llvm-strip failed");
    println!(
        "cargo gpui: built in {} ms (app crate {} ms, link {link_ms} ms, cargo {} ms, save base {save_ms} ms, strip {} ms)",
        cargo_ms + t.elapsed().as_millis(),
        rustc_ms.saturating_sub(link_ms),
        cargo_ms.saturating_sub(rustc_ms),
        t.elapsed().as_millis() - save_ms,
    );
    Ok(packaged)
}

/// `cargo gpui apk`: the base build, then Gradle. Returns the APK in dist/.
pub fn apk(build_args: &[String]) -> anyhow::Result<PathBuf> {
    let project = Project::get()?;
    build(build_args)?;
    // Gradle repackages in place and would keep the old library's bytes as dead space.
    let built = project.root.join("android/app/build/outputs/apk/debug/app-debug.apk");
    _ = std::fs::remove_file(&built);
    let status = Command::new(project.root.join("android/gradlew"))
        .args(["--no-daemon", "-q", "assembleDebug"])
        .current_dir(project.root.join("android"))
        .status()?;
    ensure!(status.success(), "gradle failed");
    let apk = debug_apk(project);
    std::fs::create_dir_all(apk.parent().unwrap())?;
    std::fs::copy(&built, &apk)?;
    println!("cargo gpui: {} ({} MB)", apk.display(), std::fs::metadata(&apk)?.len() / 1_000_000);
    Ok(apk)
}

pub fn debug_apk(project: &Project) -> PathBuf {
    project
        .root
        .join(format!("dist/{}-{}-debug.apk", project.name, project.version))
}

/// Keeps the base library and its objects' hashes for patching.
pub fn save_base() -> anyhow::Result<()> {
    let state = Project::get()?.state_dir();
    let fat_link: Vec<String> = read_json(&state.join("link-fat.json"))?;
    let base = link_output(&fat_link)?;
    // A hard link, not a copy: lld replaces its output file rather than
    // writing into it, so the next build leaves this one alone.
    let saved = state.join("base.so");
    _ = std::fs::remove_file(&saved);
    if std::fs::hard_link(&base, &saved).is_err() {
        std::fs::copy(&base, &saved)?;
    }
    // Patches report how many objects differ from these.
    let hashes: Vec<(String, u64)> = tip_objects(&fat_link)
        .into_iter()
        .map(|o| Ok((cgu_name(&o), hash_file(&o)?)))
        .collect::<anyhow::Result<_>>()?;
    write_json(&state.join("base-objects.json"), &hashes)?;
    remove_stale_objects(&tip_objects(&fat_link))?;
    Ok(())
}

/// cargo calls `<wrapper> <rustc> <args...>` for workspace crates.
pub fn wrap_rustc(args: &[String]) -> anyhow::Result<ExitCode> {
    let (rustc, rest) = (&args[0], &args[1..]);
    let fat = env::var("CARGO_GPUI_MODE").as_deref() == Ok("fat");
    let project = Project::get();
    let is_tip = |project: &Project| {
        rest.windows(2).any(|w| w[0] == "--crate-name" && w[1] == project.tip)
            && rest.iter().any(|a| a == "cdylib")
    };
    let project = match project {
        Ok(project) if fat && is_tip(project) => project,
        _ => return run_status(Command::new(rustc).args(rest)),
    };
    let linker = rest
        .windows(2)
        .find_map(|w| (w[0] == "-C").then(|| w[1].strip_prefix("linker=")).flatten())
        .or_else(|| rest.iter().find_map(|a| a.strip_prefix("-Clinker=")))
        .context("no -C linker= in the app's rustc args")?
        .to_string();
    let mut args = rest.to_vec();
    args.extend(
        [
            // Keep the object files (patches relink them) and every function,
            // so patches can call anything the app crate defines.
            "-Csave-temps=true",
            "-Clink-dead-code",
            "-Clink-arg=-Wl,--no-gc-sections",
            // Crate-local ThinLTO is most of an incremental rebuild (~5 of
            // ~6 s); without it a one-line change recompiles in ~1.2 s.
            "-Clto=off",
        ]
        .map(String::from),
    );
    args.push(format!("-Clinker={}", env::current_exe()?.display()));
    let state = project.state_dir();
    let recorded = Rustc {
        rustc: rustc.clone(),
        args: args.clone(),
        env: env::vars().collect(),
        cwd: env::current_dir()?,
        linker: linker.clone(),
    };
    write_json(&state.join("rustc.json"), &recorded)?;
    let t = Instant::now();
    let status = run_status(
        Command::new(rustc)
            .args(&args)
            .env("CARGO_GPUI_SHIM", "1")
            .env("CARGO_GPUI_REAL_LINKER", &linker),
    );
    write_json(&state.join("rustc-ms.json"), &t.elapsed().as_millis())?;
    status
}

/// rustc's linker for the app: records the arguments, then links (fat) or
/// stops (thin: the patcher links the patch itself).
pub fn link_shim(args: &[String]) -> anyhow::Result<ExitCode> {
    let mut expanded = vec![];
    for arg in args {
        match arg.strip_prefix('@') {
            Some(file) => expanded.extend(std::fs::read_to_string(file)?.lines().map(String::from)),
            None => expanded.push(arg.clone()),
        }
    }
    let state = Project::get()?.state_dir();
    let mode = env::var("CARGO_GPUI_MODE").unwrap_or_else(|_| "fat".into());
    write_json(&state.join(format!("link-{mode}.json")), &expanded)?;
    if mode == "thin" {
        return Ok(ExitCode::SUCCESS);
    }
    let t = Instant::now();
    let status = run_status(Command::new(env::var("CARGO_GPUI_REAL_LINKER")?).args(args));
    write_json(&state.join("link-ms.json"), &t.elapsed().as_millis())?;
    status
}

/// Each `-Csave-temps` compilation leaves ~65 MB of objects with new names.
pub fn remove_stale_objects(current: &[PathBuf]) -> anyhow::Result<()> {
    let Some(dir) = current.first().and_then(|o| o.parent()) else {
        return Ok(());
    };
    let tip = &Project::get()?.tip;
    for path in std::fs::read_dir(dir)? {
        let path = path?.path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name.starts_with(&format!("{tip}.")) && name.ends_with(".rcgu.o") && !current.contains(&path) {
            _ = std::fs::remove_file(&path);
        }
    }
    Ok(())
}

pub fn tip_objects(link_args: &[String]) -> Vec<PathBuf> {
    link_args
        .iter()
        .filter(|a| a.ends_with(".rcgu.o"))
        .map(PathBuf::from)
        .collect()
}

/// `<crate>.<cgu>.<session>.rcgu.o` without the session part, which rustc
/// changes on every compilation.
pub fn cgu_name(object: &Path) -> String {
    let name = object.file_name().unwrap().to_string_lossy();
    name.split('.').take(2).collect::<Vec<_>>().join(".")
}

pub fn hash_file(path: &Path) -> anyhow::Result<u64> {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?
        .hash(&mut hasher);
    Ok(hasher.finish())
}

pub fn link_output(args: &[String]) -> anyhow::Result<PathBuf> {
    let i = args.iter().position(|a| a == "-o").context("no -o in link args")?;
    Ok(PathBuf::from(&args[i + 1]))
}
