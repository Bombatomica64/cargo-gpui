//! App side of `cargo gpui dev` (the GPUI Mobile Lab's hot-patching plan): Rust
//! hot patching with Dioxus's [Subsecond](https://docs.rs/subsecond), and the
//! state a reload needs to come back where it was. Everything is a no-op
//! without debug assertions.
//!
//! - [`init`] polls for patches that `cargo gpui` pushes, loads them between
//!   frames and refreshes every window. It also writes the app's
//!   `aslr_reference`, which patches are linked against, and acknowledges
//!   each patch, so the CLI needs no logcat.
//! - Only functions reached through [`call`] switch to new code. GPUI calls
//!   `render` through vtables built when the entity was created, which a
//!   patch cannot redirect, so each view's `render` goes through it: put
//!   [`hot`] on the view's `impl Render`.
//! - [`remember`] keeps `key=value` pairs that a reload passes back to the
//!   activity as intent extras (`--es key value`), e.g. the open screen.
//!
//! Files, both readable with `run-as <package>`:
//! - `files/dev/aslr`: `<aslr_reference hex> <pid>`, written at start.
//! - `files/dev/applied`: `<generation> ok` or `<generation> error: ...`.
//! - `files/dev/extras`: the remembered pairs.
//!
//! Patches arrive in `/data/local/tmp/gpui-hot/<package>/`: a library and
//! `patch.json`, written last.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use gpui::App;

#[doc(hidden)]
pub use gpui;

/// Runs `f`, or its newest patched version once a patch has been applied.
#[inline(always)]
pub fn call<R>(f: impl FnMut() -> R) -> R {
    subsecond::call(f)
}

/// The app's package (its process name), e.g. `dev.gpui.mobile.lab`.
fn package() -> Option<String> {
    let cmdline = std::fs::read("/proc/self/cmdline").ok()?;
    let name = cmdline.split(|&b| b == 0).next()?;
    let name = String::from_utf8_lossy(name);
    // A secondary process (`pkg:remote`) shares the package's files.
    Some(name.split(':').next()?.to_string()).filter(|n| !n.is_empty())
}

/// `files/dev` in the app's data directory.
fn dev_dir() -> Option<PathBuf> {
    Some(PathBuf::from(format!("/data/data/{}/files/dev", package()?)))
}

fn write_dev_file(name: &str, contents: &str) {
    let Some(dir) = dev_dir() else { return };
    // Write then rename: the CLI may read at any moment.
    let tmp = dir.join(format!("{name}.tmp"));
    let result = std::fs::create_dir_all(&dir)
        .and_then(|()| std::fs::write(&tmp, contents))
        .and_then(|()| std::fs::rename(&tmp, dir.join(name)));
    if let Err(err) = result {
        log::warn!("hot: writing {name}: {err}");
    }
}

/// Remembers `key=value` for the next reload, which passes it to the
/// activity as an intent extra; `None` forgets it.
pub fn remember(key: &str, value: Option<&str>) {
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(path) = dev_dir().map(|d| d.join("extras")) else { return };
    let old = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<&str> = old
        .lines()
        .filter(|l| l.split_once('=').is_none_or(|(k, _)| k != key))
        .collect();
    let line = value.map(|v| format!("{key}={}", v.replace('\n', " ")));
    lines.extend(line.as_deref());
    write_dev_file("extras", &(lines.join("\n") + "\n"));
}

/// Starts watching for patches. Call once at startup, with the app's own
/// views rendering through [`call`] (see [`hot`]).
pub fn init(cx: &mut App) {
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(package) = package() else {
        log::warn!("hot: unknown package; hot patching is off");
        return;
    };
    let aslr = subsecond::aslr_reference();
    log::info!("hot: aslr_reference={aslr:#x} pid={}", std::process::id());
    write_dev_file("aslr", &format!("{aslr:#x} {}\n", std::process::id()));

    let table_path = Path::new("/data/local/tmp/gpui-hot").join(&package).join("patch.json");
    cx.spawn(async move |cx| {
        // A table left by an earlier process targets other addresses.
        let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let mut applied = modified(&table_path);
        let mut loaded = Loaded::default();
        let mut last: Option<(u64, u64)> = None;
        loop {
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            let stamp = modified(&table_path);
            if stamp.is_none() || stamp == applied {
                continue;
            }
            // adb may still be writing the table: retry on the next tick.
            let Ok(patch) = std::fs::read(&table_path)
                .map_err(anyhow::Error::from)
                .and_then(|json| Ok(serde_json::from_slice::<serde_json::Value>(&json)?))
            else {
                continue;
            };
            applied = stamp;
            let generation = patch["generation"].as_u64().unwrap_or(0);
            // adb sets the mtime after writing the file: the same table can
            // show up twice. Generations restart with each patcher session.
            let id = (patch["session"].as_u64().unwrap_or(0), generation);
            if last.is_some_and(|(session, g)| session == id.0 && g >= generation) {
                continue;
            }
            last = Some(id);
            match loaded.apply(patch) {
                Ok(entries) => {
                    log::info!("hot: patch {generation} applied ({entries} hot closures)");
                    write_dev_file("applied", &format!("{generation} ok\n"));
                    cx.update(|cx| cx.refresh_windows());
                }
                Err(err) => {
                    log::error!("hot: patch {generation} failed: {err:#}");
                    write_dev_file("applied", &format!("{generation} error: {err:#}\n"));
                }
            }
        }
    })
    .detach();
}

/// The patches loaded so far: their device path and load bias.
#[derive(Default)]
struct Loaded(Vec<(String, u64)>);

impl Loaded {
    /// `patch` is `{ session, generation, table, earlier: [{ lib, map: [[old, new]] }] }`
    /// from `cargo gpui`. Views created while an earlier patch was active
    /// call that patch's hot closures; Subsecond only redirects the app's
    /// own, so those entries are rebased here to where the earlier patch
    /// was loaded.
    fn apply(&mut self, mut patch: serde_json::Value) -> anyhow::Result<usize> {
        let mut table: subsecond::JumpTable = serde_json::from_value(patch["table"].take())?;
        // Keys become runtime addresses here, and apply_patch's own offset
        // (aslr_reference() - table.aslr_reference) becomes 0. Leaving it to
        // apply_patch would need earlier patches' keys as `bias + old - slide`,
        // which wraps when a patch loads below the app, and apply_patch's
        // add (overflow-checked in debug) then panics.
        let reference = subsecond::aslr_reference() as u64;
        let slide = reference.wrapping_sub(table.aslr_reference);
        table.aslr_reference = reference;
        table.map = table.map.iter().map(|(&old, &new)| (old.wrapping_add(slide), new)).collect();
        for earlier in patch["earlier"].as_array().into_iter().flatten() {
            let lib = earlier["lib"].as_str().unwrap_or_default();
            let Some(&(_, bias)) = self.0.iter().find(|(l, _)| l == lib) else {
                continue;
            };
            for pair in earlier["map"].as_array().into_iter().flatten() {
                if let (Some(old), Some(new)) = (pair[0].as_u64(), pair[1].as_u64()) {
                    table.map.insert(bias.wrapping_add(old), new);
                }
            }
        }
        let entries = table.map.len();
        let lib = table.lib.to_string_lossy().into_owned();
        let before = patch_biases();
        // SAFETY: the table comes from `cargo gpui`, built against this
        // process's aslr_reference; we are between frames, so no patched
        // code is running.
        unsafe { subsecond::apply_patch(table) }?;
        match patch_biases().into_iter().find(|b| !before.contains(b)) {
            Some(bias) => self.0.push((lib, bias)),
            None => log::warn!("hot: {lib} not found among loaded objects"),
        }
        Ok(entries)
    }
}

/// Load biases of the patches Subsecond loaded (memfds named `subsecond-patch`;
/// bionic reports them as `/memfd:subsecond-patch (deleted)`).
fn patch_biases() -> Vec<u64> {
    unsafe extern "C" fn each(
        info: *mut libc::dl_phdr_info,
        _: libc::size_t,
        out: *mut libc::c_void,
    ) -> libc::c_int {
        // SAFETY: called by dl_iterate_phdr with a valid entry and our Vec.
        unsafe {
            let info = &*info;
            if !info.dlpi_name.is_null()
                && std::ffi::CStr::from_ptr(info.dlpi_name)
                    .to_string_lossy()
                    .contains("subsecond-patch")
            {
                (*(out as *mut Vec<u64>)).push(info.dlpi_addr as u64);
            }
        }
        0
    }
    let mut out = Vec::new();
    // SAFETY: `each` only reads the entry and writes to `out`.
    unsafe { libc::dl_iterate_phdr(Some(each), &mut out as *mut Vec<u64> as *mut libc::c_void) };
    out
}

/// Subsecond's address reference point: it looks `main` up with `dlsym` in
/// the running process and in each patch (`cargo gpui` gives each patch one).
#[cfg(debug_assertions)]
#[unsafe(no_mangle)]
pub extern "C" fn main() {}

/// On an `impl Render for View` block, runs `render` through [`call`], so
/// patches reach views GPUI created before the patch:
///
/// ```ignore
/// #[gpui_hot::hot]
/// impl Render for Counter {
///     fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
///         div().child(format!("{}", self.count))
///     }
/// }
/// ```
pub use gpui_hot_macros::hot;
