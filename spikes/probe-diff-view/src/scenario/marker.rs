//! Маркер поколения для пути ScreenCaptureKit: полоса чёрных и белых клеток у нижнего края
//! содержимого окна. Клетки: стартовая пара, 16 бит поколения и бит чётности манчестерским кодом
//! (единица — тёмная и светлая, ноль — светлая и тёмная), конечная пара. Неверная пара — ошибка
//! чтения, а не новое поколение. Чистая часть без AppKit: раскладка клеток и чтение из пикселей
//! BGRA кадра потока.

/// Бит поколения в маркере.
pub const BITS: usize = 16;
/// Клеток в полосе: стартовая пара, биты, чётность, конечная пара.
pub const CELLS: usize = 2 + 2 * BITS + 2 + 2;
/// Ширина и высота клетки, точки.
pub const CELL_W: f64 = 10.0;
pub const CELL_H: f64 = 12.0;
/// Отступ полосы от левого края окна, точки: нижние углы окна скруглены и обрезаны.
pub const INSET_X: f64 = 40.0;
/// Яркость ниже — тёмная клетка, выше — светлая, между порогами — ошибка чтения.
pub const DARK_MAX: u8 = 96;
pub const LIGHT_MIN: u8 = 160;

/// Клетки полосы слева направо, `true` — тёмная.
pub fn cells(generation: u64) -> [bool; CELLS] {
    let value = (generation & 0xffff) as u16;
    let mut out = [false; CELLS];
    out[0] = true;
    out[1] = false;
    let mut put = |index: usize, bit: bool| {
        out[2 + 2 * index] = bit;
        out[3 + 2 * index] = !bit;
    };
    for bit in 0..BITS {
        put(bit, value >> bit & 1 == 1);
    }
    put(BITS, value.count_ones() % 2 == 1);
    out[CELLS - 2] = false;
    out[CELLS - 1] = true;
    out
}

/// Поколение по яркости клеток.
pub fn decode(luma: &[u8; CELLS]) -> Result<u16, String> {
    let dark = |index: usize| -> Result<bool, String> {
        match luma[index] {
            value if value <= DARK_MAX => Ok(true),
            value if value >= LIGHT_MIN => Ok(false),
            value => Err(format!("клетка {index}: яркость {value} между порогами")),
        }
    };
    let pair = |first: usize| -> Result<bool, String> {
        let (a, b) = (dark(first)?, dark(first + 1)?);
        if a == b {
            return Err(format!("клетки {first} и {}: пара без перехода", first + 1));
        }
        Ok(a)
    };
    if !pair(0)? || pair(CELLS - 2)? {
        return Err("нет стартовой или конечной пары".to_owned());
    }
    let mut value = 0u16;
    for bit in 0..BITS {
        if pair(2 + 2 * bit)? {
            value |= 1 << bit;
        }
    }
    if pair(2 + 2 * BITS)? != (value.count_ones() % 2 == 1) {
        return Err("чётность не сходится".to_owned());
    }
    Ok(value)
}

/// Кадр потока: пиксели BGRA построчно сверху вниз.
pub struct Frame<'a> {
    pub pixels: &'a [u8],
    pub width: usize,
    pub height: usize,
    pub bytes_per_row: usize,
}

/// Яркость клеток кадра окна размером `window_w` × `window_h` точек. Масштаб — частное ширины
/// кадра и окна, по высоте он обязан совпасть в пределах 1%. Клетка читается в центре.
pub fn sample(frame: &Frame<'_>, window_w: f64, window_h: f64) -> Result<[u8; CELLS], String> {
    if frame.width == 0 || frame.height == 0 || window_w <= 0.0 || window_h <= 0.0 {
        return Err("пустой кадр или окно".to_owned());
    }
    if frame.bytes_per_row < frame.width * 4
        || frame.pixels.len() < frame.bytes_per_row * frame.height
    {
        return Err(format!(
            "буфер {} байт мал для {}×{} при {} байт в строке",
            frame.pixels.len(),
            frame.width,
            frame.height,
            frame.bytes_per_row
        ));
    }
    let scale = frame.width as f64 / window_w;
    let scale_y = frame.height as f64 / window_h;
    if (scale - scale_y).abs() > scale * 0.01 {
        return Err(format!(
            "масштаб кадра {scale:.3} по ширине и {scale_y:.3} по высоте"
        ));
    }
    let row = (frame.height as f64 - CELL_H / 2.0 * scale).floor();
    if row < 0.0 {
        return Err("кадр ниже полосы маркера".to_owned());
    }
    let row = row as usize;
    let mut luma = [0u8; CELLS];
    for (index, out) in luma.iter_mut().enumerate() {
        let x = ((INSET_X + (index as f64 + 0.5) * CELL_W) * scale).floor() as usize;
        if x >= frame.width {
            return Err(format!("клетка {index} за правым краем кадра"));
        }
        let at = row * frame.bytes_per_row + x * 4;
        let (b, g, r) = (
            u32::from(frame.pixels[at]),
            u32::from(frame.pixels[at + 1]),
            u32::from(frame.pixels[at + 2]),
        );
        *out = ((r * 299 + g * 587 + b * 114) / 1000) as u8;
    }
    Ok(luma)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Кадр окна `w` × `h` точек при масштабе `scale` с полосой маркера и запасом строки.
    fn render(
        generation: u64,
        w: f64,
        h: f64,
        scale: f64,
        pad: usize,
    ) -> (Vec<u8>, usize, usize, usize) {
        let width = (w * scale) as usize;
        let height = (h * scale) as usize;
        let bytes_per_row = width * 4 + pad;
        let mut pixels = vec![0x80u8; bytes_per_row * height];
        let cells = cells(generation);
        for y in 0..height {
            let from_bottom = (height - y) as f64 / scale;
            if from_bottom > CELL_H {
                continue;
            }
            for x in 0..width {
                let at_pt = x as f64 / scale - INSET_X;
                if at_pt < 0.0 {
                    continue;
                }
                let index = (at_pt / CELL_W) as usize;
                if index >= CELLS {
                    continue;
                }
                let value = if cells[index] { 0 } else { 255 };
                let at = y * bytes_per_row + x * 4;
                pixels[at..at + 3].fill(value);
                pixels[at + 3] = 255;
            }
        }
        (pixels, width, height, bytes_per_row)
    }

    #[test]
    fn cells_round_trip() {
        for generation in [0u64, 1, 2, 0x5a5a, 0xffff, 0x1_0003] {
            let luma: [u8; CELLS] = cells(generation).map(|dark| if dark { 10 } else { 240 });
            assert_eq!(decode(&luma), Ok((generation & 0xffff) as u16));
        }
    }

    #[test]
    fn broken_cells_are_errors() {
        let mut luma: [u8; CELLS] = cells(7).map(|dark| if dark { 10 } else { 240 });
        luma[4] = luma[5];
        assert!(decode(&luma).is_err());
        let mut gray: [u8; CELLS] = cells(7).map(|dark| if dark { 10 } else { 240 });
        gray[10] = 128;
        assert!(decode(&gray).is_err());
        let mut parity: [u8; CELLS] = cells(7).map(|dark| if dark { 10 } else { 240 });
        let at = 2 + 2 * BITS;
        parity.swap(at, at + 1);
        assert!(decode(&parity).unwrap_err().contains("чётность"));
        let flat = [240u8; CELLS];
        assert!(decode(&flat).is_err());
    }

    #[test]
    fn samples_frames_at_any_scale_and_row_padding() {
        for (scale, pad) in [(1.0, 0), (2.0, 64), (1.0, 32)] {
            let (pixels, width, height, bytes_per_row) = render(0x1234, 1180.0, 748.0, scale, pad);
            let frame = Frame {
                pixels: &pixels,
                width,
                height,
                bytes_per_row,
            };
            let luma = sample(&frame, 1180.0, 748.0).expect("кадр читается");
            assert_eq!(decode(&luma), Ok(0x1234), "масштаб {scale}, запас {pad}");
        }
    }

    #[test]
    fn mismatched_frames_are_errors() {
        let (pixels, width, height, bytes_per_row) = render(3, 1180.0, 748.0, 1.0, 0);
        let frame = Frame {
            pixels: &pixels,
            width,
            height,
            bytes_per_row,
        };
        assert!(
            sample(&frame, 1180.0, 600.0)
                .unwrap_err()
                .contains("масштаб")
        );
        let short = Frame {
            pixels: &pixels[..10],
            width,
            height,
            bytes_per_row,
        };
        assert!(sample(&short, 1180.0, 748.0).is_err());
    }
}
