//! Форма тела «Демо: поле» без AppKit: сохранённые данные панели, черновик поля и причина последнего
//! отказа. Черновик живёт, пока живёт тело, и в данные панели попадает только командой `set`.

use shellio_plugin_api::Refusal;

use crate::model::State;

/// Что тело показывает в поле и в подписи.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Form {
    saved: State,
    draft: String,
    /// Почему последний `set` не сохранён.
    failure: Option<String>,
}

impl Form {
    /// Форма новой панели: черновик равен сохранённому значению.
    #[must_use]
    pub fn new(saved: State) -> Self {
        let draft = saved.value.clone();
        Self {
            saved,
            draft,
            failure: None,
        }
    }

    /// Текст поля.
    #[must_use]
    pub fn draft(&self) -> &str {
        &self.draft
    }

    /// Данные панели, которые тело получило последними.
    #[must_use]
    pub fn saved(&self) -> &State {
        &self.saved
    }

    /// Поле редактируется: черновик отличается от сохранённого значения.
    #[must_use]
    pub fn dirty(&self) -> bool {
        self.draft != self.saved.value
    }

    /// Пользователь изменил текст поля.
    pub fn edited(&mut self, text: &str) {
        text.clone_into(&mut self.draft);
    }

    /// `Return`: значение команды `set` — текст поля на момент нажатия.
    pub fn submit(&mut self, text: &str) -> String {
        self.edited(text);
        self.draft.clone()
    }

    /// `Esc`: черновик возвращается к сохранённому значению. Причина отказа описывала отброшенный
    /// черновик и уходит вместе с ним.
    pub fn cancel(&mut self) {
        self.draft.clone_from(&self.saved.value);
        self.failure = None;
    }

    /// Новые данные панели. Черновик следует за ними, только если поле не редактируется. Данные
    /// меняет успешное сохранение, своё или внешнее, поэтому причина прежнего отказа уходит.
    /// `false` — данные прежние.
    pub fn update(&mut self, saved: State) -> bool {
        if self.saved == saved {
            return false;
        }
        let follows = !self.dirty();
        self.saved = saved;
        if follows {
            self.draft.clone_from(&self.saved.value);
        }
        self.failure = None;
        true
    }

    /// Исход команды `set`: отказ оставляет черновик и запоминает причину, успех её убирает.
    pub fn done(&mut self, outcome: Result<(), Refusal>) {
        self.failure = outcome.err().map(|refusal| {
            if refusal.message.is_empty() {
                refusal.reason
            } else {
                refusal.message
            }
        });
    }

    /// Подпись под полем.
    #[must_use]
    pub fn caption(&self) -> String {
        match &self.failure {
            Some(reason) => format!("Не сохранено: {reason}"),
            None => format!(
                "Сохранено: {} · правок {}",
                self.saved.value, self.saved.edits
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(value: &str, edits: u64) -> State {
        State {
            value: value.to_owned(),
            edits,
        }
    }

    fn refusal(message: &str) -> Refusal {
        Refusal::new("value", "value_too_long", message)
    }

    #[test]
    fn new_form_shows_the_saved_value() {
        let form = Form::new(state("shellio", 2));
        assert_eq!(form.draft(), "shellio");
        assert!(!form.dirty());
        assert_eq!(form.caption(), "Сохранено: shellio · правок 2");
        assert_eq!(
            Form::new(State::default()).caption(),
            "Сохранено:  · правок 0"
        );
    }

    #[test]
    fn typing_changes_only_the_draft() {
        let mut form = Form::new(state("a", 1));
        form.edited("ab");
        assert_eq!(form.draft(), "ab");
        assert!(form.dirty());
        assert_eq!(form.saved(), &state("a", 1));
        assert_eq!(form.caption(), "Сохранено: a · правок 1");
    }

    #[test]
    fn return_sends_the_text_of_the_field() {
        let mut form = Form::new(state("a", 1));
        form.edited("ab");
        assert_eq!(form.submit("abc"), "abc", "текст на момент нажатия");
        assert_eq!(form.draft(), "abc");
        // Данные приходят позже: до них подпись прежняя.
        assert_eq!(form.caption(), "Сохранено: a · правок 1");
        assert!(form.update(state("abc", 2)));
        assert!(!form.dirty());
        assert_eq!(form.caption(), "Сохранено: abc · правок 2");
        // Повтор того же текста команду тоже шлёт: исход без ревизии убирает прежний отказ.
        assert_eq!(form.submit("abc"), "abc");
    }

    #[test]
    fn escape_returns_the_draft_to_the_saved_value() {
        let mut form = Form::new(state("a", 1));
        form.edited("черновик");
        form.cancel();
        assert_eq!(form.draft(), "a");
        assert!(!form.dirty());
        assert_eq!(form.saved(), &state("a", 1));

        let mut empty = Form::new(State::default());
        empty.edited("x");
        empty.cancel();
        assert_eq!(empty.draft(), "");
    }

    #[test]
    fn external_change_moves_the_draft_only_when_the_field_is_not_edited() {
        let mut form = Form::new(state("a", 1));
        assert!(form.update(state("b", 2)));
        assert_eq!(form.draft(), "b", "поле не редактируется: черновик следует");
        assert_eq!(form.caption(), "Сохранено: b · правок 2");

        form.edited("мой черновик");
        assert!(form.update(state("c", 3)));
        assert_eq!(form.draft(), "мой черновик", "черновик пользователя цел");
        assert_eq!(form.caption(), "Сохранено: c · правок 3");

        assert!(!form.update(state("c", 3)), "те же данные");
        assert_eq!(form.draft(), "мой черновик");
    }

    #[test]
    fn refusal_keeps_the_draft_and_names_the_reason() {
        let mut form = Form::new(state("a", 1));
        let long = "x".repeat(257);
        assert_eq!(form.submit(&long), long);
        form.done(Err(refusal("значение длиннее 256 символов: 257")));
        assert_eq!(form.draft(), long, "черновик остался в поле");
        assert_eq!(form.saved(), &state("a", 1));
        assert_eq!(
            form.caption(),
            "Не сохранено: значение длиннее 256 символов: 257"
        );

        // Отказ без сообщения называет стабильную причину.
        form.done(Err(Refusal::new("", "busy", "")));
        assert_eq!(form.caption(), "Не сохранено: busy");
    }

    #[test]
    fn next_successful_save_removes_the_reason() {
        let failed = || {
            let mut form = Form::new(state("a", 1));
            form.submit(&"x".repeat(257));
            form.done(Err(refusal("длинно")));
            assert_eq!(form.caption(), "Не сохранено: длинно");
            form
        };

        // Своё сохранение: исход и данные приходят в любом порядке.
        let mut form = failed();
        form.submit("b");
        form.done(Ok(()));
        assert_eq!(form.caption(), "Сохранено: a · правок 1");
        form.update(state("b", 2));
        assert_eq!(form.caption(), "Сохранено: b · правок 2");

        let mut form = failed();
        form.submit("b");
        form.update(state("b", 2));
        assert_eq!(form.caption(), "Сохранено: b · правок 2");
        form.done(Ok(()));
        assert_eq!(form.caption(), "Сохранено: b · правок 2");

        // Внешнее сохранение: значение видно в подписи, черновик из 257 символов цел.
        let mut form = failed();
        form.update(state("проверка", 2));
        assert_eq!(form.caption(), "Сохранено: проверка · правок 2");
        assert_eq!(form.draft().chars().count(), 257);
    }

    #[test]
    fn escape_removes_the_reason_with_the_draft_it_described() {
        let mut form = Form::new(state("a", 1));
        form.submit(&"x".repeat(257));
        form.done(Err(refusal("длинно")));
        form.cancel();
        assert_eq!(form.draft(), "a");
        assert_eq!(form.caption(), "Сохранено: a · правок 1");
    }
}
