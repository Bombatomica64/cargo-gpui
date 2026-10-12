//! Patches (A1): recompile the app crate with the recorded rustc command,
//! link its objects against the running app and push the result with a jump
//! table. The steps follow `dx serve --hotpatch` (Dioxus CLI,
//! packages/cli/src/build/{link,patch}.rs, MIT OR Apache-2.0), reduced to
//! ELF/aarch64; credits in ../../README.md.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use anyhow::anyhow;
use serde::Serialize;

use crate::{
    device,
    library::{self, Rustc, cgu_name, hash_file, ndk_bin, tip_objects},
    project::Project,
    read_json, stub, write_json,
};

/// How long the app gets to apply a patch.
const APPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Cargo's jobserver file descriptors don't exist outside cargo.
const JOBSERVER_VARS: [&str; 3] = ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"];

/// What the `gpui-hot` crate reads: the jump table from the app's own code,
/// plus one from each earlier patch, which the app rebases to where it
/// loaded that patch.
#[derive(Serialize)]
struct Patch {
    /// Identifies this patcher: generations restart with each one.
    session: u64,
    generation: u32,
    table: subsecond_types::JumpTable,
    earlier: Vec<EarlierMap>,
}

#[derive(Serialize)]
struct EarlierMap {
    lib: String,
    map: Vec<(u64, u64)>,
}

struct Earlier {
    lib: String,
    hot: HashMap<String, u64>,
}

#[derive(Debug)]
pub enum PatchError {
    /// The code doesn't compile: wait for the next save.
    Compile(String),
    /// Anything else: the patch can't be built or applied, a reload can.
    Other(anyhow::Error),
}

impl From<anyhow::Error> for PatchError {
    fn from(err: anyhow::Error) -> Self {
        Self::Other(err)
    }
}

impl From<std::io::Error> for PatchError {
    fn from(err: std::io::Error) -> Self {
        Self::Other(err.into())
    }
}

/// Patches for one running process of the app, built from the last base build.
pub struct Patcher {
    rustc: Rustc,
    fat_link: Vec<String>,
    base: stub::BaseSymbols,
    aslr_reference: u64,
    /// Hash of each app object as last built, to report what changed.
    objects: HashMap<String, u64>,
    earlier: Vec<Earlier>,
    session: u64,
    generation: u32,
    /// Build patches without pushing them (`CARGO_GPUI_OFFLINE`).
    offline: bool,
    /// Host copy and phone path of the last patch, the base of the next
    /// one's delta. The first patch goes whole: a delta against the staged
    /// library saves only ~20%, and costs more than that to make and apply.
    delta_base: Option<(PathBuf, String)>,
}

impl Patcher {
    pub fn new(aslr_reference: u64, offline: bool) -> anyhow::Result<Self> {
        let state = Project::get()?.state_dir();
        let t = Instant::now();
        let base = stub::BaseSymbols::load(&state.join("base.so"))?;
        println!(
            "cargo gpui: read {} base symbols in {} ms; app aslr_reference {aslr_reference:#x}",
            base.symbols.len(),
            t.elapsed().as_millis()
        );
        Ok(Self {
            rustc: read_json(&state.join("rustc.json"))?,
            fat_link: read_json(&state.join("link-fat.json"))?,
            objects: read_json::<Vec<(String, u64)>>(&state.join("base-objects.json"))?
                .into_iter()
                .collect(),
            base,
            aslr_reference,
            earlier: vec![],
            session: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
            generation: 0,
            offline,
            delta_base: None,
        })
    }

    /// Builds and pushes a patch of the current sources and waits for the app
    /// to apply it. Returns its generation, or `None` if the code didn't
    /// change.
    pub fn patch(&mut self) -> Result<Option<u32>, PatchError> {
        let project = Project::get()?;
        let state = project.state_dir();
        let mut times = vec![];
        let mut t = Instant::now();
        let mut lap = |name: &'static str, t: &mut Instant| {
            times.push((name, t.elapsed()));
            *t = Instant::now();
        };

        println!("cargo gpui: change seen, building a patch");
        // 1. Recompile the app crate with the recorded command (incremental).
        let rustc = &self.rustc;
        let out = Command::new(&rustc.rustc)
            .args(&rustc.args)
            .env_clear()
            .envs(rustc.env.iter().filter(|(k, _)| !JOBSERVER_VARS.contains(&k.as_str())).cloned())
            .env("CARGO_GPUI_SHIM", "1")
            .env("CARGO_GPUI_MODE", "thin")
            .env("CARGO_GPUI_REAL_LINKER", &rustc.linker)
            .current_dir(&rustc.cwd)
            .output()?;
        if !out.status.success() {
            return Err(PatchError::Compile(rendered_diagnostics(&out.stderr)));
        }
        lap("rustc", &mut t);

        // 2. Link the app's fresh objects, plus stubs that jump into the
        //    running app for everything else, into a shared library.
        let thin_link: Vec<String> = read_json(&state.join("link-thin.json"))?;
        //    Every app object goes in, as with `dx`: code left in the app
        //    would call the app's old copies of changed functions directly.
        let objects = tip_objects(&thin_link);
        let mut changed = 0;
        for object in &objects {
            let hash = hash_file(object)?;
            if self.objects.insert(cgu_name(object), hash) != Some(hash) {
                changed += 1;
            }
        }
        if changed == 0 {
            println!("cargo gpui: no code change");
            return Ok(None);
        }
        self.generation += 1;
        let generation = self.generation;
        let stub_path = state.join("stub.o");
        std::fs::write(&stub_path, stub::undefined_symbol_stub(&self.base, &objects, self.aslr_reference)?)?;
        lap("stub", &mut t);

        let patch = state.join(format!("patch-{generation}.so"));
        let out = Command::new(&rustc.linker)
            .args(&objects)
            .arg(&stub_path)
            .args(thin_link_args(&self.fat_link))
            .arg("-o")
            .arg(&patch)
            .env_clear()
            .envs(rustc.env.iter().cloned())
            .output()?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!("link failed:\n{stderr}").into());
        }
        library::remove_stale_objects(&objects)?;
        lap("link", &mut t);

        // 3. Map old function addresses to new ones; ship a library without DWARF.
        let device_dir = project.device_patch_dir()?;
        // Unique across sessions: the app may still hold an earlier session's patches.
        let device_lib = format!("{device_dir}/patch-{}-{generation}.so", self.session);
        let mut table = stub::jump_table(&patch, &self.base)?;
        table.lib = PathBuf::from(&device_lib);
        let stripped = state.join("push").join(Path::new(&device_lib).file_name().unwrap());
        _ = std::fs::remove_dir_all(stripped.parent().unwrap());
        std::fs::create_dir_all(stripped.parent().unwrap())?;
        let status = Command::new(ndk_bin("llvm-strip")?)
            .arg("--strip-debug")
            .arg(&patch)
            .arg("-o")
            .arg(&stripped)
            .status()?;
        if !status.success() {
            return Err(anyhow!("llvm-strip failed").into());
        }
        // Views created by an earlier patch's code (a screen opened after a
        // patch) call that patch's hot closures: map those to the new ones too.
        let hot = stub::hot_closures(&patch)?;
        let earlier = self
            .earlier
            .iter()
            .map(|e| EarlierMap {
                lib: e.lib.clone(),
                map: e.hot.iter().filter_map(|(name, old)| Some((*old, *hot.get(name)?))).collect(),
            })
            .collect();
        self.earlier.push(Earlier { lib: device_lib.clone(), hot });
        let table_path = state.join("push/patch.json");
        let hot_closures = table.map.len();
        write_json(&table_path, &Patch { session: self.session, generation, table, earlier })?;
        _ = std::fs::remove_file(&patch);
        lap("table+strip", &mut t);

        // 4. Push library and table, the table last, and wait for the app
        //    to apply them: one adb call with a delta (each costs ~0.5 s
        //    through a tunnel).
        let size = std::fs::metadata(&stripped)?.len();
        let mut sent = String::new();
        if !self.offline {
            sent = device::push_patch(
                &stripped,
                &table_path,
                &device_lib,
                self.delta_base.as_ref(),
                generation,
                APPLY_TIMEOUT,
            )?;
            sent = format!(", sent as {sent}");
            lap("push+apply", &mut t);
        }
        let base = state.join("patch-base.so");
        std::fs::rename(&stripped, &base)?;
        self.delta_base = Some((base, device_lib));
        if !self.offline {
            // Offline, the table stays in push/ for inspection.
            _ = std::fs::remove_dir_all(stripped.parent().unwrap());
        }

        let total: Duration = times.iter().map(|(_, d)| *d).sum();
        let detail: Vec<String> = times.iter().map(|(n, d)| format!("{n} {} ms", d.as_millis())).collect();
        println!(
            "cargo gpui: patch {generation}: {changed}/{} objects changed, {hot_closures} hot closures, {:.1} MB{sent}, {} ms ({})",
            objects.len(),
            size as f64 / 1e6,
            total.as_millis(),
            detail.join(", ")
        );
        Ok(Some(generation))
    }
}

/// cargo asked rustc for JSON diagnostics; this is what cargo would print.
fn rendered_diagnostics(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|d| d["rendered"].as_str().map(String::from))
                .unwrap_or_else(|| format!("{line}\n"))
        })
        .collect()
}

/// The arguments of the base link that a patch needs (the Dioxus CLI's
/// Gnu-flavour thin link, packages/cli/src/build/link.rs).
fn thin_link_args(fat: &[String]) -> Vec<String> {
    let mut out: Vec<String> = [
        "-shared",
        "-Wl,--eh-frame-hdr",
        "-Wl,-z,noexecstack",
        "-Wl,-z,relro,-z,now",
        "-nodefaultlibs",
        "-Wl,-Bdynamic",
    ]
    .map(String::from)
    .to_vec();
    for (i, arg) in fat.iter().enumerate() {
        if arg == "-L" || arg == "-target" {
            out.push(arg.clone());
            out.push(fat[i + 1].clone());
        } else if arg.starts_with("-l")
            || arg.starts_with("-m")
            || arg.starts_with("-L")
            || arg.starts_with("-Wl,--target=")
            || arg.starts_with("-fuse-ld")
            || arg.starts_with("-B")
            || arg.contains("-ld-path")
        {
            out.push(arg.clone());
        }
    }
    out
}
