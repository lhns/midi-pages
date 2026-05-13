//! Generate Rust bindings for the Windows MIDI Services WinRT loopback API
//! from the vendored `Microsoft.Windows.Devices.Midi2.winmd`. Only runs on
//! Windows builds; no-op everywhere else.

fn main() {
    println!("cargo:rerun-if-changed=vendor/wms/Microsoft.Windows.Devices.Midi2.winmd");
    println!("cargo:rerun-if-changed=build.rs");

    #[cfg(target_os = "windows")]
    generate_wms_bindings();
}

#[cfg(target_os = "windows")]
fn generate_wms_bindings() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let wms_winmd = format!("{manifest_dir}/vendor/wms/Microsoft.Windows.Devices.Midi2.winmd");
    assert!(
        std::path::Path::new(&wms_winmd).exists(),
        "missing {wms_winmd}"
    );

    // The standard Windows.winmd ships with the Windows SDK; we look it up via
    // the Windows Kits install to satisfy references to Windows.Foundation.* etc.
    let win_winmd = locate_windows_winmd().expect(
        "could not find Windows.winmd in any installed Windows Kits version. \
         Install the Windows SDK (e.g. via Visual Studio's `Desktop development with C++` \
         workload) and re-run.",
    );
    let win_winmd_str = win_winmd.to_str().expect("Windows.winmd path is not utf-8");
    println!("cargo:warning=wms-bindgen: using {win_winmd_str}");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let out_path = format!("{out_dir}/wms_bindings.rs");

    let warnings = windows_bindgen::bindgen([
        "--in",
        &wms_winmd,
        "--in",
        win_winmd_str,
        "--out",
        &out_path,
        // Pull in the whole Microsoft.Windows.Devices.Midi2 surface — the
        // Virtual Device API constructors reference declared-endpoint-info /
        // function-block / device-identity types that the narrower Loopback
        // + Virtual filter would emit as `usize`.
        "--filter",
        "Microsoft.Windows.Devices.Midi2",
        "--no-toml",
    ]);
    // bindgen emits "skipping ..." lines for types in our filter that depend on
    // out-of-scope namespaces (e.g. Windows.Data.Json). That's fine — we don't
    // use those methods. Print as warnings instead of failing.
    let warnings_s = format!("{warnings}");
    if !warnings_s.trim().is_empty() {
        for line in warnings_s.lines() {
            println!("cargo:warning=wms-bindgen: {line}");
        }
    }

    // Strip the inner-attribute block at the top of the generated file. We
    // host the same attributes on the wrapper module (`src/wms_bindings.rs`),
    // and inner attributes are only valid at the very start of a module —
    // after `include!` expansion they end up mid-module and don't compile.
    let generated = std::fs::read_to_string(&out_path).expect("read generated bindings");
    let stripped = strip_inner_allow(&generated);
    std::fs::write(&out_path, stripped).expect("write stripped bindings");
}

#[cfg(target_os = "windows")]
fn strip_inner_allow(src: &str) -> String {
    if let Some(start) = src.find("#![allow(") {
        // Find the matching closing `)]` after the start.
        let after = &src[start..];
        if let Some(end_rel) = after.find(")]") {
            let end = start + end_rel + 2;
            let mut out = String::with_capacity(src.len());
            out.push_str(&src[..start]);
            out.push_str(&src[end..]);
            return out;
        }
    }
    src.to_string()
}

#[cfg(target_os = "windows")]
fn locate_windows_winmd() -> Option<std::path::PathBuf> {
    let root = std::path::Path::new(r"C:\Program Files (x86)\Windows Kits\10\UnionMetadata");
    if !root.exists() {
        return None;
    }
    // Pick the highest version directory (numeric, e.g. "10.0.26100.0")
    // containing Windows.winmd. Skip the "Facade" alias directory whose
    // Windows.winmd lacks the type definitions bindgen needs.
    let mut versions: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.chars().next().is_some_and(|c| c.is_ascii_digit()))
                && p.join("Windows.winmd").exists()
        })
        .collect();
    versions.sort();
    versions.last().map(|p| p.join("Windows.winmd"))
}
