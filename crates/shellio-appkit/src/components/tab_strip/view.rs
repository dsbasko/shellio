//! Вид `TabStrip` на главном потоке.
//!
//! Полоса — собственный `NSView` с заливкой заголовка плитки и линией снизу. Ряд вкладок лежит в
//! виде-обрезке и сдвигается смещением прокрутки; вкладка — свой вид с заголовком, значком типа,
//! точкой статуса и кнопкой закрытия, его переиспользуют по id. Кольцо и линия под выбранной
//! вкладкой — подслои вкладки за её краями, как `box-shadow` и `::after` прототипа. Кнопки
//! «Разделить» и «Развернуть» — кнопки AppKit с подсказкой и `accessibilityHelp` по ADR-0032, меню
//! «…» — компонент [`MenuButton`].

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send,
};
use objc2_app_kit::{
    NSAccessibility, NSAccessibilityRadioButtonRole, NSAccessibilityTabButtonSubrole,
    NSAccessibilityTabGroupRole, NSButton, NSCellImagePosition, NSControl, NSEvent, NSFont,
    NSFontWeightRegular, NSImage, NSImageScaling, NSImageView, NSLineBreakMode, NSResponder,
    NSTextField, NSTrackingArea, NSTrackingAreaOptions, NSView, NSViewLayerContentsRedrawPolicy,
};
use objc2_foundation::{NSNumber, NSObject, NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{CABasicAnimation, CALayer, CAMediaTiming, CAMediaTimingFunction};
use shellio_theme::{AppTheme, Motion};

use super::layout::{self, Regime, StripLayout, TabParts};
use super::model::{
    self, StripButton, StripKey, StripOp, Tab, TabError, TabIntent, TabOp, TabStatus, TabStripState,
};
use crate::app::App;
use crate::chrome_bar::Frame;
use crate::components::menu::{Menu, MenuButton, MenuError, MenuIntent};
use crate::counters::{Live, counter};
use crate::inspect::{self, InspectNode, NodeState, Transition, TransitionPath};
use crate::layer::{self, without_actions};
use crate::own::{Callback, ComponentId, View};
use crate::piece::ns_rect;
use crate::runloop;

counter!(STRIP_OWNER, Owner, "TabStrip");
counter!(STRIP_VIEW, Class, "ShellioTabStrip");
counter!(STRIP_CLIP, Class, "ShellioTabClip");
counter!(STRIP_ROW, Class, "ShellioTabRow");
counter!(TAB_VIEW, Class, "ShellioTab");
counter!(STRIP_BUTTON, Class, "ShellioTabStripButton");

/// Ключ анимации прокрутки ряда вкладок.
const SCROLL_ANIMATION: &str = "shellio.tabs.scroll";
/// Толщина кольца выбранной вкладки (`box-shadow: 0 0 0 1px`).
const RING: f64 = 1.0;
/// Скругление линии под вкладкой (`border-radius: 1px`).
const UNDERLINE_RADIUS: f64 = 1.0;
/// Толщина кольца точки «процесс жив» и высота черты «завершён».
const IDLE_RING: f64 = 1.5;
const EXIT_BAR: f64 = 2.0;
/// Значок закрытия внутри своей кнопки.
const CLOSE_INSET: f64 = 3.0;

/// Вкладка в `ui.inspect`: что видно у вида.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabFacts {
    pub id: String,
    /// Значок типа показан (в боковой зоне его нет).
    pub icon_shown: bool,
    /// Кнопка закрытия видна.
    pub close_visible: bool,
    /// Вкладка видна в ряду целиком.
    pub fully_visible: bool,
}

/// Кнопка полосы в `ui.inspect`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ButtonFacts {
    pub button: StripButton,
    pub symbol: &'static str,
    pub hint: String,
    pub help: String,
}

/// Состояние полосы для `ui.inspect`.
#[derive(Debug, Clone, PartialEq)]
pub struct TabStripFacts {
    pub state: TabStripState,
    /// Режим по последней раскладке; `None` — раскладки ещё не было.
    pub regime: Option<Regime>,
    /// Нагрузка видна в строке.
    pub payload_visible: bool,
    pub offset: f64,
    pub max_offset: f64,
    /// Полоса — first responder окна.
    pub keyboard_focus: bool,
    pub tabs: Vec<TabFacts>,
    pub buttons: Vec<ButtonFacts>,
}

/// Rust-часть полосы. Виды читают её через слабую ссылку.
struct StripModel {
    id: ComponentId,
    theme: Cell<AppTheme>,
    motion: Cell<Motion>,
    state: RefCell<TabStripState>,
    layout: RefCell<Option<StripLayout>>,
    /// Смещение прокрутки ряда вкладок.
    offset: Cell<f64>,
    /// Вкладка под указателем.
    hover: RefCell<Option<String>>,
    /// Полоса — first responder окна: кольцо выбранной вкладки цвета режима.
    key_focus: Cell<bool>,
    on_intent: Rc<dyn Fn(TabIntent)>,
    menu: MenuButton,
}

impl StripModel {
    fn emit(&self, intent: Option<TabIntent>) {
        if let Some(intent) = intent {
            (self.on_intent)(intent);
        }
    }
}

/// Полоса вкладок на главном потоке. Освобождение убирает её вид из иерархии.
pub struct TabStrip {
    view: View<StripView, StripModel>,
}

impl TabStrip {
    /// Полоса с состоянием `state`. `on_intent` вызывается на главном потоке изнутри AppKit и не
    /// должен синхронно менять полосу.
    pub fn new(
        app: &App,
        theme: &AppTheme,
        motion: Motion,
        state: TabStripState,
        on_intent: impl Fn(TabIntent) + 'static,
    ) -> Result<TabStrip, TabError> {
        model::check(&state)?;
        let mtm = app.mtm();
        let on_intent: Rc<dyn Fn(TabIntent)> = Rc::new(on_intent);
        let menu_intent = Rc::clone(&on_intent);
        let menu = MenuButton::new(
            app,
            theme,
            StripButton::Menu.label(),
            StripButton::Menu.label(),
            Vec::new(),
            move |intent| match intent {
                MenuIntent::Chosen { id } => menu_intent(TabIntent::Menu { id }),
            },
        )
        .map_err(|error| match error {
            MenuError::DuplicateId(id) => TabError::DuplicateId(id),
        })?;
        let menu_view = menu.ns_view().retain();
        let model = StripModel {
            id: ComponentId::next(),
            theme: Cell::new(*theme),
            motion: Cell::new(motion),
            // Пустое имя: первая разница ставит AX-имя полосы.
            state: RefCell::new(TabStripState::new("", state.presentation)),
            layout: RefCell::new(None),
            offset: Cell::new(0.0),
            hover: RefCell::new(None),
            key_focus: Cell::new(false),
            on_intent,
            menu,
        };
        let mut view = View::new(&STRIP_OWNER, model, "TabStrip.view", |owner| {
            StripView::new(mtm, owner, menu_view)
        });
        let parts = view.view().ivars();
        let (split, zoom) = (parts.split.clone(), parts.zoom.clone());
        let split: &NSControl = &split;
        view.bind_action(split, "TabStrip.split", |model| {
            let axis = model.state.borrow().presentation.axis();
            (model.on_intent)(TabIntent::Split(axis));
        });
        let zoom: &NSControl = &zoom;
        view.bind_action(zoom, "TabStrip.zoom", |model| {
            let zoomed = model.state.borrow().presentation.zoomed;
            (model.on_intent)(if zoomed {
                TabIntent::Restore
            } else {
                TabIntent::Zoom
            });
        });
        let strip = TabStrip { view };
        strip.apply_theme();
        strip.configure_buttons();
        strip.set_state(state)?;
        strip.configure_buttons();
        Ok(strip)
    }

    pub fn id(&self) -> ComponentId {
        self.view.rust().id
    }

    pub fn state(&self) -> TabStripState {
        self.view.rust().state.borrow().clone()
    }

    /// Режим по последней раскладке.
    pub fn regime(&self) -> Option<Regime> {
        self.view
            .rust()
            .layout
            .borrow()
            .as_ref()
            .map(|strip| strip.regime)
    }

    /// Новое состояние: виды вкладок переиспользуются по id, выбранная вкладка прокручивается в
    /// видимую часть ряда. Прокрутка анимируется только при обычном движении.
    pub fn set_state(&self, state: TabStripState) -> Result<Transition, TabError> {
        model::check(&state)?;
        let before = self.animations();
        let model = self.view.rust();
        let ops = model::diff(&model.state.borrow(), &state);
        *model.state.borrow_mut() = state;
        let stale = model
            .hover
            .borrow()
            .as_ref()
            .is_some_and(|hover| !model.state.borrow().tabs.iter().any(|tab| &tab.id == hover));
        if stale {
            model.hover.replace(None);
        }
        let view = self.view.view();
        let mut reveal = false;
        for op in ops {
            match op {
                StripOp::Tabs(op) => {
                    self.apply_tab_op(op);
                    reveal = true;
                }
                StripOp::Select(_) => reveal = true,
                StripOp::Label(label) => {
                    view.setAccessibilityLabel(Some(&NSString::from_str(&label)));
                }
                StripOp::Payload(payload) => {
                    view.ivars().payload.setStringValue(&NSString::from_str(
                        payload.as_deref().unwrap_or_default(),
                    ));
                }
                StripOp::Presentation(_) => {
                    self.configure_tabs();
                    self.configure_buttons();
                    reveal = true;
                }
                StripOp::Keys(_) => self.configure_buttons(),
                StripOp::Focus(_) | StripOp::Mode(_) | StripOp::Actions(_) => {}
            }
        }
        view.setNeedsLayout(true);
        view.layoutSubtreeIfNeeded();
        self.restyle();
        sync_menu(model);
        let path = if reveal {
            self.reveal_selected()
        } else {
            TransitionPath::Instant
        };
        Ok(self.transition(path, &before))
    }

    /// Новая ячейка темы: цвета меняются без анимаций.
    pub fn set_theme(&self, theme: &AppTheme) -> Transition {
        let before = self.animations();
        let model = self.view.rust();
        if model.theme.get() != *theme {
            model.theme.set(*theme);
            model.menu.set_theme(theme);
            self.apply_theme();
        }
        self.transition(TransitionPath::Instant, &before)
    }

    /// Движение: при `Reduced` прокрутка к вкладке идёт без анимации, а идущая прокрутка
    /// снимается, и ряд сразу стоит на своём смещении.
    pub fn set_motion(&self, motion: Motion) {
        self.view.rust().motion.set(motion);
        if motion == Motion::Reduced
            && let Some(row_layer) = self.view.view().ivars().row.layer()
        {
            row_layer.removeAnimationForKey(&NSString::from_str(SCROLL_ANIMATION));
        }
    }

    /// Отдаёт полосе клавиатурный фокус: стрелки меняют выбор.
    pub fn focus(&self) -> bool {
        let view = self.view.view();
        view.window()
            .is_some_and(|window| window.makeFirstResponder(Some(view)))
    }

    /// Меню «…» полосы.
    pub fn menu(&self) -> &MenuButton {
        &self.view.rust().menu
    }

    /// Открывает меню выбора `menu` под кнопкой «Разделить» (ADR-0032: выбор «Открыть в новой
    /// части» открывается у кнопки), без неё — под полосой. Первый пункт подсвечен.
    pub fn pop_up_choice(&self, menu: &Menu) {
        let parts = self.view.view().ivars();
        if parts.split.isHiddenOrHasHiddenAncestor() {
            menu.pop_up_choice_below(self.view.view());
        } else {
            menu.pop_up_choice_below(&parts.split);
        }
    }

    pub(crate) fn ns_view(&self) -> &NSView {
        self.view.view()
    }

    /// Полоса и её части для `ui.inspect`: вкладки (`<id>.tab.<id вкладки>`) с кадром видимой
    /// части и выбором из AX вида, нагрузка, кнопки и меню «…».
    pub fn inspect(&self, id: &str, parent: Option<&str>) -> Vec<InspectNode> {
        let model = self.view.rust();
        let view = self.view.view();
        let parts = view.ivars();
        let state = model.state.borrow().clone();
        let mut nodes = vec![inspect::node(
            id,
            "TabStrip",
            parent,
            view,
            None,
            Some(NodeState::TabStrip(Box::new(self.facts()))),
        )];
        for (tab, tab_view) in state.tabs.iter().zip(parts.tabs.borrow().iter()) {
            let mut node = inspect::node(
                &format!("{id}.tab.{}", tab.id),
                "Tab",
                Some(id),
                tab_view,
                None,
                None,
            );
            let shown = tab_view.visibleRect();
            node.facts.frame = inspect::frame_of(tab_view.convertRect_toView(shown, None));
            node.facts.visible &= shown.size.width > 0.0 && shown.size.height > 0.0;
            node.facts.selected = Some(tab_view.isAccessibilitySelected());
            nodes.push(node);
        }
        nodes.push(inspect::node(
            &format!("{id}.payload"),
            "Payload",
            Some(id),
            &parts.payload,
            None,
            None,
        ));
        for button in state.presentation.buttons() {
            let button_id = format!("{id}.{}", button.id());
            match button {
                StripButton::Split(_) => nodes.push(inspect::node(
                    &button_id,
                    "StripButton",
                    Some(id),
                    &parts.split,
                    Some(parts.split.isEnabled()),
                    None,
                )),
                StripButton::Zoom | StripButton::Restore => nodes.push(inspect::node(
                    &button_id,
                    "StripButton",
                    Some(id),
                    &parts.zoom,
                    Some(parts.zoom.isEnabled()),
                    None,
                )),
                StripButton::Menu => nodes.extend(model.menu.inspect(&button_id, Some(id))),
            }
        }
        nodes
    }

    fn facts(&self) -> TabStripFacts {
        let model = self.view.rust();
        let view = self.view.view();
        let parts = view.ivars();
        let state = model.state.borrow().clone();
        let layout = model.layout.borrow().clone();
        let clip = parts.clip.convertRect_toView(parts.clip.bounds(), None);
        let tabs = state
            .tabs
            .iter()
            .zip(parts.tabs.borrow().iter())
            .map(|(tab, tab_view)| {
                let frame = tab_view.convertRect_toView(tab_view.bounds(), None);
                TabFacts {
                    id: tab.id.clone(),
                    icon_shown: !tab_view.ivars().icon.isHidden(),
                    close_visible: !tab_view.ivars().close.isHidden(),
                    fully_visible: contains(clip, frame),
                }
            })
            .collect();
        let buttons = state
            .presentation
            .buttons()
            .into_iter()
            .map(|button| {
                let key = button.key(&state.keys);
                ButtonFacts {
                    button,
                    symbol: button.symbol(),
                    hint: model::hint(button, key),
                    help: model::help(key),
                }
            })
            .collect();
        let keyboard_focus = view.is_key_focus();
        TabStripFacts {
            regime: layout.as_ref().map(|strip| strip.regime),
            payload_visible: !parts.payload.isHidden() && state.payload.is_some(),
            offset: model.offset.get(),
            max_offset: layout.as_ref().map_or(0.0, |strip| strip.max_offset),
            keyboard_focus,
            tabs,
            buttons,
            state,
        }
    }

    fn animations(&self) -> Vec<(usize, String, usize)> {
        self.view
            .view()
            .layer()
            .map(|layer| inspect::animation_set(&layer))
            .unwrap_or_default()
    }

    fn transition(&self, path: TransitionPath, before: &[(usize, String, usize)]) -> Transition {
        Transition {
            path,
            added: inspect::added_animations(before, &self.animations()),
        }
    }

    fn apply_tab_op(&self, op: TabOp) {
        let view = self.view.view();
        let parts = view.ivars();
        let mtm = view.mtm();
        let mut tabs = parts.tabs.borrow_mut();
        match op {
            TabOp::Remove { index } => {
                if index < tabs.len() {
                    let removed = tabs.remove(index);
                    removed.removeFromSuperview();
                }
            }
            TabOp::Insert { index, tab } => {
                let tab_view = TabView::new(mtm, parts.owner.clone(), &tab.id);
                parts.row.addSubview(&tab_view);
                let at = index.min(tabs.len());
                tabs.insert(at, tab_view);
                drop(tabs);
                self.configure_tab(&tab);
            }
            TabOp::Move { from, to } => {
                if from < tabs.len() {
                    let moved = tabs.remove(from);
                    let at = to.min(tabs.len());
                    tabs.insert(at, moved);
                }
            }
            TabOp::Update { index, .. } => {
                drop(tabs);
                let tab = self.view.rust().state.borrow().tabs.get(index).cloned();
                if let Some(tab) = tab {
                    self.configure_tab(&tab);
                }
            }
        }
    }

    /// Текст, значок и AX вкладки `tab` по её id.
    fn configure_tab(&self, tab: &Tab) {
        let parts = self.view.view().ivars();
        let presentation = self.view.rust().state.borrow().presentation;
        let tabs = parts.tabs.borrow();
        let Some(tab_view) = tabs.iter().find(|view| *view.ivars().id.borrow() == tab.id) else {
            return;
        };
        tab_view.configure(tab, presentation.shows_icons());
    }

    fn configure_tabs(&self) {
        let tabs = self.view.rust().state.borrow().tabs.clone();
        for tab in &tabs {
            self.configure_tab(tab);
        }
    }

    /// Значки, имена, подсказки и `accessibilityHelp` кнопок по признакам представления.
    fn configure_buttons(&self) {
        let state = self.view.rust().state.borrow().clone();
        let parts = self.view.view().ivars();
        let buttons = state.presentation.buttons();
        let split = buttons
            .iter()
            .copied()
            .find(|button| matches!(button, StripButton::Split(_)));
        parts.split.setHidden(split.is_none());
        if let Some(split) = split {
            parts.split.describe(split, split.key(&state.keys));
        }
        let zoom = if state.presentation.zoomed {
            StripButton::Restore
        } else {
            StripButton::Zoom
        };
        parts.zoom.describe(zoom, zoom.key(&state.keys));
        let menu: &NSView = &parts.menu;
        let key = StripButton::Menu.key(&state.keys);
        menu.setToolTip(Some(&NSString::from_str(&model::hint(
            StripButton::Menu,
            key,
        ))));
        menu.setAccessibilityHelp(Some(&NSString::from_str(&model::help(key))));
    }

    /// Прокручивает ряд так, чтобы выбранная вкладка была видна целиком.
    fn reveal_selected(&self) -> TransitionPath {
        let model = self.view.rust();
        let view = self.view.view();
        let Some(index) = model.state.borrow().selected_index() else {
            return TransitionPath::Instant;
        };
        let Some(strip) = model.layout.borrow().clone() else {
            return TransitionPath::Instant;
        };
        let Some(tab) = strip.tabs.get(index).copied() else {
            return TransitionPath::Instant;
        };
        if strip.width <= 0.0 {
            // Полоса ещё не размещена: выбранную вкладку покажет раскладка после размещения.
            return TransitionPath::Instant;
        }
        let current = model.offset.get();
        let target = layout::reveal(current, tab, strip.clip.width, strip.max_offset);
        if (target - current).abs() < f64::EPSILON {
            return TransitionPath::Instant;
        }
        model.offset.set(target);
        let row = &view.ivars().row;
        let from = row.frame().origin.x;
        without_actions(|| {
            let mut origin = row.frame().origin;
            origin.x = -target;
            row.setFrameOrigin(origin);
        });
        match model.motion.get() {
            Motion::Reduced => TransitionPath::Instant,
            Motion::Normal => {
                let tokens = AppTheme::motion(Motion::Normal);
                if let Some(row_layer) = row.layer() {
                    let animation = CABasicAnimation::animationWithKeyPath(Some(
                        &NSString::from_str("transform.translation.x"),
                    ));
                    let delta = NSNumber::new_f64(from + target);
                    let zero = NSNumber::new_f64(0.0);
                    // SAFETY: значения — NSNumber, как ждёт ключ `transform.translation.x`.
                    unsafe {
                        animation.setFromValue(Some(&delta));
                        animation.setToValue(Some(&zero));
                    }
                    animation.setDuration(tokens.duration_ms / 1000.0);
                    let easing = tokens.easing;
                    animation.setTimingFunction(Some(
                        &CAMediaTimingFunction::functionWithControlPoints(
                            easing.x1 as f32,
                            easing.y1 as f32,
                            easing.x2 as f32,
                            easing.y2 as f32,
                        ),
                    ));
                    row_layer.addAnimation_forKey(
                        &animation,
                        Some(&NSString::from_str(SCROLL_ANIMATION)),
                    );
                }
                TransitionPath::Animated
            }
        }
    }

    /// Выбор, AX и кнопки закрытия всех вкладок сразу, цвета — в ближайшем проходе отображения.
    fn restyle(&self) {
        let view = self.view.view();
        view.sync_tabs();
        view.setNeedsDisplay(true);
    }

    fn apply_theme(&self) {
        let theme = self.view.rust().theme.get();
        let parts = self.view.view().ivars();
        let muted = layer::ns_color(theme.colors.text_3);
        parts.payload.setTextColor(Some(&muted));
        parts.payload.setFont(Some(&payload_font(&theme)));
        for button in [&parts.split, &parts.zoom] {
            button.setContentTintColor(Some(&muted));
        }
        for tab_view in parts.tabs.borrow().iter() {
            tab_view.ivars().title.setFont(Some(&tab_font(&theme)));
        }
        self.restyle();
        self.view.view().setNeedsLayout(true);
    }
}

/// Пункты меню «…»: нагрузка в заголовке, если её нет в строке.
fn sync_menu(model: &StripModel) {
    let hidden = model
        .layout
        .borrow()
        .as_ref()
        .is_none_or(|strip| strip.regime.payload_hidden());
    let entries = model::menu_entries(&model.state.borrow(), hidden);
    if model.menu.menu().entries() != entries
        && let Err(error) = model.menu.menu().set_entries(entries)
    {
        // Действия панели с id заголовка нагрузки: меню остаётся прежним.
        eprintln!("shellio: меню полосы вкладок не обновлено: {error}");
    }
}

fn tab_font(theme: &AppTheme) -> Retained<NSFont> {
    // SAFETY: вес шрифта — неизменяемая константа AppKit.
    NSFont::systemFontOfSize_weight(theme.metrics.font.tab, unsafe { NSFontWeightRegular })
}

fn payload_font(theme: &AppTheme) -> Retained<NSFont> {
    // SAFETY: вес шрифта — неизменяемая константа AppKit.
    NSFont::monospacedDigitSystemFontOfSize_weight(theme.metrics.font.meta, unsafe {
        NSFontWeightRegular
    })
}

/// Кнопка закрытия видна у выбранной вкладки и при наведении; в боковой зоне — только при
/// наведении, поверх конца вкладки (прототип).
fn close_visible(state: &TabStripState, tab: &Tab, hover: Option<&str>) -> bool {
    let hovered = hover == Some(tab.id.as_str());
    let selected = state.selected.as_deref() == Some(tab.id.as_str());
    if state.presentation.reserves_close() {
        hovered || selected
    } else {
        hovered
    }
}

fn inside(point: NSPoint, rect: NSRect) -> bool {
    point.x >= rect.origin.x
        && point.x < rect.origin.x + rect.size.width
        && point.y >= rect.origin.y
        && point.y < rect.origin.y + rect.size.height
}

fn contains(outer: NSRect, inner: NSRect) -> bool {
    const EPSILON: f64 = 0.01;
    inner.origin.x >= outer.origin.x - EPSILON
        && inner.origin.y >= outer.origin.y - EPSILON
        && inner.origin.x + inner.size.width <= outer.origin.x + outer.size.width + EPSILON
        && inner.origin.y + inner.size.height <= outer.origin.y + outer.size.height + EPSILON
}

fn symbol(name: &str, description: &str) -> Option<Retained<NSImage>> {
    NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(name),
        Some(&NSString::from_str(description)),
    )
}

/// Вид со своим слоем, который перерисовывается только по `setNeedsDisplay`.
fn layered(view: &NSView) {
    view.setWantsLayer(true);
    view.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::OnSetNeedsDisplay);
}

fn strip_key(event: &NSEvent) -> Option<StripKey> {
    match event.keyCode() {
        123 => Some(StripKey::Left),
        124 => Some(StripKey::Right),
        36 | 49 | 76 => Some(StripKey::Confirm),
        _ => None,
    }
}

struct StripIvars {
    _live: Live,
    owner: Callback<StripModel>,
    clip: Retained<ClipView>,
    row: Retained<RowView>,
    tabs: RefCell<Vec<Retained<TabView>>>,
    payload: Retained<NSTextField>,
    split: Retained<StripButtonView>,
    zoom: Retained<StripButtonView>,
    /// Вид кнопки меню «…»; кнопку держит Rust-часть полосы.
    menu: Retained<NSView>,
    line: Retained<CALayer>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTabStrip"]
    #[ivars = StripIvars]
    struct StripView;

    impl StripView {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `wantsUpdateLayer` NSView.
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `updateLayer` NSView.
        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            let Some(theme) = self.ivars().owner.call(|model| {
                runloop::mark_draw(model.id);
                model.theme.get()
            }) else {
                return;
            };
            if let Some(layer) = self.layer() {
                without_actions(|| {
                    layer::set_fill(&layer, theme.colors.panel_head);
                    layer::set_fill(&self.ivars().line, theme.colors.line);
                });
            }
        }

        // SAFETY: селектор и результат — как у `layout` NSView.
        #[unsafe(method(layout))]
        fn layout(&self) {
            // SAFETY: `layout` у NSView без аргументов.
            let _: () = unsafe { msg_send![super(self), layout] };
            self.place();
        }

        // SAFETY: селектор и аргумент — как у `setFrameSize:` NSView: новая ширина меняет режим.
        #[unsafe(method(setFrameSize:))]
        fn set_frame_size(&self, size: NSSize) {
            // SAFETY: `setFrameSize:` у NSView с тем же размером.
            let _: () = unsafe { msg_send![super(self), setFrameSize: size] };
            self.setNeedsLayout(true);
        }

        // SAFETY: селектор и результат — как у `acceptsFirstResponder` NSResponder.
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `canBecomeKeyView` NSView.
        #[unsafe(method(canBecomeKeyView))]
        fn can_become_key_view(&self) -> bool {
            !self.isHiddenOrHasHiddenAncestor()
        }

        // SAFETY: селектор и результат — как у `becomeFirstResponder` NSResponder.
        #[unsafe(method(becomeFirstResponder))]
        fn become_first_responder(&self) -> bool {
            // SAFETY: `becomeFirstResponder` у NSResponder без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), becomeFirstResponder] };
            self.ivars().owner.call(|model| model.key_focus.set(accepted));
            self.sync_tabs();
            accepted
        }

        // SAFETY: селектор и результат — как у `resignFirstResponder` NSResponder.
        #[unsafe(method(resignFirstResponder))]
        fn resign_first_responder(&self) -> bool {
            // SAFETY: `resignFirstResponder` у NSResponder без аргументов.
            let accepted: bool = unsafe { msg_send![super(self), resignFirstResponder] };
            if accepted {
                self.ivars().owner.call(|model| model.key_focus.set(false));
            }
            self.sync_tabs();
            accepted
        }

        // SAFETY: селектор и аргумент — как у `keyDown:` NSResponder.
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let Some(key) = strip_key(event) else {
                // SAFETY: `keyDown:` у NSResponder с тем же событием.
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
                return;
            };
            self.ivars().owner.call(|model| {
                let intent = model::key_intent(&model.state.borrow(), key);
                model.emit(intent);
            });
        }

        // SAFETY: селектор и аргумент — как у `mouseMoved:` NSResponder.
        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let hovered = self.tab_at(event.locationInWindow());
            self.set_hover(hovered);
        }

        // SAFETY: селектор и аргумент — как у `mouseEntered:` NSResponder.
        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, event: &NSEvent) {
            let hovered = self.tab_at(event.locationInWindow());
            self.set_hover(hovered);
        }

        // SAFETY: селектор и аргумент — как у `mouseExited:` NSResponder.
        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _event: &NSEvent) {
            self.set_hover(None);
        }

        // SAFETY: селектор и аргумент — как у `scrollWheel:` NSResponder.
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let (dx, dy) = (event.scrollingDeltaX(), event.scrollingDeltaY());
            let delta = if dx.abs() >= dy.abs() { dx } else { dy };
            let scrolled = self.ivars().owner.call(|model| {
                let max = model
                    .layout
                    .borrow()
                    .as_ref()
                    .map_or(0.0, |strip| strip.max_offset);
                let offset = layout::clamp_offset(model.offset.get() - delta, max);
                let changed = (offset - model.offset.get()).abs() >= f64::EPSILON;
                model.offset.set(offset);
                changed
            });
            if scrolled == Some(true) {
                self.place();
            }
        }

        // SAFETY: селектор и результат — как у `mouseDownCanMoveWindow` NSView: за полосу окно не
        // двигается.
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }
    }
);

impl StripView {
    fn new(
        mtm: MainThreadMarker,
        owner: Callback<StripModel>,
        menu: Retained<NSView>,
    ) -> Retained<StripView> {
        let clip = ClipView::new(mtm);
        let row = RowView::new(mtm);
        clip.addSubview(&row);
        let payload = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        payload.setLineBreakMode(NSLineBreakMode::ByClipping);
        let split = StripButtonView::new(mtm);
        let zoom = StripButtonView::new(mtm);
        let this = StripView::alloc(mtm).set_ivars(StripIvars {
            _live: STRIP_VIEW.live(),
            owner,
            clip,
            row,
            tabs: RefCell::new(Vec::new()),
            payload,
            split,
            zoom,
            menu,
            line: CALayer::new(),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<StripView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        layered(&view);
        let parts = view.ivars();
        if let Some(layer) = view.layer() {
            layer.addSublayer(&parts.line);
        }
        view.addSubview(&parts.clip);
        view.addSubview(&parts.payload);
        view.addSubview(&parts.split);
        view.addSubview(&parts.zoom);
        view.addSubview(&parts.menu);
        view.setAccessibilityElement(true);
        // SAFETY: роль доступности — неизменяемая константа AppKit.
        view.setAccessibilityRole(Some(unsafe { NSAccessibilityTabGroupRole }));
        let options = NSTrackingAreaOptions::MouseEnteredAndExited
            | NSTrackingAreaOptions::MouseMoved
            | NSTrackingAreaOptions::ActiveInKeyWindow
            | NSTrackingAreaOptions::InVisibleRect;
        let owner: &NSObject = &view;
        // SAFETY: владелец области — сама полоса, она держит область до освобождения; область
        // не держит владельца, и после освобождения полосы события ей не приходят.
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                options,
                Some(owner.as_ref()),
                None,
            )
        };
        view.addTrackingArea(&area);
        view.setNeedsLayout(true);
        view
    }

    /// Раскладка по чистой части: ширины вкладок и нагрузки меряет AppKit.
    fn place(&self) {
        let parts = self.ivars();
        let Some((state, theme, offset)) = parts.owner.call(|model| {
            (
                model.state.borrow().clone(),
                model.theme.get(),
                model.offset.get(),
            )
        }) else {
            return;
        };
        let presentation = state.presentation;
        let tabs = parts.tabs.borrow();
        let measured: Vec<TabParts> = tabs
            .iter()
            .zip(&state.tabs)
            .map(|(view, tab)| {
                let size = view.ivars().title.intrinsicContentSize();
                layout::tab_parts(
                    size.width,
                    size.height.ceil(),
                    presentation.shows_icons(),
                    tab.status.has_dot(),
                    presentation.reserves_close(),
                )
            })
            .collect();
        let widths: Vec<f64> = measured.iter().map(|parts| parts.width).collect();
        let payload = state.payload.as_ref().map(|_| {
            let size = parts.payload.intrinsicContentSize();
            (
                size.width.ceil() + 2.0 * layout::PAYLOAD_PAD,
                size.height.ceil(),
            )
        });
        let buttons = presentation.buttons();
        let bounds = self.bounds();
        let strip = layout::layout(
            bounds.size.width,
            bounds.size.height,
            &widths,
            payload,
            buttons.len(),
        );
        let resized = parts.owner.call(|model| {
            model
                .layout
                .borrow()
                .as_ref()
                .is_none_or(|previous| (previous.width - strip.width).abs() >= f64::EPSILON)
        });
        let mut offset = layout::clamp_offset(offset, strip.max_offset);
        if resized == Some(true)
            && strip.width > 0.0
            && let Some(tab) = state
                .selected_index()
                .and_then(|index| strip.tabs.get(index))
        {
            offset = layout::reveal(offset, *tab, strip.clip.width, strip.max_offset);
        }
        without_actions(|| {
            parts.clip.setFrame(ns_rect(strip.clip));
            parts.row.setFrame(ns_rect(Frame::new(
                -offset,
                0.0,
                strip.content_width,
                strip.clip.height,
            )));
            for ((view, frame), tab_parts) in tabs.iter().zip(&strip.tabs).zip(&measured) {
                view.setFrame(ns_rect(*frame));
                view.place(tab_parts, theme.focus.underline);
            }
            match strip.payload {
                Some(frame) => {
                    parts.payload.setHidden(false);
                    parts.payload.setFrame(ns_rect(Frame::new(
                        frame.x + layout::PAYLOAD_PAD,
                        frame.y,
                        frame.width - 2.0 * layout::PAYLOAD_PAD,
                        frame.height,
                    )));
                }
                None => parts.payload.setHidden(true),
            }
            for (button, frame) in buttons.iter().zip(&strip.buttons) {
                let target: &NSView = match button {
                    StripButton::Split(_) => &parts.split,
                    StripButton::Zoom | StripButton::Restore => &parts.zoom,
                    StripButton::Menu => &parts.menu,
                };
                target.setFrame(ns_rect(*frame));
            }
            parts.line.setFrame(ns_rect(Frame::new(
                0.0,
                bounds.size.height - layout::BOTTOM_LINE,
                bounds.size.width,
                layout::BOTTOM_LINE,
            )));
        });
        drop(tabs);
        parts.owner.call(|model| {
            model.offset.set(offset);
            let previous = model.layout.replace(Some(strip.clone()));
            if previous.map(|previous| previous.regime) != Some(strip.regime) {
                sync_menu(model);
            }
        });
    }

    /// Вкладка под точкой `point` в координатах окна.
    fn tab_at(&self, point: NSPoint) -> Option<String> {
        let parts = self.ivars();
        if !inside(
            parts.clip.convertPoint_fromView(point, None),
            parts.clip.bounds(),
        ) {
            return None;
        }
        parts.tabs.borrow().iter().find_map(|tab| {
            inside(tab.convertPoint_fromView(point, None), tab.bounds())
                .then(|| tab.ivars().id.borrow().clone())
        })
    }

    fn set_hover(&self, hovered: Option<String>) {
        let changed = self.ivars().owner.call(|model| {
            let mut hover = model.hover.borrow_mut();
            let changed = *hover != hovered;
            *hover = hovered;
            changed
        });
        if changed == Some(true) {
            self.sync_tabs();
        }
    }

    /// Выбор и AX вкладок и видимость кнопок закрытия — сразу, цвета — в `updateLayer`.
    fn sync_tabs(&self) {
        let parts = self.ivars();
        let Some((state, hover)) = parts
            .owner
            .call(|model| (model.state.borrow().clone(), model.hover.borrow().clone()))
        else {
            return;
        };
        for tab_view in parts.tabs.borrow().iter() {
            let id = tab_view.ivars().id.borrow().clone();
            let Some(tab) = state.tabs.iter().find(|tab| tab.id == id) else {
                continue;
            };
            let selected = state.selected.as_deref() == Some(id.as_str());
            tab_view.setAccessibilitySelected(selected);
            let value = NSNumber::new_bool(selected);
            // SAFETY: значение радиокнопки доступности — NSNumber, AppKit его копирует.
            unsafe { tab_view.setAccessibilityValue(Some(&value)) };
            tab_view
                .ivars()
                .close
                .setHidden(!close_visible(&state, tab, hover.as_deref()));
            tab_view.setNeedsDisplay(true);
        }
    }

    fn is_key_focus(&self) -> bool {
        self.window()
            .and_then(|window| window.firstResponder())
            .is_some_and(|responder| {
                std::ptr::eq(
                    std::ptr::from_ref::<NSResponder>(&responder).cast::<u8>(),
                    std::ptr::from_ref::<StripView>(self).cast::<u8>(),
                )
            })
    }
}

struct PlainIvars {
    _live: Live,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTabClip"]
    #[ivars = PlainIvars]
    struct ClipView;

    impl ClipView {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

impl ClipView {
    fn new(mtm: MainThreadMarker) -> Retained<ClipView> {
        let this = ClipView::alloc(mtm).set_ivars(PlainIvars {
            _live: STRIP_CLIP.live(),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<ClipView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        layered(&view);
        if let Some(layer) = view.layer() {
            layer.setMasksToBounds(true);
        }
        view
    }
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTabRow"]
    #[ivars = PlainIvars]
    struct RowView;

    impl RowView {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `clipsToBounds` NSView: обрезает ряд вид-обрезка.
        #[unsafe(method(clipsToBounds))]
        fn clips_to_bounds(&self) -> bool {
            false
        }
    }
);

impl RowView {
    fn new(mtm: MainThreadMarker) -> Retained<RowView> {
        let this = RowView::alloc(mtm).set_ivars(PlainIvars {
            _live: STRIP_ROW.live(),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<RowView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        layered(&view);
        view
    }
}

struct TabIvars {
    _live: Live,
    owner: Callback<StripModel>,
    id: RefCell<String>,
    icon: Retained<NSImageView>,
    title: Retained<NSTextField>,
    dot: Retained<CALayer>,
    close: Retained<NSImageView>,
    /// Место точки статуса в точках вкладки.
    dot_slot: Cell<Frame>,
    /// Кольцо выбранной вкладки за её краями.
    ring: Retained<CALayer>,
    /// Линия под выбранной вкладкой части с фокусом.
    underline: Retained<CALayer>,
    /// Кнопка закрытия в точках вкладки.
    close_frame: Cell<Frame>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTab"]
    #[ivars = TabIvars]
    struct TabView;

    impl TabView {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `clipsToBounds` NSView. Кольцо и линия под
        // вкладкой лежат за её краями; `setClipsToBounds(false)` AppKit возвращает к `true`
        // (дымовой прогон P1.3), поэтому вид отвечает сам.
        #[unsafe(method(clipsToBounds))]
        fn clips_to_bounds(&self) -> bool {
            false
        }

        // SAFETY: селектор и результат — как у `wantsUpdateLayer` NSView.
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `updateLayer` NSView.
        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            self.style();
        }

        // SAFETY: селектор, аргумент и результат — как у `acceptsFirstMouse:` NSView.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `mouseDownCanMoveWindow` NSView.
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder. Щелчок по кнопке
        // закрытия закрывает вкладку, по остальной вкладке — выбирает её.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let point = self.convertPoint_fromView(event.locationInWindow(), None);
            let id = self.ivars().id.borrow().clone();
            let close = self.ivars().close_frame.get();
            let on_close = !self.ivars().close.isHidden()
                && point.x >= close.x
                && point.x < close.right()
                && point.y >= close.y
                && point.y < close.y + close.height;
            self.ivars().owner.call(|model| {
                model.emit(Some(if on_close {
                    TabIntent::Close { id }
                } else {
                    TabIntent::Select { id }
                }));
            });
        }

        // SAFETY: селектор и результат — как у `accessibilityPerformPress` NSAccessibility:
        // `AXPress` вкладки выбирает её.
        #[unsafe(method(accessibilityPerformPress))]
        fn accessibility_perform_press(&self) -> bool {
            let id = self.ivars().id.borrow().clone();
            self.ivars()
                .owner
                .call(|model| model.emit(Some(TabIntent::Select { id })))
                .is_some()
        }
    }
);

impl TabView {
    fn new(mtm: MainThreadMarker, owner: Callback<StripModel>, id: &str) -> Retained<TabView> {
        let icon = NSImageView::initWithFrame(NSImageView::alloc(mtm), NSRect::ZERO);
        icon.setImageScaling(NSImageScaling::ScaleProportionallyDown);
        let title = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        title.setLineBreakMode(NSLineBreakMode::ByClipping);
        let close = NSImageView::initWithFrame(NSImageView::alloc(mtm), NSRect::ZERO);
        close.setImageScaling(NSImageScaling::ScaleProportionallyDown);
        close.setImage(symbol("xmark", "Закрыть вкладку").as_deref());
        close.setAccessibilityElement(false);
        icon.setAccessibilityElement(false);
        title.setAccessibilityElement(false);
        let this = TabView::alloc(mtm).set_ivars(TabIvars {
            _live: TAB_VIEW.live(),
            owner,
            id: RefCell::new(id.to_owned()),
            icon,
            title,
            dot: CALayer::new(),
            close,
            dot_slot: Cell::new(Frame::new(0.0, 0.0, 0.0, 0.0)),
            ring: CALayer::new(),
            underline: CALayer::new(),
            close_frame: Cell::new(Frame::new(0.0, 0.0, 0.0, 0.0)),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<TabView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        layered(&view);
        let parts = view.ivars();
        if let Some(layer) = view.layer() {
            layer.setMasksToBounds(false);
            layer.addSublayer(&parts.ring);
            layer.addSublayer(&parts.dot);
            layer.addSublayer(&parts.underline);
        }
        view.addSubview(&parts.icon);
        view.addSubview(&parts.title);
        view.addSubview(&parts.close);
        view.setAccessibilityElement(true);
        // SAFETY: роль и подроль доступности — неизменяемые константы AppKit.
        unsafe {
            view.setAccessibilityRole(Some(NSAccessibilityRadioButtonRole));
            view.setAccessibilitySubrole(Some(NSAccessibilityTabButtonSubrole));
        }
        view
    }

    /// Заголовок, значок типа и AX по вкладке `tab`.
    fn configure(&self, tab: &Tab, shows_icon: bool) {
        let parts = self.ivars();
        parts.id.replace(tab.id.clone());
        let title = NSString::from_str(&tab.title);
        if *parts.title.stringValue() != *title {
            parts.title.setStringValue(&title);
        }
        parts.icon.setImage(symbol(&tab.icon, &tab.kind).as_deref());
        parts.icon.setHidden(!shows_icon);
        self.setAccessibilityLabel(Some(&title));
        if let Some(theme) = parts.owner.call(|model| model.theme.get()) {
            parts.title.setFont(Some(&tab_font(&theme)));
        }
        self.setNeedsDisplay(true);
    }

    /// Части вкладки по чистой раскладке.
    fn place(&self, tab_parts: &TabParts, underline: f64) {
        let parts = self.ivars();
        if let Some(icon) = tab_parts.icon {
            parts.icon.setFrame(ns_rect(icon));
        }
        parts.title.setFrame(ns_rect(tab_parts.title));
        parts
            .dot_slot
            .set(tab_parts.dot.unwrap_or(Frame::new(0.0, 0.0, 0.0, 0.0)));
        parts.close_frame.set(tab_parts.close);
        let close = tab_parts.close;
        parts.close.setFrame(ns_rect(Frame::new(
            close.x + CLOSE_INSET,
            close.y + CLOSE_INSET,
            close.width - 2.0 * CLOSE_INSET,
            close.height - 2.0 * CLOSE_INSET,
        )));
        let bounds = Frame::new(0.0, 0.0, tab_parts.width, layout::TAB_HEIGHT);
        parts.ring.setFrame(ns_rect(Frame::new(
            -RING,
            -RING,
            bounds.width + 2.0 * RING,
            bounds.height + 2.0 * RING,
        )));
        parts
            .underline
            .setFrame(ns_rect(layout::underline(bounds, underline)));
        if let Some(layer) = self.layer() {
            layer.setMasksToBounds(false);
        }
        self.setNeedsDisplay(true);
    }

    /// Цвета вкладки по выбору, наведению, фокусу и ячейке. Вызывается из `updateLayer`.
    fn style(&self) {
        let parts = self.ivars();
        let id = parts.id.borrow().clone();
        let Some((theme, state, hover, key_focus)) = parts.owner.call(|model| {
            runloop::mark_draw(model.id);
            (
                model.theme.get(),
                model.state.borrow().clone(),
                model.hover.borrow().clone(),
                model.key_focus.get(),
            )
        }) else {
            return;
        };
        let Some(tab) = state.tabs.iter().find(|tab| tab.id == id) else {
            return;
        };
        let colors = &theme.colors;
        let selected = state.selected.as_deref() == Some(id.as_str());
        let hovered = hover.as_deref() == Some(id.as_str());
        let text = if selected {
            colors.text
        } else if hovered {
            colors.text_2
        } else {
            colors.text_3
        };
        let ink = layer::ns_color(text);
        parts.title.setTextColor(Some(&ink));
        parts.icon.setContentTintColor(Some(&ink));
        parts
            .close
            .setContentTintColor(Some(&layer::ns_color(colors.text_3)));
        let Some(layer) = self.layer() else {
            return;
        };
        without_actions(|| {
            layer.setCornerRadius(theme.metrics.radius.row);
            if selected {
                layer::set_fill(&layer, colors.panel);
            } else if hovered {
                layer::set_fill(&layer, colors.hover);
            } else {
                layer.setBackgroundColor(None);
            }
            parts.ring.setHidden(!selected);
            parts.ring.setCornerRadius(theme.metrics.radius.row + RING);
            let ring = if key_focus {
                theme.mode_color(state.mode)
            } else {
                colors.line
            };
            layer::set_line(&parts.ring, Some((ring, RING)));
            parts.underline.setHidden(!(selected && state.focused));
            layer::set_fill(&parts.underline, theme.mode_color(state.mode));
            parts.underline.setCornerRadius(UNDERLINE_RADIUS);
            style_dot(&parts.dot, parts.dot_slot.get(), tab.status, &theme);
        });
    }
}

/// Точка статуса в месте `slot`: ровная заливка цветом статуса, кольцо «жив», черта «завершён»
/// (прототип).
fn style_dot(dot: &CALayer, slot: Frame, status: TabStatus, theme: &AppTheme) {
    let colors = &theme.colors;
    let (fill, line, height) = match status {
        TabStatus::None => (None, None, layout::DOT),
        TabStatus::Running => (Some(colors.status_run), None, layout::DOT),
        TabStatus::Waiting => (Some(colors.status_warn), None, layout::DOT),
        TabStatus::Error => (Some(colors.status_error), None, layout::DOT),
        TabStatus::Done => (Some(colors.status_ok), None, layout::DOT),
        TabStatus::Idle => (None, Some((colors.text_3, IDLE_RING)), layout::DOT),
        TabStatus::Exited => (Some(colors.text_3), None, EXIT_BAR),
    };
    dot.setHidden(!status.has_dot());
    dot.setFrame(ns_rect(Frame::new(
        slot.x,
        slot.y + ((slot.height - height) / 2.0).max(0.0),
        slot.width,
        height.min(slot.height),
    )));
    match fill {
        Some(color) => layer::set_fill(dot, color),
        None => dot.setBackgroundColor(None),
    }
    layer::set_line(dot, line);
    dot.setCornerRadius(height / 2.0);
}

struct ButtonIvars {
    _live: Live,
}

define_class!(
    // SAFETY: NSButton не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSButton, NSControl, NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioTabStripButton"]
    #[ivars = ButtonIvars]
    struct StripButtonView;

    impl StripButtonView {
        // SAFETY: селектор и результат — как у `acceptsFirstResponder` NSResponder: кнопка
        // получает фокус клавиатуры независимо от Full Keyboard Access.
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            self.isEnabled()
        }

        // SAFETY: селектор и результат — как у `canBecomeKeyView` NSView.
        #[unsafe(method(canBecomeKeyView))]
        fn can_become_key_view(&self) -> bool {
            self.isEnabled() && !self.isHiddenOrHasHiddenAncestor()
        }

        // SAFETY: селектор и результат — как у `mouseDownCanMoveWindow` NSView.
        #[unsafe(method(mouseDownCanMoveWindow))]
        fn mouse_down_can_move_window(&self) -> bool {
            false
        }
    }
);

impl StripButtonView {
    fn new(mtm: MainThreadMarker) -> Retained<StripButtonView> {
        let this = StripButtonView::alloc(mtm).set_ivars(ButtonIvars {
            _live: STRIP_BUTTON.live(),
        });
        // SAFETY: назначенный инициализатор NSButton (через NSControl), ivars уже заданы.
        let button: Retained<StripButtonView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        button.setBordered(false);
        button.setTitle(&NSString::from_str(""));
        button.setImagePosition(NSCellImagePosition::ImageOnly);
        button.setImageScaling(NSImageScaling::ScaleProportionallyDown);
        button
    }

    /// Значок, AX-имя действия, подсказка и клавиша в `accessibilityHelp` (ADR-0032).
    fn describe(&self, button: StripButton, key: Option<&str>) {
        let label = NSString::from_str(button.label());
        self.setImage(symbol(button.symbol(), button.label()).as_deref());
        self.setAccessibilityLabel(Some(&label));
        self.setToolTip(Some(&NSString::from_str(&model::hint(button, key))));
        self.setAccessibilityHelp(Some(&NSString::from_str(&model::help(key))));
    }
}
