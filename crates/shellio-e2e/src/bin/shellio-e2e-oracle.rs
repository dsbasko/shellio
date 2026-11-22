//! `shellio-e2e-oracle` — оракул снимков `make e2e`.
//!
//! Коды выхода: 0 — совпадение (`compare`), линия в допуске (`underline`) или маркер прочитан
//! (`marker`); 1 — ошибка
//! исполнения; 2 — неверный вызов; 3 — маркер совпал, участок вне допуска; 4 — отрисовка
//! ожидаемого поколения не наблюдалась (маркер не читается или другого поколения), снимок
//! повторяется до срока.

#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use shellio_e2e::css::{Layer, Palette};
use shellio_e2e::expect::{self, Geometry};
use shellio_e2e::underline::{UnderlineInputs, UnderlineReport, underline};
use shellio_e2e::{Error, Inputs, Report, Verdict, compare, png, read_marker};

const USAGE: &str = "\
использование:
  shellio-e2e-oracle compare --expect <ожидания.json> --inspect <ui.inspect.json>
                             --window <ui.window.json> --png <снимок sRGB> [--generation <N>]
  shellio-e2e-oracle underline --tab <id вкладки> [--layer none|veil|tone_navigation|tone_input]
                               --inspect <ui.inspect.json> --window <ui.window.json>
                               --png <снимок sRGB> [--generation <N>]
  shellio-e2e-oracle marker --inspect <ui.inspect.json> --window <ui.window.json> --png <снимок sRGB>

compare и underline печатают отчёт JSON в stdout. Ожидаемое поколение — --generation или
поколение ui.window. underline меряет поперечной серией линию под вкладкой (слой по умолчанию
none). marker печатает поколение из маркера снимка.
коды: 0 — совпадение, линия в допуске или маркер прочитан, 1 — ошибка исполнения, 2 — неверный
вызов, 3 — участок или линия вне допуска, 4 — отрисовка ожидаемого поколения не наблюдалась
";

const EXIT_ERROR: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_MISMATCH: u8 = 3;
const EXIT_GENERATION: u8 = 4;

#[derive(Debug, Default)]
struct Args {
    expect: Option<PathBuf>,
    inspect: Option<PathBuf>,
    window: Option<PathBuf>,
    png: Option<PathBuf>,
    generation: Option<u64>,
    tab: Option<String>,
    layer: Option<Layer>,
}

enum Command {
    Compare(Args),
    Underline(Args),
    Marker(Args),
    Help,
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Command, String> {
    let command = args.next().ok_or("нет подкоманды")?;
    let mut parsed = Args::default();
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag}: нет значения"));
        match flag.as_str() {
            "--expect" => parsed.expect = Some(value()?.into()),
            "--inspect" => parsed.inspect = Some(value()?.into()),
            "--window" => parsed.window = Some(value()?.into()),
            "--png" => parsed.png = Some(value()?.into()),
            "--generation" => {
                let text = value()?;
                let generation = text
                    .parse()
                    .map_err(|_| format!("--generation: не число {text:?}"))?;
                parsed.generation = Some(generation);
            }
            "--tab" => parsed.tab = Some(value()?),
            "--layer" => {
                let text = value()?;
                let layer = serde_json::from_value(serde_json::Value::String(text.clone()))
                    .map_err(|_| format!("--layer: неизвестный слой {text:?}"))?;
                parsed.layer = Some(layer);
            }
            "-h" | "--help" => return Ok(Command::Help),
            other => return Err(format!("неизвестный аргумент {other:?}")),
        }
    }
    let required = |name: &str, present: bool| {
        if present {
            Ok(())
        } else {
            Err(format!("{command}: нужен {name}"))
        }
    };
    let underline_only = parsed.tab.is_some() || parsed.layer.is_some();
    match command.as_str() {
        "compare" | "marker" if underline_only => Err(format!(
            "{command}: --tab и --layer принимает только underline"
        )),
        "underline" => {
            if parsed.expect.is_some() {
                return Err("underline: --expect не принимается".to_owned());
            }
            required("--tab", parsed.tab.is_some())?;
            required("--inspect", parsed.inspect.is_some())?;
            required("--window", parsed.window.is_some())?;
            required("--png", parsed.png.is_some())?;
            Ok(Command::Underline(parsed))
        }
        "compare" => {
            required("--expect", parsed.expect.is_some())?;
            required("--inspect", parsed.inspect.is_some())?;
            required("--window", parsed.window.is_some())?;
            required("--png", parsed.png.is_some())?;
            Ok(Command::Compare(parsed))
        }
        "marker" => {
            if parsed.expect.is_some() || parsed.generation.is_some() {
                return Err("marker: --expect и --generation не принимаются".to_owned());
            }
            required("--inspect", parsed.inspect.is_some())?;
            required("--window", parsed.window.is_some())?;
            required("--png", parsed.png.is_some())?;
            Ok(Command::Marker(parsed))
        }
        "-h" | "--help" | "help" => Ok(Command::Help),
        other => Err(format!("неизвестная подкоманда {other:?}")),
    }
}

fn read_text(path: Option<&PathBuf>, what: &str) -> Result<String, Error> {
    let path = path.ok_or_else(|| Error::new(format!("нет пути {what}")))?;
    std::fs::read_to_string(path)
        .map_err(|e| Error::new(format!("{what} {} не читается: {e}", path.display())))
}

fn read_image(path: Option<&PathBuf>) -> Result<png::Image, Error> {
    let path = path.ok_or_else(|| Error::new("нет пути снимка"))?;
    let bytes = std::fs::read(path)
        .map_err(|e| Error::new(format!("снимок {} не читается: {e}", path.display())))?;
    png::decode(&bytes).map_err(|e| Error::new(format!("{}: {e}", path.display())))
}

fn run_compare(args: &Args) -> Result<Report, Error> {
    let palette = Palette::embedded()?;
    let expectations = expect::parse_expectations(&read_text(args.expect.as_ref(), "ожидания")?)?;
    let window = expect::parse_window(&read_text(args.window.as_ref(), "ui.window")?)?;
    let inspect = expect::parse_inspect(&read_text(args.inspect.as_ref(), "ui.inspect")?)?;
    let image = read_image(args.png.as_ref())?;
    compare(&Inputs {
        palette: &palette,
        expectations: &expectations,
        window: &window,
        inspect: &inspect,
        image: &image,
        generation: args.generation.unwrap_or(window.generation),
    })
}

fn run_underline(args: &Args) -> Result<UnderlineReport, Error> {
    let palette = Palette::embedded()?;
    let window = expect::parse_window(&read_text(args.window.as_ref(), "ui.window")?)?;
    let inspect = expect::parse_inspect(&read_text(args.inspect.as_ref(), "ui.inspect")?)?;
    let image = read_image(args.png.as_ref())?;
    underline(&UnderlineInputs {
        palette: &palette,
        window: &window,
        inspect: &inspect,
        image: &image,
        tab: args.tab.as_deref().unwrap_or_default(),
        layer: args.layer.unwrap_or(Layer::None),
        generation: args.generation.unwrap_or(window.generation),
    })
}

fn underline_command(args: &Args) -> u8 {
    let tab = args.tab.as_deref().unwrap_or_default();
    let layer = args.layer.unwrap_or(Layer::None);
    let report =
        run_underline(args).unwrap_or_else(|error| UnderlineReport::error(tab, layer, &error));
    if let Err(error) = print_json(&report) {
        eprintln!("shellio-e2e-oracle: {error}");
        return EXIT_ERROR;
    }
    let reason = report.reason.as_deref().unwrap_or("");
    match report.verdict {
        Verdict::Pass => {
            eprintln!(
                "shellio-e2e-oracle: линия {tab} — {} точки, в допуске",
                report.thickness_points.unwrap_or_default()
            );
            0
        }
        Verdict::Mismatch => {
            eprintln!("shellio-e2e-oracle: линия вне допуска — {reason}");
            EXIT_MISMATCH
        }
        Verdict::Generation => {
            eprintln!("shellio-e2e-oracle: отрисовка поколения не наблюдалась — {reason}");
            EXIT_GENERATION
        }
        Verdict::Error => {
            eprintln!("shellio-e2e-oracle: ошибка исполнения — {reason}");
            EXIT_ERROR
        }
    }
}

fn print_report(report: &Report) -> Result<(), Error> {
    print_json(report)
}

fn print_json(report: &impl serde::Serialize) -> Result<(), Error> {
    let text = serde_json::to_string_pretty(report)
        .map_err(|e| Error::new(format!("отчёт не сериализуется: {e}")))?;
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{text}")
        .and_then(|()| stdout.flush())
        .map_err(|e| Error::new(format!("отчёт не записан: {e}")))
}

fn compare_command(args: &Args) -> u8 {
    let report = run_compare(args).unwrap_or_else(|error| Report::error(&error));
    if let Err(error) = print_report(&report) {
        eprintln!("shellio-e2e-oracle: {error}");
        return EXIT_ERROR;
    }
    let reason = report.reason.as_deref().unwrap_or("");
    match report.verdict {
        Verdict::Pass => {
            eprintln!(
                "shellio-e2e-oracle: участков в допуске {}: {}",
                report.tolerance,
                report.regions.len()
            );
            0
        }
        Verdict::Mismatch => {
            eprintln!("shellio-e2e-oracle: вне допуска — {reason}");
            EXIT_MISMATCH
        }
        Verdict::Generation => {
            eprintln!("shellio-e2e-oracle: отрисовка поколения не наблюдалась — {reason}");
            EXIT_GENERATION
        }
        Verdict::Error => {
            eprintln!("shellio-e2e-oracle: ошибка исполнения — {reason}");
            EXIT_ERROR
        }
    }
}

fn marker_command(args: &Args) -> u8 {
    let read = || -> Result<Result<u64, shellio_e2e::Unreadable>, Error> {
        let window = expect::parse_window(&read_text(args.window.as_ref(), "ui.window")?)?;
        let inspect = expect::parse_inspect(&read_text(args.inspect.as_ref(), "ui.inspect")?)?;
        let image = read_image(args.png.as_ref())?;
        let geometry = Geometry::new(&window, &inspect, (image.width, image.height))?;
        read_marker(&image, &inspect, &geometry)
    };
    match read() {
        Ok(Ok(generation)) => {
            println!("{generation}");
            0
        }
        Ok(Err(unreadable)) => {
            eprintln!("shellio-e2e-oracle: маркер поколения не читается: {unreadable}");
            EXIT_GENERATION
        }
        Err(error) => {
            eprintln!("shellio-e2e-oracle: ошибка исполнения — {error}");
            EXIT_ERROR
        }
    }
}

fn main() -> ExitCode {
    let code = match parse(std::env::args().skip(1)) {
        Ok(Command::Compare(args)) => compare_command(&args),
        Ok(Command::Underline(args)) => underline_command(&args),
        Ok(Command::Marker(args)) => marker_command(&args),
        Ok(Command::Help) => {
            print!("{USAGE}");
            0
        }
        Err(reason) => {
            eprintln!("shellio-e2e-oracle: {reason}\n{USAGE}");
            EXIT_USAGE
        }
    };
    ExitCode::from(code)
}
