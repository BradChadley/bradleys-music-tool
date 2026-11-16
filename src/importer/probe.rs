//! Reading a file's tags and technical details with ffprobe.
use crate::importer::tools;
use serde_json::Value;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::Path;

#[derive(Clone, Default)]
pub struct Probe {
    pub tags: HashMap<String, String>,
    pub has_art: bool,
    pub codec: String,
    pub sample_rate: u32,
    pub kbps: u32,
    pub bits: u32,
    pub duration: f64,
}

fn num(v: &Value) -> f64 {
    match v {
        Value::String(s) => s.trim().parse().unwrap_or(0.0),
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn sval(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn probe(ffprobe: &Path, file: &Path) -> Probe {
    const L: [&str; 4] = tools::LOCAL_ONLY;
    let out = tools::run_stdout(
        ffprobe,
        &[OsStr::new("-v"), OsStr::new("quiet"), OsStr::new("-print_format"), OsStr::new("json"), OsStr::new("-show_format"), OsStr::new("-show_streams"), OsStr::new(L[0]), OsStr::new(L[1]), OsStr::new(L[2]), OsStr::new(L[3]), OsStr::new("--"), file.as_os_str()],
    );
    let json: Value = serde_json::from_slice(&out).unwrap_or(Value::Null);
    let mut tags: HashMap<String, String> = HashMap::new();
    if let Some(t) = json["format"]["tags"].as_object() {
        for (k, v) in t {
            tags.insert(k.to_lowercase(), sval(v));
        }
    }
    let streams = json["streams"].as_array().cloned().unwrap_or_default();
    let audio = streams.iter().find(|s| s["codec_type"] == "audio");
    // some formats keep tags on the audio stream instead
    if let Some(t) = audio.and_then(|a| a["tags"].as_object()) {
        for (k, v) in t {
            let k = k.to_lowercase();
            if tags.get(&k).map(|x| x.is_empty()).unwrap_or(true) {
                tags.insert(k, sval(v));
            }
        }
    }
    let has_art = streams.iter().any(|s| s["codec_type"] == "video");
    let a = audio.cloned().unwrap_or(Value::Null);
    let kbps = if num(&a["bit_rate"]) > 0.0 {
        (num(&a["bit_rate"]) / 1000.0).round() as u32
    } else if num(&json["format"]["bit_rate"]) > 0.0 {
        (num(&json["format"]["bit_rate"]) / 1000.0).round() as u32
    } else {
        0
    };
    let bits = if num(&a["bits_per_raw_sample"]) > 0.0 {
        num(&a["bits_per_raw_sample"]) as u32
    } else if a["sample_fmt"].as_str().map(|s| s.starts_with("u8")).unwrap_or(false) {
        8
    } else {
        0
    };
    Probe {
        tags,
        has_art,
        codec: sval(&a["codec_name"]),
        sample_rate: num(&a["sample_rate"]) as u32,
        kbps,
        bits,
        duration: num(&json["format"]["duration"]),
    }
}
