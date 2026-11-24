// Embeds the app icon and version info in the exe (shown by Explorer, the Start menu,
// Installed apps and the file's Properties > Details tab).
use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
    let version = env::var("CARGO_PKG_VERSION").unwrap();
    let mut parts: Vec<u16> = version.split('.').map(|p| p.parse().unwrap_or(0)).collect();
    parts.resize(4, 0);
    let commas = parts.iter().map(u16::to_string).collect::<Vec<_>>().join(",");
    let icon = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("assets").join("icon.ico");
    let icon = icon.display().to_string().replace('\\', "\\\\");
    let rc = format!(
        r#"1 ICON "{icon}"
1 VERSIONINFO
FILEVERSION {commas}
PRODUCTVERSION {commas}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "Subscribity"
      VALUE "FileDescription", "Subscribity - subscription tracker"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "subscribity"
      VALUE "LegalCopyright", "MIT License"
      VALUE "OriginalFilename", "Subscribity.exe"
      VALUE "ProductName", "Subscribity"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    );
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("app.rc");
    fs::write(&out, rc).unwrap();
    let _ = embed_resource::compile(&out, embed_resource::NONE);
}
