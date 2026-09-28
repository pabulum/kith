//! Gives the Windows exe its icon, version details and manifest, which
//! Explorer, the taskbar and Task Manager show. Other targets need nothing.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let icon = root.join("assets/icon/kith.ico");
    let manifest = root.join("assets/windows/kith.exe.manifest");
    for input in [&icon, &manifest] {
        println!("cargo:rerun-if-changed={}", input.display());
    }

    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let numbers: Vec<&str> = version
        .split(['.', '-', '+'])
        .take(3)
        .chain(["0"])
        .collect();
    let commas = numbers.join(",");
    // Resource compilers take forward slashes, and backslashes would need escaping.
    let path = |path: &PathBuf| path.display().to_string().replace('\\', "/");
    let rc = format!(
        r#"#pragma code_page(65001)
1 ICON "{icon}"
1 24 "{manifest}"
1 VERSIONINFO
FILEVERSION {commas}
PRODUCTVERSION {commas}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "FileDescription", "Kith"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "kith"
      VALUE "LegalCopyright", "MIT License"
      VALUE "OriginalFilename", "kith.exe"
      VALUE "ProductName", "Kith"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#,
        icon = path(&icon),
        manifest = path(&manifest),
    );
    let rc_file = PathBuf::from(env::var("OUT_DIR").unwrap()).join("kith.rc");
    fs::write(&rc_file, rc).unwrap();

    // MSVC's linker writes a manifest of its own unless told not to, and two
    // clash; mingw's doesn't.
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/MANIFEST:NO");
    }
    embed_resource::compile(&rc_file, embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
