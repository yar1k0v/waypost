//! Index file container.
//!
//! Layout, all integers little-endian:
//!
//! ```text
//! [0..8)   magic "WAYPOST\0"
//! [8..12)  format version (u32)
//! [12..16) reserved, zero
//! [16..24) metadata offset (u64)
//! [24..32) metadata length (u64)
//! [32..)   sections, each starting at a multiple of 8
//! [meta)   metadata as JSON: build settings, sources, section table
//! ```
//!
//! The server reads every build setting from the metadata, so an index can
//! never be read with settings it was not built with.

use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use serde::{Deserialize, Serialize};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub const MAGIC: [u8; 8] = *b"WAYPOST\0";
/// Version 2 added OSM ids to addresses, interpolations, places and areas.
pub const FORMAT_VERSION: u32 = 2;
const HEADER_LEN: u64 = 32;
const ALIGN: u64 = 8;

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("not a waypost index (bad magic)")]
    BadMagic,
    #[error("index format version {found}, this build reads version {expected}")]
    Version { found: u32, expected: u32 },
    #[error("index is truncated or corrupt: {0}")]
    Corrupt(String),
    #[error("index metadata: {0}")]
    Meta(#[from] serde_json::Error),
    #[error("section {0} is missing")]
    MissingSection(String),
    #[error("section {name}: {reason}")]
    BadSection { name: String, reason: String },
    #[error("section {0} written twice")]
    DuplicateSection(String),
}

/// Settings the index was built with. The server takes them from here.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct BuildSettings {
    /// Side of a street/address grid cell, in 1e-7 degree units.
    pub street_cell: i32,
    /// Side of an admin-area grid cell, in 1e-7 degree units.
    pub admin_cell: i32,
    /// Name languages stored besides the default `name` tag, in order.
    pub langs: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SectionInfo {
    pub name: String,
    pub offset: u64,
    pub len: u64,
    pub elem_size: u32,
}

/// Index metadata, stored as JSON at the end of the file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct Meta {
    pub format_version: u32,
    pub builder_version: String,
    /// Build time, RFC 3339.
    pub built_at: String,
    /// Source extracts, for example "switzerland-latest.osm.pbf".
    pub sources: Vec<String>,
    /// Newest OSM replication timestamp among the sources, if known.
    pub data_date: Option<String>,
    pub settings: BuildSettings,
    pub sections: Vec<SectionInfo>,
}

/// Writes sections to `<path>.tmp` and renames it to `path` on `finish`, so
/// a crash never leaves a partial index under the final name.
pub struct IndexWriter {
    out: BufWriter<File>,
    tmp: PathBuf,
    path: PathBuf,
    pos: u64,
    sections: Vec<SectionInfo>,
}

impl IndexWriter {
    pub fn create(path: impl AsRef<Path>) -> Result<IndexWriter, FormatError> {
        let path = path.as_ref().to_path_buf();
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&tmp)?);
        out.write_all(&[0u8; HEADER_LEN as usize])?;
        Ok(IndexWriter {
            out,
            tmp,
            path,
            pos: HEADER_LEN,
            sections: Vec::new(),
        })
    }

    fn pad(&mut self) -> Result<(), FormatError> {
        let rem = self.pos % ALIGN;
        if rem != 0 {
            let n = (ALIGN - rem) as usize;
            self.out.write_all(&[0u8; ALIGN as usize][..n])?;
            self.pos += n as u64;
        }
        Ok(())
    }

    /// Appends one section of fixed-size records.
    pub fn add<T: IntoBytes + Immutable>(
        &mut self,
        name: &str,
        items: &[T],
    ) -> Result<(), FormatError> {
        self.add_raw(name, items.as_bytes(), size_of::<T>() as u32)
    }

    /// Appends one section of raw bytes (element size 1).
    pub fn add_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<(), FormatError> {
        self.add_raw(name, bytes, 1)
    }

    fn add_raw(&mut self, name: &str, bytes: &[u8], elem_size: u32) -> Result<(), FormatError> {
        if self.sections.iter().any(|s| s.name == name) {
            return Err(FormatError::DuplicateSection(name.to_string()));
        }
        self.pad()?;
        self.out.write_all(bytes)?;
        self.sections.push(SectionInfo {
            name: name.to_string(),
            offset: self.pos,
            len: bytes.len() as u64,
            elem_size,
        });
        self.pos += bytes.len() as u64;
        Ok(())
    }

    /// Writes the metadata and header, syncs, and renames the file into place.
    pub fn finish(mut self, mut meta: Meta) -> Result<PathBuf, FormatError> {
        meta.format_version = FORMAT_VERSION;
        meta.sections = std::mem::take(&mut self.sections);
        let json = serde_json::to_vec(&meta)?;
        self.pad()?;
        let meta_off = self.pos;
        self.out.write_all(&json)?;
        let mut header = Vec::with_capacity(HEADER_LEN as usize);
        header.extend_from_slice(&MAGIC);
        header.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
        header.extend_from_slice(&0u32.to_le_bytes());
        header.extend_from_slice(&meta_off.to_le_bytes());
        header.extend_from_slice(&(json.len() as u64).to_le_bytes());
        self.out.seek(std::io::SeekFrom::Start(0))?;
        self.out.write_all(&header)?;
        let file = self.out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&self.tmp, &self.path)?;
        Ok(self.path)
    }
}

/// A memory-mapped, validated index file.
pub struct IndexFile {
    map: Mmap,
    meta: Meta,
}

impl IndexFile {
    pub fn open(path: impl AsRef<Path>) -> Result<IndexFile, FormatError> {
        let file = File::open(path)?;
        // SAFETY: the index is immutable once written. The writer renames a
        // complete file into place and never writes an existing index, so the
        // mapped bytes do not change while this process maps them.
        let map = unsafe { Mmap::map(&file)? };
        Self::from_map(map)
    }

    fn from_map(map: Mmap) -> Result<IndexFile, FormatError> {
        let len = map.len() as u64;
        if len < HEADER_LEN {
            return Err(FormatError::Corrupt("shorter than the header".into()));
        }
        if map[0..8] != MAGIC {
            return Err(FormatError::BadMagic);
        }
        let u32_at = |o: usize| u32::from_le_bytes(map[o..o + 4].try_into().unwrap());
        let u64_at = |o: usize| u64::from_le_bytes(map[o..o + 8].try_into().unwrap());
        let version = u32_at(8);
        if version != FORMAT_VERSION {
            return Err(FormatError::Version {
                found: version,
                expected: FORMAT_VERSION,
            });
        }
        let (meta_off, meta_len) = (u64_at(16), u64_at(24));
        let meta_end = meta_off
            .checked_add(meta_len)
            .filter(|&e| e <= len && meta_off >= HEADER_LEN)
            .ok_or_else(|| FormatError::Corrupt("metadata outside the file".into()))?;
        let meta: Meta = serde_json::from_slice(&map[meta_off as usize..meta_end as usize])?;
        if meta.format_version != FORMAT_VERSION {
            return Err(FormatError::Version {
                found: meta.format_version,
                expected: FORMAT_VERSION,
            });
        }
        for s in &meta.sections {
            let bad = |reason: &str| FormatError::BadSection {
                name: s.name.clone(),
                reason: reason.to_string(),
            };
            let end = s
                .offset
                .checked_add(s.len)
                .ok_or_else(|| bad("overflows"))?;
            if s.offset < HEADER_LEN || end > meta_off {
                return Err(bad("outside the data area"));
            }
            if s.offset % ALIGN != 0 {
                return Err(bad("misaligned"));
            }
            if s.elem_size == 0 || s.len % s.elem_size as u64 != 0 {
                return Err(bad("length is not a whole number of records"));
            }
        }
        Ok(IndexFile { map, meta })
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    fn info(&self, name: &str) -> Result<&SectionInfo, FormatError> {
        self.meta
            .sections
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| FormatError::MissingSection(name.to_string()))
    }

    /// The whole mapped file.
    pub fn bytes_all(&self) -> &[u8] {
        &self.map
    }

    pub fn bytes(&self, name: &str) -> Result<&[u8], FormatError> {
        let s = self.info(name)?;
        Ok(&self.map[s.offset as usize..(s.offset + s.len) as usize])
    }

    /// A section as a slice of records. Fails if the record size differs
    /// from the one written.
    pub fn section<T: FromBytes + Immutable + KnownLayout>(
        &self,
        name: &str,
    ) -> Result<&[T], FormatError> {
        let s = self.info(name)?;
        if s.elem_size as usize != size_of::<T>() {
            return Err(FormatError::BadSection {
                name: name.to_string(),
                reason: format!(
                    "record size {} in file, {} expected",
                    s.elem_size,
                    size_of::<T>()
                ),
            });
        }
        <[T]>::ref_from_bytes(self.bytes(name)?).map_err(|_| FormatError::BadSection {
            name: name.to_string(),
            reason: "cannot view as records".into(),
        })
    }

    /// Touches every page so the first queries do not pay page faults.
    pub fn warm(&self) -> u64 {
        let mut sum = 0u64;
        for chunk in self.map.chunks(4096) {
            sum = sum.wrapping_add(chunk[0] as u64);
        }
        sum
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::Point;

    fn write_sample(path: &Path) {
        let mut w = IndexWriter::create(path).unwrap();
        w.add_bytes("strings", b"abc").unwrap();
        w.add(
            "points",
            &[Point { lat: 1, lon: 2 }, Point { lat: 3, lon: 4 }],
        )
        .unwrap();
        let meta = Meta {
            builder_version: "test".into(),
            settings: BuildSettings {
                street_cell: 50_000,
                admin_cell: 500_000,
                langs: vec!["en".into()],
            },
            ..Default::default()
        };
        w.finish(meta).unwrap();
    }

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        write_sample(&path);
        assert!(!dir.path().join("x.idx.tmp").exists());
        let f = IndexFile::open(&path).unwrap();
        assert_eq!(f.bytes("strings").unwrap(), b"abc");
        let pts: &[Point] = f.section("points").unwrap();
        assert_eq!(pts, &[Point { lat: 1, lon: 2 }, Point { lat: 3, lon: 4 }]);
        assert_eq!(f.meta().settings.street_cell, 50_000);
        assert!(matches!(
            f.section::<u32>("nope"),
            Err(FormatError::MissingSection(_))
        ));
        assert!(matches!(
            f.section::<u32>("points"),
            Err(FormatError::BadSection { .. })
        ));
    }

    #[test]
    fn rejects_truncated_and_wrong_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.idx");
        write_sample(&path);
        let full = std::fs::read(&path).unwrap();

        let cut = dir.path().join("cut.idx");
        for n in [0, 10, 31, 40, full.len() - 5] {
            std::fs::write(&cut, &full[..n]).unwrap();
            assert!(IndexFile::open(&cut).is_err(), "len {n} accepted");
        }

        let mut other = full.clone();
        other[8..12].copy_from_slice(&(FORMAT_VERSION + 1).to_le_bytes());
        std::fs::write(&cut, &other).unwrap();
        assert!(matches!(
            IndexFile::open(&cut),
            Err(FormatError::Version { found, .. }) if found == FORMAT_VERSION + 1
        ));

        let mut bad = full;
        bad[0] = b'X';
        std::fs::write(&cut, &bad).unwrap();
        assert!(matches!(IndexFile::open(&cut), Err(FormatError::BadMagic)));
    }
}
