//! Fixture tags for the import and repair tests. These use the tag crates directly, never the editor
//! being tested or an external tag writer. All callers work on made-up files in their own scratch folders.
use id3::frame::{EncapsulatedObject, Lyrics, Popularimeter, SynchronisedLyrics, SynchronisedLyricsType, TimestampFormat};
use id3::{Tag, TagLike, Version};
use std::path::Path;

pub fn edit_id3(file: &Path, edit: impl FnOnce(&mut Tag)) {
    let mut tag = match Tag::read_from_path(file) {
        Ok(tag) => tag,
        Err(e) if matches!(e.kind, id3::ErrorKind::NoTag) => Tag::with_version(Version::Id3v23),
        Err(e) => panic!("{}: {e}", file.display()),
    };
    let version = if tag.version() == Version::Id3v22 { Version::Id3v23 } else { tag.version() };
    edit(&mut tag);
    tag.write_to_path(file, version).unwrap();
}

pub fn set_lyrics(tag: &mut Tag, text: &str) {
    tag.add_frame(Lyrics { lang: "eng".into(), description: String::new(), text: text.into() });
}

pub fn personal(tag: &mut Tag) {
    tag.add_frame(Popularimeter { user: "SomePlayer".into(), rating: 196, counter: 7 });
    tag.add_frame(EncapsulatedObject {
        mime_type: "application/octet-stream".into(), filename: String::new(),
        description: "Cue points".into(), data: b"cue points".to_vec(),
    });
}

pub fn timed(tag: &mut Tag, description: &str, words: &[(u32, &str)]) {
    tag.add_frame(SynchronisedLyrics {
        lang: "eng".into(), timestamp_format: TimestampFormat::Ms,
        content_type: SynchronisedLyricsType::Lyrics, description: description.into(),
        content: words.iter().map(|(at, text)| (*at, (*text).into())).collect(),
    });
}
