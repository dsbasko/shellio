//! Метки перехода `f` — плашки верхнего слоя главного окна по правилу `.hint` прототипа: у левого
//! верхнего угла цели, высота 16, наименьшая ширина 16, поля 3, радиус 4, жирный моноширинный
//! кегль 11 цветом поверхности на заливке `warn`. Набранная часть метки приглушена долей 0,4.
//! Фокус и щелчки слой не забирает (`hitTest:` отдаёт `nil`): клавиши меток получает
//! маршрутизатор.

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSResponder, NSTextField, NSView};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CALayer;
use shellio_theme::{AppTheme, HINT_SHADOW, Shadow, Tint};

use crate::app::App;
use crate::chrome_bar::Frame;
use crate::counters::{Live, counter};
use crate::font;
use crate::inspect::{self, InspectNode};
use crate::layer::{self, without_actions};
use crate::style;
use crate::window::MainWindow;

counter!(HINT_OWNER, Owner, "HintOverlay");
counter!(HINT_BOX, Class, "ShellioHintBox");

/// Высота и наименьшая ширина плашки, её поля по сторонам и радиус.
const HEIGHT: f64 = 16.0;
const MIN_WIDTH: f64 = 16.0;
const PAD_X: f64 = 3.0;
const RADIUS: f64 = 4.0;
/// Кегль и начертание по шкале CSS (`font: 700 11px/16px var(--mono)`).
const FONT_SIZE: f64 = 11.0;
const FONT_WEIGHT: u16 = 700;
/// Наименьший отступ плашки от края окна и сдвиг от угла цели (`Math.max(2, r.left + 2)`).
const EDGE: f64 = 2.0;
const INSET_X: f64 = 2.0;
/// Наибольший сдвиг вниз от верха цели (`Math.min(4, r.height / 2 - 8)`).
const INSET_Y: f64 = 4.0;
/// Доля набранной части (`.hint em { opacity: .4 }`).
const TYPED_ALPHA: f64 = 0.4;

/// Метка на экране: исходный номер цели (с единицы, при сужении не меняется), полный текст метки
/// и кадр цели в точках окна с началом внизу слева, как в `ui.inspect`.
#[derive(Clone, Debug, PartialEq)]
pub struct HintItem {
    pub number: usize,
    pub label: String,
    pub target: Frame,
}

/// Состав слоя: набранная часть и метки, которые с неё начинаются.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HintOverlayState {
    pub typed: String,
    pub items: Vec<HintItem>,
}

/// Рамки одной метки в системе координат содержимого окна, начало сверху слева: плашка, набранная
/// часть и остаток (кадры набора без полей подписи).
#[derive(Clone, Debug, PartialEq)]
pub struct HintFrames {
    pub plate: Frame,
    pub typed: Frame,
    pub remaining: Frame,
}

/// Раскладка метки цели `target` (начало сверху слева) по ширинам набранной части и остатка и
/// высоте строки набора: плашка у левого верхнего угла цели, текст по её середине.
pub fn layout(target: Frame, typed: f64, remaining: f64, text_height: f64) -> HintFrames {
    let text = typed + remaining;
    let width = (text + 2.0 * PAD_X).max(MIN_WIDTH);
    let plate = Frame::new(
        (target.x + INSET_X).max(EDGE),
        (target.y + INSET_Y.min(target.height / 2.0 - HEIGHT / 2.0)).max(EDGE),
        width,
        HEIGHT,
    );
    let left = plate.x + (width - text) / 2.0;
    let top = plate.y + (HEIGHT - text_height) / 2.0;
    HintFrames {
        plate,
        typed: Frame::new(left, top, typed, text_height),
        remaining: Frame::new(left + typed, top, remaining, text_height),
    }
}

struct BoxIvars {
    _live: Live,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioHintBox"]
    #[ivars = BoxIvars]
    struct HintBox;

    impl HintBox {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор, аргумент и результат — как у `hitTest:` NSView. Слой меток не
        // принимает щелчков: они уходят виду под ним.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> *mut NSView {
            std::ptr::null_mut()
        }
    }
);

impl HintBox {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = HintBox::alloc(mtm).set_ivars(BoxIvars {
            _live: HINT_BOX.live(),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<HintBox> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setWantsLayer(true);
        view
    }
}

/// Виды одной метки. Переиспользуются между составами: лишние скрываются, а не снимаются.
struct LabelView {
    /// Вид-хост тени: тень несёт подслой его слоя, как у палитры и which-key, — у слоя самого
    /// вида AppKit сбрасывает `shadowOpacity` по `NSView.shadow`.
    caster: Retained<NSView>,
    shadow: Retained<CALayer>,
    plate: Retained<HintBox>,
    typed: Retained<NSTextField>,
    remaining: Retained<NSTextField>,
}

impl LabelView {
    fn new(mtm: MainThreadMarker, root: &NSView) -> Self {
        let shadow = CALayer::new();
        let host = CALayer::new();
        host.addSublayer(&shadow);
        let caster = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        caster.setLayer(Some(&host));
        caster.setWantsLayer(true);
        root.addSubview(&caster);
        let plate = HintBox::new(mtm);
        root.addSubview(&plate);
        let typed = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        let remaining = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        plate.addSubview(&typed);
        plate.addSubview(&remaining);
        LabelView {
            caster,
            shadow,
            plate,
            typed,
            remaining,
        }
    }

    fn set_hidden(&self, hidden: bool) {
        self.caster.setHidden(hidden);
        self.plate.setHidden(hidden);
    }

    fn apply_theme(&self, theme: &AppTheme) {
        let colors = &theme.colors;
        without_actions(|| {
            layer::set_fill(&self.shadow, colors.status_warn);
            layer::set_corner(&self.shadow, RADIUS);
            layer::set_shadow(&self.shadow, down(HINT_SHADOW));
            if let Some(plate) = self.plate.layer() {
                layer::set_fill(&plate, colors.status_warn);
                layer::set_radius(&plate, RADIUS);
            }
        });
        let bold = font::mono_weight(FONT_SIZE, FONT_WEIGHT);
        self.typed.setFont(Some(&bold));
        self.remaining.setFont(Some(&bold));
        self.typed.setTextColor(Some(&layer::ns_tint(Tint::new(
            colors.surface,
            TYPED_ALPHA,
        ))));
        self.remaining
            .setTextColor(Some(&layer::ns_color(colors.surface)));
    }
}

/// Слой меток на главном потоке. Один вид на всё время окна: состав меняет
/// [`HintOverlay::set_state`], показ — [`HintOverlay::show`] и [`HintOverlay::hide`].
pub struct HintOverlay {
    _live: Live,
    root: Retained<HintBox>,
    labels: RefCell<Vec<LabelView>>,
    /// Пустой слой кольца для чтения теней: у метки кольца нет.
    ring: Retained<CALayer>,
    state: RefCell<HintOverlayState>,
    theme: Cell<AppTheme>,
    shown: Cell<bool>,
}

impl HintOverlay {
    pub fn new(app: &App, theme: &AppTheme) -> Self {
        let root = HintBox::new(app.mtm());
        root.setHidden(true);
        HintOverlay {
            _live: HINT_OWNER.live(),
            root,
            labels: RefCell::new(Vec::new()),
            ring: CALayer::new(),
            state: RefCell::new(HintOverlayState::default()),
            theme: Cell::new(*theme),
            shown: Cell::new(false),
        }
    }

    /// Показан ли слой.
    pub fn is_shown(&self) -> bool {
        self.shown.get()
    }

    /// Новый состав. Виды меток переиспользуются: недостающие создаются, лишние скрываются.
    pub fn set_state(&self, state: HintOverlayState) {
        if *self.state.borrow() == state {
            return;
        }
        let mtm = MainThreadMarker::from(&*self.root);
        let theme = self.theme.get();
        let mut labels = self.labels.borrow_mut();
        while labels.len() < state.items.len() {
            let view = LabelView::new(mtm, &self.root);
            view.apply_theme(&theme);
            labels.push(view);
        }
        for (index, view) in labels.iter().enumerate() {
            match state.items.get(index) {
                Some(item) => {
                    let rest = item.label.strip_prefix(&state.typed).unwrap_or(&item.label);
                    view.typed.setStringValue(&NSString::from_str(&state.typed));
                    view.typed.setHidden(state.typed.is_empty());
                    view.remaining.setStringValue(&NSString::from_str(rest));
                    view.set_hidden(false);
                }
                None => view.set_hidden(true),
            }
        }
        drop(labels);
        *self.state.borrow_mut() = state;
    }

    pub fn set_theme(&self, theme: &AppTheme) {
        self.theme.set(*theme);
        for view in self.labels.borrow().iter() {
            view.apply_theme(theme);
        }
    }

    /// Ставит слой верхним видом окна `window` по размеру его содержимого.
    pub fn show(&self, window: &MainWindow) {
        let root = window.root_view();
        let ours: &NSView = &self.root;
        let on_top = root
            .subviews()
            .lastObject()
            .is_some_and(|top| std::ptr::eq(&*top, ours));
        // Повторное добавление поднимает вид над видами, поставленными после него.
        if !on_top {
            root.addSubview(ours);
        }
        self.root.setHidden(false);
        self.shown.set(true);
        self.place(window);
    }

    pub fn hide(&self) {
        self.root.setHidden(true);
        self.shown.set(false);
    }

    fn place(&self, window: &MainWindow) {
        let (width, height) = window.content_size();
        let state = self.state.borrow();
        let labels = self.labels.borrow();
        without_actions(|| {
            self.root
                .setFrame(Frame::new(0.0, 0.0, width, height).rect());
            for (item, view) in state.items.iter().zip(labels.iter()) {
                let target = Frame::new(
                    item.target.x,
                    height - item.target.y - item.target.height,
                    item.target.width,
                    item.target.height,
                );
                let typed = typeset(&view.typed);
                let remaining = typeset(&view.remaining);
                let text_height = view.remaining.intrinsicContentSize().height;
                let frames = layout(target, typed, remaining, text_height);
                let plate = frames.plate;
                view.plate.setFrame(plate.rect());
                view.caster.setFrame(plate.rect());
                view.shadow.setFrame(NSRect::new(
                    NSPoint::ZERO,
                    NSSize::new(plate.width, plate.height),
                ));
                let local = |frame: Frame| {
                    Frame::new(
                        frame.x - plate.x - style::LABEL_INSET,
                        frame.y - plate.y,
                        frame.width + 2.0 * style::LABEL_INSET,
                        frame.height,
                    )
                };
                view.typed.setFrame(local(frames.typed).rect());
                view.remaining.setFrame(local(frames.remaining).rect());
            }
        });
    }

    /// Узлы меток для `ui.inspect` с ключами сверки `hints.item.{номер цели}`; набранная часть —
    /// `.typed`, только если что-то набрано, остаток — `.remaining`. Пусто, пока слой скрыт.
    pub fn inspect(&self) -> Vec<InspectNode> {
        if !self.shown.get() {
            return Vec::new();
        }
        let state = self.state.borrow();
        let labels = self.labels.borrow();
        let mut nodes = Vec::new();
        for (item, view) in state.items.iter().zip(labels.iter()) {
            let ck = format!("hints.item.{}", item.number);
            let mut plate = inspect::node(&ck, "HintLabel", None, &view.plate, None, None)
                .with_ck(ck.clone())
                .with_shadow_layers(std::slice::from_ref(&view.shadow), &self.ring);
            // Слой хранит сдвиг с обратным знаком (`down`): чтение отдаёт его, как в CSS.
            if let Some(style) = plate.style.as_mut() {
                for shadow in &mut style.shadow {
                    if shadow.y != 0.0 {
                        shadow.y = -shadow.y;
                    }
                }
            }
            nodes.push(plate);
            if !state.typed.is_empty() {
                let id = format!("{ck}.typed");
                nodes.push(inspect::typeset_node(&id, "Label", &ck, &view.typed).with_ck(id));
            }
            let id = format!("{ck}.remaining");
            nodes.push(inspect::typeset_node(&id, "Label", &ck, &view.remaining).with_ck(id));
        }
        nodes
    }
}

impl Drop for HintOverlay {
    fn drop(&mut self) {
        self.root.removeFromSuperview();
    }
}

/// Ширина набора подписи.
fn typeset(field: &NSTextField) -> f64 {
    field.font().map_or(0.0, |font| {
        style::typeset_width(&field.stringValue().to_string(), &font)
    })
}

/// Сдвиг тени CSS вниз ставится с обратным знаком, как у палитры и which-key.
fn down(token: Shadow) -> Shadow {
    Shadow {
        y: -token.y,
        ..token
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ширина знака JetBrains Mono Bold 11 в паспортах `hints-*`.
    const GLYPH: f64 = 6.601562;

    /// Числа паспорта `hints-partial`, метка 10: набрано `s`, остаток `a`.
    #[test]
    fn partial_label_matches_passport() {
        let frames = layout(Frame::new(11.0, 50.5, 100.0, 30.0), GLYPH, GLYPH, 14.5);
        assert_eq!(frames.plate.x, 13.0);
        assert_eq!(frames.plate.y, 54.5);
        assert!((frames.plate.width - 19.203125).abs() < 1e-3);
        assert_eq!(frames.plate.height, HEIGHT);
        assert_eq!(frames.typed.x, 16.0);
        assert!((frames.remaining.x - 22.601562).abs() < 1e-6);
        assert!((frames.typed.y - 55.0).abs() <= 0.25);
    }

    /// `hints-full`: ничего не набрано, остаток из двух знаков стоит у поля 3.
    #[test]
    fn full_label_starts_text_at_the_padding() {
        let frames = layout(Frame::new(14.0, 12.5, 14.0, 14.0), 0.0, 2.0 * GLYPH, 14.5);
        assert_eq!(frames.plate.x, 16.0);
        assert_eq!(frames.plate.y, 11.5);
        assert_eq!(frames.remaining.x, 19.0);
        assert_eq!(frames.typed.width, 0.0);
    }

    /// Одна буква: плашка держит наименьшую ширину 16, текст по её середине.
    #[test]
    fn single_glyph_keeps_the_minimum_width_and_centres_the_text() {
        let frames = layout(Frame::new(100.0, 100.0, 40.0, 26.0), 0.0, GLYPH, 14.5);
        assert_eq!(frames.plate.width, MIN_WIDTH);
        assert!((frames.remaining.x - (102.0 + (MIN_WIDTH - GLYPH) / 2.0)).abs() < 1e-9);
    }

    /// Цель у края окна: плашка не выходит за отступ 2, низкая цель сдвигает плашку вверх.
    #[test]
    fn label_stays_inside_the_window_edge() {
        let frames = layout(Frame::new(-10.0, 0.0, 20.0, 4.0), 0.0, GLYPH, 14.5);
        assert_eq!(frames.plate.x, EDGE);
        assert_eq!(frames.plate.y, EDGE);
        let tall = layout(Frame::new(50.0, 200.0, 60.0, 60.0), 0.0, GLYPH, 14.5);
        assert_eq!(tall.plate.y, 204.0);
        let short = layout(Frame::new(50.0, 200.0, 60.0, 10.0), 0.0, GLYPH, 14.5);
        assert_eq!(short.plate.y, 197.0);
    }
}
