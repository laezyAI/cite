//! Audio and image inspection plus upload MIME types matching the Supabase buckets.
use std::path::Path;

use serde::{Deserialize, Serialize};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::core::CiteError;

pub const AUDIO_FORMATS: &[&str] = &["mp3", "wav", "m4a", "aac"];
pub const MAX_AUDIO_BYTES: u64 = 100 * 1024 * 1024;
pub const IMAGE_FORMATS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif"];
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

pub fn mime_type(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "m4a" => "audio/m4a",
        "aac" => "audio/aac",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "application/octet-stream",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioMeta {
    pub duration_secs: f64,
    pub format: String,
    pub codec: String,
    pub bitrate_kbps: u32,
    pub sample_rate_hz: u32,
    pub channels: u32,
    pub size_bytes: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageMeta {
    pub format: String,
    pub width: u32,
    pub height: u32,
    pub size_bytes: u64,
    pub sha256: String,
}

pub fn extract_audio(path: &Path) -> Result<AudioMeta, CiteError> {
    read_audio_meta(path, true)
}

pub fn inspect_audio(path: &Path) -> Result<AudioMeta, CiteError> {
    read_audio_meta(path, false)
}

pub fn extract_image(path: &Path) -> Result<ImageMeta, CiteError> {
    read_image_meta(path, true)
}

pub fn inspect_image(path: &Path) -> Result<ImageMeta, CiteError> {
    read_image_meta(path, false)
}

fn lowercase_ext(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

struct AudioProbe {
    duration_secs: f64,
    codec: String,
    sample_rate_hz: u32,
    channels: u32,
}

fn read_audio_meta(path: &Path, with_hash: bool) -> Result<AudioMeta, CiteError> {
    let size_bytes = std::fs::metadata(path)?.len();
    let sha256 = if with_hash {
        crate::core::cache::sha256_file(path)?
    } else {
        String::new()
    };
    let format = lowercase_ext(path);

    let probe = probe_audio(path, &format);
    let (duration_secs, codec, sample_rate_hz, channels) = match probe {
        Some(p) => (p.duration_secs, p.codec, p.sample_rate_hz, p.channels),
        None => (0.0, "unknown".to_string(), 0, 0),
    };
    let bitrate_kbps = if duration_secs > 0.0 && size_bytes > 0 {
        ((size_bytes as f64 * 8.0) / (duration_secs * 1000.0)).round() as u32
    } else {
        0
    };

    Ok(AudioMeta {
        duration_secs,
        format,
        codec,
        bitrate_kbps,
        sample_rate_hz,
        channels,
        size_bytes,
        sha256,
    })
}

fn probe_audio(path: &Path, ext: &str) -> Option<AudioProbe> {
    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(ext);

    let reader = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .ok()?;
    let track = reader.tracks().first()?;
    let Some(CodecParameters::Audio(params)) = &track.codec_params else {
        return None;
    };

    let codec_name = format!("{:?}", params.codec);
    let codec = codec_name
        .strip_prefix("AudioCodecId::")
        .unwrap_or(&codec_name)
        .to_lowercase();
    let sample_rate_hz = params.sample_rate.unwrap_or(0);
    let channels = params.channels.as_ref().map_or(0, |c| c.count() as u32);

    let num_frames = track.num_frames.unwrap_or(0) as f64;
    let duration_secs = match track.time_base {
        Some(tb) => num_frames * tb.numer.get() as f64 / tb.denom.get() as f64,
        None if sample_rate_hz > 0 => num_frames / sample_rate_hz as f64,
        None => 0.0,
    };

    Some(AudioProbe {
        duration_secs,
        codec,
        sample_rate_hz,
        channels,
    })
}

fn read_image_meta(path: &Path, with_hash: bool) -> Result<ImageMeta, CiteError> {
    let size_bytes = std::fs::metadata(path)?.len();
    let sha256 = if with_hash {
        crate::core::cache::sha256_file(path)?
    } else {
        String::new()
    };
    let ext = lowercase_ext(path);
    let format = match ext.as_str() {
        "jpg" | "jpeg" => "jpeg".to_string(),
        _ => ext,
    };
    let (width, height) = imagesize::size(path)
        .map(|d| (d.width as u32, d.height as u32))
        .unwrap_or((0, 0));

    Ok(ImageMeta {
        format,
        width,
        height,
        size_bytes,
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::cache::sha256_file;

    #[test]
    fn test_sha256_empty() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("empty.bin");
        std::fs::write(&f, b"").unwrap();
        let hash = sha256_file(&f).unwrap();
        assert_eq!(
            hash,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn test_sha256_known() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("data.bin");
        std::fs::write(&f, b"hello world").unwrap();
        let hash = sha256_file(&f).unwrap();
        assert_eq!(
            hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );
    }

    fn wav(secs: u32, rate: u32) -> Vec<u8> {
        let data_len = secs * rate * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.resize(out.len() + data_len as usize, 0);
        out
    }

    #[test]
    fn test_audio_duration_and_format_are_probed() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("ep.WAV");
        std::fs::write(&f, wav(2, 8000)).unwrap();

        let meta = extract_audio(&f).unwrap();
        assert!(
            (meta.duration_secs - 2.0).abs() < 0.01,
            "{}",
            meta.duration_secs
        );
        assert_eq!(meta.format, "wav");
        assert_eq!(meta.sample_rate_hz, 8000);
        assert_eq!(meta.channels, 1);
        assert_eq!(meta.bitrate_kbps, 128);
        assert!(!meta.sha256.is_empty());
        assert!(
            inspect_audio(&f).unwrap().sha256.is_empty(),
            "inspect skips hashing"
        );
    }

    #[test]
    fn test_unreadable_audio_has_no_duration() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("fake.mp3");
        std::fs::write(&f, b"not audio").unwrap();
        let meta = inspect_audio(&f).unwrap();
        assert_eq!(meta.duration_secs, 0.0);
        assert_eq!(meta.codec, "unknown");
    }

    #[test]
    fn test_mime_type_matches_podcasts_bucket() {
        assert_eq!(mime_type("MP3"), "audio/mpeg");
        assert_eq!(mime_type("m4a"), "audio/m4a");
        assert_eq!(mime_type("jpeg"), "image/jpeg");
        assert_eq!(mime_type("xyz"), "application/octet-stream");
    }

    #[test]
    fn test_extract_image_png() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("test.png");
        let min_png = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x62, 0x62, 0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x4A, 0x2E, 0x2C, 0xE8, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        std::fs::write(&f, &min_png).unwrap();
        let meta = extract_image(&f).unwrap();
        assert_eq!(meta.format, "png");
        assert_eq!(meta.width, 1);
        assert_eq!(meta.height, 1);
        assert!(meta.size_bytes > 0);
        assert!(!meta.sha256.is_empty());
    }

    #[test]
    fn test_extract_image_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("test.jpg");
        let min_jpg = vec![
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06,
            0x07, 0x06, 0x05, 0x08, 0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D,
            0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12, 0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D,
            0x1A, 0x1C, 0x1C, 0x20, 0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28,
            0x37, 0x29, 0x2C, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32,
            0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01,
            0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x1F, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01,
            0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03,
            0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0xFF, 0xC4, 0x00, 0xB5, 0x10, 0x00,
            0x02, 0x01, 0x03, 0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31,
            0x41, 0x06, 0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08,
            0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72, 0x82, 0x09,
            0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35,
            0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4A, 0x53,
            0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
            0x6A, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87,
            0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3,
            0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8,
            0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xD2, 0xD3, 0xD4,
            0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8,
            0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFF, 0xDA,
            0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0x7B, 0x94, 0x11, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xFF, 0xD9,
        ];
        std::fs::write(&f, &min_jpg).unwrap();
        let meta = extract_image(&f).unwrap();
        assert_eq!(meta.format, "jpeg");
        assert!(meta.size_bytes > 0);
    }

    #[test]
    fn test_extract_image_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("nonexistent.png");
        let result = extract_image(&f);
        assert!(result.is_err());
    }

    #[test]
    fn test_extract_image_unknown_format() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("test.xyz");
        std::fs::write(&f, b"not an image").unwrap();
        let meta = extract_image(&f).unwrap();
        assert_eq!(meta.format, "xyz");
        assert_eq!(meta.width, 0);
        assert_eq!(meta.height, 0);
    }
}
