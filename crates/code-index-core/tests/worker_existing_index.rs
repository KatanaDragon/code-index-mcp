//! Регрессия дефекта 01.10.2026 (mdm, KA-release_01, ka-upr_obnovlenie-kontura).
//!
//! Путь с уже готовым индексом уходил в ветку «новая база»: демон начинал
//! пересоздавать индекс поверх существующего, падал на открытии и крутил
//! аварийные перезапуски, не называя причины. Тесты проверяют оба конца этого
//! поведения: готовый индекс читается как готовый (сверка изменений, а не
//! пересборка), а нечитаемая база даёт внятную причину вместо тихой смерти
//! worker'а.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use code_index_core::daemon_core::config::{IndexerSection, PathEntry};
use code_index_core::daemon_core::ipc::PathStatus;
use code_index_core::daemon_core::state::DaemonState;
use code_index_core::daemon_core::worker::run_worker;
use code_index_core::storage::Storage;
use tokio::sync::broadcast;

/// Хеш-заглушка: по ней видно, переиндексировал ли worker файл.
const ХЕШ_СТАРОЙ_ЗАПИСИ: &str = "hash-из-прежней-индексации";

fn entry(path: &std::path::Path) -> PathEntry {
    PathEntry {
        path: path.to_path_buf(),
        index_dir: None,
        debounce_ms: Some(50),
        batch_ms: Some(200),
        alias: Some("probe".to_string()),
        language: None,
        max_code_file_size_bytes: None,
        bulk_batch_threshold: None,
    }
}

/// Заготовка папки-репозитория с одним исходником и базой, в которой об этом
/// исходнике уже есть запись (mtime и размер совпадают с файлом на диске —
/// ровно то состояние, при котором демон обязан пойти по быстрому пути).
fn репо_с_готовым_индексом() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let file = root.join("модуль.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();

    let meta = std::fs::metadata(&file).unwrap();
    let mtime = meta
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    let db_dir = root.join(".code-index");
    std::fs::create_dir_all(&db_dir).unwrap();
    let storage = Storage::open_file(&db_dir.join("index.db")).unwrap();
    storage
        .conn()
        .execute(
            "INSERT INTO files (path, content_hash, language, mtime, file_size) \
             VALUES (?1, ?2, 'rust', ?3, ?4)",
            rusqlite::params![
                "модуль.rs",
                ХЕШ_СТАРОЙ_ЗАПИСИ,
                mtime,
                meta.len() as i64
            ],
        )
        .unwrap();
    drop(storage);

    (tmp, root)
}

fn поднять_worker(
    root: &std::path::Path,
) -> (
    tokio::task::JoinHandle<()>,
    DaemonState,
    broadcast::Sender<()>,
    PathBuf,
) {
    let state = DaemonState::new();
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let entry = entry(root);
    // Приёмка 7а: путь — тем же хелпером, что и воркер. На Windows
    // `std::fs::canonicalize` вернул бы verbatim `\\?\…`, и ключ статуса не
    // совпал бы с тем, под которым пишет воркер.
    let canonical = code_index_core::paths::canonicalize(&entry.path);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handle = tokio::task::spawn_blocking({
        let state = state.clone();
        move || run_worker(entry, state, shutdown_rx, stop, None, IndexerSection::default(), None, None)
    });
    (handle, state, shutdown_tx, canonical)
}

/// Дождаться статуса пути (или таймаута).
async fn ждать_статус(state: &DaemonState, path: &PathBuf, wanted: PathStatus, limit: Duration) -> PathStatus {
    let до = std::time::Instant::now() + limit;
    loop {
        let статус = state
            .get(path)
            .await
            .map(|r| r.status)
            .unwrap_or(PathStatus::NotStarted);
        if статус == wanted || std::time::Instant::now() > до {
            return статус;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Готовый индекс — это сверка изменений при старте, а не пересборка: запись о
/// неизменившемся файле остаётся прежней, а worker живёт до команды остановки
/// (раньше он здесь падал и уходил в цикл перезапусков).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn готовый_индекс_не_пересобирается_и_не_роняет_worker() {
    let (tmp, root) = репо_с_готовым_индексом();
    let (handle, state, shutdown_tx, canonical) = поднять_worker(&root);

    let статус = ждать_статус(&state, &canonical, PathStatus::Ready, Duration::from_secs(30)).await;
    assert_eq!(
        статус,
        PathStatus::Ready,
        "путь с готовым индексом обязан дойти до ready, а не уйти в ошибку"
    );
    assert!(
        !handle.is_finished(),
        "worker с готовым индексом продолжает слежение за файлами, а не завершается"
    );

    // Запись о файле не переписана: файл не изменился, переиндексации не было.
    let storage = Storage::open_file_readonly(&root.join(".code-index").join("index.db")).unwrap();
    let хеш: String = storage
        .conn()
        .query_row("SELECT content_hash FROM files WHERE path = 'модуль.rs'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(хеш, ХЕШ_СТАРОЙ_ЗАПИСИ, "файл переиндексирован — базе не поверили");

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
    drop(tmp);
}

/// База есть, но не читается: worker обязан назвать причину, а не умереть
/// молча, и не тронуть файл базы (в ней может лежать готовая работа).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn нечитаемая_база_называет_причину_и_не_пересоздаётся() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    std::fs::write(root.join("модуль.rs"), "fn main() {}\n").unwrap();
    let db_dir = root.join(".code-index");
    std::fs::create_dir_all(&db_dir).unwrap();
    let db = db_dir.join("index.db");
    let мусор = b"not a sqlite database at all";
    std::fs::write(&db, мусор).unwrap();

    let (handle, state, _shutdown_tx, canonical) = поднять_worker(&root);
    let статус = ждать_статус(&state, &canonical, PathStatus::Error, Duration::from_secs(60)).await;
    assert_eq!(статус, PathStatus::Error, "нечитаемая база — это ошибка пути");

    let причина = state
        .get(&canonical)
        .await
        .and_then(|r| r.error)
        .expect("причина отказа обязана попасть в статус пути");
    assert!(
        причина.contains("не читается"),
        "причина должна объяснять отказ, получено: {}",
        причина
    );

    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
    assert_eq!(
        std::fs::read(&db).unwrap(),
        мусор,
        "файл базы не пересоздаётся: пересборка поверх чужой работы недопустима"
    );
    drop(tmp);
}
