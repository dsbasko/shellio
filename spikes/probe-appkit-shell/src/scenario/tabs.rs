//! Сценарий `tabs`: полосы вкладок центральной и боковой плиток на наборах из 1, 8 и 30 вкладок,
//! средний режим ADR-0032 в окне шире минимума, переключение клавишами, щелчком и нажатием
//! доступности, включая прокрученные за край вкладки, клавиши только у видимой полосы с фокусом и
//! 100 переключений с замером. Проверки читают состояние AppKit: кадры, слои, пиксели рендера,
//! дерево доступности и меню.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::{MainThreadMarker, MainThreadOnly, msg_send};
use objc2_app_kit::{NSEventModifierFlags, NSView, NSWindow};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use serde::Serialize;

use super::counters;
use super::events::{self, Key};
use super::material;
use super::readback::{self, Rgba};
use super::snapshot::{self, Background, ViewRender};
use super::split::Stable;
use super::{
    Poll, Report, SETTLE_TIMEOUT, Scenario, Settle, Stand, Step, StepFailure, Verdict, poll,
    run_steps,
};
use crate::fixtures::{self, TAB_COUNTS};
use crate::results::{Rect, Snapshot, SnapshotStatus};
use crate::scene::Scene;
use crate::tabs::{
    self, BUTTON, GAP, LINE_INSET, MENU_LABEL, METER_PAD, ROW_INSET, Regime, TabStrip, ZOOM_ACTION,
};
use crate::tiles::{BOTTOM, CENTER, Focus, RIGHT, SCENE_TABS};

const STEP_TIMEOUT: Duration = Duration::from_secs(3);
const LAYOUT_TIMEOUT: Duration = Duration::from_secs(5);
const SWITCH_TIMEOUT: Duration = Duration::from_secs(30);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(20);
const WARMUP: usize = 10;
const SWITCHES: usize = 100;
/// Бюджет реакции на клавишу в `Navigation` из `docs/architecture.md`.
const BUDGET_MS: f64 = 16.0;
const TOLERANCE: f64 = 0.5;
const PIXEL_TOLERANCE: u8 = 1;
/// Видимая часть вкладки у края, по которой ещё можно щёлкнуть.
const MIN_CLICK_WIDTH: f64 = 6.0;
const NAMES: [&str; 3] = ["center", "right", "bottom"];

const SHIFT: NSEventModifierFlags = NSEventModifierFlags::Shift;
const NONE: NSEventModifierFlags = NSEventModifierFlags::empty();
/// Клавиши вкладок прототипа. `charactersIgnoringModifiers` сохраняет Shift.
const KEY_L: Key = Key {
    label: "L",
    code: 37,
    characters: "L",
    ignoring: "L",
    flags: SHIFT,
};
const KEY_H: Key = Key {
    label: "H",
    code: 4,
    characters: "H",
    ignoring: "H",
    flags: SHIFT,
};
const KEY_1: Key = Key {
    label: "1",
    code: 18,
    characters: "1",
    ignoring: "1",
    flags: NONE,
};
const KEY_9: Key = Key {
    label: "9",
    code: 25,
    characters: "9",
    ignoring: "9",
    flags: NONE,
};

#[derive(Clone, Debug, Serialize)]
struct ButtonRead {
    name: &'static str,
    frame: Rect,
    /// Расстояние от начала кнопки до правого края полосы.
    from_right: f64,
    hidden: bool,
    enabled: bool,
    tooltip: Option<String>,
    /// Класс элемента доступности кнопки: сама кнопка или её ячейка.
    ax_class: Option<String>,
    ax_label: Option<String>,
    ax_help: Option<String>,
}

/// Состояние полосы, прочитанное у AppKit. Кадры вкладок и видимая часть — в точках ряда,
/// остальное — в точках полосы.
#[derive(Clone, Debug, Serialize)]
struct StripRead {
    width: f64,
    height: f64,
    tabs: Vec<Rect>,
    visible: Rect,
    document_width: f64,
    viewport: Rect,
    meter_hidden: bool,
    meter_frame: Rect,
    meter_text: String,
    meter_fitting: f64,
    header: Option<(String, bool)>,
    buttons: Vec<ButtonRead>,
    line_hidden: bool,
    line_frame: Rect,
    line_color: Option<Rgba>,
    /// Вкладки с фоном активной вкладки.
    active_layers: Vec<usize>,
    /// У вкладки виден значок с картинкой.
    icons: Vec<bool>,
}

impl StripRead {
    fn scrolling(&self) -> bool {
        self.document_width > self.visible.width + TOLERANCE
    }

    fn fully_visible(&self, index: usize) -> bool {
        self.tabs.get(index).is_some_and(|tab| {
            tab.x + TOLERANCE >= self.visible.x
                && tab.x + tab.width <= self.visible.x + self.visible.width + TOLERANCE
        })
    }

    fn visible_part(&self, index: usize) -> f64 {
        self.tabs.get(index).map_or(0.0, |tab| {
            let left = tab.x.max(self.visible.x);
            let right = (tab.x + tab.width).min(self.visible.x + self.visible.width);
            (right - left).max(0.0)
        })
    }
}

#[derive(Default)]
struct Switches {
    numbers: Vec<u64>,
    expected: usize,
    done: usize,
    post_to_state: Vec<f64>,
    post_to_dispatch: Vec<f64>,
    post_to_pass: Vec<f64>,
    dispatch_to_return: Vec<f64>,
    mismatches: Vec<String>,
    error: Option<String>,
}

struct Run {
    stand: Rc<Stand>,
    scene: Rc<Scene>,
    settle: Rc<Settle>,
    report: RefCell<Report>,
    mark: Cell<Option<Duration>>,
    stable: RefCell<Stable>,
    keys: RefCell<Vec<u64>>,
    click: RefCell<Option<(u64, u64)>>,
    before: Cell<[Option<usize>; 3]>,
    keys_before: Cell<[usize; 3]>,
    initial_frame: Cell<NSRect>,
    middle_frame: Cell<Option<NSRect>>,
    /// Вкладка, которую должен выбрать следующий шаг.
    target: Cell<Option<usize>>,
    off_edge: RefCell<Vec<usize>>,
    regimes: RefCell<Vec<(String, Regime)>>,
    buttons: RefCell<Vec<(String, Vec<f64>)>>,
    switches: RefCell<Switches>,
}

fn verdict(ok: bool) -> Verdict {
    if ok { Verdict::Pass } else { Verdict::Fail }
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= TOLERANCE
}

fn describe(rect: Rect) -> String {
    format!("{},{} {}x{}", rect.x, rect.y, rect.width, rect.height)
}

impl Run {
    fn mtm(&self) -> MainThreadMarker {
        self.stand.app.mtm()
    }

    fn window(&self) -> &NSWindow {
        &self.scene.window
    }

    fn strip(&self, index: usize) -> &TabStrip {
        self.scene.tiles.tiles[index].strip()
    }

    fn state(&self) -> String {
        let actives: Vec<Option<usize>> = (0..3).map(|i| self.layer_active(i)).collect();
        format!(
            "frame={} focus={:?} layer_actives={actives:?} right_collapsed={} {}",
            events::describe_rect(self.window().frame()),
            self.scene.tiles.focus(),
            self.scene.split.right.isCollapsed(),
            events::focus_state(&self.stand.app)
        )
    }

    fn touch(&self) {
        self.mark.set(events::elapsed());
        *self.stable.borrow_mut() = Stable::new();
    }

    fn passed(&self) -> bool {
        self.mark
            .get()
            .is_some_and(|mark| events::display_pass_after(mark).is_some())
    }

    fn watched_frames(&self) -> Vec<Rect> {
        let mut frames = vec![readback::rect(self.window().frame())];
        for index in 0..3 {
            let strip = self.strip(index);
            frames.push(readback::frame_in_window(strip));
            frames.push(readback::rect(strip.scroll().frame()));
            frames.push(readback::rect(strip.scroll().documentVisibleRect()));
        }
        frames
    }

    /// Проход отображения после изменения и неподвижные кадры.
    fn settled(&self) -> bool {
        let frames = self.watched_frames();
        self.passed() && self.stable.borrow_mut().quiet(frames)
    }

    fn press(&self, key: Key) -> Result<(), String> {
        self.capture_before();
        let numbers = events::press(key)?;
        *self.keys.borrow_mut() = numbers;
        Ok(())
    }

    /// Все события клавиши вернулись из `sendEvent`, после последнего прошёл проход отображения.
    fn keys_done(&self) -> bool {
        let numbers = self.keys.borrow();
        let returns: Option<Vec<Duration>> = numbers
            .iter()
            .map(|number| events::record(*number).and_then(|record| record.send_return))
            .collect();
        returns
            .and_then(|returns| returns.into_iter().max())
            .is_some_and(|last| events::display_pass_after(last).is_some())
    }

    fn capture_before(&self) {
        self.before
            .set([0, 1, 2].map(|index| self.layer_active(index)));
        self.keys_before
            .set([0, 1, 2].map(|index| self.strip(index).keys_handled()));
    }

    fn screen_point(&self, view: &NSView, point: NSPoint) -> NSPoint {
        let in_window = view.convertPoint_toView(point, None);
        self.window().convertPointToScreen(in_window)
    }

    fn click_at(&self, point: NSPoint, label: &str) -> Result<(), String> {
        self.capture_before();
        let gesture = events::mouse_down(self.mtm(), point, self.window(), label)?;
        let up = gesture.up(label)?;
        *self.click.borrow_mut() = Some((gesture.down, up));
        Ok(())
    }

    fn click_view(&self, view: &NSView, label: &str) -> Result<(), String> {
        let point = self.screen_point(view, events::center(view.bounds()));
        self.click_at(point, label)
    }

    /// Щелчок по середине видимой части вкладки.
    fn click_tab(&self, strip: usize, index: usize, label: &str) -> Result<(), String> {
        let strip = self.strip(strip);
        let views = strip.tab_views();
        let tab = views
            .get(index)
            .ok_or_else(|| format!("нет вкладки {index}"))?;
        let frame = tab.frame();
        let visible = strip.scroll().documentVisibleRect();
        let left = frame.origin.x.max(visible.origin.x);
        let right = (frame.origin.x + frame.size.width).min(visible.origin.x + visible.size.width);
        if right - left < MIN_CLICK_WIDTH {
            return Err(format!(
                "у вкладки {index} видно {:.1} точки, щёлкнуть нельзя",
                right - left
            ));
        }
        let in_row = NSPoint::new(
            (left + right) / 2.0,
            frame.origin.y + frame.size.height / 2.0,
        );
        let point = self.screen_point(strip.row(), in_row);
        self.click_at(point, label)
    }

    fn click_done(&self) -> bool {
        self.click
            .borrow()
            .and_then(|(down, _)| events::record(down))
            .and_then(|record| record.send_return)
            .is_some_and(|back| events::display_pass_after(back).is_some())
    }

    /// Вкладка с фоном активной вкладки по слою, первая найденная.
    fn layer_active(&self, strip: usize) -> Option<usize> {
        let panel = self.scene.tokens.panel;
        self.strip(strip)
            .tab_views()
            .iter()
            .position(|tab| tab_background(tab) == Some(panel))
    }

    fn read_strip(&self, index: usize) -> StripRead {
        let strip = self.strip(index);
        let tokens = &self.scene.tokens;
        let size = strip.bounds().size;
        let tabs: Vec<Rect> = strip
            .tab_views()
            .iter()
            .map(|tab| readback::rect(tab.frame()))
            .collect();
        let active_layers = strip
            .tab_views()
            .iter()
            .enumerate()
            .filter(|(_, tab)| tab_background(tab) == Some(tokens.panel))
            .map(|(index, _)| index)
            .collect();
        let icons = strip
            .tab_views()
            .iter()
            .map(|tab| {
                tab.icon().is_some_and(|icon| {
                    !icon.isHiddenOrHasHiddenAncestor() && icon.image().is_some()
                })
            })
            .collect();
        let meter = strip.meter();
        let header = strip
            .menu()
            .itemAtIndex(0)
            .map(|item| (item.title().to_string(), item.isHidden()));
        let names = ["split", "zoom", "more"];
        let buttons = strip
            .buttons()
            .into_iter()
            .zip(names)
            .map(|(button, name)| {
                let frame = readback::rect(button.frame());
                let tree = readback::accessibility_tree(button, 2);
                let element = tree.find_role("AXButton");
                ButtonRead {
                    name,
                    frame,
                    from_right: size.width - frame.x,
                    hidden: button.isHiddenOrHasHiddenAncestor(),
                    enabled: button.isEnabled(),
                    tooltip: button.toolTip().map(|tip| tip.to_string()),
                    ax_class: element.map(|element| element.class.clone()),
                    ax_label: element.and_then(|element| element.label.clone()),
                    ax_help: element.and_then(|element| element.help.clone()),
                }
            })
            .collect();
        let line = strip.line();
        StripRead {
            width: size.width,
            height: size.height,
            tabs,
            visible: readback::rect(strip.scroll().documentVisibleRect()),
            document_width: strip.row().frame().size.width,
            viewport: readback::rect(strip.scroll().frame()),
            meter_hidden: meter.isHiddenOrHasHiddenAncestor(),
            meter_frame: readback::rect(meter.frame()),
            meter_text: meter.stringValue().to_string(),
            meter_fitting: meter.intrinsicContentSize().width.ceil(),
            header,
            buttons,
            line_hidden: line.isHiddenOrHasHiddenAncestor(),
            line_frame: readback::rect(line.frame()),
            line_color: line
                .layer()
                .and_then(|layer| readback::layer_state(&layer).background),
            active_layers,
            icons,
        }
    }

    /// Раскладка полосы по ADR-0032: режим по ширинам, прочитанным у AppKit, кнопки на месте,
    /// нагрузка целиком или в заголовке меню, прокрутка не заходит на нагрузку и кнопки.
    fn check_layout(&self, index: usize, state: &str) -> StripRead {
        let strip = self.strip(index);
        let read = self.read_strip(index);
        let tokens = &self.scene.tokens;
        let widths: Vec<f64> = read.tabs.iter().map(|tab| tab.width).collect();
        let meter = strip.active_meter();
        let meter_width = meter.as_ref().map(|_| read.meter_fitting + 2.0 * METER_PAD);
        let want = tabs::layout(read.width, &widths, meter_width);
        let observed = match (read.meter_hidden, read.scrolling(), meter.is_some()) {
            (_, true, _) => Regime::Scroll,
            (true, false, true) => Regime::MeterInMenu,
            _ => Regime::Fit,
        };
        let mut problems = Vec::new();
        if !near(read.height, tokens.tabbar) {
            problems.push(format!("высота полосы {}", read.height));
        }
        if observed != want.regime {
            problems.push(format!(
                "режим {observed:?}, по ширинам ожидался {:?}",
                want.regime
            ));
        }
        if !read.meter_hidden && read.scrolling() {
            problems.push("нагрузка видна при прокрутке вкладок".to_owned());
        }
        let split_x = read.buttons.first().map_or(read.width, |b| b.frame.x);
        let mut right_limit = split_x - GAP;
        if !read.meter_hidden {
            if read.meter_frame.width + TOLERANCE < read.meter_fitting {
                problems.push(format!(
                    "нагрузка обрезана: {} из {}",
                    read.meter_frame.width, read.meter_fitting
                ));
            }
            if read.meter_frame.x + read.meter_frame.width + METER_PAD > split_x - GAP + TOLERANCE {
                problems.push("нагрузка заходит на кнопки".to_owned());
            }
            right_limit = read.meter_frame.x - METER_PAD - GAP;
        }
        if let Some(meter) = &meter {
            match &read.header {
                Some((title, false)) if title == meter.text() => {}
                other => problems.push(format!(
                    "заголовок меню {other:?}, ожидалась нагрузка «{}»",
                    meter.text()
                )),
            }
        }
        if read.viewport.x + read.viewport.width > right_limit + ROW_INSET + TOLERANCE {
            problems.push(format!(
                "прокрутка {} заходит за {right_limit}",
                describe(read.viewport)
            ));
        }
        for (x, tab) in want.tabs.iter().zip(&read.tabs) {
            if !near(*x, tab.x) || !near(tab.height, tabs::TAB_HEIGHT) {
                problems.push(format!("вкладка {}: ожидалось x={x}", describe(*tab)));
                break;
            }
        }
        let (split_label, split_key, _) = strip.place_kind().split_action();
        let (zoom_label, zoom_key) = ZOOM_ACTION;
        let expected = [
            (Some(split_label), Some(split_key), false),
            (Some(zoom_label), Some(zoom_key), false),
            (Some(MENU_LABEL), None, true),
        ];
        for ((button, x), (label, key, enabled)) in
            read.buttons.iter().zip(want.buttons).zip(expected)
        {
            let tip = match key {
                Some(key) => label.map(|label| format!("{label}  {key}")),
                None => label.map(str::to_owned),
            };
            let ok = !button.hidden
                && near(button.frame.x, x)
                && near(button.frame.width, BUTTON)
                && near(button.frame.height, BUTTON)
                && button.frame.y >= -TOLERANCE
                && button.frame.y + button.frame.height <= read.height + TOLERANCE
                && button.enabled == enabled
                && button.tooltip == tip
                && button.ax_label.as_deref() == label
                && button.ax_help.as_deref() == key;
            if !ok {
                problems.push(format!(
                    "кнопка {}: {button:?}, ожидалось x={x}",
                    button.name
                ));
            }
        }
        let has_icons = strip.place_kind().has_icons();
        if read.icons.iter().any(|icon| *icon != has_icons) {
            problems.push(format!(
                "значки типа {:?}, ожидалось {}",
                read.icons,
                if has_icons {
                    "у всех"
                } else {
                    "ни у одной"
                }
            ));
        }
        let name = NAMES[index];
        self.regimes
            .borrow_mut()
            .push((format!("{state}/{name}"), observed));
        self.buttons.borrow_mut().push((
            format!("{state}/{name}"),
            read.buttons
                .iter()
                .map(|button| button.from_right)
                .collect(),
        ));
        let ok = problems.is_empty();
        let mut report = self.report.borrow_mut();
        report.push_json(&format!("strip_{state}_{name}"), &read);
        report.finding(
            &format!("layout_{state}_{name}"),
            verdict(ok),
            if ok {
                format!(
                    "ширина {}, вкладок {}, режим {observed:?}: кнопки у правого края, нагрузка {}, ряд {} при видимых {}",
                    read.width,
                    read.tabs.len(),
                    if read.meter_hidden {
                        "в заголовке меню «…»"
                    } else {
                        "видна целиком"
                    },
                    read.document_width,
                    read.visible.width
                )
            } else {
                problems.join("; ")
            },
        );
        read
    }

    /// Выбор по слоям, доступности и линии под вкладкой; выбранная вкладка видна целиком.
    fn check_selection(&self, index: usize, state: &str, want: Option<usize>) -> bool {
        let strip = self.strip(index);
        let read = self.read_strip(index);
        let tokens = &self.scene.tokens;
        let mut problems = Vec::new();
        let want_layers: Vec<usize> = want.into_iter().collect();
        if read.active_layers != want_layers {
            problems.push(format!(
                "фон активной вкладки у {:?}, ожидалось {want_layers:?}",
                read.active_layers
            ));
        }
        let elements: Vec<readback::AxElement> = readback::accessibility_children(strip.row())
            .iter()
            .map(|child| readback::accessibility(child))
            .collect();
        let selected: Vec<usize> = elements
            .iter()
            .enumerate()
            .filter(|(_, element)| element.selected == Some(true))
            .map(|(index, _)| index)
            .collect();
        let valued: Vec<usize> = elements
            .iter()
            .enumerate()
            .filter(|(_, element)| element.value.as_deref() == Some("1"))
            .map(|(index, _)| index)
            .collect();
        if selected != want_layers || valued != want_layers {
            problems.push(format!(
                "доступность: выбраны {selected:?}, значение 1 у {valued:?}"
            ));
        }
        let tile = &self.scene.tiles.tiles[index];
        let focused = events::is_first_responder(self.window(), tile);
        match want
            .and_then(|want| read.tabs.get(want))
            .filter(|_| focused)
        {
            Some(tab) => {
                let line = read.line_frame;
                let ok = !read.line_hidden
                    && read.line_color == Some(tokens.focus)
                    && near(line.height, tokens.focus_h)
                    && near(line.x, tab.x + LINE_INSET)
                    && near(line.width, tab.width - 2.0 * LINE_INSET)
                    && near(line.y + line.height, read.height - 1.0);
                if !ok {
                    problems.push(format!(
                        "линия под вкладкой: скрыта={} {} цвет {:?}",
                        read.line_hidden,
                        describe(line),
                        read.line_color.map(Rgba::hex)
                    ));
                }
            }
            None if !read.line_hidden => {
                problems.push("линия под вкладкой видна у плитки без фокуса".to_owned());
            }
            None => {}
        }
        if let Some(want) = want
            && !read.fully_visible(want)
        {
            problems.push(format!(
                "вкладка {want} видна не целиком: {:?} при видимой части {}",
                read.tabs.get(want).copied().map(describe),
                describe(read.visible)
            ));
        }
        let ok = problems.is_empty();
        let name = NAMES[index];
        self.report.borrow_mut().finding(
            &format!("select_{state}_{name}"),
            verdict(ok),
            if ok {
                format!(
                    "выбрана вкладка {want:?} по слою и доступности, видна целиком, линия {}",
                    if focused {
                        "под ней"
                    } else {
                        "скрыта: плитка без фокуса"
                    }
                )
            } else {
                problems.join("; ")
            },
        );
        ok
    }

    /// Группа вкладок в дереве доступности: все вкладки, и прокрученные за край, с ролью,
    /// подписью и нажатием, в порядке навигации.
    fn check_accessibility(&self, index: usize, state: &str) {
        let strip = self.strip(index);
        let row = strip.row();
        let read = self.read_strip(index);
        let titles = strip.titles();
        let mut problems = Vec::new();
        let group = readback::accessibility(row);
        if group.element != Some(true) || group.role.as_deref() != Some("AXTabGroup") {
            problems.push(format!(
                "группа: элемент {:?}, роль {:?}",
                group.element, group.role
            ));
        }
        let tile = &self.scene.tiles.tiles[index];
        let reachable = readback::accessibility_tree(tile, 8)
            .walk()
            .iter()
            .any(|element| {
                element.role.as_deref() == Some("AXTabGroup") && element.label == group.label
            });
        if !reachable {
            problems.push("группы вкладок нет в дереве доступности плитки".to_owned());
        }
        let label_of = |children: Vec<Retained<objc2::runtime::AnyObject>>| -> Vec<String> {
            children
                .iter()
                .map(|child| readback::accessibility(child).label.unwrap_or_default())
                .collect()
        };
        let children = readback::accessibility_children(row);
        let elements: Vec<readback::AxElement> = children
            .iter()
            .map(|child| readback::accessibility(child))
            .collect();
        let labels = label_of(children);
        if labels != titles {
            problems.push(format!("дети группы {labels:?}, ожидались {titles:?}"));
        }
        let navigation = label_of(readback::accessibility_navigation_children(row));
        if navigation != titles {
            problems.push(format!("порядок навигации {navigation:?}"));
        }
        for element in &elements {
            let ok = element.element == Some(true)
                && element.role.as_deref() == Some("AXRadioButton")
                && element.subrole.as_deref() == Some("AXTabButton")
                && element.actions.iter().any(|action| action == "AXPress");
            if !ok {
                problems.push(format!("вкладка {:?}: {element:?}", element.label));
                break;
            }
        }
        let off_edge: Vec<usize> = (0..read.tabs.len())
            .filter(|tab| read.visible_part(*tab) <= 0.0)
            .collect();
        let ok = problems.is_empty();
        let name = NAMES[index];
        let mut report = self.report.borrow_mut();
        report.push_json(&format!("ax_{state}_{name}"), &elements);
        report.finding(
            &format!("accessibility_{state}_{name}"),
            verdict(ok),
            if ok {
                format!(
                    "группа AXTabGroup «{}» в дереве плитки; {} детей AXRadioButton/AXTabButton с AXPress в порядке вкладок, и в порядке навигации; за краем {} из них",
                    group.label.unwrap_or_default(),
                    elements.len(),
                    off_edge.len()
                )
            } else {
                problems.join("; ")
            },
        );
    }

    /// Живых видов вкладок столько, сколько вкладок в рядах.
    fn check_live_tabs(&self, state: &str) {
        let shown: usize = (0..3)
            .map(|index| self.strip(index).row().tab_views().len())
            .sum();
        let live = counters::snapshot()
            .into_iter()
            .find(|count| count.name == "ProbeTab")
            .map_or(0, |count| count.live());
        self.report.borrow_mut().finding(
            &format!("live_tabs_{state}"),
            verdict(live == shown),
            format!("живых ProbeTab {live}, вкладок в рядах {shown}"),
        );
    }

    /// Изменилась ли активная вкладка полос с прошлого `capture_before`.
    fn changed(&self) -> [bool; 3] {
        let before = self.before.get();
        [0, 1, 2].map(|index| self.layer_active(index) != before[index])
    }

    fn check_key_target(&self, state: &str, strip: usize, want: usize) {
        let changed = self.changed();
        let others_still = (0..3).filter(|i| *i != strip).all(|i| !changed[i]);
        self.check_selection(strip, state, Some(want));
        self.report.borrow_mut().finding(
            &format!("key_target_{state}"),
            verdict(others_still),
            format!(
                "клавиша изменила полосы {:?}; ожидалась только {}",
                changed_names(changed),
                NAMES[strip]
            ),
        );
    }

    /// Кадр окна для среднего режима: ширина центральной полосы посередине между шириной без
    /// нагрузки и шириной с нагрузкой.
    fn middle_frame(&self) -> Result<NSRect, String> {
        let strip = self.strip(CENTER);
        let read = self.read_strip(CENTER);
        let widths: Vec<f64> = read.tabs.iter().map(|tab| tab.width).collect();
        let meter = strip
            .active_meter()
            .map(|_| read.meter_fitting + 2.0 * METER_PAD)
            .ok_or("у активной вкладки нет нагрузки")?;
        let layout = tabs::layout(read.width, &widths, Some(meter));
        let target = ((layout.no_meter_width + layout.fit_width) / 2.0).floor();
        let delta = target - read.width;
        let frame = self.window().frame();
        let screen = self
            .window()
            .screen()
            .ok_or("окно не на экране")?
            .visibleFrame();
        let width = frame.size.width + delta;
        if delta <= 0.0 || width > screen.size.width {
            return Err(format!(
                "полосе нужна ширина {target} при {} (окно {width}, экран {})",
                read.width, screen.size.width
            ));
        }
        let x = frame
            .origin
            .x
            .min(screen.origin.x + screen.size.width - width)
            .max(screen.origin.x);
        Ok(NSRect::new(
            NSPoint::new(x, frame.origin.y),
            NSSize::new(width, frame.size.height),
        ))
    }

    /// Пиксели рендера: линия под активной вкладкой и фон активной вкладки против токенов, фон
    /// соседней вкладки против фона полосы над ней: у неактивной вкладки своего фона нет.
    fn probe_pixels(&self) -> Result<(), String> {
        let root = &self.scene.root;
        let render = ViewRender::of(root)?;
        let tokens = &self.scene.tokens;
        let strip = self.strip(CENTER);
        let views = strip.tab_views();
        let active = strip.active().ok_or("нет активной вкладки")?;
        let tab = views.get(active).ok_or("нет вида активной вкладки")?;
        let neighbour = views
            .get(if active == 0 { 1 } else { active - 1 })
            .ok_or("нет соседней вкладки")?;
        let tone = tokens.tone();
        let line = strip.line();
        let probes = [
            (
                "линия",
                line.convertPoint_toView(events::center(line.bounds()), Some(root)),
                tone.over(tokens.focus),
            ),
            (
                "активная вкладка",
                tab.convertPoint_toView(NSPoint::new(4.0, 13.0), Some(root)),
                tone.over(tokens.panel),
            ),
        ];
        let strip_above =
            render.pixel(neighbour.convertPoint_toView(NSPoint::new(4.0, -2.0), Some(root)))?;
        let mut ok = true;
        let mut lines = Vec::new();
        for (label, point, want) in probes {
            let got = render.pixel(point)?;
            ok &= got.distance(want) <= PIXEL_TOLERANCE;
            lines.push(format!("{label} {} (ожидался {})", got.hex(), want.hex()));
        }
        let got =
            render.pixel(neighbour.convertPoint_toView(NSPoint::new(4.0, 13.0), Some(root)))?;
        ok &= got.distance(strip_above) <= PIXEL_TOLERANCE;
        lines.push(format!(
            "соседняя вкладка {} (фон полосы над ней {}, расчёт {})",
            got.hex(),
            strip_above.hex(),
            tone.over(tokens.panel_head).hex()
        ));
        self.report.borrow_mut().finding(
            "tabs_pixels",
            verdict(ok),
            format!("рендер содержимого, плитка с фокусом: {}", lines.join(", ")),
        );
        Ok(())
    }

    /// Переключения клавишей `L` по одному: следующее событие — после возврата всех событий
    /// клавиши, прохода отображения и нужного фона вкладки. Прогревочные в замер не входят.
    fn drive_switches(&self) -> Poll<()> {
        let mut switches = self.switches.borrow_mut();
        if switches.error.is_some() {
            return Poll::Ready(());
        }
        if !switches.numbers.is_empty() {
            let records: Option<Vec<events::Record>> = switches
                .numbers
                .iter()
                .map(|number| events::record(*number))
                .collect();
            let Some(records) = records else {
                switches.error = Some("нет записи события переключения".to_owned());
                return Poll::Ready(());
            };
            let returns: Option<Vec<Duration>> =
                records.iter().map(|record| record.send_return).collect();
            let Some(last) = returns.and_then(|returns| returns.into_iter().max()) else {
                return Poll::Pending(format!(
                    "переключение {}: события не вернулись",
                    switches.done
                ));
            };
            if events::display_pass_after(last).is_none() {
                return Poll::Pending(format!("переключение {}: нет прохода", switches.done));
            }
            let key_down = &records[KEY_L.modifiers().len()];
            let active = self.layer_active(CENTER);
            if active != Some(switches.expected) {
                let done = switches.done;
                let expected = switches.expected;
                switches
                    .mismatches
                    .push(format!("{done}: фон у {active:?}, ожидалась {expected}"));
            }
            if switches.done >= WARMUP
                && let (Some(start), Some(back)) = (key_down.dispatch_start, key_down.send_return)
            {
                let pass = events::display_pass_after(back);
                switches
                    .post_to_state
                    .push(events::to_ms(back.saturating_sub(key_down.posted)));
                switches
                    .post_to_dispatch
                    .push(events::to_ms(start.saturating_sub(key_down.posted)));
                switches
                    .dispatch_to_return
                    .push(events::to_ms(back.saturating_sub(start)));
                if let Some(pass) = pass {
                    switches
                        .post_to_pass
                        .push(events::to_ms(pass.saturating_sub(key_down.posted)));
                }
            }
            switches.done += 1;
            switches.numbers.clear();
        }
        if switches.done >= WARMUP + SWITCHES {
            return Poll::Ready(());
        }
        let len = self.strip(CENTER).len();
        let current = self.layer_active(CENTER).unwrap_or(0);
        switches.expected = (current + 1) % len.max(1);
        match events::press(KEY_L) {
            Ok(numbers) => switches.numbers = numbers,
            Err(reason) => {
                switches.error = Some(reason);
                return Poll::Ready(());
            }
        }
        Poll::Pending(format!("переключение {} отправлено", switches.done))
    }
}

fn tab_background(view: &NSView) -> Option<Rgba> {
    view.layer()
        .and_then(|layer| readback::layer_state(&layer).background)
}

fn changed_names(changed: [bool; 3]) -> Vec<&'static str> {
    NAMES
        .iter()
        .zip(changed)
        .filter(|(_, changed)| *changed)
        .map(|(name, _)| *name)
        .collect()
}

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Tabs);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let Some(scene) = stand.built_scene.clone() else {
        return stand.conclude(report, Err("сцена не собрана".to_owned()));
    };
    report.push("cell", scene.cell.name());
    report.push_json("tab_counts", &TAB_COUNTS);
    report.push_json("scene_tabs", &SCENE_TABS);
    report.push(
        "interval_post_to_state",
        "postEvent → наблюдаемое состояние AppKit: смена синхронная, фон вкладки читается после возврата sendEvent, поэтому конец интервала — возврат sendEvent",
    );
    report.push(
        "interval_post_to_display_pass",
        "postEvent → конец прохода отображения после возврата sendEvent; это не момент кадра на экране",
    );
    report.push(
        "interval_dispatch_to_return",
        "начало dispatch → возврат sendEvent",
    );
    report.push(
        "interval_post_to_dispatch",
        "postEvent → начало dispatch: ожидание в очереди событий, отдельно для разбора",
    );
    let run = Rc::new(Run {
        settle: Settle::new(vec![scene.window.clone()]),
        initial_frame: Cell::new(scene.window.frame()),
        stand: Rc::clone(&stand),
        scene,
        report: RefCell::new(report),
        mark: Cell::new(None),
        stable: RefCell::new(Stable::new()),
        keys: RefCell::new(Vec::new()),
        click: RefCell::new(None),
        before: Cell::new([None; 3]),
        keys_before: Cell::new([0; 3]),
        middle_frame: Cell::new(None),
        target: Cell::new(None),
        off_edge: RefCell::new(Vec::new()),
        regimes: RefCell::new(Vec::new()),
        buttons: RefCell::new(Vec::new()),
        switches: RefCell::new(Switches::default()),
    });
    run_steps(steps(&run), move |result| finish(run, result));
}

fn step(
    run: &Rc<Run>,
    name: &'static str,
    timeout: Duration,
    ready: impl Fn(&Run) -> bool + 'static,
    act: impl FnOnce(&Run) -> Result<(), String> + 'static,
) -> Step {
    let (for_ready, for_act) = (Rc::clone(run), Rc::clone(run));
    Step::new(
        name,
        timeout,
        move || poll(ready(&for_ready), || for_ready.state()),
        move || act(&for_act),
    )
}

/// Набор вкладок `count` в центральной и правой полосах.
fn set_fixture(run: &Run, count: usize) {
    run.strip(CENTER).set_tabs(fixtures::tabs(count));
    run.strip(RIGHT).set_tabs(fixtures::tabs(count));
    run.touch();
}

fn check_fixture(run: &Run, count: usize) {
    let state = format!("n{count}");
    for index in [CENTER, RIGHT] {
        run.check_layout(index, &state);
        run.check_selection(index, &state, Some(0));
        run.check_accessibility(index, &state);
    }
    run.check_live_tabs(&state);
}

fn scroll_to(strip: &TabStrip, end: bool) {
    let scroll = strip.scroll();
    let clip = scroll.contentView();
    let bounds = clip.bounds();
    let x = if end {
        (strip.row().frame().size.width - bounds.size.width).max(0.0)
    } else {
        0.0
    };
    clip.scrollToPoint(NSPoint::new(x, bounds.origin.y));
    scroll.reflectScrolledClipView(&clip);
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
                app.isActive()
                    && key.is_some_and(|key| events::same(&*key, run.window()))
                    && events::is_first_responder(run.window(), &run.scene.tiles.tiles[CENTER]),
                || run.state(),
            )
        }
    };
    let warp = {
        let run = Rc::clone(run);
        move || {
            run.initial_frame.set(run.window().frame());
            events::warp_cursor_outside(run.mtm(), &run.stand.app).map(|_| ())
        }
    };
    let mut steps = vec![
        Step::new(
            "окно неподвижно, активность, key window и фокус на центральной плитке",
            SETTLE_TIMEOUT,
            first,
            warp,
        ),
        step(
            run,
            "курсор вне окон",
            STEP_TIMEOUT,
            |run| events::cursor_outside(&run.stand.app),
            |run| {
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "сцена установилась",
            LAYOUT_TIMEOUT,
            Run::settled,
            |run| {
                for index in [CENTER, RIGHT, BOTTOM] {
                    run.check_layout(index, "scene");
                    run.check_selection(index, "scene", Some(0));
                    run.check_accessibility(index, "scene");
                }
                run.check_live_tabs("scene");
                set_fixture(run, TAB_COUNTS[0]);
                Ok(())
            },
        ),
    ];
    for (position, count) in TAB_COUNTS.into_iter().enumerate() {
        let next = TAB_COUNTS.get(position + 1).copied();
        steps.push(step(
            run,
            "набор вкладок установился",
            LAYOUT_TIMEOUT,
            Run::settled,
            move |run| {
                check_fixture(run, count);
                if let Some(next) = next {
                    set_fixture(run, next);
                }
                Ok(())
            },
        ));
    }
    steps.extend([
        step(
            run,
            "30 вкладок справа, 8 в центре",
            STEP_TIMEOUT,
            |_| true,
            |run| {
                run.strip(CENTER).set_tabs(fixtures::tabs(8));
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "8 вкладок в центре установились",
            LAYOUT_TIMEOUT,
            Run::settled,
            |run| {
                match run.middle_frame() {
                    Ok(frame) => {
                        run.middle_frame.set(Some(frame));
                        run.window().setFrame_display(frame, true);
                    }
                    Err(reason) => {
                        run.report.borrow_mut().finding(
                            "regime_meter_in_menu",
                            Verdict::PendingManual,
                            format!("средний режим не достижим на этом экране: {reason}"),
                        );
                    }
                }
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "окно шире для среднего режима",
            LAYOUT_TIMEOUT,
            Run::settled,
            |run| {
                if run.middle_frame.get().is_some() {
                    let read = run.check_layout(CENTER, "middle");
                    run.check_selection(CENTER, "middle", Some(0));
                    let regime = match (read.meter_hidden, read.scrolling()) {
                        (true, false) => Regime::MeterInMenu,
                        (_, true) => Regime::Scroll,
                        (false, false) => Regime::Fit,
                    };
                    run.report.borrow_mut().finding(
                        "regime_meter_in_menu",
                        verdict(regime == Regime::MeterInMenu),
                        format!(
                            "полоса {} точек с 8 вкладками: режим {regime:?}, нагрузка в заголовке меню {:?}",
                            read.width, read.header
                        ),
                    );
                }
                run.window().setFrame_display(run.initial_frame.get(), true);
                run.strip(CENTER).set_tabs(fixtures::tabs(30));
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "окно вернулось к минимуму, 30 вкладок в центре",
            LAYOUT_TIMEOUT,
            |run| {
                run.settled()
                    && run.window().frame() == run.initial_frame.get()
            },
            |run| {
                run.check_layout(CENTER, "restored");
                run.check_selection(CENTER, "restored", Some(0));
                run.press(KEY_L)
            },
        ),
        step(
            run,
            "L в центре обработана",
            STEP_TIMEOUT,
            Run::keys_done,
            |run| {
                run.check_key_target("center-L", CENTER, 1);
                run.press(KEY_9)
            },
        ),
        step(
            run,
            "9 в центре обработана",
            STEP_TIMEOUT,
            Run::keys_done,
            |run| {
                run.check_key_target("center-9", CENTER, 8);
                run.press(KEY_H)
            },
        ),
        step(
            run,
            "H в центре обработана",
            STEP_TIMEOUT,
            Run::keys_done,
            |run| {
                run.check_key_target("center-H", CENTER, 7);
                run.press(KEY_1)
            },
        ),
        step(
            run,
            "1 в центре обработана",
            STEP_TIMEOUT,
            Run::keys_done,
            |run| {
                run.check_key_target("center-1", CENTER, 0);
                run.press(KEY_H)
            },
        ),
        step(
            run,
            "H с первой вкладки обработана",
            STEP_TIMEOUT,
            Run::keys_done,
            |run| {
                let last = run.strip(CENTER).len().saturating_sub(1);
                run.check_key_target("center-H-wrap", CENTER, last);
                scroll_to(run.strip(CENTER), false);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "ряд центра прокручен к началу",
            STEP_TIMEOUT,
            Run::passed,
            |run| {
                let read = run.read_strip(CENTER);
                let edge = (0..read.tabs.len())
                    .rev()
                    .find(|tab| {
                        read.visible_part(*tab) >= MIN_CLICK_WIDTH && !read.fully_visible(*tab)
                    })
                    .ok_or("у края ряда нет частично видимой вкладки")?;
                run.report.borrow_mut().push(
                    "edge_tab",
                    format!(
                        "вкладка {edge}: видно {:.1} из {} точек",
                        read.visible_part(edge),
                        read.tabs[edge].width
                    ),
                );
                run.target.set(Some(edge));
                run.click_tab(CENTER, edge, "вкладка у края")
            },
        ),
        step(
            run,
            "щелчок по вкладке у края обработан",
            STEP_TIMEOUT,
            Run::click_done,
            |run| {
                let edge = run.target.get().ok_or("нет вкладки у края")?;
                run.check_selection(CENTER, "click-edge", Some(edge));
                let read = run.read_strip(CENTER);
                *run.off_edge.borrow_mut() = (0..read.tabs.len())
                    .filter(|tab| read.visible_part(*tab) <= 0.0)
                    .collect();
                scroll_to(run.strip(CENTER), true);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "ряд центра прокручен к концу",
            STEP_TIMEOUT,
            Run::passed,
            |run| {
                let read = run.read_strip(CENTER);
                let active = run.layer_active(CENTER);
                let target = run
                    .off_edge
                    .borrow()
                    .iter()
                    .copied()
                    .find(|tab| read.fully_visible(*tab) && Some(*tab) != active)
                    .ok_or("после прокрутки не видно вкладок, бывших за краем")?;
                run.report
                    .borrow_mut()
                    .push("scrolled_click_tab", format!("вкладка {target}"));
                run.target.set(Some(target));
                run.click_tab(CENTER, target, "вкладка из-за края")
            },
        ),
        step(
            run,
            "щелчок по вкладке из-за края обработан",
            STEP_TIMEOUT,
            Run::click_done,
            |run| {
                let target = run.target.get().ok_or("нет вкладки из-за края")?;
                run.check_selection(CENTER, "click-scrolled", Some(target));
                run.click_tab(RIGHT, 1, "вторая вкладка справа")
            },
        ),
        step(
            run,
            "щелчок по вкладке справа обработан",
            STEP_TIMEOUT,
            Run::click_done,
            |run| {
                let changed = run.changed();
                let focus = run.scene.tiles.focus();
                let responder = events::is_first_responder(
                    run.window(),
                    &run.scene.tiles.tiles[RIGHT],
                );
                run.report.borrow_mut().finding(
                    "click_focuses_tile",
                    verdict(responder && focus == Focus::Tile(RIGHT) && !changed[CENTER]),
                    format!(
                        "first responder — правая плитка: {responder}, фокус сцены {focus:?}, изменились {:?}",
                        changed_names(changed)
                    ),
                );
                run.check_selection(RIGHT, "click-right", Some(1));
                let center = run.layer_active(CENTER);
                run.check_selection(CENTER, "click-right", center);
                run.press(KEY_L)
            },
        ),
        step(
            run,
            "L справа обработана",
            STEP_TIMEOUT,
            Run::keys_done,
            |run| {
                run.check_key_target("right-L", RIGHT, 2);
                run.click_view(&run.scene.tiles.navigator, "навигатор")
            },
        ),
        step(
            run,
            "щелчок по навигатору обработан",
            STEP_TIMEOUT,
            Run::click_done,
            |run| {
                if run.scene.tiles.focus() != Focus::Navigator {
                    return Err(format!(
                        "фокус не перешёл в навигатор: {:?}",
                        run.scene.tiles.focus()
                    ));
                }
                run.press(KEY_L)
            },
        ),
        step(
            run,
            "L в навигаторе обработана",
            STEP_TIMEOUT,
            Run::keys_done,
            |run| {
                let changed = run.changed();
                run.report.borrow_mut().finding(
                    "key_target_navigator",
                    verdict(changed.iter().all(|changed| !changed)),
                    format!(
                        "при фокусе в навигаторе клавиша изменила полосы {:?}",
                        changed_names(changed)
                    ),
                );
                for index in [CENTER, RIGHT, BOTTOM] {
                    let active = run.layer_active(index);
                    run.check_selection(index, "navigator", active);
                }
                run.click_tab(RIGHT, 0, "первая вкладка справа")
            },
        ),
        step(
            run,
            "фокус снова справа",
            STEP_TIMEOUT,
            Run::click_done,
            |run| {
                if !events::is_first_responder(run.window(), &run.scene.tiles.tiles[RIGHT]) {
                    return Err("правая плитка не стала first responder".to_owned());
                }
                run.scene.split.right.setCollapsed(true);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "правая зона свёрнута",
            LAYOUT_TIMEOUT,
            |run| run.scene.split.right.isCollapsed() && run.settled(),
            |run| {
                let right = run.strip(RIGHT);
                let tile = &run.scene.tiles.tiles[RIGHT];
                run.report.borrow_mut().push(
                    "collapsed_right",
                    format!(
                        "first responder {} (правая плитка: {}), фокус сцены {:?}, полоса скрыта: {}, видимая часть {}",
                        events::responder_name(run.window()),
                        events::is_first_responder(run.window(), tile),
                        run.scene.tiles.focus(),
                        right.isHiddenOrHasHiddenAncestor(),
                        events::describe_rect(right.visibleRect())
                    ),
                );
                run.press(KEY_L)
            },
        ),
        step(
            run,
            "L при свёрнутой правой зоне обработана",
            STEP_TIMEOUT,
            Run::keys_done,
            |run| {
                let changed = run.changed();
                let keys = run.keys_before.get();
                let right_keys = run.strip(RIGHT).keys_handled() - keys[RIGHT];
                // Изменить полосу может только видимая плитка, ставшая first responder.
                let allowed: Vec<usize> = [CENTER, BOTTOM]
                    .into_iter()
                    .filter(|index| {
                        events::is_first_responder(run.window(), &run.scene.tiles.tiles[*index])
                    })
                    .collect();
                let ok = !changed[RIGHT]
                    && right_keys == 0
                    && (0..3).all(|index| !changed[index] || allowed.contains(&index));
                run.report.borrow_mut().finding(
                    "key_target_hidden",
                    verdict(ok),
                    format!(
                        "свёрнутая правая полоса приняла клавиш {right_keys}, изменились {:?}, first responder {}",
                        changed_names(changed),
                        events::responder_name(run.window())
                    ),
                );
                run.scene.split.right.setCollapsed(false);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "правая зона развёрнута",
            LAYOUT_TIMEOUT,
            |run| !run.scene.split.right.isCollapsed() && run.settled(),
            |run| {
                scroll_to(run.strip(CENTER), false);
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "ряд центра снова у начала",
            STEP_TIMEOUT,
            Run::passed,
            |run| {
                let read = run.read_strip(CENTER);
                let target = (0..read.tabs.len())
                    .find(|tab| read.visible_part(*tab) <= 0.0)
                    .ok_or("нет вкладки за краем")?;
                let views = run.strip(CENTER).tab_views();
                let tab = views.get(target).ok_or("нет вида вкладки")?;
                run.report
                    .borrow_mut()
                    .push("ax_press_tab", format!("вкладка {target} за краем"));
                run.target.set(Some(target));
                run.capture_before();
                // SAFETY: `accessibilityPerformPress` у элемента доступности без аргументов.
                let pressed: bool = unsafe { msg_send![&**tab, accessibilityPerformPress] };
                if !pressed {
                    return Err("accessibilityPerformPress вернул NO".to_owned());
                }
                run.touch();
                Ok(())
            },
        ),
        step(
            run,
            "нажатие доступности обработано",
            STEP_TIMEOUT,
            Run::settled,
            |run| {
                let target = run.target.get().ok_or("нет вкладки нажатия")?;
                let focused =
                    events::is_first_responder(run.window(), &run.scene.tiles.tiles[CENTER]);
                run.report.borrow_mut().finding(
                    "ax_press_focuses_tile",
                    verdict(focused),
                    format!(
                        "после AXPress first responder — центральная плитка: {focused}, был {:?}",
                        run.before.get()
                    ),
                );
                run.check_selection(CENTER, "ax-press", Some(target));
                run.probe_pixels()
            },
        ),
    ]);
    let driver = {
        let run = Rc::clone(run);
        move || run.drive_switches()
    };
    let measure = {
        let run = Rc::clone(run);
        move || conclude_switches(&run)
    };
    steps.push(Step::new(
        "100 переключений L в центре",
        SWITCH_TIMEOUT,
        driver,
        measure,
    ));
    steps
}

fn conclude_switches(run: &Run) -> Result<(), String> {
    let switches = run.switches.replace(Switches::default());
    if let Some(error) = switches.error {
        return Err(format!("переключения: {error}"));
    }
    let mut report = run.report.borrow_mut();
    report.finding(
        "switches_state",
        verdict(switches.mismatches.is_empty()),
        if switches.mismatches.is_empty() {
            format!(
                "{} переключений, из них {WARMUP} прогревочных: после каждого фон активной вкладки на ожидаемой",
                switches.done
            )
        } else {
            switches.mismatches.join("; ")
        },
    );
    let p95 = crate::stats::summarize(&switches.post_to_pass)
        .ok()
        .map(|summary| summary.p95);
    report.metric("tab_switch_post_to_state", "ms", switches.post_to_state);
    report.metric(
        "tab_switch_post_to_dispatch",
        "ms",
        switches.post_to_dispatch,
    );
    report.metric(
        "tab_switch_post_to_display_pass",
        "ms",
        switches.post_to_pass,
    );
    report.metric(
        "tab_switch_dispatch_to_return",
        "ms",
        switches.dispatch_to_return,
    );
    report.finding(
        "tab_switch_budget",
        verdict(p95.is_some_and(|p95| p95 <= BUDGET_MS)),
        format!(
            "p95 интервала postEvent → конец прохода отображения {} при бюджете {BUDGET_MS} мс; вывод об измеренном участке, не о времени до экрана",
            p95.map_or_else(|| "нет".to_owned(), |p95| format!("{p95:.3} мс"))
        ),
    );
    Ok(())
}

fn conclude_regimes(run: &Run, report: &mut Report) {
    let regimes = run.regimes.borrow();
    report.push_json(
        "regimes",
        &regimes
            .iter()
            .map(|(state, regime)| (state.clone(), *regime))
            .collect::<Vec<_>>(),
    );
    for regime in [Regime::Fit, Regime::Scroll] {
        let seen = regimes.iter().any(|(_, seen)| *seen == regime);
        report.finding(
            &format!("regime_{regime:?}_seen").to_lowercase(),
            verdict(seen),
            format!("режим {regime:?} встретился в проверенных состояниях: {seen}"),
        );
    }
    let buttons = run.buttons.borrow();
    let mut problems = Vec::new();
    for place in NAMES {
        let mut states = buttons
            .iter()
            .filter(|(state, _)| state.ends_with(&format!("/{place}")));
        if let Some((first_state, first)) = states.next() {
            for (state, offsets) in states {
                if offsets != first {
                    problems.push(format!(
                        "{state}: {offsets:?} против {first_state}: {first:?}"
                    ));
                }
            }
        }
    }
    report.finding(
        "buttons_fixed",
        verdict(problems.is_empty()),
        if problems.is_empty() {
            format!(
                "у каждой полосы кнопки в {} состояниях на одном расстоянии от правого края, во всех режимах",
                buttons.len()
            )
        } else {
            problems.join("; ")
        },
    );
}

fn finish(run: Rc<Run>, result: Result<(), StepFailure>) {
    let stand = Rc::clone(&run.stand);
    let mut report = run.report.replace(Report::default());
    run.settle.add_to(&mut report);
    if let Err(failure) = result {
        return stand.conclude(report, Err(failure.describe()));
    }
    conclude_regimes(&run, &mut report);
    report.push_json("material", &material::scan_app(&stand.app));
    capture(run, report);
}

/// Рендер содержимого и снимок окна для визуального прохода владельца.
fn capture(run: Rc<Run>, mut report: Report) {
    let stand = Rc::clone(&run.stand);
    let render = match ViewRender::of(&run.scene.root) {
        Ok(render) => render,
        Err(reason) => return stand.conclude(report, Err(reason)),
    };
    let jobs: Vec<Background<Snapshot>> = vec![
        render.save(&stand.out, "tabs-content"),
        snapshot::capture_window(run.window(), &stand.out, "tabs-window"),
    ];
    snapshot::wait_all(jobs, SNAPSHOT_TIMEOUT, move |result| {
        let snapshots = match result {
            Ok(snapshots) => snapshots,
            Err(reason) => return stand.conclude(report, Err(reason)),
        };
        let mut failed = Vec::new();
        for snapshot in snapshots {
            match snapshot {
                Ok(snapshot) => {
                    if snapshot.status == SnapshotStatus::Error {
                        failed.push(format!(
                            "{}: {}",
                            snapshot.name,
                            snapshot.reason.as_deref().unwrap_or("ошибка")
                        ));
                    }
                    report.snapshot(snapshot);
                }
                Err(reason) => failed.push(reason),
            }
        }
        if !failed.is_empty() {
            return stand.conclude(report, Err(format!("снимки не записаны: {failed:?}")));
        }
        drop(run);
        stand.conclude(report, Ok(()));
    });
}
