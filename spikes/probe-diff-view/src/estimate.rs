//! Оценка `≈` данных модели внутри процесса (ADR-0011): байты по фактической ёмкости буферов, а не
//! по длине. Оценка пишется отдельной строкой и с измеренной памятью процесса не складывается и из
//! неё не вычитается. Деревья tree-sitter оцениваются только подсчётом выделений распределителем,
//! `size_of::<Tree>()` за оценку дерева не выдаётся.

use serde::Serialize;

/// Байты буфера `Vec` по ёмкости.
pub fn vec_bytes<T>(buffer: &Vec<T>) -> u64 {
    (buffer.capacity() as u64).saturating_mul(size_of::<T>() as u64)
}

/// Байты среза без запаса ёмкости: у `Arc<[T]>` и `Box<[T]>` ёмкость равна длине.
pub fn slice_bytes<T>(buffer: &[T]) -> u64 {
    (buffer.len() as u64).saturating_mul(size_of::<T>() as u64)
}

/// Байты строки по ёмкости.
pub fn string_bytes(text: &str, capacity: usize) -> u64 {
    capacity.max(text.len()) as u64
}

/// Одна статья оценки.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Item {
    pub name: &'static str,
    pub bytes: u64,
}

/// Деревья tree-sitter: подсчитаны распределителем или не оценены с причиной.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Trees {
    Counted { bytes: u64 },
    NotEstimated { reason: String },
}

/// Оценка данных модели: статьи по ёмкости буферов и отдельно деревья tree-sitter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Estimate {
    pub items: Vec<Item>,
    pub trees: Trees,
}

impl Estimate {
    pub fn new(trees: Trees) -> Estimate {
        Estimate {
            items: Vec::new(),
            trees,
        }
    }

    /// Добавляет статью. Статьи с одним именем складываются.
    pub fn add(&mut self, name: &'static str, bytes: u64) {
        match self.items.iter_mut().find(|item| item.name == name) {
            Some(item) => item.bytes = item.bytes.saturating_add(bytes),
            None => self.items.push(Item { name, bytes }),
        }
    }

    /// Сумма статей без деревьев: деревья подсчитаны другим способом и в сумму не входят.
    pub fn total(&self) -> u64 {
        self.items
            .iter()
            .fold(0u64, |sum, item| sum.saturating_add(item.bytes))
    }

    /// Строка отчёта: `≈` перед каждым числом, деревья отдельной частью.
    pub fn line(&self) -> String {
        let items = self
            .items
            .iter()
            .map(|item| format!("{} ≈{}", item.name, item.bytes))
            .collect::<Vec<_>>()
            .join(", ");
        let trees = match &self.trees {
            Trees::Counted { bytes } => format!("деревья tree-sitter ≈{bytes} байт"),
            Trees::NotEstimated { reason } => format!("деревья tree-sitter не оценено: {reason}"),
        };
        format!(
            "≈{} байт данных модели по ёмкости буферов ({items}); {trees}",
            self.total()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vec_counts_capacity_not_length() {
        let mut buffer: Vec<u32> = Vec::with_capacity(1000);
        buffer.push(7);
        assert_eq!(vec_bytes(&buffer), buffer.capacity() as u64 * 4);
        assert!(vec_bytes(&buffer) >= 4000);
        let pairs: Vec<(u32, u32)> = Vec::with_capacity(10);
        assert_eq!(vec_bytes(&pairs), pairs.capacity() as u64 * 8);
    }

    #[test]
    fn slices_and_strings() {
        let shared: std::sync::Arc<[u8]> = vec![0u8; 12_345].into();
        assert_eq!(slice_bytes(&shared), 12_345);
        let words: Box<[u64]> = vec![0u64; 3].into_boxed_slice();
        assert_eq!(slice_bytes(&words), 24);
        let mut text = String::with_capacity(64);
        text.push_str("src/lib.rs");
        assert_eq!(string_bytes(&text, text.capacity()), text.capacity() as u64);
    }

    #[test]
    fn total_merges_items_and_leaves_trees_out() {
        let mut estimate = Estimate::new(Trees::Counted { bytes: 1_000_000 });
        estimate.add("blob", 100);
        estimate.add("индекс высот", 40);
        estimate.add("blob", 50);
        assert_eq!(estimate.items.len(), 2);
        assert_eq!(estimate.total(), 190);
        let line = estimate.line();
        assert!(line.starts_with("≈190 байт"), "{line}");
        assert!(line.contains("blob ≈150"), "{line}");
        assert!(line.contains("деревья tree-sitter ≈1000000 байт"), "{line}");
    }

    #[test]
    fn trees_without_counting_are_not_estimated() {
        let estimate = Estimate::new(Trees::NotEstimated {
            reason: "подсчёт выделений не включён".to_owned(),
        });
        assert_eq!(estimate.total(), 0);
        assert!(
            estimate
                .line()
                .contains("деревья tree-sitter не оценено: подсчёт выделений не включён")
        );
    }

    #[test]
    fn model_buffers_are_counted_by_capacity() {
        use crate::model::heights::HeightIndex;
        use crate::model::parse::parse;
        use crate::worker::Cancel;

        let index = HeightIndex::from_heights(vec![17; 1000], Vec::new());
        assert!(index.heap_bytes() >= 1000 * 4 + 1000 * 4 + 1001 * 8);

        let text =
            "diff --git a/f.rs b/f.rs\n--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,2 @@\n a\n-b\n+c\n";
        let source: std::sync::Arc<[u8]> = text.as_bytes().into();
        let diff = parse(source, &Cancel::never()).unwrap();
        let body = diff.file(0).and_then(|file| file.text()).unwrap();
        let lines = body.lines().len() as u64;
        assert_eq!(lines, 3);
        assert!(diff.lines_bytes() >= lines * size_of::<crate::model::DiffLine>() as u64);
        assert!(diff.lines_bytes() >= body.lines_bytes() + "f.rs".len() as u64 * 2);
        assert_eq!(diff.rows_bytes(), body.rows_bytes());
        assert!(body.rows_bytes() > 0);
    }
}
