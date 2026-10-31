// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 FREYM contributors
fn main() {
    println!("cargo:rerun-if-changed=../../scripts/native-windows-resource.rs");
    println!("cargo:rerun-if-changed=../../assets/brand/icon.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").unwrap();
    let numeric = ["MAJOR", "MINOR", "PATCH"].map(|part| {
        std::env::var(format!("CARGO_PKG_VERSION_{part}"))
            .unwrap()
            .parse::<u16>()
            .unwrap()
    });
    let icon = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("../../assets/brand/icon.ico")
        .canonicalize()
        .unwrap();
    let icon = icon
        .to_str()
        .unwrap()
        .trim_start_matches(r"\\?\")
        .replace('\\', "/");
    let original = if std::env::var("CARGO_PKG_NAME").as_deref() == Ok("freym-app") {
        "freym.exe"
    } else {
        "freym-launcher.exe"
    };
    let resource = format!(
        r#"
#include <windows.h>
1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEFLAGSMASK VS_FFI_FILEFLAGSMASK
FILEFLAGS 0
FILEOS VOS_NT_WINDOWS32
FILETYPE VFT_APP
FILESUBTYPE 0
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "FREYM contributors\0"
      VALUE "FileDescription", "FREYM\0"
      VALUE "FileVersion", "{version}\0"
      VALUE "InternalName", "{original}\0"
      VALUE "OriginalFilename", "{original}\0"
      VALUE "ProductName", "FREYM\0"
      VALUE "ProductVersion", "{version}\0"
      VALUE "LegalCopyright", "Copyright (C) 2026 FREYM contributors\0"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x0409, 1200
  END
END
"#,
        major = numeric[0],
        minor = numeric[1],
        patch = numeric[2]
    );
    let path = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("freym.rc");
    std::fs::write(&path, resource).unwrap();
    embed_resource::compile(&path, embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
