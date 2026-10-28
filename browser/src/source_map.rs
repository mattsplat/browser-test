//! Source maps for wasm: map a byte offset in the module back to a source file, line and
//! column. Wasm source maps use one generated "line"; the generated column is the offset.
//! Format: https://sourcemaps.info/spec.html (base64 VLQ segments).

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, PartialEq)]
pub struct Location {
    pub file: String,
    /// 1-based, as editors show them.
    pub line: u32,
    pub column: u32,
}

impl std::fmt::Display for Location {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

struct Entry {
    offset: u32,
    /// None for segments that mark code with no source (e.g. compiler-generated).
    source: Option<(u32, u32, u32)>,
}

pub struct SourceMap {
    sources: Vec<String>,
    /// Sorted by offset.
    entries: Vec<Entry>,
}

#[derive(Deserialize)]
struct Raw {
    sources: Vec<String>,
    mappings: String,
}

impl SourceMap {
    pub fn parse(json: &str) -> Result<Self> {
        let raw: Raw = serde_json::from_str(json).context("source map is not valid JSON")?;
        let mut entries = Vec::new();
        // Running values: every field after the first segment is a delta.
        let (mut offset, mut source, mut line, mut column) = (0i64, 0i64, 0i64, 0i64);
        for segment in raw.mappings.split([',', ';']).filter(|s| !s.is_empty()) {
            let fields = decode_vlq(segment)?;
            offset += fields[0];
            let mapped = if fields.len() >= 4 {
                source += fields[1];
                line += fields[2];
                column += fields[3];
                Some((source as u32, line as u32, column as u32))
            } else {
                None
            };
            entries.push(Entry { offset: offset as u32, source: mapped });
        }
        entries.sort_by_key(|e| e.offset);
        Ok(Self { sources: raw.sources, entries })
    }

    /// The source location of the code at `offset`, if it has one.
    pub fn lookup(&self, offset: u32) -> Option<Location> {
        let i = self.entries.partition_point(|e| e.offset <= offset).checked_sub(1)?;
        let (source, line, column) = self.entries[i].source?;
        Some(Location { file: short_name(self.sources.get(source as usize)?), line: line + 1, column: column + 1 })
    }
}

/// `file:///.../lib/main.dart` → `main.dart`; SDK files → `dart-sdk/<path>`.
fn short_name(source: &str) -> String {
    if let Some(sdk) = source.strip_prefix("org-dartlang-sdk:///") {
        return format!("dart-sdk/{}", sdk.trim_start_matches("lib/"));
    }
    source.rsplit('/').next().unwrap_or(source).to_string()
}

fn decode_vlq(segment: &str) -> Result<Vec<i64>> {
    let mut values = Vec::new();
    let (mut value, mut shift) = (0i64, 0);
    for c in segment.bytes() {
        let digit = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => bail!("bad base64 character {:?} in source map", c as char),
        } as i64;
        value |= (digit & 31) << shift;
        if digit & 32 != 0 {
            shift += 5;
            continue;
        }
        // The lowest bit is the sign.
        values.push(if value & 1 == 1 { -(value >> 1) } else { value >> 1 });
        value = 0;
        shift = 0;
    }
    if values.is_empty() {
        bail!("empty source map segment");
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_vlq() {
        assert_eq!(decode_vlq("A").unwrap(), [0]);
        assert_eq!(decode_vlq("C").unwrap(), [1]);
        assert_eq!(decode_vlq("D").unwrap(), [-1]);
        assert_eq!(decode_vlq("gB").unwrap(), [16]);
        assert_eq!(decode_vlq("AAAA").unwrap(), [0, 0, 0, 0]);
    }

    #[test]
    fn looks_up_the_nearest_preceding_mapping() {
        // offset 10 → a.dart line 1 col 1; offset 20 → unmapped; offset 30 → b.dart line 3 col 5.
        // Segments: [10,0,0,0], [+10], [+10,+1,+2,+4]
        let map = SourceMap::parse(r#"{"sources":["file:///x/a.dart","org-dartlang-sdk:///lib/core/b.dart"],"mappings":"UAAA,U,UCEI"}"#).unwrap();
        assert_eq!(map.lookup(5), None, "before any mapping");
        assert_eq!(map.lookup(12).unwrap().to_string(), "a.dart:1:1");
        assert_eq!(map.lookup(25), None, "unmapped segment");
        assert_eq!(map.lookup(31).unwrap().to_string(), "dart-sdk/core/b.dart:3:5");
    }
}
