//! The tag editor: a PowerShell 7 script and the TagLib library, both carried inside the app.
//!
//! Every tag the app writes (an import, a Health repair) goes through it: the app hands it copies of the
//! files and a list of changes, it rewrites the tags on a second copy, checks that the audio is byte for
//! byte the same and that the change is there, and reports back per file (`tag-editor\TagEditor.ps1` says
//! how). The two files are written to the app's data folder the first time they are needed, and are held
//! against the copies inside the app before every use: one that differs (an older version, or a file
//! something else has changed) is put right first, and is never run as it is.
use std::path::{Path, PathBuf};

#[cfg(not(test))]
const SCRIPT: &[u8] = include_bytes!("../tag-editor/TagEditor.ps1");
#[cfg(not(test))]
const TAGLIB: &[u8] = include_bytes!("../tag-editor/TagLibSharp.dll");

/// Makes sure each file in `dir` is byte for byte what the app carries, writing it again if not. An error
/// if one differs and cannot be put right: then nothing may be run from there.
fn in_place(dir: &Path, files: &[(&str, &[u8])]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| crate::util::plain_err(format!("the tag editor's folder could not be made: {e}")))?;
    for (name, bytes) in files {
        let file = dir.join(name);
        let same = || std::fs::read(&file).is_ok_and(|have| have == *bytes);
        if same() {
            continue;
        }
        // (a copy in use by another window of the app is the same file and was passed over above)
        let wrote = crate::util::write_atomic(&file, bytes);
        if !same() {
            let why = wrote.err().map(|e| crate::util::plain_err(format!(": {e}"))).unwrap_or_default();
            return Err(format!("the tag editor in {} is not the app's own and could not be put right{why}", dir.display()));
        }
    }
    Ok(())
}

/// The script to run, with TagLib beside it.
#[cfg(not(test))]
pub fn script() -> Result<PathBuf, String> {
    let dir = crate::config::app_dir().join("tag-editor");
    in_place(&dir, &[("TagEditor.ps1", SCRIPT), ("TagLibSharp.dll", TAGLIB)])?;
    Ok(dir.join("TagEditor.ps1"))
}

/// The tests run the copy in the project itself, so they write nothing into the app's data folder.
#[cfg(test)]
pub fn script() -> Result<PathBuf, String> {
    Ok(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tag-editor").join("TagEditor.ps1"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_tag_editor_is_put_right_before_every_use() {
        let dir = std::env::temp_dir().join(format!("music-recs-tageditor-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
        let files: [(&str, &[u8]); 2] = [("TagEditor.ps1", b"the app's script"), ("TagLibSharp.dll", b"the app's library")];
        let read = |name: &str| std::fs::read(dir.join(name)).unwrap();

        // the first use writes both
        in_place(&dir, &files).unwrap();
        assert_eq!((read("TagEditor.ps1"), read("TagLibSharp.dll")), (files[0].1.to_vec(), files[1].1.to_vec()));

        // something changes them while the app runs: the next use puts both back before anything is run
        std::fs::write(dir.join("TagEditor.ps1"), "Remove-Item everything").unwrap();
        std::fs::write(dir.join("TagLibSharp.dll"), "another library").unwrap();
        in_place(&dir, &files).unwrap();
        assert_eq!((read("TagEditor.ps1"), read("TagLibSharp.dll")), (files[0].1.to_vec(), files[1].1.to_vec()));

        // one that cannot be put right (here: a folder sits where the file belongs) is an error, never a go-ahead
        std::fs::remove_file(dir.join("TagEditor.ps1")).unwrap();
        std::fs::create_dir(dir.join("TagEditor.ps1")).unwrap();
        assert!(in_place(&dir, &files).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
