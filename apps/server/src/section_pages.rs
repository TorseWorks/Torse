//! Immutable, content-addressed section pages. Publish pages before referencing them in a journal.
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
use torse_codec::{CodecLimits, wire};
use torse_types::{Digest, Fault, FaultCode, OutputBudget};
use torse_world::Snapshot;

static NEXT: AtomicU64 = AtomicU64::new(0);
mod bundle;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PageCollection {
    pub removed_pages: u32,
    pub removed_temporaries: u32,
}

#[derive(Clone)]
pub struct SectionPages {
    root: PathBuf,
    budget: OutputBudget,
    codec: CodecLimits,
    max_files: usize,
}

impl SectionPages {
    /// The caller owns the world directory lock and creates the page directory.
    pub fn new(
        root: impl AsRef<Path>,
        budget: OutputBudget,
        codec: CodecLimits,
        max_files: usize,
    ) -> Result<Self, Fault> {
        if !fs::symlink_metadata(root.as_ref())
            .map_err(io)?
            .file_type()
            .is_dir()
            || max_files == 0
            || budget.max_bytes == 0
            || budget.max_items == 0
        {
            return Err(Fault::invalid_data(
                "invalid section page directory or limits",
            ));
        }
        Ok(Self {
            root: root.as_ref().to_owned(),
            budget,
            codec,
            max_files,
        })
    }
    pub fn digest(&self, page: &Snapshot) -> Result<Digest, Fault> {
        Ok(hash(&self.encode(page)?))
    }
    pub fn max_files(&self) -> usize {
        self.max_files
    }
    pub fn budget(&self) -> OutputBudget {
        self.budget
    }
    pub(crate) fn codec_limits(&self) -> CodecLimits {
        self.codec
    }
    pub(crate) fn measure(&self, page: &Snapshot, max_bytes: u64) -> Result<wire::WireCost, Fault> {
        wire::measure_inner(
            page,
            OutputBudget {
                max_bytes: self.budget.max_bytes.min(max_bytes),
                ..self.budget
            },
            self.codec,
        )
    }
    fn encode(&self, page: &Snapshot) -> Result<Vec<u8>, Fault> {
        if page.sections.len() != 1 {
            return Err(Fault::invalid_data("page requires exactly one section"));
        }
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"TORSEPG1");
        wire::encode(page, &mut bytes, self.budget, self.codec)?;
        if bytes.len() as u64 > self.budget.max_bytes {
            return Err(Fault::resource_limit("page exceeds file budget"));
        }
        Ok(bytes)
    }
    pub fn put(&self, page: &Snapshot) -> Result<Digest, Fault> {
        let bytes = self.encode(page)?;
        let digest = hash(&bytes);
        let path = self.path(digest);
        if path.exists() {
            if self.get(digest)? != *page {
                return Err(Fault::invalid_data("page digest collision"));
            }
            return Ok(digest);
        }
        // Include interrupted publications in the directory capacity limit.
        if fs::read_dir(&self.root).map_err(io)?.count() >= self.max_files {
            return Err(Fault::resource_limit(
                "section page file capacity exhausted",
            ));
        }
        let temporary = self.root.join(format!(
            "page-{}-{}.part",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(io)?;
            file.write_all(&bytes).map_err(io)?;
            file.sync_all().map_err(io)?;
            drop(file);
            fs::rename(&temporary, &path).map_err(io)?;
            #[cfg(unix)]
            fs::File::open(&self.root)
                .and_then(|dir| dir.sync_all())
                .map_err(io)?;
            Ok(digest)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
    pub fn get(&self, digest: Digest) -> Result<Snapshot, Fault> {
        let path = self.path(digest);
        if !fs::symlink_metadata(&path)
            .map_err(io)?
            .file_type()
            .is_file()
        {
            return Err(Fault::invalid_data("section page must be a regular file"));
        }
        let mut file = fs::File::open(path).map_err(io)?;
        let limit = self
            .budget
            .max_bytes
            .min(u64::from(self.codec.max_frame_bytes).saturating_add(8));
        let length = file.metadata().map_err(io)?.len();
        let mut magic = [0; 8];
        file.read_exact(&mut magic).map_err(io)?;
        if &magic == bundle::MAGIC {
            let bytes = self.read_bundle_page(file, length, digest, limit)?;
            return self.decode_page(&bytes, digest);
        }
        if length > limit {
            return Err(Fault::resource_limit("section page too large"));
        }
        let mut bytes = magic.to_vec();
        file.take(limit.saturating_add(1).saturating_sub(8))
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() as u64 > limit {
            return Err(Fault::resource_limit("section page too large"));
        }
        self.decode_page(&bytes, digest)
    }

    fn decode_page(&self, bytes: &[u8], digest: Digest) -> Result<Snapshot, Fault> {
        if hash(bytes) != digest || !bytes.starts_with(b"TORSEPG1") {
            return Err(Fault::invalid_data("section page integrity check failed"));
        }
        let page: Snapshot = wire::decode(&bytes[8..], self.budget, self.codec)?;
        if page.sections.len() != 1 {
            return Err(Fault::invalid_data("invalid section page shape"));
        }
        Ok(page)
    }
    pub fn path(&self, digest: Digest) -> PathBuf {
        let name: String = digest.0.iter().map(|b| format!("{b:02x}")).collect();
        self.root.join(format!("{name}.page"))
    }

    /// Trace all retained checkpoint, replay, and live authority references first.
    /// The caller holds the world's exclusive lease and excludes page publication for this
    /// entire operation. A deletion failure leaves only unreachable files to retry later.
    pub fn collect(&self, retained: &BTreeSet<Digest>) -> Result<PageCollection, Fault> {
        if retained.len() > self.max_files {
            return Err(Fault::resource_limit("section reference capacity exceeded"));
        }
        for digest in retained {
            self.get(*digest)?;
        }
        let mut garbage = Vec::new();
        for (index, entry) in fs::read_dir(&self.root).map_err(io)?.enumerate() {
            if index >= self.max_files {
                return Err(Fault::resource_limit(
                    "section page directory capacity exceeded",
                ));
            }
            let entry = entry.map_err(io)?;
            if !entry.file_type().map_err(io)?.is_file() {
                return Err(Fault::invalid_data(
                    "section page directory contains a non-regular file",
                ));
            }
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| Fault::invalid_data("invalid page filename"))?;
            let temporary = if let Some(stem) = name
                .strip_prefix("page-")
                .and_then(|s| s.strip_suffix(".part"))
            {
                stem.split_once('-').is_some_and(|(pid, serial)| {
                    [pid, serial]
                        .iter()
                        .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
                })
            } else {
                false
            };
            let digest = name.strip_suffix(".page").and_then(|s| {
                if s.len() != 64
                    || !s
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return None;
                }
                let mut digest = [0; 32];
                for (index, byte) in digest.iter_mut().enumerate() {
                    *byte = u8::from_str_radix(&s[index * 2..index * 2 + 2], 16).ok()?;
                }
                Some(Digest(digest))
            });
            if !temporary && digest.is_none() {
                return Err(Fault::invalid_data(
                    "unexpected file in section page directory",
                ));
            }
            if temporary || digest.is_some_and(|d| !retained.contains(&d)) {
                garbage
                    .try_reserve(1)
                    .map_err(|_| Fault::resource_limit("page collection allocation"))?;
                garbage.push((entry.path(), temporary));
            }
        }
        let mut report = PageCollection::default();
        for (path, temporary) in garbage {
            fs::remove_file(path).map_err(io)?;
            if temporary {
                report.removed_temporaries += 1;
            } else {
                report.removed_pages += 1;
            }
        }
        #[cfg(unix)]
        fs::File::open(&self.root)
            .and_then(|dir| dir.sync_all())
            .map_err(io)?;
        Ok(report)
    }
}

fn hash(bytes: &[u8]) -> Digest {
    Digest(Sha256::digest(bytes).into())
}
fn io(error: std::io::Error) -> Fault {
    Fault::new(FaultCode::StorageUnavailable, error.to_string())
}
