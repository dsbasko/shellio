//! Чистая раскладка палитры по числам паспорта (`prototype/css/app.css`, `.pal` и ниже).
//!
//! Начало координат — верхний левый угол окна владельца, ось y вниз. Ширины текстов и плашек
//! даёт вызывающий: раскладка их не меряет. Строки и заголовки групп стоят в списке без
//! прокрутки, сдвиг прокрутки вычитает вид.

use crate::chrome_bar::Frame;

/// Ширина палитры.
pub const WIDTH: f64 = 640.0;
/// Отступ палитры от верхнего края окна.
pub const TOP: f64 = 46.0;
/// Наибольшая высота палитры и запас окна под ней: `min(560px, 100vh - 70px)`.
pub const MAX_HEIGHT: f64 = 560.0;
pub const WINDOW_MARGIN: f64 = 70.0;
/// Линия рамки палитры.
pub const BORDER: f64 = 1.0;
/// Строка поиска: обычная высота и наименьшая при переполненном списке. Линия под ней входит
/// в высоту.
pub const HEADER: f64 = 46.0;
pub const HEADER_MIN: f64 = 40.390625;
pub const HEADER_PADDING: f64 = 14.0;
pub const HEADER_GAP: f64 = 10.0;
pub const HEADER_ICON: f64 = 16.0;
pub const QUERY_HEIGHT: f64 = 30.0;
pub const CATEGORY_HEIGHT: f64 = 20.0;
/// Плашка клавиши.
pub const BADGE_HEIGHT: f64 = 17.0;
/// Поля списка сверху и снизу.
pub const LIST_TOP: f64 = 4.0;
pub const LIST_BOTTOM: f64 = 6.0;
/// Заголовок группы и строка.
pub const GROUP_TITLE: f64 = 32.0;
pub const ROW: f64 = 34.0;
pub const ROW_INSET: f64 = 6.0;
pub const ROW_PADDING: f64 = 12.0;
pub const ROW_GAP: f64 = 10.0;
pub const ROW_ICON: f64 = 14.0;
pub const ROW_TEXT: f64 = 18.0;
/// Наибольшая доля ширины содержимого строки под название.
pub const TITLE_SHARE: f64 = 0.6;
pub const KEY_GAP: f64 = 3.0;
/// Сообщение «Ничего не найдено»: поле 18 вокруг строки 18.
pub const EMPTY: f64 = 54.0;
/// Подвал: поля 8 и 14, линия над ним входит в высоту.
pub const FOOTER: f64 = 35.0;
pub const FOOTER_PADDING: f64 = 14.0;
pub const FOOTER_TOP: f64 = 8.0;
pub const HINT_GAP: f64 = 16.0;
pub const HINT_INNER_GAP: f64 = 4.0;
pub const HINT_TEXT: f64 = 18.0;

/// Содержимое списка.
#[derive(Debug, Clone, PartialEq)]
pub enum ListContent {
    /// Ничего не найдено.
    Empty,
    /// Число строк каждой группы по порядку.
    Groups(Vec<usize>),
}

impl ListContent {
    /// Высота списка без сжатия, с полями.
    pub fn natural_height(&self) -> f64 {
        let inner = match self {
            ListContent::Empty => EMPTY,
            ListContent::Groups(groups) => groups
                .iter()
                .map(|rows| GROUP_TITLE + *rows as f64 * ROW)
                .sum(),
        };
        LIST_TOP + inner + LIST_BOTTOM
    }
}

/// Подсказка подвала: ширины плашек и подписи.
#[derive(Debug, Clone, PartialEq)]
pub struct HintSpec {
    pub keys: Vec<f64>,
    pub label: f64,
}

/// Что меряет вызывающий для раскладки.
#[derive(Debug, Clone, PartialEq)]
pub struct PaletteSpec {
    /// Ширина плашки категории, `None` — категории нет.
    pub category: Option<f64>,
    /// Ширина плашки `esc` строки поиска.
    pub escape: f64,
    pub list: ListContent,
    pub hints: Vec<HintSpec>,
}

/// Группа: рамка, заголовок и строки.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupFrames {
    pub frame: Frame,
    pub title: Frame,
    pub rows: Vec<Frame>,
}

/// Подсказка подвала.
#[derive(Debug, Clone, PartialEq)]
pub struct HintFrames {
    pub frame: Frame,
    pub keys: Vec<Frame>,
    pub label: Frame,
}

/// Рамки узлов палитры.
#[derive(Debug, Clone, PartialEq)]
pub struct PaletteLayout {
    pub panel: Frame,
    pub header: Frame,
    pub icon: Frame,
    pub category: Option<Frame>,
    pub query: Frame,
    pub escape: Frame,
    /// Видимая область списка с полями.
    pub list: Frame,
    pub empty: Option<Frame>,
    pub groups: Vec<GroupFrames>,
    pub footer: Frame,
    pub hints: Vec<HintFrames>,
}

/// Высоты строки поиска и списка в палитре с наибольшей внутренней высотой `inner` (без рамки):
/// гибкое сжатие CSS по основе. Подвал не сжимается, строка поиска — не ниже [`HEADER_MIN`],
/// остаток забирает список.
pub fn shrink(inner: f64, list: f64) -> (f64, f64) {
    let deficit = HEADER + list + FOOTER - inner;
    if deficit <= 0.0 {
        return (HEADER, list);
    }
    let basis = HEADER + list;
    let header = HEADER - deficit * HEADER / basis;
    if header < HEADER_MIN {
        (HEADER_MIN, (inner - FOOTER - HEADER_MIN).max(0.0))
    } else {
        (header, list - deficit * list / basis)
    }
}

/// Раскладка палитры в окне `window_width` × `window_height`.
pub fn layout(window_width: f64, window_height: f64, spec: &PaletteSpec) -> PaletteLayout {
    let max = MAX_HEIGHT
        .min(window_height - WINDOW_MARGIN)
        .max(2.0 * BORDER);
    let natural = spec.list.natural_height();
    let (header_h, list_h) = shrink(max - 2.0 * BORDER, natural);
    let panel = Frame::new(
        window_width / 2.0 - WIDTH / 2.0,
        TOP,
        WIDTH,
        2.0 * BORDER + header_h + list_h + FOOTER,
    );
    let inner_x = panel.x + BORDER;
    let inner_w = WIDTH - 2.0 * BORDER;

    let header = Frame::new(inner_x, panel.y + BORDER, inner_w, header_h);
    // Линия под строкой поиска входит в её высоту: середина — без линии.
    let middle = header.y + (header_h - BORDER) / 2.0;
    let content_left = inner_x + HEADER_PADDING;
    let content_right = inner_x + inner_w - HEADER_PADDING;
    let icon = Frame::new(
        content_left,
        middle - HEADER_ICON / 2.0,
        HEADER_ICON,
        HEADER_ICON,
    );
    let escape = Frame::new(
        content_right - spec.escape,
        middle - BADGE_HEIGHT / 2.0,
        spec.escape,
        BADGE_HEIGHT,
    );
    let mut cursor = icon.x + icon.width + HEADER_GAP;
    let category = spec.category.map(|width| {
        let frame = Frame::new(
            cursor,
            middle - CATEGORY_HEIGHT / 2.0,
            width,
            CATEGORY_HEIGHT,
        );
        cursor += width + HEADER_GAP;
        frame
    });
    let query = Frame::new(
        cursor,
        middle - QUERY_HEIGHT / 2.0,
        (escape.x - HEADER_GAP - cursor).max(0.0),
        QUERY_HEIGHT,
    );

    let list = Frame::new(inner_x, header.y + header_h, inner_w, list_h);
    let mut y = list.y + LIST_TOP;
    let (empty, groups) = match &spec.list {
        ListContent::Empty => (Some(Frame::new(inner_x, y, inner_w, EMPTY)), Vec::new()),
        ListContent::Groups(counts) => {
            let groups = counts
                .iter()
                .map(|count| {
                    let top = y;
                    let title = Frame::new(inner_x, top, inner_w, GROUP_TITLE);
                    let rows = (0..*count)
                        .map(|index| row_frame(inner_x, inner_w, top + GROUP_TITLE, index))
                        .collect();
                    let height = GROUP_TITLE + *count as f64 * ROW;
                    y += height;
                    GroupFrames {
                        frame: Frame::new(inner_x, top, inner_w, height),
                        title,
                        rows,
                    }
                })
                .collect();
            (None, groups)
        }
    };

    let footer = Frame::new(inner_x, list.y + list_h, inner_w, FOOTER);
    let hint_y = footer.y + BORDER + FOOTER_TOP;
    let mut x = inner_x + FOOTER_PADDING;
    let hints = spec
        .hints
        .iter()
        .map(|hint| {
            let left = x;
            let keys = hint
                .keys
                .iter()
                .map(|width| {
                    let frame = Frame::new(
                        x,
                        hint_y + (HINT_TEXT - BADGE_HEIGHT) / 2.0,
                        *width,
                        BADGE_HEIGHT,
                    );
                    x += width + HINT_INNER_GAP;
                    frame
                })
                .collect();
            let label = Frame::new(x, hint_y, hint.label, HINT_TEXT);
            x += hint.label + HINT_GAP;
            HintFrames {
                frame: Frame::new(left, hint_y, label.x + label.width - left, HINT_TEXT),
                keys,
                label,
            }
        })
        .collect();

    PaletteLayout {
        panel,
        header,
        icon,
        category,
        query,
        escape,
        list,
        empty,
        groups,
        footer,
        hints,
    }
}

fn row_frame(list_x: f64, list_w: f64, top: f64, index: usize) -> Frame {
    Frame::new(
        list_x + ROW_INSET,
        top + index as f64 * ROW,
        list_w - 2.0 * ROW_INSET,
        ROW,
    )
}

/// Части строки в её системе координат: значок, название, пояснение и плашки клавиш.
#[derive(Debug, Clone, PartialEq)]
pub struct RowParts {
    pub icon: Frame,
    pub title: Frame,
    /// Пустое пояснение — рамка нулевого размера на середине строки после названия.
    pub detail: Frame,
    pub keys: Vec<Frame>,
}

/// Части строки шириной `width`: название не шире [`TITLE_SHARE`] содержимого, клавиши у правого
/// края, пояснение между ними сжимается.
pub fn row_parts(width: f64, title: f64, detail: f64, keys: &[f64]) -> RowParts {
    let content = width - 2.0 * ROW_PADDING;
    let icon = Frame::new(ROW_PADDING, (ROW - ROW_ICON) / 2.0, ROW_ICON, ROW_ICON);
    let text_y = (ROW - ROW_TEXT) / 2.0;
    let title_x = icon.x + ROW_ICON + ROW_GAP;
    let title = Frame::new(title_x, text_y, title.min(content * TITLE_SHARE), ROW_TEXT);

    let keys_width: f64 = keys.iter().sum::<f64>() + KEY_GAP * keys.len().saturating_sub(1) as f64;
    let mut x = width - ROW_PADDING - keys_width;
    let keys_left = x;
    let keys = keys
        .iter()
        .map(|key| {
            let frame = Frame::new(x, (ROW - BADGE_HEIGHT) / 2.0, *key, BADGE_HEIGHT);
            x += key + KEY_GAP;
            frame
        })
        .collect::<Vec<_>>();

    let detail_x = title.x + title.width + ROW_GAP;
    let detail = if detail <= 0.0 {
        Frame::new(detail_x, ROW / 2.0, 0.0, 0.0)
    } else {
        let room = if keys.is_empty() {
            width - ROW_PADDING - detail_x
        } else {
            keys_left - ROW_GAP - detail_x
        };
        Frame::new(detail_x, text_y, detail.min(room.max(0.0)), ROW_TEXT)
    };
    RowParts {
        icon,
        title,
        detail,
        keys,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Окно сцены `p15`.
    const WINDOW: (f64, f64) = (1180.0, 720.0);
    const ESC: f64 = 28.90625;

    fn footer_hints() -> Vec<HintSpec> {
        vec![
            HintSpec {
                keys: vec![17.0, 17.0],
                label: 47.21875,
            },
            HintSpec {
                keys: vec![17.0],
                label: 60.875,
            },
            HintSpec {
                keys: vec![ESC],
                label: 46.265625,
            },
        ]
    }

    fn near(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() < 0.01
    }

    fn assert_frame(frame: Frame, x: f64, y: f64, w: f64, h: f64) {
        assert!(
            near(frame.x, x) && near(frame.y, y) && near(frame.width, w) && near(frame.height, h),
            "{frame:?} ≠ ({x}, {y}, {w}, {h})"
        );
    }

    /// `palette-groups`: 11 команд и 6 панелей переполняют список, строка поиска сжата.
    #[test]
    fn overflowing_list_shrinks_header_to_minimum() {
        let spec = PaletteSpec {
            category: None,
            escape: ESC,
            list: ListContent::Groups(vec![11, 6]),
            hints: footer_hints(),
        };
        let layout = layout(WINDOW.0, WINDOW.1, &spec);
        assert_frame(layout.panel, 270.0, 46.0, 640.0, 560.0);
        assert_frame(layout.header, 271.0, 47.0, 638.0, 40.390625);
        assert_frame(layout.icon, 285.0, 58.695312, 16.0, 16.0);
        assert_frame(layout.query, 311.0, 51.695312, 545.09375, 30.0);
        assert_frame(layout.escape, 866.09375, 58.195312, ESC, 17.0);
        assert_frame(layout.list, 271.0, 87.390625, 638.0, 482.609375);
        assert_frame(layout.groups[0].frame, 271.0, 91.390625, 638.0, 406.0);
        assert_frame(layout.groups[0].title, 271.0, 91.390625, 638.0, 32.0);
        assert_frame(layout.groups[0].rows[0], 277.0, 123.390625, 626.0, 34.0);
        assert_frame(layout.groups[0].rows[10], 277.0, 463.390625, 626.0, 34.0);
        assert_frame(layout.groups[1].frame, 271.0, 497.390625, 638.0, 236.0);
        assert_frame(layout.groups[1].rows[1], 277.0, 563.390625, 626.0, 34.0);
        assert_frame(layout.footer, 271.0, 570.0, 638.0, 35.0);
        assert_frame(layout.hints[0].frame, 285.0, 579.0, 89.21875, 18.0);
        assert_frame(layout.hints[0].keys[1], 306.0, 579.5, 17.0, 17.0);
        assert_frame(layout.hints[0].label, 327.0, 579.0, 47.21875, 18.0);
        assert_frame(layout.hints[1].frame, 390.21875, 579.0, 81.875, 18.0);
        assert_frame(layout.hints[2].keys[0], 488.09375, 579.5, ESC, 17.0);
        assert_frame(layout.hints[2].label, 521.0, 579.0, 46.265625, 18.0);
        assert_eq!(layout.empty, None);
        assert_eq!(layout.category, None);
    }

    /// `palette-no-results`: сообщение вместо групп, строка поиска обычной высоты.
    #[test]
    fn empty_list_keeps_full_header() {
        let spec = PaletteSpec {
            category: None,
            escape: ESC,
            list: ListContent::Empty,
            hints: footer_hints(),
        };
        let layout = layout(WINDOW.0, WINDOW.1, &spec);
        assert_frame(layout.panel, 270.0, 46.0, 640.0, 147.0);
        assert_frame(layout.header, 271.0, 47.0, 638.0, 46.0);
        assert_frame(layout.icon, 285.0, 61.5, 16.0, 16.0);
        assert_frame(layout.query, 311.0, 54.5, 545.09375, 30.0);
        assert_frame(layout.escape, 866.09375, 61.0, ESC, 17.0);
        assert_frame(layout.list, 271.0, 93.0, 638.0, 64.0);
        assert_frame(
            layout.empty.expect("пустой список"),
            271.0,
            97.0,
            638.0,
            54.0,
        );
        assert_frame(layout.footer, 271.0, 157.0, 638.0, 35.0);
        assert_frame(layout.hints[0].frame, 285.0, 166.0, 89.21875, 18.0);
        assert!(layout.groups.is_empty());
    }

    /// `palette-tab-narrowed`: плашка категории сдвигает поле запроса.
    #[test]
    fn category_badge_narrows_query() {
        let spec = PaletteSpec {
            category: Some(67.765625),
            escape: ESC,
            list: ListContent::Groups(vec![3]),
            hints: footer_hints(),
        };
        let layout = layout(WINDOW.0, WINDOW.1, &spec);
        assert_frame(layout.panel, 270.0, 46.0, 640.0, 227.0);
        assert_frame(
            layout.category.expect("категория"),
            311.0,
            59.5,
            67.765625,
            20.0,
        );
        assert_frame(layout.query, 388.765625, 54.5, 467.328125, 30.0);
        assert_frame(layout.list, 271.0, 93.0, 638.0, 144.0);
        assert_frame(layout.groups[0].rows[2], 277.0, 197.0, 626.0, 34.0);
    }

    #[test]
    fn low_window_caps_palette_height() {
        let spec = PaletteSpec {
            category: None,
            escape: ESC,
            list: ListContent::Groups(vec![11, 6]),
            hints: Vec::new(),
        };
        let layout = layout(WINDOW.0, 400.0, &spec);
        assert_frame(layout.panel, 270.0, 46.0, 640.0, 330.0);
        assert!(near(layout.header.height, HEADER_MIN));
        assert!(near(layout.list.height, 330.0 - 2.0 - HEADER_MIN - FOOTER));
    }

    #[test]
    fn small_deficit_shrinks_header_and_list_by_basis() {
        // Недостаёт 7 точек: строка поиска сжимается на 7 · 46 / (46 + 500).
        let (header, list) = shrink(HEADER + 500.0 + FOOTER - 7.0, 500.0);
        assert!(near(header, HEADER - 7.0 * 46.0 / 546.0));
        assert!(near(list, 500.0 - 7.0 * 500.0 / 546.0));
        assert!(near(header + list + FOOTER, HEADER + 500.0 + FOOTER - 7.0));
        assert_eq!(shrink(1000.0, 100.0), (HEADER, 100.0));
    }

    /// Строки `palette-groups` и `palette-tab-narrowed` в системе координат строки (x от 277).
    #[test]
    fn row_parts_follow_passport() {
        let mode = row_parts(626.0, 96.734375, 152.046875, &[35.203125, 17.0]);
        assert_frame(mode.icon, 12.0, 10.0, 14.0, 14.0);
        assert_frame(mode.title, 36.0, 8.0, 96.734375, 18.0);
        assert_frame(mode.detail, 142.734375, 8.0, 152.046875, 18.0);
        assert_frame(mode.keys[0], 558.796875, 8.5, 35.203125, 17.0);
        assert_frame(mode.keys[1], 597.0, 8.5, 17.0, 17.0);

        let zoom = row_parts(626.0, 122.75, 0.0, &[35.203125, 17.0, 17.0]);
        assert_frame(zoom.detail, 168.75, 17.0, 0.0, 0.0);
        assert_frame(zoom.keys[0], 538.796875, 8.5, 35.203125, 17.0);
        assert_frame(zoom.keys[2], 597.0, 8.5, 17.0, 17.0);

        let right = row_parts(626.0, 133.5, 146.21, &[17.0, 17.0, 17.0]);
        assert_frame(right.keys[0], 557.0, 8.5, 17.0, 17.0);
        assert_frame(right.detail, 179.5, 8.0, 146.21, 18.0);
    }

    #[test]
    fn long_texts_are_clipped() {
        let parts = row_parts(626.0, 900.0, 900.0, &[17.0]);
        assert!(near(parts.title.width, 602.0 * TITLE_SHARE));
        let detail_end = parts.detail.x + parts.detail.width;
        assert!(near(detail_end, parts.keys[0].x - ROW_GAP));
        let bare = row_parts(626.0, 50.0, 900.0, &[]);
        assert!(near(bare.detail.x + bare.detail.width, 626.0 - ROW_PADDING));
    }
}
