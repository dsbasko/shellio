//! Значения темы. Цвета, метрики и движение перенесены из `prototype/css/tokens.css` с каскадом
//! ячеек; `panel`, `panel_head`, вуаль и тон режима — из таблицы ADR-0027. Тест
//! `tests/prototype.rs` сверяет каждое значение с источником, который называет [`AppTheme::slots`].

use crate::cell::{Appearance, Cell, Contrast, Motion};
use crate::color::{Srgb8, Tint};

/// Цвета слотов одной ячейки. Все непрозрачны (ADR-0023).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colors {
    /// Фон окна и заголовка.
    pub chrome: Srgb8,
    /// Наведение и включённое состояние кнопок заголовка.
    pub chrome_hi: Srgb8,
    pub chrome_press: Srgb8,
    /// Плавающие поверхности и поле поиска.
    pub surface: Srgb8,
    /// Полоса вкладок и подвалы вне плиток.
    pub head: Srgb8,
    pub head_focus: Srgb8,
    /// Постоянный фон плитки (ADR-0027).
    pub panel: Srgb8,
    /// Постоянный фон полосы вкладок плитки (ADR-0027).
    pub panel_head: Srgb8,
    pub line: Srgb8,
    pub line_2: Srgb8,
    pub text: Srgb8,
    pub text_2: Srgb8,
    pub text_3: Srgb8,
    /// Выделенная строка.
    pub selection: Srgb8,
    pub hover: Srgb8,
    /// Плашка клавиши.
    pub kbd: Srgb8,
    /// Плашка режима Навигации.
    pub accent_nav: Srgb8,
    pub accent_nav_ink: Srgb8,
    /// Цвет режима Навигации: тон фокуса и линия под активной вкладкой.
    pub accent_nav_line: Srgb8,
    pub accent_nav_hover: Srgb8,
    pub accent_nav_press: Srgb8,
    /// Плашка режима Ввода.
    pub accent_input: Srgb8,
    pub accent_input_ink: Srgb8,
    /// Цвет режима Ввода: тон фокуса и линия под активной вкладкой.
    pub accent_input_line: Srgb8,
    pub status_warn: Srgb8,
    pub status_error: Srgb8,
    pub status_ok: Srgb8,
    /// Идущая работа: ровная синяя точка.
    pub status_run: Srgb8,
    pub status_wait_tint: Srgb8,
    pub status_error_tint: Srgb8,
    pub status_ok_tint: Srgb8,
    pub status_info_tint: Srgb8,
}

/// Слой фокуса ячейки (ADR-0027) и линия под активной вкладкой (ADR-0024).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Focus {
    /// Вуаль панели без фокуса.
    pub veil: Tint,
    /// Доля тона цвета режима у панели с фокусом.
    pub tone_alpha: f64,
    /// Толщина линии под активной вкладкой в точках.
    pub underline: f64,
}

/// Радиусы скругления в точках.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Radii {
    pub row: f64,
    pub control: f64,
    pub plate: f64,
    pub float: f64,
    pub pill: f64,
}

/// Кегли в точках.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FontSizes {
    pub caption: f64,
    pub meta: f64,
    pub small: f64,
    pub tab: f64,
    pub body: f64,
    pub title: f64,
    pub large: f64,
}

/// Высоты строк текста в точках.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LineHeights {
    pub caption: f64,
    pub small: f64,
    pub body: f64,
    pub code: f64,
    pub diff: f64,
}

/// Метрики в точках, одинаковые во всех ячейках. Плагины берут их для `adapt` без AppKit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub titlebar: f64,
    pub tabbar: f64,
    pub row: f64,
    pub status_bar: f64,
    /// Отступ между плитками.
    pub gap: f64,
    pub radius: Radii,
    pub font: FontSizes,
    pub line_height: LineHeights,
}

/// Кривая `cubic-bezier` с опорными точками `(x1, y1)` и `(x2, y2)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CubicBezier {
    pub x1: f64,
    pub y1: f64,
    pub x2: f64,
    pub y2: f64,
}

/// Параметры анимаций при данном движении.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MotionTokens {
    pub duration_ms: f64,
    pub easing: CubicBezier,
}

/// Режим клавиатуры ADR-0024: от него зависит цвет фокуса.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyMode {
    Navigation,
    Input,
}

impl KeyMode {
    pub const ALL: [KeyMode; 2] = [KeyMode::Navigation, KeyMode::Input];
}

/// Тема одной ячейки.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AppTheme {
    pub cell: Cell,
    pub colors: Colors,
    pub focus: Focus,
    pub metrics: Metrics,
}

/// Столбец таблицы значений ADR-0027.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Adr0027Column {
    Panel,
    PanelHead,
    Veil,
    Tone,
}

/// Источник значения слота.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    /// Переменная `prototype/css/tokens.css`, значение после каскада ячейки и `var()`.
    Css(&'static str),
    /// Столбец таблицы ADR-0027 в строке ячейки.
    Adr0027(Adr0027Column),
}

/// Значение слота в той форме, в какой его записывает источник.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    Color(Srgb8),
    Tint(Tint),
    /// Доля от 0 до 1.
    Alpha(f64),
    Points(f64),
}

/// Слот темы: имя, источник и значение в ячейке.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Slot {
    pub name: &'static str,
    pub origin: Origin,
    pub value: Value,
}

const DARK: Colors = Colors {
    chrome: Srgb8::hex(0x1E1D1B),
    chrome_hi: Srgb8::hex(0x282725),
    chrome_press: Srgb8::hex(0x312F2C),
    surface: Srgb8::hex(0x191816),
    head: Srgb8::hex(0x201F1D),
    head_focus: Srgb8::hex(0x22211F),
    panel: Srgb8::hex(0x0A0908),
    panel_head: Srgb8::hex(0x11100E),
    line: Srgb8::hex(0x302E2B),
    line_2: Srgb8::hex(0x403D39),
    text: Srgb8::hex(0xD9D4CA),
    text_2: Srgb8::hex(0xABA59A),
    text_3: Srgb8::hex(0x8F897E),
    selection: Srgb8::hex(0x2E2C29),
    hover: Srgb8::hex(0x252421),
    kbd: Srgb8::hex(0x252421),
    accent_nav: Srgb8::hex(0x4E6E96),
    accent_nav_ink: Srgb8::hex(0xF2EFE8),
    accent_nav_line: Srgb8::hex(0x7F9BC0),
    accent_nav_hover: Srgb8::hex(0x45628A),
    accent_nav_press: Srgb8::hex(0x3A5374),
    accent_input: Srgb8::hex(0x7FA383),
    accent_input_ink: Srgb8::hex(0x1A1D19),
    accent_input_line: Srgb8::hex(0x8DB08F),
    status_warn: Srgb8::hex(0xD2A65E),
    status_error: Srgb8::hex(0xD7826F),
    status_ok: Srgb8::hex(0x93B88E),
    status_run: Srgb8::hex(0x8FAACC),
    status_wait_tint: Srgb8::hex(0x2F291F),
    status_error_tint: Srgb8::hex(0x2C201D),
    status_ok_tint: Srgb8::hex(0x1F261E),
    status_info_tint: Srgb8::hex(0x262D36),
};

const LIGHT: Colors = Colors {
    chrome: Srgb8::hex(0xEFEDE8),
    chrome_hi: Srgb8::hex(0xE5E2DC),
    chrome_press: Srgb8::hex(0xDAD6CF),
    surface: Srgb8::hex(0xFAF9F6),
    head: Srgb8::hex(0xF3F1EC),
    head_focus: Srgb8::hex(0xF1EFEA),
    panel: Srgb8::hex(0xFFFFFF),
    panel_head: Srgb8::hex(0xF8F7F3),
    line: Srgb8::hex(0xDDD9D1),
    line_2: Srgb8::hex(0xC9C4BA),
    text: Srgb8::hex(0x2B2925),
    text_2: Srgb8::hex(0x4F4B44),
    text_3: Srgb8::hex(0x66615A),
    selection: Srgb8::hex(0xE0DCD3),
    hover: Srgb8::hex(0xE8E5DF),
    kbd: Srgb8::hex(0xFAF9F6),
    accent_nav: Srgb8::hex(0x48678C),
    accent_nav_ink: Srgb8::hex(0xFFFFFF),
    accent_nav_line: Srgb8::hex(0x48678C),
    accent_nav_hover: Srgb8::hex(0x3F5B7D),
    accent_nav_press: Srgb8::hex(0x3A5374),
    accent_input: Srgb8::hex(0x4D7553),
    accent_input_ink: Srgb8::hex(0xFFFFFF),
    accent_input_line: Srgb8::hex(0x4D7553),
    status_warn: Srgb8::hex(0x855F1E),
    status_error: Srgb8::hex(0xA1493A),
    status_ok: Srgb8::hex(0x4D7553),
    status_run: Srgb8::hex(0x48678C),
    status_wait_tint: Srgb8::hex(0xF5EDDC),
    status_error_tint: Srgb8::hex(0xF4E5E0),
    status_ok_tint: Srgb8::hex(0xE6EEE2),
    status_info_tint: Srgb8::hex(0xE3E9F1),
};

const DARK_INCREASED: Colors = Colors {
    line: Srgb8::hex(0x5C5952),
    line_2: Srgb8::hex(0x7F7A70),
    text_2: Srgb8::hex(0xD0CBC0),
    text_3: Srgb8::hex(0xB3ADA1),
    ..DARK
};

const LIGHT_INCREASED: Colors = Colors {
    line: Srgb8::hex(0x9A958B),
    line_2: Srgb8::hex(0x6F6B62),
    text_2: Srgb8::hex(0x38352F),
    text_3: Srgb8::hex(0x4F4B44),
    ..LIGHT
};

const METRICS: Metrics = Metrics {
    titlebar: 40.0,
    tabbar: 34.0,
    row: 26.0,
    status_bar: 26.0,
    gap: 6.0,
    radius: Radii {
        row: 5.0,
        control: 6.0,
        plate: 7.0,
        float: 10.0,
        pill: 999.0,
    },
    font: FontSizes {
        caption: 11.0,
        meta: 11.5,
        small: 12.0,
        tab: 12.5,
        body: 13.0,
        title: 15.0,
        large: 20.0,
    },
    line_height: LineHeights {
        caption: 14.0,
        small: 16.0,
        body: 18.0,
        code: 20.0,
        diff: 18.0,
    },
};

const EASING: CubicBezier = CubicBezier {
    x1: 0.2,
    y1: 0.8,
    x2: 0.25,
    y2: 1.0,
};

impl AppTheme {
    pub fn for_cell(cell: Cell) -> AppTheme {
        let (colors, focus) = match (cell.appearance, cell.contrast) {
            (Appearance::Dark, Contrast::Normal) => (
                DARK,
                Focus {
                    veil: Tint::new(Srgb8::hex(0x43403A), 0.31),
                    tone_alpha: 0.05,
                    underline: 2.0,
                },
            ),
            (Appearance::Light, Contrast::Normal) => (
                LIGHT,
                Focus {
                    veil: Tint::new(Srgb8::hex(0xCDCAC3), 0.22),
                    tone_alpha: 0.02,
                    underline: 2.0,
                },
            ),
            (Appearance::Dark, Contrast::Increased) => (
                DARK_INCREASED,
                Focus {
                    veil: Tint::new(Srgb8::hex(0xFFFFFF), 0.07),
                    tone_alpha: 0.06,
                    underline: 3.0,
                },
            ),
            (Appearance::Light, Contrast::Increased) => (
                LIGHT_INCREASED,
                Focus {
                    veil: Tint::new(Srgb8::hex(0x43403A), 0.10),
                    tone_alpha: 0.04,
                    underline: 3.0,
                },
            ),
        };
        AppTheme {
            cell,
            colors,
            focus,
            metrics: METRICS,
        }
    }

    /// Параметры анимаций: при Reduce Motion длительность нулевая.
    pub fn motion(motion: Motion) -> MotionTokens {
        let duration_ms = match motion {
            Motion::Normal => 150.0,
            Motion::Reduced => 0.0,
        };
        MotionTokens {
            duration_ms,
            easing: EASING,
        }
    }

    /// Цвет режима: линия под активной вкладкой и тон панели с фокусом.
    pub fn mode_color(&self, mode: KeyMode) -> Srgb8 {
        match mode {
            KeyMode::Navigation => self.colors.accent_nav_line,
            KeyMode::Input => self.colors.accent_input_line,
        }
    }

    /// Тон панели с фокусом.
    pub fn tone(&self, mode: KeyMode) -> Tint {
        Tint::new(self.mode_color(mode), self.focus.tone_alpha)
    }

    /// Все слоты ячейки с источниками. Разбор структур без `..`: новое поле без источника не
    /// собирается.
    pub fn slots(&self) -> Vec<Slot> {
        let AppTheme {
            cell: _,
            colors,
            focus,
            metrics,
        } = *self;
        let Colors {
            chrome,
            chrome_hi,
            chrome_press,
            surface,
            head,
            head_focus,
            panel,
            panel_head,
            line,
            line_2,
            text,
            text_2,
            text_3,
            selection,
            hover,
            kbd,
            accent_nav,
            accent_nav_ink,
            accent_nav_line,
            accent_nav_hover,
            accent_nav_press,
            accent_input,
            accent_input_ink,
            accent_input_line,
            status_warn,
            status_error,
            status_ok,
            status_run,
            status_wait_tint,
            status_error_tint,
            status_ok_tint,
            status_info_tint,
        } = colors;
        let Focus {
            veil,
            tone_alpha,
            underline,
        } = focus;
        let Metrics {
            titlebar,
            tabbar,
            row,
            status_bar,
            gap,
            radius,
            font,
            line_height,
        } = metrics;
        let Radii {
            row: radius_row,
            control: radius_control,
            plate: radius_plate,
            float: radius_float,
            pill: radius_pill,
        } = radius;
        let FontSizes {
            caption: font_caption,
            meta: font_meta,
            small: font_small,
            tab: font_tab,
            body: font_body,
            title: font_title,
            large: font_large,
        } = font;
        let LineHeights {
            caption: line_caption,
            small: line_small,
            body: line_body,
            code: line_code,
            diff: line_diff,
        } = line_height;

        let css = |name, variable, value| Slot {
            name,
            origin: Origin::Css(variable),
            value,
        };
        let adr = |name, column, value| Slot {
            name,
            origin: Origin::Adr0027(column),
            value,
        };
        use Value::{Alpha, Color, Points};
        vec![
            css("chrome", "--chrome", Color(chrome)),
            css("chrome_hi", "--chrome-hi", Color(chrome_hi)),
            css("chrome_press", "--chrome-press", Color(chrome_press)),
            css("surface", "--surface", Color(surface)),
            css("head", "--head", Color(head)),
            css("head_focus", "--head-focus", Color(head_focus)),
            adr("panel", Adr0027Column::Panel, Color(panel)),
            adr("panel_head", Adr0027Column::PanelHead, Color(panel_head)),
            css("line", "--line", Color(line)),
            css("line_2", "--line-2", Color(line_2)),
            css("text", "--text", Color(text)),
            css("text_2", "--text-2", Color(text_2)),
            css("text_3", "--text-3", Color(text_3)),
            css("selection", "--sel", Color(selection)),
            css("hover", "--hover", Color(hover)),
            css("kbd", "--kbd", Color(kbd)),
            css("accent_nav", "--nav-chip", Color(accent_nav)),
            css("accent_nav_ink", "--nav-ink", Color(accent_nav_ink)),
            css("accent_nav_line", "--nav-line", Color(accent_nav_line)),
            css(
                "accent_nav_hover",
                "--nav-chip-hover",
                Color(accent_nav_hover),
            ),
            css(
                "accent_nav_press",
                "--nav-chip-press",
                Color(accent_nav_press),
            ),
            css("accent_input", "--inp-chip", Color(accent_input)),
            css("accent_input_ink", "--inp-ink", Color(accent_input_ink)),
            css("accent_input_line", "--inp-line", Color(accent_input_line)),
            css("status_warn", "--warn", Color(status_warn)),
            css("status_error", "--err", Color(status_error)),
            css("status_ok", "--ok", Color(status_ok)),
            css("status_run", "--run", Color(status_run)),
            css("status_wait_tint", "--wait-tint", Color(status_wait_tint)),
            css("status_error_tint", "--err-tint", Color(status_error_tint)),
            css("status_ok_tint", "--ok-tint", Color(status_ok_tint)),
            css("status_info_tint", "--info-tint", Color(status_info_tint)),
            adr("focus.veil", Adr0027Column::Veil, Value::Tint(veil)),
            adr("focus.tone_alpha", Adr0027Column::Tone, Alpha(tone_alpha)),
            css("focus.underline", "--focus-h", Points(underline)),
            css("metrics.titlebar", "--titlebar", Points(titlebar)),
            css("metrics.tabbar", "--tabbar", Points(tabbar)),
            css("metrics.row", "--row", Points(row)),
            css("metrics.status_bar", "--status", Points(status_bar)),
            css("metrics.gap", "--gap", Points(gap)),
            css("metrics.radius.row", "--r-row", Points(radius_row)),
            css("metrics.radius.control", "--r-ctl", Points(radius_control)),
            css("metrics.radius.plate", "--r-plate", Points(radius_plate)),
            css("metrics.radius.float", "--r-float", Points(radius_float)),
            css("metrics.radius.pill", "--r-pill", Points(radius_pill)),
            css("metrics.font.caption", "--t-cap", Points(font_caption)),
            css("metrics.font.meta", "--t-meta", Points(font_meta)),
            css("metrics.font.small", "--t-sm", Points(font_small)),
            css("metrics.font.tab", "--t-tab", Points(font_tab)),
            css("metrics.font.body", "--t-body", Points(font_body)),
            css("metrics.font.title", "--t-title", Points(font_title)),
            css("metrics.font.large", "--t-large", Points(font_large)),
            css(
                "metrics.line_height.caption",
                "--lh-cap",
                Points(line_caption),
            ),
            css("metrics.line_height.small", "--lh-sm", Points(line_small)),
            css("metrics.line_height.body", "--lh-body", Points(line_body)),
            css("metrics.line_height.code", "--lh-code", Points(line_code)),
            css("metrics.line_height.diff", "--lh-diff", Points(line_diff)),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_names_are_unique_and_cell_independent() {
        let names = |cell| {
            AppTheme::for_cell(cell)
                .slots()
                .iter()
                .map(|slot| slot.name)
                .collect::<Vec<_>>()
        };
        let first = names(Cell::ALL[0]);
        let unique: std::collections::BTreeSet<_> = first.iter().collect();
        assert_eq!(unique.len(), first.len());
        for cell in Cell::ALL {
            assert_eq!(names(cell), first, "{}", cell.name());
        }
    }

    #[test]
    fn mode_color_and_tone_follow_mode() {
        for cell in Cell::ALL {
            let theme = AppTheme::for_cell(cell);
            assert_eq!(
                theme.mode_color(KeyMode::Navigation),
                theme.colors.accent_nav_line
            );
            assert_eq!(
                theme.mode_color(KeyMode::Input),
                theme.colors.accent_input_line
            );
            for mode in KeyMode::ALL {
                let tone = theme.tone(mode);
                assert_eq!(tone.color, theme.mode_color(mode));
                assert_eq!(tone.alpha, theme.focus.tone_alpha);
            }
        }
    }

    #[test]
    fn reduced_motion_has_no_duration() {
        assert_eq!(AppTheme::motion(Motion::Reduced).duration_ms, 0.0);
        assert!(AppTheme::motion(Motion::Normal).duration_ms > 0.0);
    }
}
