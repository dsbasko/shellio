//! История переходов воркспейса (ADR-0010, «История переходов»): точки и курсор.

/// Глубина истории: точек на воркспейс.
pub const JUMP_DEPTH: usize = 100;

/// Точки переходов по порядку и курсор на текущей. `P` — точка: рабочая область, панель и режим
/// источника; что считать той же точкой, решает её `PartialEq`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JumpList<P> {
    points: Vec<P>,
    cursor: usize,
    depth: usize,
}

impl<P> Default for JumpList<P> {
    fn default() -> Self {
        Self::with_depth(JUMP_DEPTH)
    }
}

impl<P> JumpList<P> {
    pub fn with_depth(depth: usize) -> Self {
        Self {
            points: Vec::new(),
            cursor: 0,
            depth: depth.max(1),
        }
    }

    pub fn points(&self) -> &[P] {
        &self.points
    }

    /// Номер текущей точки. В пустой истории его нет.
    pub fn cursor(&self) -> Option<usize> {
        (!self.points.is_empty()).then_some(self.cursor)
    }

    /// Назад к ближайшей точке, которую принимает `alive`: закрытые панели и панели вне видимых
    /// проектов пропускаются. Нет такой — курсор на месте.
    pub fn back(&mut self, alive: impl Fn(&P) -> bool) -> Option<&P> {
        let target = (0..self.cursor).rev().find(|&at| alive(&self.points[at]))?;
        self.cursor = target;
        self.points.get(target)
    }

    pub fn forward(&mut self, alive: impl Fn(&P) -> bool) -> Option<&P> {
        let target = (self.cursor + 1..self.points.len()).find(|&at| alive(&self.points[at]))?;
        self.cursor = target;
        self.points.get(target)
    }
}

impl<P: PartialEq> JumpList<P> {
    /// Переход пользователя. Точка под курсором повторно не добавляется, поэтому переход по самой
    /// истории новой точки не даёт. Новый переход после возврата отсекает ветку вперёд.
    pub fn visit(&mut self, point: P) {
        if self.points.get(self.cursor) == Some(&point) {
            return;
        }
        self.points.truncate(self.cursor + 1);
        self.points.push(point);
        if self.points.len() > self.depth {
            let extra = self.points.len() - self.depth;
            self.points.drain(..extra);
        }
        self.cursor = self.points.len() - 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(_: &&str) -> bool {
        true
    }

    fn list(points: &[&'static str]) -> JumpList<&'static str> {
        let mut list = JumpList::default();
        for point in points {
            list.visit(*point);
        }
        list
    }

    #[test]
    fn empty_history_goes_nowhere() {
        let mut list: JumpList<&str> = JumpList::default();
        assert_eq!(list.cursor(), None);
        assert_eq!(list.back(all), None);
        assert_eq!(list.forward(all), None);
    }

    #[test]
    fn back_and_forward_walk_the_points() {
        let mut list = list(&["A", "B", "A"]);
        assert_eq!(list.points(), ["A", "B", "A"]);
        assert_eq!(list.cursor(), Some(2));

        assert_eq!(list.back(all), Some(&"B"));
        assert_eq!(list.back(all), Some(&"A"));
        assert_eq!(list.back(all), None);
        assert_eq!(list.cursor(), Some(0));
        assert_eq!(list.forward(all), Some(&"B"));
        assert_eq!(list.forward(all), Some(&"A"));
        assert_eq!(list.forward(all), None);
        assert_eq!(list.cursor(), Some(2));
    }

    #[test]
    fn traversal_adds_no_point() {
        let mut list = list(&["A", "B", "A"]);
        assert_eq!(list.back(all), Some(&"B"));
        // Зеркало сообщает о фокусе на B после перехода по истории.
        list.visit("B");
        assert_eq!(list.points(), ["A", "B", "A"]);
        assert_eq!(list.cursor(), Some(1));
        assert_eq!(list.forward(all), Some(&"A"));
    }

    #[test]
    fn a_new_jump_after_going_back_cuts_the_branch() {
        let mut list = list(&["A", "B", "A"]);
        assert_eq!(list.back(all), Some(&"B"));
        list.visit("C");
        assert_eq!(list.points(), ["A", "B", "C"]);
        assert_eq!(list.forward(all), None);
        assert_eq!(list.back(all), Some(&"B"));
    }

    #[test]
    fn closed_targets_are_skipped() {
        let mut list = list(&["A", "B", "C", "D"]);
        let open = |point: &&str| *point != "B" && *point != "C";
        assert_eq!(list.back(open), Some(&"A"));
        assert_eq!(list.cursor(), Some(0));
        assert_eq!(list.forward(open), Some(&"D"));

        let none = |point: &&str| *point == "D";
        assert_eq!(list.back(none), None);
        assert_eq!(list.cursor(), Some(3));
        // Точка другой рабочей области — обычная точка: её открывает приложение.
        let mut list = list_of_workbenches();
        assert_eq!(list.back(|_| true), Some(&("wb-1", "A")));
    }

    fn list_of_workbenches() -> JumpList<(&'static str, &'static str)> {
        let mut list = JumpList::default();
        list.visit(("wb-1", "A"));
        list.visit(("wb-2", "A"));
        list
    }

    #[test]
    fn depth_is_capped_and_the_cursor_follows() {
        let mut list = JumpList::default();
        for point in 0..JUMP_DEPTH + 5 {
            list.visit(point);
        }
        assert_eq!(list.points().len(), JUMP_DEPTH);
        assert_eq!(list.points()[0], 5);
        assert_eq!(list.cursor(), Some(JUMP_DEPTH - 1));
        assert_eq!(list.back(|_| true), Some(&(JUMP_DEPTH + 3)));

        let mut small = JumpList::with_depth(2);
        for point in ["A", "B", "C"] {
            small.visit(point);
        }
        assert_eq!(small.points(), ["B", "C"]);
        assert_eq!(small.back(all), Some(&"B"));
        assert_eq!(small.back(all), None);
    }
}
