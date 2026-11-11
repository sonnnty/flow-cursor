// Shared by the build scripts of flow-agent and flow-tray (`include!`):
// embeds the FlowCursor icon, version information and optionally an
// application manifest into the Windows executables, so Explorer, the
// taskbar, "Apps & features" and the installer show the icon. Compiled
// with mingw's windres (embed-resource) when building for Windows; does
// nothing for other targets.

#[allow(dead_code)]
fn embed_windows_resources(exe: &str, description: &str, manifest: Option<&str>) {
    use std::path::PathBuf;
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("..");
    let ico = root.join("deploy/icons/flowcursor.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    println!("cargo:rerun-if-changed=../deploy/windows/resources.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let v = |k: &str| std::env::var(k).unwrap();
    let (major, minor, patch) = (v("CARGO_PKG_VERSION_MAJOR"), v("CARGO_PKG_VERSION_MINOR"), v("CARGO_PKG_VERSION_PATCH"));
    let version = v("CARGO_PKG_VERSION");
    let path = |p: &std::path::Path| p.display().to_string().replace('\\', "/");
    let mut rc = format!("1 ICON \"{}\"\n", path(&ico));
    if let Some(m) = manifest {
        let mf = out.join(format!("{exe}.manifest"));
        std::fs::write(&mf, m).unwrap();
        // 24 = RT_MANIFEST, 1 = CREATEPROCESS_MANIFEST_RESOURCE_ID
        rc.push_str(&format!("1 24 \"{}\"\n", path(&mf)));
    }
    rc.push_str(&format!(
        r#"1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS 0x40004
FILETYPE 1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904B0"
    BEGIN
      VALUE "CompanyName", "FlowCursor"
      VALUE "FileDescription", "{description}"
      VALUE "FileVersion", "{version}"
      VALUE "InternalName", "{exe}"
      VALUE "OriginalFilename", "{exe}.exe"
      VALUE "ProductName", "FlowCursor"
      VALUE "ProductVersion", "{version}"
      VALUE "LegalCopyright", "MIT License"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    ));
    let rc_path = out.join(format!("{exe}.rc"));
    std::fs::write(&rc_path, rc).unwrap();
    embed_resource::compile(&rc_path, embed_resource::NONE).manifest_required().unwrap();
}
