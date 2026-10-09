// Разделяемое состояние демона: статусы отслеживаемых папок, прогресс, ошибки.
//
// Все изменения статуса идут через методы `DaemonState`, чтобы watcher,
// indexer и HTTP-сервер видели одни и те же данные.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::RwLock;

use super::config::PathEntry;
use super::ipc::{PathHealth, PathStatus, Progress};

/// Разделяемое состояние, которое демон держит в памяти.
#[derive(Clone)]
pub struct DaemonState {
    inner: Arc<RwLock<DaemonStateInner>>,
}

struct DaemonStateInner {
    /// Время старта демона (unix seconds).
    started_at_unix: u64,
    /// Время старта в RFC 3339 — кешируем, чтобы не форматировать каждый раз.
    started_at_rfc3339: String,
    /// Статусы папок.
    paths: HashMap<PathBuf, PathRuntime>,
    /// Значимые поля записи `[[paths]]`, применённые в прошлый раз. По ним
    /// `reload` отличает изменённый путь от неизменного: раньше сравнивались
    /// только пути, и смена `index_dir` молча попадала в `unchanged`.
    entries: HashMap<PathBuf, EntrySig>,
}

/// Значимые для воркера поля `PathEntry` — без `path` и `alias`.
///
/// `path` — это ключ мапы (уже приведён к одной форме упрощённой канонизацией),
/// поэтому сравнивать исходные написания пути нельзя: один и тот же каталог,
/// записанный с хвостовым слешем или другим регистром, дал бы ложное «изменение».
/// `alias` демон игнорирует (его читает только `serve`), и он не должен
/// блокировать reload.
#[derive(Debug, Clone, PartialEq)]
struct EntrySig {
    index_dir: Option<PathBuf>,
    debounce_ms: Option<u64>,
    batch_ms: Option<u64>,
    language: Option<String>,
    max_code_file_size_bytes: Option<usize>,
    bulk_batch_threshold: Option<usize>,
}

impl EntrySig {
    fn of(entry: &PathEntry) -> Self {
        Self {
            index_dir: entry.index_dir.clone(),
            debounce_ms: entry.debounce_ms,
            batch_ms: entry.batch_ms,
            language: entry.language.clone(),
            max_code_file_size_bytes: entry.max_code_file_size_bytes,
            bulk_batch_threshold: entry.bulk_batch_threshold,
        }
    }
}

/// Итог применения конфига к состоянию. `changed` — путь был и остался, но
/// значимые поля записи поменялись: воркер надо перезапустить с новой записью.
/// `exclude_dirs` сюда не входит намеренно: это поле проекта
/// (`.code-index/config.json`), а не `daemon.toml`, и reload его не видит.
#[derive(Debug, Clone, Default)]
pub struct ApplyOutcome {
    pub added: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
    pub changed: Vec<PathBuf>,
    pub unchanged: Vec<PathBuf>,
}

/// Runtime-данные по одной папке.
#[derive(Debug, Clone)]
pub struct PathRuntime {
    pub status: PathStatus,
    pub progress: Option<Progress>,
    pub error: Option<String>,
    /// Когда папка последний раз приходила в `Ready`.
    pub last_ready_at: Option<String>,
    /// Момент последнего изменения статуса или прогресса. По нему строка
    /// состояния демона считает, сколько папка стоит без движения — главный
    /// признак «встало» при разборе обращения.
    pub changed_at: Instant,
    /// Поток, который ведёт эту папку. По нему берётся текущий этап работы:
    /// сам поток занят долгой синхронной работой и состояние обновлять не
    /// может, поэтому этап читается из общего реестра `logging`.
    pub worker_thread: Option<std::thread::ThreadId>,
}

impl Default for PathRuntime {
    fn default() -> Self {
        Self {
            status: PathStatus::NotStarted,
            progress: None,
            error: None,
            last_ready_at: None,
            changed_at: Instant::now(),
            worker_thread: None,
        }
    }
}

/// Срез одной папки для строки состояния демона в журнале.
#[derive(Debug, Clone)]
pub struct PathPulse {
    pub path: PathBuf,
    pub status: PathStatus,
    pub progress: Option<Progress>,
    /// Секунд без изменения статуса и прогресса.
    pub still_sec: u64,
    /// Чем папка занята прямо сейчас и сколько секунд: («граф вызовов», 40).
    /// `None` — между этапами или когда работа закончена.
    pub stage: Option<(String, u64)>,
}

impl DaemonState {
    pub fn new() -> Self {
        let now = SystemTime::now();
        let started_at_unix = now.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let started_at_rfc3339 = chrono::DateTime::<chrono::Utc>::from(now).to_rfc3339();
        Self {
            inner: Arc::new(RwLock::new(DaemonStateInner {
                started_at_unix,
                started_at_rfc3339,
                paths: HashMap::new(),
                entries: HashMap::new(),
            })),
        }
    }

    /// Список отслеживаемых путей.
    pub async fn tracked_paths(&self) -> Vec<PathBuf> {
        let guard = self.inner.read().await;
        guard.paths.keys().cloned().collect()
    }

    /// Применить к состоянию набор записей `[[paths]]`. Новые пути добавляются
    /// со статусом `NotStarted`; убранные — удаляются; у существующих сравнение
    /// значимых полей записи отделяет изменённые (`changed`) от неизменных
    /// (`unchanged`). Ключи — упрощённая канонизация (без verbatim-префикса),
    /// та же, что строит воркер для своего пути.
    pub async fn apply_config(&self, entries: &[PathEntry]) -> ApplyOutcome {
        let mut guard = self.inner.write().await;
        let mut out = ApplyOutcome::default();

        // Порядок конфига сохраняем в списке ключей: результат reload виден
        // оператору, и «прыгающие» строки читались бы хуже.
        let mut keys: Vec<PathBuf> = Vec::with_capacity(entries.len());
        let mut wanted: HashMap<PathBuf, EntrySig> = HashMap::with_capacity(entries.len());
        for entry in entries {
            let key = crate::paths::canonicalize(&entry.path);
            if wanted.insert(key.clone(), EntrySig::of(entry)).is_none() {
                keys.push(key);
            }
        }

        for key in &keys {
            match guard.entries.get(key) {
                None => {
                    guard.paths.insert(key.clone(), PathRuntime::default());
                    out.added.push(key.clone());
                }
                Some(prev) if prev != &wanted[key] => out.changed.push(key.clone()),
                Some(_) => out.unchanged.push(key.clone()),
            }
        }

        let mut removed: Vec<PathBuf> = guard
            .paths
            .keys()
            .filter(|p| !wanted.contains_key(*p))
            .cloned()
            .collect();
        removed.sort();
        for p in &removed {
            guard.paths.remove(p);
        }
        out.removed = removed;

        // Снимок применённой конфигурации — база для сравнения на следующем reload.
        guard.entries = wanted;

        out
    }

    /// Запомнить поток, который ведёт эту папку. Вызывается самим рабочим
    /// потоком в начале работы: по этой отметке строка состояния демона
    /// узнаёт, каким этапом папка занята прямо сейчас.
    pub async fn note_worker_thread(&self, path: &PathBuf) {
        let mut guard = self.inner.write().await;
        let entry = guard.paths.entry(path.clone()).or_default();
        entry.worker_thread = Some(std::thread::current().id());
    }

    /// Выставить статус папки. Используется фоновыми задачами демона.
    pub async fn set_status(&self, path: &PathBuf, status: PathStatus) {
        let mut guard = self.inner.write().await;
        let entry = guard.paths.entry(path.clone()).or_default();
        entry.status = status;
        entry.error = None;
        entry.changed_at = Instant::now();
        if status == PathStatus::Ready {
            entry.progress = None;
            entry.last_ready_at = Some(chrono::Utc::now().to_rfc3339());
        }
    }

    /// Обновить прогресс индексации папки. Статус должен быть `InitialIndexing`
    /// или `ReindexingBatch`, иначе вызов игнорируется.
    pub async fn set_progress(&self, path: &PathBuf, progress: Progress) {
        let mut guard = self.inner.write().await;
        if let Some(entry) = guard.paths.get_mut(path) {
            if matches!(
                entry.status,
                PathStatus::InitialIndexing | PathStatus::ReindexingBatch
            ) {
                entry.progress = Some(progress);
                entry.changed_at = Instant::now();
            }
        }
    }

    /// Зафиксировать ошибку индексации папки.
    pub async fn set_error(&self, path: &PathBuf, message: impl Into<String>) {
        let mut guard = self.inner.write().await;
        let entry = guard.paths.entry(path.clone()).or_default();
        entry.status = PathStatus::Error;
        entry.progress = None;
        entry.error = Some(message.into());
        entry.changed_at = Instant::now();
    }

    /// Получить текущий runtime одной папки.
    pub async fn get(&self, path: &PathBuf) -> Option<PathRuntime> {
        let guard = self.inner.read().await;
        guard.paths.get(path).cloned()
    }

    /// Время старта демона в секундах UNIX.
    pub async fn started_at_unix(&self) -> u64 {
        self.inner.read().await.started_at_unix
    }

    /// Время старта демона в RFC 3339.
    pub async fn started_at_rfc3339(&self) -> String {
        self.inner.read().await.started_at_rfc3339.clone()
    }

    /// Срез всех папок для пульса в журнале: статус, прогресс и сколько
    /// секунд не было движения. Порядок — по пути, чтобы строки в журнале
    /// от снимка к снимку шли одинаково и разница читалась глазами.
    pub async fn pulse_snapshot(&self) -> Vec<PathPulse> {
        let guard = self.inner.read().await;
        let now = Instant::now();
        let mut out: Vec<PathPulse> = guard
            .paths
            .iter()
            .map(|(path, rt)| PathPulse {
                path: path.clone(),
                status: rt.status,
                progress: rt.progress.clone(),
                still_sec: now.duration_since(rt.changed_at).as_secs(),
                stage: rt.worker_thread.and_then(crate::logging::stage_running),
            })
            .collect();
        out.sort_by(|a, b| a.path.cmp(&b.path));
        out
    }

    /// Сформировать срез состояния для ответа GET /health.
    pub async fn to_health_paths(&self) -> Vec<PathHealth> {
        let guard = self.inner.read().await;
        guard
            .paths
            .iter()
            .map(|(path, rt)| PathHealth {
                path: path.clone(),
                status: rt.status,
                progress: rt.progress.clone(),
                error: rt.error.clone(),
                last_ready_at: rt.last_ready_at.clone(),
            })
            .collect()
    }
}

impl Default for DaemonState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Минимальная запись `[[paths]]` для тестов состояния.
    fn entry(path: impl AsRef<std::path::Path>) -> PathEntry {
        PathEntry {
            path: path.as_ref().to_path_buf(),
            index_dir: None,
            debounce_ms: None,
            batch_ms: None,
            alias: None,
            language: None,
            max_code_file_size_bytes: None,
            bulk_batch_threshold: None,
        }
    }

    /// Ключ, под которым запись попадает в состояние: та же упрощённая
    /// канонизация, что и в `apply_config`.
    fn key(p: &std::path::Path) -> PathBuf {
        crate::paths::canonicalize(p)
    }

    /// Пути берём под временным каталогом, а не литералами вида `/a`: на
    /// windows-latest рабочая папка — `D:\a\…`, поэтому `canonicalize("/a")`
    /// разрешается в существующий `D:\a` и ключ перестаёт совпадать с ожидаемым.
    #[tokio::test]
    async fn apply_config_tracks_diff() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (a, b, c) = (tmp.path().join("a"), tmp.path().join("b"), tmp.path().join("c"));
        let st = DaemonState::new();

        let out = st
            .apply_config(&[entry(&a), entry(&b)])
            .await;
        assert_eq!(out.added.len(), 2);
        assert_eq!(out.removed.len(), 0);
        assert_eq!(out.changed.len(), 0);
        assert_eq!(out.unchanged.len(), 0);

        let out = st
            .apply_config(&[entry(&b), entry(&c)])
            .await;
        assert_eq!(out.added, vec![key(&c)]);
        assert_eq!(out.removed, vec![key(&a)]);
        assert_eq!(out.changed.len(), 0);
        assert_eq!(out.unchanged, vec![key(&b)]);
    }

    /// Приёмка 7б: `reload` видит смену `index_dir` у существующего пути.
    /// Раньше сравнивались только пути, и смена каталога индекса оставалась
    /// в `unchanged` — воркер продолжал писать в старый.
    #[tokio::test]
    async fn apply_config_detects_index_dir_change() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let st = DaemonState::new();
        st.apply_config(&[entry(&repo)]).await;

        let same = st.apply_config(&[entry(&repo)]).await;
        assert!(same.changed.is_empty(), "без правок путь не менялся: {:?}", same.changed);
        assert_eq!(same.unchanged, vec![key(&repo)]);

        let mut moved = entry(&repo);
        moved.index_dir = Some(tmp.path().join("indexes").join("repo"));
        let out = st.apply_config(&[moved.clone()]).await;

        assert_eq!(out.changed, vec![key(&repo)]);
        assert!(out.unchanged.is_empty());
        assert!(out.added.is_empty());

        // Повторное применение той же записи снова считает её неизменной —
        // снимок обновился.
        let again = st.apply_config(&[moved]).await;
        assert!(again.changed.is_empty());
        assert_eq!(again.unchanged, vec![key(&repo)]);
    }

    /// `alias` демон игнорирует — правка `alias` не должна перезапускать воркер.
    #[tokio::test]
    async fn apply_config_ignores_alias_change() {
        let tmp = tempfile::TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let st = DaemonState::new();
        st.apply_config(&[entry(&repo)]).await;

        let mut renamed = entry(&repo);
        renamed.alias = Some("widgets".into());
        let out = st.apply_config(&[renamed]).await;
        assert!(out.changed.is_empty(), "alias не влияет на воркер: {:?}", out.changed);
        assert_eq!(out.unchanged, vec![key(&repo)]);
    }

    #[tokio::test]
    async fn set_ready_clears_progress() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = key(&tmp.path().join("a"));
        let st = DaemonState::new();
        st.apply_config(&[entry(&path)]).await;
        st.set_status(&path, PathStatus::InitialIndexing).await;
        st.set_progress(&path, Progress::new(10, 100)).await;

        st.set_status(&path, PathStatus::Ready).await;

        let rt = st.get(&path).await.unwrap();
        assert_eq!(rt.status, PathStatus::Ready);
        assert!(rt.progress.is_none());
        assert!(rt.last_ready_at.is_some());
    }
}
