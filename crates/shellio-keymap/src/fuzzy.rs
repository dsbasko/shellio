//! Нечёткое совпадение палитры и отката командной строки (прототип `S.match`, `util.js:14`).

/// Оценка совпадения `query` с `text` без учёта регистра; 0 — совпадения нет. Подстрока даёт
/// `1000 - позиция`, иначе знаки запроса ищутся по порядку: соседний знак — 6, остальные — 1.
/// Позиции считаются в знаках, а не в байтах.
pub(crate) fn score(text: &str, query: &str) -> u32 {
    if query.is_empty() {
        return 1;
    }
    let text: Vec<char> = text.chars().flat_map(char::to_lowercase).collect();
    let query: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    if let Some(direct) = text.windows(query.len()).position(|window| window == query) {
        return 1000u32.saturating_sub(u32::try_from(direct).unwrap_or(u32::MAX));
    }
    let mut from = 0;
    let mut last: Option<usize> = None;
    let mut total = 0;
    for sign in query {
        let Some(offset) = text
            .get(from..)
            .and_then(|rest| rest.iter().position(|&c| c == sign))
        else {
            return 0;
        };
        let at = from + offset;
        total += if last.is_some_and(|last| last + 1 == at) {
            6
        } else {
            1
        };
        last = Some(at);
        from = at + 1;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substring_wins_by_position() {
        assert_eq!(score("Тёмная тема", ""), 1);
        assert_eq!(score("Тёмная тема", "тём"), 1000);
        assert_eq!(score("Тёмная тема", "ТЕМА"), 993);
        assert_eq!(score("Скрыть нижнюю зону", "нижн"), 993);
        assert_eq!(score("zoom", "zoom"), 1000);
    }

    #[test]
    fn scattered_signs_score_by_adjacency() {
        // «т», затем «м» через знак, затем соседняя «н».
        assert_eq!(score("Тёмная", "тмн"), 1 + 1 + 6);
        // Первый знак на позиции 0 соседом «-1» не считается.
        assert_eq!(score("abc", "ac"), 2);
        assert_eq!(score("a b", "ab"), 2);
        assert_eq!(score("Режим «Ввод»", "рв"), 2);
        assert_eq!(score("Тёмная тема", "тю"), 0);
        assert_eq!(score("abc", "abcd"), 0);
        assert_eq!(score("", "a"), 0);
    }
}
