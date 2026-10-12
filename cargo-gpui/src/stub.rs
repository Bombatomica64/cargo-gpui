//! Adapted from the Dioxus CLI's packages/cli/src/build/patch.rs
//! (DioxusLabs/dioxus f951996, MIT OR Apache-2.0; see ../../README.md and
//! ../../LICENSE-MIT-dioxus), reduced to ELF/aarch64: the base library's symbol
//! table, the stub object that resolves a patch's undefined symbols to
//! addresses in the running app, and the jump table from old to new
//! function addresses.

use std::{
    collections::{HashMap, HashSet},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, ensure};
use object::{
    Architecture, BinaryFormat, Endianness, Object, ObjectSection, ObjectSymbol, SymbolKind,
    SymbolScope,
    write::{StandardSection, Symbol, SymbolSection},
};
use subsecond_types::{AddressMap, JumpTable};

/// Subsecond's reference symbol, exported by the app (the gpui-hot crate) and given to each patch by the stub.
const SENTINEL: &str = "main";

pub struct BaseSymbol {
    pub address: u64,
    pub size: u64,
    pub kind: SymbolKind,
    pub undefined: bool,
    pub weak: bool,
}

pub struct BaseSymbols {
    pub symbols: HashMap<String, BaseSymbol>,
    /// `.tdata`, the TLS initialisation image.
    tdata: Vec<u8>,
}

impl BaseSymbols {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let obj = object::File::parse(&*bytes)?;
        let symbols = obj
            .symbols()
            .filter_map(|s| {
                let name = s.name().ok()?;
                // aarch64 mapping symbols ($x, $d) are not functions.
                (!name.is_empty() && !name.starts_with('$')).then(|| {
                    (
                        name.to_string(),
                        BaseSymbol {
                            address: s.address(),
                            size: s.size(),
                            kind: s.kind(),
                            undefined: s.is_undefined(),
                            weak: s.is_weak(),
                        },
                    )
                })
            })
            .collect::<HashMap<_, _>>();
        ensure!(
            symbols.contains_key(SENTINEL),
            "no `{SENTINEL}` in {} (debug build with the gpui-hot crate?)",
            path.display()
        );
        let tdata = obj
            .section_by_name(".tdata")
            .and_then(|s| s.data().ok())
            .unwrap_or(&[])
            .to_vec();
        Ok(Self { symbols, tdata })
    }
}

/// An object defining every symbol the patch objects need but don't define,
/// pointing at its address in the running app (base address + ASLR slide).
pub fn undefined_symbol_stub(
    base: &BaseSymbols,
    objects: &[PathBuf],
    aslr_reference: u64,
) -> anyhow::Result<Vec<u8>> {
    let mut undefined = HashSet::new();
    let mut defined = HashSet::new();
    for path in objects {
        collect_symbols(path, &mut undefined, &mut defined)?;
    }

    let base_main = base.symbols[SENTINEL].address;
    ensure!(
        aslr_reference >= base_main,
        "aslr_reference {aslr_reference:#x} is below the base `main` at {base_main:#x}"
    );
    let slide = aslr_reference - base_main;

    let mut obj = object::write::Object::new(BinaryFormat::Elf, Architecture::Aarch64, Endianness::Little);
    let text = obj.section_id(StandardSection::Text);
    for name in undefined.difference(&defined) {
        let Some(sym) = base.symbols.get(name.as_str()) else {
            continue;
        };
        // Imports of the base (libc, liblog…) are left to the dynamic linker.
        if sym.undefined {
            continue;
        }
        let addr = sym.address + slide;
        match sym.kind {
            SymbolKind::Text => {
                // ldr x16, #8; br x16; .quad addr
                let mut code = vec![0x50, 0x00, 0x00, 0x58, 0x00, 0x02, 0x1F, 0xD6];
                code.extend_from_slice(&addr.to_le_bytes());
                let offset = obj.append_section_data(text, &code, 8);
                obj.add_symbol(Symbol {
                    name: name.as_bytes().to_vec(),
                    value: offset,
                    size: code.len() as u64,
                    kind: SymbolKind::Text,
                    scope: SymbolScope::Linkage,
                    weak: false,
                    section: SymbolSection::Section(text),
                    flags: object::SymbolFlags::None,
                });
            }
            // The patch gets its own copy of the thread-local, initialised
            // from the base's image: thread-locals reset on each patch.
            SymbolKind::Tls => {
                let tls = obj.section_id(StandardSection::Tls);
                let size = sym.size.max(8);
                let (start, end) = (sym.address as usize, (sym.address + size) as usize);
                let init = base
                    .tdata
                    .get(start..end)
                    .map(<[u8]>::to_vec)
                    .unwrap_or_else(|| vec![0; size as usize]);
                let id = obj.add_symbol(Symbol {
                    name: name.as_bytes().to_vec(),
                    value: 0,
                    size: 0,
                    kind: SymbolKind::Tls,
                    scope: SymbolScope::Linkage,
                    weak: false,
                    section: SymbolSection::Undefined,
                    flags: object::SymbolFlags::None,
                });
                obj.add_symbol_data(id, tls, &init, size.min(8).next_power_of_two());
            }
            kind => {
                obj.add_symbol(Symbol {
                    name: name.as_bytes().to_vec(),
                    value: addr,
                    size: 0,
                    kind: if kind == SymbolKind::Unknown { SymbolKind::Data } else { kind },
                    scope: SymbolScope::Linkage,
                    weak: sym.weak,
                    section: SymbolSection::Absolute,
                    flags: object::SymbolFlags::None,
                });
            }
        }
    }
    // Subsecond finds each patch's base with dlsym(patch, "main"); `main`
    // usually lives in the gpui-hot crate, not in the patched app crate.
    if !defined.contains(SENTINEL) {
        let ret = 0xD65F03C0u32.to_le_bytes(); // ret
        let offset = obj.append_section_data(text, &ret, 4);
        obj.add_symbol(Symbol {
            name: SENTINEL.as_bytes().to_vec(),
            value: offset,
            size: 4,
            kind: SymbolKind::Text,
            scope: SymbolScope::Dynamic,
            weak: false,
            section: SymbolSection::Section(text),
            flags: object::SymbolFlags::None,
        });
    }
    Ok(obj.write()?)
}

/// What an object defines and needs, cached by path, size and mtime: rustc
/// rewrites only the objects of changed codegen units.
struct Summary {
    undefined: Vec<String>,
    global: Vec<String>,
    data: Vec<String>,
}

fn summary(path: &Path) -> anyhow::Result<std::sync::Arc<Summary>> {
    static CACHE: std::sync::Mutex<
        Option<HashMap<(PathBuf, u64, std::time::SystemTime), std::sync::Arc<Summary>>>,
    > = std::sync::Mutex::new(None);
    let meta = std::fs::metadata(path)?;
    let key = (path.to_path_buf(), meta.len(), meta.modified()?);
    if let Some(hit) = CACHE.lock().unwrap().get_or_insert_default().get(&key) {
        return Ok(hit.clone());
    }
    let bytes = std::fs::read(path)?;
    let obj = object::File::parse(&*bytes)?;
    let mut summary = Summary { undefined: vec![], global: vec![], data: vec![] };
    for sym in obj.symbols() {
        let name = sym.name()?.to_string();
        if sym.is_undefined() {
            summary.undefined.push(name);
        } else if sym.is_global() {
            if sym.kind() != SymbolKind::Text {
                summary.data.push(name.clone());
            }
            summary.global.push(name);
        }
    }
    let summary = std::sync::Arc::new(summary);
    CACHE.lock().unwrap().get_or_insert_default().insert(key, summary.clone());
    Ok(summary)
}

fn collect_symbols(
    path: &Path,
    undefined: &mut HashSet<String>,
    defined: &mut HashSet<String>,
) -> anyhow::Result<()> {
    if path.extension().is_some_and(|e| e == "o") {
        let summary = summary(path)?;
        undefined.extend(summary.undefined.iter().cloned());
        defined.extend(summary.global.iter().cloned());
        return Ok(());
    }
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    if path.extension().is_some_and(|e| e == "rlib" || e == "a") {
        let mut archive = ar::Archive::new(std::io::Cursor::new(bytes));
        while let Some(entry) = archive.next_entry() {
            let mut entry = entry?;
            if !std::str::from_utf8(entry.header().identifier()).unwrap_or("").ends_with(".o") {
                continue;
            }
            let mut member = vec![];
            entry.read_to_end(&mut member)?;
            collect_object(&member, undefined, defined)?;
        }
        return Ok(());
    }
    collect_object(&bytes, undefined, defined)
}

fn collect_object(
    bytes: &[u8],
    undefined: &mut HashSet<String>,
    defined: &mut HashSet<String>,
) -> anyhow::Result<()> {
    for sym in object::File::parse(bytes)?.symbols() {
        if sym.is_undefined() {
            undefined.insert(sym.name()?.to_string());
        } else if sym.is_global() {
            defined.insert(sym.name()?.to_string());
        }
    }
    Ok(())
}

/// Subsecond looks the jump table up only for `HotFunction::call_it`, the
/// closures passed to `subsecond::call`.
fn is_hot_closure(name: &str) -> bool {
    name.contains("11HotFunction") && name.contains("7call_it")
}

/// The patch's hot closures by name.
pub fn hot_closures(patch: &Path) -> anyhow::Result<HashMap<String, u64>> {
    let bytes = std::fs::read(patch)?;
    let obj = object::File::parse(&*bytes)?;
    Ok(obj
        .symbols()
        .filter_map(|s| {
            let name = s.name().ok()?;
            (is_hot_closure(name) && !s.is_undefined()).then(|| (name.to_string(), s.address()))
        })
        .collect())
}

/// Old (base) → new (patch) address of every hot closure.
pub fn jump_table(patch: &Path, base: &BaseSymbols) -> anyhow::Result<JumpTable> {
    let bytes = std::fs::read(patch)?;
    let obj = object::File::parse(&*bytes)?;
    let mut map = AddressMap::default();
    let mut new_base_address = None;
    for sym in obj.symbols() {
        let Ok(name) = sym.name() else { continue };
        if sym.kind() != SymbolKind::Text || sym.is_undefined() {
            if name == SENTINEL {
                new_base_address = Some(sym.address());
            }
            continue;
        }
        if name == SENTINEL {
            new_base_address = Some(sym.address());
        }
        if !is_hot_closure(name) {
            continue;
        }
        if let Some(old) = base.symbols.get(name) {
            if !old.undefined {
                map.insert(old.address, sym.address());
            }
        }
    }
    Ok(JumpTable {
        lib: patch.to_path_buf(),
        map,
        new_base_address: new_base_address.context("no `main` in the patch")?,
        aslr_reference: base.symbols[SENTINEL].address,
        ifunc_count: 0,
    })
}
