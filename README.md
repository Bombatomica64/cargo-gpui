# cargo gpui

The dev loop for [GPUI](https://www.gpui.rs) apps on Android
([gpui-mobile](https://github.com/longbridge/gpui-mobile)). Debug arm64 builds
only. Built in the [GPUI Mobile Lab](https://github.com/Bombatomica64/Gpui-android);
its [hot-patching plan](https://github.com/Bombatomica64/Gpui-android/blob/main/docs/hot-patching-plan.md)
has the measurements and design notes.

| Crate | What |
|---|---|
| [`cargo-gpui`](cargo-gpui) | The `cargo gpui` CLI (host), with [`unpatch`](cargo-gpui/unpatch), a small arm64 binary it builds for the phone |
| [`gpui-hot`](gpui-hot) | App side: loads patches, reports to the CLI |
| [`gpui-hot-macros`](gpui-hot-macros) | `#[gpui_hot::hot]` |

- **Hot patching (A1)** with [Subsecond](https://docs.rs/subsecond): on save,
  recompile the app crate, link a patch library against the running app and
  push it; the app loads it between frames and keeps its state. From the
  second patch on, it sends a zstd `--patch-from` delta against the last
  patch (~0.1 MB instead of ~2.3 MB compressed), which `unpatch/`, a small
  arm64 binary the tool builds and pushes, applies on the phone; the same adb
  call waits for the app to apply it.
- **Reload (A2)**: push a new build of the library and restart the app where
  it was, with no Gradle and no reinstall. After the first push it sends a
  delta against the last library (~1.5 MB instead of ~21 MB), and applies
  it, copies the library into the app and restarts it in one adb call.

```sh
cargo install --git https://github.com/Bombatomica64/cargo-gpui cargo-gpui
cargo gpui apk && cargo gpui install   # once: a debug APK, installed
cargo gpui dev                         # reload, then patch on every save
```

| Command | |
|---|---|
| `dev [cargo args]` | Reload, then patch on every save. Reloads when a patch fails to link or to apply, and on `r` + Enter (e.g. after changing a struct's layout). Compile errors wait for the next save. |
| `reload [cargo args]` | Build, push the library, restart the app with the intent extras it remembered. |
| `watch` | Patch the running app on every save (`CARGO_GPUI_OFFLINE=<hex aslr>` builds patches without a phone). |
| `build [cargo args]` | Only the library: the base for patches, and a stripped copy in `android/app/src/main/jniLibs`. Prints where the time went. |
| `apk [cargo args]` | `build`, then Gradle: `dist/<name>-<version>-debug.apk`. |
| `install [apk]` | Install it (pushed in parts, then `pm install`), drop any reloaded library and stage the APK's as the base of the first delta. |

`CARGO_GPUI_ADB` replaces `adb`, e.g. `CARGO_GPUI_ADB="phone adb"` for a phone
shared behind a lock.

## The app side

The app depends on [`gpui-hot`](gpui-hot) and names its package
and activity:

```toml
[package.metadata.gpui]
android-package = "com.example.app"
android-activity = ".MainActivity"

[dependencies]
gpui-hot = { git = "https://github.com/Bombatomica64/cargo-gpui", rev = "…" }
```

```rust
gpui_hot::init(cx); // at startup

#[gpui_hot::hot] // on each view of the app crate
impl Render for Counter {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().child(format!("{}", self.count))
    }
}

gpui_hot::remember("screen", Some("Buttons")); // a reload passes `--es screen Buttons`
```

The debug Activity must load `files/dev/lib<crate>.so` when it exists. For a
host-driven Activity (`gpui_mobile::android::host`), gpui-mobile's
`GpuiLibraryLoader.load(this, "<crate>")` does it (in the example project's
Java helpers). `gpui-hot` pins `gpui-pre =0.3.8`, the version gpui-mobile and
GPUI Kit use.

Limits (details in the plan, A1): statics of the app crate restart from their
initial values in each patch, so keep process state in a dependency crate or
in GPUI entities and globals; thread-locals reset; listeners registered once
(`cx.subscribe`, `cx.observe`) keep running old code; struct layout changes
need a reload.

## Credits

This tool is built on the work of the [Dioxus](https://github.com/DioxusLabs/dioxus)
project (Jonathan Kelley and the Dioxus contributors):

- The app side (`gpui-hot`) is built on Dioxus's `subsecond` crate,
  used as a dependency.
- The patch builder (`cargo-gpui/src/library.rs`, `cargo-gpui/src/patch.rs`) follows the Dioxus CLI's hot patching (`dx serve
  --hotpatch`): recording rustc and linker arguments in a "fat" build, then
  "thin" links of the app crate against the running binary.
- `cargo-gpui/src/stub.rs` is adapted from the Dioxus CLI's
  `packages/cli/src/build/patch.rs` (`HotpatchModuleCache`,
  `create_undefined_symbol_stub`, `create_native_jump_table`), reduced to
  ELF/aarch64. The thin-link arguments in `cargo-gpui/src/patch.rs` (`thin_link_args`)
  come from `packages/cli/src/build/link.rs`. Both are taken from
  DioxusLabs/dioxus at commit `f951996` (2026-10-03).

The Dioxus CLI is licensed under MIT OR Apache-2.0. Its MIT license text is in
[LICENSE-MIT-dioxus](LICENSE-MIT-dioxus).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option. The parts adapted from the Dioxus
CLI keep their notice, above.
