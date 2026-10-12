//! `cargo gpui dev` and `cargo gpui watch`: patch on save, reload when a patch
//! can't do it.

use std::{
    env,
    io::BufRead,
    path::{Path, PathBuf},
    process::ExitCode,
    sync::mpsc,
    time::{Duration, Instant, SystemTime},
};

use crate::{
    device, library,
    patch::{PatchError, Patcher},
    project::Project,
};

/// How long the app gets to apply a patch, and to start after a reload.
const APPLY_TIMEOUT: Duration = Duration::from_secs(10);
const START_TIMEOUT: Duration = Duration::from_secs(30);

enum Event {
    /// Sources changed; the instant they were seen.
    Change(Instant),
    Reload,
}

/// Polls the app's `src/` and stdin.
struct Watcher {
    src: PathBuf,
    seen: SystemTime,
    input: mpsc::Receiver<String>,
}

impl Watcher {
    fn new() -> anyhow::Result<Self> {
        let src = Project::get()?.root.join("src");
        let (tx, input) = mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Self { seen: newest_mtime(&src)?, src, input })
    }

    /// Changes from now on count; earlier ones are in the build about to run.
    fn reset(&mut self) -> anyhow::Result<()> {
        self.seen = newest_mtime(&self.src)?;
        Ok(())
    }

    /// The build touched src/lib.rs to make cargo recompile: not a save.
    /// Saves during the build still count.
    fn ignore_build_touch(&mut self) {
        self.seen = self.seen.max(library::last_touch().unwrap_or(SystemTime::UNIX_EPOCH));
    }

    fn next(&mut self) -> anyhow::Result<Event> {
        loop {
            if let Ok(line) = self.input.try_recv() {
                if line.trim() == "r" {
                    return Ok(Event::Reload);
                }
            }
            let now = newest_mtime(&self.src)?;
            if now > self.seen {
                self.seen = now;
                return Ok(Event::Change(Instant::now()));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

pub fn dev(build_args: &[String]) -> anyhow::Result<ExitCode> {
    let mut watcher = Watcher::new()?;
    'reload: loop {
        watcher.reset()?;
        let started = device::reload(build_args)
            .and_then(|()| device::wait_for_aslr(START_TIMEOUT))
            .and_then(|aslr| Patcher::new(aslr, false));
        watcher.ignore_build_touch();
        let mut patcher = match started {
            Ok(patcher) => patcher,
            Err(err) => {
                eprintln!("cargo gpui: reload failed: {err:#}");
                eprintln!("cargo gpui: fix it and save, or `r` + Enter, to try again");
                watcher.next()?;
                continue 'reload;
            }
        };
        println!("cargo gpui: ready. Save to patch; `r` + Enter reloads.");
        loop {
            let saved = match watcher.next()? {
                Event::Reload => continue 'reload,
                Event::Change(at) => at,
            };
            let failed = match patcher.patch() {
                Ok(None) => continue,
                Ok(Some(generation)) => match device::wait_applied(generation, APPLY_TIMEOUT) {
                    Ok(()) => {
                        println!(
                            "cargo gpui: patch {generation} applied {} ms after the save",
                            saved.elapsed().as_millis()
                        );
                        continue;
                    }
                    Err(err) => err,
                },
                Err(PatchError::Compile(stderr)) => {
                    eprint!("{stderr}");
                    continue;
                }
                Err(PatchError::Other(err)) => err,
            };
            eprintln!("cargo gpui: {failed:#}");
            eprintln!("cargo gpui: reloading instead");
            continue 'reload;
        }
    }
}

/// Patches the running app (installed or reloaded from the last base build)
/// until stopped. `CARGO_GPUI_OFFLINE=<hex aslr_reference>` builds patches
/// without a phone.
pub fn watch() -> anyhow::Result<ExitCode> {
    let offline = env::var("CARGO_GPUI_OFFLINE").ok();
    let aslr = match &offline {
        Some(hex) => u64::from_str_radix(hex.trim_start_matches("0x"), 16)?,
        None => device::wait_for_aslr(Duration::ZERO)?,
    };
    let mut patcher = Patcher::new(aslr, offline.is_some())?;
    let mut watcher = Watcher::new()?;
    println!("cargo gpui: watching {}", watcher.src.display());
    loop {
        let Event::Change(saved) = watcher.next()? else {
            continue;
        };
        match patcher.patch() {
            Ok(Some(generation)) if offline.is_none() => {
                match device::wait_applied(generation, APPLY_TIMEOUT) {
                    Ok(()) => println!(
                        "cargo gpui: patch {generation} applied {} ms after the save",
                        saved.elapsed().as_millis()
                    ),
                    Err(err) => eprintln!("cargo gpui: {err:#}"),
                }
            }
            Ok(_) => {}
            Err(PatchError::Compile(stderr)) => eprint!("{stderr}"),
            Err(PatchError::Other(err)) => eprintln!("cargo gpui: patch failed: {err:#}"),
        }
    }
}

fn newest_mtime(dir: &Path) -> anyhow::Result<SystemTime> {
    let mut newest = SystemTime::UNIX_EPOCH;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let mtime = if entry.file_type()?.is_dir() {
            newest_mtime(&path)?
        } else {
            entry.metadata()?.modified()?
        };
        newest = newest.max(mtime);
    }
    Ok(newest)
}
