//! Сэмплер процесса. Источник — `spikes/probe-appkit-shell/src/scenario/sampler.rs` (P0.2),
//! упрощён: монотонное время `CLOCK_UPTIME_RAW` (те же часы, что у `CACurrentMediaTime` и меток
//! `displayLink`), время CPU процесса `CLOCK_PROCESS_CPUTIME_ID`, `ri_phys_footprint` и
//! `ri_lifetime_max_phys_footprint` из `proc_pid_rusage` с `RUSAGE_INFO_V4`.

use std::mem::MaybeUninit;

use serde::{Deserialize, Serialize};

fn clock_ns(clock: libc::clockid_t, name: &str) -> Result<u64, String> {
    let mut time = MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: `clock_gettime` пишет `timespec` по переданному указателю.
    if unsafe { libc::clock_gettime(clock, time.as_mut_ptr()) } != 0 {
        return Err(format!(
            "clock_gettime({name}): {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: при нулевом коде структура заполнена.
    let time = unsafe { time.assume_init() };
    Ok(time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64)
}

/// Монотонное время без сна, наносекунды.
pub fn uptime_ns() -> Result<u64, String> {
    clock_ns(libc::CLOCK_UPTIME_RAW, "CLOCK_UPTIME_RAW")
}

/// Время CPU процесса, наносекунды.
pub fn cpu_ns() -> Result<u64, String> {
    clock_ns(libc::CLOCK_PROCESS_CPUTIME_ID, "CLOCK_PROCESS_CPUTIME_ID")
}

fn rusage() -> Result<libc::rusage_info_v4, String> {
    let mut info = MaybeUninit::<libc::rusage_info_v4>::uninit();
    // SAFETY: для `RUSAGE_INFO_V4` ядро пишет `rusage_info_v4` по переданному указателю.
    let code = unsafe {
        libc::proc_pid_rusage(
            std::process::id() as libc::c_int,
            libc::RUSAGE_INFO_V4,
            info.as_mut_ptr().cast(),
        )
    };
    if code != 0 {
        return Err(format!(
            "proc_pid_rusage: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: при нулевом коде структура заполнена.
    Ok(unsafe { info.assume_init() })
}

/// Замер процесса в один момент.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    pub mono_ns: u64,
    pub cpu_ns: u64,
    pub footprint_bytes: u64,
    pub lifetime_max_bytes: u64,
}

pub fn sample() -> Result<Sample, String> {
    let mono_ns = uptime_ns()?;
    let cpu_ns = cpu_ns()?;
    let info = rusage()?;
    Ok(Sample {
        mono_ns,
        cpu_ns,
        footprint_bytes: info.ri_phys_footprint,
        lifetime_max_bytes: info.ri_lifetime_max_phys_footprint,
    })
}

/// Загрузка CPU в процентах одного ядра: `100 × Δcpu / Δмонотонного времени`. `None`, если окно
/// пустое.
pub fn cpu_percent(cpu_ns: u64, mono_ns: u64) -> Option<f64> {
    (mono_ns > 0).then(|| 100.0 * cpu_ns as f64 / mono_ns as f64)
}

/// Размер контрольного выделения сверки пика, байты.
pub const CONTROL_BYTES: usize = 64 << 20;

/// Итог сверки `ri_lifetime_max_phys_footprint`: поле есть в `rusage_info_v4` и растёт на
/// контрольном выделении, которое освобождается до следующей выборки.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeakCheck {
    pub before: Sample,
    pub touched: Sample,
    pub after: Sample,
    pub control_bytes: u64,
    /// `None` — сверка прошла, иначе причина `not_run` пика между выборками.
    pub failure: Option<String>,
}

/// Правило сверки: пик вырос не меньше чем на 3/4 контрольного выделения, след после освобождения
/// упал ниже пика, а пик после освобождения не уменьшился.
pub fn judge_peak(before: Sample, touched: Sample, after: Sample, control: u64) -> Option<String> {
    let grown = touched
        .lifetime_max_bytes
        .saturating_sub(before.lifetime_max_bytes);
    if before.lifetime_max_bytes == 0 {
        return Some("ri_lifetime_max_phys_footprint равно нулю".into());
    }
    if grown < control / 4 * 3 {
        return Some(format!(
            "пик вырос на {grown} байт при контрольном выделении {control}"
        ));
    }
    if after.lifetime_max_bytes < touched.lifetime_max_bytes {
        return Some("пик уменьшился после освобождения".into());
    }
    if after.footprint_bytes >= after.lifetime_max_bytes {
        return Some("след после освобождения не ниже пика".into());
    }
    None
}

/// Сверяет `ri_lifetime_max_phys_footprint` на контрольном выделении [`CONTROL_BYTES`]: каждая
/// страница записывается, затем буфер освобождается.
pub fn check_peak() -> Result<PeakCheck, String> {
    let before = sample()?;
    // Отображение без распределителя: `munmap` возвращает страницы ядру сразу, а освобождённый
    // `Vec` распределитель может держать, и след после освобождения не упал бы (прогон
    // калибровки 02:01).
    // SAFETY: анонимное отображение без файла, указатель проверяется на `MAP_FAILED`.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            CONTROL_BYTES,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(format!("mmap: {}", std::io::Error::last_os_error()));
    }
    let page = 4096;
    for offset in (0..CONTROL_BYTES).step_by(page) {
        // SAFETY: смещение внутри отображения длины `CONTROL_BYTES`, запись в каждую страницу.
        unsafe { std::ptr::write_volatile(base.cast::<u8>().add(offset), 1) };
    }
    let touched = sample()?;
    // SAFETY: то же отображение той же длины, после вызова указатель не используется.
    if unsafe { libc::munmap(base, CONTROL_BYTES) } != 0 {
        return Err(format!("munmap: {}", std::io::Error::last_os_error()));
    }
    let after = sample()?;
    let control = CONTROL_BYTES as u64;
    Ok(PeakCheck {
        before,
        touched,
        after,
        control_bytes: control,
        failure: judge_peak(before, touched, after, control),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(footprint: u64, peak: u64) -> Sample {
        Sample {
            mono_ns: 0,
            cpu_ns: 0,
            footprint_bytes: footprint,
            lifetime_max_bytes: peak,
        }
    }

    #[test]
    fn peak_check_sees_the_control_mapping() {
        let check = check_peak().unwrap();
        assert_eq!(check.failure, None, "{check:?}");
    }

    #[test]
    fn cpu_formula() {
        assert_eq!(cpu_percent(50_000_000, 10_000_000_000), Some(0.5));
        assert_eq!(cpu_percent(1, 0), None);
    }

    #[test]
    fn peak_rule() {
        let control = 64 << 20;
        let ok = judge_peak(
            at(10, 100),
            at(10 + control, 100 + control),
            at(20, 100 + control),
            control,
        );
        assert_eq!(ok, None);
        assert!(judge_peak(at(10, 0), at(10, 0), at(10, 0), control).is_some());
        assert!(judge_peak(at(10, 100), at(20, 110), at(20, 110), control).is_some());
        assert!(
            judge_peak(
                at(10, 100),
                at(control, 100 + control),
                at(control, 100),
                control
            )
            .is_some()
        );
        assert!(
            judge_peak(
                at(10, 100),
                at(control, 100 + control),
                at(200 + control, 100 + control),
                control
            )
            .is_some()
        );
    }
}
