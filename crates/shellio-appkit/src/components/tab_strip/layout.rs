//! Геометрия `TabStrip` по прототипу (`.tabbar`, `.tab`, `.ib.sm`, `.res`): режимы по ширине
//! (ADR-0032), части вкладки, кадры полосы и прокрутка к выбранной вкладке. Ширины текста меряет
//! вид, здесь только арифметика.

use crate::chrome_bar::Frame;

/// Отступы содержимого полосы слева и справа (`.tabbar { padding: 0 6px 0 4px }`).
pub const PAD_LEFT: f64 = 4.0;
pub const PAD_RIGHT: f64 = 6.0;
/// Промежуток между вкладками и между частями полосы (`gap: 2px`).
pub const GAP: f64 = 2.0;
/// Линия под полосой (`border-bottom: 1px`).
pub const BOTTOM_LINE: f64 = 1.0;
/// Запас ряда вкладок слева и справа под линию выбранной вкладки (`.tabs { padding: 1px }`).
pub const TABS_INSET: f64 = 1.0;
/// Вкладка: высота и верх в полосе. Содержимое полосы без линии — 33 точки, вкладка 26 стоит по
/// середине с округлением вниз.
pub const TAB_HEIGHT: f64 = 26.0;
pub const TAB_TOP: f64 = 3.0;
/// Части вкладки (`.tab { padding: 0 6px 0 10px; gap: 7px }`).
pub const TAB_PAD_LEFT: f64 = 10.0;
pub const TAB_PAD_RIGHT: f64 = 6.0;
pub const TAB_GAP: f64 = 7.0;
pub const ICON: f64 = 13.0;
pub const DOT: f64 = 7.0;
pub const CLOSE: f64 = 16.0;
/// Кнопка закрытия поверх конца вкладки в боковой зоне (`right: 2px; top: 5px`).
pub const CLOSE_OVERLAY_RIGHT: f64 = 2.0;
/// Кнопка-значок полосы (`.ib.sm`).
pub const BUTTON: f64 = 22.0;
/// Поля нагрузки (`.res { padding: 0 6px }`).
pub const PAYLOAD_PAD: f64 = 6.0;
/// Линия под активной вкладкой: отступ от краёв вкладки и низ на 4 точки ниже вкладки
/// (`.tab.on::after { left: 8px; right: 8px; bottom: -4px }`).
pub const UNDERLINE_INSET: f64 = 8.0;
pub const UNDERLINE_BELOW: f64 = 4.0;

/// Режим полосы по ширине (ADR-0032).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Regime {
    /// Вкладки, нагрузка и кнопки помещаются: всё видно.
    Visible,
    /// Без нагрузки помещается: нагрузка целиком уходит в заголовок меню «…».
    InMenu,
    /// Вкладки прокручиваются, кнопки остаются, нагрузка в меню «…».
    Scroll,
}

impl Regime {
    pub const fn name(self) -> &'static str {
        match self {
            Regime::Visible => "visible",
            Regime::InMenu => "in_menu",
            Regime::Scroll => "scroll",
        }
    }

    /// Нагрузка не в строке, а в заголовке меню «…».
    pub const fn payload_hidden(self) -> bool {
        match self {
            Regime::Visible => false,
            Regime::InMenu | Regime::Scroll => true,
        }
    }
}

/// Ширина ряда вкладок с промежутками между ними.
pub fn tabs_width(widths: &[f64]) -> f64 {
    let gaps = widths.len().saturating_sub(1) as f64 * GAP;
    widths.iter().sum::<f64>() + gaps
}

/// Ширина группы кнопок с промежутками.
pub fn buttons_width(count: usize) -> f64 {
    count as f64 * BUTTON + count.saturating_sub(1) as f64 * GAP
}

/// Режим полосы: `widths` — ширины вкладок, `payload` — ширина нагрузки с полями (`None` — её
/// нет), `buttons` — ширина группы кнопок, `available` — ширина содержимого полосы без отступов.
/// Обрезанная нагрузка не показывается, кнопки не скрываются.
pub fn regime(widths: &[f64], payload: Option<f64>, buttons: f64, available: f64) -> Regime {
    let tabs = tabs_width(widths);
    let without_payload = tabs + GAP + buttons;
    match payload {
        None if without_payload <= available => Regime::Visible,
        None => Regime::Scroll,
        Some(payload) if without_payload + payload + GAP <= available => Regime::Visible,
        Some(_) if without_payload <= available => Regime::InMenu,
        Some(_) => Regime::Scroll,
    }
}

/// Части вкладки в её точках, начало сверху слева.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TabParts {
    pub width: f64,
    pub icon: Option<Frame>,
    pub title: Frame,
    pub dot: Option<Frame>,
    /// Кнопка закрытия: в своём месте или поверх конца вкладки.
    pub close: Frame,
    /// Кнопка закрытия лежит поверх конца вкладки и места не занимает (боковая зона).
    pub close_overlay: bool,
}

fn middle(size: f64) -> f64 {
    ((TAB_HEIGHT - size) / 2.0).floor()
}

/// Части вкладки с заголовком шириной `title_width` и высотой `title_height`: значок типа,
/// заголовок, точка статуса, кнопка закрытия.
pub fn tab_parts(
    title_width: f64,
    title_height: f64,
    icon: bool,
    dot: bool,
    reserve_close: bool,
) -> TabParts {
    let mut x = TAB_PAD_LEFT;
    let icon = icon.then(|| {
        let frame = Frame::new(x, middle(ICON), ICON, ICON);
        x += ICON + TAB_GAP;
        frame
    });
    let title_width = title_width.ceil();
    let title = Frame::new(x, middle(title_height), title_width, title_height);
    x += title_width;
    let dot = dot.then(|| {
        x += TAB_GAP;
        let frame = Frame::new(x, middle(DOT), DOT, DOT);
        x += DOT;
        frame
    });
    let close = if reserve_close {
        x += TAB_GAP;
        let frame = Frame::new(x, middle(CLOSE), CLOSE, CLOSE);
        x += CLOSE;
        frame
    } else {
        Frame::new(0.0, middle(CLOSE), CLOSE, CLOSE)
    };
    let width = x + TAB_PAD_RIGHT;
    let close = if reserve_close {
        close
    } else {
        Frame {
            x: width - CLOSE_OVERLAY_RIGHT - CLOSE,
            ..close
        }
    };
    TabParts {
        width,
        icon,
        title,
        dot,
        close,
        close_overlay: !reserve_close,
    }
}

/// Кадры полосы шириной `width` и высотой `height`.
#[derive(Debug, Clone, PartialEq)]
pub struct StripLayout {
    /// Ширина полосы, по которой посчитана раскладка.
    pub width: f64,
    pub regime: Regime,
    /// Видимая часть ряда вкладок в точках полосы.
    pub clip: Frame,
    /// Вкладки в точках ряда (документа прокрутки).
    pub tabs: Vec<Frame>,
    /// Ширина ряда вкладок с запасом по краям.
    pub content_width: f64,
    /// Наибольшее смещение прокрутки; 0 — ряд помещается.
    pub max_offset: f64,
    /// Нагрузка в строке; `None` — она в меню «…» или её нет.
    pub payload: Option<Frame>,
    /// Кнопки по порядку слева направо.
    pub buttons: Vec<Frame>,
}

/// Кадры полосы: `tabs` — ширины вкладок, `payload` — размер нагрузки с полями, `buttons` — число
/// кнопок. Кнопки прижаты к правому краю, нагрузка — к кнопкам, ряд вкладок начинается слева.
pub fn layout(
    width: f64,
    height: f64,
    tabs: &[f64],
    payload: Option<(f64, f64)>,
    buttons: usize,
) -> StripLayout {
    let content = (height - BOTTOM_LINE).max(0.0);
    let available = (width - PAD_LEFT - PAD_RIGHT).max(0.0);
    let group = buttons_width(buttons);
    let regime = regime(tabs, payload.map(|(w, _)| w), group, available);
    let right = width - PAD_RIGHT;
    let button_y = ((content - BUTTON) / 2.0).floor();
    let first_button = right - group;
    let buttons: Vec<Frame> = (0..buttons)
        .map(|index| {
            Frame::new(
                first_button + index as f64 * (BUTTON + GAP),
                button_y,
                BUTTON,
                BUTTON,
            )
        })
        .collect();
    let payload_frame = match (regime, payload) {
        (Regime::Visible, Some((w, h))) => Some(Frame::new(
            first_button - GAP - w,
            ((content - h) / 2.0).floor(),
            w,
            h,
        )),
        _ => None,
    };
    let mut x = TABS_INSET;
    let frames: Vec<Frame> = tabs
        .iter()
        .map(|&w| {
            let frame = Frame::new(x, TAB_TOP, w, TAB_HEIGHT);
            x += w + GAP;
            frame
        })
        .collect();
    let row = tabs_width(tabs);
    let content_width = row + 2.0 * TABS_INSET;
    let clip_x = PAD_LEFT - TABS_INSET;
    let clip_width = match regime {
        Regime::Visible | Regime::InMenu => content_width,
        Regime::Scroll => (first_button - GAP - PAD_LEFT).max(0.0) + 2.0 * TABS_INSET,
    };
    StripLayout {
        width,
        regime,
        clip: Frame::new(clip_x, 0.0, clip_width, content),
        tabs: frames,
        content_width,
        max_offset: (content_width - clip_width).max(0.0),
        payload: payload_frame,
        buttons,
    }
}

/// Смещение прокрутки в пределах `[0, max_offset]`.
pub fn clamp_offset(offset: f64, max_offset: f64) -> f64 {
    offset.clamp(0.0, max_offset.max(0.0))
}

/// Наименьший сдвиг прокрутки, при котором вкладка `tab` (в точках ряда) видна целиком вместе с
/// запасом под линию; видимая вкладка смещения не меняет. Вкладка, которой с запасом не хватает
/// видимой части ряда, встаёт своим началом к её началу: значок и название видны, конец уходит
/// под кнопки (прототип: `scrollIntoView` с `inline: 'nearest'`).
pub fn reveal(offset: f64, tab: Frame, clip_width: f64, max_offset: f64) -> f64 {
    let left = tab.x - TABS_INSET;
    let right = tab.right() + TABS_INSET;
    let offset = if right - left > clip_width {
        tab.x
    } else if left < offset {
        left
    } else if right > offset + clip_width {
        right - clip_width
    } else {
        offset
    };
    clamp_offset(offset, max_offset)
}

/// Линия под активной вкладкой `tab` толщиной `thickness`, в точках ряда.
pub fn underline(tab: Frame, thickness: f64) -> Frame {
    Frame::new(
        tab.x + UNDERLINE_INSET,
        tab.y + tab.height + UNDERLINE_BELOW - thickness,
        (tab.width - 2.0 * UNDERLINE_INSET).max(0.0),
        thickness,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const THREE: usize = 3;

    #[test]
    fn regime_boundaries_are_exact() {
        let tabs = [100.0, 120.0];
        let row = tabs_width(&tabs);
        assert_eq!(row, 222.0);
        let buttons = buttons_width(THREE);
        assert_eq!(buttons, 70.0);
        let payload = 80.0;
        let visible = row + GAP + payload + GAP + buttons;
        let in_menu = row + GAP + buttons;
        assert_eq!(
            regime(&tabs, Some(payload), buttons, visible),
            Regime::Visible
        );
        assert_eq!(
            regime(&tabs, Some(payload), buttons, visible - 0.5),
            Regime::InMenu,
            "на полточки меньше — нагрузка уходит в меню"
        );
        assert_eq!(
            regime(&tabs, Some(payload), buttons, in_menu),
            Regime::InMenu
        );
        assert_eq!(
            regime(&tabs, Some(payload), buttons, in_menu - 0.5),
            Regime::Scroll
        );
        assert_eq!(regime(&tabs, None, buttons, in_menu), Regime::Visible);
        assert_eq!(regime(&tabs, None, buttons, in_menu - 0.5), Regime::Scroll);
        assert_eq!(regime(&[], Some(payload), buttons, 0.0), Regime::Scroll);
    }

    #[test]
    fn payload_is_shown_whole_or_not_at_all() {
        let tabs = [90.0, 90.0];
        for width in (200..=500).map(f64::from) {
            let strip = layout(width, 34.0, &tabs, Some((84.0, 15.0)), THREE);
            match strip.payload {
                Some(frame) => {
                    assert_eq!(frame.width, 84.0, "нагрузка не обрезается при {width}");
                    assert_eq!(strip.regime, Regime::Visible);
                    assert!(frame.x >= strip.clip.right(), "нагрузка правее вкладок");
                    assert!(frame.right() <= strip.buttons[0].x - GAP);
                }
                None => assert!(strip.regime.payload_hidden()),
            }
        }
    }

    #[test]
    fn buttons_stay_whole_at_every_width() {
        for width in (40..=1200).step_by(7).map(f64::from) {
            for count in [2, THREE] {
                let strip = layout(width, 34.0, &[110.0; 8], Some((70.0, 15.0)), count);
                assert_eq!(strip.buttons.len(), count);
                let last = strip.buttons[count - 1];
                assert_eq!(last.right(), width - PAD_RIGHT, "кнопки у правого края");
                for button in &strip.buttons {
                    assert_eq!((button.width, button.height), (BUTTON, BUTTON));
                    assert_eq!(button.y, 5.0, "по середине содержимого 33");
                }
                if width >= 200.0 {
                    assert!(
                        strip.clip.right() <= strip.buttons[0].x - GAP + TABS_INSET,
                        "ряд вкладок не заходит под кнопки при {width}"
                    );
                }
            }
        }
    }

    #[test]
    fn scroll_regime_clips_the_row_and_allows_reaching_the_last_tab() {
        let tabs = [120.0; 8];
        let strip = layout(560.0, 34.0, &tabs, Some((80.0, 15.0)), THREE);
        assert_eq!(strip.regime, Regime::Scroll);
        assert!(strip.payload.is_none());
        assert_eq!(strip.content_width, tabs_width(&tabs) + 2.0);
        assert!(strip.max_offset > 0.0);
        let last = strip.tabs[7];
        let offset = reveal(0.0, last, strip.clip.width, strip.max_offset);
        assert_eq!(offset, strip.max_offset, "последняя вкладка — в конце ряда");
        assert!(last.x >= offset && last.right() <= offset + strip.clip.width);
        assert_eq!(
            reveal(offset, strip.tabs[0], strip.clip.width, strip.max_offset),
            0.0
        );
        let visible = reveal(0.0, strip.tabs[1], strip.clip.width, strip.max_offset);
        assert_eq!(visible, 0.0, "видимая вкладка смещения не меняет");
        assert_eq!(clamp_offset(-5.0, 10.0), 0.0);
        assert_eq!(clamp_offset(15.0, 10.0), 10.0);
    }

    #[test]
    fn tab_wider_than_the_clip_is_shown_from_its_start() {
        // Паспорт `layout-stub`: полоса 135, три кнопки, вкладка 120 — видимая часть ряда 55.
        let strip = layout(135.0, 34.0, &[120.0], None, THREE);
        assert_eq!(strip.regime, Regime::Scroll);
        assert_eq!(strip.clip, Frame::new(3.0, 0.0, 55.0, 33.0));
        assert_eq!(strip.tabs[0], Frame::new(1.0, TAB_TOP, 120.0, TAB_HEIGHT));
        assert_eq!(strip.max_offset, 67.0);
        let tab = strip.tabs[0];
        let offset = reveal(0.0, tab, strip.clip.width, strip.max_offset);
        assert_eq!(offset, tab.x, "левый конец вкладки, а не правый");
        assert_eq!(
            reveal(strip.max_offset, tab, strip.clip.width, strip.max_offset),
            offset,
            "и после прокрутки к правому концу"
        );
        assert_eq!(
            reveal(offset, tab, strip.clip.width, strip.max_offset),
            offset,
            "смещение устойчиво"
        );
        assert_eq!(
            strip.clip.x + tab.x - offset,
            3.0,
            "вкладка у начала видимой части: в паспорте 663 при полосе с 660"
        );

        // Вторая вкладка шире видимой части: её начало у левого края, первая уходит.
        let two = layout(135.0, 34.0, &[40.0, 120.0], None, THREE);
        let second = two.tabs[1];
        let offset = reveal(0.0, second, two.clip.width, two.max_offset);
        assert_eq!(offset, second.x);
        assert!(offset <= two.max_offset);

        // Вкладке хватает места, а запасу под линию — нет: она видна целиком с начала.
        let tight = layout(141.0, 34.0, &[60.0, 60.0], None, THREE);
        assert_eq!(tight.clip.width, 61.0);
        let second = tight.tabs[1];
        let offset = reveal(0.0, second, tight.clip.width, tight.max_offset);
        assert_eq!(offset, second.x);
        assert!(second.right() <= offset + tight.clip.width);

        // Вкладка, которая помещается с запасом, ведёт себя по-прежнему: наименьший сдвиг.
        let fits = layout(200.0, 34.0, &[60.0, 60.0, 60.0], None, THREE);
        assert_eq!(fits.regime, Regime::Scroll);
        let last = fits.tabs[2];
        let offset = reveal(0.0, last, fits.clip.width, fits.max_offset);
        assert_eq!(offset, last.right() + TABS_INSET - fits.clip.width);
    }

    #[test]
    fn tab_rows_have_gaps_and_whole_point_tops() {
        let strip = layout(1100.0, 34.0, &[100.0, 80.0, 60.0], None, THREE);
        assert_eq!(strip.regime, Regime::Visible);
        assert_eq!(strip.tabs[0].x, TABS_INSET);
        assert_eq!(strip.tabs[1].x, strip.tabs[0].right() + GAP);
        assert_eq!(
            strip.clip.x + TABS_INSET,
            PAD_LEFT,
            "первая вкладка у отступа"
        );
        assert_eq!(strip.clip.height, 33.0);
        for tab in &strip.tabs {
            assert_eq!((tab.y, tab.height), (TAB_TOP, TAB_HEIGHT));
        }
        assert_eq!(strip.max_offset, 0.0);
    }

    #[test]
    fn tab_parts_follow_the_zone() {
        let center = tab_parts(50.0, 16.0, true, true, true);
        assert_eq!(center.icon, Some(Frame::new(10.0, 6.0, ICON, ICON)));
        assert_eq!(center.title.x, 30.0);
        assert_eq!(center.dot.map(|dot| dot.x), Some(87.0));
        assert_eq!(center.close.x, 101.0);
        assert!(!center.close_overlay);
        assert_eq!(center.width, 123.0);
        let side = tab_parts(50.0, 16.0, false, true, false);
        assert!(side.icon.is_none(), "в боковой зоне значка нет");
        assert_eq!(side.title.x, TAB_PAD_LEFT);
        assert_eq!(side.width, 10.0 + 50.0 + 7.0 + 7.0 + 6.0);
        assert!(side.close_overlay);
        assert_eq!(side.close.right(), side.width - CLOSE_OVERLAY_RIGHT);
        let plain = tab_parts(50.4, 16.0, false, false, false);
        assert_eq!(
            plain.width,
            10.0 + 51.0 + 6.0,
            "ширина заголовка вверх до точки"
        );
    }

    #[test]
    fn underline_sits_four_points_below_the_tab() {
        let tab = Frame::new(1.0, TAB_TOP, 100.0, TAB_HEIGHT);
        assert_eq!(underline(tab, 2.0), Frame::new(9.0, 31.0, 84.0, 2.0));
        assert_eq!(
            underline(tab, 3.0),
            Frame::new(9.0, 30.0, 84.0, 3.0),
            "в Increase Contrast толще вверх"
        );
        assert!(underline(tab, 3.0).y + 3.0 <= 33.0, "над линией полосы");
    }
}
