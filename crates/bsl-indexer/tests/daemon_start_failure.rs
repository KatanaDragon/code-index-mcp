// Отказ старта демона должен оставлять причину в daemon.log.
//
// Отвязанный от консоли демон (daemon-hidden.vbs → daemon-run.cmd) пишет
// stderr в никуда: до правки ТЗ «фидбек КА 33–36» §1 при дубле index_dir в
// daemon.toml он молча завершался после трёх строк старта, и причину находили
// только ручным запуском `daemon run` в консоли.
//
// Проверяем отдельным процессом: CODE_INDEX_HOME читается из окружения, и
// менять его внутри процесса тестов нельзя — тесты идут параллельно.

use std::fs;
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Дольше этого демон, упавший на старте, жить не должен.
const ТАЙМАУТ: Duration = Duration::from_secs(10);

/// Запустить `bsl-indexer daemon run` с CODE_INDEX_HOME=`home` без
/// самоперезапуска в фон и дождаться выхода. Не вышел за [`ТАЙМАУТ`] —
/// процесс снимается, тест падает.
fn запустить_демон(home: &Path) -> ExitStatus {
    let stderr = fs::File::create(home.join("stderr.txt")).expect("stderr.txt");
    let mut child = Command::new(env!("CARGO_BIN_EXE_bsl-indexer"))
        .args(["daemon", "run"])
        .env("CODE_INDEX_HOME", home)
        .env("CODE_INDEX_DAEMON_DETACHED", "1")
        // RUST_LOG из окружения разработчика мог бы заглушить ERROR.
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("запуск bsl-indexer");

    let начало = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if начало.elapsed() > ТАЙМАУТ {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "демон не завершился за {:?}; журнал:\n{}",
                ТАЙМАУТ,
                прочитать(&home.join("daemon.log"))
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn прочитать(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// Строки журнала с уровнем ERROR.
fn строки_error(журнал: &str) -> Vec<&str> {
    журнал.lines().filter(|l| l.contains(" ERROR ")).collect()
}

/// Литеральная строка TOML: обратные слэши Windows в ней не экранируются.
fn toml_путь(p: &Path) -> String {
    format!("'{}'", p.display())
}

#[test]
fn дубль_index_dir_пишется_в_журнал_с_путём_конфига() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();
    let репо_а = home.join("repo_a");
    let репо_б = home.join("repo_b");
    let индекс = home.join("indexes").join("общий");
    fs::create_dir_all(&репо_а).unwrap();
    fs::create_dir_all(&репо_б).unwrap();

    let конфиг = home.join("daemon.toml");
    fs::write(
        &конфиг,
        format!(
            "[daemon]\nhttp_port = 0\n\n\
             [[paths]]\npath = {а}\nindex_dir = {и}\n\n\
             [[paths]]\npath = {б}\nindex_dir = {и}\n",
            а = toml_путь(&репо_а),
            б = toml_путь(&репо_б),
            и = toml_путь(&индекс),
        ),
    )
    .unwrap();

    let status = запустить_демон(home);
    let журнал = прочитать(&home.join("daemon.log"));
    assert!(
        !status.success(),
        "демон с дублем index_dir должен завершиться с ошибкой; журнал:\n{}",
        журнал
    );

    let ошибки = строки_error(&журнал);
    let строка = ошибки
        .iter()
        .find(|l| l.contains("демон не запущен"))
        .unwrap_or_else(|| panic!("нет строки ERROR «демон не запущен»; журнал:\n{}", журнал));
    assert!(
        строка.contains("Один index_dir задан для нескольких путей"),
        "в строке ERROR нет причины: {}",
        строка
    );
    assert!(
        строка.contains(&конфиг.display().to_string()),
        "в строке ERROR нет пути daemon.toml ({}): {}",
        конфиг.display(),
        строка
    );
    // До загрузки конфига демон не дошёл — строки «конфиг: …» быть не должно.
    assert!(
        !журнал.contains("конфиг: "),
        "журнал не должен содержать строку успешной загрузки конфига:\n{}",
        журнал
    );
    assert!(
        !home.join("daemon.json").exists(),
        "daemon.json не должен создаваться при отказе старта"
    );
    // PID-lock отпущен при выходе — следующий старт не наткнётся на него.
    assert!(
        !home.join("daemon.pid").exists(),
        "daemon.pid должен удаляться при отказе старта"
    );
}

#[test]
fn занятый_pid_lock_пишется_в_журнал() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path();
    // Формат lock.rs: в файле только десятичный PID. PID процесса тестов
    // заведомо жив, поэтому демон обязан отказаться стартовать.
    let pid_файл = home.join("daemon.pid");
    let наш_pid = std::process::id();
    fs::write(&pid_файл, наш_pid.to_string()).unwrap();

    let status = запустить_демон(home);
    let журнал = прочитать(&home.join("daemon.log"));
    assert!(
        !status.success(),
        "демон при занятом PID-lock должен завершиться с ошибкой; журнал:\n{}",
        журнал
    );

    let ошибки = строки_error(&журнал);
    let строка = ошибки
        .iter()
        .find(|l| l.contains("демон не запущен"))
        .unwrap_or_else(|| panic!("нет строки ERROR «демон не запущен»; журнал:\n{}", журнал));
    assert!(
        строка.contains("уже запущен") && строка.contains(&format!("PID {}", наш_pid)),
        "в строке ERROR нет причины из lock::acquire: {}",
        строка
    );
    assert!(
        !home.join("daemon.json").exists(),
        "daemon.json не должен создаваться при отказе старта"
    );
    // Чужой PID-файл отказавший демон трогать не должен.
    assert_eq!(прочитать(&pid_файл).trim(), наш_pid.to_string());
}
