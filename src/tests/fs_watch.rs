use std::{
    collections::HashMap,
    path::Path,
    sync::{Mutex, mpsc as std_mpsc},
};

use fluent_uri::Uri;
use notify::Watcher;
use tokio::sync::mpsc;

use crate::{scheme::Delta, utils::Span};

thread_local! {
    static FILE_CONTENT: Mutex<HashMap<Uri<&'static str>, String>> = Mutex::new(HashMap::new());
}

pub async fn watch_directory(
    tx: mpsc::Sender<Delta<Span, String>>,
    dir: impl AsRef<Path>,
) -> notify::Result<()> {
    let dir = std::fs::canonicalize(dir.as_ref()).unwrap_or_else(|_| dir.as_ref().to_path_buf());

    let (event_tx, mut event_rx) =
        tokio::sync::mpsc::unbounded_channel::<notify::Result<notify::Event>>();

    let watch_dir = dir.clone();
    std::thread::spawn(move || {
        let (std_tx, std_rx) = std_mpsc::channel::<notify::Result<notify::Event>>();
        let Ok(mut watcher) = notify::recommended_watcher(std_tx) else {
            return;
        };
        if watcher
            .watch(&watch_dir, notify::RecursiveMode::Recursive)
            .is_err()
        {
            return;
        }
        for res in std_rx {
            if event_tx.send(res).is_err() {
                break;
            }
        }
    });

    scan_and_send_directory(&tx, &dir).await;

    while let Some(res) = event_rx.recv().await {
        let Ok(event) = res else { continue };

        if !matches!(event.kind, notify::EventKind::Modify(_)) {
            continue;
        }

        for path in &event.paths {
            process_file_change(&tx, path).await;
        }
    }

    Ok(())
}

async fn process_file_change(tx: &mpsc::Sender<Delta<Span, String>>, path: &Path) {
    use similar::{Algorithm, DiffOp, capture_diff_slices};

    let Ok(new) = std::fs::read_to_string(path) else {
        return;
    };
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let uri = Span::new(&format!("file://{}", abs.display()), 0, 0)
        .expect("watch: invalid file path")
        .uri;

    let old_content =
        FILE_CONTENT.with(|c| c.lock().unwrap().get(&uri).cloned().unwrap_or_default());

    if new == old_content {
        return;
    }

    let ops = capture_diff_slices(Algorithm::Patience, old_content.as_bytes(), new.as_bytes());

    for op in &ops {
        match op {
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                let _ = tx
                    .send(Delta::Delete {
                        key: Span::new_uri(uri, *old_index, old_index + old_len).unwrap(),
                    })
                    .await;
            }
            DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                let text = new[*new_index..*new_index + *new_len].to_string();
                let _ = tx
                    .send(Delta::Insert {
                        key: Span::new_uri(uri, *old_index, *old_index).unwrap(),
                        value: text,
                    })
                    .await;
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                let _ = tx
                    .send(Delta::Delete {
                        key: Span::new_uri(uri, *old_index, old_index + old_len).unwrap(),
                    })
                    .await;
                let text = new[*new_index..*new_index + *new_len].to_string();
                let _ = tx
                    .send(Delta::Insert {
                        key: Span::new_uri(uri, *old_index, *old_index).unwrap(),
                        value: text,
                    })
                    .await;
            }
            DiffOp::Equal { .. } => {}
        }
    }

    FILE_CONTENT.with(|c| {
        c.lock().unwrap().insert(uri, new);
    });
}

async fn scan_and_send_directory(tx: &mpsc::Sender<Delta<Span, String>>, dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.is_file() {
            process_file_change(tx, &path).await;
        }
    }
}
