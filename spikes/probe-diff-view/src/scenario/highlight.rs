//! Сценарий `highlight`: подсветка на worker без окна и без `NSApplication`. Замеры — наблюдение
//! без вердикта: компиляция запроса, индекс строк, предварительный снимок, исправление, полный
//! разбор каждой стороны и запрос видимых строк по готовым деревьям. Выводы — сверка снимков
//! полного разбора с эталоном `--reference`, одно исправление на открытие, расхождение
//! предварительной подсветки у последнего ханка `sparse` и отмена разбора при замене документа.
//!
//! Экран сценария — строки сторон у первых [`SCREEN_DIFF_LINES`] строк diff, у `sparse` также у
//! последних: столько строк diff вмещает первый экран окна сценариев в режиме «Рядом».
//! Синхронизация — каналы снимков и событий worker со сроком [`WAIT`] на каждое ожидание.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use probe_diff_view::fixtures::{self, FixtureName};
use probe_diff_view::highlight::reference::{self, diff_lines};
use probe_diff_view::highlight::{
    Event, HighlightWorker, LOCAL_WINDOW_LINES, LineIndex, LineSet, Observer, SideStyles, Sides,
    Snapshot, Source, Styler, compare,
};
use probe_diff_view::model::{Side, parse};
use probe_diff_view::worker::Cancel;

use crate::results::{Report, Verdict};

/// Срок каждого ожидания снимка или события.
pub const WAIT: Duration = Duration::from_secs(30);
/// Открытий каждого экрана в одном процессе.
pub const REPEATS: usize = 5;
/// Строк diff на экране сценария.
pub const SCREEN_DIFF_LINES: usize = 90;
/// Компиляций запроса подсветки для замера.
pub const COMPILES: usize = 5;
/// Выводов о расхождениях в причине, остальные только считаются.
const SHOWN: usize = 6;

type Timed<T> = (Instant, T);

struct Probe {
    worker: HighlightWorker,
    snapshots: Receiver<Timed<Snapshot>>,
    events: Receiver<Timed<Event>>,
}

/// Worker с наблюдателем. `gate` держит начало полного разбора документа, пока стенд не
/// отпустит его жетоном.
fn probe(gate: Option<(u64, Receiver<()>)>) -> Result<Probe, String> {
    let (snapshots_tx, snapshots) = mpsc::channel();
    let (events_tx, events) = mpsc::channel();
    let gate = gate.map(|(document, release)| (document, Mutex::new(release)));
    let observer: Observer = Arc::new(move |event: &Event| {
        let _ = events_tx.send((Instant::now(), event.clone()));
        if let (Some((gated, release)), Event::ParseStarted { document, .. }) = (&gate, event)
            && document == gated
            && let Ok(release) = release.lock()
        {
            let _ = release.recv_timeout(WAIT);
        }
    });
    let worker = HighlightWorker::spawn(
        move |snapshot| {
            let _ = snapshots_tx.send((Instant::now(), snapshot));
        },
        Some(observer),
    )
    .map_err(|err| format!("worker подсветки не запущен: {err}"))?;
    Ok(Probe {
        worker,
        snapshots,
        events,
    })
}

fn recv<T>(rx: &Receiver<T>, what: &str) -> Result<T, String> {
    match rx.recv_timeout(WAIT) {
        Ok(value) => Ok(value),
        Err(RecvTimeoutError::Timeout) => Err(format!("{what} не пришло за {} с", WAIT.as_secs())),
        Err(RecvTimeoutError::Disconnected) => Err(format!("{what}: worker остановился")),
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

/// Экран: тексты сторон, видимые строки и эталон этих строк.
struct Screen {
    label: String,
    texts: Sides<Arc<[u8]>>,
    visible: Sides<LineSet>,
    expected: Sides<SideStyles>,
}

impl Screen {
    /// Строки снимка, которые расходятся с эталоном, по сторонам.
    fn mismatches(&self, snapshot: &Snapshot) -> Sides<Vec<u32>> {
        self.visible.map(|side, lines| {
            let comparison = compare(self.expected.get(side), snapshot.styles.get(side), lines);
            let mut lines = comparison.mismatched;
            lines.extend(comparison.missing_actual);
            lines.sort_unstable();
            lines
        })
    }
}

fn screens(fixtures_dir: &Path, report: &mut Report) -> Result<Vec<Screen>, String> {
    let mut screens = Vec::new();
    for name in reference::FIXTURES {
        let fixture = fixtures::generate(name);
        let reference = reference::read(fixtures_dir, name.as_str())?;
        reference.check(name.as_str(), fixture.digest())?;
        let file = reference
            .file(&fixture.path)
            .ok_or_else(|| format!("в эталоне {} нет файла {}", name.as_str(), fixture.path))?;
        let expected = file.sides()?;
        let diff = parse::parse(fixture.diff.clone(), &Cancel::never())
            .map_err(|err| format!("{}: diff не разобран: {err:?}", name.as_str()))?;
        let body = diff
            .files()
            .first()
            .and_then(|file| file.text())
            .ok_or_else(|| format!("{}: нет текстового файла", name.as_str()))?;
        let total = body.lines().len();
        let mut positions = vec![("top", 0..SCREEN_DIFF_LINES.min(total))];
        if name == FixtureName::Sparse {
            positions.push(("end", total.saturating_sub(SCREEN_DIFF_LINES)..total));
        }
        report.push(
            &format!("{}.bytes", name.as_str()),
            format!(
                "diff {}, старая сторона {}, новая {}",
                fixture.diff.len(),
                fixture.old.len(),
                fixture.new.len()
            ),
        );
        for (position, range) in positions {
            let label = format!("{}-{position}", name.as_str());
            let visible = diff_lines(body, range);
            for side in Side::BOTH {
                let uncovered: Vec<u32> = visible
                    .get(side)
                    .lines()
                    .filter(|&line| expected.get(side).line(line).is_none())
                    .take(SHOWN)
                    .collect();
                if !uncovered.is_empty() {
                    return Err(format!(
                        "{label}: эталон не покрывает строки {side:?} {uncovered:?}"
                    ));
                }
            }
            let index = Sides {
                old: LineIndex::new(&fixture.old),
                new: LineIndex::new(&fixture.new),
            };
            report.push(
                &format!("{label}.visible"),
                format!(
                    "строк старой стороны {} ({} окон локального разбора), новой {} ({} окон)",
                    visible.old.len(),
                    visible
                        .old
                        .windows(LOCAL_WINDOW_LINES, index.old.count())
                        .len(),
                    visible.new.len(),
                    visible
                        .new
                        .windows(LOCAL_WINDOW_LINES, index.new.count())
                        .len(),
                ),
            );
            screens.push(Screen {
                label,
                texts: Sides {
                    old: Arc::clone(&fixture.old),
                    new: Arc::clone(&fixture.new),
                },
                visible,
                expected: expected.clone(),
            });
        }
    }
    Ok(screens)
}

/// Итоги замеров всех экранов для выводов.
#[derive(Default)]
struct Tally {
    full_mismatches: Vec<String>,
    order_violations: Vec<String>,
    /// Расхождения предварительного снимка `sparse-end` по открытиям: `None` — снимка не было.
    sparse_end: Vec<Option<Sides<usize>>>,
}

fn measure(screen: &Screen, report: &mut Report, tally: &mut Tally) -> Result<(), String> {
    let probe = probe(None)?;
    let label = &screen.label;
    let mut index_ms = Vec::new();
    let mut preliminary_ms = Vec::new();
    let mut correction_ms = Vec::new();
    let mut parse_ms = Sides::<Vec<f64>>::default();
    let mut query_ms = Vec::new();
    let mut preliminary_compute_ms = Vec::new();
    let mut full_compute_ms = Vec::new();
    let mut preliminary_mismatched = Vec::new();
    let mut preliminary_matched = 0;
    for repeat in 1..=REPEATS {
        let started = Instant::now();
        let (document, request) = probe
            .worker
            .open(screen.texts.clone(), screen.visible.clone())
            .map_err(|err| format!("{label}: {err}"))?;
        let mut local: Option<Timed<Snapshot>> = None;
        let (corrected, correction) = loop {
            let (at, snapshot) = recv(&probe.snapshots, &format!("{label}: снимок"))?;
            if snapshot.document != document {
                tally.order_violations.push(format!(
                    "{label} №{repeat}: снимок документа {} во время {document}",
                    snapshot.document
                ));
                continue;
            }
            match snapshot.source {
                Source::Local if local.is_some() => tally
                    .order_violations
                    .push(format!("{label} №{repeat}: второй предварительный снимок")),
                Source::Local => local = Some((at, snapshot)),
                Source::Full => break (at, snapshot),
            }
        };
        if correction.request != request {
            tally.order_violations.push(format!(
                "{label} №{repeat}: исправление отвечает на запрос {} вместо {request}",
                correction.request
            ));
        }
        let mut indexed = None;
        let mut parsed = Sides::<Option<f64>>::default();
        while indexed.is_none() || parsed.old.is_none() || parsed.new.is_none() {
            let (_, event) = recv(&probe.events, &format!("{label}: событие разбора"))?;
            match event {
                Event::Indexed {
                    document: id,
                    elapsed,
                } if id == document => {
                    indexed = Some(ms(elapsed));
                }
                Event::ParseFinished {
                    document: id,
                    side,
                    elapsed,
                    ..
                } if id == document => *parsed.get_mut(side) = Some(ms(elapsed)),
                Event::ParseCancelled {
                    document: id, side, ..
                } if id == document => {
                    return Err(format!(
                        "{label} №{repeat}: разбор {side:?} отменён без замены"
                    ));
                }
                Event::ParseFailed { reason, .. } | Event::QueryFailed { reason } => {
                    return Err(format!("{label}: {reason}"));
                }
                _ => {}
            }
        }
        let asked = Instant::now();
        let next = probe
            .worker
            .visible(screen.visible.clone())
            .map_err(|err| format!("{label}: {err}"))?;
        let (answered, answer) = loop {
            let (at, snapshot) = recv(&probe.snapshots, &format!("{label}: ответ на запрос"))?;
            if snapshot.request == next && snapshot.source == Source::Full {
                break (at, snapshot);
            }
            tally.order_violations.push(format!(
                "{label} №{repeat}: лишний снимок {:?} запроса {}",
                snapshot.source, snapshot.request
            ));
        };
        index_ms.extend(indexed);
        parse_ms.old.extend(parsed.old);
        parse_ms.new.extend(parsed.new);
        correction_ms.push(ms(corrected - started));
        query_ms.push(ms(answered - asked));
        full_compute_ms.push(ms(answer.compute));
        for (name, snapshot) in [("исправление", &correction), ("ответ", &answer)] {
            let wrong = screen.mismatches(snapshot);
            if !wrong.old.is_empty() || !wrong.new.is_empty() {
                tally.full_mismatches.push(format!(
                    "{label} №{repeat}, {name}: старая сторона {:?}, новая {:?}",
                    &wrong.old[..wrong.old.len().min(SHOWN)],
                    &wrong.new[..wrong.new.len().min(SHOWN)]
                ));
            }
        }
        let local_counts = local.map(|(at, snapshot)| {
            preliminary_ms.push(ms(at - started));
            preliminary_compute_ms.push(ms(snapshot.compute));
            let wrong = screen.mismatches(&snapshot);
            Sides {
                old: wrong.old.len(),
                new: wrong.new.len(),
            }
        });
        if let Some(counts) = &local_counts {
            preliminary_mismatched.push((counts.old + counts.new) as f64);
            if counts.old + counts.new == 0 {
                preliminary_matched += 1;
            }
        }
        if label == "sparse-end" {
            tally.sparse_end.push(local_counts);
        }
    }
    report.metric(&format!("{label}.line_index_ms"), "ms", index_ms);
    report.metric(
        &format!("{label}.preliminary_ms"),
        "ms",
        preliminary_ms.clone(),
    );
    report.metric(&format!("{label}.correction_ms"), "ms", correction_ms);
    report.metric(&format!("{label}.parse_old_ms"), "ms", parse_ms.old);
    report.metric(&format!("{label}.parse_new_ms"), "ms", parse_ms.new);
    report.metric(&format!("{label}.visible_query_ms"), "ms", query_ms);
    report.metric(
        &format!("{label}.preliminary_compute_ms"),
        "ms",
        preliminary_compute_ms,
    );
    report.metric(&format!("{label}.full_compute_ms"), "ms", full_compute_ms);
    report.metric(
        &format!("{label}.preliminary_mismatched_lines"),
        "lines",
        preliminary_mismatched,
    );
    report.push(
        &format!("{label}.preliminary"),
        format!(
            "предварительный снимок пришёл в {} из {REPEATS} открытий, совпал с эталоном в {preliminary_matched}",
            preliminary_ms.len()
        ),
    );
    Ok(())
}

/// Замена документа во время полного разбора: разбор прежнего отменяется, его снимков после
/// снимков нового нет, исправление нового совпадает с эталоном.
fn cancel_on_replace(first: &Screen, second: &Screen) -> Result<(Verdict, String), String> {
    let (release, gate) = mpsc::channel();
    let probe = probe(Some((1, gate)))?;
    let (document, _) = probe
        .worker
        .open(first.texts.clone(), first.visible.clone())
        .map_err(|err| err.to_string())?;
    let mut started = 0;
    while started < 2 {
        if let (_, Event::ParseStarted { document: id, .. }) =
            recv(&probe.events, "начало разбора")?
            && id == document
        {
            started += 1;
        }
    }
    let (replacement, _) = probe
        .worker
        .open(second.texts.clone(), second.visible.clone())
        .map_err(|err| err.to_string())?;
    let replaced = Instant::now();
    for _ in 0..2 {
        release
            .send(())
            .map_err(|_| "наблюдатель не ждёт жетона".to_owned())?;
    }
    let mut cancelled = Vec::new();
    let mut finished = Vec::new();
    while cancelled.len() + finished.len() < 2 {
        match recv(&probe.events, "конец разбора заменённого документа")?
        {
            (
                at,
                Event::ParseCancelled {
                    document: id,
                    side,
                    at_byte,
                },
            ) if id == document => {
                cancelled.push(format!(
                    "{side:?} на байте {at_byte} через {:.1} мс",
                    ms(at - replaced)
                ));
            }
            (
                _,
                Event::ParseFinished {
                    document: id, side, ..
                },
            ) if id == document => {
                finished.push(format!("{side:?}"));
            }
            _ => {}
        }
    }
    let mut stale_after_new = 0;
    let mut seen_new = false;
    let correction = loop {
        let (_, snapshot) = recv(&probe.snapshots, "исправление нового документа")?;
        if snapshot.document == replacement {
            seen_new = true;
            if snapshot.source == Source::Full {
                break snapshot;
            }
        } else if seen_new {
            stale_after_new += 1;
        }
    };
    let wrong = second.mismatches(&correction);
    let reason = format!(
        "{} заменён {} во время разбора: отменены {cancelled:?}, дошли до конца {finished:?}, \
         снимков заменённого после нового {stale_after_new}, расхождений исправления с эталоном {}",
        first.label,
        second.label,
        wrong.old.len() + wrong.new.len()
    );
    let pass =
        finished.is_empty() && stale_after_new == 0 && wrong.old.is_empty() && wrong.new.is_empty();
    Ok((if pass { Verdict::Pass } else { Verdict::Fail }, reason))
}

/// Записанное сценарием и ошибка исполнения, если сценарий не дошёл до конца.
pub fn run(fixtures_dir: &Path) -> (Report, Result<(), String>) {
    let mut report = Report::default();
    let outcome = body(fixtures_dir, &mut report);
    (report, outcome)
}

fn body(fixtures_dir: &Path, report: &mut Report) -> Result<(), String> {
    let mut compile_ms = Vec::with_capacity(COMPILES);
    for _ in 0..COMPILES {
        let started = Instant::now();
        let styler = Styler::new()?;
        compile_ms.push(ms(started.elapsed()));
        let unmapped = styler.unmapped_captures();
        if !unmapped.is_empty() {
            return Err(format!("захваты без класса: {unmapped:?}"));
        }
    }
    report.metric("query_compile_ms", "ms", compile_ms);
    report.push("grammar", reference::grammar_fingerprint());
    report.push("local_window_lines", LOCAL_WINDOW_LINES);
    report.push("screen_diff_lines", SCREEN_DIFF_LINES);
    report.push("repeats", REPEATS);

    let screens = screens(fixtures_dir, report)?;
    let mut tally = Tally::default();
    for screen in &screens {
        measure(screen, report, &mut tally)?;
    }

    let checked = screens.len() * REPEATS * 2;
    if tally.full_mismatches.is_empty() {
        report.finding(
            "final_matches_reference",
            Verdict::Pass,
            format!(
                "все {checked} снимков полного разбора совпали с эталоном на всех видимых строках"
            ),
        );
    } else {
        report.finding(
            "final_matches_reference",
            Verdict::Fail,
            format!(
                "{} из {checked} снимков полного разбора расходятся с эталоном: {}",
                tally.full_mismatches.len(),
                tally.full_mismatches[..tally.full_mismatches.len().min(SHOWN)].join("; ")
            ),
        );
    }
    if tally.order_violations.is_empty() {
        report.finding(
            "single_correction",
            Verdict::Pass,
            format!(
                "в каждом из {} открытий не больше одного предварительного снимка, затем ровно одно исправление на запрос открытия",
                screens.len() * REPEATS
            ),
        );
    } else {
        report.finding(
            "single_correction",
            Verdict::Fail,
            tally.order_violations[..tally.order_violations.len().min(SHOWN)].join("; "),
        );
    }
    let detected = tally
        .sparse_end
        .iter()
        .filter(|counts| {
            counts
                .as_ref()
                .is_some_and(|counts| counts.old > 0 && counts.new > 0)
        })
        .count();
    let counts: Vec<String> = tally
        .sparse_end
        .iter()
        .map(|counts| match counts {
            Some(counts) => format!("{}/{}", counts.old, counts.new),
            None => "нет снимка".to_owned(),
        })
        .collect();
    report.finding(
        "preliminary_detects_context",
        if detected == REPEATS && tally.sparse_end.len() == REPEATS {
            Verdict::Pass
        } else {
            Verdict::Fail
        },
        format!(
            "sparse-end: предварительный снимок расходится с эталоном на обеих сторонах в {detected} из {REPEATS} открытий, строк старой/новой: {}",
            counts.join(", ")
        ),
    );
    let first = screens.iter().find(|screen| screen.label == "modified-top");
    let second = screens.iter().find(|screen| screen.label == "sparse-end");
    let (Some(first), Some(second)) = (first, second) else {
        return Err("нет экранов modified-top и sparse-end".to_owned());
    };
    let (verdict, reason) = cancel_on_replace(first, second)?;
    report.finding("cancel_on_replace", verdict, reason);
    Ok(())
}
