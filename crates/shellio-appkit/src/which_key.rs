//! Подсказка which-key — плавающая плитка верхнего слоя главного окна по правилам `.wk` прототипа:
//! по середине окна над строкой статуса, поля 10 и 14, заголовок с плашкой префикса и сетка
//! продолжений по пять строк в столбце. Фокус и клавиши она не забирает: щелчок проходит сквозь
//! неё (`hitTest:` отдаёт `nil`).

use std::cell::{Cell, RefCell};

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSFont, NSFontWeightRegular, NSFontWeightSemibold, NSResponder, NSTextField, NSView,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CALayer;
use shellio_theme::{AppTheme, Shadow};

use crate::app::App;
use crate::chrome_bar::Frame;
use crate::counters::{Live, counter};
use crate::inspect::{self, InspectNode};
use crate::key_badge::{KEY_SIZE, KeyBadge};
use crate::layer::{self, without_actions};
use crate::status_line::key_badges;
use crate::style;
use crate::window::MainWindow;

counter!(WHICH_OWNER, Owner, "WhichKey");
counter!(WHICH_BOX, Class, "ShellioWhichKeyBox");

/// Поля плитки (`.wk { padding: 10px 14px }`) и её линия.
const PAD_X: f64 = 14.0;
const PAD_Y: f64 = 10.0;
const BORDER: f64 = 1.0;
/// Отступ от строки статуса (`bottom: calc(var(--status) + 8px)`) и наибольшая ширина.
const BOTTOM_GAP: f64 = 8.0;
const MAX_WIDTH: f64 = 760.0;
/// Строка заголовка: промежуток между префиксом и названием и отступ до сетки.
const TITLE_GAP: f64 = 6.0;
const TITLE_BOTTOM: f64 = 6.0;
/// Сетка: строк в столбце, промежутки строк и столбцов, промежуток клавиши и подписи.
const GRID_ROWS: usize = 5;
const ROW_GAP: f64 = 3.0;
const COLUMN_GAP: f64 = 22.0;
const ITEM_GAP: f64 = 8.0;
/// Поле соседней плашки одной записи (`kbd + kbd { margin-left: 3px }`). Плашки — отдельные
/// элементы гибкой строки, поэтому между ними ещё и промежуток строки: в заголовке 6, в
/// продолжении 8.
const BADGE_MARGIN: f64 = 3.0;
/// Высота строки текста заголовка и продолжения.
const LINE: f64 = 18.0;

/// Продолжение подсказки: аккорд в записи keymap, подпись и признак группы.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WhichKeyItem {
    pub key: String,
    pub label: String,
    pub group: bool,
}

/// Состав подсказки: набранный префикс, название группы префикса и продолжения.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WhichKeyState {
    /// Набранные аккорды в записи keymap по одному.
    pub prefix: Vec<String>,
    pub title: String,
    pub items: Vec<WhichKeyItem>,
}

/// Ширины частей, из которых считается раскладка.
#[derive(Clone, Debug, PartialEq)]
pub struct WhichKeySpec {
    /// Ширина плашек префикса вместе с промежутками между ними.
    pub prefix: f64,
    pub title: f64,
    /// Ширины плашек клавиши и подписи каждого продолжения.
    pub items: Vec<(f64, f64)>,
}

/// Рамки частей в системе координат содержимого окна, начало сверху слева.
#[derive(Clone, Debug, PartialEq)]
pub struct WhichKeyLayout {
    pub plate: Frame,
    pub prefix: Frame,
    pub title: Frame,
    /// Строка продолжения, плашка клавиши и подпись.
    pub items: Vec<(Frame, Frame, Frame)>,
}

/// Раскладка плитки в окне `width` × `height` над строкой статуса высотой `status`. Сетка с пятью
/// явными строками держит четыре промежутка строк при любом числе продолжений, как CSS grid.
pub fn layout(width: f64, height: f64, status: f64, spec: &WhichKeySpec) -> WhichKeyLayout {
    let columns: Vec<&[(f64, f64)]> = spec.items.chunks(GRID_ROWS).collect();
    let column_widths: Vec<f64> = columns
        .iter()
        .map(|column| {
            column
                .iter()
                .map(|(key, label)| key + ITEM_GAP + label)
                .fold(0.0, f64::max)
        })
        .collect();
    let grid_width = column_widths.iter().sum::<f64>()
        + COLUMN_GAP * column_widths.len().saturating_sub(1) as f64;
    let title_width = spec.prefix + TITLE_GAP + spec.title;
    let inset_x = PAD_X + BORDER;
    let inset_y = PAD_Y + BORDER;
    let plate_width = (grid_width.max(title_width) + 2.0 * inset_x).min(MAX_WIDTH);
    let rows = spec.items.len().min(GRID_ROWS);
    let grid_height = if rows == 0 {
        0.0
    } else {
        rows as f64 * LINE + (GRID_ROWS - 1) as f64 * ROW_GAP
    };
    let plate_height = 2.0 * inset_y + LINE + TITLE_BOTTOM + grid_height;
    let plate = Frame::new(
        (width - plate_width) / 2.0,
        height - status - BOTTOM_GAP - plate_height,
        plate_width,
        plate_height,
    );
    let left = plate.x + inset_x;
    let top = plate.y + inset_y;
    let badge_y = (LINE - KEY_SIZE) / 2.0;
    let prefix = Frame::new(left, top + badge_y, spec.prefix, KEY_SIZE);
    let title = Frame::new(left + spec.prefix + TITLE_GAP, top, spec.title, LINE);
    let grid_top = top + LINE + TITLE_BOTTOM;
    let mut items = Vec::with_capacity(spec.items.len());
    let mut x = left;
    for (column, column_width) in columns.iter().zip(&column_widths) {
        for (row, (key, label)) in column.iter().enumerate() {
            let y = grid_top + row as f64 * (LINE + ROW_GAP);
            items.push((
                Frame::new(x, y, *column_width, LINE),
                Frame::new(x, y + badge_y, *key, KEY_SIZE),
                Frame::new(x + key + ITEM_GAP, y, *label, LINE),
            ));
        }
        x += column_width + COLUMN_GAP;
    }
    WhichKeyLayout {
        plate,
        prefix,
        title,
        items,
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
    #[name = "ShellioWhichKeyBox"]
    #[ivars = BoxIvars]
    struct WhichBox;

    impl WhichBox {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор, аргумент и результат — как у `hitTest:` NSView. Подсказка не
        // принимает щелчков: они уходят виду под ней.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> *mut NSView {
            std::ptr::null_mut()
        }
    }
);

impl WhichBox {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = WhichBox::alloc(mtm).set_ivars(BoxIvars {
            _live: WHICH_BOX.live(),
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<WhichBox> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setWantsLayer(true);
        view
    }
}

/// Плашки одной записи аккордов по правилу `K.kbd` прототипа ([`key_badges`]).
struct Keys {
    badges: Vec<KeyBadge>,
    /// Промежуток соседних плашек.
    gap: f64,
}

impl Keys {
    fn new(mtm: MainThreadMarker, parent: &NSView, chords: &[String], gap: f64) -> Keys {
        let badges = key_badges(chords)
            .iter()
            .map(|text| {
                let badge = KeyBadge::new(mtm, text);
                parent.addSubview(badge.view());
                badge
            })
            .collect();
        Keys { badges, gap }
    }

    fn width(&self) -> f64 {
        self.badges.iter().map(KeyBadge::width).sum::<f64>()
            + self.gap * self.badges.len().saturating_sub(1) as f64
    }

    fn place(&self, x: f64, y: f64) {
        let mut x = x;
        for badge in &self.badges {
            badge.place(x, y);
            x += badge.width() + self.gap;
        }
    }

    fn apply_theme(&self, colors: &shellio_theme::Colors) {
        for badge in &self.badges {
            badge.apply_theme(colors);
        }
    }

    fn remove(&self) {
        for badge in &self.badges {
            badge.view().removeFromSuperview();
        }
    }

    /// Узлы плашек: одна плашка — ключ `ck`, несколько — `ck.1`, `ck.2` и далее, как `K.kbd`.
    fn nodes(&self, ck: &str, parent: &str) -> Vec<InspectNode> {
        let count = self.badges.len();
        self.badges
            .iter()
            .enumerate()
            .map(|(index, badge)| {
                let key = if count == 1 {
                    ck.to_owned()
                } else {
                    format!("{ck}.{}", index + 1)
                };
                badge.node(&key, parent).with_ck(key.clone())
            })
            .collect()
    }
}

/// Строка продолжения: контейнер, плашки клавиши и подпись.
struct ItemView {
    row: Retained<NSView>,
    key: Keys,
    label: Retained<NSTextField>,
    group: bool,
}

/// Подсказка which-key на главном потоке. Один вид на всё время окна: состав меняет
/// [`WhichKey::set_state`], показ — [`WhichKey::show`] и [`WhichKey::hide`].
pub struct WhichKey {
    _live: Live,
    root: Retained<WhichBox>,
    casters: Vec<Retained<NSView>>,
    shadows: Vec<Retained<CALayer>>,
    ring: Retained<CALayer>,
    plate: Retained<WhichBox>,
    prefix: RefCell<Keys>,
    title: Retained<NSTextField>,
    items: RefCell<Vec<ItemView>>,
    state: RefCell<WhichKeyState>,
    theme: Cell<AppTheme>,
    shown: Cell<bool>,
}

impl WhichKey {
    pub fn new(app: &App, theme: &AppTheme) -> Self {
        let mtm = app.mtm();
        let root = WhichBox::new(mtm);
        let (casters, shadows): (Vec<_>, Vec<_>) = theme
            .shadow
            .iter()
            .map(|_| {
                // Тень несёт подслой слоя-хоста, как у палитры: у слоя самого вида AppKit
                // сбрасывает `shadowOpacity` по `NSView.shadow`.
                let shadow = CALayer::new();
                let host = CALayer::new();
                host.addSublayer(&shadow);
                let caster = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
                caster.setLayer(Some(&host));
                caster.setWantsLayer(true);
                root.addSubview(&caster);
                (caster, shadow)
            })
            .unzip();
        let plate = WhichBox::new(mtm);
        root.addSubview(&plate);
        let prefix = Keys::new(mtm, &plate, &[], TITLE_GAP + BADGE_MARGIN);
        let title = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        plate.addSubview(&title);
        root.setHidden(true);
        let which = WhichKey {
            _live: WHICH_OWNER.live(),
            root,
            casters,
            shadows,
            ring: CALayer::new(),
            plate,
            prefix: RefCell::new(prefix),
            title,
            items: RefCell::new(Vec::new()),
            state: RefCell::new(WhichKeyState::default()),
            theme: Cell::new(*theme),
            shown: Cell::new(false),
        };
        which.apply_theme();
        which
    }

    /// Показана ли подсказка.
    pub fn is_shown(&self) -> bool {
        self.shown.get()
    }

    /// Новый состав. Виды строк переиспользуются, лишние снимаются.
    pub fn set_state(&self, state: WhichKeyState) {
        if *self.state.borrow() == state {
            return;
        }
        let mtm = MainThreadMarker::from(&*self.root);
        {
            let mut prefix = self.prefix.borrow_mut();
            prefix.remove();
            *prefix = Keys::new(mtm, &self.plate, &state.prefix, TITLE_GAP + BADGE_MARGIN);
        }
        self.title.setStringValue(&NSString::from_str(&state.title));
        {
            let mut items = self.items.borrow_mut();
            for item in items.drain(..) {
                item.row.removeFromSuperview();
            }
            for spec in &state.items {
                let row = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
                let key = Keys::new(
                    mtm,
                    &row,
                    std::slice::from_ref(&spec.key),
                    ITEM_GAP + BADGE_MARGIN,
                );
                let label = NSTextField::labelWithString(&NSString::from_str(&spec.label), mtm);
                row.addSubview(&label);
                self.plate.addSubview(&row);
                items.push(ItemView {
                    row,
                    key,
                    label,
                    group: spec.group,
                });
            }
        }
        *self.state.borrow_mut() = state;
        self.apply_theme();
    }

    pub fn set_theme(&self, theme: &AppTheme) {
        self.theme.set(*theme);
        self.apply_theme();
    }

    /// Ставит подсказку верхним слоем окна `window` по размеру его содержимого.
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

    fn apply_theme(&self) {
        let theme = self.theme.get();
        let colors = &theme.colors;
        let radius = theme.metrics.radius.float;
        without_actions(|| {
            for (shadow, token) in self.shadows.iter().zip(theme.shadow) {
                layer::set_fill(shadow, colors.surface);
                layer::set_corner(shadow, radius);
                layer::set_shadow(shadow, down(token));
            }
            if let Some(plate) = self.plate.layer() {
                layer::set_fill(&plate, colors.surface);
                layer::set_line(&plate, Some((colors.line_2, BORDER)));
                layer::set_radius(&plate, radius);
            }
        });
        // SAFETY: веса шрифта — неизменяемые константы AppKit.
        let (regular, semibold) = unsafe { (NSFontWeightRegular, NSFontWeightSemibold) };
        self.prefix.borrow().apply_theme(colors);
        self.title.setFont(Some(&NSFont::systemFontOfSize_weight(
            theme.metrics.font.small,
            semibold,
        )));
        self.title
            .setTextColor(Some(&layer::ns_color(colors.text_2)));
        let body = NSFont::systemFontOfSize_weight(theme.metrics.font.body, regular);
        for item in self.items.borrow().iter() {
            item.key.apply_theme(colors);
            item.label.setFont(Some(&body));
            let color = if item.group {
                colors.accent_nav_line
            } else {
                colors.text
            };
            item.label.setTextColor(Some(&layer::ns_color(color)));
        }
    }

    fn place(&self, window: &MainWindow) {
        let (width, height) = window.content_size();
        let theme = self.theme.get();
        let typeset = |field: &NSTextField| {
            field.font().map_or(0.0, |font| {
                style::typeset_width(&field.stringValue().to_string(), &font)
            })
        };
        let items = self.items.borrow();
        let spec = WhichKeySpec {
            prefix: self.prefix.borrow().width(),
            title: typeset(&self.title),
            items: items
                .iter()
                .map(|item| (item.key.width(), typeset(&item.label)))
                .collect(),
        };
        let frames = layout(width, height, theme.metrics.status_bar, &spec);
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
            for caster in &self.casters {
                caster.setFrame(plate.rect());
            }
            for shadow in &self.shadows {
                shadow.setFrame(NSRect::new(
                    NSPoint::ZERO,
                    NSSize::new(plate.width, plate.height),
                ));
            }
            let prefix = local(frames.prefix);
            self.prefix.borrow().place(prefix.x, prefix.y);
            place_label(&self.title, local(frames.title));
            for (item, (row, key, label)) in items.iter().zip(&frames.items) {
                let row_local = local(*row);
                item.row.setFrame(row_local.rect());
                item.key.place(key.x - row.x, key.y - row.y);
                place_label(
                    &item.label,
                    Frame::new(label.x - row.x, label.y - row.y, label.width, label.height),
                );
            }
        });
    }

    /// Узлы подсказки для `ui.inspect` с ключами сверки; пусто, пока она скрыта.
    pub fn inspect(&self) -> Vec<InspectNode> {
        if !self.shown.get() {
            return Vec::new();
        }
        let mut nodes = Vec::new();
        let mut plate = inspect::node("whichkey", "WhichKey", None, &self.plate, None, None)
            .with_ck("whichkey")
            .with_shadow_layers(&self.shadows, &self.ring);
        // Слои хранят сдвиг с обратным знаком (`down`): чтение отдаёт его, как в CSS.
        if let Some(style) = plate.style.as_mut() {
            for shadow in &mut style.shadow {
                if shadow.y != 0.0 {
                    shadow.y = -shadow.y;
                }
            }
        }
        nodes.push(plate);
        nodes.extend(self.prefix.borrow().nodes("whichkey.prefix", "whichkey"));
        nodes.push(
            inspect::typeset_node("whichkey.title", "Label", "whichkey", &self.title)
                .with_ck("whichkey.title"),
        );
        for (index, item) in self.items.borrow().iter().enumerate() {
            let ck = format!("whichkey.item.{}", index + 1);
            nodes.push(
                inspect::node(&ck, "WhichKeyItem", Some("whichkey"), &item.row, None, None)
                    .with_ck(ck.clone()),
            );
            nodes.extend(item.key.nodes(&format!("{ck}.key"), &ck));
            nodes.push(
                inspect::typeset_node(&format!("{ck}.label"), "Label", &ck, &item.label)
                    .with_ck(format!("{ck}.label")),
            );
        }
        nodes
    }
}

impl Drop for WhichKey {
    fn drop(&mut self) {
        self.root.removeFromSuperview();
    }
}

/// Сдвиг тени CSS вниз ставится с обратным знаком, как у палитры.
fn down(token: Shadow) -> Shadow {
    Shadow {
        y: -token.y,
        ..token
    }
}

fn place_label(field: &NSTextField, frame: Frame) {
    field.setFrame(
        Frame::new(
            frame.x - style::LABEL_INSET,
            frame.y,
            frame.width + 2.0 * style::LABEL_INSET,
            frame.height,
        )
        .rect(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Числа паспорта `whichkey-leader` (окно 1180 × 720, строка статуса 26).
    #[test]
    fn leader_layout_matches_passport() {
        let spec = WhichKeySpec {
            prefix: 41.5,
            title: 56.828125,
            items: vec![(41.5, 110.6875), (17.0, 124.132812), (17.0, 32.734375)],
        };
        let frames = layout(1180.0, 720.0, 26.0, &spec);
        assert_eq!(frames.plate, Frame::new(494.90625, 574.0, 190.1875, 112.0));
        assert_eq!(frames.prefix, Frame::new(509.90625, 585.5, 41.5, 17.0));
        assert_eq!(frames.title.x, 557.40625);
        assert_eq!(frames.title.y, 585.0);
        let (row, key, label) = frames.items[1];
        assert_eq!(row, Frame::new(509.90625, 630.0, 160.1875, 18.0));
        assert_eq!(key, Frame::new(509.90625, 630.5, 17.0, 17.0));
        assert_eq!(label.x, 534.90625);
    }

    /// Числа паспорта `whichkey-ctrlw`: три столбца, пять строк.
    #[test]
    fn ctrlw_layout_matches_passport() {
        let labels = [
            79.5625, 85.296875, 72.25, 85.21875, 79.054688, 86.398438, 87.546875, 93.21875,
            185.429688, 249.828125, 100.109375, 115.109375, 128.546875, 119.84375,
        ];
        let spec = WhichKeySpec {
            prefix: 35.203125 + TITLE_GAP + BADGE_MARGIN + 17.0,
            title: 90.96875,
            items: labels.iter().map(|label| (17.0, *label)).collect(),
        };
        let frames = layout(1180.0, 720.0, 26.0, &spec);
        assert_eq!(frames.plate.height, 148.0);
        assert!((frames.plate.width - 612.671875).abs() < 1e-3);
        assert!((frames.plate.x - 283.664062).abs() < 1e-3);
        assert_eq!(frames.plate.y, 538.0);
        assert!((frames.title.x - 365.867188).abs() < 1e-3);
        assert!((frames.items[10].1.x - 727.789062).abs() < 1e-3);
        assert_eq!(frames.items[10].1.y, 573.5);
    }

    #[test]
    fn sixth_item_starts_a_column() {
        let spec = WhichKeySpec {
            prefix: 17.0,
            title: 0.0,
            items: vec![(17.0, 40.0); 6],
        };
        let frames = layout(1000.0, 600.0, 26.0, &spec);
        let first = frames.items[0].0;
        let sixth = frames.items[5].0;
        assert_eq!(sixth.y, first.y);
        assert_eq!(sixth.x, first.x + first.width + COLUMN_GAP);
        assert_eq!(frames.plate.width, 2.0 * 65.0 + COLUMN_GAP + 30.0);
    }
}
