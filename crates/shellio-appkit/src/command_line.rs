//! Командная строка `:` — плашка верхнего слоя главного окна по правилам `.cmdline` прототипа:
//! внизу слева на отступе `--gap`, высотой строки статуса и шириной 420, заливка `surface`, кольцо
//! `0 0 0 1px` цвета `line-2`, поля 0 и 8, знак `:` и поле ввода моноширинным мелким кеглем.
//! Поле получает фокус в том же окне: окно владельца остаётся ключевым. Щелчок мимо плашки
//! закрывает строку и до вида под ней не доходит: решает монитор окна по [`CommandLine::on_plate`].
//! Нажатие на плашку остаётся у неё. Слой на всё окно сам событий не принимает, поэтому прокрутка
//! проходит виду под ним.

use std::cell::Cell;

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSEvent, NSResponder, NSTextField, NSView};
use objc2_foundation::{NSPoint, NSRect, NSString};
use objc2_quartz_core::CALayer;
use shellio_theme::AppTheme;

use crate::app::App;
use crate::chrome_bar::Frame;
use crate::components::text_field::{TextField, TextFieldIntent, TextFieldState};
use crate::counters::{Live, counter};
use crate::font;
use crate::inspect::{self, InspectNode};
use crate::layer::{self, without_actions};
use crate::style;
use crate::surface::LINE_WIDTH;
use crate::window::MainWindow;

counter!(CMDLINE_OWNER, Owner, "CommandLine");
counter!(CMDLINE_BOX, Class, "ShellioCommandLineBox");

/// Ширина плашки (`.cmdline { width: 420px }`).
const WIDTH: f64 = 420.0;
/// Поле плашки по горизонтали (`padding: 0 8px`) и промежуток знака и поля (`gap: 4px`).
const PAD_X: f64 = 8.0;
const GAP: f64 = 4.0;
/// Высота строки знака `:` и поля ввода моноширинным кеглем 12 (паспорт `cmdline`).
const PROMPT_LINE: f64 = 15.5;
const INPUT_LINE: f64 = 17.5;
/// Знак строки и плейсхолдер поля прототипа.
const PROMPT: &str = ":";
pub const CMDLINE_PLACEHOLDER: &str = "Команда: zoom, split, theme light, q";

/// Рамки частей в системе координат содержимого окна, начало сверху слева.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CommandLineLayout {
    pub plate: Frame,
    pub prompt: Frame,
    pub input: Frame,
}

/// Раскладка плашки в окне высотой `height`: строка статуса высотой `status`, отступ `gap`,
/// ширина знака `prompt`. Плашка накрывает левую часть строки статуса, как `position: fixed`
/// прототипа.
pub fn layout(height: f64, status: f64, gap: f64, prompt: f64) -> CommandLineLayout {
    let plate = Frame::new(gap, height - gap - status, WIDTH, status);
    let prompt_x = plate.x + PAD_X;
    let input_x = prompt_x + prompt + GAP;
    CommandLineLayout {
        plate,
        prompt: Frame::new(
            prompt_x,
            plate.y + (status - PROMPT_LINE) / 2.0,
            prompt,
            PROMPT_LINE,
        ),
        input: Frame::new(
            input_x,
            plate.y + (status - INPUT_LINE) / 2.0,
            (plate.x + plate.width - PAD_X - input_x).max(0.0),
            INPUT_LINE,
        ),
    }
}

/// Точка `(x, y)` плашки `width` × `height` в её координатах лежит на плашке или её кольце.
fn on_ring(x: f64, y: f64, width: f64, height: f64) -> bool {
    (-LINE_WIDTH..=width + LINE_WIDTH).contains(&x)
        && (-LINE_WIDTH..=height + LINE_WIDTH).contains(&y)
}

struct BoxIvars {
    _live: Live,
    /// Слой на всё окно пропускает события мимо своих подвидов, плашка принимает свои.
    pass_through: bool,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioCommandLineBox"]
    #[ivars = BoxIvars]
    struct CommandBox;

    impl CommandBox {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор, аргумент и результат — как у `hitTest:` NSView. Слой на всё окно
        // принимает события только своими подвидами: мимо плашки событие уходит виду под ней.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, point: NSPoint) -> *mut NSView {
            // SAFETY: `hitTest:` у NSView с той же точкой.
            let hit: *mut NSView = unsafe { msg_send![super(self), hitTest: point] };
            let ours: *const NSView = std::ptr::from_ref::<NSView>(self);
            if self.ivars().pass_through && std::ptr::eq(hit.cast_const(), ours) {
                std::ptr::null_mut()
            } else {
                hit
            }
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder. Нажатие на плашку
        // остаётся у неё: по цепочке ответчиков оно не идёт.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {}
    }
);

impl CommandBox {
    fn new(mtm: MainThreadMarker, pass_through: bool) -> Retained<Self> {
        let this = CommandBox::alloc(mtm).set_ivars(BoxIvars {
            _live: CMDLINE_BOX.live(),
            pass_through,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<CommandBox> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setWantsLayer(true);
        view
    }
}

/// Командная строка на главном потоке. Один вид на всё время окна: текст меняет
/// [`CommandLine::set_text`], показ — [`CommandLine::show`] и [`CommandLine::hide`].
pub struct CommandLine {
    _live: Live,
    root: Retained<CommandBox>,
    plate: Retained<CommandBox>,
    ring: Retained<CALayer>,
    prompt: Retained<NSTextField>,
    field: TextField,
    theme: Cell<AppTheme>,
    shown: Cell<bool>,
}

impl CommandLine {
    /// `on_intent` получает намерения поля на главном потоке изнутри AppKit и не должен
    /// синхронно трогать окно.
    pub fn new(app: &App, theme: &AppTheme, on_intent: impl Fn(TextFieldIntent) + 'static) -> Self {
        let mtm = app.mtm();
        let root = CommandBox::new(mtm, true);
        let plate = CommandBox::new(mtm, false);
        let ring = CALayer::new();
        // Кольцо снаружи кадра плашки, под её содержимым.
        ring.setZPosition(-1.0);
        if let Some(host) = plate.layer() {
            host.addSublayer(&ring);
        }
        root.addSubview(&plate);
        let prompt = NSTextField::labelWithString(&NSString::from_str(PROMPT), mtm);
        plate.addSubview(&prompt);
        let field = TextField::command(app, theme, "", CMDLINE_PLACEHOLDER, on_intent);
        plate.addSubview(field.ns_view());
        root.setHidden(true);
        let line = CommandLine {
            _live: CMDLINE_OWNER.live(),
            root,
            plate,
            ring,
            prompt,
            field,
            theme: Cell::new(*theme),
            shown: Cell::new(false),
        };
        line.apply_theme();
        line
    }

    /// Показана ли строка.
    pub fn is_shown(&self) -> bool {
        self.shown.get()
    }

    /// Идёт ли ввод в поле строки.
    pub fn is_editing(&self) -> bool {
        self.field.is_editing()
    }

    /// Текст поля, включая ввод пользователя.
    pub fn text(&self) -> String {
        self.field.state().value
    }

    /// Новый текст поля; тот же текст вид не трогает.
    pub fn set_text(&self, text: &str) {
        self.field.set_state(TextFieldState {
            value: text.to_owned(),
            placeholder: CMDLINE_PLACEHOLDER.to_owned(),
            editable: true,
        });
    }

    pub fn set_theme(&self, theme: &AppTheme) {
        self.theme.set(*theme);
        self.field.set_theme(theme);
        self.apply_theme();
    }

    /// Ставит строку верхним слоем окна `window` по размеру его содержимого. Первый показ
    /// передаёт фокус полю: `false` — окно его не приняло.
    pub fn show(&self, window: &MainWindow) -> bool {
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
        self.place(window);
        if self.shown.replace(true) {
            return true;
        }
        self.root.setHidden(false);
        self.field.focus()
    }

    /// Лежит ли точка окна `location` (начало внизу слева, как `locationInWindow`) на видимой
    /// плашке вместе с её кольцом.
    pub fn on_plate(&self, location: (f64, f64)) -> bool {
        if !self.shown.get() {
            return false;
        }
        let (x, y) = location;
        let point = self.plate.convertPoint_fromView(NSPoint::new(x, y), None);
        on_ring(
            point.x,
            point.y,
            self.plate.bounds().size.width,
            self.plate.bounds().size.height,
        )
    }

    /// Прячет строку: ввод в поле отдаётся окну, текст забывается.
    pub fn hide(&self) {
        self.field.resign_focus();
        self.set_text("");
        self.root.setHidden(true);
        self.shown.set(false);
    }

    fn apply_theme(&self) {
        let theme = self.theme.get();
        let colors = &theme.colors;
        let radius = theme.metrics.radius.control;
        without_actions(|| {
            if let Some(plate) = self.plate.layer() {
                layer::set_fill(&plate, colors.surface);
                layer::set_line(&plate, None);
                // Без обрезки: кольцо — подслой снаружи кадра плашки.
                layer::set_corner(&plate, radius);
            }
            layer::set_line(&self.ring, Some((colors.line_2, LINE_WIDTH)));
            layer::set_corner(&self.ring, radius + LINE_WIDTH);
        });
        self.prompt
            .setFont(Some(&font::mono(theme.metrics.font.small)));
        self.prompt
            .setTextColor(Some(&layer::ns_color(colors.text)));
    }

    fn place(&self, window: &MainWindow) {
        let (width, height) = window.content_size();
        let theme = self.theme.get();
        let prompt = self.prompt.font().map_or(0.0, |font| {
            style::typeset_width(&self.prompt.stringValue().to_string(), &font)
        });
        let frames = layout(height, theme.metrics.status_bar, theme.metrics.gap, prompt);
        let plate = frames.plate;
        let local = |frame: Frame| {
            Frame::new(
                frame.x - plate.x,
                frame.y - plate.y,
                frame.width,
                frame.height,
            )
        };
        without_actions(|| {
            self.root
                .setFrame(Frame::new(0.0, 0.0, width, height).rect());
            self.plate.setFrame(plate.rect());
            self.ring.setFrame(
                Frame::new(
                    -LINE_WIDTH,
                    -LINE_WIDTH,
                    plate.width + 2.0 * LINE_WIDTH,
                    plate.height + 2.0 * LINE_WIDTH,
                )
                .rect(),
            );
            let prompt = local(frames.prompt);
            self.prompt.setFrame(
                Frame::new(
                    prompt.x - style::LABEL_INSET,
                    prompt.y,
                    prompt.width + 2.0 * style::LABEL_INSET,
                    prompt.height,
                )
                .rect(),
            );
            self.field.ns_view().setFrame(local(frames.input).rect());
        });
    }

    /// Узлы строки для `ui.inspect` с ключами сверки; пусто, пока она скрыта.
    pub fn inspect(&self) -> Vec<InspectNode> {
        if !self.shown.get() {
            return Vec::new();
        }
        let mut nodes = vec![
            inspect::node("cmdline", "CommandLine", None, &self.plate, None, None)
                .with_ck("cmdline")
                .with_ring_layer(&self.ring),
            inspect::typeset_node("cmdline.prompt", "Label", "cmdline", &self.prompt)
                .with_ck("cmdline.prompt"),
        ];
        nodes.extend(
            self.field
                .inspect("cmdline.input", Some("cmdline"))
                .into_iter()
                .map(|node| node.with_ck("cmdline.input")),
        );
        nodes
    }
}

impl Drop for CommandLine {
    fn drop(&mut self) {
        self.root.removeFromSuperview();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Числа паспорта `cmdline/light-normal` (окно 1180 × 720, строка статуса 26, отступ 6).
    #[test]
    fn layout_matches_passport() {
        let frames = layout(720.0, 26.0, 6.0, 7.203125);
        assert_eq!(frames.plate, Frame::new(6.0, 688.0, 420.0, 26.0));
        assert_eq!(frames.prompt, Frame::new(14.0, 693.25, 7.203125, 15.5));
        assert_eq!(frames.input.x, 25.203125);
        assert_eq!(frames.input.y, 692.25);
        assert_eq!(frames.input.height, 17.5);
        assert!((frames.input.width - 392.797).abs() < 1e-3);
    }

    /// Плашка держится у нижнего края при любой высоте окна.
    #[test]
    fn plate_follows_the_bottom_edge() {
        let frames = layout(500.0, 26.0, 6.0, 7.0);
        assert_eq!(frames.plate.y, 468.0);
        assert_eq!(frames.plate.x, 6.0);
    }

    /// Кольцо в точку снаружи кадра — часть видимой плашки, дальше — мимо неё.
    #[test]
    fn ring_belongs_to_the_plate() {
        assert!(on_ring(0.0, 0.0, 420.0, 26.0));
        assert!(on_ring(-1.0, 27.0, 420.0, 26.0));
        assert!(on_ring(421.0, 13.0, 420.0, 26.0));
        assert!(!on_ring(-1.5, 13.0, 420.0, 26.0));
        assert!(!on_ring(200.0, 27.5, 420.0, 26.0));
        assert!(!on_ring(422.0, -2.0, 420.0, 26.0));
    }
}
