#![cfg(target_os = "linux")]

use jwm_file_core::{Cancellation, FileIdentity, ListError, WalkOptions, WalkSummary, visit_local_index};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf};

const SCHEMA: &str = include_str!("schema.sql");
const MAX_QUERY_BYTES: usize = 512;
const MAX_QUERY_CHARS: usize = 128;
const MAX_STAGED_PATH_BYTES: usize = 32 * 1024 * 1024;
const MAX_REGISTERED_ROOTS: i64 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IndexScope {
    incarnation: [u8; 16],
    generation: i64,
}

#[derive(Debug)]
pub enum IndexError {
    Sql(rusqlite::Error),
    Walk(ListError),
    StaleScope,
    Incomplete,
    ByteBudget,
    InvalidRoot,
    RootBudget,
}

impl From<rusqlite::Error> for IndexError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sql(value)
    }
}

pub struct NameIndex {
    connection: Connection,
}

impl NameIndex {
    /// A process-local index; this entry point never creates a database file.
    pub fn memory() -> Result<Self, IndexError> {
        let connection = Connection::open_in_memory()?;
        connection.execute_batch(SCHEMA)?;
        Ok(Self { connection })
    }

    pub fn register(
        &self,
        id: i64,
        root: &Path,
        identity: FileIdentity,
    ) -> Result<IndexScope, IndexError> {
        let raw = root.as_os_str().as_bytes();
        if !root.is_absolute() || raw.len() > 4096 || raw.contains(&0)
            || root.components().any(|part| part == Component::ParentDir)
        {
            return Err(IndexError::InvalidRoot);
        }
        let roots: i64 = self.connection.query_row("SELECT count(*) FROM roots", [], |row| {
            row.get(0)
        })?;
        if roots >= MAX_REGISTERED_ROOTS {
            return Err(IndexError::RootBudget);
        }
        let encoded = encode_identity(identity);
        let incarnation = self.connection.query_row(
            "INSERT INTO roots(id,incarnation,raw_root,root_identity,enabled,generation)
             VALUES (?1,randomblob(16),?2,?3,1,0) RETURNING incarnation",
            params![id, raw, encoded.as_slice()],
            |row| row.get::<_, Vec<u8>>(0),
        )?.try_into().map_err(|_| IndexError::InvalidRoot)?;
        Ok(IndexScope { incarnation, generation: 0 })
    }

    pub fn revoke(&mut self, id: i64, scope: IndexScope) -> Result<(), IndexError> {
        let transaction = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let n = transaction.execute(
            "DELETE FROM roots WHERE id=?1 AND enabled AND incarnation=?2 AND generation=?3",
            params![id, scope.incarnation.as_slice(), scope.generation],
        )?;
        if n != 1 {
            return Err(IndexError::StaleScope);
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn rebuild(
        &mut self,
        id: i64,
        scope: IndexScope,
        options: WalkOptions,
        cancel: &Cancellation,
    ) -> Result<(IndexScope, WalkSummary), IndexError> {
        let (root, identity) = self.root_for(id, scope)?;
        let mut names = Vec::new();
        let mut byte_count = 0usize;
        let summary = visit_local_index(&root, identity, options, cancel, |entry| {
            byte_count = byte_count.saturating_add(entry.relative.as_os_str().as_bytes().len());
            if byte_count <= MAX_STAGED_PATH_BYTES {
                names.push(entry.relative.to_path_buf());
            }
        })
        .map_err(IndexError::Walk)?;
        if summary.incomplete {
            return Err(IndexError::Incomplete);
        }
        if byte_count > MAX_STAGED_PATH_BYTES {
            return Err(IndexError::ByteBudget);
        }
        let scope = self.replace_complete(id, scope, &names, cancel)?;
        Ok((scope, summary))
    }

    fn replace_complete(
        &mut self,
        id: i64,
        scope: IndexScope,
        names: &[PathBuf],
        cancel: &Cancellation,
    ) -> Result<IndexScope, IndexError> {
        cancel.check().map_err(IndexError::Walk)?;
        let transaction = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next = scope.generation.checked_add(1).ok_or(IndexError::StaleScope)?;
        let n = transaction.execute(
            "UPDATE roots SET generation=?4 WHERE id=?1 AND enabled AND incarnation=?2 AND generation=?3",
            params![id, scope.incarnation.as_slice(), scope.generation, next],
        )?;
        if n != 1 {
            return Err(IndexError::StaleScope);
        }
        transaction.execute("DELETE FROM entries WHERE root_id=?1", [id])?;
        {
            let mut insert = transaction.prepare(
                "INSERT INTO entries(root_id,raw_key,folded) VALUES (?1,?2,?3)",
            )?;
            for path in names {
                cancel.check().map_err(IndexError::Walk)?;
                let label = path.file_name().ok_or(IndexError::Incomplete)?.to_string_lossy();
                insert.execute(params![id, path.as_os_str().as_bytes(), label.to_lowercase()])?;
            }
        }
        cancel.check().map_err(IndexError::Walk)?;
        transaction.commit()?;
        Ok(IndexScope { generation: next, ..scope })
    }

    pub fn search(
        &self,
        id: i64,
        scope: IndexScope,
        literal: &str,
    ) -> Result<Vec<Vec<u8>>, IndexError> {
        self.root_for(id, scope)?;
        if literal.is_empty() || literal.len() > MAX_QUERY_BYTES || literal.contains('\0') {
            return Ok(Vec::new());
        }
        let folded = literal.to_lowercase();
        let size = folded.chars().count();
        if size > MAX_QUERY_CHARS {
            return Ok(Vec::new());
        }
        let (sql, parameter) = if size < 3 {
            (
                "SELECT e.raw_key FROM entries e JOIN roots r ON r.id=e.root_id
                 WHERE r.id=?1 AND r.enabled AND instr(e.folded,?2)>0 ORDER BY e.raw_key LIMIT 50",
                folded,
            )
        } else {
            (
                "SELECT e.raw_key FROM names
                 JOIN entries e ON e.id=names.rowid JOIN roots r ON r.id=e.root_id
                 WHERE r.id=?1 AND r.enabled AND names MATCH ?2 ORDER BY e.raw_key LIMIT 50",
                format!("\"{}\"", folded.replace('"', "\"\"")),
            )
        };
        let mut stmt = self.connection.prepare(sql)?;
        let raw = stmt.query_map(params![id, parameter], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(raw)
    }

    fn root_for(&self, id: i64, scope: IndexScope) -> Result<(PathBuf, FileIdentity), IndexError> {
        let (raw, encoded) = self.connection.query_row(
            "SELECT raw_root,root_identity FROM roots
             WHERE id=?1 AND enabled AND incarnation=?2 AND generation=?3",
            params![id, scope.incarnation.as_slice(), scope.generation],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        ).optional()?.ok_or(IndexError::StaleScope)?;
        let (device, inode) = encoded.split_at_checked(8).ok_or(IndexError::InvalidRoot)?;
        let identity = FileIdentity {
            device: u64::from_le_bytes(device.try_into().map_err(|_| IndexError::InvalidRoot)?),
            inode: u64::from_le_bytes(inode.try_into().map_err(|_| IndexError::InvalidRoot)?),
        };
        Ok((PathBuf::from(OsString::from_vec(raw)), identity))
    }
}

fn encode_identity(identity: FileIdentity) -> [u8; 16] {
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&identity.device.to_le_bytes());
    bytes[8..].copy_from_slice(&identity.inode.to_le_bytes());
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use jwm_file_core::DirectoryHandle;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let time = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let next = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "jwm-file-index-{}-{time}-{next}",
                std::process::id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn identity(&self) -> FileIdentity {
            DirectoryHandle::open(&self.0).unwrap().identity()
        }

        fn file(&self, name: &[u8]) {
            fs::write(self.0.join(OsString::from_vec(name.to_vec())), b"data").unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn options() -> WalkOptions {
        WalkOptions {
            entries: 32,
            directories: 8,
            depth: 3,
            show_hidden: false,
        }
    }

    #[test]
    fn literal_queries_keep_exact_byte_keys_and_root_scope() {
        let f = Fixture::new();
        let other = Fixture::new();
        f.file(b"ALPHA-%20-\x80");
        f.file(b"ALPHA-%20-\x81");
        f.file("合同三版".as_bytes());
        f.file(b"some\" OR \"literal");
        other.file(b"alpha-foreign");
        let mut index = NameIndex::memory().unwrap();
        let root = index.register(1, &f.0, f.identity()).unwrap();
        let other_root = index.register(2, &other.0, other.identity()).unwrap();
        let cancel = Cancellation::default();
        let (root, _) = index.rebuild(1, root, options(), &cancel).unwrap();
        let (other_root, _) = index.rebuild(2, other_root, options(), &cancel).unwrap();
        assert_eq!(
            index.search(1, root, "alpha").unwrap(),
            [b"ALPHA-%20-\x80".to_vec(), b"ALPHA-%20-\x81".to_vec()]
        );
        assert_eq!(index.search(1, root, "合同").unwrap(), ["合同三版".as_bytes()]);
        assert_eq!(index.search(1, root, "合同三").unwrap(), ["合同三版".as_bytes()]);
        assert_eq!(
            index.search(1, root, "some\" OR \"literal").unwrap(),
            [b"some\" OR \"literal".as_slice()]
        );
        assert_eq!(index.search(2, other_root, "alpha").unwrap(), [b"alpha-foreign".as_slice()]);
        assert!(matches!(index.search(2, root, "alpha"), Err(IndexError::StaleScope)));
    }

    #[test]
    fn incomplete_cancelled_and_stale_rebuilds_preserve_previous_generation() {
        let f = Fixture::new();
        f.file(b"first");
        let mut index = NameIndex::memory().unwrap();
        let root = index.register(1, &f.0, f.identity()).unwrap();
        let (complete, _) = index.rebuild(1, root, options(), &Cancellation::default()).unwrap();
        f.file(b"second");
        let short = WalkOptions { entries: 1, ..options() };
        assert!(matches!(
            index.rebuild(1, complete, short, &Cancellation::default()),
            Err(IndexError::Incomplete)
        ));
        let cancelled = Cancellation::default();
        cancelled.cancel();
        assert!(matches!(
            index.rebuild(1, complete, options(), &cancelled),
            Err(IndexError::Walk(ListError::Cancelled))
        ));
        assert!(matches!(
            index.rebuild(1, root, options(), &Cancellation::default()),
            Err(IndexError::StaleScope)
        ));
        assert_eq!(index.search(1, complete, "first").unwrap(), [b"first".as_slice()]);
        assert!(index.search(1, complete, "second").unwrap().is_empty());
    }

    #[test]
    fn revoked_numeric_id_cannot_replay_when_rebound_to_a_different_root() {
        let f = Fixture::new();
        let other = Fixture::new();
        f.file(b"first");
        other.file(b"second");
        let mut index = NameIndex::memory().unwrap();
        let stale = index.register(1, &f.0, f.identity()).unwrap();
        index.revoke(1, stale).unwrap();
        let fresh = index.register(1, &other.0, other.identity()).unwrap();
        assert_ne!(stale, fresh);
        assert!(matches!(
            index.rebuild(1, stale, options(), &Cancellation::default()),
            Err(IndexError::StaleScope)
        ));
        let (fresh, _) = index.rebuild(1, fresh, options(), &Cancellation::default()).unwrap();
        assert_eq!(index.search(1, fresh, "second").unwrap(), [b"second".as_slice()]);
        assert!(index.search(1, fresh, "first").unwrap().is_empty());
    }
}
