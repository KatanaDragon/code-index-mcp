// HTTP-клиент к демону. Все методы асинхронные — CLI-команды вызываются из
// контекста #[tokio::main], а MCP-сервер тоже async.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Result};

use super::ipc::{HealthResponse, PathStatusResponse, ReloadResponse, RuntimeInfo, StopResponse};
use super::runner;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// Базовый URL запущенного демона. `Err` если runtime-info файл не найден.
pub fn base_url() -> Result<String> {
    let info =
        runner::read_runtime_info().ok_or_else(|| anyhow!("{}", daemon_unavailable_hint()))?;
    Ok(info.base_url())
}

/// Прочитать runtime-info без ошибки (Some если демон запущен).
pub fn runtime_info() -> Option<RuntimeInfo> {
    runner::read_runtime_info()
}

/// Команда управления демоном для подсказок: `<текущий exe> daemon <sub>`.
///
/// Один `cli.rs` собирается в два бинарника (`code-index` и `bsl-indexer`), а
/// MCP-сервер (`serve`) и демон на машине — один и тот же exe. Поэтому имя
/// берётся из `current_exe()` (полный путь, в кавычках, если в нём пробелы), а не
/// пишется литералом: на машине с `bsl-indexer.exe` подсказка «code-index daemon
/// run» вела в несуществующую команду. Откат при ошибке `current_exe()` — оба
/// имени бинарника: `CARGO_PKG_NAME` здесь — `code-index-core`, это библиотека,
/// а не exe.
pub fn daemon_command(sub: &str) -> String {
    let exe = match std::env::current_exe() {
        Ok(path) => {
            let text = path.display().to_string();
            if text.chars().any(char::is_whitespace) {
                format!("\"{}\"", text)
            } else {
                text
            }
        }
        Err(_) => "<bsl-indexer|code-index>".to_string(),
    };
    format!("{} daemon {}", exe, sub)
}

/// Каталог демона этого процесса (`CODE_INDEX_HOME`), `None` — не задан.
fn current_home() -> Option<PathBuf> {
    super::paths::home_dir().ok()
}

/// Диагностическое сообщение, когда демон не найден через runtime-info.
///
/// Самая частая причина (issue #1, подтверждено воспроизведением) — НЕ «демон не
/// запущен», а РАССИНХРОН `CODE_INDEX_HOME` между процессом `serve` и демоном:
/// они находят друг друга только через `$CODE_INDEX_HOME/daemon.json`, и если у
/// `serve` переменная не задана или указывает в другую папку, runtime-info не
/// читается, хотя демон жив. На Linux/macOS GUI-клиенты (VS Code, Continue, Cline)
/// не наследуют `~/.bashrc`, поэтому `serve`, запущенный клиентом с пустым `env`,
/// не видит `CODE_INDEX_HOME` из шелла. Поэтому сообщение явно называет ожидаемый
/// путь и куда смотреть.
pub fn daemon_unavailable_hint() -> String {
    daemon_unavailable_hint_at(current_home().as_deref())
}

/// То же, что [`daemon_unavailable_hint`], но каталог демона задан явно
/// (`None` — `CODE_INDEX_HOME` не задана). Отдельно — чтобы тесты не меняли
/// переменную окружения процесса.
pub fn daemon_unavailable_hint_at(home: Option<&Path>) -> String {
    match home {
        Some(home) => format!(
            "Демон не найден: отсутствует runtime-info файл {}. \
             CODE_INDEX_HOME = {}. \
             Проверьте: (1) демон запущен — `{}`; \
             (2) демон и этот процесс используют ОДИН CODE_INDEX_HOME. \
             Частая причина: GUI-клиент (VS Code/Continue/Cline) на Linux/macOS не читает ~/.bashrc, \
             поэтому serve не видит CODE_INDEX_HOME из шелла — задайте его тем же абсолютным путём, \
             что у демона, в секции \"env\" MCP-конфигурации клиента.",
            home.join("daemon.json").display(),
            home.display(),
            daemon_command("run"),
        ),
        None => format!(
            "Демон не найден: переменная окружения {} не задана для этого процесса. \
             Задайте её тем же абсолютным путём, что у демона. Для GUI-клиентов \
             (VS Code/Continue/Cline) на Linux/macOS — в секции \"env\" MCP-конфигурации, \
             т.к. ~/.bashrc к ним не применяется. Пример: \"env\": {{ \"{}\": \"/home/you/code-index\" }}.",
            super::paths::HOME_ENV,
            super::paths::HOME_ENV,
        ),
    }
}

/// Текст `ToolUnavailable::DaemonOffline`: запрос к демону не прошёл (`err`).
///
/// Три части: причина, действие, где искать причину отказа старта. Причина
/// различает «демон снят снаружи» — `daemon.json` есть, а процесса с его PID нет
/// (штатная остановка удаляет `daemon.json` и пишет в журнал «демон остановлен»),
/// — от прочих случаев, где остаётся текст ошибки запроса.
pub fn daemon_offline_hint(err: &dyn std::fmt::Display) -> String {
    daemon_offline_hint_at(current_home().as_deref(), err)
}

/// То же, что [`daemon_offline_hint`], но каталог демона задан явно
/// (`None` — `CODE_INDEX_HOME` не задана). Тесты зовут её без `set_var`.
pub fn daemon_offline_hint_at(home: Option<&Path>, err: &dyn std::fmt::Display) -> String {
    let stale = home
        .and_then(|h| runner::read_runtime_info_from(&h.join("daemon.json")))
        .filter(|info| !super::lock::is_process_alive(info.pid));
    let cause = match stale {
        Some(info) => format!(
            "Демон code-index PID {} (старт {}) завершён без остановки, daemon.json устарел: \
             процесс сняли снаружи (запроса не принял: {}).",
            info.pid, info.started_at, err
        ),
        None => format!("Демон code-index не доступен ({}).", err),
    };
    let home_text = home
        .map(|h| h.display().to_string())
        .unwrap_or_else(|| "<не задана>".to_string());
    let log_text = home
        .map(|h| h.join("daemon.log").display().to_string())
        .unwrap_or_else(|| "<CODE_INDEX_HOME>/daemon.log".to_string());
    format!(
        "{} Запустите `{}` с CODE_INDEX_HOME={} вне сессии агента (процесс, запущенный \
         из сессии, завершится вместе с ней) или штатным запускателем машины (задача \
         планировщика / автозагрузка / systemd user unit). Если демон не поднимается — \
         причина в строке ERROR в {}.",
        cause,
        daemon_command("run"),
        home_text,
        log_text,
    )
}

fn async_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(DEFAULT_TIMEOUT)
        .build()
        .map_err(|e| anyhow!("reqwest::Client: {}", e))
}

/// GET /health
pub async fn health() -> Result<HealthResponse> {
    let url = format!("{}/health", base_url()?);
    let resp = async_client()?
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("GET {} → {}", url, e))?
        .error_for_status()?;
    let body: HealthResponse = resp.json().await?;
    Ok(body)
}

/// POST /reload
pub async fn reload() -> Result<ReloadResponse> {
    let url = format!("{}/reload", base_url()?);
    let resp = async_client()?
        .post(&url)
        .send()
        .await
        .map_err(|e| anyhow!("POST {} → {}", url, e))?
        .error_for_status()?;
    let body: ReloadResponse = resp.json().await?;
    Ok(body)
}

/// POST /stop
pub async fn stop() -> Result<StopResponse> {
    let url = format!("{}/stop", base_url()?);
    let resp = async_client()?
        .post(&url)
        .send()
        .await
        .map_err(|e| anyhow!("POST {} → {}", url, e))?
        .error_for_status()?;
    let body: StopResponse = resp.json().await?;
    Ok(body)
}

/// GET /path-status?path=...
pub async fn path_status_async(path: &Path) -> Result<PathStatusResponse> {
    let url = format!(
        "{}/path-status?path={}",
        base_url()?,
        urlencoding(path.to_string_lossy().as_ref())
    );
    let resp = async_client()?
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow!("GET {} → {}", url, e))?
        .error_for_status()?;
    let body: PathStatusResponse = resp.json().await?;
    Ok(body)
}

/// Минимальная percent-encoding для параметра path в URL.
fn urlencoding(input: &str) -> String {
    let mut out = String::with_capacity(input.len() * 3);
    for b in input.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push_str(&format!("{:02X}", b));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Имя файла exe текущего процесса (тестового бинарника).
    fn current_exe_name() -> String {
        std::env::current_exe()
            .expect("current_exe")
            .file_name()
            .expect("имя файла exe")
            .to_string_lossy()
            .into_owned()
    }

    /// ТЗ фидбека КА 33–36, §2: команда в подсказках — текущий exe, а не литерал
    /// `code-index`, которого на машине с `bsl-indexer.exe` нет.
    #[test]
    fn daemon_command_names_current_exe() {
        let cmd = daemon_command("run");
        assert!(cmd.contains(&current_exe_name()), "нет имени exe: {cmd}");
        assert!(cmd.ends_with("daemon run"), "нет «daemon run»: {cmd}");
        // Литерал собран из частей, чтобы grep по исходникам давал 0.
        let literal = concat!("'", "code-index", " daemon");
        assert!(!cmd.contains(literal), "литерал code-index: {cmd}");
        assert!(daemon_command("reload").ends_with("daemon reload"));
    }

    /// daemon.json есть, процесса с его PID нет → «завершён без остановки»,
    /// команда запуска с CODE_INDEX_HOME и путь к daemon.log.
    #[test]
    fn offline_hint_detects_stale_runtime_info() {
        let tmp = tempfile::tempdir().unwrap();
        let pid = u32::MAX - 1;
        let info = RuntimeInfo {
            pid,
            version: "0.0.0".into(),
            http_host: "127.0.0.1".into(),
            http_port: 1,
            started_at: "2026-10-16T00:00:00+00:00".into(),
        };
        std::fs::write(
            tmp.path().join("daemon.json"),
            serde_json::to_string(&info).unwrap(),
        )
        .unwrap();
        assert!(!super::super::lock::is_process_alive(pid));

        let text = daemon_offline_hint_at(Some(tmp.path()), &"GET … → connection refused");
        assert!(text.contains("завершён без остановки"), "{text}");
        assert!(text.contains(&format!("PID {pid}")), "{text}");
        assert!(text.contains("2026-10-16T00:00:00+00:00"), "{text}");
        assert!(text.contains("daemon.json устарел"), "{text}");
        assert!(text.contains(&daemon_command("run")), "{text}");
        assert!(
            text.contains(&format!("CODE_INDEX_HOME={}", tmp.path().display())),
            "{text}"
        );
        assert!(text.contains("вне сессии агента"), "{text}");
        // Переносы строк в литерале не должны оставлять серий пробелов.
        assert!(!text.contains("  "), "двойные пробелы: {text}");
        let log = tmp.path().join("daemon.log").display().to_string();
        assert!(text.contains(&format!("строке ERROR в {log}")), "{text}");
    }

    /// daemon.json с PID живого процесса (нашего) → прежняя причина с текстом ошибки.
    #[test]
    fn offline_hint_keeps_error_when_process_alive() {
        let tmp = tempfile::tempdir().unwrap();
        let info = RuntimeInfo {
            pid: std::process::id(),
            version: "0.0.0".into(),
            http_host: "127.0.0.1".into(),
            http_port: 1,
            started_at: "2026-10-16T00:00:00+00:00".into(),
        };
        std::fs::write(
            tmp.path().join("daemon.json"),
            serde_json::to_string(&info).unwrap(),
        )
        .unwrap();
        let text = daemon_offline_hint_at(Some(tmp.path()), &"тайм-аут");
        assert!(
            text.starts_with("Демон code-index не доступен (тайм-аут)."),
            "{text}"
        );
        assert!(!text.contains("завершён без остановки"), "{text}");
        assert!(text.contains("daemon.log"), "{text}");
    }

    /// Нет daemon.json → ошибка запроса — это `daemon_unavailable_hint` (из
    /// `base_url`), и в ней команда запуска текущим exe.
    #[test]
    fn offline_hint_without_runtime_info_uses_unavailable_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let hint = daemon_unavailable_hint_at(Some(tmp.path()));
        assert!(hint.contains(&daemon_command("run")), "{hint}");
        assert!(hint.contains("отсутствует runtime-info файл"), "{hint}");
        assert!(!hint.contains("code-index daemon"), "{hint}");
        assert!(!hint.contains("  "), "двойные пробелы: {hint}");

        let text = daemon_offline_hint_at(Some(tmp.path()), &hint);
        assert!(
            text.starts_with(&format!("Демон code-index не доступен ({hint}).")),
            "{text}"
        );
        assert!(!text.contains("завершён без остановки"), "{text}");
        assert!(text.contains("daemon.log"), "{text}");
    }

    /// CODE_INDEX_HOME не задана → подсказка про переменную, без паники.
    #[test]
    fn offline_hint_without_home() {
        let hint = daemon_unavailable_hint_at(None);
        assert!(hint.contains("не задана"), "{hint}");
        let text = daemon_offline_hint_at(None, &hint);
        assert!(text.contains("CODE_INDEX_HOME=<не задана>"), "{text}");
        assert!(text.contains("<CODE_INDEX_HOME>/daemon.log"), "{text}");
    }
}
