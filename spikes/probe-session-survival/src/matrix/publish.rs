//! E5 на Rust-хосте (план P0.4, «Окно публикации»): хост останавливается в каждой точке окна
//! между `bind`, `listen` и публикацией ссылки, стенд в это время выполняет `list` и attach,
//! затем отпускает хост и проверяет итог. Так окно доказано, а не угадано. Отдельно — путь, где
//! имя сессии помещается в `sun_path`, а временное имя нет: хост отказывает явно, запасного пути
//! нет.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::{Command as Process, Stdio};
use std::thread;

use serde_json::json;

use crate::backend::rust::{Config, RustHost};
use crate::backend::{Backend, Lead, Liveness, SessionId, wait_child};
use crate::fixture::Mode;
use crate::host::socket::{self, MAX_SOCKET_PATH, Point};
use crate::launch;
use crate::matrix::evidence::{Evidence, Spec, cell};
use crate::matrix::stand::{STOP_GRACE, Stand, WAIT, barrier};
use crate::pty;
use crate::results::{Cell, DefectOrigin};

pub fn cells(stand: &mut Stand) -> Result<Vec<Cell>, String> {
    let mut cells = Vec::new();

    let id = stand.session("e5w")?;
    stand.sessions.push(id.clone());
    let mut evidence = Evidence::default();
    let outcome = window(stand, &id, &mut evidence);
    let notes = stand.discard(&id);
    evidence.note("cleanup", notes);
    outcome?;
    let artifact = stand.artifact("e5-window.json", &evidence)?;
    cells.push(cell(
        Spec {
            id: "E5",
            part: Some("window"),
            stimulus: "ensure с точками остановки хоста после bind и после listen; в каждой точке \
                       — сокеты каталога, list, состояние сессии и attach-клиент"
                .to_owned(),
            oracle: "список сокетов (временное имя есть, имени сессии нет), list без сессии, \
                     attach-клиент отказывает и сессию не создаёт; после публикации — одна \
                     нагрузка, сокет без временного имени, лидер и барьер через attach",
            completion: "хост прошёл обе точки, ensure вернулся, барьер через клиента пройден",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ));

    let id = stand.session("e5l")?;
    let mut evidence = Evidence::default();
    long_path(stand, &id, &mut evidence)?;
    let artifact = stand.artifact("e5-long-path.json", &evidence)?;
    cells.push(cell(
        Spec {
            id: "E5",
            part: Some("long-path"),
            stimulus: format!(
                "каталог сокетов, где имя сессии занимает ровно {MAX_SOCKET_PATH} байт пути, а \
                 временное имя .<имя>.<pid> не помещается в sun_path"
            ),
            oracle: "итог ensure, сокеты каталога, процессы с меткой нагрузки",
            completion: "ensure вернулся",
            origin: DefectOrigin::Host,
        },
        &evidence,
        vec![artifact, stand.log_name(&id)],
    ));
    Ok(cells)
}

fn rust(stand: &Stand) -> Result<&RustHost, String> {
    stand
        .rust
        .as_ref()
        .ok_or_else(|| "E5 окна — сценарий Rust-хоста".to_owned())
}

/// Следующая точка остановки хоста: соединение и имя точки.
fn next_point(listener: &UnixListener) -> Result<(UnixStream, String), String> {
    if !pty::wait_ready(listener.as_fd(), false, Some(WAIT))
        .map_err(|err| format!("ожидание точки: {err}"))?
    {
        return Err(format!("хост не дошёл до точки за {} с", WAIT.as_secs()));
    }
    let (stream, _) = listener
        .accept()
        .map_err(|err| format!("точка остановки: {err}"))?;
    let mut line = String::new();
    BufReader::new(&stream)
        .read_line(&mut line)
        .map_err(|err| format!("имя точки: {err}"))?;
    Ok((stream, line.trim().to_owned()))
}

/// Attach-клиент без PTY: в окне он должен отказать, не создав сессию.
fn attach_once(backend: &RustHost, id: &SessionId) -> Result<serde_json::Value, String> {
    let command = backend
        .attach_command(id, Lead::IfFree)
        .map_err(|err| err.to_string())?;
    let mut child = Process::new(&command.program)
        .args(&command.args)
        .env_clear()
        .envs(command.env.iter().map(|(name, value)| (name, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("attach-клиент: {err}"))?;
    let stderr = child.stderr.take();
    let status = wait_child(&mut child, WAIT).map_err(|err| err.to_string())?;
    let mut text = String::new();
    if let Some(mut pipe) = stderr {
        let _ = std::io::Read::read_to_string(&mut pipe, &mut text);
    }
    Ok(json!({ "code": status.code(), "stderr": text }))
}

fn window(stand: &Stand, id: &SessionId, evidence: &mut Evidence) -> Result<(), String> {
    let base = rust(stand)?;
    let break_path = stand.root.join("b.sock");
    let listener = UnixListener::bind(&break_path).map_err(|err| format!("сокет точек: {err}"))?;
    let backend = base.with_config(Config {
        break_socket: Some(break_path.clone()),
        ..base.config().clone()
    })?;
    let fixture_nonce = format!("{id}-w");
    let workload = stand.workload(Mode::Emitter, &fixture_nonce)?;
    let size = stand.size;
    let outcome = thread::scope(|scope| -> Result<(), String> {
        let ensure = scope.spawn(|| backend.ensure(id, &workload, size));
        let mut seen = Vec::new();
        for expected in Point::ALL {
            let (mut stream, name) = next_point(&listener)?;
            let point = name.clone();
            seen.push(name.clone());
            let sockets = backend
                .socket_entries()
                .map_err(|err| format!("сокеты: {err}"))?;
            let listed = backend.list().map_err(|err| err.to_string())?;
            let liveness = backend.liveness(id).map_err(|err| err.to_string())?;
            let attach = attach_once(&backend, id)?;
            evidence.note(
                &format!("point.{point}"),
                json!({
                    "sockets": sockets,
                    "list": listed,
                    "liveness": liveness,
                    "attach": attach,
                }),
            );
            evidence.check(
                &format!("{point}: точка по порядку"),
                Point::from_name(&name) == Some(expected),
                format!("пришла «{name}», ожидалась «{}»", expected.name()),
            );
            evidence.check(
                &format!("{point}: имени сессии нет, временное есть"),
                !sockets.sessions.iter().any(|name| name == id.as_str())
                    && sockets.staged.len() == 1
                    && sockets.staged[0].starts_with(&format!(".{id}.")),
                format!("{sockets:?}"),
            );
            evidence.check(
                &format!("{point}: list без сессии"),
                !listed.iter().any(|entry| entry.name == id.as_str()),
                format!("{listed:?}"),
            );
            evidence.check(
                &format!("{point}: сессии нет"),
                liveness == Liveness::Absent,
                format!("{liveness:?}"),
            );
            evidence.check(
                &format!("{point}: attach отказал явно"),
                attach["code"] == json!(1)
                    && attach["stderr"]
                        .as_str()
                        .is_some_and(|text| text.contains("нет сессии")),
                attach.to_string(),
            );
            stream
                .write_all(b"g")
                .map_err(|err| format!("отпустить хост: {err}"))?;
        }
        evidence.note("points", &seen);
        let ensured = ensure
            .join()
            .map_err(|_| "поток ensure упал".to_owned())?
            .map_err(|err| format!("ensure: {err}"))?;
        evidence.note("ensured", &ensured);
        evidence.check(
            "ensure создал сессию",
            ensured.created,
            format!("{:?}", ensured.record),
        );
        Ok(())
    });
    let _ = fs::remove_file(&break_path);
    outcome?;
    let (mut link, ready) = stand.accept(Some(&fixture_nonce))?;
    evidence.note("ready", &ready);
    let record = base
        .record(id)
        .or_else(|| backend.record(id))
        .ok_or("нет записи сессии после ensure")?;
    let sockets = backend
        .socket_entries()
        .map_err(|err| format!("сокеты: {err}"))?;
    evidence.check(
        "после публикации: имя сессии без временного",
        sockets.sessions.iter().any(|name| name == id.as_str()) && sockets.staged.is_empty(),
        format!("{sockets:?}"),
    );
    let listed = backend.list().map_err(|err| err.to_string())?;
    evidence.check(
        "после публикации: list видит нагрузку",
        listed.iter().any(|entry| {
            entry.name == id.as_str() && entry.workload_pid == Some(record.workload.pid)
        }),
        format!("{listed:?}"),
    );
    let client = stand.client_on(&backend, id, "c1", Lead::IfFree, stand.size)?;
    if evidence.require("c1 лидер", client.wait_leader()).is_some() {
        evidence.require("барьер через c1", barrier(&mut link, &client));
    }
    drop(client);
    let stopped = backend
        .stop(id, STOP_GRACE)
        .map_err(|err| format!("stop: {err}"))?;
    evidence.note("stop", &stopped);
    Ok(())
}

fn long_path(stand: &Stand, id: &SessionId, evidence: &mut Evidence) -> Result<(), String> {
    let base = rust(stand)?;
    let root_len = stand.root.as_os_str().len() + 1;
    let dir_len = MAX_SOCKET_PATH - 1 - id.as_str().len();
    if dir_len <= root_len {
        return Err(format!(
            "корень стенда {} длиннее каталога E5 ({dir_len} байт)",
            stand.root.display()
        ));
    }
    let dir = stand.root.join("l".repeat(dir_len - root_len));
    fs::create_dir(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let final_path = dir.join(id.as_str());
    evidence.note(
        "paths",
        json!({
            "final_len": final_path.as_os_str().len(),
            "temp_min_len": final_path.as_os_str().len() + 3,
        }),
    );
    let backend = base.with_config(Config {
        dir: dir.clone(),
        ..base.config().clone()
    })?;
    let fixture_nonce = format!("{id}-w");
    let workload = stand.workload(Mode::Emitter, &fixture_nonce)?;
    let outcome = backend.ensure(id, &workload, stand.size);
    evidence.note("ensure", format!("{outcome:?}"));
    evidence.check(
        "ensure отказал явно из-за длины пути",
        matches!(&outcome, Err(err) if err.to_string().contains("публикация невозможна")),
        format!("{outcome:?}"),
    );
    let sockets = socket::entries(&dir).map_err(|err| format!("сокеты: {err}"))?;
    evidence.check(
        "сокетов нет",
        sockets.sessions.is_empty() && sockets.staged.is_empty(),
        format!("{sockets:?}"),
    );
    let running = launch::find_by_token(&fixture_nonce);
    evidence.check(
        "нагрузка не запускалась и хоста нет",
        running.is_empty(),
        format!(
            "{:?}",
            running
                .iter()
                .map(|process| (process.pid, process.command.clone()))
                .collect::<Vec<_>>()
        ),
    );
    if Path::new(&final_path).exists() {
        let _ = fs::remove_file(&final_path);
    }
    Ok(())
}
