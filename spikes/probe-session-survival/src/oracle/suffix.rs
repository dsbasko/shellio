//! Суффиксы (раздел плана «Оракулы»): после attach один и тот же хвост идёт эталону напрямую и
//! приёмнику через кандидата, затем состояния сравниваются. Так видно состояние, которого formatter
//! не выдаёт и C API не отдаёт: неактивный экран, сохранённый курсор, стек Kitty, отложенный
//! перенос, табуляторы, поля прокрутки и перенос строк при ресайзе.
//!
//! Шаг — вывод нагрузки или ресайз. Кодирование ввода суффиксом не подаётся: оракул снимает его
//! вместе с состоянием (`model::INPUT_PROBES`).

/// Шаг суффикса.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Байты, которые пишет нагрузка.
    Output(Vec<u8>),
    /// Новый размер окна клиента.
    Resize { cols: u16, rows: u16 },
}

/// Именованный суффикс и клетки матрицы, которым он нужен.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suffix {
    pub name: &'static str,
    pub cells: &'static [&'static str],
    pub steps: Vec<Step>,
}

/// Куда подаётся суффикс: эталон напрямую или приёмник через кандидата. Шаг возвращается, когда
/// получатель его обработал.
pub trait Sink {
    fn output(&mut self, bytes: &[u8]) -> Result<(), String>;
    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String>;
}

/// Наименьшая ширина при сжатии суффикса `resize`.
const MIN_RESIZE_COLS: u16 = 10;

/// Каталог суффиксов для окна `cols × rows`. Каждый суффикс печатает метку там, где оказался
/// курсор, поэтому расхождение позиции видно и в клетках.
pub fn catalog(cols: u16, rows: u16) -> Vec<Suffix> {
    let narrow = (cols / 2).max(MIN_RESIZE_COLS).min(cols);
    vec![
        Suffix {
            name: "alt-exit",
            cells: &["B2"],
            steps: vec![Step::Output(b"\x1b[?1049lA".to_vec())],
        },
        Suffix {
            name: "decrc",
            cells: &["B3"],
            steps: vec![Step::Output(b"\x1b8R".to_vec())],
        },
        Suffix {
            name: "kitty-pop",
            cells: &["B4"],
            steps: vec![Step::Output(b"\x1b[<u".to_vec())],
        },
        Suffix {
            name: "pending-wrap",
            cells: &["B6"],
            steps: vec![Step::Output(b"W".to_vec())],
        },
        Suffix {
            name: "tab",
            cells: &["B5"],
            steps: vec![Step::Output(b"\r\tT\tT".to_vec())],
        },
        Suffix {
            name: "margins",
            cells: &["B5"],
            // К верхнему полю (с DECOM — верх области), вниз до нижнего поля, две прокрутки области.
            steps: vec![Step::Output(b"\x1b[H\x1b[999B\n\nM".to_vec())],
        },
        Suffix {
            name: "resize",
            cells: &["B9"],
            steps: vec![
                Step::Resize { cols: narrow, rows },
                Step::Output(b"Z".to_vec()),
                Step::Resize { cols, rows },
            ],
        },
    ]
}

pub fn by_name(name: &str, cols: u16, rows: u16) -> Option<Suffix> {
    catalog(cols, rows)
        .into_iter()
        .find(|suffix| suffix.name == name)
}

/// Шаги по порядку; первая ошибка останавливает подачу.
pub fn apply(steps: &[Step], sink: &mut dyn Sink) -> Result<(), String> {
    for (index, step) in steps.iter().enumerate() {
        match step {
            Step::Output(bytes) => sink.output(bytes),
            Step::Resize { cols, rows } => sink.resize(*cols, *rows),
        }
        .map_err(|err| format!("шаг {index}: {err}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        steps: Vec<Step>,
        fail_at: Option<usize>,
    }

    impl Sink for Recorder {
        fn output(&mut self, bytes: &[u8]) -> Result<(), String> {
            self.push(Step::Output(bytes.to_vec()))
        }

        fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
            self.push(Step::Resize { cols, rows })
        }
    }

    impl Recorder {
        fn push(&mut self, step: Step) -> Result<(), String> {
            if self.fail_at == Some(self.steps.len()) {
                return Err("получатель отказал".to_owned());
            }
            self.steps.push(step);
            Ok(())
        }
    }

    #[test]
    fn catalog_covers_oracle_section_with_unique_names_and_known_cells() {
        let catalog = catalog(80, 24);
        let names: Vec<&str> = catalog.iter().map(|suffix| suffix.name).collect();
        assert_eq!(
            names,
            [
                "alt-exit",
                "decrc",
                "kitty-pop",
                "pending-wrap",
                "tab",
                "margins",
                "resize"
            ]
        );
        for suffix in &catalog {
            assert!(!suffix.steps.is_empty(), "{}", suffix.name);
            for cell in suffix.cells {
                assert!(
                    crate::matrix::is_matrix_cell(cell),
                    "{}: {cell}",
                    suffix.name
                );
            }
            assert_eq!(by_name(suffix.name, 80, 24).as_ref(), Some(suffix));
        }
        assert_eq!(by_name("нет такого", 80, 24), None);
    }

    #[test]
    fn resize_shrinks_and_returns_to_original_size() {
        let Some(resize) = by_name("resize", 80, 24) else {
            panic!("суффикс resize в каталоге");
        };
        assert_eq!(
            resize.steps,
            [
                Step::Resize { cols: 40, rows: 24 },
                Step::Output(b"Z".to_vec()),
                Step::Resize { cols: 80, rows: 24 },
            ]
        );
        let Some(tiny) = by_name("resize", 12, 3) else {
            panic!("суффикс resize в каталоге");
        };
        assert_eq!(tiny.steps[0], Step::Resize { cols: 10, rows: 3 });
        let Some(narrower) = by_name("resize", 8, 3) else {
            panic!("суффикс resize в каталоге");
        };
        assert_eq!(narrower.steps[0], Step::Resize { cols: 8, rows: 3 });
    }

    #[test]
    fn apply_feeds_steps_in_order_and_stops_at_first_error() {
        let steps = vec![
            Step::Output(b"a".to_vec()),
            Step::Resize { cols: 10, rows: 5 },
            Step::Output(b"b".to_vec()),
        ];
        let mut recorder = Recorder::default();
        apply(&steps, &mut recorder).expect("все шаги поданы");
        assert_eq!(recorder.steps, steps);

        let mut failing = Recorder {
            fail_at: Some(1),
            ..Recorder::default()
        };
        assert_eq!(
            apply(&steps, &mut failing),
            Err("шаг 1: получатель отказал".to_owned())
        );
        assert_eq!(failing.steps, steps[..1]);
    }
}
