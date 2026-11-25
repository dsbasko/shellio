//! `shellio-e2e-oracle` — оракул снимков `make e2e`.
//!
//! Коды выхода: 0 — совпадение (`compare`), линия в допуске (`underline`) или маркер прочитан
//! (`marker`); 1 — ошибка
//! исполнения; 2 — неверный вызов; 3 — маркер совпал, участок вне допуска; 4 — отрисовка
//! ожидаемого поколения не наблюдалась (маркер не читается или другого поколения), снимок
//! повторяется до срока.
//!
//! Режим без маркера (обычное окно, ADR-0021, «Уточнено в P1.4 (планирование)»):
//! `--previous-window` и `--previous-png` — предыдущий снимок серии, `--elapsed-ms` — время от
//! первого снимка. Код 4 с `"retry": true` в отчёте — снимки ещё не устоялись, нужен следующий;
//! с `"retry": false` — срок истёк, вердикт окончательный.

#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use shellio_conformance::calibration::cells;
use shellio_conformance::mask::Mask;
use shellio_conformance::passport::Passport;
use shellio_conformance::registry::Registry;
use shellio_e2e::calibrate::{self, ChromeCell, Measured, Plan};
use shellio_e2e::conform::{self, Mismatch, Product, Snapshot, Tolerances, Unjudged};
use shellio_e2e::css::{Appearance, Contrast, Layer, Palette};
use shellio_e2e::expect::{self, Geometry};
use shellio_e2e::underline::{UnderlineInputs, UnderlineReport, underline};
use shellio_e2e::{
    Error, Inputs, Previous, Report, Verdict, compare, frame_unsettled, png, read_marker,
};

const USAGE: &str = "\
использование:
  shellio-e2e-oracle compare --expect <ожидания.json> --inspect <ui.inspect.json>
                             --window <ui.window.json> --png <снимок sRGB> [--generation <N>]
  shellio-e2e-oracle underline --tab <id вкладки> [--layer none|veil|tone_navigation|tone_input]
                               --inspect <ui.inspect.json> --window <ui.window.json>
                               --png <снимок sRGB> [--generation <N>]
  shellio-e2e-oracle compare|underline … --previous-window <ui.window.json>
                             --previous-png <снимок sRGB> --elapsed-ms <мс>
  shellio-e2e-oracle marker --inspect <ui.inspect.json> --window <ui.window.json> --png <снимок sRGB>
  shellio-e2e-oracle conform --passport <passports/состояние/ячейка.json> --inspect <ui.inspect.json>
                             --window <ui.window.json> --png <снимок sRGB>
                             --registry <registry.toml> --calibration <calibration.json>
                             [--previous-window <ui.window.json> --previous-png <снимок sRGB>
                              --elapsed-ms <мс>]
  shellio-e2e-oracle coverage --passport <passports/состояние/ячейка.json> --inspect <ui.inspect.json>
                              --registry <registry.toml>
  shellio-e2e-oracle calibrate --chrome <каталог shellio-snap calibrate> --product <каталог чтений>
                               --materials <conformance/calibration> --raw <замер.json>
                               --out <calibration.json>

compare и underline печатают отчёт JSON в stdout. Ожидаемое поколение — --generation или
поколение ui.window. underline меряет поперечной серией линию под вкладкой (слой по умолчанию
none). marker печатает поколение из маркера снимка. С --previous-* маркер не читается: снимок
принимается, когда участки двух снимков подряд совпадают в допуске, а поколение ui.window то же;
код 4 и \"retry\": true — нужен следующий снимок, \"retry\": false — срок истёк.
conform сверяет паспорт прототипа с продуктом: состав, элементы по ключу, поля контракта и
стадию снимка; состояние — имя каталога паспорта, маски значков — <ячейка>.icons/ рядом с ним.
Отчёт JSON перечисляет расхождения; поле снимка, которое стадия не судила, — код 1. С --previous-*
снимок принимается, когда поколение ui.window между двумя снимками то же и ни один пиксель окна не
изменился больше допуска; иначе код 4 с тем же \"retry\".
coverage судит только ключи сверки: состав сцены, узел продукта у каждого видимого элемента
паспорта, элемент паспорта у каждого узла с ключом, ключи продукта — из таблицы ключей и без
повторов. Калибровка и снимок не нужны; отчёт той же формы, что у conform.
calibrate выводит допуск ширины текста и порог маски из замеров двух сторон: chrome.json с
масками <ячейка>.icons/ и чтения страницы «Калибровка» <ячейка>.{inspect.json,window.json,png}
каждой ячейки controls.json. Сырые числа пишутся в --raw при любом исходе вывода, --out — только
если контроли разделены; иначе код 1.
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
    previous_window: Option<PathBuf>,
    previous_png: Option<PathBuf>,
    elapsed_ms: Option<u64>,
    passport: Option<PathBuf>,
    registry: Option<PathBuf>,
    calibration: Option<PathBuf>,
    chrome: Option<PathBuf>,
    product: Option<PathBuf>,
    materials: Option<PathBuf>,
    raw: Option<PathBuf>,
    out: Option<PathBuf>,
}

enum Command {
    Compare(Args),
    Underline(Args),
    Marker(Args),
    Conform(Args),
    Coverage(Args),
    Calibrate(Args),
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
            "--previous-window" => parsed.previous_window = Some(value()?.into()),
            "--previous-png" => parsed.previous_png = Some(value()?.into()),
            "--elapsed-ms" => {
                let text = value()?;
                let elapsed = text
                    .parse()
                    .map_err(|_| format!("--elapsed-ms: не число {text:?}"))?;
                parsed.elapsed_ms = Some(elapsed);
            }
            "--passport" => parsed.passport = Some(value()?.into()),
            "--registry" => parsed.registry = Some(value()?.into()),
            "--calibration" => parsed.calibration = Some(value()?.into()),
            "--chrome" => parsed.chrome = Some(value()?.into()),
            "--product" => parsed.product = Some(value()?.into()),
            "--materials" => parsed.materials = Some(value()?.into()),
            "--raw" => parsed.raw = Some(value()?.into()),
            "--out" => parsed.out = Some(value()?.into()),
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
    let stable = [
        parsed.previous_window.is_some(),
        parsed.previous_png.is_some(),
        parsed.elapsed_ms.is_some(),
    ];
    if stable.contains(&true) && stable.contains(&false) {
        return Err(format!(
            "{command}: --previous-window, --previous-png и --elapsed-ms задаются вместе"
        ));
    }
    if stable[0] && (command == "marker" || parsed.generation.is_some()) {
        return Err(format!(
            "{command}: режим без маркера не принимает marker и --generation"
        ));
    }
    let passport_flags = parsed.passport.is_some() || parsed.registry.is_some();
    if passport_flags && command != "conform" && command != "coverage" {
        return Err(format!(
            "{command}: --passport и --registry принимают только conform и coverage"
        ));
    }
    if parsed.calibration.is_some() && command != "conform" {
        return Err(format!("{command}: --calibration принимает только conform"));
    }
    let calibrate_flags = [
        parsed.chrome.is_some(),
        parsed.product.is_some(),
        parsed.materials.is_some(),
        parsed.raw.is_some(),
        parsed.out.is_some(),
    ];
    if calibrate_flags.contains(&true) && command != "calibrate" {
        return Err(format!(
            "{command}: --chrome, --product, --materials, --raw и --out принимает только calibrate"
        ));
    }
    match command.as_str() {
        "calibrate" => {
            let foreign = parsed.expect.is_some()
                || parsed.inspect.is_some()
                || parsed.window.is_some()
                || parsed.png.is_some()
                || parsed.generation.is_some()
                || underline_only
                || stable[0];
            if foreign || calibrate_flags.contains(&false) {
                return Err(
                    "calibrate: нужны ровно --chrome, --product, --materials, --raw и --out"
                        .to_owned(),
                );
            }
            Ok(Command::Calibrate(parsed))
        }
        "conform" => {
            if parsed.expect.is_some() || parsed.generation.is_some() || underline_only {
                return Err(
                    "conform: принимает только --passport, --inspect, --window, --png, \
                            --registry, --calibration и --previous-*"
                        .to_owned(),
                );
            }
            required("--passport", parsed.passport.is_some())?;
            required("--inspect", parsed.inspect.is_some())?;
            required("--window", parsed.window.is_some())?;
            required("--png", parsed.png.is_some())?;
            required("--registry", parsed.registry.is_some())?;
            required("--calibration", parsed.calibration.is_some())?;
            Ok(Command::Conform(parsed))
        }
        "coverage" => {
            let foreign = parsed.expect.is_some()
                || parsed.window.is_some()
                || parsed.png.is_some()
                || parsed.generation.is_some()
                || underline_only
                || stable[0];
            if foreign {
                return Err(
                    "coverage: принимает только --passport, --inspect и --registry".to_owned(),
                );
            }
            required("--passport", parsed.passport.is_some())?;
            required("--inspect", parsed.inspect.is_some())?;
            required("--registry", parsed.registry.is_some())?;
            Ok(Command::Coverage(parsed))
        }
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
    let previous = read_previous(args)?;
    compare(&Inputs {
        palette: &palette,
        expectations: &expectations,
        window: &window,
        inspect: &inspect,
        image: &image,
        generation: args.generation.unwrap_or(window.generation),
        previous: previous.as_ref().map(|(window, image)| Previous {
            window,
            image,
            elapsed_ms: args.elapsed_ms.unwrap_or_default(),
        }),
    })
}

/// Предыдущий снимок режима без маркера.
fn read_previous(args: &Args) -> Result<Option<(expect::WindowView, png::Image)>, Error> {
    if args.previous_window.is_none() {
        return Ok(None);
    }
    let window = expect::parse_window(&read_text(
        args.previous_window.as_ref(),
        "предыдущий ui.window",
    )?)?;
    let image = read_image(args.previous_png.as_ref())?;
    Ok(Some((window, image)))
}

fn run_underline(args: &Args) -> Result<UnderlineReport, Error> {
    let palette = Palette::embedded()?;
    let window = expect::parse_window(&read_text(args.window.as_ref(), "ui.window")?)?;
    let inspect = expect::parse_inspect(&read_text(args.inspect.as_ref(), "ui.inspect")?)?;
    let image = read_image(args.png.as_ref())?;
    let previous = read_previous(args)?;
    underline(&UnderlineInputs {
        palette: &palette,
        window: &window,
        inspect: &inspect,
        image: &image,
        tab: args.tab.as_deref().unwrap_or_default(),
        layer: args.layer.unwrap_or(Layer::None),
        generation: args.generation.unwrap_or(window.generation),
        previous: previous.as_ref().map(|(window, image)| Previous {
            window,
            image,
            elapsed_ms: args.elapsed_ms.unwrap_or_default(),
        }),
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

/// Отчёт подкоманды `conform`.
#[derive(Debug, serde::Serialize)]
struct ConformReport {
    verdict: Verdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// Режим без маркера: при вердикте `generation` — нужен ли следующий снимок до срока.
    #[serde(skip_serializing_if = "Option::is_none")]
    retry: Option<bool>,
    mismatches: Vec<Mismatch>,
    unjudged: Vec<Unjudged>,
}

/// Паспорт, реестр и состояние — имя каталога паспорта: `passports/<состояние>/<ячейка>.json`.
fn read_passport(args: &Args) -> Result<(&PathBuf, String, Passport, Registry), Error> {
    let path = args
        .passport
        .as_ref()
        .ok_or_else(|| Error::new("нет пути паспорта"))?;
    let state = path
        .parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .ok_or_else(|| Error::new(format!("{}: нет каталога состояния", path.display())))?
        .to_owned();
    let text = |e: shellio_conformance::Error| Error::new(e.to_string());
    let passport = Passport::parse(
        &path.display().to_string(),
        &read_text(Some(path), "паспорт")?,
    )
    .map_err(text)?;
    let registry = Registry::parse(
        "registry.toml",
        &read_text(args.registry.as_ref(), "реестр")?,
    )
    .map_err(text)?;
    Ok((path, state, passport, registry))
}

fn run_coverage(args: &Args) -> Result<ConformReport, Error> {
    let (_, state, passport, registry) = read_passport(args)?;
    let product = Product::parse(&read_text(args.inspect.as_ref(), "ui.inspect")?)?;
    let mismatches = conform::coverage(&passport, &product, &registry, &state)?;
    let (verdict, reason) = match mismatches.first() {
        Some(first) => (
            Verdict::Mismatch,
            Some(format!(
                "расхождений: {}; первое — {:?} {}",
                mismatches.len(),
                first.ck,
                first.actual
            )),
        ),
        None => (Verdict::Pass, None),
    };
    Ok(ConformReport {
        verdict,
        state: Some(state),
        reason,
        retry: None,
        mismatches,
        unjudged: Vec::new(),
    })
}

fn run_conform(args: &Args) -> Result<ConformReport, Error> {
    let (path, state, passport, registry) = read_passport(args)?;
    let tolerances =
        Tolerances::from_calibration(&read_text(args.calibration.as_ref(), "калибровка")?)?;
    let inspect_text = read_text(args.inspect.as_ref(), "ui.inspect")?;
    let window = expect::parse_window(&read_text(args.window.as_ref(), "ui.window")?)?;
    let inspect = expect::parse_inspect(&inspect_text)?;
    let image = read_image(args.png.as_ref())?;
    let geometry = Geometry::new(&window, &inspect, (image.width, image.height))?;
    let previous = read_previous(args)?;
    let mut retry = None;
    if let Some((previous_window, previous_image)) = &previous {
        let pair = Previous {
            window: previous_window,
            image: previous_image,
            elapsed_ms: args.elapsed_ms.unwrap_or_default(),
        };
        if let Some((reason, again)) = frame_unsettled(&pair, &window, &inspect, &image)? {
            return Ok(ConformReport {
                verdict: Verdict::Generation,
                state: Some(state),
                reason: Some(reason),
                retry: Some(again),
                mismatches: Vec::new(),
                unjudged: Vec::new(),
            });
        }
        retry = Some(false);
    }
    let product = Product::parse(&inspect_text)?;
    let report = conform::judge(&passport, &product, &registry, &state, tolerances)?;
    let icons = path.with_extension("icons");
    let pixel = |x: u32, y: u32| image.pixel(x, y).map(|p| [p.rgb.r, p.rgb.g, p.rgb.b]);
    let mask = |ck: &str| {
        let file = icons.join(format!("{ck}.png"));
        let bytes = std::fs::read(&file)
            .map_err(|e| format!("эталонная маска {} не читается: {e}", file.display()))?;
        Mask::read_png(&bytes)
    };
    let staged = conform::snapshot_stage(
        &passport,
        &product,
        &registry,
        &state,
        &report,
        tolerances,
        &Snapshot {
            scale: geometry.scale,
            pixel: &pixel,
            mask: &mask,
        },
    );
    let mut mismatches = report.mismatches;
    mismatches.extend(staged.mismatches);
    // Поле снимка без суждения — ошибка исполнения: `pass` и `fail` без него не выносятся.
    let (verdict, reason) = if let Some(first) = staged.unjudged.first() {
        (
            Verdict::Error,
            Some(format!(
                "стадия снимка не судила полей: {}; первое — {} ({})",
                staged.unjudged.len(),
                first.ck,
                first.reason
            )),
        )
    } else if let Some(first) = mismatches.first() {
        (
            Verdict::Mismatch,
            Some(format!(
                "расхождений: {}; первое — {:?} {:?}",
                mismatches.len(),
                first.ck,
                first.field
            )),
        )
    } else {
        (Verdict::Pass, None)
    };
    Ok(ConformReport {
        verdict,
        state: Some(state),
        reason,
        retry,
        mismatches,
        unjudged: staged.unjudged,
    })
}

fn conform_command(args: &Args, run: fn(&Args) -> Result<ConformReport, Error>) -> u8 {
    let report = run(args).unwrap_or_else(|error| ConformReport {
        verdict: Verdict::Error,
        state: None,
        reason: Some(error.to_string()),
        retry: None,
        mismatches: Vec::new(),
        unjudged: Vec::new(),
    });
    if let Err(error) = print_json(&report) {
        eprintln!("shellio-e2e-oracle: {error}");
        return EXIT_ERROR;
    }
    let reason = report.reason.as_deref().unwrap_or("");
    match report.verdict {
        Verdict::Pass => {
            eprintln!("shellio-e2e-oracle: паспорт и продукт совпали");
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

/// Замер двух сторон по ячейкам плана и вывод допусков. Сырые числа пишутся до вывода.
fn run_calibrate(args: &Args) -> Result<String, Error> {
    let dir = |path: &Option<PathBuf>, what: &str| {
        path.clone()
            .ok_or_else(|| Error::new(format!("нет каталога {what}")))
    };
    let (chrome, product, materials) = (
        dir(&args.chrome, "замера Chrome")?,
        dir(&args.product, "чтений продукта")?,
        dir(&args.materials, "материалов")?,
    );
    let material = |name: &str| read_text(Some(&materials.join(name)), name);
    let controls = material("controls.json")?;
    let sheet = calibrate::primitives(&material("icons.svg")?)?;
    let plan = Plan::parse(&material("text.json")?, &controls, &sheet)?;
    let dpr = serde_json::from_str::<serde_json::Value>(&controls)
        .ok()
        .and_then(|v| v["dpr"].as_f64())
        .ok_or_else(|| Error::new("controls.json: нет dpr"))?;
    let record: serde_json::Value = serde_json::from_str(&read_text(
        Some(&chrome.join("chrome.json")),
        "chrome.json",
    )?)
    .map_err(|e| Error::new(format!("chrome.json: {e}")))?;
    if record["dpr"].as_f64() != Some(dpr) {
        return Err(Error::new(format!(
            "chrome.json: масштаб {}, а не {dpr}",
            record["dpr"]
        )));
    }
    let mut measured = Measured::default();
    for cell in cells("controls.json", &controls).map_err(|e| Error::new(e.to_string()))? {
        let cell = cell.name();
        let masks = chrome.join(format!("{cell}.icons"));
        let side = ChromeCell::parse(&record, cell, |id| {
            let file = masks.join(format!("{id}.png"));
            let bytes = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            Mask::read_png(&bytes)
        })?;
        let file = |tail: &str| product.join(format!("{cell}.{tail}"));
        let inspect_text = read_text(Some(&file("inspect.json")), "ui.inspect")?;
        let window = expect::parse_window(&read_text(Some(&file("window.json")), "ui.window")?)?;
        // Имена осей ячейки окна против имени ячейки паспорта.
        let shown = window.cell();
        let named = format!(
            "{}-{}",
            match shown.appearance {
                Appearance::Light => "light",
                Appearance::Dark => "dark",
            },
            match shown.contrast {
                Contrast::Normal => "normal",
                Contrast::Increased => "high",
            }
        );
        if named != cell {
            return Err(Error::new(format!("{cell}: ui.window ячейки {named}")));
        }
        let inspect = expect::parse_inspect(&inspect_text)?;
        let image = read_image(Some(&file("png")))?;
        let geometry = Geometry::new(&window, &inspect, (image.width, image.height))?;
        if geometry.scale != dpr {
            return Err(Error::new(format!(
                "предусловие: {cell}: масштаб окна {}, а не {dpr}",
                geometry.scale
            )));
        }
        let tree = Product::parse(&inspect_text)?;
        let pixel = |x: u32, y: u32| image.pixel(x, y).map(|p| [p.rgb.r, p.rgb.g, p.rgb.b]);
        let measure = calibrate::measure_cell(&plan, &side, &tree, dpr, &pixel)
            .map_err(|e| Error::new(format!("{cell}: {e}")))?;
        measured.insert(cell, measure);
    }
    let write = |path: &Option<PathBuf>, text: &str| {
        let path = path.as_ref().ok_or_else(|| Error::new("нет пути записи"))?;
        std::fs::write(path, text)
            .map_err(|e| Error::new(format!("{} не записан: {e}", path.display())))
    };
    let raw = serde_json::to_string_pretty(&measured.raw())
        .map_err(|e| Error::new(format!("замер не сериализуется: {e}")))?;
    write(&args.raw, &(raw + "\n"))?;
    let text = calibrate::derive(&plan, &measured)?;
    write(&args.out, &text)?;
    Ok(text)
}

fn calibrate_command(args: &Args) -> u8 {
    match run_calibrate(args).and_then(|text| Tolerances::from_calibration(&text)) {
        Ok(tolerances) => {
            eprintln!(
                "shellio-e2e-oracle: калибровка состоялась: допуск ширины текста {} точки, порог \
                 маски {}",
                tolerances.text_width, tolerances.mask_coverage
            );
            0
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
        Ok(Command::Conform(args)) => conform_command(&args, run_conform),
        Ok(Command::Coverage(args)) => conform_command(&args, run_coverage),
        Ok(Command::Calibrate(args)) => calibrate_command(&args),
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
