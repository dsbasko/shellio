//! Скрипты наборов пробы на поддельных исходниках и Zig, без сети и без настоящих наборов:
//! `scripts/test-zmx.sh` проверяет штамп, сверку, публикацию, блокировку и правила комбинаций zmx.

#[cfg(test)]
mod set_scripts {
    use std::path::Path;
    use std::process::Command;

    #[test]
    fn zmx_sets() {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/test-zmx.sh");
        let output = Command::new(&script)
            .output()
            .expect("test-zmx.sh не запускается");
        assert!(
            output.status.success(),
            "{} завершился с {}\n{}\n{}",
            script.display(),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
