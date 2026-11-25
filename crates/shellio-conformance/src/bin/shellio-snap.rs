//! Съёмщик паспортов сверки (ADR-0037): запускает закреплённый `chrome-headless-shell` с новым
//! профилем, говорит с ним по CDP через loopback, загружает сцену и рецепт во вход
//! `?conformance=1` прототипа и пишет канонический паспорт.
//!
//! Вызов: `shellio-snap --chrome <путь> --root <репозиторий> --recipe <имя> --cell <ячейка>
//! --out <файл> [--record <файл>] [--surface <поверхность>]`. Без `--surface` в паспорт идут элементы
//! всего окна, с ним — только ключи поверхности состояния. Маски значков ложатся рядом: `<файл без .json>.icons/`.
//! `--record` записывает все ответы браузера ([`cdp::Capture`]) для тестов на записанных ответах.
//! `calibrate --chrome <c> --root <р> --out <каталог>` — сторона прототипа калибровки допусков.
//! Коды: 0 — записано, 1 — ошибка исполнения, 2 — неверный вызов.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use shellio_conformance::calibration;
use shellio_conformance::cdp;
use shellio_conformance::fields::ElementKind;
use shellio_conformance::mask::{ICON_FG, Mask, PixelClip};
use shellio_conformance::passport::{
    Cell, Contrast, Passport, Rect, Rgba, Theme, Window, canonical_json,
};
use shellio_conformance::recipe::Recipe;
use shellio_conformance::registry::Inventory;
use shellio_conformance::registry::Registry;
use shellio_conformance::registry::Surface;
use shellio_conformance::scene::Scene;
use shellio_conformance::validity::{Inputs, check_registry};
use tungstenite::{Message, WebSocket, stream::MaybeTlsStream};

struct Args {
    chrome: PathBuf,
    root: PathBuf,
    recipe: String,
    cell: Cell,
    out: PathBuf,
    record: Option<PathBuf>,
    surface: Surface,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let (mut chrome, mut root, mut recipe, mut cell, mut out) = (None, None, None, None, None);
    let mut record = None;
    let mut surface = Surface::Layout;
    while let Some(flag) = it.next() {
        let value = it.next().ok_or_else(|| format!("{flag}: нет значения"))?;
        match flag.as_str() {
            "--chrome" => chrome = Some(PathBuf::from(value)),
            "--root" => root = Some(PathBuf::from(value)),
            "--recipe" => recipe = Some(value),
            "--cell" => cell = Some(value.parse::<Cell>().map_err(|e| e.to_string())?),
            "--out" => out = Some(PathBuf::from(value)),
            "--record" => record = Some(PathBuf::from(value)),
            "--surface" => {
                surface = serde_json::from_value(json!(value))
                    .map_err(|_| format!("неизвестная поверхность {value}"))?;
            }
            other => return Err(format!("неизвестный флаг {other}")),
        }
    }
    Ok(Args {
        chrome: chrome.ok_or("нет --chrome")?,
        root: root.ok_or("нет --root")?,
        recipe: recipe.ok_or("нет --recipe")?,
        cell: cell.ok_or("нет --cell")?,
        out: out.ok_or("нет --out")?,
        record,
        surface,
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

/// Браузер с новым профилем и сеанс CDP его единственной страницы.
fn open_page(chrome: &Path) -> Result<(Browser, Cdp), String> {
    let (browser, url) = launch(chrome)?;
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
    Ok((browser, cdp))
}

/// Разметка обычного мира без ключей сверки и значков: `body` без скриптов, без `data-ck` и
/// `data-icon`; порядковые id (`<префикс>-<n>`) заменены на `<префикс>-#`.
const WORLD_DOM: &str = r#"(() => {
  const body = document.body.cloneNode(true);
  body.querySelectorAll('script').forEach((e) => e.remove());
  body.querySelectorAll('[data-ck]').forEach((e) => e.removeAttribute('data-ck'));
  body.querySelectorAll('[data-icon]').forEach((e) => e.removeAttribute('data-icon'));
  return body.innerHTML.replace(/\b([a-z]+)-\d+\b/g, '$1-#');
})()"#;

/// `world --chrome <c> --index <index.html> --out <каталог>`: обычный мир прототипа без
/// `?conformance=1` (с `?still`, такт агентов стоит). Стартовый мир — `start.html`, каждый
/// сценарий `S.scenarios` в свежей странице — `<id>.html`, исключение сценария — текстом в файле.
/// Сравнение двух копий прототипа — `diff -r` каталогов.
fn world(chrome: &Path, index: &Path, out: &Path) -> Result<(), String> {
    let index = fs::canonicalize(index).map_err(|e| format!("{}: {e}", index.display()))?;
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let (_browser, mut cdp) = open_page(chrome)?;
    cdp.call(
        "Emulation.setDeviceMetricsOverride",
        json!({"width": 1180, "height": 720, "deviceScaleFactor": 2, "mobile": false}),
    )?;
    cdp.call("Page.enable", json!({}))?;
    let frames = "new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(() => requestAnimationFrame(r))))";
    let fresh = |cdp: &mut Cdp| -> Result<(), String> {
        cdp.call(
            "Page.navigate",
            json!({"url": format!("file://{}?still", index.display())}),
        )?;
        cdp.wait_event("Page.loadEventFired")?;
        cdp.eval(frames)?;
        Ok(())
    };
    fresh(&mut cdp)?;
    let start = cdp.eval(WORLD_DOM)?;
    fs::write(out.join("start.html"), start.as_str().unwrap_or_default())
        .map_err(|e| e.to_string())?;
    let ids = cdp.eval("S.scenarios.map((s) => s.id)")?;
    for id in ids
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        fresh(&mut cdp)?;
        let run = format!(
            "(() => {{ S.scenarios.find((s) => s.id === {id:?}).run(); S.renderNow(); }})()"
        );
        let text = match cdp.eval(&run).and_then(|_| cdp.eval(frames)) {
            Ok(_) => cdp.eval(WORLD_DOM)?.as_str().unwrap_or_default().to_owned(),
            Err(e) => format!("исключение: {e}"),
        };
        fs::write(out.join(format!("{id}.html")), text).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn snap(args: &Args) -> Result<(String, BTreeMap<String, Vec<u8>>), String> {
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

    let (_browser, mut cdp) = open_page(&args.chrome)?;
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
    let facts = cdp.eval("window.conformance.facts()")?;
    let capture = capture(&mut cdp, facts, f64::from(recipe.window.dpr))?;
    if let Some(path) = &args.record {
        let text = serde_json::to_string_pretty(&capture).map_err(|e| e.to_string())?;
        fs::write(path, text + "\n").map_err(|e| format!("{}: {e}", path.display()))?;
    }

    let scene =
        Scene::parse(&scene_path.display().to_string(), &scene_text).map_err(|e| e.to_string())?;
    let assembled = cdp::assemble_surface(args.surface, &scene, &recipe, &capture)?;
    let inputs = Inputs::from_root(&args.root).map_err(|e| e.to_string())?;
    let passport = Passport {
        validity: inputs
            .expected(&args.recipe, args.cell)
            .map_err(|e| e.to_string())?,
        window: Window {
            width: f64::from(recipe.window.width),
            height: f64::from(recipe.window.height),
            dpr: f64::from(recipe.window.dpr),
        },
        composition: assembled.composition,
        elements: assembled.elements,
    };
    passport.check()?;
    Ok((
        passport.to_canonical().map_err(|e| e.to_string())?,
        assembled.masks,
    ))
}

/// Узлы документа с ключом: ключ → `nodeId` и псевдоэлементы `::before` и `::after` в порядке
/// документа. Один `DOM.getDocument` на съём: следующий сделал бы эти `nodeId` негодными.
fn keyed_nodes(
    node: &Value,
    owner: Option<&str>,
    keys: &mut BTreeMap<String, u64>,
    pseudo: &mut Vec<(String, String, u64)>,
) {
    let attrs: Vec<&str> = node["attributes"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let ck = attrs
        .chunks(2)
        .find(|pair| pair.first() == Some(&"data-ck"))
        .and_then(|pair| pair.get(1).copied());
    if let (Some(ck), Some(id)) = (ck, node["nodeId"].as_u64()) {
        keys.insert(ck.to_owned(), id);
    }
    let here = ck.or(owner);
    if let Some(ck) = ck {
        for p in node["pseudoElements"].as_array().into_iter().flatten() {
            if let (Some(kind @ ("before" | "after")), Some(id)) =
                (p["pseudoType"].as_str(), p["nodeId"].as_u64())
            {
                pseudo.push((ck.to_owned(), kind.to_owned(), id));
            }
        }
    }
    for child in node["children"].as_array().into_iter().flatten() {
        keyed_nodes(child, here, keys, pseudo);
    }
}

/// Ответы браузера для паспорта: шрифты текста по ключу, псевдоэлементы, снимки видимых значков
/// по пиксельной сетке. Снимки идут последними, после всех фактов.
fn capture(cdp: &mut Cdp, facts: Value, dpr: f64) -> Result<cdp::Capture, String> {
    let list = cdp::elements(&facts)?;
    cdp.call("DOM.enable", json!({}))?;
    cdp.call("CSS.enable", json!({}))?;
    let doc = cdp.call("DOM.getDocument", json!({"depth": -1, "pierce": true}))?;
    let mut keys = BTreeMap::new();
    let mut pseudo_nodes = Vec::new();
    keyed_nodes(&doc["root"], None, &mut keys, &mut pseudo_nodes);
    if keys.len() != list.len() {
        return Err(format!(
            "узлов с data-ck {}, фактов {}",
            keys.len(),
            list.len()
        ));
    }
    let mut fonts = BTreeMap::new();
    for element in list.iter().filter(|e| e.font.is_some()) {
        let node = keys
            .get(&element.ck)
            .ok_or_else(|| format!("{}: узла нет в документе", element.ck))?;
        let reply = cdp.call("CSS.getPlatformFontsForNode", json!({"nodeId": node}))?;
        fonts.insert(element.ck.clone(), reply);
    }
    let mut pseudo = Vec::new();
    for (owner, kind, node) in pseudo_nodes {
        let style = cdp.call("CSS.getComputedStyleForNode", json!({"nodeId": node}))?;
        // Псевдоэлемент без блока (`display: none`) отвечает ошибкой: кадра у него нет.
        let box_model = cdp
            .call("DOM.getBoxModel", json!({"nodeId": node}))
            .unwrap_or(Value::Null);
        pseudo.push(cdp::PseudoReply {
            owner,
            pseudo: kind,
            style,
            box_model,
        });
    }
    let mut icons = BTreeMap::new();
    for icon in list
        .iter()
        .filter(|e| e.kind == ElementKind::Icon && e.visible)
    {
        let area = PixelClip::of(&icon.frame, dpr)?.in_points(dpr);
        let shot = cdp.call(
            "Page.captureScreenshot",
            json!({"format": "png", "captureBeyondViewport": false, "fromSurface": true,
                   "clip": {"x": area.x, "y": area.y, "width": area.w, "height": area.h,
                            "scale": 1}}),
        )?;
        let data = shot["data"].as_str().ok_or("снимок без данных")?;
        icons.insert(icon.ck.clone(), data.to_owned());
    }
    Ok(cdp::Capture {
        dpr,
        facts,
        fonts,
        pseudo,
        icons,
    })
}

/// `inventory --chrome <c> --root <р> --out <файл>`: инвентарь обычного мира прототипа
/// (`?conformance=1` без `load`) — id зарегистрированных сценариев и плагины с размещениями — со
/// штампом `prototype/`, канонической записью.
fn inventory(chrome: &Path, root: &Path, out: &Path) -> Result<(), String> {
    let index = fs::canonicalize(root.join("prototype/index.html")).map_err(|e| e.to_string())?;
    let (_browser, mut cdp) = open_page(chrome)?;
    cdp.call("Network.enable", json!({}))?;
    cdp.call(
        "Network.setBlockedURLs",
        json!({"urls": ["http://*", "https://*"]}),
    )?;
    cdp.call("Page.enable", json!({}))?;
    cdp.call(
        "Page.navigate",
        json!({"url": format!("file://{}?conformance=1", index.display())}),
    )?;
    cdp.wait_event("Page.loadEventFired")?;
    cdp.eval("window.conformance.settle()")?;
    let mut value = cdp.eval("window.conformance.inventory()")?;
    let stamp = Inputs::from_root(root)
        .map_err(|e| e.to_string())?
        .prototype;
    value["prototype"] = json!(stamp);
    let text = serde_json::to_string(&value).map_err(|e| e.to_string())?;
    Inventory::parse("инвентарь", &text).map_err(|e| e.to_string())?;
    let canonical = canonical_json(&value).map_err(|e| e.to_string())?;
    fs::write(out, canonical).map_err(|e| format!("{}: {e}", out.display()))
}

/// Замер страницы калибровки в текущей ячейке: цвета тонов, ширина `Range` по тексту каждого
/// случая и кадры ячеек листа по их местам на листе (пустая ячейка контроля рисованного
/// прямоугольника не имеет). Все вложенные начертания загружены до замера.
const CALIBRATION_FACTS: &str = r#"(async () => {
  await Promise.all([...document.fonts].map((face) => face.load()));
  await document.fonts.ready;
  await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
  const body = getComputedStyle(document.body);
  const tone = document.querySelector('#icons [data-tone="secondary"]');
  const text = {};
  for (const span of document.querySelectorAll('#text span[data-case]')) {
    const range = document.createRange();
    range.selectNodeContents(span);
    text[span.dataset.case] = range.getBoundingClientRect().width;
  }
  const icons = {};
  const sheet = document.querySelector('#icons > svg').getBoundingClientRect();
  for (const cell of document.querySelectorAll('#icons > svg > svg[id]')) {
    icons[cell.id] = { x: sheet.x + cell.x.baseVal.value, y: sheet.y + cell.y.baseVal.value,
                       w: cell.width.baseVal.value, h: cell.height.baseVal.value,
                       role: cell.dataset.role, size: Number(cell.dataset.size) };
  }
  return { background: body.backgroundColor, primary: body.color,
           secondary: getComputedStyle(tone).stroke, text, icons };
})()"#;

/// Узлы `span` случаев корпуса: имя случая → `nodeId`.
fn case_nodes(node: &Value, out: &mut BTreeMap<String, u64>) {
    let attrs: Vec<&str> = node["attributes"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let case = attrs
        .chunks(2)
        .find(|pair| pair.first() == Some(&"data-case"))
        .and_then(|pair| pair.get(1).copied());
    if let (Some(case), Some(id)) = (case, node["nodeId"].as_u64()) {
        out.insert(case.to_owned(), id);
    }
    for child in node["children"].as_array().into_iter().flatten() {
        case_nodes(child, out);
    }
}

fn tone(facts: &Value, name: &str) -> Result<Rgba, String> {
    facts[name]
        .as_str()
        .and_then(cdp::parse_color)
        .filter(|c| c.a == 1.0)
        .ok_or_else(|| format!("тон {name}: {} — не непрозрачный цвет", facts[name]))
}

/// `calibrate --chrome <c> --root <р> --out <каталог>`: сторона прототипа калибровки. Страница
/// `<каталог>/page.html` с листом `icons.svg` и корпусом `text.json` на `tokens.css` прототипа,
/// DPR 2, в каждой ячейке `controls.json`: ширины случаев, тона и маски ячеек листа
/// (`<каталог>/<ячейка>.icons/<id>.png`) тем же правилом переднего плана, что у паспортов.
/// Итог — `<каталог>/chrome.json` канонической записью.
fn calibrate(chrome: &Path, root: &Path, out: &Path) -> Result<(), String> {
    let dir = root.join("conformance/calibration");
    let read = |name: &str| {
        let path = dir.join(name);
        fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))
    };
    let corpus =
        calibration::Corpus::parse("text.json", &read("text.json")?).map_err(|e| e.to_string())?;
    let controls = read("controls.json")?;
    let cells = calibration::cells("controls.json", &controls).map_err(|e| e.to_string())?;
    let controls: Value = serde_json::from_str(&controls).map_err(|e| e.to_string())?;
    let dpr = controls["dpr"].as_f64().ok_or("controls.json: нет dpr")?;
    if controls["icon_fg"].as_u64() != Some(u64::from(ICON_FG)) {
        return Err("controls.json: icon_fg расходится с ICON_FG".to_owned());
    }
    let tokens =
        fs::canonicalize(root.join("prototype/css/tokens.css")).map_err(|e| e.to_string())?;
    fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let out = fs::canonicalize(out).map_err(|e| e.to_string())?;
    let page = out.join("page.html");
    let html = calibration::page(
        &format!("file://{}", tokens.display()),
        &read("icons.svg")?,
        &corpus,
    );
    fs::write(&page, html).map_err(|e| format!("{}: {e}", page.display()))?;

    let (_browser, mut cdp) = open_page(chrome)?;
    cdp.call("Network.enable", json!({}))?;
    cdp.call(
        "Network.setBlockedURLs",
        json!({"urls": ["http://*", "https://*"]}),
    )?;
    cdp.call(
        "Emulation.setDeviceMetricsOverride",
        json!({"width": 1180, "height": 720, "deviceScaleFactor": dpr, "mobile": false}),
    )?;
    cdp.call("Page.enable", json!({}))?;
    cdp.call(
        "Page.navigate",
        json!({"url": format!("file://{}", page.display())}),
    )?;
    cdp.wait_event("Page.loadEventFired")?;

    let mut record = json!({
        "schema": 1,
        "dpr": dpr,
        "prototype": Inputs::from_root(root).map_err(|e| e.to_string())?.prototype,
        "cells": {},
    });
    for cell in &cells {
        let theme = match cell.theme {
            Theme::Light => "light",
            Theme::Dark => "dark",
        };
        let contrast = match cell.contrast {
            Contrast::Normal => "delete d.contrast",
            Contrast::High => "d.contrast = 'more'",
        };
        cdp.eval(&format!(
            "(() => {{ const d = document.documentElement.dataset; d.theme = '{theme}'; {contrast}; }})()"
        ))?;
        let facts = cdp.eval(CALIBRATION_FACTS)?;
        let background = tone(&facts, "background")?;
        let (primary, secondary) = (tone(&facts, "primary")?, tone(&facts, "secondary")?);
        let widths = facts["text"].as_object().ok_or("замер без ширин")?;
        if let Some((style, line)) = corpus
            .cases()
            .find(|(s, l)| !widths.contains_key(&calibration::case_id(s, l)))
        {
            return Err(format!(
                "{cell}: случай {} не измерен",
                calibration::case_id(style, line)
            ));
        }
        let masks = out.join(format!("{cell}.icons"));
        if masks.exists() {
            fs::remove_dir_all(&masks).map_err(|e| format!("{}: {e}", masks.display()))?;
        }
        fs::create_dir_all(&masks).map_err(|e| format!("{}: {e}", masks.display()))?;
        let mut icons = serde_json::Map::new();
        for (id, icon) in facts["icons"].as_object().ok_or("замер без листа")? {
            let side = |key: &str| icon[key].as_f64().ok_or_else(|| format!("{id}: нет {key}"));
            let frame = Rect {
                x: side("x")?,
                y: side("y")?,
                w: side("w")?,
                h: side("h")?,
            };
            let clip = PixelClip::of(&frame, dpr)?;
            let area = clip.in_points(dpr);
            let shot = cdp.call(
                "Page.captureScreenshot",
                json!({"format": "png", "captureBeyondViewport": false, "fromSurface": true,
                       "clip": {"x": area.x, "y": area.y, "width": area.w, "height": area.h,
                                "scale": 1}}),
            )?;
            let png = calibration::shot(shot["data"].as_str().ok_or("снимок без данных")?)?;
            let mask = Mask::from_png(&png, background)?;
            if (mask.width, mask.height) != (clip.width, clip.height) {
                return Err(format!(
                    "{cell}: {id}: снимок {}×{}, кадр {}×{}",
                    mask.width, mask.height, clip.width, clip.height
                ));
            }
            let path = masks.join(format!("{id}.png"));
            fs::write(&path, mask.to_png()?).map_err(|e| format!("{}: {e}", path.display()))?;
            icons.insert(
                id.clone(),
                json!({"frame": frame, "role": icon["role"], "size": icon["size"],
                       "pixels": mask.on.iter().filter(|on| **on).count()}),
            );
        }
        record["cells"][cell.name()] = json!({
            "tones": {"background": background, "primary": primary, "secondary": secondary},
            "two_tone": calibration::two_tone(background, primary, secondary, ICON_FG),
            "text": facts["text"],
            "icons": icons,
        });
    }

    cdp.call("DOM.enable", json!({}))?;
    cdp.call("CSS.enable", json!({}))?;
    let doc = cdp.call("DOM.getDocument", json!({"depth": -1, "pierce": true}))?;
    let mut nodes = BTreeMap::new();
    case_nodes(&doc["root"], &mut nodes);
    let mut fonts = serde_json::Map::new();
    for (style, line) in corpus.cases() {
        let case = calibration::case_id(style, line);
        let node = nodes
            .get(&case)
            .ok_or_else(|| format!("{case}: узла нет в документе"))?;
        let reply = cdp.call("CSS.getPlatformFontsForNode", json!({"nodeId": node}))?;
        let faces = cdp::font_faces(&reply)?;
        if style.family == calibration::Family::Mono {
            calibration::mono_faces(&case, &faces)?;
        }
        fonts.insert(case, json!(faces));
    }
    record["fonts"] = Value::Object(fonts);
    let text = canonical_json(&record).map_err(|e| e.to_string())?;
    let path = out.join("chrome.json");
    fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn read_registry(root: &Path) -> Result<Registry, String> {
    let path = root.join("conformance/registry.toml");
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Registry::parse(&path.display().to_string(), &text).map_err(|e| e.to_string())
}

/// `states --root <р> [имя…]`: строки «состояние рецепт ячейка» выбранных состояний реестра.
fn states(root: &Path, names: &[String]) -> Result<ExitCode, String> {
    let registry = read_registry(root)?;
    if let Some(unknown) = names.iter().find(|n| registry.state(n).is_none()) {
        eprintln!("shellio-snap: состояния «{unknown}» нет в реестре");
        return Ok(ExitCode::from(2));
    }
    for state in &registry.states {
        if names.is_empty() || names.contains(&state.id) {
            for cell in state.cells() {
                let surface = serde_json::to_value(state.surface)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default();
                println!("{} {} {cell} {surface}", state.id, state.recipe);
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `check --root <р>`: годность всех паспортов реестра; негодные перечисляются, код 1.
fn check(root: &Path) -> Result<ExitCode, String> {
    let registry = read_registry(root)?;
    let inputs = Inputs::from_root(root).map_err(|e| e.to_string())?;
    let findings = check_registry(&inputs, &registry).map_err(|e| e.to_string())?;
    for finding in &findings {
        eprintln!("{finding}");
    }
    Ok(if findings.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Паспорт и маски значков рядом с ним: `<имя>.json` и `<имя>.icons/<ключ>.png`.
fn write(out: &Path, text: &str, masks: &BTreeMap<String, Vec<u8>>) -> Result<(), String> {
    let dir = out.with_extension("icons");
    if dir.exists() {
        fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    if !masks.is_empty() {
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    for (ck, png) in masks {
        let path = dir.join(format!("{ck}.png"));
        fs::write(&path, png).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    fs::write(out, text).map_err(|e| format!("{}: {e}", out.display()))
}

fn main() -> ExitCode {
    let all: Vec<String> = std::env::args().skip(1).collect();
    if all.first().map(String::as_str) == Some("world") {
        return match all.get(1..).unwrap_or_default() {
            [c, chrome, i, index, o, out] if c == "--chrome" && i == "--index" && o == "--out" => {
                match world(Path::new(chrome), Path::new(index), Path::new(out)) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("shellio-snap: {e}");
                        ExitCode::from(1)
                    }
                }
            }
            _ => {
                eprintln!("shellio-snap: world --chrome <c> --index <index.html> --out <каталог>");
                ExitCode::from(2)
            }
        };
    }
    if all.first().map(String::as_str) == Some("calibrate") {
        return match all.get(1..).unwrap_or_default() {
            [c, chrome, r, root, o, out] if c == "--chrome" && r == "--root" && o == "--out" => {
                match calibrate(Path::new(chrome), Path::new(root), Path::new(out)) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("shellio-snap: {e}");
                        ExitCode::from(1)
                    }
                }
            }
            _ => {
                eprintln!(
                    "shellio-snap: calibrate --chrome <c> --root <репозиторий> --out <каталог>"
                );
                ExitCode::from(2)
            }
        };
    }
    if all.first().map(String::as_str) == Some("inventory") {
        return match all.get(1..).unwrap_or_default() {
            [c, chrome, r, root, o, out] if c == "--chrome" && r == "--root" && o == "--out" => {
                match inventory(Path::new(chrome), Path::new(root), Path::new(out)) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("shellio-snap: {e}");
                        ExitCode::from(1)
                    }
                }
            }
            _ => {
                eprintln!("shellio-snap: inventory --chrome <c> --root <репозиторий> --out <файл>");
                ExitCode::from(2)
            }
        };
    }
    if let Some(sub) = all.first().filter(|s| *s == "states" || *s == "check") {
        let root = match (all.get(1).map(String::as_str), all.get(2)) {
            (Some("--root"), Some(root)) => PathBuf::from(root),
            _ => {
                eprintln!("shellio-snap: {sub} --root <репозиторий>");
                return ExitCode::from(2);
            }
        };
        let run = if sub == "states" {
            states(&root, &all[3..])
        } else {
            check(&root)
        };
        return run.unwrap_or_else(|e| {
            eprintln!("shellio-snap: {e}");
            ExitCode::from(1)
        });
    }
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("shellio-snap: {e}");
            return ExitCode::from(2);
        }
    };
    match snap(&args).and_then(|(text, masks)| write(&args.out, &text, &masks)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("shellio-snap: {e}");
            ExitCode::from(1)
        }
    }
}
