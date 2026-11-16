//! Omit redundant single-disc labels; retain evidence of multi-disc editions.
use crate::importer::model::Track;

const DISC_FIELDS: &[&str] = &["disc", "discnumber", "disk", "disctotal", "totaldiscs", "disccount"];

pub fn clear_single_disc(tracks: &mut [Track], catalog_multi: bool) -> usize {
    let multi = catalog_multi || tracks.iter().any(|track| {
        track.disc_no.unwrap_or(0) > 1 || DISC_FIELDS.iter().any(|key| {
            let value = track.old(key);
            !value.is_empty() && value.split('/').any(|n| n.trim().parse::<u32>().map(|n| n > 1).unwrap_or(true))
        })
    });
    if multi { return 0; }
    let mut changed = 0;
    for track in tracks {
        // The importer's passthrough formats are never retagged. Do not promise
        // a tag fix for a file that will only be copied byte-for-byte.
        if !track.lossless && !track.wrapped && !matches!(track.ext.as_str(), ".mp3" | ".m4a") { continue; }
        let fields: Vec<&str> = DISC_FIELDS.iter().copied().filter(|key| {
            !track.old(key).is_empty() || track.new.iter().any(|(k, v)| k == key && !v.is_empty())
        }).collect();
        if !fields.is_empty() {
            for field in fields {
                if field != "disc" && !track.old(field).is_empty() {
                    track.junk.push((field.into(), track.old(field).into(), "redundant single-disc tag".into()));
                }
                track.set_new(field, "");
            }
            // Both ffmpeg and TagLib receive an explicit clear, including the total.
            track.set_new("disc", "");
            track.set_new("disctotal", "");
            track.set_new("totaldiscs", "");
            changed += 1;
        }
        track.disc_no = None;
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    fn track(disc: &str) -> Track {
        let mut t = Track::default();
        t.ext = ".mp3".into();
        if !disc.is_empty() { t.old.insert("disc".into(), disc.into()); }
        t.disc_no = disc.split('/').next().and_then(|n| n.parse().ok());
        t
    }
    #[test]
    fn removes_single_disc_number_and_total() {
        let mut tracks = vec![track("1/1"), track("1"), track("")];
        assert_eq!(clear_single_disc(&mut tracks, false), 2);
        assert!(tracks.iter().all(|t| t.disc_no.is_none()));
        assert!(tracks[0].new.contains(&("disc".into(), "".into())));
        assert!(tracks[0].new.contains(&("totaldiscs".into(), "".into())));
        assert!(tracks[2].new.is_empty());
    }
    #[test]
    fn preserves_multi_disc_and_partial_editions() {
        for mut tracks in [vec![track("1/2")], vec![track("2")], vec![track("1"), track("2")], vec![track("Side A")]] {
            assert_eq!(clear_single_disc(&mut tracks, false), 0);
            assert!(tracks.iter().all(|t| t.new.is_empty()));
        }
        let mut tracks = vec![track("1")];
        assert_eq!(clear_single_disc(&mut tracks, true), 0);
        tracks[0].old.insert("totaldiscs".into(), "2".into());
        assert_eq!(clear_single_disc(&mut tracks, false), 0);
    }
}
