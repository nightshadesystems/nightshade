//! The commit archive.
//!
//! Every confirmed commit writes a revision: the whole configuration as it
//! was left, in curly-brace format, gzipped, next to a sidecar naming who did
//! it, when, why, and what changed.
//!
//! # Why the config and not the diff
//!
//! A chain of diffs is smaller and is the wrong thing to keep. Rolling back to
//! revision 3 by replaying diffs means every revision between here and there
//! has to be intact and has to apply cleanly; one corrupt file in the middle
//! takes out everything older than it. A revision that holds the whole
//! configuration can be read on its own, by `zcat`, by somebody who has never
//! heard of Nightshade.
//!
//! # Curly, not JSON
//!
//! Same reason. The archive is where an operator goes when something has gone
//! wrong, and what they find there should be the format they already know how
//! to read -- the same bytes `save` would have written.
//!
//! # Reproducible bytes
//!
//! gzip records a modification time in its header. Left to itself that makes
//! two archives of an identical configuration differ, which would defeat
//! comparing them. It is set to zero; the real time is in the filename and the
//! sidecar, where something can be done with it.

use std::path::PathBuf;

use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use nightshade_proto::message::RevisionInfo;
use nightshade_schema::config::ConfigTree;
use nightshade_schema::curly;
use nightshade_schema::model::Schema;
use tracing::{info, warn};

use std::io::Read;

/// Revisions kept before the oldest are pruned.
pub const KEEP: usize = nightshade_common::ARCHIVE_KEEP;

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("{action} {path}: {source}")]
    Io {
        action: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("revision {revision} is not in the archive")]
    NoSuchRevision { revision: u64 },

    #[error("revision {revision} is damaged: {reason}")]
    Damaged { revision: u64, reason: String },
}

fn io(action: &'static str, path: &std::path::Path) -> impl FnOnce(std::io::Error) -> ArchiveError {
    let path = path.display().to_string();
    move |source| ArchiveError::Io {
        action,
        path,
        source,
    }
}

pub struct Archive {
    dir: PathBuf,
}

impl Archive {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// Record a revision, then prune the oldest beyond [`KEEP`].
    pub fn write(
        &self,
        info: &RevisionInfo,
        config: &ConfigTree,
        schema: &Schema,
    ) -> Result<(), ArchiveError> {
        std::fs::create_dir_all(&self.dir).map_err(io("creating", &self.dir))?;

        let stem = format!("{:06}-{}", info.revision, info.timestamp);

        // The sidecar first. A revision whose config is missing is a gap in
        // the history; one whose metadata is missing is a file nobody can
        // identify, and the listing would not show it at all.
        let config_path = self.dir.join(format!("{stem}.boot.gz"));
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        std::io::Write::write_all(&mut encoder, curly::render(config, schema).as_bytes())
            .map_err(io("compressing", &config_path))?;
        let compressed = encoder.finish().map_err(io("compressing", &config_path))?;
        std::fs::write(&config_path, compressed).map_err(io("writing", &config_path))?;

        let meta_path = self.dir.join(format!("{stem}.meta.json"));
        let encoded = serde_json::to_vec_pretty(info).expect("revision metadata always serialises");
        std::fs::write(&meta_path, encoded).map_err(io("writing", &meta_path))?;

        let pruned = self.prune()?;
        if pruned > 0 {
            info!(pruned, "pruned old archive revisions");
        }
        Ok(())
    }

    /// Every revision, newest first.
    ///
    /// Unreadable sidecars are skipped with a warning rather than failing the
    /// listing: one damaged file should not make the whole history
    /// unreadable, which is precisely when somebody needs it.
    pub fn list(&self) -> Result<Vec<RevisionInfo>, ArchiveError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io("reading", &self.dir)(e)),
        };

        let mut revisions: Vec<RevisionInfo> = Vec::new();
        for entry in entries {
            let path = entry.map_err(io("reading", &self.dir))?.path();
            if !path.to_string_lossy().ends_with(".meta.json") {
                continue;
            }
            match std::fs::read(&path).map(|bytes| serde_json::from_slice(&bytes)) {
                Ok(Ok(info)) => revisions.push(info),
                Ok(Err(e)) => warn!(path = %path.display(), error = %e, "skipping a damaged revision"),
                Err(e) => warn!(path = %path.display(), error = %e, "skipping an unreadable revision"),
            }
        }

        // By revision, not by filename: the sequence is the ordering, and a
        // filename comparison would go wrong the first time the number gained
        // a digit.
        revisions.sort_by_key(|info| std::cmp::Reverse(info.revision));
        Ok(revisions)
    }

    /// The configuration a revision holds.
    pub fn read(&self, revision: u64, schema: &Schema) -> Result<ConfigTree, ArchiveError> {
        let info = self
            .list()?
            .into_iter()
            .find(|info| info.revision == revision)
            .ok_or(ArchiveError::NoSuchRevision { revision })?;

        let path = self
            .dir
            .join(format!("{:06}-{}.boot.gz", info.revision, info.timestamp));

        let compressed = std::fs::read(&path).map_err(io("reading", &path))?;
        let mut text = String::new();
        GzDecoder::new(compressed.as_slice())
            .read_to_string(&mut text)
            .map_err(|e| ArchiveError::Damaged {
                revision,
                reason: e.to_string(),
            })?;

        let config = curly::parse(&text).map_err(|e| ArchiveError::Damaged {
            revision,
            reason: e.to_string(),
        })?;

        // An archived revision was valid when it was written. If it is not
        // valid now, the schema moved under it -- which is worth saying,
        // because the operator is about to be handed a candidate that will not
        // commit.
        let violations = schema.validate_tree(&config);
        if !violations.is_empty() {
            warn!(
                revision,
                count = violations.len(),
                "an archived revision no longer matches the schema"
            );
        }

        Ok(config)
    }

    /// Remove everything older than the newest [`KEEP`] revisions.
    fn prune(&self) -> Result<usize, ArchiveError> {
        let revisions = self.list()?;
        let mut pruned = 0;
        for info in revisions.into_iter().skip(KEEP) {
            let stem = format!("{:06}-{}", info.revision, info.timestamp);
            for extension in ["boot.gz", "meta.json"] {
                let path = self.dir.join(format!("{stem}.{extension}"));
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(io("removing", &path)(e)),
                }
            }
            pruned += 1;
        }
        Ok(pruned)
    }
}

/// `YYYYMMDDTHHMMSSZ`, UTC.
///
/// Sorts lexically in time order, has no characters that need quoting in a
/// filename, and is unambiguous on a box whose time zone somebody is about to
/// change. Built from the parts rather than a format string so that what ends
/// up in a filename is visible here.
pub fn stamp() -> String {
    let now = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
    let date = now.date();
    let time = now.time();
    format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        date.year(),
        date.month(),
        date.day(),
        time.hour(),
        time.minute(),
        time.second(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightshade_schema::path::Path;

    fn schema() -> &'static Schema {
        Schema::compiled()
    }

    fn config(host_name: &str) -> ConfigTree {
        let mut tree = ConfigTree::new();
        tree.set(&Path::parse("system host-name").unwrap(), host_name)
            .unwrap();
        tree
    }

    fn info(revision: u64) -> RevisionInfo {
        RevisionInfo {
            revision,
            timestamp: format!("2026081{revision:01}T120000Z"),
            actor: "nightshade".into(),
            actor_uid: 1000,
            comment: Some(format!("change {revision}")),
            changes: Vec::new(),
        }
    }

    #[test]
    fn a_revision_survives_a_write_and_a_read() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path().to_path_buf());

        let original = config("fw-01");
        archive.write(&info(1), &original, schema()).unwrap();

        assert_eq!(archive.read(1, schema()).unwrap(), original);
    }

    #[test]
    fn the_stored_config_is_readable_curly_text() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path().to_path_buf());
        archive.write(&info(1), &config("fw-01"), schema()).unwrap();

        // Anyone with zcat can read it, which is the point of storing the
        // format an operator already knows.
        let compressed = std::fs::read(dir.path().join("000001-20260811T120000Z.boot.gz")).unwrap();
        let mut text = String::new();
        GzDecoder::new(compressed.as_slice())
            .read_to_string(&mut text)
            .unwrap();
        assert!(text.contains("host-name fw-01"), "{text}");
    }

    #[test]
    fn identical_configurations_archive_to_identical_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path().to_path_buf());
        archive.write(&info(1), &config("fw"), schema()).unwrap();
        archive.write(&info(2), &config("fw"), schema()).unwrap();

        let one = std::fs::read(dir.path().join("000001-20260811T120000Z.boot.gz")).unwrap();
        let two = std::fs::read(dir.path().join("000002-20260812T120000Z.boot.gz")).unwrap();
        assert_eq!(one, two, "gzip recorded something that varies between runs");
    }

    #[test]
    fn revisions_list_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path().to_path_buf());
        for revision in 1..=3 {
            archive
                .write(&info(revision), &config(&format!("fw-{revision}")), schema())
                .unwrap();
        }

        let listed: Vec<u64> = archive.list().unwrap().iter().map(|i| i.revision).collect();
        assert_eq!(listed, [3, 2, 1]);
        assert_eq!(archive.list().unwrap()[0].comment.as_deref(), Some("change 3"));
    }

    #[test]
    fn the_oldest_revisions_are_pruned() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path().to_path_buf());

        for revision in 1..=(KEEP as u64 + 5) {
            let mut info = info(revision);
            info.timestamp = format!("2026{revision:04}T120000Z");
            archive.write(&info, &config("fw"), schema()).unwrap();
        }

        let listed = archive.list().unwrap();
        assert_eq!(listed.len(), KEEP);
        assert_eq!(listed[0].revision, KEEP as u64 + 5);
        assert_eq!(listed[KEEP - 1].revision, 6);

        // Both files went, not just the metadata.
        let files = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(files, KEEP * 2);
    }

    #[test]
    fn an_unknown_revision_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path().to_path_buf());
        assert!(matches!(
            archive.read(42, schema()),
            Err(ArchiveError::NoSuchRevision { revision: 42 })
        ));
    }

    /// One damaged revision must not make the rest of the history unreadable.
    #[test]
    fn a_damaged_revision_does_not_hide_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path().to_path_buf());
        archive.write(&info(1), &config("fw-01"), schema()).unwrap();
        archive.write(&info(2), &config("fw-02"), schema()).unwrap();

        std::fs::write(dir.path().join("000002-20260812T120000Z.meta.json"), b"{ broken").unwrap();

        let listed = archive.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].revision, 1);
        assert_eq!(archive.read(1, schema()).unwrap(), config("fw-01"));
    }

    #[test]
    fn a_truncated_config_is_reported_as_damaged() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path().to_path_buf());
        archive.write(&info(1), &config("fw-01"), schema()).unwrap();
        std::fs::write(dir.path().join("000001-20260811T120000Z.boot.gz"), b"not gzip").unwrap();

        assert!(matches!(
            archive.read(1, schema()),
            Err(ArchiveError::Damaged { revision: 1, .. })
        ));
    }

    #[test]
    fn an_empty_archive_lists_nothing_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let archive = Archive::new(dir.path().join("never-created"));
        assert!(archive.list().unwrap().is_empty());
    }

    #[test]
    fn the_stamp_is_sortable_and_unambiguous() {
        let stamp = stamp();
        assert_eq!(stamp.len(), 16, "{stamp}");
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert!(stamp[..8].chars().all(|c| c.is_ascii_digit()), "{stamp}");
        assert_eq!(&stamp[8..9], "T", "{stamp}");
    }
}
