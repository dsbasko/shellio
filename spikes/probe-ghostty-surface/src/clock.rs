//! Монотонные отметки `CLOCK_UPTIME_RAW` в наносекундах. Шкала общая у процессов пробы, запуска и
//! фикстуры, поэтому отметки разных процессов вычитаются друг из друга.

pub fn uptime_ns() -> u64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: структура живёт на стеке на время вызова, часы `CLOCK_UPTIME_RAW` есть всегда.
    let status = unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut now) };
    if status != 0 {
        return 0;
    }
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

/// Интервал `from` → `to` в миллисекундах; отрицательный, если `to` раньше.
pub fn interval_ms(from: u64, to: u64) -> f64 {
    (i128::from(to) - i128::from(from)) as f64 / 1e6
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptime_is_monotonic() {
        let first = uptime_ns();
        let second = uptime_ns();
        assert!(first > 0);
        assert!(second >= first);
    }

    #[test]
    fn interval_in_ms_keeps_sign() {
        assert_eq!(interval_ms(1_000_000, 3_500_000), 2.5);
        assert_eq!(interval_ms(3_500_000, 1_000_000), -2.5);
    }
}
