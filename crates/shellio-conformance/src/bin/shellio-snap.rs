//! Съёмщик паспортов сверки (ADR-0037): запускает закреплённый `chrome-headless-shell` с новым
//! профилем, говорит с ним по CDP через loopback, загружает сцену и рецепт во вход
//! `?conformance=1` прототипа и пишет собранные факты.
//!
//! Вызов: `shellio-snap --chrome <путь> --root <репозиторий> --recipe <имя> --cell <ячейка>
//! --out <файл>`. Коды: 0 — записано, 1 — ошибка исполнения, 2 — неверный вызов.

#![forbid(unsafe_code)]

use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use shellio_conformance::passport::Cell;
use shellio_conformance::recipe::Recipe;
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

struct Args {
    chrome: PathBuf,
    root: PathBuf,
    recipe: String,
    cell: Cell,
    out: PathBuf,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let (mut chrome, mut root, mut recipe, mut cell, mut out) = (None, None, None, None, None);
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag}: нет значения"))?;
        match flag.as_str() {
            "--chrome" => chrome = Some(PathBuf::from(value)),
            "--root" => root = Some(PathBuf::from(value)),
            "--recipe" => recipe = Some(value),
            "--cell" => cell = Some(value.parse::<Cell>().map_err(|e| e.to_string())?),
            "--out" => out = Some(PathBuf::from(value)),
            other => return Err(format!("неизвестный флаг {other}")),
        }
    }
    Ok(Args {
        chrome: chrome.ok_or("нет --chrome")?,
        root: root.ok_or("нет --root")?,
        recipe: recipe.ok_or("нет --recipe")?,
        cell: cell.ok_or("нет --cell")?,
        out: out.ok_or("нет --out")?,
    })
}

/// Браузер с собственным временным профилем; профиль и процесс убираются при выходе.
struct Browser {
    child: Child,
    profile: PathBuf,
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.profile);
    }
}

fn launch(chrome: &Path) -> Result<(Browser, String), String> {
    let profile = std::env::temp_dir().join(format!("shellio-snap.{}", std::process::id()));
    let _ = fs::remove_dir_all(&profile);
    fs::create_dir_all(&profile).map_err(|e| e.to_string())?;
    let child = Command::new(chrome)
        .args([
            "--headless",
            "--remote-debugging-address=127.0.0.1",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--disable-component-update",
            "--disable-sync",
            "--hide-scrollbars",
            "--force-color-profile=srgb",
            "--font-render-hinting=none",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("about:blank")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", chrome.display()))?;
    let browser = Browser { child, profile };
    let port_file = browser.profile.join("DevToolsActivePort");
    let start = Instant::now();
    loop {
        if let Ok(text) = fs::read_to_string(&port_file) {
            let mut lines = text.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                return Ok((browser, format!("ws://127.0.0.1:{port}{path}")));
            }
        }
        if start.elapsed() > Duration::from_secs(20) {
            return Err("браузер не открыл порт отладки за 20 с".to_owned());
        }
        std::thread::park_timeout(Duration::from_millis(50));
    }
}

struct Cdp {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    next: u64,
    session: Option<String>,
    events: Vec<Value>,
}

impl Cdp {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = &self.session {
            msg["sessionId"] = json!(s);
        }
        self.ws
            .send(Message::text(msg.to_string()))
            .map_err(|e| format!("{method}: {e}"))?;
        loop {
            let reply = self.read()?;
            if reply.get("id").and_then(Value::as_u64) == Some(id) {
                if let Some(err) = reply.get("error") {
                    return Err(format!("{method}: {err}"));
                }
                return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
            }
            self.events.push(reply);
        }
    }

    fn read(&mut self) -> Result<Value, String> {
        loop {
            match self.ws.read().map_err(|e| e.to_string())? {
                Message::Text(t) => return serde_json::from_str(&t).map_err(|e| e.to_string()),
                Message::Close(_) => return Err("CDP закрыл соединение".to_owned()),
                _ => {}
            }
        }
    }

    fn wait_event(&mut self, method: &str) -> Result<Value, String> {
        if let Some(i) = self.events.iter().position(|e| e["method"] == method) {
            return Ok(self.events.remove(i));
        }
        loop {
            let event = self.read()?;
            if event["method"] == method {
                return Ok(event);
            }
            self.events.push(event);
        }
    }

    /// Выражение страницы с ожиданием обещания; результат — значение JSON.
    fn eval(&mut self, expression: &str) -> Result<Value, String> {
        let result = self.call(
            "Runtime.evaluate",
            json!({"expression": expression, "awaitPromise": true, "returnByValue": true}),
        )?;
        if let Some(ex) = result.get("exceptionDetails") {
            return Err(format!("исключение страницы: {ex}"));
        }
        Ok(result["result"]["value"].clone())
    }
}

fn snap(args: &Args) -> Result<Value, String> {
    let recipe_path = args
        .root
        .join(format!("conformance/recipes/{}.json", args.recipe));
    let recipe_text = fs::read_to_string(&recipe_path).map_err(|e| e.to_string())?;
    let recipe = Recipe::parse(&recipe_path.display().to_string(), &recipe_text)
        .map_err(|e| e.to_string())?;
    let scene_path = args
        .root
        .join(format!("conformance/scenes/{}.json", recipe.scene));
    let scene_text = fs::read_to_string(&scene_path).map_err(|e| e.to_string())?;
    let index =
        fs::canonicalize(args.root.join("prototype/index.html")).map_err(|e| e.to_string())?;

    let (_browser, url) = launch(&args.chrome)?;
    let (ws, _) = tungstenite::connect(url.as_str()).map_err(|e| e.to_string())?;
    let mut cdp = Cdp {
        ws,
        next: 0,
        session: None,
        events: Vec::new(),
    };
    let targets = cdp.call("Target.getTargets", json!({}))?;
    let page = targets["targetInfos"]
        .as_array()
        .and_then(|t| t.iter().find(|t| t["type"] == "page"))
        .and_then(|t| t["targetId"].as_str())
        .ok_or("нет страницы")?
        .to_owned();
    let attached = cdp.call(
        "Target.attachToTarget",
        json!({"targetId": page, "flatten": true}),
    )?;
    cdp.session = attached["sessionId"].as_str().map(str::to_owned);
    cdp.call("Network.enable", json!({}))?;
    cdp.call(
        "Network.setBlockedURLs",
        json!({"urls": ["http://*", "https://*"]}),
    )?;
    cdp.call(
        "Emulation.setDeviceMetricsOverride",
        json!({"width": recipe.window.width, "height": recipe.window.height,
               "deviceScaleFactor": recipe.window.dpr, "mobile": false}),
    )?;
    cdp.call("Page.enable", json!({}))?;
    cdp.call(
        "Page.navigate",
        json!({"url": format!("file://{}?conformance=1", index.display())}),
    )?;
    cdp.wait_event("Page.loadEventFired")?;
    cdp.eval("window.conformance.settle()")?;
    cdp.eval(&format!("window.conformance.load({scene_text})"))?;
    let cell = json!({
        "theme": serde_json::to_value(args.cell.theme).map_err(|e| e.to_string())?,
        "contrast": serde_json::to_value(args.cell.contrast).map_err(|e| e.to_string())?,
    });
    cdp.eval(&format!("window.conformance.apply({recipe_text}, {cell})"))?;
    cdp.eval("window.conformance.facts()")
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("shellio-snap: {e}");
            return ExitCode::from(2);
        }
    };
    match snap(&args).and_then(|facts| {
        let text = serde_json::to_string_pretty(&facts).map_err(|e| e.to_string())?;
        fs::write(&args.out, text + "\n").map_err(|e| e.to_string())
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("shellio-snap: {e}");
            ExitCode::from(1)
        }
    }
}
