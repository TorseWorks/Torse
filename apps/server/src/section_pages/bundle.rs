//! Store each batch in one immutable file, addressed by its member digests.
use super::*;
use std::io::{Seek, SeekFrom};

pub(super) const MAGIC: &[u8; 8] = b"TORSEPB1";
const HEADER_BYTES: u64 = 12;
const ENTRY_BYTES: u64 = 36;

impl SectionPages {
    /// Each new member name links to the same synced file. Existing names are
    /// validated and left in place. A failed publication can leave unreachable
    /// names for the normal page collector.
    pub(crate) fn put_bundle(&self, pages: &[Snapshot]) -> Result<(Vec<Digest>, bool), Fault> {
        if !(1..=1024).contains(&pages.len()) {
            return Err(Fault::resource_limit("section bundle page count"));
        }
        let mut members = Vec::with_capacity(pages.len());
        let mut digests = Vec::with_capacity(pages.len());
        let mut total = HEADER_BYTES + ENTRY_BYTES * pages.len() as u64;
        let mut new_names = Vec::new();
        for page in pages {
            let bytes = self.encode(page)?;
            total = total.saturating_add(bytes.len() as u64);
            if total > self.budget.max_bytes || bytes.len() > u32::MAX as usize {
                return Err(Fault::resource_limit("section bundle bytes"));
            }
            let digest = hash(&bytes);
            let name = self.path(digest);
            if name.try_exists().map_err(io)? {
                if self.get(digest)? != *page {
                    return Err(Fault::invalid_data("section page digest collision"));
                }
            } else {
                new_names.push(name);
            }
            members.push((digest, bytes));
            digests.push(digest);
        }
        members.sort_unstable_by_key(|member| member.0);
        if members.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(Fault::invalid_data("duplicate section bundle member"));
        }
        if new_names.is_empty() {
            #[cfg(unix)]
            fs::File::open(&self.root)
                .and_then(|dir| dir.sync_all())
                .map_err(io)?;
            return Ok((digests, false));
        }
        if fs::read_dir(&self.root)
            .map_err(io)?
            .count()
            .saturating_add(new_names.len())
            .saturating_add(1)
            > self.max_files
        {
            return Err(Fault::resource_limit("section bundle file capacity"));
        }
        let temporary = self.root.join(format!(
            "page-{}-{}.part",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(io)?;
        let written = (|| {
            let mut out = std::io::BufWriter::new(&mut file);
            out.write_all(MAGIC)?;
            out.write_all(&(members.len() as u32).to_le_bytes())?;
            for (digest, bytes) in &members {
                out.write_all(&digest.0)?;
                out.write_all(&(bytes.len() as u32).to_le_bytes())?;
            }
            for (_, bytes) in &members {
                out.write_all(bytes)?;
            }
            out.flush()?;
            drop(out);
            file.sync_all()
        })();
        drop(file);
        let published = written.and_then(|()| {
            for name in new_names {
                fs::hard_link(&temporary, name)?;
            }
            Ok(())
        });
        let removed = fs::remove_file(&temporary);
        published.and(removed).map_err(io)?;
        #[cfg(unix)]
        fs::File::open(&self.root)
            .and_then(|dir| dir.sync_all())
            .map_err(io)?;
        Ok((digests, true))
    }

    pub(super) fn read_bundle_page(
        &self,
        mut file: fs::File,
        length: u64,
        digest: Digest,
        limit: u64,
    ) -> Result<Vec<u8>, Fault> {
        let invalid = || Fault::invalid_data("invalid section bundle directory");
        if length > self.budget.max_bytes {
            return Err(Fault::resource_limit("section bundle bytes"));
        }
        let mut count = [0; 4];
        file.read_exact(&mut count).map_err(io)?;
        let count = u32::from_le_bytes(count);
        if !(1..=1024).contains(&count) {
            return Err(invalid());
        }
        let mut offset = HEADER_BYTES + u64::from(count) * ENTRY_BYTES;
        if offset > length {
            return Err(invalid());
        }
        let mut directory = vec![0; (u64::from(count) * ENTRY_BYTES) as usize];
        file.read_exact(&mut directory).map_err(io)?;
        let mut previous = None;
        let mut member = None;
        for entry in directory.chunks_exact(ENTRY_BYTES as usize) {
            let id = Digest(entry[..32].try_into().expect("directory digest width"));
            let size = u64::from(u32::from_le_bytes(
                entry[32..].try_into().expect("directory size width"),
            ));
            if previous.is_some_and(|last| last >= id) || size < 8 || size > limit {
                return Err(invalid());
            }
            previous = Some(id);
            if id == digest {
                member = Some((offset, size));
            }
            offset += size;
        }
        if offset != length {
            return Err(invalid());
        }
        let (offset, size) = member.ok_or_else(invalid)?;
        file.seek(SeekFrom::Start(offset)).map_err(io)?;
        let mut bytes = vec![0; usize::try_from(size).map_err(|_| invalid())?];
        file.read_exact(&mut bytes).map_err(io)?;
        Ok(bytes)
    }
}
