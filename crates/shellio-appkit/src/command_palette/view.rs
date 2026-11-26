//! Вид палитры на главном потоке: окно `NSPanel` без рамки и системной тени над окном владельца.
//! Окно прозрачно только в поле тени вокруг непрозрачной плашки (ADR-0023, «Уточнено в P0.2»):
//! поле — наибольшие размытие и смещение тени токена. Щелчок в поле тени окно получает само
//! (`ignoresMouseEvents` задан явно) и отдаёт намерение закрыть, до окна под ним он не доходит.
//! Рамки частей — из [`layout`](super::layout) по числам паспорта, начало — верхний левый угол
//! окна владельца.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::NSObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSEvent, NSFont, NSFontWeightMedium, NSFontWeightRegular,
    NSFontWeightSemibold, NSImageView, NSLineBreakMode, NSPanel, NSResponder, NSScreen,
    NSTextField, NSView, NSWindow, NSWindowCollectionBehavior, NSWindowOrderingMode,
    NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::CALayer;
use shellio_theme::AppTheme;

use super::layout::{
    self as geometry, BORDER, GROUP_TITLE, HintSpec, PaletteLayout, PaletteSpec, ROW, ROW_ICON,
    ROW_INSET,
};
use super::rows::{
    self, CommandPaletteState, EMPTY_TEXT, FOOTER_HINTS, PLACEHOLDER, PaletteRowSpec,
};
use crate::app::App;
use crate::chrome_bar::Frame;
use crate::components::text_field::{TextField, TextFieldIntent};
use crate::components::virtual_list::model::{ListIntent, Row, RowKind};
use crate::components::virtual_list::{RowViews, VirtualList};
use crate::counters::{Live, counter};
use crate::icon;
use crate::inspect::{self, InspectNode};
use crate::key_badge::KeyBadge;
use crate::layer::{self, without_actions};
use crate::own::ComponentId;
use crate::style;
use crate::window::{WINDOW_HEIGHT, WINDOW_WIDTH};

counter!(PALETTE_OWNER, Owner, "CommandPalette");
counter!(PALETTE_PANEL, Class, "ShellioPalettePanel");
counter!(PALETTE_BOX, Class, "ShellioPaletteBox");
counter!(PALETTE_ROW, Class, "ShellioPaletteRow");

/// Значок строки поиска.
const SEARCH_ICON: &str = "search";
/// Поля плашки категории по горизонтали (`.pal-cat { padding: 0 7px }`).
const CATEGORY_PAD: f64 = 7.0;
/// Поля заголовка группы (`.pg { padding: 10px 18px 4px }`) и межбуквенный интервал в долях кегля.
const GROUP_PAD_X: f64 = 18.0;
const GROUP_PAD_TOP: f64 = 10.0;
const GROUP_SPACING: f64 = 0.06;
/// Радиус заливки выбранной строки (`.pi { border-radius: 6px }`).
const SELECTION_RADIUS: f64 = 6.0;
/// Поле сообщения пустой выдачи (`.pal-empty { padding: 18px }`).
const EMPTY_PAD: f64 = 18.0;
/// Идентификаторы переиспользования видов строки и заголовка.
const ROW_VIEW: &str = "shellio.palette.row";
const HEADER_VIEW: &str = "shellio.palette.header";

/// Намерение палитры. Вызывается на главном потоке изнутри AppKit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaletteIntent {
    /// Пользователь изменил запрос.
    Edited(String),
    /// Щелчок по строке.
    Select(String),
    /// Строку открыли из списка.
    Activate(String),
    /// Щелчок в поле тени или `Escape` поля.
    Dismiss,
}

struct BoxIvars {
    _live: Live,
    /// Щелчок по самому виду: у корня — намерение закрыть, у плашки — ничего.
    on_click: Option<Rc<dyn Fn(PaletteIntent)>>,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioPaletteBox"]
    #[ivars = BoxIvars]
    struct PaletteBox;

    impl PaletteBox {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и аргумент — как у `mouseDown:` NSResponder. Щелчок не уходит дальше по
        // цепочке ответчиков: у корня это поле тени, у плашки — её фон.
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, _event: &NSEvent) {
            if let Some(on_click) = &self.ivars().on_click {
                on_click(PaletteIntent::Dismiss);
            }
        }
    }
);

impl PaletteBox {
    fn new(mtm: MainThreadMarker, on_click: Option<Rc<dyn Fn(PaletteIntent)>>) -> Retained<Self> {
        let this = PaletteBox::alloc(mtm).set_ivars(BoxIvars {
            _live: PALETTE_BOX.live(),
            on_click,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<PaletteBox> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        view.setWantsLayer(true);
        view
    }
}

struct PanelIvars {
    _live: Live,
}

define_class!(
    /// Окно палитры без рамки: становится key, но не main, поэтому окно владельца остаётся main.
    // SAFETY: NSPanel не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioPalettePanel"]
    #[ivars = PanelIvars]
    struct PalettePanel;

    impl PalettePanel {
        // SAFETY: селектор и результат — как у `canBecomeKeyWindow` NSWindow.
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key_window(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `canBecomeMainWindow` NSWindow.
        #[unsafe(method(canBecomeMainWindow))]
        fn can_become_main_window(&self) -> bool {
            false
        }

        // SAFETY: селектор, аргументы и результат — как у `constrainFrameRect:toScreen:` NSWindow.
        // Кадр задаёт палитра от окна владельца: иначе AppKit сдвигает окно, поле тени которого
        // заходит под строку меню.
        #[unsafe(method(constrainFrameRect:toScreen:))]
        fn constrain_frame_rect(&self, frame: NSRect, _screen: Option<&NSScreen>) -> NSRect {
            frame
        }
    }
);

impl PalettePanel {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = PalettePanel::alloc(mtm).set_ivars(PanelIvars {
            _live: PALETTE_PANEL.live(),
        });
        // SAFETY: назначенный инициализатор NSWindow с окном без рамки.
        let panel: Retained<PalettePanel> = unsafe {
            msg_send![
                super(this),
                initWithContentRect: NSRect::ZERO,
                styleMask: NSWindowStyleMask::Borderless,
                backing: NSBackingStoreType::Buffered,
                defer: false
            ]
        };
        // SAFETY: окно держит Rust, AppKit не освобождает его при закрытии.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setTitle(&NSString::from_str("Переход и команды"));
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setHidesOnDeactivate(false);
        panel.setFloatingPanel(false);
        panel.setBecomesKeyOnlyIfNeeded(false);
        // Прозрачное поле тени иначе пропускает щелчок к окну под палитрой.
        panel.setIgnoresMouseEvents(false);
        panel.setCollectionBehavior(NSWindowCollectionBehavior::FullScreenAuxiliary);
        panel
    }
}

/// Поле тени вокруг плашки: наибольшие размытие и смещение тени токена.
pub fn shadow_margin(theme: &AppTheme) -> f64 {
    theme
        .shadow
        .iter()
        .map(|shadow| (shadow.blur + shadow.y.abs()).ceil())
        .fold(0.0, f64::max)
}

struct RowIvars {
    _live: Live,
    icon: Retained<NSImageView>,
    title: Retained<NSTextField>,
    detail: Retained<NSTextField>,
    keys: RefCell<Vec<KeyBadge>>,
    icon_name: RefCell<String>,
    /// Ширины набора названия, пояснения и плашек для раскладки частей.
    widths: Cell<(f64, f64)>,
    header: bool,
}

define_class!(
    // SAFETY: NSView не требует от подкласса ничего сверх переопределённых ниже методов, `Drop` у
    // класса нет.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "ShellioPaletteRow"]
    #[ivars = RowIvars]
    struct PaletteRowView;

    impl PaletteRowView {
        // SAFETY: селектор и результат — как у `isFlipped` NSView.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        // SAFETY: селектор и результат — как у `layout` NSView.
        #[unsafe(method(layout))]
        fn layout(&self) {
            // SAFETY: `layout` у NSView без аргументов.
            let _: () = unsafe { msg_send![super(self), layout] };
            self.place();
        }
    }
);

impl PaletteRowView {
    fn new(mtm: MainThreadMarker, header: bool) -> Retained<Self> {
        let label = || {
            let field = NSTextField::labelWithString(&NSString::from_str(""), mtm);
            field.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
            field
        };
        let this = PaletteRowView::alloc(mtm).set_ivars(RowIvars {
            _live: PALETTE_ROW.live(),
            icon: NSImageView::initWithFrame(NSImageView::alloc(mtm), NSRect::ZERO),
            title: label(),
            detail: label(),
            keys: RefCell::new(Vec::new()),
            icon_name: RefCell::new(String::new()),
            widths: Cell::new((0.0, 0.0)),
            header,
        });
        // SAFETY: назначенный инициализатор NSView, ivars уже заданы.
        let view: Retained<PaletteRowView> =
            unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        let ivars = view.ivars();
        view.addSubview(&ivars.title);
        if !header {
            view.addSubview(&ivars.icon);
            view.addSubview(&ivars.detail);
        }
        view
    }

    fn show_header(&self, title: &str, theme: &AppTheme) {
        // SAFETY: вес шрифта — неизменяемая константа AppKit.
        let font = NSFont::systemFontOfSize_weight(theme.metrics.font.caption, unsafe {
            NSFontWeightSemibold
        });
        let color = layer::ns_color(theme.colors.text_3);
        let size = theme.metrics.font.caption;
        self.ivars()
            .title
            .setAttributedStringValue(&style::spaced_text(
                &title.to_uppercase(),
                &font,
                &color,
                size * GROUP_SPACING,
            ));
        self.place();
    }

    fn show_row(&self, spec: &PaletteRowSpec, selected: bool, theme: &AppTheme) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let ivars = self.ivars();
        let colors = &theme.colors;
        // SAFETY: веса шрифта — неизменяемые константы AppKit.
        let (medium, regular) = unsafe { (NSFontWeightMedium, NSFontWeightRegular) };
        let title_font = NSFont::systemFontOfSize_weight(theme.metrics.font.body, medium);
        let detail_font = NSFont::systemFontOfSize_weight(theme.metrics.font.small, regular);
        ivars.title.setStringValue(&NSString::from_str(&spec.title));
        ivars.title.setFont(Some(&title_font));
        ivars
            .title
            .setTextColor(Some(&layer::ns_color(colors.text)));
        ivars
            .detail
            .setStringValue(&NSString::from_str(&spec.detail));
        ivars.detail.setFont(Some(&detail_font));
        ivars
            .detail
            .setTextColor(Some(&layer::ns_color(colors.text_3)));
        spec.icon.clone_into(&mut ivars.icon_name.borrow_mut());
        let description = NSString::from_str(&spec.title);
        ivars
            .icon
            .setImage(icon::image(&spec.icon, ROW_ICON, &description).as_deref());
        let ink = if selected { colors.text } else { colors.text_2 };
        ivars.icon.setContentTintColor(Some(&layer::ns_color(ink)));
        {
            let mut keys = ivars.keys.borrow_mut();
            let same = keys.len() == spec.keys.len()
                && keys
                    .iter()
                    .zip(&spec.keys)
                    .all(|(badge, text)| badge.text() == *text);
            if !same {
                for badge in keys.drain(..) {
                    badge.view().removeFromSuperview();
                }
                for text in &spec.keys {
                    let badge = KeyBadge::new(mtm, text);
                    self.addSubview(badge.view());
                    keys.push(badge);
                }
            }
            for badge in keys.iter() {
                badge.apply_theme(colors);
            }
        }
        ivars.widths.set((
            style::typeset_width(&spec.title, &title_font),
            if spec.detail.is_empty() {
                0.0
            } else {
                style::typeset_width(&spec.detail, &detail_font)
            },
        ));
        self.place();
    }

    /// Части по ширине вида: строка с полями [`ROW_INSET`], заголовок — с полями `.pg`.
    fn place(&self) {
        let ivars = self.ivars();
        let width = self.bounds().size.width;
        without_actions(|| {
            if ivars.header {
                let height = ivars.title.intrinsicContentSize().height.ceil();
                place_label(
                    &ivars.title,
                    Frame::new(
                        GROUP_PAD_X,
                        GROUP_PAD_TOP,
                        (width - 2.0 * GROUP_PAD_X).max(0.0),
                        height,
                    ),
                );
                return;
            }
            let keys = ivars.keys.borrow();
            let key_widths: Vec<f64> = keys.iter().map(KeyBadge::width).collect();
            let (title, detail) = ivars.widths.get();
            let row_width = (width - 2.0 * ROW_INSET).max(0.0);
            let parts = geometry::row_parts(row_width, title, detail, &key_widths);
            let shift =
                |frame: Frame| Frame::new(frame.x + ROW_INSET, frame.y, frame.width, frame.height);
            ivars.icon.setFrame(shift(parts.icon).rect());
            place_label(&ivars.title, shift(parts.title));
            place_label(&ivars.detail, shift(parts.detail));
            for (badge, frame) in keys.iter().zip(&parts.keys) {
                let frame = shift(*frame);
                badge.place(frame.x, frame.y);
            }
        });
    }

    /// Узлы строки для `ui.inspect` под ключом `ck`: строка, у видимой целиком — её части.
    fn nodes(&self, ck: &str, parent: &str) -> Vec<InspectNode> {
        let ivars = self.ivars();
        let mut row = inspect::node(ck, "PaletteRow", Some(parent), self, None, None).with_ck(ck);
        if !ivars.header {
            let frame = row.facts.frame;
            row.facts.frame = Frame::new(
                frame.x + ROW_INSET,
                frame.y,
                (frame.width - 2.0 * ROW_INSET).max(0.0),
                frame.height,
            );
        }
        let whole = self.visibleRect().size.height >= self.bounds().size.height;
        let mut nodes = vec![row];
        if !whole {
            return nodes;
        }
        if ivars.header {
            nodes[0] =
                inspect::typeset_node(ck, "PaletteGroupTitle", parent, &ivars.title).with_ck(ck);
            nodes[0].facts.frame = inspect::frame_of(self.convertRect_toView(self.bounds(), None));
            return nodes;
        }
        let color = ivars
            .icon
            .contentTintColor()
            .and_then(|color| style::rgba(&color));
        nodes.push(
            inspect::node(
                &format!("{ck}.icon"),
                "Icon",
                Some(ck),
                &ivars.icon,
                None,
                None,
            )
            .with_icon(&ivars.icon_name.borrow(), ROW_ICON, color)
            .with_ck(format!("{ck}.icon")),
        );
        nodes.push(
            inspect::typeset_node(&format!("{ck}.title"), "Label", ck, &ivars.title)
                .with_ck(format!("{ck}.title")),
        );
        nodes.push(
            inspect::typeset_node(&format!("{ck}.detail"), "Label", ck, &ivars.detail)
                .with_ck(format!("{ck}.detail")),
        );
        let keys = ivars.keys.borrow();
        for (index, badge) in keys.iter().enumerate() {
            let key = rows::key_ck(ck, index, keys.len());
            nodes.push(badge.node(&key, ck).with_ck(key.clone()));
        }
        nodes
    }
}

/// Подпись `field` с местом набора `frame`: поле шире набора на поля ячейки.
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

/// Поставщик видов строк палитры: содержимое строки ищется по id в общем наборе.
struct PaletteRows {
    specs: Rc<RefCell<HashMap<String, PaletteRowSpec>>>,
}

impl RowViews for PaletteRows {
    fn heights(&self) -> (f64, f64) {
        (ROW, GROUP_TITLE)
    }

    fn identifier(&self, row: &Row) -> &'static str {
        match row.kind {
            RowKind::Item => ROW_VIEW,
            RowKind::Header => HEADER_VIEW,
        }
    }

    fn make(&self, mtm: MainThreadMarker, row: &Row) -> Retained<NSView> {
        Retained::into_super(PaletteRowView::new(mtm, row.kind == RowKind::Header))
    }

    fn show(&self, view: &NSView, row: &Row, selected: bool, theme: &AppTheme) {
        let Some(view) = view.downcast_ref::<PaletteRowView>() else {
            return;
        };
        match row.kind {
            RowKind::Header => view.show_header(&row.text, theme),
            RowKind::Item => {
                if let Some(spec) = self.specs.borrow().get(&row.id) {
                    view.show_row(spec, selected, theme);
                }
            }
        }
    }

    fn selection(&self, bounds: NSRect) -> (NSRect, f64) {
        (
            NSRect::new(
                NSPoint::new(bounds.origin.x + ROW_INSET, bounds.origin.y),
                NSSize::new(
                    (bounds.size.width - 2.0 * ROW_INSET).max(0.0),
                    bounds.size.height,
                ),
            ),
            SELECTION_RADIUS,
        )
    }
}

/// Подсказка подвала.
struct FooterHint {
    group: Retained<NSView>,
    keys: Vec<KeyBadge>,
    label: Retained<NSTextField>,
}

/// Палитра на главном потоке. Освобождение снимает окно палитры с экрана.
pub struct CommandPalette {
    _live: Live,
    id: ComponentId,
    panel: Retained<PalettePanel>,
    root: Retained<PaletteBox>,
    /// Хосты слоёв тени и сами слои по порядку записи токена.
    casters: Vec<Retained<NSView>>,
    shadows: Vec<Retained<CALayer>>,
    /// Кольца у плашки нет: линию рисует её слой.
    ring: Retained<CALayer>,
    plate: Retained<PaletteBox>,
    header: Retained<PaletteBox>,
    header_line: Retained<NSView>,
    icon: Retained<NSImageView>,
    category: Retained<NSView>,
    category_label: Retained<NSTextField>,
    query: TextField,
    escape: KeyBadge,
    list: VirtualList,
    empty: Retained<NSTextField>,
    footer: Retained<PaletteBox>,
    footer_line: Retained<NSView>,
    hints: Vec<FooterHint>,
    specs: Rc<RefCell<HashMap<String, PaletteRowSpec>>>,
    state: RefCell<CommandPaletteState>,
    theme: Cell<AppTheme>,
    /// Размер содержимого окна владельца, по которому стоит раскладка.
    window: Cell<(f64, f64)>,
    layout: RefCell<Option<PaletteLayout>>,
}

impl CommandPalette {
    /// Палитра с пустым состоянием, окно не показано. `on_intent` вызывается на главном потоке
    /// изнутри AppKit и не должен синхронно освобождать палитру.
    pub fn new(app: &App, theme: &AppTheme, on_intent: impl Fn(PaletteIntent) + 'static) -> Self {
        let mtm = app.mtm();
        let on_intent: Rc<dyn Fn(PaletteIntent)> = Rc::new(on_intent);
        let panel = PalettePanel::new(mtm);
        let root = PaletteBox::new(mtm, Some(Rc::clone(&on_intent)));
        let (casters, shadows): (Vec<_>, Vec<_>) = theme
            .shadow
            .iter()
            .map(|_| {
                // Тень несёт подслой слоя-хоста: у слоя самого вида AppKit сбрасывает
                // `shadowOpacity` по `NSView.shadow`.
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
        let plate = PaletteBox::new(mtm, None);
        root.addSubview(&plate);
        let header = PaletteBox::new(mtm, None);
        plate.addSubview(&header);
        let header_line = plain(mtm);
        plate.addSubview(&header_line);
        let icon = NSImageView::initWithFrame(NSImageView::alloc(mtm), NSRect::ZERO);
        let description = NSString::from_str(PLACEHOLDER);
        icon.setImage(icon::image(SEARCH_ICON, geometry::HEADER_ICON, &description).as_deref());
        header.addSubview(&icon);
        let category = plain(mtm);
        let category_label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
        category.addSubview(&category_label);
        header.addSubview(&category);
        let edits = Rc::clone(&on_intent);
        let query = TextField::palette(app, theme, "", PLACEHOLDER, move |intent| match intent {
            TextFieldIntent::Changed(value) => edits(PaletteIntent::Edited(value)),
            TextFieldIntent::Cancelled => edits(PaletteIntent::Dismiss),
            TextFieldIntent::Submitted(_) => {}
        });
        header.addSubview(query.ns_view());
        let escape = KeyBadge::new(mtm, "esc");
        header.addSubview(escape.view());

        let specs: Rc<RefCell<HashMap<String, PaletteRowSpec>>> = Rc::default();
        let views = Rc::new(PaletteRows {
            specs: Rc::clone(&specs),
        });
        let picks = Rc::clone(&on_intent);
        let list = VirtualList::with_row_views(
            app,
            theme,
            rows::list_state(&CommandPaletteState::default()),
            views,
            move |intent| match intent {
                ListIntent::Select { id } => picks(PaletteIntent::Select(id)),
                ListIntent::Activate { id } => picks(PaletteIntent::Activate(id)),
                ListIntent::Cancel => picks(PaletteIntent::Dismiss),
            },
        )
        .unwrap_or_else(|_| unreachable!("пустое состояние списка проходит проверку"));
        list.set_insets(geometry::LIST_TOP, geometry::LIST_BOTTOM);
        plate.addSubview(list.ns_view());
        let empty = NSTextField::labelWithString(&NSString::from_str(EMPTY_TEXT), mtm);
        plate.addSubview(&empty);

        let footer = PaletteBox::new(mtm, None);
        plate.addSubview(&footer);
        let footer_line = plain(mtm);
        footer.addSubview(&footer_line);
        let hints = FOOTER_HINTS
            .iter()
            .map(|(keys, label)| {
                let group = plain(mtm);
                let keys = keys
                    .iter()
                    .map(|key| {
                        let badge = KeyBadge::new(mtm, key);
                        group.addSubview(badge.view());
                        badge
                    })
                    .collect();
                let label = NSTextField::labelWithString(&NSString::from_str(label), mtm);
                group.addSubview(&label);
                footer.addSubview(&group);
                FooterHint { group, keys, label }
            })
            .collect();
        panel.setContentView(Some(&root));

        let palette = CommandPalette {
            _live: PALETTE_OWNER.live(),
            id: ComponentId::next(),
            panel,
            root,
            casters,
            shadows,
            ring: CALayer::new(),
            plate,
            header,
            header_line,
            icon,
            category,
            category_label,
            query,
            escape,
            list,
            empty,
            footer,
            footer_line,
            hints,
            specs,
            state: RefCell::new(CommandPaletteState::default()),
            theme: Cell::new(*theme),
            window: Cell::new((WINDOW_WIDTH, WINDOW_HEIGHT)),
            layout: RefCell::new(None),
        };
        palette.apply_theme();
        palette.place();
        palette
    }

    pub fn id(&self) -> ComponentId {
        self.id
    }

    /// Состояние, которое показывает вид.
    pub fn state(&self) -> CommandPaletteState {
        self.state.borrow().clone()
    }

    /// Новое состояние: запрос, категория, строки и выбор. Эхо запроса не трогает ввод в поле.
    pub fn set_state(&self, state: CommandPaletteState) {
        {
            let mut specs = self.specs.borrow_mut();
            specs.clear();
            for row in state.groups.iter().flat_map(|group| &group.rows) {
                specs.insert(row.id.clone(), row.clone());
            }
        }
        let mut field = self.query.state();
        field.value.clone_from(&state.query);
        self.query.set_state(field);
        self.category_label.setStringValue(&NSString::from_str(
            state.category.as_deref().unwrap_or_default(),
        ));
        let list = rows::list_state(&state);
        *self.state.borrow_mut() = state;
        if self.list.set_state(list).is_err() {
            // Дубли id отбрасывает модель палитры: сюда они не доходят.
            return;
        }
        self.place();
    }

    pub fn set_theme(&self, theme: &AppTheme) {
        if self.theme.get() == *theme {
            return;
        }
        self.theme.set(*theme);
        self.apply_theme();
        self.place();
    }

    /// Показывает палитру над окном `parent` с размером содержимого `content` (точки, начало
    /// окна — нижний левый угол экрана), делает её key и ставит ввод в поле запроса.
    pub fn open(&self, parent: &crate::window::MainWindow) {
        let window = parent.ns_window();
        let frame = window.frame();
        self.window.set((frame.size.width, frame.size.height));
        self.place();
        let Some(layout) = self.layout.borrow().clone() else {
            return;
        };
        let margin = shadow_margin(&self.theme.get());
        let plate = layout.panel;
        let origin = NSPoint::new(
            frame.origin.x + plate.x - margin,
            frame.origin.y + frame.size.height - plate.y - plate.height - margin,
        );
        let size = NSSize::new(plate.width + 2.0 * margin, plate.height + 2.0 * margin);
        self.panel
            .setFrame_display(NSRect::new(origin, size), false);
        if self.panel.parentWindow().is_none() {
            // SAFETY: окно палитры держит палитра дольше связи, связь снимается при закрытии.
            unsafe { window.addChildWindow_ordered(&self.panel, NSWindowOrderingMode::Above) };
        }
        self.panel.makeKeyAndOrderFront(None);
        self.query.focus();
    }

    /// Убирает палитру с экрана и возвращает key окну владельца, если оно видно.
    pub fn close(&self, parent: &crate::window::MainWindow) {
        let window = parent.ns_window();
        if self.panel.parentWindow().is_some() {
            window.removeChildWindow(&self.panel);
        }
        self.query.resign_focus();
        self.panel.orderOut(None);
        if window.isVisible() && !window.isKeyWindow() {
            window.makeKeyWindow();
        }
    }

    /// Палитра на экране.
    pub fn is_open(&self) -> bool {
        self.panel.isVisible()
    }

    /// Окно палитры key.
    pub fn is_key(&self) -> bool {
        self.panel.isKeyWindow()
    }

    /// Номер окна палитры.
    pub fn number(&self) -> i64 {
        self.panel.windowNumber() as i64
    }

    /// Ставит ввод в поле запроса.
    pub fn focus_query(&self) -> bool {
        self.query.focus()
    }

    /// Рамки частей в системе координат окна владельца; `None` — раскладки ещё нет.
    pub fn layout(&self) -> Option<PaletteLayout> {
        self.layout.borrow().clone()
    }

    fn apply_theme(&self) {
        let theme = self.theme.get();
        let colors = &theme.colors;
        let radius = theme.metrics.radius.float;
        without_actions(|| {
            for (shadow, token) in self.shadows.iter().zip(theme.shadow) {
                layer::set_fill(shadow, colors.surface);
                layer::set_corner(shadow, radius);
                layer::set_shadow(shadow, token);
            }
            if let Some(plate) = self.plate.layer() {
                layer::set_fill(&plate, colors.surface);
                layer::set_line(&plate, Some((colors.line_2, BORDER)));
                layer::set_radius(&plate, radius);
            }
            for line in [&self.header_line, &self.footer_line] {
                if let Some(layer) = line.layer() {
                    layer::set_fill(&layer, colors.line);
                }
            }
            if let Some(footer) = self.footer.layer() {
                layer::set_fill(&footer, colors.head);
            }
            if let Some(category) = self.category.layer() {
                layer::set_fill(&category, colors.selection);
                layer::set_radius(&category, theme.metrics.radius.pill);
            }
        });
        // SAFETY: вес шрифта — неизменяемая константа AppKit.
        let regular = unsafe { NSFontWeightRegular };
        let small = NSFont::systemFontOfSize_weight(theme.metrics.font.small, regular);
        self.category_label.setFont(Some(&small));
        self.category_label
            .setTextColor(Some(&layer::ns_color(colors.text_2)));
        self.icon
            .setContentTintColor(Some(&layer::ns_color(colors.text_3)));
        self.empty.setFont(Some(&NSFont::systemFontOfSize_weight(
            theme.metrics.font.body,
            regular,
        )));
        self.empty
            .setTextColor(Some(&layer::ns_color(colors.text_3)));
        let meta = NSFont::systemFontOfSize_weight(theme.metrics.font.meta, regular);
        for hint in &self.hints {
            hint.label.setFont(Some(&meta));
            hint.label
                .setTextColor(Some(&layer::ns_color(colors.text_3)));
            for badge in &hint.keys {
                badge.apply_theme(colors);
            }
        }
        self.escape.apply_theme(colors);
        self.query.set_theme(&theme);
        let _ = self.list.set_theme(&theme);
    }

    /// Раскладка по размеру окна владельца и состоянию.
    fn place(&self) {
        let state = self.state.borrow();
        let category = state.category.as_ref().map(|text| {
            self.category_label
                .font()
                .map_or(0.0, |font| style::typeset_width(text, &font))
                + 2.0 * CATEGORY_PAD
        });
        let hints = self
            .hints
            .iter()
            .map(|hint| HintSpec {
                keys: hint.keys.iter().map(KeyBadge::width).collect(),
                label: hint.label.font().map_or(0.0, |font| {
                    style::typeset_width(&hint.label.stringValue().to_string(), &font)
                }),
            })
            .collect();
        let spec = PaletteSpec {
            category,
            escape: self.escape.width(),
            list: rows::list_content(&state),
            hints,
        };
        let (width, height) = self.window.get();
        let frames = geometry::layout(width, height, &spec);
        let margin = shadow_margin(&self.theme.get());
        let plate = frames.panel;
        // Части плашки — в её системе координат.
        let local = |frame: Frame| {
            Frame::new(
                frame.x - plate.x,
                frame.y - plate.y,
                frame.width,
                frame.height,
            )
        };
        let within = |frame: Frame, outer: Frame| {
            Frame::new(
                frame.x - outer.x,
                frame.y - outer.y,
                frame.width,
                frame.height,
            )
        };
        without_actions(|| {
            self.root.setFrame(
                Frame::new(
                    0.0,
                    0.0,
                    plate.width + 2.0 * margin,
                    plate.height + 2.0 * margin,
                )
                .rect(),
            );
            let surface = Frame::new(margin, margin, plate.width, plate.height);
            self.plate.setFrame(surface.rect());
            for caster in &self.casters {
                caster.setFrame(surface.rect());
            }
            for shadow in &self.shadows {
                shadow.setFrame(NSRect::new(
                    NSPoint::ZERO,
                    NSSize::new(plate.width, plate.height),
                ));
            }
            let header = local(frames.header);
            self.header.setFrame(header.rect());
            self.header_line.setFrame(
                Frame::new(
                    header.x,
                    header.y + header.height - BORDER,
                    header.width,
                    BORDER,
                )
                .rect(),
            );
            self.icon
                .setFrame(within(frames.icon, frames.header).rect());
            match frames.category {
                Some(frame) => {
                    let frame = within(frame, frames.header);
                    self.category.setHidden(false);
                    self.category.setFrame(frame.rect());
                    let natural = self.category_label.intrinsicContentSize();
                    place_label(
                        &self.category_label,
                        Frame::new(
                            CATEGORY_PAD,
                            ((frame.height - natural.height) / 2.0).round(),
                            (frame.width - 2.0 * CATEGORY_PAD).max(0.0),
                            natural.height,
                        ),
                    );
                }
                None => self.category.setHidden(true),
            }
            self.query
                .ns_view()
                .setFrame(within(frames.query, frames.header).rect());
            let escape = within(frames.escape, frames.header);
            self.escape.place(escape.x, escape.y);
            self.list.ns_view().setFrame(local(frames.list).rect());
            match frames.empty {
                Some(frame) => {
                    let frame = local(frame);
                    self.empty.setHidden(false);
                    let natural = self.empty.intrinsicContentSize();
                    place_label(
                        &self.empty,
                        Frame::new(
                            frame.x + EMPTY_PAD,
                            frame.y + EMPTY_PAD,
                            (frame.width - 2.0 * EMPTY_PAD).max(0.0),
                            natural.height,
                        ),
                    );
                }
                None => self.empty.setHidden(true),
            }
            let footer = local(frames.footer);
            self.footer.setFrame(footer.rect());
            self.footer_line
                .setFrame(Frame::new(0.0, 0.0, footer.width, BORDER).rect());
            for (hint, frame) in self.hints.iter().zip(&frames.hints) {
                hint.group
                    .setFrame(within(frame.frame, frames.footer).rect());
                for (badge, key) in hint.keys.iter().zip(&frame.keys) {
                    let key = within(*key, frame.frame);
                    badge.place(key.x, key.y);
                }
                place_label(&hint.label, within(frame.label, frame.frame));
            }
        });
        *self.layout.borrow_mut() = Some(frames);
    }

    /// Узлы палитры для `ui.inspect` с ключами сверки. Кадры — в системе координат окна палитры:
    /// перевод в окно владельца — срез 4 задачи 4 плана `command-overlays`.
    pub fn inspect(&self) -> Vec<InspectNode> {
        let mut nodes = Vec::new();
        let plate = inspect::node("palette", "CommandPalette", None, &self.plate, None, None)
            .with_ck("palette")
            .with_shadow_layers(&self.shadows, &self.ring);
        nodes.push(plate);
        nodes.push(
            inspect::node(
                "palette.header",
                "PaletteHeader",
                Some("palette"),
                &self.header,
                None,
                None,
            )
            .with_ck("palette.header"),
        );
        let icon_color = self
            .icon
            .contentTintColor()
            .and_then(|color| style::rgba(&color));
        nodes.push(
            inspect::node(
                "palette.header.icon",
                "Icon",
                Some("palette.header"),
                &self.icon,
                None,
                None,
            )
            .with_icon(SEARCH_ICON, geometry::HEADER_ICON, icon_color)
            .with_ck("palette.header.icon"),
        );
        if !self.category.isHidden() {
            nodes.push(
                inspect::node(
                    "palette.category",
                    "PaletteCategory",
                    Some("palette.header"),
                    &self.category,
                    None,
                    None,
                )
                .with_text_of(&self.category_label)
                .with_ck("palette.category"),
            );
        }
        nodes.extend(
            self.query
                .inspect("palette.query", Some("palette.header"))
                .into_iter()
                .map(|node| node.with_ck("palette.query")),
        );
        nodes.push(
            self.escape
                .node("palette.escape", "palette.header")
                .with_ck("palette.escape"),
        );
        nodes.push(
            inspect::node(
                "palette.list",
                "PaletteList",
                Some("palette"),
                self.list.ns_view(),
                None,
                None,
            )
            .with_ck("palette.list"),
        );
        let state = self.state.borrow();
        for (index, view) in self.list.shown_views() {
            let Some(view) = view.downcast_ref::<PaletteRowView>() else {
                continue;
            };
            if let Some(ck) = rows::row_ck(&state, index) {
                nodes.extend(view.nodes(&ck, "palette.list"));
            }
        }
        if !self.empty.isHidden() {
            let mut empty =
                inspect::typeset_node("palette.empty", "Label", "palette.list", &self.empty)
                    .with_ck("palette.empty");
            // Узел паспорта — коробка сообщения с полями, а не место набора подписи.
            if let Some(layout) = self.layout.borrow().as_ref()
                && let Some(frame) = layout.empty
            {
                let plate = layout.panel;
                let local = Frame::new(
                    frame.x - plate.x,
                    frame.y - plate.y,
                    frame.width,
                    frame.height,
                );
                empty.facts.frame =
                    inspect::frame_of(self.plate.convertRect_toView(local.rect(), None));
            }
            nodes.push(empty);
        }
        nodes.push(
            inspect::node(
                "palette.footer",
                "PaletteFooter",
                Some("palette"),
                &self.footer,
                None,
                None,
            )
            .with_ck("palette.footer"),
        );
        for (index, hint) in self.hints.iter().enumerate() {
            let ck = format!("palette.footer.hint.{}", index + 1);
            nodes.push(
                inspect::node(
                    &ck,
                    "PaletteHint",
                    Some("palette.footer"),
                    &hint.group,
                    None,
                    None,
                )
                .with_ck(ck.clone()),
            );
            for (key, badge) in hint.keys.iter().enumerate() {
                let key_ck = rows::key_ck(&ck, key, hint.keys.len());
                nodes.push(badge.node(&key_ck, &ck).with_ck(key_ck.clone()));
            }
            nodes.push(
                inspect::typeset_node(&format!("{ck}.label"), "Label", &ck, &hint.label)
                    .with_ck(format!("{ck}.label")),
            );
        }
        nodes
    }
}

impl Drop for CommandPalette {
    fn drop(&mut self) {
        if let Some(parent) = self.panel.parentWindow() {
            parent.removeChildWindow(&self.panel);
        }
        self.panel.makeFirstResponder(None);
        self.panel.orderOut(None);
        self.panel.setContentView(None);
    }
}

/// Вид со слоем без собственного рисования: линия, плашка категории, группа подсказки.
fn plain(mtm: MainThreadMarker) -> Retained<NSView> {
    let view = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
    view.setWantsLayer(true);
    view
}
