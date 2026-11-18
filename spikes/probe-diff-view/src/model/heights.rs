//! Индекс высот строк показа на дереве Фенвика: координата → строка показа, строка →
//! координата, правка высоты за O(log n).
//!
//! Высоты — целые точки, координаты — сумма высот в `u64`, поэтому индекс точен и смещения
//! не накапливают ошибку округления. Строка показа занимает свой текст и блоки тредов после
//! неё: раскрытие и свёртка треда правят высоту блока его строки, а не вставляют строку в
//! индекс. Полная перестройка при смене ширины, режима или diff идёт на [`HeightWorker`].

use std::collections::BTreeMap;
use std::io;
use std::ops::Range;
use std::sync::Arc;

use super::rows::{Cancelled, Mode};
use super::wrap::{self, Wrap};
use super::{Diff, DiffSnapshot, FileDiff, TextBody};
use crate::worker::{Cancel, Delivery, Stopped, Worker};

/// Отмена проверяется раз в столько строк показа.
const CANCEL_CHECK_ROWS: u32 = 4096;

/// Высота визуальной строки и перенос области текста одной стороны. В «Рядом» `wrap` — ширина
/// половины.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Metrics {
    pub line_height: u32,
    pub wrap: Wrap,
}

/// Блок тредов после строки diff `line` высотой `height` точек. Строка показа блока — строка,
/// в которой стоит `line` в текущей раскладке, поэтому один список годится обоим режимам.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    pub line: u32,
    pub height: u32,
}

/// Попадание координаты в строку показа: `offset` от верха строки, `in_block` — координата в
/// блоке тредов под текстом строки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub row: u32,
    pub offset: u64,
    pub in_block: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeightIndex {
    text: Vec<u32>,
    block: Vec<u32>,
    /// Дерево Фенвика с 1: `tree[i]` — сумма высот строк `(i - lowbit(i), i]`.
    tree: Vec<u64>,
}

impl Default for HeightIndex {
    fn default() -> Self {
        HeightIndex::from_heights(Vec::new(), Vec::new())
    }
}

impl HeightIndex {
    /// Индекс из высот текста строк и блоков тредов. Недостающие блоки — нулевые, лишние
    /// отбрасываются. Построение за O(n).
    pub fn from_heights(text: Vec<u32>, mut block: Vec<u32>) -> Self {
        block.resize(text.len(), 0);
        let mut tree = Vec::with_capacity(text.len() + 1);
        tree.push(0);
        tree.extend(
            text.iter()
                .zip(&block)
                .map(|(&text, &block)| u64::from(text) + u64::from(block)),
        );
        let n = text.len();
        for i in 1..=n {
            let parent = i + lowbit(i);
            if parent <= n {
                tree[parent] += tree[i];
            }
        }
        HeightIndex { text, block, tree }
    }

    pub fn len(&self) -> u32 {
        self.text.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Высота документа.
    pub fn total(&self) -> u64 {
        self.prefix(self.text.len())
    }

    pub fn text_height(&self, row: u32) -> Option<u32> {
        self.text.get(row as usize).copied()
    }

    pub fn block_height(&self, row: u32) -> Option<u32> {
        self.block.get(row as usize).copied()
    }

    /// Высота строки вместе с блоком тредов под ней.
    pub fn span_height(&self, row: u32) -> Option<u64> {
        Some(u64::from(self.text_height(row)?) + u64::from(self.block_height(row)?))
    }

    /// Верх строки показа. `top(len)` — конец документа.
    pub fn top(&self, row: u32) -> Option<u64> {
        let row = row as usize;
        (row <= self.text.len()).then(|| self.prefix(row))
    }

    /// Строка показа, в которой стоит координата. Строки нулевой высоты координату не
    /// содержат. `None` за концом документа.
    pub fn row_at(&self, y: u64) -> Option<Hit> {
        let n = self.text.len();
        if y >= self.total() {
            return None;
        }
        let mut pos = 0;
        let mut rest = y;
        let mut step = if n == 0 { 0 } else { 1 << n.ilog2() };
        while step > 0 {
            let next = pos + step;
            if let Some(&sum) = self.tree.get(next)
                && sum <= rest
            {
                pos = next;
                rest -= sum;
            }
            step >>= 1;
        }
        let text = u64::from(*self.text.get(pos)?);
        Some(Hit {
            row: pos as u32,
            offset: rest,
            in_block: rest >= text,
        })
    }

    /// Строки показа, которые пересекает полоса `[y, y + height)`.
    pub fn visible(&self, y: u64, height: u64) -> Range<u32> {
        let Some(first) = self.row_at(y) else {
            return self.len()..self.len();
        };
        let last = y
            .saturating_add(height.max(1))
            .saturating_sub(1)
            .min(self.total().saturating_sub(1));
        let end = self.row_at(last).map_or(self.len(), |hit| hit.row + 1);
        first.row..end
    }

    /// Правка высоты текста строки за O(log n). `false` за пределами индекса.
    pub fn set_text_height(&mut self, row: u32, height: u32) -> bool {
        let Some(slot) = self.text.get_mut(row as usize) else {
            return false;
        };
        let delta = u64::from(height).wrapping_sub(u64::from(*slot));
        *slot = height;
        self.add(row as usize, delta);
        true
    }

    /// Правка высоты блока тредов строки за O(log n). `false` за пределами индекса.
    pub fn set_block_height(&mut self, row: u32, height: u32) -> bool {
        let Some(slot) = self.block.get_mut(row as usize) else {
            return false;
        };
        let delta = u64::from(height).wrapping_sub(u64::from(*slot));
        *slot = height;
        self.add(row as usize, delta);
        true
    }

    /// Переводит блоки индекса, построенного со списком `built`, на текущий список `current`:
    /// треды могли раскрыться или свернуться, пока шла перестройка. Правятся только строки
    /// блоков обоих списков.
    pub fn rebase_blocks(
        &mut self,
        body: &TextBody,
        mode: Mode,
        built: &[Block],
        current: &[Block],
    ) {
        let mut sums: BTreeMap<u32, u32> = BTreeMap::new();
        for block in built {
            if let Some(row) = body.row_of_line(mode, block.line) {
                sums.entry(row).or_default();
            }
        }
        for block in current {
            if let Some(row) = body.row_of_line(mode, block.line) {
                let sum = sums.entry(row).or_default();
                *sum = sum.saturating_add(block.height);
            }
        }
        for (row, height) in sums {
            self.set_block_height(row, height);
        }
    }

    fn prefix(&self, count: usize) -> u64 {
        let mut i = count.min(self.text.len());
        let mut sum: u64 = 0;
        while i > 0 {
            sum = sum.wrapping_add(self.tree.get(i).copied().unwrap_or_default());
            i &= i - 1;
        }
        sum
    }

    /// Прибавляет `delta` по модулю 2^64: отрицательная правка — дополнение до двух, суммы
    /// поддеревьев при этом остаются точными.
    fn add(&mut self, row: usize, delta: u64) {
        let mut i = row + 1;
        while let Some(sum) = self.tree.get_mut(i) {
            *sum = sum.wrapping_add(delta);
            i += lowbit(i);
        }
    }
}

fn lowbit(i: usize) -> usize {
    i.isolate_lowest_one()
}

/// Высота текста строки показа: визуальные строки самой длинной половины, умноженные на высоту
/// визуальной строки. Пустая половина «Рядом» высоты не даёт.
pub fn row_text_height(
    diff: &Diff,
    body: &TextBody,
    mode: Mode,
    row: u32,
    metrics: Metrics,
) -> u32 {
    let Some(halves) = body.row(mode, row) else {
        return 0;
    };
    let lines = [halves.old, halves.new]
        .into_iter()
        .flatten()
        .filter_map(|line| body.line(line))
        .map(|line| wrap::visual_lines(diff.line_text(line), metrics.wrap))
        .max()
        .unwrap_or(1);
    metrics.line_height.saturating_mul(lines)
}

/// Полное построение индекса раскладки `mode` текстового файла.
pub fn build(
    diff: &Diff,
    body: &TextBody,
    mode: Mode,
    metrics: Metrics,
    blocks: &[Block],
    cancel: &Cancel,
) -> Result<HeightIndex, Cancelled> {
    let count = body.row_count(mode) as u32;
    let mut text = Vec::with_capacity(count as usize);
    for row in 0..count {
        if row % CANCEL_CHECK_ROWS == 0 && cancel.is_cancelled() {
            return Err(Cancelled);
        }
        text.push(row_text_height(diff, body, mode, row, metrics));
    }
    let mut block = vec![0u32; count as usize];
    for item in blocks {
        if let Some(slot) = body
            .row_of_line(mode, item.line)
            .and_then(|row| block.get_mut(row as usize))
        {
            *slot = slot.saturating_add(item.height);
        }
    }
    Ok(HeightIndex::from_heights(text, block))
}

/// Задание перестройки: файл `file` снимка diff в раскладке `mode` с переносом `metrics` и
/// блоками тредов `blocks`.
#[derive(Clone, Debug)]
pub struct HeightJob {
    pub snapshot: Arc<DiffSnapshot>,
    pub file: usize,
    pub mode: Mode,
    pub metrics: Metrics,
    pub blocks: Arc<[Block]>,
}

/// Неизменяемый индекс высот, который worker отдаёт главному потоку. Получатель сверяет
/// `generation` с текущим поколением worker, `diff_generation`, `file`, `mode` и `metrics` —
/// со своим видом, а блоки тредов переводит на текущие через [`HeightIndex::rebase_blocks`].
#[derive(Debug)]
pub struct HeightSnapshot {
    pub generation: u64,
    pub diff_generation: u64,
    pub file: usize,
    pub mode: Mode,
    pub metrics: Metrics,
    pub blocks: Arc<[Block]>,
    pub index: HeightIndex,
}

/// Перестройка индекса высот на отдельном потоке. Новое задание отменяет прежнее.
pub struct HeightWorker {
    worker: Worker<HeightJob>,
}

impl HeightWorker {
    pub fn spawn(
        deliver: impl Fn(Delivery<Arc<HeightSnapshot>>) + Send + 'static,
    ) -> io::Result<Self> {
        let worker = Worker::spawn("probe-diff-heights", height_job, deliver)?;
        Ok(HeightWorker { worker })
    }

    pub fn submit(&self, job: HeightJob) -> Result<u64, Stopped> {
        self.worker.submit(job)
    }

    pub fn cancel(&self) {
        self.worker.cancel();
    }

    pub fn generation(&self) -> u64 {
        self.worker.generation()
    }
}

fn height_job(job: HeightJob, cancel: &Cancel) -> Option<Arc<HeightSnapshot>> {
    let diff = &job.snapshot.diff;
    let index = match diff.file(job.file).and_then(FileDiff::text) {
        Some(body) => build(diff, body, job.mode, job.metrics, &job.blocks, cancel).ok()?,
        None => HeightIndex::default(),
    };
    Some(Arc::new(HeightSnapshot {
        generation: cancel.generation(),
        diff_generation: job.snapshot.generation,
        file: job.file,
        mode: job.mode,
        metrics: job.metrics,
        blocks: job.blocks,
        index,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;
    use crate::model::parse::parse;
    use crate::model::wrap::TAB_WIDTH;

    const WAIT: Duration = Duration::from_secs(10);

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
    }

    /// Линейный эталон: верх каждой строки и полный перебор попадания.
    struct Linear {
        text: Vec<u32>,
        block: Vec<u32>,
    }

    impl Linear {
        fn span(&self, row: usize) -> u64 {
            u64::from(self.text[row]) + u64::from(self.block[row])
        }

        fn top(&self, row: usize) -> u64 {
            (0..row).map(|r| self.span(r)).sum()
        }

        fn row_at(&self, y: u64) -> Option<Hit> {
            let mut top = 0;
            for row in 0..self.text.len() {
                let span = self.span(row);
                if y < top + span {
                    let offset = y - top;
                    return Some(Hit {
                        row: row as u32,
                        offset,
                        in_block: offset >= u64::from(self.text[row]),
                    });
                }
                top += span;
            }
            None
        }
    }

    fn check_against(index: &HeightIndex, linear: &Linear, samples: impl Iterator<Item = u64>) {
        let n = linear.text.len();
        assert_eq!(index.len() as usize, n);
        assert_eq!(index.total(), linear.top(n));
        for row in 0..=n {
            assert_eq!(index.top(row as u32), Some(linear.top(row)), "верх {row}");
        }
        assert_eq!(index.top(n as u32 + 1), None);
        for y in samples {
            assert_eq!(index.row_at(y), linear.row_at(y), "координата {y}");
        }
    }

    #[test]
    fn matches_linear_reference_on_random_heights() {
        let mut rng = Rng(0x0006_FE11);
        for n in [1usize, 2, 3, 7, 64, 100, 1000] {
            let text: Vec<u32> = (0..n)
                .map(|_| {
                    if rng.below(8) == 0 {
                        0
                    } else {
                        rng.below(60) as u32
                    }
                })
                .collect();
            let block: Vec<u32> = (0..n)
                .map(|_| {
                    if rng.below(10) == 0 {
                        rng.below(200) as u32
                    } else {
                        0
                    }
                })
                .collect();
            let mut index = HeightIndex::from_heights(text.clone(), block.clone());
            let mut linear = Linear { text, block };
            let total = linear.top(n);
            if n <= 100 {
                check_against(&index, &linear, 0..=total + 1);
            } else {
                let samples: Vec<u64> = (0..2000).map(|_| rng.below(total + 2)).collect();
                check_against(&index, &linear, samples.into_iter());
            }
            for _ in 0..200 {
                let row = rng.below(n as u64) as usize;
                let height = rng.below(120) as u32;
                if rng.below(2) == 0 {
                    assert!(index.set_text_height(row as u32, height));
                    linear.text[row] = height;
                } else {
                    assert!(index.set_block_height(row as u32, height));
                    linear.block[row] = height;
                }
                assert_eq!(index.text_height(row as u32), Some(linear.text[row]));
                assert_eq!(index.block_height(row as u32), Some(linear.block[row]));
            }
            let total = linear.top(n);
            let samples: Vec<u64> = (0..500).map(|_| rng.below(total + 2)).collect();
            check_against(
                &index,
                &linear,
                samples
                    .into_iter()
                    .chain([0, total.saturating_sub(1), total]),
            );
        }
    }

    #[test]
    fn boundary_coordinates() {
        let mut index = HeightIndex::from_heights(vec![10, 0, 20, 5], vec![0, 0, 7, 0]);
        assert_eq!(index.total(), 42);
        let hit = |row, offset, in_block| {
            Some(Hit {
                row,
                offset,
                in_block,
            })
        };
        assert_eq!(index.row_at(0), hit(0, 0, false));
        assert_eq!(index.row_at(9), hit(0, 9, false));
        // Ровно на границе — следующая строка с ненулевой высотой, нулевая пропускается.
        assert_eq!(index.row_at(10), hit(2, 0, false));
        assert_eq!(index.row_at(29), hit(2, 19, false));
        assert_eq!(index.row_at(30), hit(2, 20, true));
        assert_eq!(index.row_at(36), hit(2, 26, true));
        assert_eq!(index.row_at(37), hit(3, 0, false));
        assert_eq!(index.row_at(41), hit(3, 4, false));
        assert_eq!(index.row_at(42), None);
        assert_eq!(index.row_at(u64::MAX), None);
        assert_eq!(index.top(1), Some(10));
        assert_eq!(index.top(2), Some(10));
        assert_eq!(index.top(4), Some(42));
        assert_eq!(index.span_height(2), Some(27));
        assert_eq!(index.visible(0, 15), 0..3);
        assert_eq!(index.visible(30, 100), 2..4);
        assert_eq!(index.visible(42, 10), 4..4);
        assert!(!index.set_text_height(4, 1));
        assert!(!index.set_block_height(4, 1));

        let empty = HeightIndex::default();
        assert!(empty.is_empty());
        assert_eq!(empty.total(), 0);
        assert_eq!(empty.row_at(0), None);
        assert_eq!(empty.top(0), Some(0));
        assert_eq!(empty.visible(0, 100), 0..0);
        assert_eq!(HeightIndex::from_heights(vec![], vec![]), empty);
    }

    #[test]
    fn edits_shrink_and_grow_exactly() {
        let mut index = HeightIndex::from_heights(vec![16; 1000], vec![]);
        assert_eq!(index.total(), 16_000);
        index.set_block_height(10, 300);
        assert_eq!(index.top(11), Some(11 * 16 + 300));
        assert_eq!(index.total(), 16_300);
        index.set_block_height(10, 0);
        index.set_text_height(999, 0);
        assert_eq!(index.total(), 15_984);
        assert_eq!(index.row_at(15_983).map(|hit| hit.row), Some(998));
        assert_eq!(index.row_at(15_984), None);
    }

    fn parse_text(text: &str) -> Diff {
        match parse(Arc::from(text.as_bytes()), &Cancel::never()) {
            Ok(diff) => diff,
            Err(failure) => panic!("разбор не прошёл: {failure:?}"),
        }
    }

    /// Ханк: контекст, длинная удалённая строка на 3 визуальные, вставка на 1, контекст.
    fn wrapped_diff() -> Diff {
        let long = "x".repeat(25);
        parse_text(&format!(
            "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-{long}\n+short\n c\n"
        ))
    }

    fn metrics(columns: u32) -> Metrics {
        Metrics {
            line_height: 16,
            wrap: Wrap::new(columns, TAB_WIDTH),
        }
    }

    #[test]
    fn row_heights_follow_wrap_and_pairs() {
        let diff = wrapped_diff();
        let body = diff.files()[0].text().unwrap();
        let never = Cancel::never();
        let unified = build(&diff, body, Mode::Unified, metrics(10), &[], &never).unwrap();
        assert_eq!(
            (0..4)
                .map(|row| unified.text_height(row).unwrap())
                .collect::<Vec<_>>(),
            [16, 48, 16, 16]
        );
        let side = build(&diff, body, Mode::SideBySide, metrics(10), &[], &never).unwrap();
        // Пара «-длинная, +short»: слева 3 визуальные строки, справа 1, высота пары — 3.
        assert_eq!(
            (0..3)
                .map(|row| side.text_height(row).unwrap())
                .collect::<Vec<_>>(),
            [16, 48, 16]
        );
        let wide = build(&diff, body, Mode::SideBySide, metrics(30), &[], &never).unwrap();
        assert_eq!(wide.total(), 3 * 16);
    }

    #[test]
    fn blocks_follow_their_line_in_both_modes() {
        let diff = wrapped_diff();
        let body = diff.files()[0].text().unwrap();
        let never = Cancel::never();
        // Блок после `+short` (строка diff 2) и два блока после `c` (строка diff 3).
        let blocks = [
            Block {
                line: 2,
                height: 40,
            },
            Block {
                line: 3,
                height: 10,
            },
            Block { line: 3, height: 5 },
        ];
        let unified = build(&diff, body, Mode::Unified, metrics(80), &blocks, &never).unwrap();
        assert_eq!(unified.block_height(2), Some(40));
        assert_eq!(unified.block_height(3), Some(15));
        let side = build(&diff, body, Mode::SideBySide, metrics(80), &blocks, &never).unwrap();
        // В «Рядом» `+short` стоит в паре строки 1.
        assert_eq!(side.block_height(1), Some(40));
        assert_eq!(side.block_height(2), Some(15));
        assert_eq!(side.total(), 3 * 16 + 55);
    }

    #[test]
    fn rebase_blocks_matches_rebuild() {
        let diff = wrapped_diff();
        let body = diff.files()[0].text().unwrap();
        let never = Cancel::never();
        let built = [
            Block {
                line: 1,
                height: 30,
            },
            Block {
                line: 3,
                height: 12,
            },
        ];
        let current = [
            Block {
                line: 3,
                height: 12,
            },
            Block { line: 0, height: 7 },
        ];
        for mode in [Mode::Unified, Mode::SideBySide] {
            let mut index = build(&diff, body, mode, metrics(10), &built, &never).unwrap();
            index.rebase_blocks(body, mode, &built, &current);
            let direct = build(&diff, body, mode, metrics(10), &current, &never).unwrap();
            assert_eq!(index, direct, "{mode:?}");
        }
    }

    #[test]
    fn cancelled_build_stops() {
        let diff = wrapped_diff();
        let body = diff.files()[0].text().unwrap();
        assert_eq!(
            build(
                &diff,
                body,
                Mode::Unified,
                metrics(10),
                &[],
                &Cancel::cancelled()
            ),
            Err(Cancelled)
        );
    }

    fn snapshot(diff: Diff, generation: u64) -> Arc<DiffSnapshot> {
        Arc::new(DiffSnapshot { generation, diff })
    }

    #[test]
    fn worker_rebuilds_for_new_width_and_replaces_old_job() {
        let (tx, rx) = mpsc::channel();
        let worker = HeightWorker::spawn(move |delivery| {
            let _ = tx.send(delivery);
        })
        .unwrap();
        let shared = snapshot(wrapped_diff(), 7);
        let job = |columns| HeightJob {
            snapshot: Arc::clone(&shared),
            file: 0,
            mode: Mode::SideBySide,
            metrics: metrics(columns),
            blocks: Arc::from([Block { line: 3, height: 9 }]),
        };
        let first = worker.submit(job(10)).unwrap();
        let second = worker.submit(job(30)).unwrap();
        let delivered = loop {
            let delivery = rx.recv_timeout(WAIT).unwrap();
            if delivery.generation == second {
                break delivery.value;
            }
            assert_eq!(delivery.generation, first);
        };
        assert_eq!(worker.generation(), second);
        assert_eq!(delivered.generation, second);
        assert_eq!(delivered.diff_generation, 7);
        assert_eq!((delivered.file, delivered.mode), (0, Mode::SideBySide));
        assert_eq!(delivered.metrics, metrics(30));
        let body = shared.diff.files()[0].text().unwrap();
        let direct = build(
            &shared.diff,
            body,
            Mode::SideBySide,
            metrics(30),
            &delivered.blocks,
            &Cancel::never(),
        )
        .unwrap();
        assert_eq!(delivered.index, direct);
    }

    #[test]
    fn worker_gives_empty_index_for_file_without_text() {
        let (tx, rx) = mpsc::channel();
        let worker = HeightWorker::spawn(move |delivery| {
            let _ = tx.send(delivery);
        })
        .unwrap();
        let binary = parse_text(
            "diff --git a/b.bin b/b.bin\nindex 1111111..2222222 100644\nBinary files a/b.bin and b/b.bin differ\n",
        );
        worker
            .submit(HeightJob {
                snapshot: snapshot(binary, 1),
                file: 0,
                mode: Mode::Unified,
                metrics: metrics(80),
                blocks: Arc::from([]),
            })
            .unwrap();
        let delivered = rx.recv_timeout(WAIT).unwrap().value;
        assert!(delivered.index.is_empty());
    }
}
