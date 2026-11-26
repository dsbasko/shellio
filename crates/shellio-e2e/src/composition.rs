//! Снимок композиции для поверхности палитры (план `command-overlays`, «Снимок палитры»).
//!
//! Палитра — отдельное окно над окном владельца, поэтому `screencapture -l` окна владельца её
//! не снимает как часть кадра окна. Снимается область экрана по кадру окна владельца
//! (`screencapture -R`): кадр `ui.window` в точках AppKit с началом внизу слева основного экрана
//! переводится в точки экрана с началом вверху слева основного экрана ([`capture_region`]).
//! Пара окон сверяется со списком окон экрана `CGWindowListCopyWindowInfo` ([`judge_pair`]),
//! каретка поля запроса в сравнении стабильности маскируется ([`caret_mask`]).

use serde::Deserialize;

use crate::Error;
use crate::css::Rgb;
use crate::expect::{Geometry, InspectView, OwnedWindow, PixelRect, Rect, WindowView};

/// Узел каретки поля запроса палитры в `ui.inspect`: ключа сверки у него нет.
pub const CARET_COMPONENT: &str = "palette.query.caret";

/// Погрешность сравнения кадров окон в точках.
const FRAME_TOLERANCE: f64 = 1.0;

/// Роль окна палитры в `ui.window.windows`.
const PALETTE_ROLE: &str = "palette";
const MAIN_ROLE: &str = "main";

/// Прямоугольник AppKit (начало внизу слева основного экрана) — в точках экрана с началом
/// вверху слева основного экрана, как у `screencapture -R` и `kCGWindowBounds`. Основной экран —
/// первый в `NSScreen.screens`, его начало — (0, 0).
pub fn capture_region(frame: Rect, primary: Rect) -> Result<Rect, Error> {
    if primary.x.abs() > f64::EPSILON || primary.y.abs() > f64::EPSILON {
        return Err(Error::new(format!(
            "начало основного экрана {}×{}, а не 0×0",
            primary.x, primary.y
        )));
    }
    Ok(Rect::new(
        frame.x,
        primary.height - (frame.y + frame.height),
        frame.width,
        frame.height,
    ))
}

/// Аргумент `-R` для области в точках экрана: `x,y,w,h` целыми точками.
pub fn region_arg(region: Rect) -> Result<String, Error> {
    let whole = [region.x, region.y, region.width, region.height];
    if whole.iter().any(|v| (v - v.round()).abs() > 1e-6) {
        return Err(Error::new(format!(
            "область снимка {region:?} не в целых точках: -R её не снимет ровно"
        )));
    }
    Ok(whole
        .iter()
        .map(|v| format!("{}", v.round() as i64))
        .collect::<Vec<_>>()
        .join(","))
}

/// Область снимка композиции по ответу `ui.window`: кадр главного окна в точках экрана.
pub fn region_of(window: &WindowView) -> Result<Rect, Error> {
    let primary = window
        .screens
        .first()
        .ok_or_else(|| Error::new("в ui.window нет экранов"))?;
    capture_region(window.frame, primary.frame)
}

/// Окно из списка `CGWindowListCopyWindowInfo` в порядке спереди назад.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ListedWindow {
    #[serde(rename = "kCGWindowNumber")]
    pub number: i64,
    #[serde(rename = "kCGWindowOwnerPID")]
    pub pid: u32,
    #[serde(rename = "kCGWindowLayer")]
    pub layer: i64,
    #[serde(rename = "kCGWindowBounds")]
    pub bounds: ListedBounds,
}

/// `kCGWindowBounds`: точки экрана с началом вверху слева основного экрана.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct ListedBounds {
    #[serde(rename = "X")]
    pub x: f64,
    #[serde(rename = "Y")]
    pub y: f64,
    #[serde(rename = "Width")]
    pub width: f64,
    #[serde(rename = "Height")]
    pub height: f64,
}

impl ListedBounds {
    fn rect(self) -> Rect {
        Rect::new(self.x, self.y, self.width, self.height)
    }
}

/// Список окон экрана: массив JSON спереди назад.
pub fn parse_listed(text: &str) -> Result<Vec<ListedWindow>, Error> {
    serde_json::from_str(text).map_err(|e| Error::new(format!("список окон экрана: {e}")))
}

/// Что ждёт снимок от палитры.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    /// Палитра открыта: она над окном владельца.
    Open,
    /// Палитра закрыта: её нет на экране.
    Closed,
}

/// Отказ сверки пары окон.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairFault {
    /// Ошибка исполнения: снимок не годится (чужое окно над областью, разные экраны, неверный
    /// перевод координат).
    Execution(String),
    /// Дефект продукта: `fail` пункта (палитра позади владельца, нет после открытия, видима
    /// после закрытия).
    Product(String),
}

fn close(a: Rect, b: Rect) -> bool {
    (a.x - b.x).abs() <= FRAME_TOLERANCE
        && (a.y - b.y).abs() <= FRAME_TOLERANCE
        && (a.width - b.width).abs() <= FRAME_TOLERANCE
        && (a.height - b.height).abs() <= FRAME_TOLERANCE
}

fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

fn owned<'a>(window: &'a WindowView, role: &str) -> Option<&'a OwnedWindow> {
    window.windows.iter().find(|w| w.role == role)
}

/// Сверка пары окон перед снимком композиции: окно владельца и палитра на одном экране с
/// масштабом `scale`, кадры `ui.window`, переведённые [`capture_region`], совпадают с
/// `kCGWindowBounds` в пределах точки, над областью нет чужого окна обычного уровня, палитра над
/// окном владельца, когда открыта, и отсутствует на экране, когда закрыта. Окна системных
/// уровней (строка меню, Dock) не судятся: их пересечение с областью ловят контрольные пиксели.
pub fn judge_pair(
    window: &WindowView,
    listed: &[ListedWindow],
    scale: f64,
    expect: Expect,
) -> Result<(), PairFault> {
    let execution = |text: String| Err(PairFault::Execution(text));
    let product = |text: String| Err(PairFault::Product(text));
    let Some(pid) = window.pid else {
        return execution("в ui.window нет pid".to_owned());
    };
    let primary = match window.screens.first() {
        Some(screen) => screen.frame,
        None => return execution("в ui.window нет экранов".to_owned()),
    };
    let to_screen = |frame: Rect| capture_region(frame, primary).map_err(|e| e.to_string());
    let Some(main) = owned(window, MAIN_ROLE) else {
        return execution("в ui.window.windows нет главного окна".to_owned());
    };
    let Some(palette) = owned(window, PALETTE_ROLE) else {
        return execution("в ui.window.windows нет окна палитры".to_owned());
    };
    if main.window_number != window.window_number {
        return execution(format!(
            "главное окно списка {} не окно ui.window {}",
            main.window_number, window.window_number
        ));
    }
    if (main.backing_scale_factor - scale).abs() > f64::EPSILON {
        return execution(format!(
            "предусловие: экран — окно владельца на экране с масштабом {}, а не {scale}",
            main.backing_scale_factor
        ));
    }
    let region = match to_screen(main.frame) {
        Ok(region) => region,
        Err(e) => return execution(e),
    };
    let Some(main_at) = listed.iter().position(|w| w.number == main.window_number) else {
        return execution(format!(
            "окна владельца {} нет в списке окон экрана",
            main.window_number
        ));
    };
    let listed_main = &listed[main_at];
    if listed_main.pid != pid || !close(listed_main.bounds.rect(), region) {
        return execution(format!(
            "окно {} списка экрана — PID {}, {:?}; ожидалось PID {pid}, {region:?}",
            main.window_number, listed_main.pid, listed_main.bounds
        ));
    }
    let palette_at = listed
        .iter()
        .position(|w| w.number == palette.window_number);
    for other in &listed[..main_at] {
        if other.layer == 0
            && other.number != palette.window_number
            && overlaps(other.bounds.rect(), region)
        {
            return execution(format!(
                "чужое окно {} (PID {}) над областью снимка {:?}",
                other.number, other.pid, other.bounds
            ));
        }
    }
    match expect {
        Expect::Open => {
            if palette.screen != main.screen {
                return execution(format!(
                    "предусловие: экран — палитра на экране {:?}, окно владельца на {:?}",
                    palette.screen, main.screen
                ));
            }
            let Some(at) = palette_at.filter(|_| palette.visible) else {
                return product("палитры нет на экране после открытия".to_owned());
            };
            let listed_palette = &listed[at];
            let want = match to_screen(palette.frame) {
                Ok(want) => want,
                Err(e) => return execution(e),
            };
            if listed_palette.pid != pid || !close(listed_palette.bounds.rect(), want) {
                return execution(format!(
                    "окно палитры {} списка экрана — PID {}, {:?}; ожидалось PID {pid}, {want:?}",
                    palette.window_number, listed_palette.pid, listed_palette.bounds
                ));
            }
            if at > main_at {
                return product("палитра позади окна владельца".to_owned());
            }
            Ok(())
        }
        Expect::Closed => {
            if palette.visible || palette_at.is_some() {
                product("палитра видима после закрытия".to_owned())
            } else {
                Ok(())
            }
        }
    }
}

/// Пиксели каретки поля запроса: прямоугольник узла [`CARET_COMPONENT`], расширенный на точку
/// и округлённый наружу. Нет узла или он скрыт — маски нет.
pub fn caret_mask(inspect: &InspectView, geometry: &Geometry) -> Vec<PixelRect> {
    inspect
        .components
        .iter()
        .filter(|c| c.id == CARET_COMPONENT && c.visible)
        .filter_map(|c| outward(geometry, geometry.flip(c.frame)))
        .collect()
}

/// Прямоугольник в точках снимка (начало сверху слева) — пиксели наружу с запасом в точку,
/// обрезанные кадром снимка.
fn outward(geometry: &Geometry, rect: Rect) -> Option<PixelRect> {
    let (width, height) = geometry.pixels();
    let s = geometry.scale;
    let x0 = ((rect.x - 1.0) * s).floor().max(0.0);
    let y0 = ((rect.y - 1.0) * s).floor().max(0.0);
    let x1 = ((rect.x + rect.width + 1.0) * s)
        .ceil()
        .min(f64::from(width));
    let y1 = ((rect.y + rect.height + 1.0) * s)
        .ceil()
        .min(f64::from(height));
    (x1 > x0 && y1 > y0).then_some(PixelRect {
        x: x0 as u32,
        y: y0 as u32,
        width: (x1 - x0) as u32,
        height: (y1 - y0) as u32,
    })
}

/// Пиксель внутри одной из масок.
pub fn masked(mask: &[PixelRect], x: u32, y: u32) -> bool {
    mask.iter()
        .any(|r| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
}

/// Число пикселей, изменившихся между снимками больше `tolerance`, и наибольшая разница канала;
/// пиксели масок не сравниваются. `None` у пикселя — он за кадром снимка.
pub fn moved_pixels(
    (width, height): (u32, u32),
    before: impl Fn(u32, u32) -> Option<Rgb>,
    now: impl Fn(u32, u32) -> Option<Rgb>,
    mask: &[PixelRect],
    tolerance: u8,
) -> Result<(u64, u8), Error> {
    let (mut moved, mut worst) = (0_u64, 0_u8);
    for y in 0..height {
        for x in 0..width {
            if masked(mask, x, y) {
                continue;
            }
            let (Some(a), Some(b)) = (before(x, y), now(x, y)) else {
                return Err(Error::new(format!("пиксель {x}×{y} за кадром снимка")));
            };
            let delta = a.max_delta(b);
            if delta > tolerance {
                moved += 1;
                worst = worst.max(delta);
            }
        }
    }
    Ok((moved, worst))
}

/// Пара окон двух снимков серии: палитра та же (номер, кадр, видимость). `Some` — причина
/// повтора.
pub fn pair_changed(previous: &WindowView, window: &WindowView) -> Option<String> {
    let before = owned(previous, PALETTE_ROLE);
    let now = owned(window, PALETTE_ROLE);
    let same = match (before, now) {
        (Some(a), Some(b)) => {
            a.window_number == b.window_number && a.frame == b.frame && a.visible == b.visible
        }
        (None, None) => true,
        _ => false,
    };
    (!same).then(|| format!("окно палитры между снимками изменилось: {before:?} → {now:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::expect::parse_window;

    /// Основной экран 1512×982, второй экран слева от него и выше (начало AppKit −1920, 200).
    const PRIMARY: Rect = Rect::new(0.0, 0.0, 1512.0, 982.0);

    #[test]
    fn owner_frame_is_flipped_against_the_primary_screen() {
        // Окно живого прохода среза 4: (166, 171, 1180×720) — верх на 982 − 891 = 91.
        let region = capture_region(Rect::new(166.0, 171.0, 1180.0, 720.0), PRIMARY);
        assert_eq!(region, Ok(Rect::new(166.0, 91.0, 1180.0, 720.0)));
        assert_eq!(
            region_arg(Rect::new(166.0, 91.0, 1180.0, 720.0)).as_deref(),
            Ok("166,91,1180,720")
        );
    }

    #[test]
    fn window_on_a_secondary_screen_keeps_primary_origin() {
        // Второй экран 1920×1080 слева, его низ на 200 выше низа основного: окно в нём уходит в
        // отрицательные x и y экрана.
        let region = capture_region(Rect::new(-1800.0, 600.0, 1180.0, 720.0), PRIMARY);
        assert_eq!(region, Ok(Rect::new(-1800.0, -338.0, 1180.0, 720.0)));
        assert_eq!(
            region_arg(region.unwrap()).as_deref(),
            Ok("-1800,-338,1180,720")
        );
    }

    #[test]
    fn primary_screen_not_at_origin_and_fractional_region_are_refused() {
        assert!(capture_region(PRIMARY, Rect::new(10.0, 0.0, 1512.0, 982.0)).is_err());
        assert!(region_arg(Rect::new(0.5, 0.0, 10.0, 10.0)).is_err());
    }

    fn window_json(palette_visible: bool) -> WindowView {
        let text = format!(
            r#"{{"window_number":40,"pid":7,"generation":3,
               "frame":{{"x":166,"y":171,"width":1180,"height":720}},
               "backing_scale_factor":2,
               "lightness":{{"value":"light"}},"contrast":{{"value":"normal"}},
               "screens":[{{"index":0,"frame":{{"x":0,"y":0,"width":1512,"height":982}},
                            "backing_scale_factor":2}}],
               "windows":[
                 {{"role":"main","window_number":40,"frame":{{"x":166,"y":171,"width":1180,"height":720}},
                   "backing_scale_factor":2,"visible":true,"key":false,"screen":0}},
                 {{"role":"palette","window_number":41,"frame":{{"x":370,"y":219,"width":772,"height":692}},
                   "backing_scale_factor":2,"visible":{palette_visible},"key":true,"screen":0}}]}}"#
        );
        parse_window(&text).unwrap()
    }

    fn listed(
        number: i64,
        pid: u32,
        layer: i64,
        (x, y, w, h): (f64, f64, f64, f64),
    ) -> ListedWindow {
        ListedWindow {
            number,
            pid,
            layer,
            bounds: ListedBounds {
                x,
                y,
                width: w,
                height: h,
            },
        }
    }

    const MAIN: (f64, f64, f64, f64) = (166.0, 91.0, 1180.0, 720.0);
    // Палитра (370, 219, 772×692): верх 982 − 911 = 71.
    const PALETTE: (f64, f64, f64, f64) = (370.0, 71.0, 772.0, 692.0);

    #[test]
    fn open_palette_above_owner_passes() {
        let list = [
            listed(1, 99, 25, (0.0, 0.0, 1512.0, 33.0)),
            listed(41, 7, 0, PALETTE),
            listed(40, 7, 0, MAIN),
        ];
        assert_eq!(
            judge_pair(&window_json(true), &list, 2.0, Expect::Open),
            Ok(())
        );
    }

    #[test]
    fn palette_behind_absent_or_lingering_is_a_product_fault() {
        let behind = [listed(40, 7, 0, MAIN), listed(41, 7, 0, PALETTE)];
        assert!(matches!(
            judge_pair(&window_json(true), &behind, 2.0, Expect::Open),
            Err(PairFault::Product(_))
        ));
        let absent = [listed(40, 7, 0, MAIN)];
        assert!(matches!(
            judge_pair(&window_json(false), &absent, 2.0, Expect::Open),
            Err(PairFault::Product(_))
        ));
        let lingering = [listed(41, 7, 0, PALETTE), listed(40, 7, 0, MAIN)];
        assert!(matches!(
            judge_pair(&window_json(true), &lingering, 2.0, Expect::Closed),
            Err(PairFault::Product(_))
        ));
        assert_eq!(
            judge_pair(&window_json(false), &absent, 2.0, Expect::Closed),
            Ok(())
        );
    }

    #[test]
    fn foreign_window_wrong_bounds_or_scale_is_an_execution_error() {
        let foreign = [
            listed(9, 55, 0, (1200.0, 700.0, 400.0, 300.0)),
            listed(41, 7, 0, PALETTE),
            listed(40, 7, 0, MAIN),
        ];
        assert!(matches!(
            judge_pair(&window_json(true), &foreign, 2.0, Expect::Open),
            Err(PairFault::Execution(_))
        ));
        // Перевод без переворота: кадр AppKit как есть.
        let unflipped = [
            listed(41, 7, 0, PALETTE),
            listed(40, 7, 0, (166.0, 171.0, 1180.0, 720.0)),
        ];
        assert!(matches!(
            judge_pair(&window_json(true), &unflipped, 2.0, Expect::Open),
            Err(PairFault::Execution(_))
        ));
        let list = [listed(41, 7, 0, PALETTE), listed(40, 7, 0, MAIN)];
        assert!(matches!(
            judge_pair(&window_json(true), &list, 1.0, Expect::Open),
            Err(PairFault::Execution(_))
        ));
        // Чужое окно в стороне от области не мешает.
        let aside = [
            listed(9, 55, 0, (1400.0, 900.0, 100.0, 50.0)),
            listed(41, 7, 0, PALETTE),
            listed(40, 7, 0, MAIN),
        ];
        assert_eq!(
            judge_pair(&window_json(true), &aside, 2.0, Expect::Open),
            Ok(())
        );
    }

    #[test]
    fn caret_mask_covers_its_rect_and_two_pixels_outside_still_count() {
        // Каретка 1×18 в точках снимка (100, 50) при масштабе 2: маска с запасом в точку —
        // столбцы 198..204, строки 98..138.
        let mask = [PixelRect {
            x: 198,
            y: 98,
            width: 6,
            height: 40,
        }];
        let base = |_: u32, _: u32| Some(Rgb::new(255, 255, 255));
        let blink = |x: u32, y: u32| {
            Some(if (200..202).contains(&x) && (100..136).contains(&y) {
                Rgb::new(0, 0, 0)
            } else {
                Rgb::new(255, 255, 255)
            })
        };
        assert_eq!(moved_pixels((300, 200), base, blink, &mask, 3), Ok((0, 0)));
        // Изменение в двух пикселях за краем маски не маскируется.
        let outside = |x: u32, y: u32| {
            Some(if x == 205 && y == 100 {
                Rgb::new(0, 0, 0)
            } else {
                Rgb::new(255, 255, 255)
            })
        };
        assert_eq!(
            moved_pixels((300, 200), base, outside, &mask, 3),
            Ok((1, 255))
        );
        assert_eq!(moved_pixels((300, 200), base, blink, &[], 3), Ok((72, 255)));
    }

    #[test]
    fn caret_node_is_flipped_into_snapshot_pixels_with_a_point_of_margin() {
        let window = window_json(true);
        let inspect = crate::expect::parse_inspect(
            r#"{"window_number":40,"content":{"width":1180,"height":720},"generation":3,
                "components":[
                  {"id":"palette.query.caret","parent":"palette.query","visible":true,
                   "frame":{"x":300,"y":640,"width":1,"height":18}},
                  {"id":"palette.query","parent":null,"visible":true,
                   "frame":{"x":290,"y":636,"width":600,"height":30}}]}"#,
        )
        .unwrap();
        let geometry = Geometry::new(&window, &inspect, (2360, 1440)).unwrap();
        // Верх каретки в точках снимка: 720 − 640 − 18 = 62; с запасом в точку — 61..81, 299..302.
        assert_eq!(
            caret_mask(&inspect, &geometry),
            vec![PixelRect {
                x: 598,
                y: 122,
                width: 6,
                height: 40,
            }]
        );
    }

    #[test]
    fn changed_palette_window_breaks_the_pair() {
        assert_eq!(pair_changed(&window_json(true), &window_json(true)), None);
        assert!(pair_changed(&window_json(true), &window_json(false)).is_some());
    }
}
