use std::fmt;

/// Непрозрачный цвет в sRGB, по 8 бит на канал.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Srgb8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Srgb8 {
    pub const fn new(r: u8, g: u8, b: u8) -> Srgb8 {
        Srgb8 { r, g, b }
    }

    /// Цвет из записи `0xRRGGBB`.
    pub const fn hex(value: u32) -> Srgb8 {
        Srgb8 {
            r: (value >> 16) as u8,
            g: (value >> 8) as u8,
            b: value as u8,
        }
    }

    /// Кодированные значения каналов в долях единицы.
    pub fn unit(self) -> UnitRgb {
        UnitRgb {
            r: f64::from(self.r) / 255.0,
            g: f64::from(self.g) / 255.0,
            b: f64::from(self.b) / 255.0,
        }
    }
}

impl fmt::Display for Srgb8 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

/// Цвет в кодированных значениях sRGB без округления до байта: результат смешения слоёв.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UnitRgb {
    pub r: f64,
    pub g: f64,
    pub b: f64,
}

impl UnitRgb {
    /// Округление каналов до 8 бит.
    pub fn to_srgb8(self) -> Srgb8 {
        let channel = |value: f64| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        Srgb8::new(channel(self.r), channel(self.g), channel(self.b))
    }
}

impl From<Srgb8> for UnitRgb {
    fn from(color: Srgb8) -> UnitRgb {
        color.unit()
    }
}

/// Слой цвета с долей непрозрачности: вуаль, тон режима. Доля хранится точно.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tint {
    pub color: Srgb8,
    /// Доля от 0 до 1.
    pub alpha: f64,
}

impl Tint {
    pub const fn new(color: Srgb8, alpha: f64) -> Tint {
        Tint { color, alpha }
    }

    /// Наложение слоя на непрозрачный цвет в кодированных значениях sRGB, как смешивает
    /// композитор окна с `colorSpace = sRGB`.
    pub fn over(self, base: impl Into<UnitRgb>) -> UnitRgb {
        let base = base.into();
        let top = self.color.unit();
        let mix = |under: f64, over: f64| under * (1.0 - self.alpha) + over * self.alpha;
        UnitRgb {
            r: mix(base.r, top.r),
            g: mix(base.g, top.g),
            b: mix(base.b, top.b),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_and_display_round_trip() {
        let color = Srgb8::hex(0x43403A);
        assert_eq!(color, Srgb8::new(0x43, 0x40, 0x3A));
        assert_eq!(color.to_string(), "#43403A");
        assert_eq!(color.unit().to_srgb8(), color);
    }

    #[test]
    fn tint_blends_in_encoded_values() {
        let veil = Tint::new(Srgb8::hex(0x43403A), 0.31);
        let mixed = veil.over(Srgb8::hex(0x0A0908));
        assert!((mixed.r * 255.0 - 27.67).abs() < 1e-9);
        assert!((mixed.g * 255.0 - 26.05).abs() < 1e-9);
        assert!((mixed.b * 255.0 - 23.5).abs() < 1e-9);
        assert_eq!(mixed.to_srgb8().r, 28);
        assert_eq!(mixed.to_srgb8().g, 26);
        assert_eq!(
            Tint::new(Srgb8::hex(0xFFFFFF), 0.0)
                .over(Srgb8::hex(0x102030))
                .to_srgb8(),
            Srgb8::hex(0x102030)
        );
        assert_eq!(
            Tint::new(Srgb8::hex(0xFFFFFF), 1.0)
                .over(Srgb8::hex(0x102030))
                .to_srgb8(),
            Srgb8::hex(0xFFFFFF)
        );
    }
}
