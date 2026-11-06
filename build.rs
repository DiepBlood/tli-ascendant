//! Builds the icon into Ascendant.exe, for Explorer, the Start menu and Apps & features, and keeps the exes'
//! imported DLLs to System32.

fn main() {
    println!("cargo:rerun-if-changed=assets/ascendant.rc");
    println!("cargo:rerun-if-changed=assets/ascendant.ico");
    embed_resource::compile_for("assets/ascendant.rc", ["Ascendant"], embed_resource::NONE)
        .manifest_required()
        .expect("assets/ascendant.rc: the icon resource did not compile");
    // The import table is resolved before main, so update::restrict_dll_search cannot cover it, and several of the
    // imported DLLs (dwmapi, uxtheme, dxgi, d3d11, bcryptprimitives, and bcrypt, which update::BCRYPT_IMPORT adds so
    // that mimalloc's load of it by name finds this copy) are not KnownDLLs: by default the loader looks for them in
    // the exe's folder first, which for a player is often Downloads. 0x800 is
    // LOAD_LIBRARY_SEARCH_SYSTEM32: the linker writes it into the load config's DependentLoadFlags, and the loader
    // then resolves the exe's own imports from System32 only. The imports of those DLLs follow each DLL's own flags,
    // not these (see update::restrict_dll_search for what was measured). MSVC's link.exe only; the GNU linker has no
    // such switch.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
    }
}
