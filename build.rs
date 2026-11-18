//! Gives each program file its own icon, name and version, so Explorer, the taskbar and Windows' own
//! messages about the file show what it is: the Music Tool's window (`bradleys-music-tool.exe`) and
//! Cadence's hidden engine (`cadence-engine.exe`, which Task Manager shows as "Cadence engine"). The
//! version is the one in `Cargo.toml`.
//!
//! The resource text is written here and only compiled by `winresource`: its own writer puts a backslash
//! before an apostrophe, and the name came out as "Bradley\'s Music Tool".
use std::path::PathBuf;

/// (program, the name Windows shows)
const PROGRAMS: [(&str, &str); 2] = [("bradleys-music-tool", "Bradley's Music Tool"), ("cadence-engine", "Cadence engine")];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=BMT_LASTFM_API_KEY");
    println!("cargo:rerun-if-env-changed=BMT_LASTFM_SHARED_SECRET");
    println!("cargo:rerun-if-changed=assets/bradleys-music-tool.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let var = |name: &str| std::env::var(name).unwrap_or_default();
    let (major, minor, patch) = (var("CARGO_PKG_VERSION_MAJOR"), var("CARGO_PKG_VERSION_MINOR"), var("CARGO_PKG_VERSION_PATCH"));
    let version = var("CARGO_PKG_VERSION");
    // (the icon's place is only read while building; it is not kept in the program file)
    let icon = PathBuf::from(var("CARGO_MANIFEST_DIR")).join("assets").join("bradleys-music-tool.ico");
    let icon = icon.to_string_lossy().replace('\\', "\\\\");
    for (program, name) in PROGRAMS {
        // the engine never shows a window, so it carries no icon of its own
        let icon_line = if program == "bradleys-music-tool" { format!("1 ICON \"{icon}\"\n") } else { String::new() };
        let rc = format!(
            r#"#pragma code_page(65001)
{icon_line}1 VERSIONINFO
FILEVERSION {major},{minor},{patch},0
PRODUCTVERSION {major},{minor},{patch},0
FILEOS 0x40004
FILETYPE 0x1
{{
    BLOCK "StringFileInfo"
    {{
        BLOCK "040904B0"
        {{
            VALUE "ProductName", "{name}"
            VALUE "FileDescription", "{name}"
            VALUE "FileVersion", "{version}"
            VALUE "ProductVersion", "{version}"
            VALUE "InternalName", "{program}"
            VALUE "OriginalFilename", "{program}.exe"
        }}
    }}
    BLOCK "VarFileInfo"
    {{
        VALUE "Translation", 0x0409, 0x04B0
    }}
}}
"#
        );
        let file = PathBuf::from(var("OUT_DIR")).join(format!("{program}.rc"));
        std::fs::write(&file, rc).expect("the resource text could not be written");
        let mut res = winresource::WindowsResource::new();
        res.set_resource_file(&file.to_string_lossy());
        if let Err(why) = res.compile_for(program) {
            panic!("the icon and version of {program}.exe could not be built in: {why}");
        }
    }
}
