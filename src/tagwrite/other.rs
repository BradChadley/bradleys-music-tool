//! The kinds of file Health can meet besides MP3, FLAC and M4A, through lofty's own type for each: Ogg
//! Vorbis, Opus and Speex (a comment block like a FLAC's), and WAV, AIFF, WavPack, Monkey's Audio and
//! Musepack (their ID3, RIFF INFO, AIFF text and APE tags). lofty's general view of a tag has no place for
//! entries it has no name for, so a tag is changed through `split_tag` / `merge_tag`: the general view is
//! edited, and every entry it has no name for is put back as it was.
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::iff::aiff::AiffFile;
use lofty::iff::wav::WavFile;
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::{OpusFile, SpeexFile, VorbisFile};
use lofty::tag::{MergeTag, SplitTag, Tag};
use std::path::Path;

pub(crate) enum OggFile {
    Vorbis(VorbisFile),
    Opus(OpusFile),
    Speex(SpeexFile),
}

impl OggFile {
    pub fn open(path: &Path, ext: &str, opts: ParseOptions) -> Result<Option<OggFile>, String> {
        let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
        Ok(Some(match ext {
            "opus" => OggFile::Opus(OpusFile::read_from(&mut f, opts).map_err(|e| e.to_string())?),
            "spx" => OggFile::Speex(SpeexFile::read_from(&mut f, opts).map_err(|e| e.to_string())?),
            "ogg" | "oga" => OggFile::Vorbis(VorbisFile::read_from(&mut f, opts).map_err(|e| e.to_string())?),
            _ => return Ok(None),
        }))
    }
    pub fn comments(&self) -> &VorbisComments {
        match self {
            OggFile::Vorbis(f) => f.vorbis_comments(),
            OggFile::Opus(f) => f.vorbis_comments(),
            OggFile::Speex(f) => f.vorbis_comments(),
        }
    }
    pub fn comments_mut(&mut self) -> &mut VorbisComments {
        match self {
            OggFile::Vorbis(f) => f.vorbis_comments_mut(),
            OggFile::Opus(f) => f.vorbis_comments_mut(),
            OggFile::Speex(f) => f.vorbis_comments_mut(),
        }
    }
    pub fn save(&self, file: &mut std::fs::File) -> Result<(), String> {
        let o = WriteOptions::new();
        match self {
            OggFile::Vorbis(f) => f.save_to(file, o),
            OggFile::Opus(f) => f.save_to(file, o),
            OggFile::Speex(f) => f.save_to(file, o),
        }
        .map_err(|e| e.to_string())
    }
}

pub(crate) enum OtherFile {
    Wav(WavFile),
    Aiff(AiffFile),
    WavPack(lofty::wavpack::WavPackFile),
    Ape(lofty::ape::ApeFile),
    Mpc(lofty::musepack::MpcFile),
}

/// Runs `$body` for each tag kind a file has: `$t` is the concrete tag taken out of the file, which the
/// body gives back.
macro_rules! each {
    ($file:expr, $f:expr, [$(($remove:ident, $set:ident)),*]) => {{
        $(
            if let Some(t) = $file.$remove() {
                let (rest, mut g) = t.split_tag();
                $f(&mut g);
                $file.$set(rest.merge_tag(g));
            }
        )*
    }};
}
macro_rules! views {
    ($file:expr, [$($get:ident),*]) => {{
        let mut v: Vec<Tag> = vec![];
        $(
            if let Some(t) = $file.$get() {
                v.push(t.clone().into());
            }
        )*
        v
    }};
}

impl OtherFile {
    pub fn open(path: &Path, ext: &str, opts: ParseOptions) -> Result<Option<OtherFile>, String> {
        let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
        let e = |x: lofty::error::FileParseError| x.to_string();
        Ok(Some(match ext {
            "wav" => OtherFile::Wav(WavFile::read_from(&mut f, opts).map_err(e)?),
            "aif" | "aiff" | "aifc" => OtherFile::Aiff(AiffFile::read_from(&mut f, opts).map_err(e)?),
            "wv" => OtherFile::WavPack(lofty::wavpack::WavPackFile::read_from(&mut f, opts).map_err(e)?),
            "ape" => OtherFile::Ape(lofty::ape::ApeFile::read_from(&mut f, opts).map_err(e)?),
            "mpc" => OtherFile::Mpc(lofty::musepack::MpcFile::read_from(&mut f, opts).map_err(e)?),
            _ => return Ok(None),
        }))
    }

    /// Leaves a WAV's or AIFF's ID3 chunk out (it is read and written by id3 instead): saving then leaves
    /// that chunk as it is.
    pub fn strip_id3(&mut self) {
        match self {
            OtherFile::Wav(f) => drop(f.remove_id3v2()),
            OtherFile::Aiff(f) => drop(f.remove_id3v2()),
            _ => {}
        }
    }

    /// Every tag the file has, in lofty's general view (for reading).
    pub fn views(&self) -> Vec<Tag> {
        match self {
            OtherFile::Wav(f) => views!(f, [id3v2, riff_info]),
            OtherFile::Aiff(f) => views!(f, [id3v2, text_chunks]),
            OtherFile::WavPack(f) => views!(f, [ape, id3v1]),
            OtherFile::Ape(f) => views!(f, [ape, id3v2, id3v1]),
            OtherFile::Mpc(f) => views!(f, [ape, id3v2, id3v1]),
        }
    }

    /// Changes every tag the file has the same way (as TagLib did), making the file's main kind of tag
    /// first when it has none.
    pub fn edit(&mut self, mut f: impl FnMut(&mut Tag)) {
        if self.views().is_empty() {
            match self {
                OtherFile::Wav(x) => drop(x.set_id3v2(Default::default())),
                OtherFile::Aiff(x) => drop(x.set_id3v2(Default::default())),
                OtherFile::WavPack(x) => drop(x.set_ape(Default::default())),
                OtherFile::Ape(x) => drop(x.set_ape(Default::default())),
                OtherFile::Mpc(x) => drop(x.set_ape(Default::default())),
            }
        }
        match self {
            OtherFile::Wav(x) => each!(x, f, [(remove_id3v2, set_id3v2), (remove_riff_info, set_riff_info)]),
            OtherFile::Aiff(x) => each!(x, f, [(remove_id3v2, set_id3v2), (remove_text_chunks, set_text_chunks)]),
            OtherFile::WavPack(x) => each!(x, f, [(remove_ape, set_ape), (remove_id3v1, set_id3v1)]),
            OtherFile::Ape(x) => each!(x, f, [(remove_ape, set_ape), (remove_id3v2, set_id3v2), (remove_id3v1, set_id3v1)]),
            OtherFile::Mpc(x) => each!(x, f, [(remove_ape, set_ape), (remove_id3v2, set_id3v2), (remove_id3v1, set_id3v1)]),
        }
    }

    pub fn save(&self, file: &mut std::fs::File) -> Result<(), String> {
        let o = WriteOptions::new();
        match self {
            OtherFile::Wav(f) => f.save_to(file, o),
            OtherFile::Aiff(f) => f.save_to(file, o),
            OtherFile::WavPack(f) => f.save_to(file, o),
            OtherFile::Ape(f) => f.save_to(file, o),
            OtherFile::Mpc(f) => f.save_to(file, o),
        }
        .map_err(|e| e.to_string())
    }
}
