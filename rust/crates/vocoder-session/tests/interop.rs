//! Interop test against the committed dsh snapshot corpus:
//! Rust reads every generation under dsh/snapshots/session/**.

use std::path::PathBuf;
use vocoder_session::*;

fn snapshots_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../dsh/snapshots/session")
}

#[test]
fn parses_canonical_and_rejects_noncanonical_names() {
    assert_eq!(parse_generation_filename("session.jsonl"), Some(0));
    assert_eq!(parse_generation_filename("session.v2.jsonl"), Some(2));
    assert_eq!(parse_generation_filename("session.v3.jsonl.zstd"), Some(3));
    assert_eq!(parse_generation_filename("session.v03.jsonl"), None);
    assert_eq!(parse_generation_filename("Session.v2.jsonl"), None);
    assert_eq!(parse_generation_filename("session.tmp"), None);
    assert_eq!(parse_generation_filename("session.jsonl.tmp"), None);
}

#[test]
fn reads_full_generations_and_projections() {
    // Deep-read a couple of generations whole (not just headers).
    let root = snapshots_root();
    let mut checked = 0;
    for entry in std::fs::read_dir(&root).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        for v in list_generations(&dir).unwrap() {
            let path = generation_path(&dir, v).unwrap();
            let rows = read_generation(&path).unwrap();
            assert!(!rows.is_empty());
            // First row is always the session header with matching version.
            assert_eq!(rows[0]["type"], "session");
            assert_eq!(rows[0]["version"], v);
            checked += 1;
            if checked >= 12 {
                return;
            }
        }
    }
    assert!(checked >= 12);
}

#[test]
fn write_generation_is_atomic_and_immutable() {
    let dir = std::env::temp_dir().join(format!("vocoder-session-test-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();

    let rows = vec![
        serde_json::json!({"type":"session","version":2,"id":"s-1","createdAt":0}),
        serde_json::json!({"type":"turn/start"}),
    ];
    let written = write_generation(&dir, &rows, 2, false).unwrap();
    assert!(written.exists());
    // Second write to the same generation must fail — committed artifacts are frozen.
    assert!(matches!(
        write_generation(&dir, &rows, 2, false),
        Err(SessionError::Committed(_))
    ));
    // v3 publish works alongside.
    write_generation(&dir, &rows, 3, false).unwrap();
    assert_eq!(latest_generation(&dir).unwrap(), Some(3));

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn zstd_round_trip() {
    let dir = std::env::temp_dir().join(format!("vocoder-zstd-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).unwrap();
    let rows = vec![
        serde_json::json!({"type":"session","version":3,"id":"z"}),
        serde_json::json!({"type":"user/message","data":{"text":"compressed payload payload payload"}}),
    ];
    write_generation(&dir, &rows, 3, true).unwrap();
    let path = generation_path(&dir, 3).unwrap();
    assert!(path.to_string_lossy().ends_with(".jsonl.zstd"));
    let back = read_generation(&path).unwrap();
    assert_eq!(back, rows);
    std::fs::remove_dir_all(&dir).ok();
}

struct BumpVersion;
impl MigrationStep for BumpVersion {
    fn from(&self) -> u32 {
        2
    }
    fn migrate(
        &self,
        mut rows: Vec<serde_json::Value>,
    ) -> Result<Vec<serde_json::Value>, SessionError> {
        rows[0]["version"] = serde_json::json!(3);
        Ok(rows)
    }
}

#[test]
fn migration_chain_composes() {
    let rows = vec![serde_json::json!({"type":"session","version":2,"id":"s"})];
    let steps: Vec<&dyn MigrationStep> = vec![&BumpVersion];
    let out = compose(&steps, 2, 3, rows).unwrap();
    assert_eq!(out[0]["version"], 3);
    // Missing steps fail loudly.
    assert!(matches!(
        compose(&vec![], 2, 3, vec![]),
        Err(SessionError::NoMigration { .. })
    ));
}

#[test]
fn highest_generation_wins() {
    let root = snapshots_root();
    let mut total_dirs = 0;
    let mut total_files = 0;
    for entry in std::fs::read_dir(&root).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        total_dirs += 1;
        let gens = list_generations(&dir).unwrap();
        if gens.is_empty() {
            continue;
        }
        // Top generation exists and is the max.
        let top = *gens.last().unwrap();
        assert!(generation_path(&dir, top).is_some());
        assert_eq!(latest_generation(&dir).unwrap(), Some(top));
        total_files += gens.len();
    }
    assert!(
        total_dirs > 20,
        "expected many snapshot dirs, got {total_dirs}"
    );
    assert!(
        total_files > 40,
        "expected many generation files, got {total_files}"
    );
}

#[test]
fn reads_headers_of_every_committed_generation() {
    let root = snapshots_root();
    let mut count = 0;
    for entry in std::fs::read_dir(&root).unwrap() {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        for v in list_generations(&dir).unwrap() {
            let path = generation_path(&dir, v).unwrap();
            let header =
                read_header(&path).unwrap_or_else(|e| panic!("{} v{v}: {e}", dir.display()));
            assert_eq!(header.row_type, "session", "{}: v{v}", dir.display());
            assert_eq!(
                header.version,
                v,
                "{}: header.version ≠ filename v",
                dir.display()
            );
            count += 1;
        }
    }
    assert!(count >= 50, "expected ≥50 snapshots, got {count}");
}
