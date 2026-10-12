use std::{
    io::{BufRead, BufReader, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    client::ThingsCloudClient,
    dirs::{
        create_private_dir, open_private_append, open_private_read, read_private_to_string,
        write_private_atomic,
    },
    store::{RawState, fold_item},
    wire::wire_object::WireItem,
};

#[derive(Debug, Clone, Default)]
struct SyncSnapshot {
    history_key: Option<String>,
    head_index: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct CursorData {
    next_start_index: i64,
    history_key: String,
    #[serde(default)]
    head_index: i64,
    /// Account that produced `history_key`. Empty on cursors written before this field existed.
    #[serde(default)]
    account_email: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StateCacheData {
    #[serde(default)]
    version: u8,
    log_offset: u64,
    state: RawState,
}

const STATE_CACHE_VERSION: u8 = 3;

fn read_cursor(path: &Path) -> Result<CursorData> {
    let Some(raw) = read_private_to_string(path)? else {
        return Ok(CursorData::default());
    };
    if raw.trim().is_empty() {
        return Ok(CursorData::default());
    }
    serde_json::from_str(&raw)
        .with_context(|| format!("cursor at {} is not valid JSON", path.display()))
}

fn same_account(saved: &str, current: &str) -> bool {
    saved.trim().eq_ignore_ascii_case(current.trim())
}

/// A saved history key is the capability for one Things database. Do not reuse
/// it for a different email, and do not reuse a key that no longer matches the
/// account we can authenticate.
fn bind_cursor_account(
    client: &mut ThingsCloudClient,
    cursor: &CursorData,
    cache_dir: &Path,
) -> Result<()> {
    if cursor.history_key.is_empty() {
        if client.history_key.is_none() {
            client.authenticate()?;
        }
        return Ok(());
    }

    if !cursor.account_email.is_empty() && !same_account(&cursor.account_email, &client.email) {
        bail!(
            "state directory {} belongs to {}, current credentials are for {}. Refusing to sync so the two Things Cloud accounts are not mixed.",
            cache_dir.display(),
            cursor.account_email,
            client.email
        );
    }

    if cursor.account_email.is_empty() {
        let authed = client
            .authenticate()
            .context("saved history key has no account email; authenticating before reuse")?;
        if authed != cursor.history_key {
            bail!(
                "saved history key in {} does not match the current Things Cloud account. Refusing to sync so two accounts are not mixed.",
                cache_dir.display()
            );
        }
    }

    if client.history_key.is_none() {
        client.history_key = Some(cursor.history_key.clone());
    }
    Ok(())
}

fn write_cursor(
    path: &Path,
    next_start_index: i64,
    history_key: &str,
    head_index: i64,
    account_email: &str,
) -> Result<()> {
    let payload = serde_json::to_string(&serde_json::json!({
        "next_start_index": next_start_index,
        "history_key": history_key,
        "head_index": head_index,
        "account_email": account_email,
        "updated_at": crate::client::now_timestamp(),
    }))?;
    write_private_atomic(path, payload.as_bytes())?;
    Ok(())
}

pub fn sync_append_log(client: &mut ThingsCloudClient, cache_dir: &Path) -> Result<()> {
    let state_dir = crate::dirs::app_state_dir();
    if cache_dir.starts_with(&state_dir) {
        create_private_dir(&state_dir)?;
    }
    create_private_dir(cache_dir)?;
    let log_path = cache_dir.join("things.log");
    let cursor_path = cache_dir.join("cursor.json");

    let cursor = read_cursor(&cursor_path)?;
    bind_cursor_account(client, &cursor, cache_dir)?;
    let mut start_index = cursor.next_start_index;

    let mut fp = open_private_append(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;

    loop {
        let page = match client.get_items_page(start_index) {
            Ok(v) => v,
            Err(_) => {
                let _ = client.authenticate()?;
                client.get_items_page(start_index)?
            }
        };

        let items = page
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let end = page
            .get("end-total-content-size")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let latest = page
            .get("latest-total-content-size")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        client.head_index = page
            .get("current-item-index")
            .and_then(Value::as_i64)
            .unwrap_or(client.head_index);

        for item in &items {
            writeln!(fp, "{}", serde_json::to_string(item)?)?;
        }

        if !items.is_empty() {
            fp.flush()?;
            start_index += items.len() as i64;
            write_cursor(
                &cursor_path,
                start_index,
                client.history_key.as_deref().unwrap_or_default(),
                client.head_index,
                &client.email,
            )?;
        }

        if items.is_empty() || end >= latest {
            break;
        }
    }

    let current_history_key = client.history_key.clone().unwrap_or_default();
    if current_history_key != cursor.history_key || client.head_index != cursor.head_index {
        write_cursor(
            &cursor_path,
            start_index,
            &current_history_key,
            client.head_index,
            &client.email,
        )?;
    }

    Ok(())
}

fn read_state_cache(cache_dir: &Path) -> Result<(RawState, u64)> {
    let path = cache_dir.join("state_cache.json");
    let Some(raw) = read_private_to_string(&path)? else {
        return Ok((RawState::new(), 0));
    };
    let Ok(cache) = serde_json::from_str::<StateCacheData>(&raw) else {
        return Ok((RawState::new(), 0));
    };

    if cache.version != STATE_CACHE_VERSION {
        return Ok((RawState::new(), 0));
    }

    Ok((cache.state, cache.log_offset))
}

fn write_state_cache(cache_dir: &Path, state: &RawState, log_offset: u64) -> Result<()> {
    let path = cache_dir.join("state_cache.json");
    let payload = serde_json::to_string(&StateCacheData {
        version: STATE_CACHE_VERSION,
        log_offset,
        state: state.clone(),
    })?;
    write_private_atomic(&path, payload.as_bytes())?;
    Ok(())
}

pub fn fold_state_from_append_log(cache_dir: &Path) -> Result<RawState> {
    let log_path = cache_dir.join("things.log");
    if !log_path.exists() {
        return Ok(RawState::new());
    }

    let (mut state, byte_offset) = read_state_cache(cache_dir)?;
    let mut new_lines = 0u64;

    let mut file = open_private_read(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    file.seek(SeekFrom::Start(byte_offset))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut safe_offset = byte_offset;

    loop {
        let entry_offset = reader.stream_position()?;
        line.clear();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            break;
        }

        if !line.ends_with('\n') {
            break;
        }

        let stripped = line.trim();
        if stripped.is_empty() {
            safe_offset = reader.stream_position()?;
            continue;
        }
        let item: WireItem = serde_json::from_str(stripped).map_err(|error| {
            anyhow!(
                "Corrupt log entry at {} byte {}: {}",
                log_path.display(),
                entry_offset,
                error
            )
        })?;
        fold_item(item, &mut state);
        new_lines += 1;
        safe_offset = reader.stream_position()?;
    }

    if new_lines > 0 {
        write_state_cache(cache_dir, &state, safe_offset)?;
    }

    Ok(state)
}

pub fn get_state_with_append_log(
    client: &mut ThingsCloudClient,
    cache_dir: PathBuf,
) -> Result<RawState> {
    let mut sync_client = client.clone();
    let sync_cache_dir = cache_dir.clone();

    let sync_worker = std::thread::spawn(move || -> Result<SyncSnapshot> {
        sync_append_log(&mut sync_client, &sync_cache_dir)?;
        Ok(SyncSnapshot {
            history_key: sync_client.history_key,
            head_index: sync_client.head_index,
        })
    });

    let _stale_state = fold_state_from_append_log(&cache_dir)?;

    let sync_snapshot = sync_worker
        .join()
        .map_err(|_| anyhow!("sync worker panicked"))??;

    client.history_key = sync_snapshot.history_key;
    client.head_index = sync_snapshot.head_index;

    fold_state_from_append_log(&cache_dir)
}

pub fn fold_state_from_append_log_or_empty(cache_dir: &Path) -> RawState {
    fold_state_from_append_log(cache_dir).unwrap_or_default()
}

pub fn read_cached_head_index(cache_dir: &Path) -> Result<i64> {
    Ok(read_cursor(&cache_dir.join("cursor.json"))?.head_index)
}

pub fn sync_append_log_or_err(client: &mut ThingsCloudClient, cache_dir: &Path) -> Result<()> {
    sync_append_log(client, cache_dir).map_err(|e| anyhow!(e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn state_cache_version_change_refolds_the_append_log() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = temp_dir.path();
        let task_id = "A7h5eCi24RvAWKC3Hv3muf";
        let log = format!(
            "{{\"{task_id}\":{{\"t\":0,\"e\":\"Task6\",\"p\":{{\"tt\":\"Current task\",\"ss\":0}}}}}}\n\
             {{\"{task_id}\":{{\"t\":1,\"e\":\"Task7\",\"p\":{{\"md\":2.0}}}}}}\n"
        );
        fs::write(cache_dir.join("things.log"), &log).expect("seed log");
        fs::write(
            cache_dir.join("state_cache.json"),
            format!(
                "{{\"version\":{},\"log_offset\":{},\"state\":{{}}}}",
                STATE_CACHE_VERSION - 1,
                log.len()
            ),
        )
        .expect("seed stale state cache");

        let state = fold_state_from_append_log(cache_dir).expect("refold stale cache");

        assert!(state.contains_key(&task_id.parse().expect("task id")));
        let store = crate::store::ThingsStore::from_raw_state(&state);
        let task = store.get_task(task_id).expect("Task7 task after refold");
        assert_eq!(task.title, "Current task");
        assert_eq!(task.entity, crate::wire::wire_object::EntityType::Task7);
        let (cached_state, offset) = read_state_cache(cache_dir).expect("read state cache");
        assert_eq!(cached_state, state);
        assert_eq!(offset, log.len() as u64);
    }

    #[test]
    fn fold_state_accepts_legacy_action_group_ids() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = temp_dir.path();
        let action_group_id = "ACTIONGROUP-11111111-2222-4333-8444-555555555555";
        let task_id = "3C6BBD49-8D11-4FFF-8B0E-B8F33FA9C00A";
        let log = format!(
            r#"{{"{action_group_id}":{{"t":0,"e":"Task3","p":{{"tt":"Heading","ss":0,"tp":2,"st":1}}}},"{task_id}":{{"t":0,"e":"Task3","p":{{"tt":"Legacy child","ss":0,"tp":0,"st":1,"agr":["{action_group_id}"]}}}}}}"#
        ) + "\n";
        fs::write(cache_dir.join("things.log"), log).expect("seed legacy log");

        let state = fold_state_from_append_log(cache_dir).expect("fold legacy action-group IDs");
        let store = crate::store::ThingsStore::from_raw_state(&state);
        let task = store.get_task(task_id).expect("legacy child task");

        assert_eq!(
            task.action_group,
            Some(action_group_id.parse().expect("action-group ID"))
        );
    }

    #[test]
    fn fold_state_reports_the_offset_and_parse_error() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = temp_dir.path();
        fs::write(cache_dir.join("things.log"), "{not-json}\n").expect("seed corrupt log");

        let error = fold_state_from_append_log(cache_dir)
            .expect_err("corrupt log must fail")
            .to_string();

        assert!(error.contains("byte 0"));
        assert!(error.contains("key must be a string"));
    }

    #[test]
    fn fold_state_ignores_trailing_partial_line() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = temp_dir.path();
        let log_path = cache_dir.join("things.log");

        let line_one = r#"{"3C6BBD49-8D11-4FFF-8B0E-B8F33FA9C00A":{"t":0,"e":"Settings5","p":{}}}"#;
        let line_two = r#"{"4C6BBD49-8D11-4FFF-8B0E-B8F33FA9C00B":{"t":0,"e":"Settings5","p":{}}}"#;
        let split_at = line_two.len() / 2;

        fs::write(
            &log_path,
            format!("{}\n{}", line_one, &line_two[..split_at]),
        )
        .expect("seed log");

        let first_state = fold_state_from_append_log(cache_dir).expect("first fold");
        assert_eq!(first_state.len(), 1);

        let (_, first_offset) = read_state_cache(cache_dir).expect("read state cache");
        assert_eq!(first_offset, (line_one.len() + 1) as u64);

        let mut fp = fs::OpenOptions::new()
            .append(true)
            .open(&log_path)
            .expect("open log for append");
        writeln!(fp, "{}", &line_two[split_at..]).expect("append line remainder");

        let second_state = fold_state_from_append_log(cache_dir).expect("second fold");
        assert_eq!(second_state.len(), 2);

        let expected_offset = fs::metadata(&log_path).expect("log metadata").len();
        let (_, second_offset) = read_state_cache(cache_dir).expect("read state cache");
        assert_eq!(second_offset, expected_offset);
    }

    #[test]
    fn sync_refuses_cursor_for_a_different_account() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = temp_dir.path();
        fs::write(
            cache_dir.join("cursor.json"),
            r#"{"next_start_index":4,"history_key":"old-history-key","head_index":4,"account_email":"old@example.com"}"#,
        )
        .expect("seed cursor");

        let mut client =
            ThingsCloudClient::new("new@example.com".to_string(), "not-sent".to_string())
                .expect("client");
        let error = sync_append_log(&mut client, cache_dir)
            .expect_err("different account must fail before any network use")
            .to_string();
        assert!(error.contains("old@example.com"), "{error}");
        assert!(error.contains("new@example.com"), "{error}");
        assert!(client.history_key.is_none());
        assert!(!cache_dir.join("things.log").exists());
    }

    #[cfg(unix)]
    #[test]
    fn fold_refuses_a_symlinked_log() {
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = temp_dir.path();
        let victim = cache_dir.join("victim.log");
        fs::write(&victim, "secret\n").expect("seed");
        std::os::unix::fs::symlink(&victim, cache_dir.join("things.log")).expect("symlink");
        let error = fold_state_from_append_log(cache_dir).expect_err("symlink log");
        let error = format!("{error:#}");
        assert!(error.contains("symlink"), "{error}");
    }
}
