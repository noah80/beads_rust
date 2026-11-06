//! Per-issue git tree projection of the JSONL export.
//!
//! When `metadata.json` names an `issue_tree` directory (for example
//! `"issue_tree": "issues"`), the JSONL export stays the local file that every
//! sync safety mechanism works on, but git tracks one file per issue instead:
//! `.beads/issues/<id>.jsonl`, holding exactly that issue's export line.
//! Branches that touch different issues then never edit the same file, so a
//! hosted merge (which ignores custom merge drivers) no longer reports
//! conflicts between neighbouring JSONL lines.
//!
//! Two directions keep the projection and the JSONL in step:
//!
//! * **split** writes the JSONL rows into the tree after `br` published a new
//!   export;
//! * **rebuild** assembles the tree into the JSONL when git changed the tree
//!   (pull, merge, checkout), so the ordinary staleness probe and import treat
//!   it exactly like a pulled `issues.jsonl`.
//!
//! A local state file records stat fingerprints of both sides as of the last
//! reconciliation, and only the side whose fingerprint moved is propagated.
//! When both moved and their contents differ, nothing is overwritten and the
//! caller receives [`IssueTreeOutcome::Diverged`].

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::history::{self, HistoryConfig};
use super::path::capture_optional_jsonl_source_snapshot;
use super::{
    ExclusiveLockMechanism, ExportConfig, JsonlSourceSnapshot, JsonlSourceStateWitness,
    LockRetrySafety, LockWaitPolicy, TempFileGuard, blocking_jsonl_family_write_lock_with_timeout,
    compute_jsonl_hash, compute_jsonl_hash_from_reader,
    create_full_export_temp_file_under_authority, ensure_no_conflict_markers_snapshot,
    open_and_lock_regular_file, publish_staged_jsonl_conditionally,
    validate_jsonl_snapshot_issue_records, validate_sync_path_with_external,
};
use crate::error::{BeadsError, Result};
use crate::util::{hex_encode, sync_directory_best_effort};

/// Local (gitignored) record of the fingerprints last reconciled.
pub const ISSUE_TREE_STATE_FILE: &str = ".issue-tree-state.json";
/// Lock sidecar serializing reconciliations between `br` processes.
pub const ISSUE_TREE_LOCK_FILE: &str = ".issue-tree.lock";

const ENTRY_EXTENSION: &str = "jsonl";
const STATE_VERSION: u32 = 1;
const LOCK_TIMEOUT_MS: u64 = 30_000;
const MAX_ID_LEN: usize = 200;
const CAPTURE_ATTEMPTS: usize = 3;

/// Which directions a reconciliation may propagate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueTreeScope {
    /// Before a command reads issues: rebuild the JSONL from a changed tree,
    /// or split a JSONL that changed without being split.
    Startup,
    /// After a command ran: only split a JSONL that the command published.
    AfterCommand,
}

/// What a reconciliation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueTreeOutcome {
    /// Both sides match the recorded state; nothing was written.
    Unchanged,
    /// The JSONL rows were written into the tree.
    Split { written: usize, removed: usize },
    /// The JSONL was rebuilt from the tree.
    Rebuilt { issues: usize },
    /// Both sides changed but hold the same rows; only the state was recorded.
    Adopted,
    /// Both sides changed to different rows; nothing was overwritten.
    Diverged {
        tree_dir: PathBuf,
        jsonl_path: PathBuf,
    },
}

impl IssueTreeOutcome {
    /// Operator guidance for a [`IssueTreeOutcome::Diverged`] outcome.
    #[must_use]
    pub fn diverged_message(&self) -> Option<String> {
        let Self::Diverged {
            tree_dir,
            jsonl_path,
        } = self
        else {
            return None;
        };
        Some(format!(
            "both {jsonl} and the issue tree {tree} changed since they were last reconciled, \
             and they hold different issues; br left both untouched. To keep the tree \
             (for example after a git pull), move {jsonl} aside and rerun; br rebuilds it from \
             the tree and imports it. To keep the JSONL instead, move the tree directory aside \
             and rerun; br writes the tree from the JSONL.",
            jsonl = jsonl_path.display(),
            tree = tree_dir.display(),
        ))
    }
}

/// Inputs of one reconciliation.
#[derive(Debug, Clone)]
pub struct IssueTreeContext<'a> {
    pub beads_dir: &'a Path,
    pub jsonl_path: &'a Path,
    /// Directory name below `beads_dir`, from `metadata.json` `issue_tree`.
    pub tree_name: &'a str,
    /// History knobs for the backup taken before a rebuild replaces the JSONL.
    pub history: HistoryConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct TreeState {
    version: u32,
    /// Fingerprint of the JSONL, `None` when it was missing.
    jsonl: Option<String>,
    /// Fingerprint of the tree, `None` when the directory was missing.
    tree: Option<String>,
    /// JSONL fingerprint of a split that started but did not finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_split: Option<String>,
}

#[derive(Debug, Deserialize)]
struct EntryId {
    id: String,
}

/// Validate an `issue_tree` directory name and resolve it below `beads_dir`.
///
/// # Errors
///
/// Returns an error when the name is not a single plain directory name or the
/// JSONL export does not live directly in `beads_dir`.
pub fn resolve_issue_tree_dir(beads_dir: &Path, jsonl_path: &Path, name: &str) -> Result<PathBuf> {
    let plain = !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_');
    if !plain {
        return Err(BeadsError::Config(format!(
            "metadata.json issue_tree must be a plain directory name (letters, digits, '-', '_'), got {name:?}"
        )));
    }
    if jsonl_path.parent() != Some(beads_dir) {
        return Err(BeadsError::Config(format!(
            "metadata.json issue_tree requires the JSONL export directly inside {}, but it resolves to {}",
            beads_dir.display(),
            jsonl_path.display()
        )));
    }
    Ok(beads_dir.join(name))
}

/// Reconcile the JSONL export with its per-issue tree.
///
/// # Errors
///
/// Returns an error when the tree holds an unreadable, conflicted, or
/// mislabelled entry, when an issue id cannot be a file name, or when a file
/// system operation fails. A failed reconciliation leaves the recorded state
/// unchanged, so the next run retries it.
pub fn reconcile_issue_tree(
    ctx: &IssueTreeContext<'_>,
    scope: IssueTreeScope,
) -> Result<IssueTreeOutcome> {
    let tree_dir = resolve_issue_tree_dir(ctx.beads_dir, ctx.jsonl_path, ctx.tree_name)?;
    let _lock = open_and_lock_regular_file(
        &ctx.beads_dir.join(ISSUE_TREE_LOCK_FILE),
        Some(LOCK_TIMEOUT_MS),
        true,
        "issue-tree lock",
        false,
        ExclusiveLockMechanism::LockSidecar,
        LockWaitPolicy::new(LockRetrySafety::BeforeMutation, None),
    )?;
    let state_path = ctx.beads_dir.join(ISSUE_TREE_STATE_FILE);
    let state = load_state(&state_path);
    let jsonl_now = file_fingerprint(ctx.jsonl_path)?;

    // A split interrupted after it started rewriting the tree: the tree
    // changes are ours, so finish it instead of reporting a divergence.
    if let Some(state) = &state
        && state.pending_split.is_some()
        && state.pending_split == jsonl_now
    {
        return split(ctx, &tree_dir, &state_path);
    }

    let jsonl_changed = state.as_ref().is_none_or(|state| state.jsonl != jsonl_now);
    if scope == IssueTreeScope::AfterCommand && (!jsonl_changed || jsonl_now.is_none()) {
        return Ok(IssueTreeOutcome::Unchanged);
    }
    let tree_now = tree_fingerprint(&tree_dir)?;
    let tree_changed = state.as_ref().is_none_or(|state| state.tree != tree_now);

    match (jsonl_now.is_some(), tree_now.is_some()) {
        (false, false) => {
            save_state(&state_path, &recorded(None, None))?;
            Ok(IssueTreeOutcome::Unchanged)
        }
        // First split after enabling the tree, or the tree was removed.
        (true, false) => split(ctx, &tree_dir, &state_path),
        // Fresh clone (the JSONL is not tracked) or the JSONL was removed.
        (false, true) => {
            if scope == IssueTreeScope::Startup {
                rebuild(ctx, &tree_dir, &state_path)
            } else {
                Ok(IssueTreeOutcome::Unchanged)
            }
        }
        (true, true) => match (jsonl_changed, tree_changed) {
            (false, false) => Ok(IssueTreeOutcome::Unchanged),
            (true, false) => split(ctx, &tree_dir, &state_path),
            (false, true) => {
                if scope == IssueTreeScope::Startup {
                    rebuild(ctx, &tree_dir, &state_path)
                } else {
                    Ok(IssueTreeOutcome::Unchanged)
                }
            }
            (true, true) => adopt_or_diverge(ctx, &tree_dir, &state_path),
        },
    }
}

fn adopt_or_diverge(
    ctx: &IssueTreeContext<'_>,
    tree_dir: &Path,
    state_path: &Path,
) -> Result<IssueTreeOutcome> {
    let (tree_before, assembled, _) = stable_tree_assembly(tree_dir)?;
    let assembled_hash = compute_jsonl_hash_from_reader(std::io::Cursor::new(&assembled))?;
    let jsonl_before = file_fingerprint(ctx.jsonl_path)?;
    let jsonl_hash = compute_jsonl_hash(ctx.jsonl_path)?;
    if assembled_hash != jsonl_hash || file_fingerprint(ctx.jsonl_path)? != jsonl_before {
        return Ok(IssueTreeOutcome::Diverged {
            tree_dir: tree_dir.to_path_buf(),
            jsonl_path: ctx.jsonl_path.to_path_buf(),
        });
    }
    save_state(state_path, &recorded(jsonl_before, tree_before))?;
    Ok(IssueTreeOutcome::Adopted)
}

fn recorded(jsonl: Option<String>, tree: Option<String>) -> TreeState {
    TreeState {
        version: STATE_VERSION,
        jsonl,
        tree,
        pending_split: None,
    }
}

// ---------------------------------------------------------------------------
// split: JSONL -> tree
// ---------------------------------------------------------------------------

fn split(
    ctx: &IssueTreeContext<'_>,
    tree_dir: &Path,
    state_path: &Path,
) -> Result<IssueTreeOutcome> {
    for _ in 0..CAPTURE_ATTEMPTS {
        let before = file_fingerprint(ctx.jsonl_path)?;
        let source = capture_optional_jsonl_source_snapshot(ctx.jsonl_path)?;
        if file_fingerprint(ctx.jsonl_path)? != before {
            continue;
        }
        let Some(source) = source else {
            return Ok(IssueTreeOutcome::Unchanged);
        };
        let entries = entries_from_jsonl(&source)?;

        save_state(
            state_path,
            &TreeState {
                pending_split: before.clone(),
                ..load_state(state_path).unwrap_or_else(|| recorded(None, None))
            },
        )?;
        let (written, removed) = write_tree(tree_dir, &entries)?;
        save_state(state_path, &recorded(before, tree_fingerprint(tree_dir)?))?;
        return Ok(IssueTreeOutcome::Split { written, removed });
    }
    Err(BeadsError::SyncConflict {
        message: format!(
            "{} kept changing while br wrote the issue tree; rerun the command",
            ctx.jsonl_path.display()
        ),
    })
}

/// Map each JSONL row to its tree entry (`<id>.jsonl` -> line + newline).
fn entries_from_jsonl(source: &JsonlSourceSnapshot) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut entries = BTreeMap::new();
    let mut folded_names = HashSet::new();
    let mut reader = source.reader();
    let mut line = Vec::with_capacity(4096);
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        let row = line.trim_ascii();
        if row.is_empty() {
            continue;
        }
        let EntryId { id } = serde_json::from_slice(row).map_err(|error| {
            BeadsError::Config(format!(
                "cannot split {} into the issue tree: unreadable row: {error}",
                source.display_path().display()
            ))
        })?;
        validate_entry_id(&id)?;
        let name = format!("{id}.{ENTRY_EXTENSION}");
        // Case-insensitive file systems (macOS, Windows) would merge two ids
        // that differ only in case into one file.
        if !folded_names.insert(name.to_ascii_lowercase()) {
            return Err(BeadsError::Config(format!(
                "cannot split the JSONL into the issue tree: issue id {id} collides with another id \
                 that differs only in letter case, or appears twice"
            )));
        }
        let mut content = Vec::with_capacity(row.len() + 1);
        content.extend_from_slice(row);
        content.push(b'\n');
        entries.insert(name, content);
    }
    Ok(entries)
}

fn validate_entry_id(id: &str) -> Result<()> {
    let safe = !id.is_empty()
        && id.len() <= MAX_ID_LEN
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if safe {
        Ok(())
    } else {
        Err(BeadsError::Config(format!(
            "issue id {id:?} cannot be stored in the issue tree: ids must be at most \
             {MAX_ID_LEN} characters of letters, digits, '-', '_' or '.', not starting with '.'"
        )))
    }
}

/// Make the tree hold exactly `entries`, rewriting only differing files.
fn write_tree(tree_dir: &Path, entries: &BTreeMap<String, Vec<u8>>) -> Result<(usize, usize)> {
    match fs::symlink_metadata(tree_dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(BeadsError::Config(format!(
                "issue tree {} is not a directory",
                tree_dir.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(tree_dir)?;
        }
        Err(error) => return Err(error.into()),
    }
    let existing = list_entries(tree_dir)?;

    let mut written = 0;
    for (name, content) in entries {
        let path = tree_dir.join(name);
        let unchanged = match fs::read(&path) {
            Ok(current) => current == *content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.into()),
        };
        if !unchanged {
            write_entry(tree_dir, name, content)?;
            written += 1;
        }
    }
    let mut removed = 0;
    for name in existing.keys() {
        if !entries.contains_key(name) {
            fs::remove_file(tree_dir.join(name))?;
            removed += 1;
        }
    }
    if written + removed > 0 {
        sync_directory_best_effort(tree_dir)?;
    }
    Ok((written, removed))
}

fn write_entry(tree_dir: &Path, name: &str, content: &[u8]) -> Result<()> {
    let temp_path = tree_dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let mut temp = File::create(&temp_path)?;
    let written = temp.write_all(content).and_then(|()| temp.sync_all());
    drop(temp);
    if let Err(error) = written.and_then(|()| fs::rename(&temp_path, tree_dir.join(name))) {
        let _ = fs::remove_file(&temp_path);
        return Err(error.into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// rebuild: tree -> JSONL
// ---------------------------------------------------------------------------

fn rebuild(
    ctx: &IssueTreeContext<'_>,
    tree_dir: &Path,
    state_path: &Path,
) -> Result<IssueTreeOutcome> {
    let (tree_before, assembled, issues) = stable_tree_assembly(tree_dir)?;
    let current_hash = match file_fingerprint(ctx.jsonl_path)? {
        Some(_) => Some(compute_jsonl_hash(ctx.jsonl_path)?),
        None => None,
    };
    let assembled_hash = compute_jsonl_hash_from_reader(std::io::Cursor::new(&assembled))?;
    if current_hash.as_deref() != Some(assembled_hash.as_str()) {
        publish_rebuilt_jsonl(ctx, &assembled)?;
    }
    save_state(
        state_path,
        &recorded(file_fingerprint(ctx.jsonl_path)?, tree_before),
    )?;
    Ok(IssueTreeOutcome::Rebuilt { issues })
}

/// Assemble the tree, retrying while a concurrent writer (git) changes it.
fn stable_tree_assembly(tree_dir: &Path) -> Result<(Option<String>, Vec<u8>, usize)> {
    for _ in 0..CAPTURE_ATTEMPTS {
        let before = tree_fingerprint(tree_dir)?;
        let (assembled, issues) = assemble_tree(tree_dir)?;
        if tree_fingerprint(tree_dir)? == before {
            return Ok((before, assembled, issues));
        }
    }
    Err(BeadsError::SyncConflict {
        message: format!(
            "issue tree {} kept changing while br read it; rerun the command",
            tree_dir.display()
        ),
    })
}

/// Concatenate the tree entries into JSONL bytes sorted by issue id.
fn assemble_tree(tree_dir: &Path) -> Result<(Vec<u8>, usize)> {
    let mut rows = BTreeMap::new();
    for name in list_entries(tree_dir)?.into_keys() {
        let path = tree_dir.join(&name);
        let bytes = fs::read(&path)?;
        let row = bytes.trim_ascii();
        if has_conflict_markers(&bytes) {
            return Err(BeadsError::Config(format!(
                "issue tree entry {} contains git merge conflict markers; resolve the conflict \
                 (keep one complete JSON line) and rerun",
                path.display()
            )));
        }
        if row.is_empty() || row.contains(&b'\n') {
            return Err(BeadsError::Config(format!(
                "issue tree entry {} must hold exactly one JSON line",
                path.display()
            )));
        }
        let EntryId { id } = serde_json::from_slice(row).map_err(|error| {
            BeadsError::Config(format!(
                "issue tree entry {} is not a valid issue record: {error}",
                path.display()
            ))
        })?;
        let expected_name = format!("{id}.{ENTRY_EXTENSION}");
        if expected_name != name {
            return Err(BeadsError::Config(format!(
                "issue tree entry {} holds issue {id}; it must be named {expected_name}",
                path.display()
            )));
        }
        rows.insert(id, row.to_vec());
    }
    let issues = rows.len();
    let mut assembled = Vec::new();
    for row in rows.into_values() {
        assembled.extend_from_slice(&row);
        assembled.push(b'\n');
    }
    Ok((assembled, issues))
}

fn has_conflict_markers(bytes: &[u8]) -> bool {
    bytes.split(|&byte| byte == b'\n').any(|line| {
        line.starts_with(b"<<<<<<< ")
            || line.starts_with(b">>>>>>> ")
            || line.trim_ascii_end() == b"======="
            || line.starts_with(b"||||||| ")
    })
}

/// Publish rebuilt JSONL bytes through the same conditional, backed-up
/// replacement protocol as a full export.
fn publish_rebuilt_jsonl(ctx: &IssueTreeContext<'_>, assembled: &[u8]) -> Result<()> {
    validate_sync_path_with_external(ctx.jsonl_path, ctx.beads_dir, false)?;
    let authority =
        blocking_jsonl_family_write_lock_with_timeout(ctx.jsonl_path, Some(LOCK_TIMEOUT_MS))?;
    let previous = authority.capture_optional_target()?;
    let export_config = ExportConfig {
        beads_dir: Some(ctx.beads_dir.to_path_buf()),
        ..ExportConfig::default()
    };
    let (temp_path, pinned_temp, mut temp_file) =
        create_full_export_temp_file_under_authority(ctx.jsonl_path, &export_config, &authority)?;
    let mut temp_guard = TempFileGuard::new(temp_path.clone());
    temp_file.write_all(assembled)?;
    temp_file.sync_all()?;
    drop(temp_file);

    let staged = pinned_temp.capture()?;
    ensure_no_conflict_markers_snapshot(&staged)?;
    let validation = validate_jsonl_snapshot_issue_records(&staged)?;
    if validation.invalid_count != 0 {
        return Err(BeadsError::Config(format!(
            "the issue tree holds {} invalid issue record(s); run `br doctor` after fixing the tree entries",
            validation.invalid_count
        )));
    }

    if let Some(previous) = previous.as_ref() {
        history::backup_before_export_snapshot(
            ctx.beads_dir,
            &ctx.history,
            ctx.jsonl_path,
            previous,
        )?;
    }
    let expected_previous = previous.as_ref().map_or(
        JsonlSourceStateWitness::Missing,
        JsonlSourceSnapshot::state_witness,
    );
    temp_guard.persist();
    publish_staged_jsonl_conditionally(
        &temp_path,
        temp_guard,
        ctx.jsonl_path,
        &staged,
        &expected_previous,
        staged.content_sha256(),
        &authority,
        None,
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// fingerprints and state
// ---------------------------------------------------------------------------

/// List the regular `*.jsonl` entries of the tree, refusing symlinks.
fn list_entries(tree_dir: &Path) -> Result<BTreeMap<String, fs::Metadata>> {
    let mut entries = BTreeMap::new();
    for entry in fs::read_dir(tree_dir)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with('.')
            || Path::new(&name).extension().and_then(|ext| ext.to_str()) != Some(ENTRY_EXTENSION)
        {
            continue;
        }
        let metadata = fs::symlink_metadata(entry.path())?;
        if !metadata.is_file() {
            return Err(BeadsError::Config(format!(
                "issue tree entry {} is not a regular file",
                entry.path().display()
            )));
        }
        entries.insert(name, metadata);
    }
    Ok(entries)
}

fn metadata_fingerprint(metadata: &fs::Metadata) -> String {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        format!(
            "{}:{}.{}:{}.{}:{}:{}",
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
            metadata.dev(),
            metadata.ino()
        )
    }
    #[cfg(not(unix))]
    {
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |duration| duration.as_nanos());
        format!("{}:{modified}", metadata.len())
    }
}

fn file_fingerprint(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(Some(metadata_fingerprint(&metadata))),
        Ok(_) => Err(BeadsError::Config(format!(
            "{} is not a regular file",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn tree_fingerprint(tree_dir: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(tree_dir) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(BeadsError::Config(format!(
                "issue tree {} is not a directory",
                tree_dir.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut hasher = Sha256::new();
    for (name, metadata) in list_entries(tree_dir)? {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hasher.update(metadata_fingerprint(&metadata).as_bytes());
        hasher.update(b"\n");
    }
    Ok(Some(hex_encode(&hasher.finalize())))
}

fn load_state(state_path: &Path) -> Option<TreeState> {
    let bytes = fs::read(state_path).ok()?;
    serde_json::from_slice::<TreeState>(&bytes)
        .ok()
        .filter(|state| state.version == STATE_VERSION)
}

fn save_state(state_path: &Path, state: &TreeState) -> Result<()> {
    let parent = state_path.parent().ok_or_else(|| {
        BeadsError::Config(format!(
            "invalid issue-tree state path {}",
            state_path.display()
        ))
    })?;
    let temp_path = parent.join(format!(
        "{ISSUE_TREE_STATE_FILE}.{}.tmp",
        std::process::id()
    ));
    let bytes = serde_json::to_vec_pretty(state)?;
    let mut temp = File::create(&temp_path)?;
    let written = temp.write_all(&bytes).and_then(|()| temp.sync_all());
    drop(temp);
    if let Err(error) = written.and_then(|()| fs::rename(&temp_path, state_path)) {
        let _ = fs::remove_file(&temp_path);
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const ROW_A: &str = r#"{"id":"bd-a1","title":"first","status":"open","priority":2,"issue_type":"task","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}"#;
    const ROW_B: &str = r#"{"id":"bd-b2","title":"second","status":"open","priority":2,"issue_type":"task","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}"#;
    const ROW_B_CLOSED: &str = r#"{"id":"bd-b2","title":"second","status":"closed","priority":2,"issue_type":"task","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z","closed_at":"2026-01-02T00:00:00Z"}"#;
    const ROW_C: &str = r#"{"id":"bd-c3","title":"third","status":"open","priority":2,"issue_type":"task","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}"#;

    struct Fixture {
        _temp: TempDir,
        beads_dir: PathBuf,
        jsonl_path: PathBuf,
        tree_dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let temp = TempDir::new().expect("tempdir");
            let beads_dir = temp.path().join(".beads");
            fs::create_dir_all(&beads_dir).expect("beads dir");
            let jsonl_path = beads_dir.join("issues.jsonl");
            let tree_dir = beads_dir.join("issues");
            Self {
                _temp: temp,
                beads_dir,
                jsonl_path,
                tree_dir,
            }
        }

        fn ctx(&self) -> IssueTreeContext<'_> {
            IssueTreeContext {
                beads_dir: &self.beads_dir,
                jsonl_path: &self.jsonl_path,
                tree_name: "issues",
                history: HistoryConfig {
                    enabled: false,
                    ..HistoryConfig::default()
                },
            }
        }

        fn reconcile(&self, scope: IssueTreeScope) -> IssueTreeOutcome {
            reconcile_issue_tree(&self.ctx(), scope).expect("reconcile")
        }

        fn write_jsonl(&self, rows: &[&str]) {
            let mut content = rows.join("\n");
            content.push('\n');
            // Replace by rename, as the exporter does, so the fingerprint moves.
            let temp = self.beads_dir.join("issues.jsonl.test.tmp");
            fs::write(&temp, content).expect("write jsonl");
            fs::rename(&temp, &self.jsonl_path).expect("publish jsonl");
        }

        fn tree_entry(&self, id: &str) -> Option<String> {
            fs::read_to_string(self.tree_dir.join(format!("{id}.jsonl"))).ok()
        }

        fn jsonl(&self) -> String {
            fs::read_to_string(&self.jsonl_path).expect("read jsonl")
        }
    }

    #[test]
    fn first_split_writes_one_file_per_issue() {
        let fx = Fixture::new();
        fx.write_jsonl(&[ROW_A, ROW_B]);
        assert_eq!(
            fx.reconcile(IssueTreeScope::Startup),
            IssueTreeOutcome::Split {
                written: 2,
                removed: 0
            }
        );
        assert_eq!(
            fx.tree_entry("bd-a1").as_deref(),
            Some(&*format!("{ROW_A}\n"))
        );
        assert_eq!(
            fx.tree_entry("bd-b2").as_deref(),
            Some(&*format!("{ROW_B}\n"))
        );
        assert_eq!(
            fx.reconcile(IssueTreeScope::Startup),
            IssueTreeOutcome::Unchanged
        );
    }

    #[test]
    fn after_command_split_rewrites_only_changed_and_removed_entries() {
        let fx = Fixture::new();
        fx.write_jsonl(&[ROW_A, ROW_B]);
        fx.reconcile(IssueTreeScope::Startup);
        fx.write_jsonl(&[ROW_B_CLOSED, ROW_C]);
        assert_eq!(
            fx.reconcile(IssueTreeScope::AfterCommand),
            IssueTreeOutcome::Split {
                written: 2,
                removed: 1
            }
        );
        assert_eq!(fx.tree_entry("bd-a1"), None);
        assert_eq!(
            fx.tree_entry("bd-b2").as_deref(),
            Some(&*format!("{ROW_B_CLOSED}\n"))
        );
        assert_eq!(
            fx.tree_entry("bd-c3").as_deref(),
            Some(&*format!("{ROW_C}\n"))
        );
    }

    #[test]
    fn after_command_never_rebuilds_from_a_changed_tree() {
        let fx = Fixture::new();
        fx.write_jsonl(&[ROW_A]);
        fx.reconcile(IssueTreeScope::Startup);
        fs::write(fx.tree_dir.join("bd-b2.jsonl"), format!("{ROW_B}\n")).expect("add entry");
        assert_eq!(
            fx.reconcile(IssueTreeScope::AfterCommand),
            IssueTreeOutcome::Unchanged
        );
        assert_eq!(fx.jsonl(), format!("{ROW_A}\n"));
    }

    #[test]
    fn startup_rebuilds_jsonl_sorted_from_a_changed_tree() {
        let fx = Fixture::new();
        fx.write_jsonl(&[ROW_B]);
        fx.reconcile(IssueTreeScope::Startup);
        // A git merge adds a new issue file and edits an existing one.
        fs::write(fx.tree_dir.join("bd-a1.jsonl"), format!("{ROW_A}\n")).expect("add entry");
        fs::write(fx.tree_dir.join("bd-b2.jsonl"), format!("{ROW_B_CLOSED}\n")).expect("edit");
        assert_eq!(
            fx.reconcile(IssueTreeScope::Startup),
            IssueTreeOutcome::Rebuilt { issues: 2 }
        );
        assert_eq!(fx.jsonl(), format!("{ROW_A}\n{ROW_B_CLOSED}\n"));
        assert_eq!(
            fx.reconcile(IssueTreeScope::Startup),
            IssueTreeOutcome::Unchanged
        );
    }

    #[test]
    fn fresh_clone_without_jsonl_rebuilds_it_from_the_tree() {
        let fx = Fixture::new();
        fs::create_dir(&fx.tree_dir).expect("tree");
        fs::write(fx.tree_dir.join("bd-c3.jsonl"), format!("{ROW_C}\n")).expect("entry");
        fs::write(fx.tree_dir.join("bd-a1.jsonl"), format!("{ROW_A}\n")).expect("entry");
        assert_eq!(
            fx.reconcile(IssueTreeScope::Startup),
            IssueTreeOutcome::Rebuilt { issues: 2 }
        );
        assert_eq!(fx.jsonl(), format!("{ROW_A}\n{ROW_C}\n"));
    }

    #[test]
    fn both_sides_changed_to_different_rows_is_left_untouched() {
        let fx = Fixture::new();
        fx.write_jsonl(&[ROW_A]);
        fx.reconcile(IssueTreeScope::Startup);
        fx.write_jsonl(&[ROW_A, ROW_C]);
        fs::write(fx.tree_dir.join("bd-b2.jsonl"), format!("{ROW_B}\n")).expect("entry");
        let outcome = fx.reconcile(IssueTreeScope::Startup);
        assert!(matches!(outcome, IssueTreeOutcome::Diverged { .. }));
        assert!(outcome.diverged_message().is_some());
        assert_eq!(fx.jsonl(), format!("{ROW_A}\n{ROW_C}\n"));
        assert_eq!(fx.tree_entry("bd-c3"), None);
        assert!(fx.tree_entry("bd-b2").is_some());
    }

    #[test]
    fn both_sides_matching_without_state_are_adopted() {
        let fx = Fixture::new();
        fx.write_jsonl(&[ROW_A, ROW_B]);
        fs::create_dir(&fx.tree_dir).expect("tree");
        fs::write(fx.tree_dir.join("bd-a1.jsonl"), format!("{ROW_A}\n")).expect("entry");
        fs::write(fx.tree_dir.join("bd-b2.jsonl"), format!("{ROW_B}\n")).expect("entry");
        assert_eq!(
            fx.reconcile(IssueTreeScope::Startup),
            IssueTreeOutcome::Adopted
        );
        assert_eq!(
            fx.reconcile(IssueTreeScope::Startup),
            IssueTreeOutcome::Unchanged
        );
    }

    #[test]
    fn conflicted_tree_entry_refuses_rebuild_and_keeps_state() {
        let fx = Fixture::new();
        fx.write_jsonl(&[ROW_B]);
        fx.reconcile(IssueTreeScope::Startup);
        fs::write(
            fx.tree_dir.join("bd-b2.jsonl"),
            format!("<<<<<<< HEAD\n{ROW_B}\n=======\n{ROW_B_CLOSED}\n>>>>>>> branch\n"),
        )
        .expect("conflict");
        let error = reconcile_issue_tree(&fx.ctx(), IssueTreeScope::Startup)
            .expect_err("conflict markers must refuse");
        assert!(error.to_string().contains("conflict markers"), "{error}");
        assert_eq!(fx.jsonl(), format!("{ROW_B}\n"));
        // Still refused on the next run: the failed rebuild recorded nothing.
        assert!(reconcile_issue_tree(&fx.ctx(), IssueTreeScope::Startup).is_err());
    }

    #[test]
    fn misnamed_tree_entry_is_refused() {
        let fx = Fixture::new();
        fs::create_dir(&fx.tree_dir).expect("tree");
        fs::write(fx.tree_dir.join("bd-zz.jsonl"), format!("{ROW_A}\n")).expect("entry");
        let error = reconcile_issue_tree(&fx.ctx(), IssueTreeScope::Startup)
            .expect_err("misnamed entry must refuse");
        assert!(
            error.to_string().contains("must be named bd-a1.jsonl"),
            "{error}"
        );
    }

    #[test]
    fn interrupted_split_is_finished_instead_of_diverging() {
        let fx = Fixture::new();
        fx.write_jsonl(&[ROW_A]);
        fx.reconcile(IssueTreeScope::Startup);
        fx.write_jsonl(&[ROW_A, ROW_B]);
        // Simulate a crash after the split marked itself pending and wrote
        // one unrelated-looking change into the tree.
        let state_path = fx.beads_dir.join(ISSUE_TREE_STATE_FILE);
        let mut state = load_state(&state_path).expect("state");
        state.pending_split = file_fingerprint(&fx.jsonl_path).expect("fingerprint");
        save_state(&state_path, &state).expect("save");
        fs::remove_file(fx.tree_dir.join("bd-a1.jsonl")).expect("partial split");
        assert_eq!(
            fx.reconcile(IssueTreeScope::Startup),
            IssueTreeOutcome::Split {
                written: 2,
                removed: 0
            }
        );
        assert_eq!(
            fx.reconcile(IssueTreeScope::Startup),
            IssueTreeOutcome::Unchanged
        );
    }

    #[test]
    fn unsafe_ids_and_case_collisions_are_refused() {
        assert!(validate_entry_id("bd-abc.1").is_ok());
        assert!(validate_entry_id("../escape").is_err());
        assert!(validate_entry_id(".hidden").is_err());
        assert!(validate_entry_id("").is_err());
        assert!(validate_entry_id(&"x".repeat(MAX_ID_LEN + 1)).is_err());

        let fx = Fixture::new();
        let upper = ROW_A.replace("bd-a1", "BD-A1");
        fx.write_jsonl(&[ROW_A, upper.as_str()]);
        let error = reconcile_issue_tree(&fx.ctx(), IssueTreeScope::Startup)
            .expect_err("case collision must refuse");
        assert!(error.to_string().contains("letter case"), "{error}");
    }

    #[test]
    fn tree_name_and_jsonl_location_are_validated() {
        let fx = Fixture::new();
        assert!(resolve_issue_tree_dir(&fx.beads_dir, &fx.jsonl_path, "issues").is_ok());
        for bad in ["", ".hidden", "a/b", "..", "issues.d"] {
            assert!(
                resolve_issue_tree_dir(&fx.beads_dir, &fx.jsonl_path, bad).is_err(),
                "{bad:?} must be refused"
            );
        }
        let outside = fx.beads_dir.parent().expect("parent").join("issues.jsonl");
        assert!(resolve_issue_tree_dir(&fx.beads_dir, &outside, "issues").is_err());
    }

    #[test]
    fn non_entry_files_in_the_tree_are_ignored() {
        let fx = Fixture::new();
        fx.write_jsonl(&[ROW_A]);
        fx.reconcile(IssueTreeScope::Startup);
        fs::write(fx.tree_dir.join("README.md"), "notes").expect("readme");
        fs::write(fx.tree_dir.join(".gitkeep"), "").expect("gitkeep");
        assert_eq!(
            fx.reconcile(IssueTreeScope::Startup),
            IssueTreeOutcome::Unchanged
        );
        fx.write_jsonl(&[ROW_B]);
        fx.reconcile(IssueTreeScope::AfterCommand);
        assert!(fx.tree_dir.join("README.md").exists());
        assert!(fx.tree_dir.join(".gitkeep").exists());
    }
}
