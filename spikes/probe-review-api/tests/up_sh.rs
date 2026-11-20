//! `gitlab/up.sh` с подставной командой docker (`PROBE_REVIEW_DOCKER`): `not_run` при недоступном
//! docker, малой памяти, остановившемся контейнере и истёкшем сроке, аргументы `docker run` и
//! готовность через порт хоста против loopback-сервера. Срок готовности 0 с: опрос делает одну
//! попытку и не ждёт.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

type TestResult = Result<(), String>;

const IMAGE: &str = "gitlab/gitlab-ce@sha256:0000";
const RUN_ID: &str = "20261120-000000-1";
const PASSWORD: &str = "sentinel-root-password-7f3a";
const SEP: char = '\u{1f}';

/// Подставной docker: пишет аргументы каждого вызова строкой через `\x1f`, значение
/// `GITLAB_ROOT_PASSWORD` у `run -d` — отдельным файлом; поведение задаёт окружение `FAKE_*`.
const FAKE_DOCKER: &str = r#"#!/bin/bash
log="$FAKE_DIR/calls"
( IFS=$'\x1f'; printf '%s\n' "$*" ) >>"$log"
case "$1" in
info) [ "${FAKE_INFO:-ok}" = ok ] ;;
image)
    if [ "${3:-}" = --format ]; then echo "${FAKE_ARCH:-linux/arm64}"; fi ;;
pull) exit 1 ;;
run)
    if [ "$2" = --rm ]; then
        echo "MemAvailable:   ${FAKE_MEM_KB:-11000000} kB"
    else
        printf '%s' "${GITLAB_ROOT_PASSWORD:-}" >"$FAKE_DIR/run-password"
        echo fakecontainerid
    fi ;;
port) echo "127.0.0.1:${FAKE_PORT:-1}" ;;
inspect) echo "${FAKE_STATE:-running}" ;;
exec) [ "${FAKE_READY:-0}" = 1 ] ;;
logs) echo "fake container log line" ;;
*) exit 64 ;;
esac
"#;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("gitlab/up.sh")
}

struct Stand {
    _root: tempfile::TempDir,
    fake: PathBuf,
    docker: PathBuf,
    out: PathBuf,
}

impl Stand {
    fn new() -> Result<Stand, String> {
        let root = tempfile::tempdir().map_err(|e| e.to_string())?;
        let fake = root.path().join("fake");
        std::fs::create_dir(&fake).map_err(|e| e.to_string())?;
        let docker = root.path().join("docker");
        std::fs::write(&docker, FAKE_DOCKER).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
        let out = root.path().join("gitlab-ce");
        Ok(Stand {
            _root: root,
            fake,
            docker,
            out,
        })
    }

    fn run(&self, docker: &Path, envs: &[(&str, &str)]) -> Result<Output, String> {
        let mut command = Command::new(script());
        command
            .arg(IMAGE)
            .arg(RUN_ID)
            .arg(&self.out)
            .env("PROBE_REVIEW_DOCKER", docker)
            .env("PROBE_REVIEW_GITLAB_DEADLINE", "0")
            .env("GITLAB_ROOT_PASSWORD", PASSWORD)
            .env("FAKE_DIR", &self.fake);
        for (key, value) in envs {
            command.env(key, value);
        }
        command
            .output()
            .map_err(|e| format!("up.sh не запускается: {e}"))
    }

    fn fake_run(&self, envs: &[(&str, &str)]) -> Result<Output, String> {
        self.run(&self.docker, envs)
    }

    fn read(&self, name: &str) -> Option<String> {
        std::fs::read_to_string(self.out.join(name)).ok()
    }

    fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.fake.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(|line| line.split(SEP).map(str::to_owned).collect())
            .collect()
    }

    fn call(&self, first: &str, second: &str) -> Option<Vec<String>> {
        self.calls().into_iter().find(|args| {
            args.first().map(String::as_str) == Some(first)
                && args.get(1).map(String::as_str) == Some(second)
        })
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Код 0, записан `not_run` с ожидаемым началом причины, `url` нет.
fn assert_not_run(stand: &Stand, output: &Output, reason: &str) -> TestResult {
    if output.status.code() != Some(0) {
        return Err(format!(
            "код {:?}: {}",
            output.status.code(),
            stderr(output)
        ));
    }
    let written = stand
        .read("not_run")
        .ok_or_else(|| format!("нет not_run: {}", stderr(output)))?;
    if !written.starts_with(reason) {
        return Err(format!("not_run «{}», ждали «{reason}…»", written.trim()));
    }
    if stand.read("url").is_some() {
        return Err("url записан вместе с not_run".to_owned());
    }
    Ok(())
}

fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
    args.windows(2)
        .any(|pair| pair[0] == flag && pair[1] == value)
}

#[test]
fn missing_or_silent_docker_is_not_run() -> TestResult {
    let stand = Stand::new()?;
    let output = stand.run(Path::new("/nonexistent/docker"), &[])?;
    assert_not_run(&stand, &output, "docker недоступен: команды docker нет")?;

    let stand = Stand::new()?;
    let output = stand.fake_run(&[("FAKE_INFO", "down")])?;
    assert_not_run(&stand, &output, "docker недоступен: docker info не ответил")?;
    assert!(stand.call("run", "-d").is_none(), "контейнер не запускался");
    Ok(())
}

#[test]
fn low_vm_memory_is_not_run_without_a_container() -> TestResult {
    let stand = Stand::new()?;
    let output = stand.fake_run(&[("FAKE_MEM_KB", "4000000")])?;
    assert_not_run(&stand, &output, "мало памяти ВМ docker")?;
    let probe = stand.call("run", "--rm").ok_or("память не читалась")?;
    assert!(
        has_pair(&probe, "--label", &format!("shellio.probe.run-id={RUN_ID}")),
        "контейнер замера памяти без метки прогона: {probe:?}"
    );
    assert!(stand.call("run", "-d").is_none(), "контейнер не запускался");
    Ok(())
}

#[test]
fn stopped_container_is_not_run_with_saved_log() -> TestResult {
    let stand = Stand::new()?;
    let output = stand.fake_run(&[("FAKE_STATE", "exited")])?;
    assert_not_run(
        &stand,
        &output,
        "GitLab не поднялся: контейнер в состоянии exited",
    )?;
    let log = stand.read("container.log").ok_or("нет container.log")?;
    assert!(log.contains("fake container log line"), "{log}");
    Ok(())
}

#[test]
fn expired_deadline_is_not_run_with_saved_log() -> TestResult {
    let stand = Stand::new()?;
    let output = stand.fake_run(&[("FAKE_READY", "0")])?;
    assert_not_run(&stand, &output, "GitLab не поднялся за 0 с")?;
    let log = stand.read("container.log").ok_or("нет container.log")?;
    assert!(log.contains("fake container log line"), "{log}");
    Ok(())
}

#[test]
fn docker_run_carries_label_loopback_port_volumes_and_no_password() -> TestResult {
    let stand = Stand::new()?;
    let output = stand.fake_run(&[("FAKE_READY", "0")])?;
    assert_not_run(&stand, &output, "GitLab не поднялся")?;
    let run = stand
        .call("run", "-d")
        .ok_or("docker run -d не вызывался")?;
    assert!(
        has_pair(&run, "--label", &format!("shellio.probe.run-id={RUN_ID}")),
        "{run:?}"
    );
    assert!(has_pair(&run, "-p", "127.0.0.1::80"), "{run:?}");
    assert!(has_pair(&run, "-e", "GITLAB_ROOT_PASSWORD"), "{run:?}");
    assert!(has_pair(&run, "--platform", "linux/arm64"), "{run:?}");
    let out = stand.out.to_string_lossy().into_owned();
    for (volume, target) in [
        ("config", "/etc/gitlab"),
        ("logs", "/var/log/gitlab"),
        ("data", "/var/opt/gitlab"),
    ] {
        let mount = format!("{out}/volumes/{volume}:{target}");
        assert!(has_pair(&run, "-v", &mount), "нет тома {mount}: {run:?}");
        assert!(stand.out.join("volumes").join(volume).is_dir());
    }
    assert_eq!(run.last().map(String::as_str), Some(IMAGE));
    for args in stand.calls() {
        assert!(
            args.iter().all(|arg| !arg.contains(PASSWORD)),
            "пароль root в аргументах docker: {args:?}"
        );
    }
    let passed =
        std::fs::read_to_string(stand.fake.join("run-password")).map_err(|e| e.to_string())?;
    assert_eq!(passed, PASSWORD, "пароль root идёт окружением docker run");
    Ok(())
}

/// Loopback-сервер, который на каждый запрос отвечает `status`; поток живёт до конца процесса.
fn loopback(status: &'static str) -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => request.extend_from_slice(&buf[..n]),
                }
            }
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        }
    });
    Ok(port)
}

#[test]
fn readiness_through_host_port_answering_401_writes_url() -> TestResult {
    let port = loopback("401 Unauthorized")?.to_string();
    let stand = Stand::new()?;
    let output = stand.fake_run(&[("FAKE_READY", "1"), ("FAKE_PORT", &port)])?;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(
        stand.read("url").as_deref().map(str::trim),
        Some(format!("http://127.0.0.1:{port}").as_str()),
        "{}",
        stderr(&output)
    );
    assert!(stand.read("not_run").is_none());
    assert!(
        stand.read("container.log").is_none(),
        "журнал при успехе не сохраняется"
    );
    Ok(())
}

#[test]
fn host_port_answering_502_is_not_readiness() -> TestResult {
    let port = loopback("502 Bad Gateway")?.to_string();
    let stand = Stand::new()?;
    let output = stand.fake_run(&[("FAKE_READY", "1"), ("FAKE_PORT", &port)])?;
    assert_not_run(&stand, &output, "GitLab не поднялся за 0 с")
}

#[test]
fn missing_root_password_is_execution_error() -> TestResult {
    let stand = Stand::new()?;
    let output = stand.fake_run(&[("GITLAB_ROOT_PASSWORD", "")])?;
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stand.calls().is_empty(), "docker вызывался без пароля");
    Ok(())
}
