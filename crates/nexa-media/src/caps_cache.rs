//! `caps.json` (§3.5, §11): probed codec capabilities, cached per OS build and GPU,
//! so the real-session probe runs once per system update or graphics change rather
//! than at every start.
//!
//! An entry is used only when OS build, GPU and probe version all match, and only
//! if its contents are plausible; anything else (a missing, damaged or foreign
//! file) means probing again. The file is written through a temporary file and a
//! rename. At most [`MAX_ENTRIES`] systems are remembered, newest first.

use std::{
    io,
    path::{Path, PathBuf},
};

use nexa_wire::control::{Caps, Chroma, Codec, CodecCap};
use serde::{Deserialize, Serialize};

pub const FILE_NAME: &str = "caps.json";
/// Bumped when the probe changes what it measures.
pub const PROBE_VERSION: u32 = 1;
const MAX_ENTRIES: usize = 4;
const MAX_FILE: u64 = 256 * 1024;
const MAX_DIMENSION: u32 = 16_384;
const MAX_FPS: u32 = 1_000;

/// What a cached result is valid for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsKey {
    /// e.g. `25A354` (`kern.osversion`): drivers ship with the OS on macOS.
    pub os_build: String,
    /// e.g. `Apple M3 Pro`.
    pub gpu: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct JsonCodec {
    codec: i32,
    chroma: i32,
    max_w: u32,
    max_h: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct JsonCaps {
    encode: Vec<JsonCodec>,
    decode: Vec<JsonCodec>,
    rfi: bool,
    ltr: bool,
    max_fps: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Entry {
    #[serde(flatten)]
    key: CapsKey,
    probe_version: u32,
    /// The app version that probed, for people reading the file.
    app_version: String,
    caps: JsonCaps,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct File {
    entries: Vec<Entry>,
}

fn to_json(c: &CodecCap) -> JsonCodec {
    JsonCodec {
        codec: c.codec,
        chroma: c.chroma,
        max_w: c.max_w,
        max_h: c.max_h,
    }
}

fn from_json(c: &JsonCodec) -> Option<CodecCap> {
    Codec::try_from(c.codec).ok()?;
    Chroma::try_from(c.chroma).ok()?;
    if c.max_w == 0 || c.max_h == 0 || c.max_w > MAX_DIMENSION || c.max_h > MAX_DIMENSION {
        return None;
    }
    Some(CodecCap {
        codec: c.codec,
        chroma: c.chroma,
        max_w: c.max_w,
        max_h: c.max_h,
    })
}

fn caps_of(j: &JsonCaps) -> Option<Caps> {
    if j.max_fps == 0 || j.max_fps > MAX_FPS || j.encode.len() > 16 || j.decode.len() > 16 {
        return None;
    }
    Some(Caps {
        encode: j.encode.iter().map(from_json).collect::<Option<_>>()?,
        decode: j.decode.iter().map(from_json).collect::<Option<_>>()?,
        rfi: j.rfi,
        ltr: j.ltr,
        max_fps: j.max_fps,
    })
}

fn read(path: &Path) -> File {
    let ok = std::fs::metadata(path).is_ok_and(|m| m.len() <= MAX_FILE);
    if !ok {
        return File::default();
    }
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// The cached capabilities for `key`, if any are usable.
pub fn load(path: &Path, key: &CapsKey) -> Option<Caps> {
    read(path)
        .entries
        .iter()
        .find(|e| e.key == *key && e.probe_version == PROBE_VERSION)
        .and_then(|e| caps_of(&e.caps))
}

/// Remembers `caps` for `key`, replacing an older result for the same system.
pub fn store(path: &Path, key: &CapsKey, caps: &Caps) -> io::Result<()> {
    let mut file = read(path);
    file.entries.retain(|e| e.key != *key);
    file.entries.insert(
        0,
        Entry {
            key: key.clone(),
            probe_version: PROBE_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            caps: JsonCaps {
                encode: caps.encode.iter().map(to_json).collect(),
                decode: caps.decode.iter().map(to_json).collect(),
                rfi: caps.rfi,
                ltr: caps.ltr,
                max_fps: caps.max_fps,
            },
        },
    );
    file.entries.truncate(MAX_ENTRIES);
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
    let json = serde_json::to_vec_pretty(&file).map_err(io::Error::other)?;
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

/// The cached result for `key`, or `probe()`'s, which is then cached. A probe
/// that found no codec at all is not cached: it may be a passing failure.
pub fn cached(path: &Path, key: &CapsKey, probe: impl FnOnce() -> Caps) -> Caps {
    if let Some(caps) = load(path, key) {
        return caps;
    }
    let caps = probe();
    if !caps.encode.is_empty() || !caps.decode.is_empty() {
        let _ = store(path, key, &caps);
    }
    caps
}

/// `~/Library/Caches/NexaDesk/caps.json`.
pub fn default_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library/Caches/NexaDesk")
            .join(FILE_NAME),
    )
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn key(build: &str, gpu: &str) -> CapsKey {
        CapsKey {
            os_build: build.into(),
            gpu: gpu.into(),
        }
    }

    fn caps(fps: u32) -> Caps {
        let c = CodecCap {
            codec: Codec::Hevc as i32,
            chroma: Chroma::Yuv420 as i32,
            max_w: 3840,
            max_h: 2160,
        };
        Caps {
            encode: vec![c.clone()],
            decode: vec![c],
            rfi: false,
            ltr: true,
            max_fps: fps,
        }
    }

    #[test]
    fn probes_once_per_os_build_and_gpu() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache/caps.json");
        let probes = Cell::new(0);
        let probe = |fps| {
            probes.set(probes.get() + 1);
            caps(fps)
        };
        let a = key("25A354", "Apple M3 Pro");
        assert_eq!(cached(&path, &a, || probe(120)), caps(120));
        assert_eq!(cached(&path, &a, || probe(60)), caps(120), "from the cache");
        assert_eq!(probes.get(), 1);
        // A system update or another GPU probes again; both are kept.
        let b = key("25B78", "Apple M3 Pro");
        assert_eq!(cached(&path, &b, || probe(60)), caps(60));
        assert_eq!(probes.get(), 2);
        assert_eq!(load(&path, &a), Some(caps(120)));
        assert_eq!(load(&path, &key("25A354", "AMD Radeon Pro 5500M")), None);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"os_build\": \"25B78\""), "{text}");
    }

    #[test]
    fn damaged_or_implausible_files_mean_probing_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caps.json");
        let k = key("b", "g");
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(load(&path, &k), None);
        assert_eq!(cached(&path, &k, || caps(60)), caps(60));
        assert_eq!(load(&path, &k), Some(caps(60)));
        let tampered = std::fs::read_to_string(&path)
            .unwrap()
            .replace("3840", "99999");
        std::fs::write(&path, tampered).unwrap();
        assert_eq!(load(&path, &k), None);
        let other_version = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\"probe_version\": 1", "\"probe_version\": 0");
        std::fs::write(&path, other_version).unwrap();
        assert_eq!(load(&path, &k), None);
    }

    #[test]
    fn an_empty_probe_is_not_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caps.json");
        let empty = Caps {
            encode: vec![],
            decode: vec![],
            rfi: false,
            ltr: false,
            max_fps: 60,
        };
        assert_eq!(cached(&path, &key("b", "g"), || empty.clone()), empty);
        assert!(!path.exists());
    }

    #[test]
    fn only_a_few_systems_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("caps.json");
        for i in 0..10 {
            store(&path, &key(&format!("b{i}"), "g"), &caps(60)).unwrap();
        }
        assert_eq!(read(&path).entries.len(), MAX_ENTRIES);
        assert!(load(&path, &key("b9", "g")).is_some());
        assert!(load(&path, &key("b0", "g")).is_none());
    }
}
