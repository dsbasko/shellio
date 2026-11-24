//! Коды цели `make e2e` и разбор `result.json` сценариев: `scripts/e2e/codes.sh` отдельным
//! процессом на временных файлах.

use std::path::{Path, PathBuf};
use std::process::Command;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/e2e/codes.sh")
}

fn codes(args: &[&str]) -> Result<(i32, String, String)> {
    let output = Command::new("/bin/bash")
        .arg(script())
        .args(args)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()?;
    Ok((
        output.status.code().ok_or("codes.sh убит сигналом")?,
        String::from_utf8(output.stdout)?.trim_end().to_owned(),
        String::from_utf8(output.stderr)?,
    ))
}

fn combine(list: &[&str]) -> Result<String> {
    let mut args = vec!["combine"];
    args.extend_from_slice(list);
    let (code, stdout, stderr) = codes(&args)?;
    assert_eq!(code, 0, "{stderr}");
    Ok(stdout)
}

/// Код цели — наибольший из кодов запущенных сценариев по порядку 1 > 3 > 0.
#[test]
fn target_code_orders_one_over_three_over_zero() -> Result<()> {
    assert_eq!(combine(&["0", "0", "0"])?, "0");
    assert_eq!(combine(&["0", "3", "0"])?, "3");
    assert_eq!(combine(&["3", "0", "1"])?, "1");
    assert_eq!(combine(&["1", "3"])?, "1");
    assert_eq!(combine(&["0", "0", "3"])?, "3");
    // Неизвестный код сценария (126 — нет права исполнения) — ошибка исполнения.
    assert_eq!(combine(&["0", "126"])?, "1");
    assert_eq!(codes(&["combine"])?.0, 2);
    Ok(())
}

/// Четыре сценария цели: код четвёртого (`plugins.sh`) входит в итог по тому же порядку, а без
/// него итог считается по трём запущенным.
#[test]
fn target_code_of_four_scenarios_includes_the_fourth() -> Result<()> {
    assert_eq!(combine(&["0", "0", "0", "0"])?, "0");
    assert_eq!(combine(&["0", "0", "0", "3"])?, "3");
    assert_eq!(combine(&["0", "0", "0", "1"])?, "1");
    assert_eq!(combine(&["0", "0", "3", "1"])?, "1");
    assert_eq!(combine(&["3", "3", "3", "0"])?, "3");
    assert_eq!(combine(&["0", "3", "0", "3"])?, "3");
    // Третий сценарий с кодом 1 четвёртого не запускает: итог — по трём кодам.
    assert_eq!(codes(&["next", "1"])?.1, "1");
    assert_eq!(combine(&["0", "3", "1"])?, "1");
    // После кода 0 или 3 третьего четвёртый запускается.
    assert_eq!(codes(&["next", "0"])?.1, "0");
    assert_eq!(codes(&["next", "3"])?.1, "0");
    Ok(())
}

/// Рецепт `e2e` запускает четыре сценария в порядке матрицы: каждый следующий — за проверкой
/// `next` кода предыдущего, итог — `combine` всех четырёх кодов.
#[test]
fn makefile_runs_four_scenarios_in_order() -> Result<()> {
    let makefile =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Makefile"))?;
    for (name, script) in [
        ("E2E_SINGLE_INSTANCE", "single-instance.sh"),
        ("E2E_APPKIT", "appkit.sh"),
        ("E2E_LAYOUT", "layout.sh"),
        ("E2E_PLUGINS", "plugins.sh"),
    ] {
        let line = format!("{name} := scripts/e2e/{script}\n");
        assert!(makefile.contains(&line), "нет переменной {name}");
    }
    let start = makefile
        .find("\nlocked-e2e:\n")
        .ok_or("нет цели locked-e2e")?;
    let recipe = &makefile[start + 1..];
    let recipe = &recipe[..recipe.find("\n\n").unwrap_or(recipe.len())];
    let order = [
        "$(E2E_SINGLE_INSTANCE) |",
        "$(E2E_CODES) next $$first",
        "$(E2E_APPKIT) --single-instance",
        "$(E2E_CODES) next $$second",
        "$(E2E_LAYOUT) ||",
        "$(E2E_CODES) next $$third",
        "$(E2E_PLUGINS) ||",
        "$(E2E_CODES) combine $$first $$second $$third $$fourth",
    ];
    let mut from = 0;
    for step in order {
        let at = recipe[from..]
            .find(step)
            .ok_or_else(|| format!("в рецепте e2e нет шага «{step}» после предыдущих"))?;
        from += at + step.len();
    }
    Ok(())
}

/// Следующий сценарий запускается после кода 0 или 3 предыдущего и не запускается после 1.
#[test]
fn next_scenario_runs_after_zero_or_three() -> Result<()> {
    for (code, want) in [("0", "0"), ("3", "0"), ("1", "1"), ("2", "1"), ("126", "1")] {
        assert_eq!(codes(&["next", code])?.1, want, "после кода {code}");
    }
    Ok(())
}

#[test]
fn result_path_is_taken_from_the_last_summary_line() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let log = dir.path().join("log");
    std::fs::write(
        &log,
        "шум\nитог: код 3, result.json — /tmp/a/result.json\nещё\n\
         итог: код 0, result.json — /tmp/b/result.json\n",
    )?;
    let (code, stdout, _) = codes(&["result", &log.to_string_lossy()])?;
    assert_eq!((code, stdout.as_str()), (0, "/tmp/b/result.json"));
    std::fs::write(&log, "итога нет\n")?;
    assert_eq!(codes(&["result", &log.to_string_lossy()])?.1, "");
    Ok(())
}

fn result_json(exit: u8, error: Option<&str>, items: &str) -> String {
    let error = error.map_or("null".to_owned(), |e| format!("\"{e}\""));
    format!(
        r#"{{"scenario": "layout", "run_dir": "/tmp/r", "bundle": "/tmp/shellio.app",
            "exit": {exit}, "error": {error}, "items": [{items}]}}"#
    )
}

const PASS: &str = r#"{"item": "layout-default", "required": true, "status": "pass", "reason": "ок", "raw": "layout-default"}"#;
const FAIL: &str = r#"{"item": "move-cli", "required": true, "status": "fail", "reason": "нет", "raw": "move-cli"}"#;
const OPTIONAL: &str = r#"{"item": "voiceover-speech", "required": false, "status": "not_run", "reason": "речь", "raw": "voiceover-speech"}"#;

fn check(text: &str) -> Result<(i32, String)> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("result.json");
    std::fs::write(&file, text)?;
    let (code, _, stderr) = codes(&["check", &file.to_string_lossy()])?;
    Ok((code, stderr))
}

/// `result.json` разбирается: поля, статусы, код выхода по обязательным пунктам и ошибке.
#[test]
fn result_json_is_parsed_and_its_exit_matches_the_items() -> Result<()> {
    assert_eq!(
        check(&result_json(0, None, &format!("{PASS}, {OPTIONAL}")))?.0,
        0
    );
    assert_eq!(
        check(&result_json(3, None, &format!("{PASS}, {FAIL}")))?.0,
        0
    );
    assert_eq!(
        check(&result_json(1, Some("предусловие: AeroSpace"), PASS))?.0,
        0
    );

    let (code, stderr) = check(&result_json(0, None, &format!("{PASS}, {FAIL}")))?;
    assert_eq!(code, 1);
    assert!(stderr.contains("ожидался 3"), "{stderr}");
    let (code, stderr) = check(&result_json(0, Some("сигнал"), PASS))?;
    assert_eq!(code, 1);
    assert!(stderr.contains("ожидался 1"), "{stderr}");
    let (code, stderr) = check(&result_json(0, None, &PASS.replace("pass", "ok")))?;
    assert_eq!(code, 1);
    assert!(stderr.contains("не pass, fail или not_run"), "{stderr}");
    let (code, stderr) = check(&result_json(0, None, &format!("{PASS}, {PASS}")))?;
    assert_eq!(code, 1);
    assert!(stderr.contains("дважды"), "{stderr}");
    let (code, stderr) = check(&result_json(0, None, &PASS.replace("\"raw\"", "\"rav\"")))?;
    assert_eq!(code, 1);
    assert!(stderr.contains("нет поля raw"), "{stderr}");
    assert_eq!(check(&result_json(0, None, ""))?.0, 1);
    assert_eq!(check("не json")?.0, 1);
    Ok(())
}
