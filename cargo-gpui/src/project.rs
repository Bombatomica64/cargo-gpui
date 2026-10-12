//! The app being developed: its Cargo.toml and where things go.
//!
//! ```toml
//! [package.metadata.gpui]
//! android-package = "dev.gpui.mobile.lab"
//! android-activity = ".LabActivity"   # default ".MainActivity"
//! ```

use std::{
    env,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use anyhow::Context;

/// Set for the cargo, rustc and linker processes this tool starts, which
/// run it again as a wrapper and must find the same project.
pub const ROOT_VAR: &str = "CARGO_GPUI_ROOT";

pub struct Project {
    pub root: PathBuf,
    pub name: String,
    pub version: String,
    /// The library crate (`[lib] name`), the one patches rebuild.
    pub tip: String,
    pub package: Option<String>,
    pub activity: String,
}

impl Project {
    pub fn get() -> anyhow::Result<&'static Project> {
        static PROJECT: OnceLock<Project> = OnceLock::new();
        if let Some(p) = PROJECT.get() {
            return Ok(p);
        }
        let project = Self::load()?;
        Ok(PROJECT.get_or_init(|| project))
    }

    fn load() -> anyhow::Result<Self> {
        let root = match env::var_os(ROOT_VAR) {
            Some(root) => PathBuf::from(root),
            None => find_root(&env::current_dir()?)?,
        };
        let manifest: toml::Table = std::fs::read_to_string(root.join("Cargo.toml"))?.parse()?;
        let package = manifest.get("package").context("Cargo.toml has no [package]")?;
        let str_at = |v: Option<&toml::Value>| v.and_then(|v| v.as_str()).map(String::from);
        let name = str_at(package.get("name")).context("no package name")?;
        let version = str_at(package.get("version")).unwrap_or_else(|| "0.0.0".into());
        let tip = str_at(manifest.get("lib").and_then(|l| l.get("name")))
            .unwrap_or_else(|| name.replace('-', "_"));
        let gpui = package.get("metadata").and_then(|m| m.get("gpui"));
        Ok(Self {
            package: str_at(gpui.and_then(|g| g.get("android-package"))),
            activity: str_at(gpui.and_then(|g| g.get("android-activity")))
                .unwrap_or_else(|| ".MainActivity".into()),
            root,
            name,
            version,
            tip,
        })
    }

    pub fn package(&self) -> anyhow::Result<&str> {
        self.package
            .as_deref()
            .context("set [package.metadata.gpui] android-package in Cargo.toml")
    }

    pub fn target_dir(&self) -> PathBuf {
        env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| self.root.join("target"))
    }

    /// Recorded commands, the base library and work files.
    pub fn state_dir(&self) -> PathBuf {
        self.target_dir().join("gpui-dev")
    }

    /// What Gradle packages.
    pub fn jni_lib(&self) -> PathBuf {
        self.root
            .join("android/app/src/main/jniLibs/arm64-v8a")
            .join(format!("lib{}.so", self.tip))
    }

    /// Where the app looks for patches (the `gpui-hot` crate).
    pub fn device_patch_dir(&self) -> anyhow::Result<String> {
        Ok(format!("/data/local/tmp/gpui-hot/{}", self.package()?))
    }

    /// The last library pushed by a reload, base of the next delta.
    pub fn device_stage_dir(&self) -> anyhow::Result<String> {
        Ok(format!("{DEVICE_TOOLS}/{}", self.package()?))
    }
}

/// Device-side files of this tool (shell-owned).
pub const DEVICE_TOOLS: &str = "/data/local/tmp/gpui-dev";

fn find_root(start: &Path) -> anyhow::Result<PathBuf> {
    start
        .ancestors()
        .find(|dir| {
            std::fs::read_to_string(dir.join("Cargo.toml"))
                .is_ok_and(|toml| toml.lines().any(|l| l.trim() == "[package]"))
        })
        .map(Path::to_path_buf)
        .context("no Cargo.toml with a [package] here or above")
}
