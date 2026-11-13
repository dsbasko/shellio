//! Сценарий `theme`. В открытом окне сцена проходит системную, светлую и тёмную темы, в каждой
//! обе ячейки контраста через переопределение `--contrast`. В каждом состоянии цвета слоёв
//! заголовка, плиток, вкладок, списка, дерева и палитры сверяются с токенами запрошенной ячейки,
//! толщина линии под активной вкладкой — с `--focus-h`, пиксели рендера плиток — со смешением
//! `--panel` с вуалью или тоном режима. Затем при идущем индикаторе работы включается
//! переопределение Reduce Motion: анимация снимается, поворот дуги больше не меняется, на её
//! месте неподвижная пунктирная дуга с подписью. Уведомление о параметрах отображения
//! доступности стенд посылает сам: системные переключатели процесс не меняет, их проверяет
//! владелец в задаче 14.
//!
//! Ожидания — оформление окна и проход отображения после него, они наступают при любом ответе.
//! Неверная ячейка или цвет — отрицательный вывод, а не ошибка исполнения.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadOnly, Message, msg_send};
use objc2_app_kit::{
    NSAppearance, NSAppearanceCustomization, NSAppearanceNameAqua, NSAppearanceNameDarkAqua,
    NSTableCellView, NSTableView, NSTextField, NSView, NSWorkspace,
    NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification,
};
use objc2_foundation::{NSNumber, NSPoint, NSString, NSUserDefaults, ns_string};
use objc2_quartz_core::CALayer;

use super::events;
use super::readback::{self, Rgba};
use super::snapshot::ViewRender;
use super::{
    Poll, Report, SETTLE_TIMEOUT, Scenario, Settle, Stand, Step, StepFailure, Verdict, poll,
    run_steps,
};
use crate::args::{Contrast, Motion};
use crate::chrome::is_dark;
use crate::fixtures::{self, SCENE_ROWS};
use crate::scene::Scene;
use crate::theme::{self, Choice, Overrides};
use crate::tiles::{BOTTOM, CENTER, Focus, SPIN_KEY, Tile};
use crate::tokens::{self, Cell as ThemeCell, InputMode, Theme, Tint, Tokens};

const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// Если ячейка сцены не догнала оформление окна за это время, шаг всё равно идёт дальше, а
/// расхождение становится выводом.
const CELL_GRACE: Duration = Duration::from_secs(1);
/// Окно наблюдения за поворотом дуги индикатора.
const WATCH: Duration = Duration::from_millis(400);
const TOLERANCE: u8 = 1;
/// Строка центрального списка, которую выбирает сценарий: вверху, вдали от пиксельной пробы.
const SELECTED_ROW: usize = 1;
/// После этого состояния центральный список пустеет и виды его строк уходят в очередь
/// переиспользования; следующая смена ячейки застаёт их там.
const EMPTY_AFTER: usize = 1;
/// После этого состояния строки возвращаются, и их виды выходят из очереди без новой смены.
const REFILL_AFTER: usize = 2;
/// Поворот считается неизменным в пределах этой разницы, в радианах.
const ANGLE_EPSILON: f64 = 1e-6;
/// Сколько устаревших поворотов слоя отображения допустимо в начале окна после снятия
/// анимации: около 50 мс при опросе раз в 5 мс.
const MAX_STALE_SAMPLES: usize = 10;
/// Сколько разных поворотов дуги должно попасть в окно наблюдения у идущей анимации.
const MIN_DISTINCT_ANGLES: usize = 3;

/// Состояния по порядку и имена их шагов.
const STATES: [(Choice, Contrast, &str); 6] = [
    (
        Choice::System,
        Contrast::Normal,
        "системная тема, обычный контраст",
    ),
    (
        Choice::System,
        Contrast::More,
        "системная тема, Increase Contrast",
    ),
    (
        Choice::Light,
        Contrast::Normal,
        "светлая тема, обычный контраст",
    ),
    (
        Choice::Light,
        Contrast::More,
        "светлая тема, Increase Contrast",
    ),
    (
        Choice::Dark,
        Contrast::Normal,
        "тёмная тема, обычный контраст",
    ),
    (
        Choice::Dark,
        Contrast::More,
        "тёмная тема, Increase Contrast",
    ),
];

fn verdict(ok: bool) -> Verdict {
    if ok { Verdict::Pass } else { Verdict::Fail }
}

fn contrast_name(contrast: Contrast) -> &'static str {
    match contrast {
        Contrast::Normal => "normal",
        Contrast::More => "more",
    }
}

fn tint(tint: Tint) -> Rgba {
    let (r, g, b) = tint.color.unit();
    Rgba::from_unit(r, g, b, tint.alpha)
}

fn with_alpha(color: Rgba, alpha: f64) -> Rgba {
    let (r, g, b) = color.unit();
    Rgba::from_unit(r, g, b, alpha)
}

fn background(view: &NSView) -> Option<Rgba> {
    view.layer()
        .and_then(|layer| readback::layer_state(&layer).background)
}

fn border(view: &NSView) -> Option<Rgba> {
    view.layer()
        .and_then(|layer| readback::layer_state(&layer).border)
}

fn ink(field: &NSTextField) -> Option<Rgba> {
    field.textColor().and_then(|color| readback::srgb(&color))
}

/// Цвет подписи ячейки строки `row`, если вид строки создан.
fn row_ink(table: &NSTableView, row: isize) -> Option<Rgba> {
    let view = table.rowViewAtRow_makeIfNecessary(row, false)?;
    view.subviews().iter().find_map(|subview| {
        let cell = subview.downcast::<NSTableCellView>().ok()?;
        // SAFETY: `textField` у NSTableCellView; поле задано при создании ячейки.
        unsafe { cell.textField() }.and_then(|field| ink(&field))
    })
}

fn row_background(table: &NSTableView, row: isize) -> Option<Rgba> {
    let view = table.rowViewAtRow_makeIfNecessary(row, false)?;
    background(&view)
}

/// Поворот дуги вокруг оси z у слоя отображения; `None`, пока слой отображения не создан.
fn presented_angle(layer: &CALayer) -> Option<f64> {
    // SAFETY: слой отображения читается на главном потоке, где живёт дерево слоёв.
    let presented = unsafe { layer.presentationLayer() }?;
    let object: &AnyObject = &presented;
    // SAFETY: `valueForKeyPath:` у CALayer возвращает NSNumber для пути поворота.
    let value: Option<Retained<AnyObject>> =
        unsafe { msg_send![object, valueForKeyPath: ns_string!("transform.rotation.z")] };
    value?
        .downcast::<NSNumber>()
        .ok()
        .map(|number| number.as_f64())
}

fn appearance_name(appearance: Option<&NSAppearance>) -> String {
    appearance.map_or_else(
        || "nil".to_owned(),
        |appearance| appearance.name().to_string(),
    )
}

/// Сверка одного свойства с токеном.
struct Check {
    label: String,
    got: Option<Rgba>,
    want: Rgba,
}

impl Check {
    fn new(label: impl Into<String>, got: Option<Rgba>, want: Rgba) -> Check {
        Check {
            label: label.into(),
            got,
            want,
        }
    }

    fn ok(&self) -> bool {
        self.got
            .is_some_and(|got| got.distance(self.want) <= TOLERANCE)
    }

    fn describe(&self) -> String {
        format!(
            "{} {} (ожидался {})",
            self.label,
            self.got.map_or_else(|| "нет".to_owned(), Rgba::hex),
            self.want.hex()
        )
    }
}

/// Состояние и итог каждой его проверки.
type StateSummary = (String, Vec<(&'static str, bool)>);

struct Run {
    stand: Rc<Stand>,
    scene: Rc<Scene>,
    settle: Rc<Settle>,
    report: RefCell<Report>,
    initial_appearance: Option<Retained<NSAppearance>>,
    initial_overrides: Overrides,
    initial_cell: ThemeCell,
    /// Запрошенное состояние: индекс в `STATES`.
    requested: Cell<Option<usize>>,
    /// Окно получило запрошенное оформление в этот момент; ждётся проход отображения после.
    since: Cell<Option<Duration>>,
    watch_started: Cell<Option<Instant>>,
    angles: RefCell<Vec<Option<f64>>>,
    keys_after_reduce: RefCell<Vec<String>>,
    /// Момент включения Reduce Motion и повороты, прочитанные до прохода отображения после него.
    reduced_at: Cell<Option<Duration>>,
    before_pass: RefCell<Vec<Option<f64>>>,
    /// Отметка опустошения или возврата строк центрального списка; ждётся проход после неё.
    list_mark: Cell<Option<Duration>>,
    /// Видов ячеек и строк центрального списка создано к его опустошению.
    created_before_empty: Cell<Option<(usize, usize)>>,
    /// Сводка по состояниям: имя и итог каждой проверки.
    states: RefCell<Vec<StateSummary>>,
}

impl Run {
    fn app_appearance(&self) -> Option<Retained<NSAppearance>> {
        self.stand.app.appearance()
    }

    fn window_dark(&self) -> bool {
        is_dark(&self.scene.window.effectiveAppearance())
    }

    fn state(&self) -> String {
        format!(
            "NSApp.appearance={} окно={} корень={} ячейка={} движение={:?} входы={:?} переопределения={:?} смен ячейки={} {}",
            appearance_name(self.app_appearance().as_deref()),
            self.scene.window.effectiveAppearance().name(),
            self.scene.root.effectiveAppearance().name(),
            self.scene.cell().name(),
            self.scene.theme.motion(),
            self.scene.theme.inputs(),
            self.scene.theme.overrides(),
            self.scene.theme.changes(),
            events::focus_state(&self.stand.app)
        )
    }

    /// Тёмное ли оформление ожидается при выборе `choice`. У системного — по оформлению
    /// приложения без явного значения, то есть по системе.
    fn expected_dark(&self, choice: Choice) -> bool {
        match choice {
            Choice::Light => false,
            Choice::Dark => true,
            Choice::System => is_dark(&self.stand.app.effectiveAppearance()),
        }
    }

    fn app_matches(&self, choice: Choice) -> bool {
        let current = self.app_appearance();
        // SAFETY: имена оформления — неизменяемые константы AppKit.
        let (aqua, dark) = unsafe { (NSAppearanceNameAqua, NSAppearanceNameDarkAqua) };
        match choice {
            Choice::System => current.is_none(),
            Choice::Light => current.is_some_and(|appearance| &*appearance.name() == aqua),
            Choice::Dark => current.is_some_and(|appearance| &*appearance.name() == dark),
        }
    }

    fn expected_cell(&self, index: usize) -> ThemeCell {
        let (choice, contrast, _) = STATES[index];
        let theme = if self.expected_dark(choice) {
            Theme::Dark
        } else {
            Theme::Light
        };
        ThemeCell::new(theme, contrast)
    }

    /// Следующее состояние или, после последнего, обычное движение перед проверкой индикатора.
    fn advance(&self, index: usize) {
        match index + 1 {
            next if next < STATES.len() => self.request(next),
            _ => self.scene.theme.set_motion(Some(Motion::Normal)),
        }
    }

    fn request(&self, index: usize) {
        let (choice, contrast, _) = STATES[index];
        self.requested.set(Some(index));
        self.since.set(None);
        events::note(format!(
            "запрошено состояние {}: {} {}",
            index,
            choice.name(),
            contrast_name(contrast)
        ));
        choice.apply(&self.stand.app);
        self.scene.theme.set_contrast(Some(contrast));
    }

    /// Окно получило запрошенное оформление и после этого прошёл проход отображения; ячейка
    /// сцены совпала с ожидаемой или истёк срок, после которого расхождение — вывод.
    fn state_ready(&self) -> bool {
        let Some(index) = self.requested.get() else {
            return false;
        };
        let (choice, _, _) = STATES[index];
        let appearance_ok =
            self.app_matches(choice) && self.window_dark() == self.expected_dark(choice);
        if !appearance_ok {
            self.since.set(None);
            return false;
        }
        let now = events::elapsed().unwrap_or_default();
        let since = match self.since.get() {
            Some(since) => since,
            None => {
                self.since.set(Some(now));
                now
            }
        };
        let passed = events::display_pass_after(since).is_some();
        let cell_ok = self.scene.cell() == self.expected_cell(index);
        passed && (cell_ok || now.saturating_sub(since) >= CELL_GRACE)
    }

    fn state_name(index: usize) -> String {
        let (choice, contrast, _) = STATES[index];
        format!("{}_{}", choice.name(), contrast_name(contrast))
    }

    fn check_state(&self, index: usize) -> Result<(), String> {
        let name = Run::state_name(index);
        let expected = self.expected_cell(index);
        let want = tokens::tokens(expected, InputMode::Navigation);
        let cell = self.scene.cell();
        let inputs = self.scene.theme.inputs();
        let cell_ok = cell == expected && inputs.dark == self.window_dark();
        let mut report = self.report.borrow_mut();
        report.finding(
            &format!("cell_{name}"),
            verdict(cell_ok),
            format!(
                "ячейка сцены {} (ожидалась {}); NSApp.appearance {}, оформление окна {}, корня {}; входы темы {inputs:?}",
                cell.name(),
                expected.name(),
                appearance_name(self.app_appearance().as_deref()),
                self.scene.window.effectiveAppearance().name(),
                self.scene.root.effectiveAppearance().name()
            ),
        );
        drop(report);

        let checks = self.layer_checks(&want);
        let failed: Vec<String> = checks
            .iter()
            .filter(|check| !check.ok())
            .map(Check::describe)
            .collect();
        let colors_ok = failed.is_empty();
        let mut report = self.report.borrow_mut();
        report.finding(
            &format!("colors_{name}"),
            verdict(colors_ok),
            if colors_ok {
                format!(
                    "{} свойств слоёв, подписей и окна совпали с токенами ячейки {} в пределах {TOLERANCE}",
                    checks.len(),
                    expected.name()
                )
            } else {
                format!(
                    "расходятся {} из {}: {}",
                    failed.len(),
                    checks.len(),
                    failed.join("; ")
                )
            },
        );
        report.push(
            &format!("colors_{name}"),
            checks
                .iter()
                .map(Check::describe)
                .collect::<Vec<_>>()
                .join("; "),
        );
        drop(report);

        let line_ok = self.check_focus_line(&name, &want);
        let pixels_ok = self.probe_pixels(&name, &want)?;
        self.states.borrow_mut().push((
            format!("{name} → {}", expected.name()),
            vec![
                ("cell", cell_ok),
                ("colors", colors_ok),
                ("focus_line", line_ok),
                ("veil_pixel", pixels_ok),
            ],
        ));
        Ok(())
    }

    fn center(&self) -> Retained<Tile> {
        self.scene.tiles.tile(CENTER)
    }

    /// Виды строк, которые таблица центрального списка держит сейчас.
    fn available_rows(&self) -> usize {
        let count = Cell::new(0);
        let center = self.center();
        if let Some(list) = center.list() {
            crate::list::each_row_view(list.table(), |_| count.set(count.get() + 1));
        }
        count.get()
    }

    /// Строки центрального списка убираются: виды уходят в очередь переиспользования.
    fn empty_center(&self) {
        let center = self.center();
        if let Some(list) = center.list() {
            self.created_before_empty.set(Some(list.created()));
            list.set_rows(Vec::new());
        }
        self.list_mark.set(events::elapsed());
    }

    fn center_empty(&self) -> bool {
        self.list_mark
            .get()
            .and_then(events::display_pass_after)
            .is_some()
            && self.available_rows() == 0
    }

    /// Строки возвращаются после смены ячейки; выбор — прежняя строка.
    fn refill_center(&self) {
        let center = self.center();
        if let Some(list) = center.list() {
            list.set_rows(fixtures::list(SCENE_ROWS));
            list.select(SELECTED_ROW);
        }
        self.list_mark.set(events::elapsed());
    }

    fn center_refilled(&self) -> bool {
        let shown = self.center().list().is_some_and(|list| {
            list.table()
                .rowViewAtRow_makeIfNecessary(SELECTED_ROW as isize, false)
                .is_some()
        });
        self.list_mark
            .get()
            .and_then(events::display_pass_after)
            .is_some()
            && shown
    }

    /// Строки убраны до смены ячейки и возвращены после неё: их цвета дала выдача видов, а не
    /// перекраска видов, которые держала таблица. Вышли ли виды из очереди переиспользования
    /// или созданы заново, решает AppKit; итог записывается рядом.
    fn check_reuse(&self, index: usize) {
        let want = tokens::tokens(self.expected_cell(index), InputMode::Navigation);
        let center = self.center();
        let Some(list) = center.list() else {
            return;
        };
        let table = list.table();
        let selected = SELECTED_ROW as isize;
        let checks = [
            Check::new(
                "выбранная строка",
                row_background(table, selected),
                want.sel,
            ),
            Check::new("её подпись", row_ink(table, selected), want.text),
            Check::new(
                "подпись соседней строки",
                row_ink(table, selected + 1),
                want.text,
            ),
        ];
        let neighbour_clear = row_background(table, selected + 1).is_none();
        let created = list.created();
        let before = self.created_before_empty.get();
        let from_queue = before == Some(created);
        let ok = checks.iter().all(Check::ok) && neighbour_clear;
        self.report.borrow_mut().finding(
            "list_refill_colors",
            verdict(ok),
            format!(
                "строки центрального списка убраны до смены ячейки на {} и возвращены после неё: {}; заливки у соседней строки нет: {neighbour_clear}; видов ячеек и строк создано {before:?} до опустошения и {created:?} после возврата, все виды из очереди переиспользования: {from_queue}",
                self.expected_cell(index).name(),
                checks
                    .iter()
                    .map(Check::describe)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }

    /// Цвета, которые компоненты держат у себя, против токенов `want`.
    fn layer_checks(&self, want: &Tokens) -> Vec<Check> {
        let scene = &self.scene;
        let mut checks = vec![
            Check::new(
                "фон окна",
                readback::srgb(&scene.window.backgroundColor()),
                want.chrome,
            ),
            Check::new("корень", background(&scene.root), want.chrome),
            Check::new("заголовок", background(&scene.chrome), want.chrome),
            Check::new(
                "делитель сплита",
                readback::srgb(&scene.split.split_view.dividerColor()),
                want.chrome,
            ),
        ];
        if let Some(parts) = scene.chrome.parts() {
            checks.push(Check::new(
                "линия заголовка",
                background(&parts.line),
                want.line,
            ));
            checks.push(Check::new(
                "поле поиска",
                background(&parts.search),
                want.surface,
            ));
            checks.push(Check::new("рамка поиска", border(&parts.search), want.line));
            checks.push(Check::new(
                "подсказка поиска",
                ink(parts.search.hint()),
                want.text_3,
            ));
        }
        let tiles = &scene.tiles;
        for (label, host) in [
            ("хост центра", &tiles.center_host),
            ("хост правой зоны", &tiles.right_host),
        ] {
            checks.push(Check::new(label, background(host), want.chrome));
        }
        let navigator = &tiles.navigator;
        let outline = navigator.tree().outline();
        checks.push(Check::new("навигатор", background(navigator), want.chrome));
        checks.push(Check::new(
            "фон дерева",
            readback::srgb(&outline.backgroundColor()),
            want.chrome,
        ));
        let selected = outline.selectedRow();
        checks.push(Check::new(
            "выбранная строка дерева",
            row_background(outline, selected),
            want.sel,
        ));
        checks.push(Check::new(
            "подпись выбранной строки дерева",
            row_ink(outline, selected),
            want.text,
        ));
        let other = if selected == 0 { 1 } else { 0 };
        checks.push(Check::new(
            "подпись строки дерева",
            row_ink(outline, other),
            want.text_2,
        ));
        let focus = tiles.focus();
        for (index, tile) in tiles.all().iter().enumerate() {
            checks.push(Check::new(
                format!("плитка {index}"),
                background(tile),
                want.panel,
            ));
            checks.push(Check::new(
                format!("рамка плитки {index}"),
                border(tile),
                want.line,
            ));
            let overlay = if focus == Focus::Tile(index) {
                tint(want.tone())
            } else {
                tint(want.dim)
            };
            checks.push(Check::new(
                format!("слой над плиткой {index}"),
                background(tile.veil()),
                overlay,
            ));
            let strip = tile.strip();
            checks.push(Check::new(
                format!("полоса вкладок {index}"),
                background(strip),
                want.panel_head,
            ));
            checks.push(Check::new(
                format!("линия полосы {index}"),
                background(strip.border()),
                want.line,
            ));
            checks.push(Check::new(
                format!("нагрузка полосы {index}"),
                ink(strip.meter()),
                want.text_3,
            ));
            let views = strip.tab_views();
            let active = strip.active();
            for (tab_index, tab) in views.iter().enumerate() {
                if Some(tab_index) == active {
                    checks.push(Check::new(
                        format!("активная вкладка {index}"),
                        background(tab),
                        want.panel,
                    ));
                    checks.push(Check::new(
                        format!("рамка активной вкладки {index}"),
                        border(tab),
                        want.line,
                    ));
                    checks.push(Check::new(
                        format!("подпись активной вкладки {index}"),
                        ink(tab.title()),
                        want.text,
                    ));
                } else if tab_index == usize::from(active == Some(0)) {
                    checks.push(Check::new(
                        format!("подпись вкладки {index}.{tab_index}"),
                        ink(tab.title()),
                        want.text_3,
                    ));
                }
            }
            if let Some(list) = tile.list() {
                let table = list.table();
                checks.push(Check::new(
                    format!("фон списка {index}"),
                    readback::srgb(&table.backgroundColor()),
                    want.panel,
                ));
                if let Some(row) = list.selected() {
                    let row = row as isize;
                    checks.push(Check::new(
                        format!("выбранная строка списка {index}"),
                        row_background(table, row),
                        want.sel,
                    ));
                    checks.push(Check::new(
                        format!("подпись строки списка {index}"),
                        row_ink(table, row),
                        want.text,
                    ));
                }
            }
        }
        let bottom = tiles.tile(BOTTOM);
        if let Some(indicator) = bottom.indicator() {
            checks.push(Check::new(
                "дорожка индикатора",
                indicator
                    .track()
                    .strokeColor()
                    .as_deref()
                    .and_then(readback::cg_srgb),
                with_alpha(want.run, 0.25),
            ));
            checks.push(Check::new(
                "дуга индикатора",
                indicator
                    .arc()
                    .strokeColor()
                    .as_deref()
                    .and_then(readback::cg_srgb),
                want.run,
            ));
            checks.push(Check::new(
                "подпись индикатора",
                ink(indicator.status()),
                want.run,
            ));
        }
        let palette = &scene.palette;
        checks.push(Check::new(
            "поверхность палитры",
            background(palette.surface()),
            want.surface,
        ));
        checks.push(Check::new(
            "рамка палитры",
            border(palette.surface()),
            want.line_2,
        ));
        checks.push(Check::new(
            "фон поля палитры",
            palette
                .field()
                .backgroundColor()
                .and_then(|color| readback::srgb(&color)),
            want.surface,
        ));
        checks.push(Check::new(
            "текст поля палитры",
            ink(palette.field()),
            want.text,
        ));
        checks.push(Check::new(
            "линия палитры",
            background(palette.separator()),
            want.line,
        ));
        checks.push(Check::new(
            "фон списка палитры",
            readback::srgb(&palette.list().table().backgroundColor()),
            want.surface,
        ));
        checks.push(Check::new(
            "надпись пустого результата",
            ink(palette.empty_label()),
            want.text_3,
        ));
        for (index, (layer, shadow)) in palette.shadows().iter().zip(&want.shadow).enumerate() {
            let got = layer
                .shadowColor()
                .as_deref()
                .and_then(readback::cg_srgb)
                .map(|color| with_alpha(color, f64::from(layer.shadowOpacity())));
            checks.push(Check::new(
                format!("тень палитры {index}"),
                got,
                tint(shadow.color),
            ));
            checks.push(Check::new(
                format!("поверхность под тенью {index}"),
                readback::layer_state(layer).background,
                want.surface,
            ));
        }
        checks
    }

    /// Линия под активной вкладкой плитки с фокусом: видима, цвета `--focus` и толщины
    /// `--focus-h` ячейки.
    fn check_focus_line(&self, name: &str, want: &Tokens) -> bool {
        let Focus::Tile(focused) = self.scene.tiles.focus() else {
            self.report.borrow_mut().finding(
                &format!("focus_line_{name}"),
                Verdict::Fail,
                "фокус не на плитке, линии под вкладкой нет",
            );
            return false;
        };
        let tile = self.scene.tiles.tile(focused);
        let line = tile.strip().line();
        let height = line.frame().size.height;
        let color = background(line);
        let ok = !line.isHiddenOrHasHiddenAncestor()
            && (height - want.focus_h).abs() < 1e-9
            && color.is_some_and(|color| color.distance(want.focus) <= TOLERANCE);
        self.report.borrow_mut().finding(
            &format!("focus_line_{name}"),
            verdict(ok),
            format!(
                "линия под активной вкладкой плитки {focused}: скрыта {}, толщина {height} (ожидалась --focus-h {}), цвет {} (ожидался {})",
                line.isHiddenOrHasHiddenAncestor(),
                want.focus_h,
                color.map_or_else(|| "нет".to_owned(), Rgba::hex),
                want.focus.hex()
            ),
        );
        ok
    }

    /// Пиксельная проба рендера содержимого: плитка с фокусом — тон режима поверх `--panel`,
    /// остальные — вуаль поверх `--panel`, навигатор — фон окна.
    fn probe_pixels(&self, name: &str, want: &Tokens) -> Result<bool, String> {
        let root = &self.scene.root;
        let render = ViewRender::of(root)?;
        let tiles = &self.scene.tiles;
        let focus = tiles.focus();
        let mut probes: Vec<(String, NSPoint, Rgba, Option<Rgba>)> = tiles
            .all()
            .iter()
            .enumerate()
            .map(|(index, tile)| {
                let body = tile.body();
                let center = events::center(body.bounds());
                let shift = if tile.list().is_some() {
                    3.0 * want.row
                } else {
                    0.0
                };
                let point =
                    body.convertPoint_toView(NSPoint::new(center.x, center.y - shift), Some(root));
                let overlay = if focus == Focus::Tile(index) {
                    want.tone()
                } else {
                    want.dim
                };
                let in_space = render.blend(want.panel, overlay.color, overlay.alpha);
                (
                    format!("плитка {index}"),
                    point,
                    overlay.over(want.panel),
                    in_space,
                )
            })
            .collect();
        let navigator = &tiles.navigator;
        let size = navigator.bounds().size;
        probes.push((
            "навигатор".to_owned(),
            navigator.convertPoint_toView(
                NSPoint::new(size.width - 40.0, size.height - 40.0),
                Some(root),
            ),
            want.chrome,
            None,
        ));
        let mut ok = true;
        let mut lines = Vec::new();
        for (label, point, expected, in_space) in probes {
            let got = render.pixel(point)?;
            ok &= got.distance(expected) <= TOLERANCE;
            let space = in_space.map_or_else(String::new, |in_space| {
                format!(", смешение в пространстве bitmap {}", in_space.hex())
            });
            lines.push(format!(
                "{label} {} (ожидался {}{space})",
                got.hex(),
                expected.hex()
            ));
        }
        self.report.borrow_mut().finding(
            &format!("veil_pixel_{name}"),
            verdict(ok),
            format!(
                "рендер вида корня в пространстве «{}»; ожидание — смешение в кодированных значениях sRGB, допуск {TOLERANCE}: {}",
                render.color_space(),
                lines.join(", ")
            ),
        );
        Ok(ok)
    }

    fn arc(&self) -> Option<Retained<objc2_quartz_core::CAShapeLayer>> {
        self.scene
            .tiles
            .tile(BOTTOM)
            .indicator()
            .map(|indicator| indicator.arc().retain())
    }

    fn arc_keys(&self) -> Vec<String> {
        self.arc()
            .map(|arc| readback::animation_keys(&arc))
            .unwrap_or_default()
    }

    /// Собирает повороты дуги, пока не истечёт окно наблюдения.
    fn watch(&self) -> bool {
        let started = match self.watch_started.get() {
            Some(started) => started,
            None => {
                let now = Instant::now();
                self.watch_started.set(Some(now));
                self.angles.borrow_mut().clear();
                now
            }
        };
        let angle = self.arc().and_then(|arc| presented_angle(&arc));
        self.angles.borrow_mut().push(angle);
        started.elapsed() >= WATCH
    }

    /// До прохода отображения после включения Reduce Motion повороты копятся отдельно: снятие
    /// анимации доходит до слоя отображения только с фиксацией транзакции.
    fn watch_after_reduce(&self) -> bool {
        let passed = self
            .reduced_at
            .get()
            .and_then(events::display_pass_after)
            .is_some();
        if !passed {
            let angle = self.arc().and_then(|arc| presented_angle(&arc));
            self.before_pass.borrow_mut().push(angle);
            return false;
        }
        self.watch()
    }

    fn end_watch(&self) -> Vec<Option<f64>> {
        self.watch_started.set(None);
        self.angles.replace(Vec::new())
    }

    fn check_spinning(&self) {
        let angles = self.end_watch();
        let keys = self.arc_keys();
        let mut distinct: Vec<f64> = Vec::new();
        for angle in angles.iter().flatten() {
            if distinct
                .iter()
                .all(|seen| (seen - angle).abs() > ANGLE_EPSILON)
            {
                distinct.push(*angle);
            }
        }
        let has_key = keys.iter().any(|key| key == SPIN_KEY);
        let ok = has_key && distinct.len() >= MIN_DISTINCT_ANGLES;
        self.report.borrow_mut().finding(
            "spin_running",
            verdict(ok),
            format!(
                "до Reduce Motion: ключи анимаций дуги {keys:?}; за {} мс прочитано {} поворотов слоя отображения, разных {} (нужно не меньше {MIN_DISTINCT_ANGLES})",
                WATCH.as_millis(),
                angles.len(),
                distinct.len()
            ),
        );
    }

    fn check_reduced(&self) {
        let angles = self.end_watch();
        let keys_now = self.arc_keys();
        let keys_at_switch = self.keys_after_reduce.borrow().clone();
        let before_pass = self.before_pass.borrow().clone();
        let bottom = self.scene.tiles.tile(BOTTOM);
        let Some(indicator) = bottom.indicator() else {
            self.report.borrow_mut().finding(
                "reduce_motion_static",
                Verdict::Fail,
                "в нижней плитке нет индикатора работы",
            );
            return;
        };
        let read: Vec<f64> = angles.iter().flatten().copied().collect();
        let model = presented_angle_model(indicator.arc());
        let at_model = |angle: &f64| (angle - model).abs() < ANGLE_EPSILON;
        // Слой отображения может отстать от фиксации транзакции на кадр: ведущие устаревшие
        // повороты допустимы, но после первого равного модели поворот больше не меняется.
        let settled = read.iter().position(at_model);
        let frozen = read.len() == angles.len()
            && settled.is_some_and(|first| {
                first <= MAX_STALE_SAMPLES && read[first..].iter().all(at_model)
            });
        let moved: Vec<(usize, f64)> = read
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, angle)| !at_model(angle))
            .take(5)
            .collect();
        let dash: Vec<f64> = indicator
            .arc()
            .lineDashPattern()
            .map(|pattern| pattern.iter().map(|step| step.as_f64()).collect())
            .unwrap_or_default();
        let status = indicator.status();
        let shown = !indicator.spinner().isHiddenOrHasHiddenAncestor()
            && !status.isHiddenOrHasHiddenAncestor()
            && !status.stringValue().to_string().is_empty();
        let ok = keys_at_switch.is_empty()
            && keys_now.is_empty()
            && frozen
            && dash == [2.0, 2.0]
            && shown
            && self.scene.theme.motion() == Motion::Reduced
            && indicator.motion() == Motion::Reduced;
        self.report.borrow_mut().finding(
            "reduce_motion_static",
            verdict(ok),
            format!(
                "ключи анимаций дуги сразу после переключения {keys_at_switch:?}, через {} мс {keys_now:?}; поворотов слоя отображения прочитано {} из {}, с первого равного модели {model} (№ {settled:?}, допустимо до {MAX_STALE_SAMPLES}) больше не меняются: {frozen}, отличные {moved:?}; до прохода отображения после переключения прочитано отдельно {before_pass:?}; пунктир дуги {dash:?} (ожидался [2, 2]); дуга и подпись «{}» видимы: {shown}; движение темы {:?}, индикатора {:?}",
                WATCH.as_millis(),
                read.len(),
                angles.len(),
                status.stringValue(),
                self.scene.theme.motion(),
                indicator.motion()
            ),
        );
    }

    /// Обычное движение возвращает анимацию, остановка скрывает индикатор вместе с подписью.
    fn check_restart_and_stop(&self) {
        let bottom = self.scene.tiles.tile(BOTTOM);
        let Some(indicator) = bottom.indicator() else {
            return;
        };
        self.scene.theme.set_motion(Some(Motion::Normal));
        let keys = self.arc_keys();
        let solid = indicator.arc().lineDashPattern().is_none();
        let restored = keys.iter().any(|key| key == SPIN_KEY) && solid;
        indicator.set_running(false);
        let hidden = indicator.spinner().isHidden() && indicator.status().isHidden();
        let keys_stopped = self.arc_keys();
        indicator.set_running(true);
        let keys_running = self.arc_keys();
        let mut report = self.report.borrow_mut();
        report.finding(
            "motion_restored",
            verdict(restored),
            format!(
                "после возврата обычного движения ключи дуги {keys:?}, сплошная линия: {solid}"
            ),
        );
        report.finding(
            "indicator_stop",
            verdict(
                hidden
                    && keys_stopped.is_empty()
                    && keys_running.iter().any(|key| key == SPIN_KEY),
            ),
            format!(
                "остановленный индикатор скрыт вместе с подписью: {hidden}, ключи дуги {keys_stopped:?}; после запуска {keys_running:?}"
            ),
        );
    }

    /// Уведомление о параметрах отображения доступности перечитывает системные значения.
    fn check_notification(&self) {
        let theme = &self.scene.theme;
        theme.set_contrast(None);
        theme.set_motion(None);
        let before = theme.notifications();
        let workspace = NSWorkspace::sharedWorkspace();
        let object: &AnyObject = &workspace;
        // SAFETY: имя уведомления — неизменяемая константа AppKit; уведомление адресовано
        // центру уведомлений рабочего стола этого процесса.
        unsafe {
            workspace.notificationCenter().postNotificationName_object(
                NSWorkspaceAccessibilityDisplayOptionsDidChangeNotification,
                Some(object),
            )
        };
        let fresh = theme::Inputs {
            dark: is_dark(&self.scene.root.effectiveAppearance()),
            increase_contrast: workspace.accessibilityDisplayShouldIncreaseContrast(),
            reduce_motion: workspace.accessibilityDisplayShouldReduceMotion(),
        };
        let expected = theme::select(fresh, Overrides::default());
        let ok = theme.notifications() == before + 1
            && theme.inputs() == fresh
            && theme.selection() == expected;
        self.report.borrow_mut().finding(
            "display_options_notification",
            verdict(ok),
            format!(
                "уведомлений о параметрах доступности {before} → {}; входы темы {:?}, системные {fresh:?}; выбор {:?} (ожидался {expected:?}). Живое переключение системных настроек проверяет владелец в задаче 14",
                theme.notifications(),
                theme.inputs(),
                theme.selection()
            ),
        );
    }

    fn restore(&self) {
        self.stand
            .app
            .setAppearance(self.initial_appearance.as_deref());
        let theme = &self.scene.theme;
        theme.set_contrast(self.initial_overrides.contrast);
        theme.set_motion(self.initial_overrides.motion);
        self.since.set(None);
    }

    fn restored(&self) -> bool {
        let app_ok = match (&self.initial_appearance, self.app_appearance()) {
            (None, None) => true,
            (Some(initial), Some(current)) => initial.name() == current.name(),
            _ => false,
        };
        let cell_ok = self.scene.cell() == self.initial_cell;
        if !(app_ok && cell_ok) {
            return false;
        }
        let now = events::elapsed().unwrap_or_default();
        let since = self.since.get().unwrap_or(now);
        self.since.set(Some(since));
        events::display_pass_after(since).is_some()
    }
}

/// Поворот дуги у модели слоя: анимация его не меняет.
fn presented_angle_model(layer: &CALayer) -> f64 {
    let object: &AnyObject = layer;
    // SAFETY: `valueForKeyPath:` у CALayer возвращает NSNumber для пути поворота.
    let value: Option<Retained<AnyObject>> =
        unsafe { msg_send![object, valueForKeyPath: ns_string!("transform.rotation.z")] };
    value
        .and_then(|value| value.downcast::<NSNumber>().ok())
        .map_or(f64::NAN, |number| number.as_f64())
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Theme);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let Some(scene) = stand.built_scene.clone() else {
        return stand.conclude(report, Err("сцена не собрана".to_owned()));
    };
    let initial_appearance = stand.app.appearance();
    let workspace = NSWorkspace::sharedWorkspace();
    report.push(
        "initial_app_appearance",
        appearance_name(initial_appearance.as_deref()),
    );
    report.push(
        "system_appearance",
        stand.app.effectiveAppearance().name().to_string(),
    );
    report.push(
        "apple_interface_style",
        NSUserDefaults::standardUserDefaults()
            .stringForKey(&NSString::from_str("AppleInterfaceStyle"))
            .map_or_else(|| "нет (светлая)".to_owned(), |style| style.to_string()),
    );
    report.push(
        "system_increase_contrast",
        workspace.accessibilityDisplayShouldIncreaseContrast(),
    );
    report.push(
        "system_reduce_motion",
        workspace.accessibilityDisplayShouldReduceMotion(),
    );
    report.push("initial_cell", scene.cell().name());
    report.push("initial_motion", format!("{:?}", scene.theme.motion()));
    report.push(
        "states",
        STATES
            .iter()
            .map(|(choice, contrast, _)| format!("{} {}", choice.name(), contrast_name(*contrast)))
            .collect::<Vec<_>>()
            .join(", "),
    );
    let run = Rc::new(Run {
        settle: Settle::new(vec![scene.window.clone()]),
        stand: Rc::clone(&stand),
        initial_overrides: scene.theme.overrides(),
        initial_cell: scene.cell(),
        scene,
        report: RefCell::new(report),
        initial_appearance,
        requested: Cell::new(None),
        since: Cell::new(None),
        watch_started: Cell::new(None),
        angles: RefCell::new(Vec::new()),
        keys_after_reduce: RefCell::new(Vec::new()),
        reduced_at: Cell::new(None),
        before_pass: RefCell::new(Vec::new()),
        list_mark: Cell::new(None),
        created_before_empty: Cell::new(None),
        states: RefCell::new(Vec::new()),
    });
    run_steps(steps(&run), move |result| finish(run, result));
}

fn step(
    run: &Rc<Run>,
    name: &'static str,
    ready: impl Fn(&Run) -> bool + 'static,
    act: impl FnOnce(&Run) -> Result<(), String> + 'static,
) -> Step {
    let (for_ready, for_act) = (Rc::clone(run), Rc::clone(run));
    Step::new(
        name,
        STEP_TIMEOUT,
        move || poll(ready(&for_ready), || for_ready.state()),
        move || act(&for_act),
    )
}

fn steps(run: &Rc<Run>) -> Vec<Step> {
    let first = {
        let run = Rc::clone(run);
        move || {
            if let Poll::Pending(state) = run.settle.poll() {
                return Poll::Pending(state);
            }
            let app = &run.stand.app;
            let key = app.keyWindow();
            poll(
                app.isActive() && key.is_some_and(|key| events::same(&*key, &*run.scene.window)),
                || run.state(),
            )
        }
    };
    let warp = {
        let run = Rc::clone(run);
        move || events::warp_cursor_outside(run.stand.app.mtm(), &run.stand.app).map(|_| ())
    };
    let mut steps = vec![
        Step::new(
            "окно неподвижно, активность и key window",
            SETTLE_TIMEOUT,
            first,
            warp,
        ),
        step(
            run,
            "курсор вне окон",
            |run| events::cursor_outside(&run.stand.app),
            |run| {
                let tiles = &run.scene.tiles;
                if tiles.focus() != Focus::Tile(CENTER) {
                    return Err(format!(
                        "начальный фокус сцены {:?}, ожидалась центральная плитка",
                        tiles.focus()
                    ));
                }
                if let Some(list) = tiles.tile(CENTER).list() {
                    list.select(SELECTED_ROW);
                }
                run.request(0);
                Ok(())
            },
        ),
    ];
    for (index, (_, _, name)) in STATES.iter().enumerate() {
        steps.push(step(run, name, Run::state_ready, move |run| {
            run.check_state(index)?;
            match index {
                EMPTY_AFTER => run.empty_center(),
                REFILL_AFTER => run.refill_center(),
                _ => run.advance(index),
            }
            Ok(())
        }));
        if index == EMPTY_AFTER {
            steps.push(step(
                run,
                "центральный список пуст, виды строк в очереди",
                Run::center_empty,
                move |run| {
                    run.advance(index);
                    Ok(())
                },
            ));
        }
        if index == REFILL_AFTER {
            steps.push(step(
                run,
                "строки центрального списка вышли из очереди",
                Run::center_refilled,
                move |run| {
                    run.check_reuse(index);
                    run.advance(index);
                    Ok(())
                },
            ));
        }
    }
    steps.push(step(
        run,
        "индикатор идёт: окно наблюдения",
        Run::watch,
        |run| {
            run.check_spinning();
            run.scene.theme.set_motion(Some(Motion::Reduced));
            run.reduced_at.set(events::elapsed());
            *run.keys_after_reduce.borrow_mut() = run.arc_keys();
            Ok(())
        },
    ));
    steps.push(step(
        run,
        "Reduce Motion: проход отображения и окно наблюдения",
        Run::watch_after_reduce,
        |run| {
            run.check_reduced();
            run.check_restart_and_stop();
            run.check_notification();
            run.restore();
            Ok(())
        },
    ));
    steps.push(step(
        run,
        "исходное оформление и ячейка восстановлены",
        Run::restored,
        |_| Ok(()),
    ));
    steps
}

fn finish(run: Rc<Run>, result: Result<(), StepFailure>) {
    let stand = Rc::clone(&run.stand);
    let mut report = run.report.replace(Report::default());
    run.settle.add_to(&mut report);
    report.push("theme_changes", run.scene.theme.changes());
    report.push(
        "root_appearance_changes",
        run.scene.root.appearance_changes(),
    );
    let states = run.states.borrow();
    if result.is_ok() {
        let ok = states.len() == STATES.len()
            && states
                .iter()
                .all(|(_, checks)| checks.iter().all(|(_, ok)| *ok));
        report.finding(
            "theme_cells",
            verdict(ok),
            format!(
                "проверки по состояниям: {}",
                states
                    .iter()
                    .map(|(name, checks)| format!(
                        "{name}: {}",
                        checks
                            .iter()
                            .map(|(check, ok)| format!(
                                "{check} {}",
                                if *ok { "pass" } else { "fail" }
                            ))
                            .collect::<Vec<_>>()
                            .join(" ")
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        );
    }
    drop(states);
    let result = result.map_err(|failure| failure.describe());
    drop(run);
    stand.conclude(report, result);
}
