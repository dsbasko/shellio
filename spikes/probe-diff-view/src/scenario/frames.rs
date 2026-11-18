//! Разбор выгрузки `xctrace export` шаблона Animation Hitches: таблицы `display-vsyncs-interval`,
//! `displayed-surfaces-interval` и `time-info`. XML выгрузки разбирается без зависимостей: строки
//! `<row>` с ячейками по порядку колонок схемы, повтор значения — ссылка `ref` на `id` первого.
//!
//! Время в таблицах — наносекунды от начала трассы. `time-info` даёт `mach_absolute_time` начала
//! (`mabs-epoch`) и `timebase`, отсюда время `CLOCK_UPTIME_RAW` процесса пробы:
//! `mabs × numer / denom + t`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Значение ячейки: текст элемента и его атрибут `fmt`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub fmt: String,
}

/// Таблица выгрузки: имя схемы, мнемоники колонок и строки. Пустая ячейка (`<sentinel/>`) — `None`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub schema: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<Cell>>>,
}

impl Table {
    pub fn column(&self, mnemonic: &str) -> Result<usize, String> {
        self.columns
            .iter()
            .position(|column| column == mnemonic)
            .ok_or_else(|| format!("{}: нет колонки «{mnemonic}»", self.schema))
    }
}

#[derive(Debug)]
enum Token<'a> {
    Open {
        name: &'a str,
        attrs: Vec<(&'a str, String)>,
        empty: bool,
    },
    Close(&'a str),
    Text(&'a str),
}

fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

fn attrs(mut body: &str) -> Result<Vec<(&str, String)>, String> {
    let mut out = Vec::new();
    loop {
        body = body.trim_start();
        if body.is_empty() {
            return Ok(out);
        }
        let eq = body
            .find('=')
            .ok_or_else(|| format!("атрибут без значения: «{body}»"))?;
        let name = body[..eq].trim();
        let rest = body[eq + 1..].trim_start();
        let quote = rest
            .chars()
            .next()
            .filter(|c| *c == '"' || *c == '\'')
            .ok_or_else(|| format!("значение атрибута {name} без кавычек"))?;
        let end = rest[1..]
            .find(quote)
            .ok_or_else(|| format!("незакрытое значение атрибута {name}"))?;
        out.push((name, unescape(&rest[1..1 + end])));
        body = &rest[end + 2..];
    }
}

fn tokens(xml: &str) -> Result<Vec<Token<'_>>, String> {
    let mut out = Vec::new();
    let mut rest = xml;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            out.push(Token::Text(rest));
            break;
        };
        if lt > 0 {
            out.push(Token::Text(&rest[..lt]));
        }
        rest = &rest[lt..];
        if rest.starts_with("<?") || rest.starts_with("<!") {
            let end = rest.find('>').ok_or("незакрытое объявление")?;
            rest = &rest[end + 1..];
            continue;
        }
        let end = rest.find('>').ok_or("незакрытый тег")?;
        let tag = &rest[1..end];
        rest = &rest[end + 1..];
        if let Some(name) = tag.strip_prefix('/') {
            out.push(Token::Close(name.trim()));
            continue;
        }
        let (tag, empty) = match tag.strip_suffix('/') {
            Some(tag) => (tag, true),
            None => (tag, false),
        };
        let split = tag.find(char::is_whitespace).unwrap_or(tag.len());
        out.push(Token::Open {
            name: &tag[..split],
            attrs: attrs(&tag[split..])?,
            empty,
        });
    }
    Ok(out)
}

fn attr<'a>(attrs: &'a [(&str, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.as_str())
}

/// Разбирает первую таблицу выгрузки `xctrace export --xpath …/table[@schema=…]`.
pub fn parse_table(xml: &str) -> Result<Table, String> {
    let tokens = tokens(xml)?;
    let mut table = Table::default();
    let mut ids: HashMap<String, Cell> = HashMap::new();
    let mut i = 0;
    let mut in_schema = false;
    let mut in_mnemonic = false;
    while i < tokens.len() {
        match &tokens[i] {
            Token::Open { name, attrs, .. } if *name == "schema" => {
                table.schema = attr(attrs, "name").unwrap_or_default().to_string();
                in_schema = true;
            }
            Token::Close("schema") => in_schema = false,
            Token::Open { name, .. } if in_schema && *name == "mnemonic" => in_mnemonic = true,
            Token::Close("mnemonic") => in_mnemonic = false,
            Token::Text(text) if in_mnemonic => table.columns.push(text.trim().to_string()),
            Token::Open { name, empty, .. } if *name == "row" && !empty => {
                let (row, next) = parse_row(&tokens, i + 1, &mut ids)?;
                table.rows.push(row);
                i = next;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    if table.schema.is_empty() {
        return Err("в выгрузке нет схемы таблицы".into());
    }
    Ok(table)
}

/// Ячейки одной строки до `</row>`. Возвращает строку и индекс токена после неё.
fn parse_row(
    tokens: &[Token<'_>],
    mut i: usize,
    ids: &mut HashMap<String, Cell>,
) -> Result<(Vec<Option<Cell>>, usize), String> {
    let mut row = Vec::new();
    while i < tokens.len() {
        match &tokens[i] {
            Token::Close("row") => return Ok((row, i + 1)),
            Token::Open { name, attrs, empty } => {
                let (cell, next) = parse_cell(tokens, i, name, attrs, *empty, ids)?;
                row.push(cell);
                i = next;
            }
            Token::Text(text) if text.trim().is_empty() => i += 1,
            other => return Err(format!("неожиданный токен в строке: {other:?}")),
        }
    }
    Err("незакрытая строка <row>".into())
}

/// Один элемент с вложенными элементами. Текст ячейки — собственный текст элемента верхнего
/// уровня, вложенные `id` тоже запоминаются.
fn parse_cell(
    tokens: &[Token<'_>],
    start: usize,
    name: &str,
    attrs: &[(&str, String)],
    empty: bool,
    ids: &mut HashMap<String, Cell>,
) -> Result<(Option<Cell>, usize), String> {
    if name == "sentinel" {
        return Ok((None, start + 1));
    }
    if let Some(reference) = attr(attrs, "ref") {
        let cell = ids
            .get(reference)
            .cloned()
            .ok_or_else(|| format!("ссылка ref={reference} раньше id"))?;
        let next = if empty {
            start + 1
        } else {
            skip(tokens, start)?
        };
        return Ok((Some(cell), next));
    }
    let mut cell = Cell {
        text: String::new(),
        fmt: attr(attrs, "fmt").unwrap_or_default().to_string(),
    };
    let mut i = start + 1;
    if !empty {
        loop {
            match tokens.get(i) {
                Some(Token::Close(close)) if *close == name => {
                    i += 1;
                    break;
                }
                Some(Token::Text(text)) => {
                    cell.text.push_str(&unescape(text));
                    i += 1;
                }
                Some(Token::Open {
                    name: inner,
                    attrs: inner_attrs,
                    empty: inner_empty,
                }) => {
                    let (_, next) = parse_cell(tokens, i, inner, inner_attrs, *inner_empty, ids)?;
                    i = next;
                }
                Some(Token::Close(other)) => {
                    return Err(format!("закрыт {other} вместо {name}"));
                }
                None => return Err(format!("незакрытый элемент {name}")),
            }
        }
    }
    if let Some(id) = attr(attrs, "id") {
        ids.insert(id.to_string(), cell.clone());
    }
    Ok((Some(cell), i))
}

fn skip(tokens: &[Token<'_>], start: usize) -> Result<usize, String> {
    let mut depth = 0usize;
    for (offset, token) in tokens[start..].iter().enumerate() {
        match token {
            Token::Open { empty: false, .. } => depth += 1,
            Token::Close(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok(start + offset + 1);
                }
            }
            _ => {}
        }
    }
    Err("незакрытый элемент со ссылкой".into())
}

fn number(cell: Option<&Option<Cell>>, what: &str) -> Result<u64, String> {
    let cell = cell
        .and_then(Option::as_ref)
        .ok_or_else(|| format!("нет значения «{what}»"))?;
    cell.text
        .trim()
        .parse()
        .map_err(|_| format!("«{what}» не число: «{}»", cell.text))
}

fn text(cell: Option<&Option<Cell>>) -> String {
    cell.and_then(Option::as_ref)
        .map(|cell| cell.text.trim().to_string())
        .unwrap_or_default()
}

/// Перевод времени трассы во время `CLOCK_UPTIME_RAW`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Clock {
    pub mabs_epoch: u64,
    pub numer: u32,
    pub denom: u32,
}

impl Clock {
    pub fn from_table(table: &Table) -> Result<Clock, String> {
        let row = table.rows.first().ok_or("time-info без строк")?;
        let epoch = table.column("mabs-epoch")?;
        let timebase = table.column("timebase-info")?;
        let fmt = row
            .get(timebase)
            .and_then(Option::as_ref)
            .map(|cell| cell.fmt.clone())
            .ok_or("time-info без timebase")?;
        let (numer, denom) = fmt
            .split_once('/')
            .and_then(|(n, d)| Some((n.trim().parse().ok()?, d.trim().parse().ok()?)))
            .filter(|&(_, d): &(u32, u32)| d != 0)
            .ok_or_else(|| format!("timebase «{fmt}» не дробь"))?;
        Ok(Clock {
            mabs_epoch: number(row.get(epoch), "mabs-epoch")?,
            numer,
            denom,
        })
    }

    /// Время трассы, наносекунды от начала, во время `CLOCK_UPTIME_RAW`.
    pub fn uptime_ns(&self, trace_ns: u64) -> u64 {
        let epoch = u128::from(self.mabs_epoch) * u128::from(self.numer) / u128::from(self.denom);
        epoch as u64 + trace_ns
    }
}

/// Такт vsync дисплея.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Vsync {
    pub at_ns: u64,
    pub display: String,
}

pub fn vsyncs(table: &Table) -> Result<Vec<Vsync>, String> {
    let at = table.column("timestamp")?;
    let display = table.column("display-name")?;
    table
        .rows
        .iter()
        .map(|row| {
            Ok(Vsync {
                at_ns: number(row.get(at), "timestamp")?,
                display: text(row.get(display)),
            })
        })
        .collect()
}

/// Показ поверхности на дисплее.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Surface {
    pub swap_ns: u64,
    pub duration_ns: u64,
    pub display: String,
    pub connection: String,
    pub surface_id: u64,
    pub label: String,
    pub resolution: String,
}

/// Источник кадра из подписи показа: `Surface 21 (<процесс> (<pid>):Frame <n>:Surface <m>)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub process: String,
    pub pid: u32,
    pub frame: u64,
    pub surface: u64,
}

impl Surface {
    /// Процесс, кадр и поверхность источника, если подпись их называет. Подпись без источника —
    /// поверхность самого дисплея.
    pub fn source(&self) -> Option<Source> {
        let open = self.label.find(" (")?;
        let inner = self.label[open + 2..].strip_suffix(')')?;
        let (process, rest) = inner.rsplit_once("):Frame ")?;
        let (name, pid) = process.rsplit_once(" (")?;
        let (frame, surface) = rest.split_once(":Surface ")?;
        Some(Source {
            process: name.to_string(),
            pid: pid.parse().ok()?,
            frame: frame.parse().ok()?,
            surface: surface.parse().ok()?,
        })
    }
}

pub fn surfaces(table: &Table) -> Result<Vec<Surface>, String> {
    let swap = table.column("start")?;
    let duration = table.column("duration")?;
    let display = table.column("display-name")?;
    let connection = table.column("connection-UUID")?;
    let surface = table.column("surface-id")?;
    let label = table.column("event-label")?;
    let resolution = table.column("resolution")?;
    table
        .rows
        .iter()
        .map(|row| {
            Ok(Surface {
                swap_ns: number(row.get(swap), "start")?,
                duration_ns: number(row.get(duration), "duration")?,
                display: text(row.get(display)),
                connection: text(row.get(connection)),
                surface_id: number(row.get(surface), "surface-id")?,
                label: row
                    .get(label)
                    .and_then(Option::as_ref)
                    .map(|cell| cell.fmt.clone())
                    .unwrap_or_default(),
                resolution: text(row.get(resolution)),
            })
        })
        .collect()
}

/// Процесс из подписи `<имя> (<pid>)` ячейки `process`.
fn process(cell: Option<&Option<Cell>>) -> Option<(String, u32)> {
    let fmt = &cell.and_then(Option::as_ref)?.fmt;
    let (name, pid) = fmt.strip_suffix(')')?.rsplit_once(" (")?;
    Some((name.to_string(), pid.parse().ok()?))
}

/// Обновление процесса в кадре дисплея `swap_id` (`hitches-updates`): у каждой строки есть
/// процесс-источник, ряд сплошной (пробная запись 02:01).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    pub start_ns: u64,
    pub duration_ns: u64,
    pub process: String,
    pub pid: u32,
    pub swap_id: u64,
    pub surface_id: u64,
}

pub fn updates(table: &Table) -> Result<Vec<Update>, String> {
    let start = table.column("start")?;
    let duration = table.column("duration")?;
    let source = table.column("process")?;
    let swap = table.column("swap-id")?;
    let surface = table.column("surface-id")?;
    table
        .rows
        .iter()
        .map(|row| {
            let (process, pid) = process(row.get(source))
                .ok_or_else(|| format!("обновление без процесса: {:?}", row.get(source)))?;
            Ok(Update {
                start_ns: number(row.get(start), "start")?,
                duration_ns: number(row.get(duration), "duration")?,
                process,
                pid,
                swap_id: number(row.get(swap), "swap-id")?,
                surface_id: number(row.get(surface), "surface-id")?,
            })
        })
        .collect()
}

/// Жизнь кадра дисплея `swap_id` (`hitches-frame-lifetimes`): конец — показ, он совпадает со
/// временем `display-surface-swap` того же кадра.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lifetime {
    pub start_ns: u64,
    pub duration_ns: u64,
    pub display: String,
    pub swap_id: u64,
    pub surface_id: u64,
}

impl Lifetime {
    pub fn shown_ns(&self) -> u64 {
        self.start_ns + self.duration_ns
    }
}

pub fn lifetimes(table: &Table) -> Result<Vec<Lifetime>, String> {
    let start = table.column("start")?;
    let duration = table.column("duration")?;
    let display = table.column("display")?;
    let swap = table.column("swap-id")?;
    let surface = table.column("surface-id")?;
    table
        .rows
        .iter()
        .map(|row| {
            Ok(Lifetime {
                start_ns: number(row.get(start), "start")?,
                duration_ns: number(row.get(duration), "duration")?,
                display: text(row.get(display)),
                swap_id: number(row.get(swap), "swap-id")?,
                surface_id: number(row.get(surface), "surface-id")?,
            })
        })
        .collect()
}

/// Время показа кадров дисплея, в которых есть обновление процесса `pid`, по возрастанию. Кадр
/// с обновлением без жизни в выгрузке — ошибка: показ такого кадра неизвестен.
pub fn shown_frames(
    updates: &[Update],
    lifetimes: &[Lifetime],
    pid: u32,
) -> Result<Vec<u64>, String> {
    let shown: HashMap<u64, u64> = lifetimes
        .iter()
        .map(|lifetime| (lifetime.swap_id, lifetime.shown_ns()))
        .collect();
    let mut swaps: Vec<u64> = updates
        .iter()
        .filter(|update| update.pid == pid)
        .map(|update| update.swap_id)
        .collect();
    swaps.sort_unstable();
    swaps.dedup();
    let mut frames = swaps
        .into_iter()
        .map(|swap| {
            shown
                .get(&swap)
                .copied()
                .ok_or_else(|| format!("кадр дисплея {swap} без hitches-frame-lifetimes"))
        })
        .collect::<Result<Vec<u64>, String>>()?;
    frames.sort_unstable();
    Ok(frames)
}

/// Подтверждение частоты по периоду vsync одного дисплея.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rate {
    pub display: String,
    pub ticks: usize,
    pub median_period_ns: u64,
    pub expected_period_ns: u64,
    pub confirmed: bool,
}

/// Допуск периода vsync против ожидаемого, доли.
pub const PERIOD_TOLERANCE: f64 = 0.01;

/// Медиана интервалов vsync дисплея против ожидаемого периода с допуском [`PERIOD_TOLERANCE`].
pub fn confirm_rate(vsyncs: &[Vsync], display: &str, expected_period_ns: u64) -> Rate {
    let mut times: Vec<u64> = vsyncs
        .iter()
        .filter(|vsync| vsync.display == display)
        .map(|vsync| vsync.at_ns)
        .collect();
    times.sort_unstable();
    let mut periods: Vec<u64> = times.windows(2).map(|pair| pair[1] - pair[0]).collect();
    periods.sort_unstable();
    let median = periods.get(periods.len() / 2).copied().unwrap_or(0);
    let error = (median as f64 - expected_period_ns as f64).abs() / expected_period_ns as f64;
    Rate {
        display: display.to_string(),
        ticks: times.len(),
        median_period_ns: median,
        expected_period_ns,
        confirmed: periods.len() >= 2 && error <= PERIOD_TOLERANCE,
    }
}

/// Строка `xctrace record`, после которой запись идёт.
pub const RECORDING_STARTED: &str = "Ctrl-C to stop the recording";

/// Таблицы выгрузки, нужные калибровке и кадрам.
pub const TABLES: [&str; 5] = [
    "display-vsyncs-interval",
    "displayed-surfaces-interval",
    "hitches-updates",
    "hitches-frame-lifetimes",
    "time-info",
];

/// Идущая запись `xctrace record`. Останавливается `SIGINT`, как Ctrl-C, и ждёт сохранения
/// трассы: у Xcode 27 сохранение записи на 2 с длится около 30 с.
pub struct Recorder {
    child: std::process::Child,
    output: std::path::PathBuf,
    log: std::thread::JoinHandle<Vec<String>>,
}

/// Команда записи шаблона Animation Hitches с присоединением к процессу `pid`.
pub fn record_command(pid: u32, output: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new("xcrun");
    command
        .args([
            "xctrace",
            "record",
            "--template",
            "Animation Hitches",
            "--attach",
        ])
        .arg(pid.to_string())
        .arg("--output")
        .arg(output)
        .arg("--no-prompt");
    command
}

impl Recorder {
    /// Запускает команду записи и блокирует вызывающий поток до строки [`RECORDING_STARTED`].
    /// Вызывается не с главного потока.
    pub fn start(
        mut command: std::process::Command,
        output: std::path::PathBuf,
    ) -> Result<Recorder, String> {
        use std::io::BufRead;
        let mut child = command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .map_err(|error| format!("xctrace record: {error}"))?;
        let stdout = child.stdout.take().ok_or("xctrace record без stdout")?;
        let (started, wait) = std::sync::mpsc::channel();
        let log = std::thread::spawn(move || {
            let mut lines = Vec::new();
            let mut started = Some(started);
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if line.contains(RECORDING_STARTED)
                    && let Some(started) = started.take()
                {
                    let _ = started.send(());
                }
                lines.push(line);
            }
            lines
        });
        if wait.recv().is_err() {
            let _ = child.wait();
            let lines = log.join().unwrap_or_default();
            return Err(format!(
                "xctrace record не начал запись: {}",
                lines.join(" | ")
            ));
        }
        Ok(Recorder { child, output, log })
    }

    /// Останавливает запись и ждёт сохранения трассы. Возвращает журнал `xctrace`.
    pub fn stop(mut self) -> Result<Vec<String>, String> {
        // SAFETY: `kill` посылает сигнал дочернему процессу по его pid, память не затрагивается.
        if unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGINT) } != 0 {
            return Err(format!(
                "SIGINT xctrace: {}",
                std::io::Error::last_os_error()
            ));
        }
        drop(self.child.stdin.take());
        let status = self
            .child
            .wait()
            .map_err(|error| format!("ожидание xctrace: {error}"))?;
        let lines = self.log.join().map_err(|_| "поток журнала xctrace упал")?;
        if !status.success() {
            return Err(format!("xctrace record: {status}, {}", lines.join(" | ")));
        }
        if !self.output.exists() {
            return Err(format!("трасса {} не записана", self.output.display()));
        }
        Ok(lines)
    }
}

/// Выгружает таблицу схемы `schema` первого прогона трассы.
pub fn export_table(trace: &std::path::Path, schema: &str) -> Result<String, String> {
    let output = std::process::Command::new("xcrun")
        .args(["xctrace", "export", "--input"])
        .arg(trace)
        .arg("--xpath")
        .arg(format!(
            "/trace-toc/run[@number=\"1\"]/data/table[@schema=\"{schema}\"]"
        ))
        .output()
        .map_err(|error| format!("xctrace export: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "xctrace export {schema}: {}, {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| format!("выгрузка {schema} не UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VSYNCS: &str = r#"<?xml version="1.0"?>
<trace-query-result>
<node xpath='//trace-toc[1]/run[1]/data[1]/table[56]'><schema name="display-vsyncs-interval"><col><mnemonic>timestamp</mnemonic></col><col><mnemonic>duration</mnemonic></col><col><mnemonic>display-name</mnemonic></col><col><mnemonic>event-label</mnemonic></col></schema><row><start-time id="1" fmt="00:00.000.100">100</start-time><duration id="2" fmt="1 ns">1</duration><display-name id="3" fmt="Built-In Display 1">Built-In Display 1</display-name><narrative id="5" fmt="VSync Request"><narrative-text id="6" fmt="VSync Request ">VSync Request </narrative-text><start-time ref="1"/></narrative></row>
<row><start-time id="8" fmt="x">8333433</start-time><duration ref="2"/><display-name ref="3"/><narrative id="9" fmt="VSync Request"><narrative-text ref="6"/><start-time ref="8"/></narrative></row>
<row><start-time id="10" fmt="x">16666767</start-time><duration ref="2"/><display-name id="11" fmt="Ext &amp; 1">Ext &amp; 1</display-name><sentinel/></row>
<row><start-time id="12" fmt="x">16666766</start-time><duration ref="2"/><display-name ref="3"/><sentinel/></row>
</node></trace-query-result>"#;

    #[test]
    fn parses_rows_with_references() {
        let table = parse_table(VSYNCS).unwrap();
        assert_eq!(table.schema, "display-vsyncs-interval");
        assert_eq!(table.columns.len(), 4);
        assert_eq!(table.rows.len(), 4);
        assert_eq!(table.rows[3][3], None);
        let vsyncs = vsyncs(&table).unwrap();
        assert_eq!(vsyncs[1].at_ns, 8_333_433);
        assert_eq!(vsyncs[1].display, "Built-In Display 1");
        assert_eq!(vsyncs[2].display, "Ext & 1");
        let rate = confirm_rate(&vsyncs, "Built-In Display 1", 8_333_333);
        assert_eq!(rate.ticks, 3);
        assert_eq!(rate.median_period_ns, 8_333_333);
        assert!(rate.confirmed);
        assert!(!confirm_rate(&vsyncs, "Built-In Display 1", 16_666_667).confirmed);
        assert!(!confirm_rate(&vsyncs, "Ext & 1", 16_666_667).confirmed);
    }

    const UPDATES: &str = r#"<schema name="hitches-updates"><col><mnemonic>start</mnemonic></col><col><mnemonic>duration</mnemonic></col><col><mnemonic>process</mnemonic></col><col><mnemonic>display</mnemonic></col><col><mnemonic>swap-id</mnemonic></col><col><mnemonic>surface-id</mnemonic></col><col><mnemonic>label</mnemonic></col></schema>
<row><start-time id="1" fmt="x">453018208</start-time><duration id="2" fmt="x">579541</duration><process id="3" fmt="probe (diff) (77099)"><pid id="4" fmt="77099">77099</pid><device-session id="5" fmt="TODO">TODO</device-session></process><display-name id="6" fmt="Display 1">Display 1</display-name><uint32 id="7" fmt="18121491">18121491</uint32><uint32 id="8" fmt="119">119</uint32><string id="11" fmt="0x1148313">0x1148313</string></row>
<row><start-time id="12" fmt="x">461348291</start-time><duration id="13" fmt="x">458833</duration><process ref="3"/><display-name ref="6"/><uint32 id="14" fmt="18121492">18121492</uint32><uint32 id="15" fmt="21">21</uint32><string id="17" fmt="0x1148314">0x1148314</string></row>
<row><start-time id="18" fmt="x">469000000</start-time><duration ref="13"/><process id="19" fmt="app1 (500)"><pid id="20" fmt="500">500</pid></process><display-name ref="6"/><uint32 ref="14"/><uint32 ref="15"/><string ref="17"/></row>"#;

    const LIFETIMES: &str = r#"<schema name="hitches-frame-lifetimes"><col><mnemonic>start</mnemonic></col><col><mnemonic>duration</mnemonic></col><col><mnemonic>display</mnemonic></col><col><mnemonic>swap-id</mnemonic></col><col><mnemonic>surface-id</mnemonic></col><col><mnemonic>label</mnemonic></col></schema>
<row><start-time id="1" fmt="x">436698958</start-time><duration id="2" fmt="33.33 ms">33332750</duration><display-name id="3" fmt="Display 1">Display 1</display-name><uint32 id="15" fmt="18121491">18121491</uint32><uint32 id="16" fmt="119">119</uint32><string id="18" fmt="0x1148313">0x1148313</string></row>
<row><start-time id="4" fmt="x">445032291</start-time><duration ref="2"/><display-name ref="3"/><uint32 id="5" fmt="18121492">18121492</uint32><uint32 id="6" fmt="21">21</uint32><string id="7" fmt="0x1148314">0x1148314</string></row>"#;

    #[test]
    fn updates_name_the_process_and_lifetimes_the_display_time() {
        let updates = updates(&parse_table(UPDATES).unwrap()).unwrap();
        assert_eq!(updates.len(), 3);
        assert_eq!(
            updates[1],
            Update {
                start_ns: 461_348_291,
                duration_ns: 458_833,
                process: "probe (diff)".into(),
                pid: 77_099,
                swap_id: 18_121_492,
                surface_id: 21,
            }
        );
        assert_eq!((updates[2].process.as_str(), updates[2].pid), ("app1", 500));
        let lifetimes = lifetimes(&parse_table(LIFETIMES).unwrap()).unwrap();
        assert_eq!(lifetimes[1].swap_id, 18_121_492);
        assert_eq!(lifetimes[1].display, "Display 1");
        assert_eq!(lifetimes[0].shown_ns(), 470_031_708);
        assert_eq!(
            shown_frames(&updates, &lifetimes, 77_099).unwrap(),
            vec![470_031_708, 478_365_041]
        );
        assert_eq!(
            shown_frames(&updates, &lifetimes, 500).unwrap(),
            vec![478_365_041]
        );
        assert!(shown_frames(&updates, &lifetimes[..1], 77_099).is_err());
        assert!(shown_frames(&updates, &lifetimes, 1).unwrap().is_empty());
        let orphan = UPDATES.replace(r#"<process ref="3"/>"#, "<sentinel/>");
        assert!(super::updates(&parse_table(&orphan).unwrap()).is_err());
    }

    #[test]
    fn label_names_the_source() {
        let mut surface = Surface {
            swap_ns: 0,
            duration_ns: 1,
            display: String::new(),
            connection: String::new(),
            surface_id: 21,
            label: "Surface 21 (probe (diff) (4242):Frame 7:Surface 139)".into(),
            resolution: String::new(),
        };
        assert_eq!(
            surface.source(),
            Some(Source {
                process: "probe (diff)".into(),
                pid: 4242,
                frame: 7,
                surface: 139
            })
        );
        surface.label = "Surface 21".into();
        assert_eq!(surface.source(), None);
    }

    #[test]
    fn broken_reference_is_an_error() {
        let xml = r#"<schema name="t"><col><mnemonic>a</mnemonic></col></schema><row><uint64 ref="9"/></row>"#;
        assert!(parse_table(xml).is_err());
        assert!(parse_table("<row><a>1</a>").is_err());
        assert!(parse_table("<row><a>1</a></row>").is_err());
        let table = parse_table(r#"<schema name="t"></schema>"#).unwrap();
        assert!(table.column("x").is_err());
    }

    #[test]
    fn clock_maps_trace_time_to_uptime() {
        let xml = r#"<schema name="time-info"><col><mnemonic>update-time</mnemonic></col><col><mnemonic>mabs-epoch</mnemonic></col><col><mnemonic>timebase-info</mnemonic></col></schema><row><sample-time id="1" fmt="0">0</sample-time><mach-absolute-time id="2" fmt="30">30</mach-absolute-time><mach-timebase-info id="4" fmt="125/3"><mach-timebase-info-field id="5" fmt="125">125</mach-timebase-info-field><mach-timebase-info-field id="6" fmt="3">3</mach-timebase-info-field></mach-timebase-info></row>"#;
        let clock = Clock::from_table(&parse_table(xml).unwrap()).unwrap();
        assert_eq!(
            clock,
            Clock {
                mabs_epoch: 30,
                numer: 125,
                denom: 3
            }
        );
        assert_eq!(clock.uptime_ns(7), 1_257);
    }

    /// Образцы настоящей выгрузки Xcode 27 (`--attach Finder`, 2 с) — проверка формы, а не
    /// калибровки.
    #[test]
    fn parses_recorded_samples() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/");
        let read = |name: &str| std::fs::read_to_string(format!("{dir}{name}")).unwrap();
        let vsync_table = parse_table(&read("xctrace-attach-vsyncs.xml")).unwrap();
        let vsyncs = vsyncs(&vsync_table).unwrap();
        assert!(vsyncs.len() > 10);
        let display = vsyncs[0].display.clone();
        assert!(confirm_rate(&vsyncs, &display, 8_333_333).confirmed);
        let surface_table = parse_table(&read("xctrace-attach-surfaces.xml")).unwrap();
        let surfaces = surfaces(&surface_table).unwrap();
        assert!(!surfaces.is_empty());
        assert!(surfaces.iter().all(|surface| surface.duration_ns > 0));
        let sourced: Vec<Source> = surfaces.iter().filter_map(Surface::source).collect();
        assert!(!sourced.is_empty());
        assert!(
            sourced
                .iter()
                .all(|source| source.process == "app1" && source.pid > 0)
        );
        let clock = Clock::from_table(&parse_table(&read("xctrace-attach-time-info.xml")).unwrap())
            .unwrap();
        assert_eq!((clock.numer, clock.denom), (125, 3));
    }
    #[test]
    fn recorder_waits_for_start_and_stops_on_sigint() {
        let dir = std::env::temp_dir().join(format!("probe-diff-recorder-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let output = dir.join("fake.trace");
        let mut command = std::process::Command::new("/bin/sh");
        command.arg("-c").arg(format!(
            "trap 'touch {out}; exit 0' INT; echo 'Starting'; echo '{RECORDING_STARTED}'; read line; read line",
            out = output.display()
        ));
        let recorder = Recorder::start(command, output.clone()).unwrap();
        let lines = recorder.stop().unwrap();
        assert_eq!(
            lines,
            vec!["Starting".to_string(), RECORDING_STARTED.to_string()]
        );
        assert!(output.exists());
        std::fs::remove_dir_all(&dir).unwrap();

        let mut silent = std::process::Command::new("/bin/sh");
        silent.arg("-c").arg("echo 'no permission'; exit 1");
        let error = Recorder::start(silent, dir.join("none.trace"))
            .err()
            .unwrap();
        assert!(error.contains("no permission"), "{error}");
    }
}
