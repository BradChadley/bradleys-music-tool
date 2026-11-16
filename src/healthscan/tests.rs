//! The check run on made-up libraries in a scratch folder: never the real library or the real report. They need ffmpeg and ffprobe (the app does too).
use super::*;
use std::os::windows::fs::OpenOptionsExt;

struct Lab {
    base: PathBuf,
    lib: PathBuf,
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

impl Drop for Lab {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn lab(name: &str) -> Lab {
    let base = std::env::temp_dir().join(format!("music-recs-health-{name}-{}-{:x}", std::process::id(), crate::importer::scan::rand_id()));
    let lib = base.join("lib");
    std::fs::create_dir_all(&lib).unwrap();
    Lab { base, lib, ffmpeg: tools::find_tool("ffmpeg").unwrap(), ffprobe: tools::find_tool("ffprobe").unwrap() }
}

impl Lab {
    fn options(&self, deep: bool) -> Options {
        Options {
            library: self.lib.clone(),
            deep,
            skip_folders: "Compilations, Various Artists, Dubstep".into(),
            alias_file: self.base.join("aliases.txt").to_string_lossy().to_string(),
            report: self.base.join("report.json"),
            cache_dir: self.base.join("cache"),
            quality_cache: self.base.join("cache").join("quality.txt"),
            seed_tags: None,
            tracklists: self.base.join("tracklists.json"),
            lookups: self.base.join("cache").join("lookups"),
            lyrics_dir: self.base.join("cache").join("lyrics"),
            online: false,
            only: vec![],
        }
    }

    /// A tagged track of `secs` seconds. `tags` are "name=value"; an empty value leaves the tag out.
    fn tone(&self, rel: &str, secs: u32, kbps: u32, tags: &[&str]) -> PathBuf {
        let out = self.lib.join(rel);
        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        let mut c = tools::command(&self.ffmpeg, false);
        c.args(["-hide_banner", "-v", "error", "-y", "-f", "lavfi", "-i", &format!("sine=frequency=440:duration={secs}")]);
        if out.extension().is_some_and(|e| e == "flac") {
            c.args(["-c:a", "flac"]);
        } else {
            c.args(["-c:a", "libmp3lame", "-b:a", &format!("{kbps}k")]);
        }
        for t in tags {
            c.args(["-metadata", t]);
        }
        c.arg(&out).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        assert!(tools::run_limited(&mut c, 60), "ffmpeg could not make {}", out.display());
        out
    }

    /// An album track with the usual tags: "A\Album (2020)\01 One.mp3" gets artist A, album "Album", track 1, title "One".
    fn track(&self, rel: &str) -> PathBuf {
        let parts: Vec<&str> = rel.split('\\').collect();
        let (artist, album) = (parts[parts.len() - 3.min(parts.len())], strip_year(parts[parts.len() - 2]));
        let stem = crate::importer::util::file_stem(parts[parts.len() - 1]);
        let (no, title) = stem.split_once(' ').unwrap_or((&stem, ""));
        self.tone(rel, 2, 320, &[&format!("artist={artist}"), &format!("album={album}"), &format!("title={title}"), &format!("track={}", no.trim_start_matches('0')), "date=2020", "genre=Rock"])
    }

    /// A picture file that says it is this big (only its header is ever read).
    fn cover(&self, rel: &str, side: u32) {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(side.to_be_bytes());
        png.extend(side.to_be_bytes());
        png.extend([8, 2, 0, 0, 0]);
        let p = self.lib.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, png).unwrap();
    }

    fn check(&self, deep: bool) -> Vec<Found> {
        let p = Progress::default();
        let report = run(&self.options(deep), &p, &self.ffmpeg, &self.ffprobe).unwrap().expect("a report");
        assert!(p.wrote.load(Ordering::Relaxed));
        // what was written is what the page will read
        let saved: Value = serde_json::from_slice(&std::fs::read(self.base.join("report.json")).unwrap()).unwrap();
        assert_eq!(saved["Issues"], report["Issues"]);
        report["Issues"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| Found {
                s: i["S"].as_u64().unwrap() as u8,
                c: text(&i["C"]),
                a: text(&i["A"]),
                l: text(&i["L"]),
                p: text(&i["P"]),
                d: text(&i["D"]),
                k: text(&i["K"]),
                f: i["F"].as_array().unwrap().iter().map(text).collect(),
                x: i["X"].as_object().unwrap().iter().map(|(k, v)| (k.clone(), text(v))).collect(),
            })
            .collect()
    }
}

fn of<'a>(found: &'a [Found], cat: &str, album: &str) -> Vec<&'a Found> {
    found.iter().filter(|f| f.c == cat && f.l == album).collect()
}

fn one<'a>(found: &'a [Found], cat: &str, album: &str) -> &'a Found {
    let hits = of(found, cat, album);
    assert_eq!(hits.len(), 1, "{cat} for {album}: {:#?}", found.iter().filter(|f| f.l == album).map(|f| (&f.c, &f.d)).collect::<Vec<_>>());
    hits[0]
}

#[test]
fn a_made_up_library_gets_the_findings_it_should_and_no_others() {
    let l = lab("rules");
    // a clean album with its cover
    l.track(r"A\Good (2020)\01 One.mp3");
    l.track(r"A\Good (2020)\02 Two.mp3");
    l.cover(r"A\Good (2020)\cover.png", 600);
    // a gap in the numbers, a small cover
    for t in ["01 One", "02 Two", "04 Four"] {
        l.track(&format!(r"A\Gap (2019)\{t}.mp3"));
    }
    l.cover(r"A\Gap (2019)\cover.png", 300);
    // the same song twice; and two untitled tracks with one number, which are not
    l.track(r"A\Twice (2018)\01 One.mp3");
    l.tone(r"A\Twice (2018)\01 One (copy).mp3", 2, 320, &["artist=A", "album=Twice", "title=One", "track=1", "date=2020", "genre=Rock"]);
    l.tone(r"A\Twice (2018)\02 a.mp3", 2, 320, &["artist=A", "album=Twice", "track=2", "date=2020", "genre=Rock"]);
    l.tone(r"A\Twice (2018)\02 b.mp3", 2, 320, &["artist=A", "album=Twice", "track=2", "date=2020", "genre=Rock"]);
    // two records that share a name, and one record kept twice
    l.track(r"A\Rocket (1994)\01 One.mp3");
    l.track(r"A\Rocket (2001)\01 One.mp3");
    l.track(r"A\Twin\01 One.mp3");
    l.track(r"A\Twin (Deluxe Edition)\01 One.mp3");
    // a shelf, a loose track, an empty folder, import folders old and new, a temp file, a file that is not audio
    l.track(r"A\EPs and Singles\[1994] Philyra\01 One.mp3");
    l.track(r"A\EPs and Singles\[1995] Songs\01 One.mp3");
    l.tone(r"A\loose.mp3", 2, 320, &["artist=A", "title=Loose", "date=2020", "genre=Rock"]);
    std::fs::create_dir_all(l.lib.join(r"A\Empty")).unwrap();
    for (name, age) in [("Old.importing", 3600), ("New.importing", 0)] {
        let dir = l.lib.join("A").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("01.mp3"), b"half").unwrap();
        let then = SystemTime::now() - std::time::Duration::from_secs(age);
        std::fs::File::options().write(true).open(dir.join("01.mp3")).unwrap().set_modified(then).unwrap();
        // a folder's own time moves when a file is added, so it is set after
        let d = std::fs::File::options().write(true).custom_flags(0x0200_0000).open(&dir).unwrap();
        d.set_modified(then).unwrap();
    }
    std::fs::write(l.lib.join(r"A\Good (2020)\03 Three.mp3.part"), b"x").unwrap();
    std::fs::write(l.lib.join(r"A\Gap (2019)\05 Broken.mp3"), b"this is not audio at all, only words in a file").unwrap();
    // a genre folder holds artists; a folder of compilations holds albums by anyone
    l.tone(r"Dubstep\Skream\Skream! (2006)\01 One.mp3", 2, 320, &["artist=Skream", "album=Skream!", "title=One", "track=1", "date=2006", "genre=Dubstep"]);
    l.tone(r"Various Artists\Hits (1999)\01 One.mp3", 2, 320, &["artist=Somebody", "album=Hits", "title=One", "track=1", "date=1999", "genre=Pop"]);
    l.tone(r"Various Artists\Hits (1999)\03 Three.mp3", 2, 320, &["artist=Other", "album=Hits", "title=Three", "track=3", "date=1999", "genre=Pop"]);
    // filed under the wrong artist, junk in a tag, a low bitrate, mixed album names
    l.tone(r"Zed\Wrong (2020)\01 One.mp3", 2, 96, &["artist=Somebody Else", "album=Wrong", "title=One", "track=1", "date=2020", "genre=Rock", "comment=Downloaded from www.example.com"]);
    l.tone(r"Zed\Wrong (2020)\02 Two.mp3", 2, 96, &["artist=Somebody Else", "album=Wrong Again", "title=Two", "track=2", "date=2020", "genre=Rock"]);
    l.tone("stray.mp3", 2, 320, &["artist=Nobody", "title=Stray"]);

    let found = l.check(false);
    // (no track of this library has lyrics, so every album also gets "No lyrics": that finding and the
    // other two about lyrics have a test of their own, `lyrics_are_found_missing_hidden_or_carrying_junk`)
    let cats = |album: &str| {
        let mut c: Vec<&str> = found.iter().filter(|f| f.l == album && f.c != "No lyrics").map(|f| f.c.as_str()).collect();
        c.sort();
        c
    };
    let lib = |rel: &str| l.lib.join(rel).to_string_lossy().to_string();

    // the clean album: only that its cover is a file, not embedded
    assert_eq!(cats("Good (2020)"), vec!["Cover on some tracks only", "Junk file"]);
    let junk = one(&found, "Junk file", "Good (2020)");
    assert_eq!((junk.k.as_str(), junk.f.clone(), junk.s), ("remove", vec![lib(r"A\Good (2020)\03 Three.mp3.part")], 3));

    let gap = one(&found, "Missing tracks", "Gap (2019)");
    assert_eq!((gap.d.as_str(), gap.k.as_str(), gap.x["search"].as_str(), gap.x["missing"].as_str()), ("Missing track 3 of 4", "redownload", "A Gap", "1:3"));
    assert_eq!(one(&found, "Small cover art", "Gap (2019)").d, "Best cover is only 300x300 (500+ looks sharp on most screens)");
    let broken = one(&found, "Unreadable file", "Gap (2019)");
    assert_eq!((broken.p.clone(), broken.s, broken.a.as_str()), (lib(r"A\Gap (2019)\05 Broken.mp3"), 1, "A"));
    assert!(broken.d.starts_with("'05 Broken.mp3': "), "{}", broken.d);

    let twice = one(&found, "Duplicate tracks", "Twice (2018)");
    assert_eq!(twice.d, "Same song twice: track 1 (01 One (copy).mp3, 01 One.mp3)");
    assert_eq!(twice.x["groups"], format!("{}|{}", lib(r"A\Twice (2018)\01 One (copy).mp3"), lib(r"A\Twice (2018)\01 One.mp3")));
    assert_eq!(one(&found, "Missing tags", "Twice (2018)").d, "Of 4 tracks: 2 without a title");

    // two years are two records; an edition is the record again
    let same: Vec<&Found> = found.iter().filter(|f| f.c == "Same album twice?").collect();
    assert_eq!(same.len(), 1, "{same:#?}");
    assert_eq!((same[0].p.clone(), same[0].x["folders"].clone()), (lib("A"), format!("{}|{}", lib(r"A\Twin"), lib(r"A\Twin (Deluxe Edition)"))));
    assert!(same[0].d.starts_with("'Twin' (1 tracks) / 'Twin (Deluxe Edition)' (1 tracks)"));

    // the shelf's folders are albums; nothing calls the shelf one
    assert!(found.iter().all(|f| f.l != "EPs and Singles"), "{:#?}", of(&found, "Loose tracks", "EPs and Singles"));
    assert!(!cats("[1994] Philyra").contains(&"Missing tracks"));
    let loose: Vec<&Found> = found.iter().filter(|f| f.c == "Loose tracks").collect();
    assert_eq!(loose.iter().map(|f| (f.d.as_str(), f.p.clone())).collect::<Vec<_>>(), vec![("stray.mp3 isn't in an artist folder", l.lib.to_string_lossy().to_string()), ("loose.mp3 isn't in an album folder", lib("A"))]);
    assert_eq!(one(&found, "Empty folder", "Empty").f, vec![lib(r"A\Empty")]);
    // an import folder is only called abandoned once it has been quiet for a while
    let left: Vec<&Found> = found.iter().filter(|f| f.c == "Leftover import folder").collect();
    assert_eq!(left.iter().map(|f| f.l.as_str()).collect::<Vec<_>>(), vec!["Old.importing"]);

    // the artist inside the genre folder is the artist; the compilation is not held to one artist or a full run of numbers
    assert!(found.iter().any(|f| f.a == "Skream" && f.l == "Skream! (2006)"));
    assert!(found.iter().all(|f| f.c != "Tagged under another artist" || f.l == "Wrong (2020)"), "{:#?}", found.iter().filter(|f| f.c == "Tagged under another artist").collect::<Vec<_>>());
    assert!(!cats("Hits (1999)").contains(&"Missing tracks"));

    let wrong = one(&found, "Tagged under another artist", "Wrong (2020)");
    assert_eq!((wrong.d.as_str(), wrong.k.as_str()), ("Tagged as 'Somebody Else' but filed under 'Zed'", "artist-mismatch"));
    let ad = one(&found, "Junk in tags", "Wrong (2020)");
    assert_eq!(ad.d, "1 of 2 tracks: comment 'Downloaded from www.example.com' (release-group or download-site tag). Players show this as clutter; safe to remove");
    assert_eq!(ad.x["junk"], Value::Array(vec![json!([lib(r"Zed\Wrong (2020)\01 One.mp3"), "comment", "Downloaded from www.example.com"])]).to_string());
    assert_eq!(one(&found, "Low bitrate", "Wrong (2020)").s, 2);
    assert_eq!(one(&found, "Mixed-up album name", "Wrong (2020)").x["default"], "Wrong");
    assert!(of(&found, "Mixed-up tags", "Wrong (2020)").is_empty());

    // every finding has its own key, and the report says what was counted
    let mut keys: Vec<String> = found.iter().map(|f| format!("{}|{}", f.c, f.p.to_lowercase())).collect();
    keys.sort();
    let all = keys.len();
    keys.dedup();
    assert_eq!(keys.len(), all, "two findings share a key");
    let report: Value = serde_json::from_slice(&std::fs::read(l.base.join("report.json")).unwrap()).unwrap();
    assert_eq!((report["Files"].as_u64(), report["Albums"].as_u64(), report["Artists"].as_u64(), report["Deep"].as_bool()), (Some(23), Some(12), Some(4), Some(false)));
    assert_eq!(report["DeepLeft"].as_u64(), Some(22));

    // a second check reads no file again: with no ffprobe to ask, it finds the same
    let again = run(&l.options(false), &Progress::default(), &l.ffmpeg, Path::new("no-such-ffprobe.exe")).unwrap().unwrap();
    assert_eq!(again["Issues"], report["Issues"]);
    // nothing in the library was touched
    assert!(l.lib.join(r"A\Good (2020)\03 Three.mp3.part").exists() && l.lib.join(r"A\Empty").exists() && l.lib.join(r"A\Old.importing").exists());
}

#[test]
fn the_deep_check_finds_a_cut_off_file_and_a_stopped_check_keeps_the_last_report() {
    let l = lab("deep");
    l.track(r"A\Good (2020)\01 One.mp3");
    l.tone(r"A\Cut (2020)\01 One.flac", 12, 0, &["artist=A", "album=Cut", "title=One", "track=1", "date=2020", "genre=Rock"]);
    l.tone(r"A\Cut (2020)\02 Two.flac", 12, 0, &["artist=A", "album=Cut", "title=Two", "track=2", "date=2020", "genre=Rock"]);
    // the second half of the file never arrived
    let cut = l.lib.join(r"A\Cut (2020)\02 Two.flac");
    let bytes = std::fs::read(&cut).unwrap();
    std::fs::write(&cut, &bytes[..bytes.len() / 2]).unwrap();

    // a quick check knows nothing about the audio yet
    let quick = l.check(false);
    assert!(of(&quick, "Damaged or incomplete", "Cut (2020)").is_empty());

    let found = l.check(true);
    let hurt = one(&found, "Damaged or incomplete", "Cut (2020)");
    assert_eq!((hurt.s, hurt.k.as_str(), hurt.f.clone()), (1, "redownload", vec![cut.to_string_lossy().to_string()]));
    assert!(hurt.d.starts_with("02 Two.flac (") && hurt.d.ends_with(". This one needs a new download"), "{}", hurt.d);
    assert!(of(&found, "Damaged or incomplete", "Good (2020)").is_empty());
    let report: Value = serde_json::from_slice(&std::fs::read(l.base.join("report.json")).unwrap()).unwrap();
    assert_eq!((report["Deep"].as_bool(), report["DeepLeft"].as_u64(), report["Unchecked"].as_u64()), (Some(true), Some(0), Some(0)));

    // a quick check afterwards still knows what the full one heard
    let later = l.check(false);
    assert_eq!(of(&later, "Damaged or incomplete", "Cut (2020)").len(), 1);

    // stopped while the tags are being read: no report is written, the last one stands
    let before = std::fs::read(l.base.join("report.json")).unwrap();
    let p = Progress::default();
    p.cancel.store(true, Ordering::Relaxed);
    assert!(run(&l.options(false), &p, &l.ffmpeg, &l.ffprobe).unwrap().is_none());
    assert!(!p.wrote.load(Ordering::Relaxed));
    assert_eq!(std::fs::read(l.base.join("report.json")).unwrap(), before);
}

#[test]
fn a_file_that_cannot_be_read_is_not_asked_again_for_a_week() {
    let l = lab("failed");
    l.track(r"A\Good (2020)\01 One.mp3");
    std::fs::write(l.lib.join(r"A\Good (2020)\02 Broken.mp3"), b"words, not audio").unwrap();
    let first = l.check(false);
    assert_eq!(of(&first, "Unreadable file", "Good (2020)").len(), 1);
    // the failure was written down: with no ffprobe at all the answer is the same
    let again = run(&l.options(false), &Progress::default(), &l.ffmpeg, Path::new("no-such-ffprobe.exe")).unwrap().unwrap();
    assert_eq!(again["Issues"].as_array().unwrap().iter().filter(|i| i["C"] == "Unreadable file").count(), 1);
    // the file changes: it is read afresh
    std::fs::remove_file(l.lib.join(r"A\Good (2020)\02 Broken.mp3")).unwrap();
    l.track(r"A\Good (2020)\02 Two.mp3");
    assert!(of(&l.check(false), "Unreadable file", "Good (2020)").is_empty());
}

#[test]
fn small_readers() {
    assert_eq!(strip_edition_all("Rocket (Deluxe) [FLAC]"), "Rocket");
    assert_eq!(strip_edition_all("Rocket - EP"), "Rocket");
    assert_eq!(strip_edition_all("Rocket, Disc 2"), "Rocket");
    assert_eq!(strip_edition_all("(Deluxe)"), "(Deluxe)");
    assert_eq!((lead(" 07/12"), tag_total("07/12"), lead("x"), tag_total("7")), (7, 12, 0, 0));
    assert_eq!((time(61.9), time(5.0)), ("1:01".to_string(), "0:05".to_string()));
    assert_eq!(list_of(&["a".to_string(), "b".to_string(), "c".to_string()], 2), "a; b; and 1 more");
    assert_eq!(most_common(["b", "a", "a", "", "b"].into_iter()), Some("b"));
    assert_eq!(search_words("A", "Rocket (Deluxe) (2017)", r"X:\A\Rocket"), "A Rocket");
    assert_eq!(search_words("A", "(loose tracks)", r"X:\A\Some Song.mp3"), "A Some Song");
    // a tag far too long is cut; a short one is only trimmed
    assert_eq!(cut(&"x".repeat(5000)).len(), TAG_MAX);
    assert_eq!(cut("  hi "), "hi");
    // tags saved long ago, under the first field names, read as this check's
    let theirs: Info = serde_json::from_str(r#"{"Ok":true,"Error":"","Quirk":"","Codec":"mp3","SampleRate":44100,"Bits":0,"Kbps":320,"ArtW":600,"ArtH":600,"Duration":183.5,"HasArt":true,"Tags":{"title":"One","track":"1/12"}}"#).unwrap();
    assert!(theirs.ok && theirs.has_art && theirs.kbps == 320 && theirs.art_w == 600 && theirs.tag("title") == "One" && theirs.duration == 183.5);
}

#[test]
fn what_the_official_track_list_says_is_part_of_the_report() {
    use crate::importer::tracklist::Finding;
    use std::cell::Cell;
    let l = lab("lists");
    for t in ["01 One", "02 Two", "04 Four"] {
        l.track(&format!(r"A\Gap (2019)\{t}.mp3"));
    }
    l.track(r"A\Whole (2020)\01 One.mp3");
    l.tone(r"Various Artists\Hits (1999)\01 One.mp3", 2, 320, &["artist=Somebody", "album=Hits", "title=One", "track=1"]);
    let gap = l.lib.join(r"A\Gap (2019)").to_string_lossy().to_string();
    // how many albums the report on disk says are still waiting for a track-list comparison
    let left = || serde_json::from_slice::<Value>(&std::fs::read(l.base.join("report.json")).unwrap()).unwrap()["ListsLeft"].as_u64().unwrap();
    let run_it = |judge: &dyn Fn(&Job) -> Option<(recheck::Outcome, String)>| {
        let report = run_with(&l.options(false), &Progress::default(), &l.ffmpeg, &l.ffprobe, Some(judge), None).unwrap().unwrap();
        let found: Vec<Found> = report["Issues"].as_array().unwrap().iter().filter_map(Found::from_json).collect();
        found
    };
    // the list has three songs and the copy has them all, one under the wrong title: the gap in the numbers was
    // never a missing song
    let asked = Cell::new(0);
    let fits = |j: &Job| {
        asked.set(asked.get() + 1);
        assert!(j.album != "Hits (1999)", "a compilation is not held to one track list");
        let retitle = Finding { category: "Wrong track numbers or titles", severity: 3, detail: "1 title differs from the official list".into(), changes: vec![(j.reads[0].track.file.clone(), "Uno".into(), String::new(), String::new())] };
        let fit = j.album == "Gap (2019)";
        Some((recheck::Outcome { kind: crate::albumkind::Kind::Studio, source: if fit { "Apple Music".into() } else { String::new() }, fit, all_there: fit, findings: if fit { vec![retitle] } else { vec![] }, real: vec![] }, "A Gap".to_string()))
    };
    let found = run_it(&fits);
    assert_eq!(asked.get(), 2);
    // every album has its answer: none is left waiting for a track-list comparison
    assert_eq!(left(), 0);
    // what was learned about the album is kept with the official list's lengths (none in this made-up answer),
    // which is what the second look at damaged files reads
    let kept: Value = serde_json::from_slice(&std::fs::read(l.base.join("tracklists.json")).unwrap()).unwrap();
    assert!(kept["Albums"][gap.to_lowercase()]["Real"].is_array(), "{kept}");
    assert!(of(&found, "Missing tracks", "Gap (2019)").is_empty());
    let titles = one(&found, "Wrong track numbers or titles", "Gap (2019)");
    assert_eq!((titles.k.as_str(), titles.p.as_str(), titles.a.as_str(), titles.f.len()), ("tracklist-titles", gap.as_str(), "A", 1));
    // the tracks were handed over as the check read them
    let seen = |j: &Job| {
        if j.album == "Gap (2019)" {
            assert_eq!(j.reads.iter().map(|r| (r.track.title.as_str(), r.track.no, r.album_tag.as_str())).collect::<Vec<_>>(), vec![("One", 1, "Gap"), ("Two", 2, "Gap"), ("Four", 4, "Gap"), ("Six", 6, "Gap")]);
            assert_eq!(j.reads[0].artist_tags, vec!["A".to_string()]);
        }
        None
    };
    // what was learned is kept: the next check asks nobody and says the same
    let never = |_: &Job| -> Option<(recheck::Outcome, String)> { panic!("an album that has not changed is not asked about again") };
    let again = run_it(&never);
    assert!(of(&again, "Missing tracks", "Gap (2019)").is_empty() && of(&again, "Wrong track numbers or titles", "Gap (2019)").len() == 1);
    // the album changes: it is asked about again; with no answer (the connection is down) nothing is kept,
    // and the check's own finding stands
    l.track(r"A\Gap (2019)\06 Six.mp3");
    let silent = run_it(&seen);
    assert_eq!(left(), 1, "the album that got no answer is still waiting, and the report says so");
    assert_eq!(one(&silent, "Missing tracks", "Gap (2019)").d, "Missing tracks 3, 5 of 6");
    assert!(of(&silent, "Wrong track numbers or titles", "Gap (2019)").is_empty());
    let asked_again = Cell::new(0);
    let count = |_: &Job| {
        asked_again.set(asked_again.get() + 1);
        None
    };
    run_it(&count);
    assert_eq!(asked_again.get(), 1, "only the album that changed, and it was not written down as having no list");
}

#[test]
fn damaged_files_get_a_second_look_against_the_real_song_lengths() {
    let folder = r"x:\lib\a\rocket (2020)";
    let rows = |rows: &[(&str, &str, u32, f64, f64, u32)]| Value::Array(rows.iter().map(|(file, title, no, plays, header, errors)| json!([format!(r"X:\Lib\A\Rocket (2020)\{file}"), title, "1", no.to_string(), format!("{plays:.1}"), format!("{header:.1}"), errors.to_string()])).collect()).to_string();
    let found = |c: &str, s: u8, k: &str, tracks: String| {
        let mut f = Found { s, c: c.into(), a: "A".into(), l: "Rocket (2020)".into(), p: r"X:\Lib\A\Rocket (2020)".into(), d: "as the check said".into(), k: k.into(), f: vec![r"X:\Lib\A\Rocket (2020)\x.mp3".into()], x: BTreeMap::new() };
        f.fact("tracks", tracks);
        f
    };
    let list = json!({ "List": "Apple Music", "Real": [[1, 1, "One", 200.0], [1, 2, "Two", 180.0], [1, 3, "Three", 240.0]] });

    // stops a minute early with decode errors: cut short, and it says how much of the real song plays
    let mut issues = vec![found("Damaged or incomplete", 1, "redownload", rows(&[("01 One.mp3", "One", 1, 140.0, 200.0, 4)]))];
    second_look(&mut issues, folder, &list);
    assert_eq!((issues[0].c.as_str(), issues[0].s, issues[0].k.as_str()), ("Damaged or incomplete", 1, "redownload"));
    assert_eq!(issues[0].d, "01 One.mp3: cut short, plays 2:20 of the real 3:20 (Apple Music) before breaking. This one needs a new download");

    // shorter than the real song but it decodes cleanly: possibly cut short, worth a listen, and it can be searched for
    let mut issues = vec![found("Length mismatch", 3, "vbr-header", rows(&[("02 Two.mp3", "Two", 2, 120.0, 180.0, 0)]))];
    second_look(&mut issues, folder, &list);
    assert_eq!((issues[0].c.as_str(), issues[0].s, issues[0].k.as_str()), ("Possibly cut short", 2, "redownload"));
    assert!(issues[0].d.starts_with("02 Two.mp3: possibly cut short, plays 2:00, the Apple Music version is 3:00") && issues[0].d.ends_with("Have a listen, and get a new copy if it ends abruptly"), "{}", issues[0].d);
    assert_eq!(issues[0].x.get("search").map(|s| s.as_str()), Some("A Rocket"));

    // the header claimed six minutes, the song is four and all of it plays: only the length shown is wrong
    let mut issues = vec![found("Damaged or incomplete", 1, "redownload", rows(&[("03 Three.mp3", "Three", 3, 239.0, 360.0, 2)]))];
    second_look(&mut issues, folder, &list);
    assert_eq!((issues[0].c.as_str(), issues[0].s, issues[0].k.as_str()), ("Length mismatch", 3, "vbr-header"));
    assert!(issues[0].d.contains("plays the full 3:59 (real track: 4:00, Apple Music)") && issues[0].d.ends_with("The fix adds the missing length header"), "{}", issues[0].d);
    assert_eq!(issues[0].f, vec![r"X:\Lib\A\Rocket (2020)\03 Three.mp3".to_string()]);

    // a lossless file that failed its own check stays serious even at full length
    let mut issues = vec![found("Damaged or incomplete", 1, "redownload", rows(&[("01 One.flac", "One", 1, 200.0, 200.0, 0)]))];
    second_look(&mut issues, folder, &list);
    assert_eq!((issues[0].c.as_str(), issues[0].s), ("Damaged or incomplete", 1));
    assert!(issues[0].d.contains("full length (3:20, real 3:20) but the file does not pass its own check"), "{}", issues[0].d);

    // a song that is not on the list keeps the finding as it was, and says so
    let mut issues = vec![found("Damaged or incomplete", 1, "redownload", rows(&[("09 Hidden.mp3", "Hidden", 9, 30.0, 90.0, 3)]))];
    second_look(&mut issues, folder, &list);
    assert_eq!((issues[0].c.as_str(), issues[0].s), ("Damaged or incomplete", 1));
    assert!(issues[0].d.contains("no matching track on Apple Music"), "{}", issues[0].d);

    // two findings that end up the same kind become one (each kind and folder is ignored as a pair)
    let mut issues = vec![
        found("Damaged or incomplete", 1, "redownload", rows(&[("03 Three.mp3", "Three", 3, 239.0, 360.0, 2)])),
        found("Length mismatch", 3, "vbr-header", rows(&[("01 One.mp3", "One", 1, 199.0, 400.0, 0)])),
    ];
    second_look(&mut issues, folder, &list);
    assert_eq!(issues.len(), 1);
    assert!(issues[0].c == "Length mismatch" && issues[0].d.contains("03 Three.mp3") && issues[0].d.contains("01 One.mp3"), "{}", issues[0].d);

    // no list, or an album in another folder: nothing changes
    let mut issues = vec![found("Damaged or incomplete", 1, "redownload", rows(&[("01 One.mp3", "One", 1, 140.0, 200.0, 4)]))];
    second_look(&mut issues, folder, &json!({ "List": "", "Real": [] }));
    second_look(&mut issues, r"x:\lib\a\other", &list);
    assert_eq!(issues[0].d, "as the check said");
    // and an answer saved before the lengths were kept is asked again only for an album with such a finding
    assert!(wants_lengths(&issues, r"X:\Lib\A\Rocket (2020)") && !wants_lengths(&issues, r"X:\Lib\A\Other"));
}

#[test]
fn each_loose_track_is_given_its_album_and_whether_that_album_is_here() {
    use std::cell::Cell;
    let l = lab("loosealbums");
    l.track(r"A\Rocket (2020)\01 One.mp3");
    l.track(r"A\Rocket (2020)\02 Two.mp3");
    let tags = |album: &str, title: &str| vec!["artist=A".to_string(), format!("album={album}"), format!("title={title}"), "date=2020".to_string(), "genre=Rock".to_string()];
    let loose = |name: &str, album: &str, title: &str| {
        let t = tags(album, title);
        l.tone(&format!(r"A\{name}"), 2, 320, &t.iter().map(|s| s.as_str()).collect::<Vec<_>>()).to_string_lossy().to_string()
    };
    // two loose tracks of an album that is not here, one of an album that is (and has the song), one that
    // names no album
    let (alpha, beta) = (loose("Alpha.mp3", "Comet", "Alpha"), loose("Beta.mp3", "Comet (Deluxe Edition)", "Beta"));
    let two = loose("Two.mp3", "Rocket", "Two");
    let nameless = loose("Nameless.mp3", "", "Nameless");
    let artist_dir = l.lib.join("A").to_string_lossy().to_string();
    let albums_of = |report: &Value| -> Vec<Value> {
        let i = report["Issues"].as_array().unwrap().iter().find(|i| i["C"] == "Loose tracks" && i["P"] == artist_dir.as_str()).expect("the loose tracks finding");
        assert_eq!(i["K"], "file-loose");
        serde_json::from_str(i["X"]["albums"].as_str().expect("its albums")).unwrap()
    };
    let no_list = |_: &Job| -> Option<(recheck::Outcome, String)> { None };
    // the group that holds this loose file
    let with = |albums: &[Value], file: &str| albums.iter().find(|a| a["Files"].as_array().unwrap().iter().any(|f| f == file)).cloned().expect("a group for the file");
    let asked = Cell::new(0);

    // the sites give no answer for the song: nothing is known about the nameless one, and nothing is kept
    let silent = |_: &str, _: &str| -> Option<(String, String)> {
        asked.set(asked.get() + 1);
        None
    };
    let report = run_with(&l.options(false), &Progress::default(), &l.ffmpeg, &l.ffprobe, Some(&no_list), Some(&silent)).unwrap().unwrap();
    assert_eq!(asked.get(), 1, "only the track with no album tag is looked up");
    let albums = albums_of(&report);
    assert_eq!(albums.len(), 3, "{albums:?}");
    // one group per album, an edition of it included: one control for both files
    let comet = &with(&albums, &alpha);
    assert_eq!((comet["Album"].as_str(), comet["Source"].as_str(), comet["Folder"].as_str(), comet["Search"].as_str()), (Some("Comet"), Some("tag"), Some(""), Some("A Comet")));
    assert_eq!(comet["Files"], json!([alpha, beta]));
    assert_eq!(comet["Held"], json!([]));
    // the album that is here, and holds the song
    let rocket = &with(&albums, &two);
    assert_eq!((rocket["Album"].as_str(), rocket["Folder"].as_str()), (Some("Rocket"), Some(l.lib.join(r"A\Rocket (2020)").to_string_lossy().as_ref())));
    assert_eq!((rocket["Files"].clone(), rocket["Held"].clone()), (json!([two]), json!([two])));
    let unknown = with(&albums, &nameless);
    assert_eq!((unknown["Album"].as_str(), unknown["Source"].as_str(), unknown["Files"].clone()), (Some(""), Some(""), json!([nameless])));

    // the lookup finds the song's album: it is said, with where it was found, and kept for the next check
    let finds = |artist: &str, title: &str| {
        asked.set(asked.get() + 1);
        assert_eq!((artist, title), ("A", "Nameless"));
        Some(("Tides".to_string(), "Apple Music".to_string()))
    };
    let report = run_with(&l.options(false), &Progress::default(), &l.ffmpeg, &l.ffprobe, Some(&no_list), Some(&finds)).unwrap().unwrap();
    let albums = albums_of(&report);
    let tides = with(&albums, &nameless);
    assert_eq!((tides["Album"].as_str(), tides["Source"].as_str(), tides["Search"].as_str()), (Some("Tides"), Some("Apple Music"), Some("A Tides")));
    assert_eq!(asked.get(), 2);
    let never = |_: &str, _: &str| -> Option<(String, String)> { panic!("a song that was looked up is not asked about again") };
    let report = run_with(&l.options(false), &Progress::default(), &l.ffmpeg, &l.ffprobe, Some(&no_list), Some(&never)).unwrap().unwrap();
    assert_eq!(with(&albums_of(&report), &nameless)["Album"].as_str(), Some("Tides"));
    // what is on disk is what was returned
    let saved: Value = serde_json::from_slice(&std::fs::read(l.base.join("report.json")).unwrap()).unwrap();
    assert_eq!(saved["Issues"], report["Issues"]);

    // looked for and not found is remembered as that
    let other = loose("Ghost.mp3", "", "Ghost");
    let nothing = |_: &str, title: &str| {
        assert_eq!(title, "Ghost");
        Some((String::new(), String::new()))
    };
    let report = run_with(&l.options(false), &Progress::default(), &l.ffmpeg, &l.ffprobe, Some(&no_list), Some(&nothing)).unwrap().unwrap();
    let albums = albums_of(&report);
    let ghost = with(&albums, &other);
    assert_eq!((ghost["Album"].as_str(), ghost["Source"].as_str()), (Some(""), Some("not found")));

    // the same title at another length is another recording: a nine-second "Two" lying loose is not held by
    // the album, whose "Two" is as long as the two-second loose one
    let t = tags("Rocket", "Two");
    let long = l.tone(r"A\Two (long).mp3", 9, 320, &t.iter().map(|s| s.as_str()).collect::<Vec<_>>()).to_string_lossy().to_string();
    let quiet = |_: &str, _: &str| -> Option<(String, String)> { None };
    let report = run_with(&l.options(false), &Progress::default(), &l.ffmpeg, &l.ffprobe, Some(&no_list), Some(&quiet)).unwrap().unwrap();
    let rocket = with(&albums_of(&report), &long);
    assert_eq!((rocket["Files"].clone(), rocket["Held"].clone()), (json!([long, two]), json!([two])));
}

#[test]
fn tracks_lying_in_the_library_folder_itself_say_whose_they_are_and_which_album() {
    let l = lab("strayalbums");
    l.track(r"The A\Rocket (2020)\01 One.mp3");
    let tags = |artist: &str, album: &str, title: &str| vec![format!("artist={artist}"), format!("album={album}"), format!("title={title}")];
    let stray = |name: &str, artist: &str, album: &str, title: &str| {
        let t = tags(artist, album, title);
        l.tone(name, 2, 320, &t.iter().map(|s| s.as_str()).collect::<Vec<_>>()).to_string_lossy().to_string()
    };
    // one whose artist has a folder and the album (and the song), one of a band with no folder yet
    let one = stray("One.mp3", "the a", "Rocket", "One");
    let zed = stray("Zed.mp3", "New Band", "Debut", "Zed");
    let no_list = |_: &Job| -> Option<(recheck::Outcome, String)> { None };
    let quiet = |_: &str, _: &str| -> Option<(String, String)> { None };
    let report = run_with(&l.options(false), &Progress::default(), &l.ffmpeg, &l.ffprobe, Some(&no_list), Some(&quiet)).unwrap().unwrap();
    let lib = l.lib.to_string_lossy().to_string();
    let i = report["Issues"].as_array().unwrap().iter().find(|i| i["C"] == "Loose tracks" && i["P"] == lib.as_str()).expect("the finding for the library folder itself");
    // it can be filed now, like loose tracks in an artist's folder
    assert_eq!(i["K"], "file-loose");
    let albums: Vec<Value> = serde_json::from_str(i["X"]["albums"].as_str().expect("its albums")).unwrap();
    assert_eq!(albums.len(), 2, "{albums:?}");
    let of = |file: &str| albums.iter().find(|a| a["Files"].as_array().unwrap().iter().any(|f| f == file)).cloned().unwrap();
    let rocket = of(&one);
    assert_eq!((rocket["Artist"].as_str(), rocket["Album"].as_str(), rocket["Search"].as_str()), (Some("the a"), Some("Rocket"), Some("the a Rocket")));
    assert_eq!(rocket["ArtistDir"].as_str(), Some(l.lib.join("The A").to_string_lossy().as_ref()), "the folder the artist has, however the tag spells the name");
    assert_eq!(rocket["Folder"].as_str(), Some(l.lib.join(r"The A\Rocket (2020)").to_string_lossy().as_ref()));
    assert_eq!(rocket["Held"], json!([one]), "the album is there and holds the song");
    let debut = of(&zed);
    assert_eq!((debut["Artist"].as_str(), debut["Folder"].as_str(), debut["Held"].clone()), (Some("New Band"), Some(""), json!([])));
    assert_eq!(debut["ArtistDir"].as_str(), Some(l.lib.join("New Band").to_string_lossy().as_ref()), "the folder the band would get");
}

#[test]
fn a_check_of_two_folders_at_once_changes_only_those_two() {
    let l = lab("twofolders");
    for artist in ["A", "B", "C"] {
        l.track(&format!(r"{artist}\Rocket (2020)\01 One.mp3"));
        l.track(&format!(r"{artist}\Rocket (2020)\03 Three.mp3"));
    }
    let whole = l.check(false);
    assert_eq!(whole.iter().filter(|i| i.c == "Missing tracks").count(), 3);
    // every album gets its missing track, but only A and B are looked at (in one pass)
    for artist in ["A", "B", "C"] {
        l.track(&format!(r"{artist}\Rocket (2020)\02 Two.mp3"));
    }
    let mut two = l.options(false);
    two.only = vec![l.lib.join("A"), l.lib.join("b")];
    let report = run(&two, &Progress::default(), &l.ffmpeg, &l.ffprobe).unwrap().unwrap();
    let left: Vec<&str> = report["Issues"].as_array().unwrap().iter().filter(|i| i["C"] == "Missing tracks").filter_map(|i| i["A"].as_str()).collect();
    assert_eq!(left, vec!["C"], "A's and B's findings are gone, C's stands until C is checked");
}

#[test]
fn a_check_of_one_folder_brings_only_that_folder_up_to_date() {
    let l = lab("onefolder");
    l.track(r"A\Rocket (2020)\01 One.mp3");
    l.track(r"A\Rocket (2020)\03 Three.mp3");
    l.track(r"B\Comet (2020)\01 One.mp3");
    l.track(r"B\Comet (2020)\03 Three.mp3");
    // with no report yet there is nothing to bring up to date
    let mut only_a = l.options(false);
    only_a.only = vec![l.lib.join("A")];
    assert!(run(&only_a, &Progress::default(), &l.ffmpeg, &l.ffprobe).is_err());

    let whole = l.check(false);
    assert_eq!((of(&whole, "Missing tracks", "Rocket (2020)").len(), of(&whole, "Missing tracks", "Comet (2020)").len()), (1, 1));
    let before: Value = serde_json::from_slice(&std::fs::read(l.base.join("report.json")).unwrap()).unwrap();

    // A gets its missing track, and B changes too, but only A is looked at
    l.track(r"A\Rocket (2020)\02 Two.mp3");
    l.track(r"B\Comet (2020)\02 Two.mp3");
    let report = run(&only_a, &Progress::default(), &l.ffmpeg, &l.ffprobe).unwrap().unwrap();
    let issues: Vec<Found> = report["Issues"].as_array().unwrap().iter().filter_map(Found::from_json).collect();
    assert!(of(&issues, "Missing tracks", "Rocket (2020)").is_empty(), "A's finding is gone");
    assert_eq!(of(&issues, "Missing tracks", "Comet (2020)").len(), 1, "what the report said about B stands until B is checked");
    // the rest of the report is as it was: the date of the whole check, and who wrote it
    assert_eq!((&report["When"], &report["By"]), (&before["When"], &before["By"]));
    let saved: Value = serde_json::from_slice(&std::fs::read(l.base.join("report.json")).unwrap()).unwrap();
    assert_eq!(saved["Issues"], report["Issues"]);
    // the header's facts follow: A's new track is counted (B's is not: B was not looked at), it has not
    // been listened to, and the report says when it was brought up to date
    let counts = |r: &Value| [r["Files"].as_u64(), r["Albums"].as_u64(), r["Artists"].as_u64(), r["DeepLeft"].as_u64()].map(|n| n.expect("a count"));
    assert_eq!(counts(&before), [4, 2, 2, 4]);
    assert!(before.get("Updated").is_none());
    assert_eq!(counts(&report), [5, 2, 2, 5]);
    let updated = |r: &Value| chrono::DateTime::parse_from_rfc3339(r["Updated"].as_str().expect("when it was brought up to date")).unwrap();
    assert!(updated(&report) >= chrono::DateTime::parse_from_rfc3339(before["When"].as_str().unwrap()).unwrap());
    assert_eq!(counts(&saved), [5, 2, 2, 5]);

    // a new album in A: one more album, one more track, still two artists
    l.track(r"A\Planet (2021)\01 One.mp3");
    let report = run(&only_a, &Progress::default(), &l.ffmpeg, &l.ffprobe).unwrap().unwrap();
    assert_eq!(counts(&report), [6, 3, 2, 6]);

    // a folder that is gone loses its findings and its counts
    std::fs::remove_dir_all(l.lib.join("B")).unwrap();
    let mut only_b = l.options(false);
    only_b.only = vec![l.lib.join("B")];
    let report = run(&only_b, &Progress::default(), &l.ffmpeg, &l.ffprobe).unwrap().unwrap();
    assert!(report["Issues"].as_array().unwrap().iter().all(|i| !i["P"].as_str().unwrap().contains(r"\B\")));
    assert_eq!(counts(&report), [4, 2, 1, 4]);

    // a report from before the counts were kept folder by folder: its counts stay (they cannot be made right
    // from one folder), and it still says when it was brought up to date
    let mut old: Value = serde_json::from_slice(&std::fs::read(l.base.join("report.json")).unwrap()).unwrap();
    let o = old.as_object_mut().unwrap();
    o.remove("Tops");
    o.remove("Updated");
    o.insert("Files".into(), json!(99));
    std::fs::write(l.base.join("report.json"), old.to_string()).unwrap();
    l.track(r"A\Planet (2021)\02 Two.mp3");
    let report = run(&only_a, &Progress::default(), &l.ffmpeg, &l.ffprobe).unwrap().unwrap();
    assert_eq!(report["Files"].as_u64(), Some(99));
    assert!(report.get("Tops").is_none() && report["Updated"].is_string());

    // a whole check afterwards agrees with what is on disk, and is no longer "brought up to date"
    let again = l.check(false);
    assert!(of(&again, "Missing tracks", "Rocket (2020)").is_empty() && of(&again, "Missing tracks", "Comet (2020)").is_empty());
    let whole: Value = serde_json::from_slice(&std::fs::read(l.base.join("report.json")).unwrap()).unwrap();
    assert_eq!(counts(&whole), [5, 2, 1, 5]);
    assert!(whole.get("Updated").is_none() && whole["Tops"].is_object());
}

#[test]
fn lyrics_are_found_missing_hidden_or_carrying_junk() {
    use crate::importer::lyrics::{Answer, Lookup};
    let l = lab("lyrics");
    let words = "The lamps along the water\nburn a little lower now\nand every boat that leaves the harbor\ntakes a piece of you somehow";
    // long lyrics with a site's credit at the very end, past where the check cuts the tags it keeps
    let long = format!("{}\nLyrics provided by SomeSite.com", (0..60).map(|n| format!("line number {n} of a very long song about the sea")).collect::<Vec<_>>().join("\n"));
    assert!(long.len() > TAG_MAX + 200);
    let tags = |title: &str, no: u32| vec!["artist=B".to_string(), "album=Words".to_string(), format!("title={title}"), format!("track={no}"), "date=2020".to_string(), "genre=Rock".to_string()];
    let make = |name: &str, title: &str, no: u32, lyrics: Option<&str>| {
        let mut t = tags(title, no);
        if let Some(text) = lyrics {
            // (in a FLAC this is the lyrics field itself; ffmpeg writes it into an MP3 as a text tag that
            // only has the field's name, which is what a converted album carries)
            t.push(format!("lyrics={text}"));
        }
        let refs: Vec<&str> = t.iter().map(|s| s.as_str()).collect();
        l.tone(&format!(r"B\Words (2020)\{name}"), 2, 320, &refs)
    };
    let good = make("01 Good.flac", "Good", 1, Some(words));
    let hidden = make("02 Hidden.mp3", "Hidden", 2, Some(words));
    let address = make("03 Address.flac", "Address", 3, Some("www.t.me/pmedia_music"));
    let credit = make("04 Credit.flac", "Credit", 4, Some(&long));
    let none = make("05 None.mp3", "None", 5, None);
    make("06 Quiet.mp3", "Quiet", 6, None);
    make("07 Asked.mp3", "Asked", 7, None);
    make("08 Says So.flac", "Says So", 8, Some("Instrumental"));
    // LRCLIB was asked about two of the songs before: one is an instrumental, for one it has nothing
    let kept = Lookup { dir: l.base.join("cache").join("lyrics"), online: false, timed: true, agent: ureq::Agent::new_with_defaults() };
    kept.keep("B", "Quiet", 2.0, &Answer::Instrumental);
    kept.keep("B", "Asked", 2.0, &Answer::Nothing);

    let s = |p: &PathBuf| p.to_string_lossy().to_string();
    for round in 0..2 {
        // (the second check reads every file's lyrics from what the first one kept)
        let found = l.check(false);
        let unseen = one(&found, "Lyrics players cannot see", "Words (2020)");
        assert_eq!((unseen.k.as_str(), unseen.f.clone(), unseen.s), ("lyrics-unhide", vec![s(&hidden)], 3), "round {round}");
        let junk = one(&found, "Lyrics with junk in them", "Words (2020)");
        assert_eq!((junk.k.as_str(), junk.f.clone(), junk.s), ("lyrics-junk", vec![s(&address), s(&credit)], 3));
        assert_eq!(junk.d, "2 of 8 tracks have lines in their lyrics that are not lyrics");
        let lines: Value = serde_json::from_str(&junk.x["lines"]).unwrap();
        assert_eq!(lines[0]["lines"][0], "www.t.me/pmedia_music");
        assert_eq!(lines[1]["lines"][0], "Lyrics provided by SomeSite.com");
        assert_eq!((lines[0]["only"].as_bool(), lines[1]["only"].as_bool()), (Some(true), Some(false)));
        // no lyrics: only the song nothing is known about. Not the one that is only an address (it has its
        // finding), not the instrumentals, not the one LRCLIB has nothing for
        let missing = one(&found, "No lyrics", "Words (2020)");
        assert_eq!((missing.k.as_str(), missing.f.clone(), missing.s), ("lyrics-fill", vec![s(&none)], 3));
        assert!(of(&found, "No lyrics", "Words (2020)").len() == 1 && good.exists());
    }
}
