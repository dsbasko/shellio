//! Намерения панели (ADR-0008, «Уточнено в P1.6 (планирование)»): хост переводит действия keymap
//! `list.*` и `panel.filter` в [`ViewIntent`] и отдаёт виду панели с фокусом.

/// Движение списка. Порядок — порядок действий `list.*` в ADR-0010.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ListMove {
    Down,
    Up,
    Top,
    Bottom,
    PageDown,
    PageUp,
    Collapse,
    Expand,
    Activate,
}

impl ListMove {
    pub const ALL: [Self; 9] = [
        Self::Down,
        Self::Up,
        Self::Top,
        Self::Bottom,
        Self::PageDown,
        Self::PageUp,
        Self::Collapse,
        Self::Expand,
        Self::Activate,
    ];
}

/// Намерение панели от хоста.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ViewIntent {
    /// Действие `list.*`: его получает вид плагина с признаком `list_keys`.
    List(ListMove),
    /// Текст строки фильтра хоста, пустой — фильтр снят: его получает вид плагина с признаком
    /// `filter`. Строки сужает плагин.
    Filter { text: String },
}

/// Ответ вида на намерение. Намерение, которое состояние не изменило (`j` на последней строке),
/// тоже принято: клавиша остаётся в панели.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Handled {
    Yes,
    /// Вид намерение не обрабатывает. У плагина, объявившего признак намерения, это ошибка
    /// контракта.
    No,
}

/// Текст фильтра в контракте намерений: непустой текст и снятие фильтра.
const CONTRACT_FILTER: [&str; 2] = ["а", ""];

impl ViewIntent {
    /// Намерения, которые обязан принять вид плагина с признаками манифеста `list_keys` и
    /// `filter`: девять движений списка, текст фильтра и его снятие.
    pub fn declared(list_keys: bool, filter: bool) -> Vec<ViewIntent> {
        let moves = list_keys
            .then_some(ListMove::ALL)
            .into_iter()
            .flatten()
            .map(ViewIntent::List);
        let texts = filter
            .then_some(CONTRACT_FILTER)
            .into_iter()
            .flatten()
            .map(|text| ViewIntent::Filter {
                text: text.to_owned(),
            });
        moves.chain(texts).collect()
    }
}

/// Нарушение контракта намерений: вид ответил [`Handled::No`] на намерение объявленного признака.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntentRefusal {
    /// Случай проверки: зона, размер и уровень панели.
    pub case: String,
    pub intent: ViewIntent,
}

/// Контракт намерений (ADR-0008, «Уточнено в P1.6 (планирование)»): в каждом случае `cases` вид
/// отвечает на каждое намерение `intents`, и ответ [`Handled::No`] — нарушение. `answer` готовит
/// вид к случаю (геометрия зоны, уровень панели) и отдаёт ему намерение. Пустой ответ — контракт
/// выполнен.
pub fn check_intents<C>(
    cases: impl IntoIterator<Item = (String, C)>,
    intents: &[ViewIntent],
    mut answer: impl FnMut(&C, &ViewIntent) -> Handled,
) -> Vec<IntentRefusal> {
    let mut refusals = Vec::new();
    for (case, prepared) in cases {
        for intent in intents {
            if answer(&prepared, intent) == Handled::No {
                refusals.push(IntentRefusal {
                    case: case.clone(),
                    intent: intent.clone(),
                });
            }
        }
    }
    refusals
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use shellio_layout::Zone;

    use super::*;

    fn zones() -> Vec<(String, Zone)> {
        Zone::ALL
            .into_iter()
            .map(|zone| (format!("{zone:?}"), zone))
            .collect()
    }

    #[test]
    fn declared_intents_follow_the_manifest_features() {
        assert_eq!(ViewIntent::declared(false, false), []);
        let moves = ViewIntent::declared(true, false);
        assert_eq!(moves, ListMove::ALL.map(ViewIntent::List));
        let texts = ViewIntent::declared(false, true);
        assert_eq!(
            texts,
            CONTRACT_FILTER.map(|text| ViewIntent::Filter {
                text: text.to_owned()
            })
        );
        let both = ViewIntent::declared(true, true);
        assert_eq!(both.len(), moves.len() + texts.len());
        let distinct: HashSet<&ViewIntent> = both.iter().collect();
        assert_eq!(distinct.len(), both.len());
    }

    #[test]
    fn view_that_accepts_every_intent_in_every_zone_passes() {
        let intents = ViewIntent::declared(true, true);
        let mut asked = 0;
        let refusals = check_intents(zones(), &intents, |_, _| {
            asked += 1;
            Handled::Yes
        });
        assert_eq!(refusals, []);
        assert_eq!(asked, Zone::ALL.len() * intents.len());
    }

    #[test]
    fn declaring_view_that_refuses_an_intent_fails_the_contract() {
        // Вид объявил `list_keys`, но в нижней зоне движение к началу не обрабатывает.
        let intents = ViewIntent::declared(true, false);
        let refusals = check_intents(zones(), &intents, |zone, intent| {
            if *zone == Zone::Bottom && *intent == ViewIntent::List(ListMove::Top) {
                Handled::No
            } else {
                Handled::Yes
            }
        });
        assert_eq!(
            refusals,
            [IntentRefusal {
                case: "Bottom".to_owned(),
                intent: ViewIntent::List(ListMove::Top),
            }]
        );
        // Вид, который не обрабатывает ничего, нарушает контракт в каждой зоне на каждом намерении.
        let all = check_intents(zones(), &intents, |_, _| Handled::No);
        assert_eq!(all.len(), Zone::ALL.len() * intents.len());
        // Без объявленных признаков проверять нечего.
        let none = check_intents(zones(), &ViewIntent::declared(false, false), |_, _| {
            Handled::No
        });
        assert_eq!(none, []);
    }

    #[test]
    fn list_moves_are_nine_distinct_steps() {
        let steps: HashSet<ListMove> = ListMove::ALL.into_iter().collect();
        assert_eq!(steps.len(), 9);
        assert_eq!(ListMove::ALL[0], ListMove::Down);
        assert_eq!(ListMove::ALL[8], ListMove::Activate);
    }
}
