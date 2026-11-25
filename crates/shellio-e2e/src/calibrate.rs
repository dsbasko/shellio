//! Вывод допусков калибровки (ADR-0037, «Калибровка допусков»): допуск ширины текста и порог
//! маски значка из замеров двух сторон по правилам `conformance/calibration/text.json` и
//! `controls.json`.
//!
//! Модуль чистый: замеры приходят числами. Исход судится теми же правилами, что применяет судья
//! ([`over`], [`mask_within`], [`Tolerances::text_width_limit`]), и по записи, прочитанной обратно
//! [`Tolerances::from_calibration`]: запись и исход не расходятся. Неполный замер и неразделённые
//! контроли — ошибка, допуск и порог не подбираются.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use shellio_conformance::calibration::{Corpus, case_id, cells};

use crate::Error;
use crate::conform::{Tolerances, mask_within, over};

/// Половина корпуса.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Half {
    /// По ней считается допуск.
    Fit,
    /// Отложенная: обязана пройти допуск.
    Holdout,
}

impl Half {
    fn name(self) -> &'static str {
        match self {
            Half::Fit => "fit",
            Half::Holdout => "holdout",
        }
    }
}

/// Отрицательный контроль маски одного размера: эталон — ячейка листа у Chrome, продукт — другая
/// ячейка листа на странице «Калибровка».
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconControl {
    /// `<id контроля>-<размер>`.
    pub id: String,
    pub reference: String,
    pub product: String,
}

/// Что обязан содержать замер: из закреплённых материалов, а не из самого замера.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Ячейки в именах паспорта.
    pub cells: Vec<String>,
    pub text_margin: f64,
    pub mask_margin: f64,
    /// Случаи корпуса `<стиль>.<строка>` с половиной.
    pub cases: BTreeMap<String, Half>,
    /// Случаи контролей ширины `<контроль>.<стиль>.<строка>`.
    pub text_controls: BTreeSet<String>,
    /// Ячейки листа с ролью `primitive`: положительные пары.
    pub primitives: BTreeSet<String>,
    pub icon_controls: Vec<IconControl>,
}

fn field<'a>(place: &str, value: &'a Value, pointer: &str) -> Result<&'a Value, Error> {
    value
        .pointer(pointer)
        .ok_or_else(|| Error::new(format!("{place}: нет поля {pointer}")))
}

fn text_of<'a>(place: &str, value: &'a Value, pointer: &str) -> Result<&'a str, Error> {
    field(place, value, pointer)?
        .as_str()
        .ok_or_else(|| Error::new(format!("{place}: поле {pointer} не строка")))
}

fn list_of<'a>(place: &str, value: &'a Value, pointer: &str) -> Result<&'a Vec<Value>, Error> {
    field(place, value, pointer)?
        .as_array()
        .filter(|list| !list.is_empty())
        .ok_or_else(|| Error::new(format!("{place}: поле {pointer} не список или пусто")))
}

fn margin_of(place: &str, value: &Value, pointer: &str) -> Result<f64, Error> {
    field(place, value, pointer)?
        .as_f64()
        .filter(|v| v.is_finite() && *v > 0.0)
        .ok_or_else(|| Error::new(format!("{place}: поле {pointer} не положительное число")))
}

impl Plan {
    /// План замера из `text.json`, `controls.json` и ячеек листа с ролью `primitive`. Контроли
    /// перечисляются списками `controls` файла: контроль `other-primitive` сравнивает две ячейки с
    /// ролью `primitive`, по роли ячейки его не найти.
    pub fn parse(text: &str, controls: &str, primitives: &[String]) -> Result<Plan, Error> {
        let corpus = Corpus::parse("text.json", text).map_err(|e| Error::new(e.to_string()))?;
        let rules: Value =
            serde_json::from_str(text).map_err(|e| Error::new(format!("text.json: {e}")))?;
        let fit = text_of("text.json", &rules, "/tolerance/fit_half")?;
        let holdout = text_of("text.json", &rules, "/tolerance/holdout_half")?;
        let half = |name: &str| match name {
            n if n == fit => Ok(Half::Fit),
            n if n == holdout => Ok(Half::Holdout),
            other => Err(Error::new(format!("text.json: половина {other}"))),
        };
        let mut cases = BTreeMap::new();
        for (style, line) in corpus.cases() {
            cases.insert(case_id(style, line), half(&line.half)?);
        }

        let file: Value = serde_json::from_str(controls)
            .map_err(|e| Error::new(format!("controls.json: {e}")))?;
        let place = "controls.json";
        let cells = cells(place, controls)
            .map_err(|e| Error::new(e.to_string()))?
            .into_iter()
            .map(|cell| cell.name().to_owned())
            .collect();
        let mut text_controls = BTreeSet::new();
        for control in list_of(place, &file, "/text/controls")? {
            let id = text_of(place, control, "/id")?;
            for style in list_of(place, control, "/styles")? {
                let style = style
                    .as_str()
                    .ok_or_else(|| Error::new(format!("{place}: стиль контроля {id} не строка")))?;
                let found = corpus
                    .styles
                    .iter()
                    .find(|s| s.id == style)
                    .ok_or_else(|| Error::new(format!("{place}: стиля {style} нет в корпусе")))?;
                for line in corpus.strings.iter().filter(|line| line.half == holdout) {
                    text_controls.insert(format!("{id}.{}", case_id(found, line)));
                }
            }
        }
        let mut icon_controls = Vec::new();
        for control in list_of(place, &file, "/icons/controls")? {
            let id = text_of(place, control, "/id")?;
            for size in list_of(place, &file, "/icons/sizes")? {
                let size = size
                    .as_u64()
                    .ok_or_else(|| Error::new(format!("{place}: размер листа не целое")))?;
                let cell = |pointer: &str| {
                    text_of(place, control, pointer).map(|t| t.replace("{size}", &size.to_string()))
                };
                icon_controls.push(IconControl {
                    id: format!("{id}-{size}"),
                    reference: cell("/reference")?,
                    product: cell("/product")?,
                });
            }
        }
        if primitives.is_empty() {
            return Err(Error::new("лист без ячеек с ролью primitive"));
        }
        Ok(Plan {
            cells,
            text_margin: margin_of("text.json", &rules, "/tolerance/margin_pt")?,
            mask_margin: margin_of(place, &file, "/icons/threshold_margin")?,
            cases,
            text_controls,
            primitives: primitives.iter().cloned().collect(),
            icon_controls,
        })
    }
}

/// Ширина одной строки у двух сторон, точки.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Width {
    pub prototype: f64,
    pub product: f64,
}

impl Width {
    fn gap(self) -> f64 {
        (self.prototype - self.product).abs()
    }
}

/// Замеры двух сторон по ячейкам. Ключи вложенных таблиц — имена плана: случай корпуса, случай
/// контроля ширины, ячейка листа, контроль маски.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Measured {
    pub text: BTreeMap<String, BTreeMap<String, Width>>,
    pub text_controls: BTreeMap<String, BTreeMap<String, Width>>,
    /// Пересечение к объединению маски Chrome и маски продукта одной ячейки листа.
    pub positives: BTreeMap<String, BTreeMap<String, f64>>,
    /// Пересечение к объединению эталона контроля и ячейки продукта контроля.
    pub icon_controls: BTreeMap<String, BTreeMap<String, f64>>,
    /// Условие двух тонов ячейки.
    pub two_tone: BTreeMap<String, bool>,
}

/// Замер назван по ячейкам плана: ни пропуска, ни лишней.
fn cells_match<V>(what: &str, plan: &Plan, table: &BTreeMap<String, V>) -> Result<(), Error> {
    let cells: BTreeSet<&str> = plan.cells.iter().map(String::as_str).collect();
    let got: BTreeSet<&str> = table.keys().map(String::as_str).collect();
    if cells == got {
        Ok(())
    } else {
        Err(Error::new(format!(
            "{what}: ячейки замера {got:?}, а не {cells:?}"
        )))
    }
}

/// Замер покрывает ровно план в каждой ячейке: ни пропуска, ни лишнего.
fn complete<V>(
    what: &str,
    plan: &Plan,
    table: &BTreeMap<String, BTreeMap<String, V>>,
    wanted: &BTreeSet<String>,
) -> Result<(), Error> {
    cells_match(what, plan, table)?;
    for (cell, inner) in table {
        let got: BTreeSet<String> = inner.keys().cloned().collect();
        if let Some(missing) = wanted.difference(&got).next() {
            return Err(Error::new(format!("{what}: в {cell} нет замера {missing}")));
        }
        if let Some(extra) = got.difference(wanted).next() {
            return Err(Error::new(format!(
                "{what}: в {cell} замер {extra} вне плана"
            )));
        }
    }
    Ok(())
}

fn widths(what: &str, table: &BTreeMap<String, BTreeMap<String, Width>>) -> Result<(), Error> {
    let good = |v: f64| v.is_finite() && v > 0.0;
    match flat(table).find(|(_, _, w)| !good(w.prototype) || !good(w.product)) {
        Some((cell, case, w)) => Err(Error::new(format!(
            "{what}: {cell} {case}: ширина {} и {} не положительное число",
            w.prototype, w.product
        ))),
        None => Ok(()),
    }
}

fn ratios(what: &str, table: &BTreeMap<String, BTreeMap<String, f64>>) -> Result<(), Error> {
    match flat(table).find(|(_, _, iou)| !(0.0..=1.0).contains(iou)) {
        Some((cell, id, iou)) => Err(Error::new(format!(
            "{what}: {cell} {id}: мера {iou} вне отрезка от 0 до 1"
        ))),
        None => Ok(()),
    }
}

fn flat<V: Copy>(
    table: &BTreeMap<String, BTreeMap<String, V>>,
) -> impl Iterator<Item = (&str, &str, V)> {
    table.iter().flat_map(|(cell, inner)| {
        inner
            .iter()
            .map(move |(key, value)| (cell.as_str(), key.as_str(), *value))
    })
}

fn nested<V: Copy, T: Into<Value>>(
    table: &BTreeMap<String, BTreeMap<String, V>>,
    value: impl Fn(&str, V) -> T,
) -> Value {
    Value::Object(
        table
            .iter()
            .map(|(cell, inner)| {
                let inner = inner
                    .iter()
                    .map(|(key, v)| (key.clone(), value(key, *v).into()))
                    .collect();
                (cell.clone(), Value::Object(inner))
            })
            .collect(),
    )
}

/// Сколько худших случаев половины `fit` называет отказ.
const WORST: usize = 5;

/// Запись `calibration.json` по замерам: допуск ширины — наибольшая разница половины `fit` плюс
/// запас, порог маски — наименьшая мера положительных пар минус запас. Калибровка состоялась,
/// только если отложенная половина проходит допуск, каждый контроль ширины не проходит то, что
/// применяет судья, каждая положительная пара проходит порог, каждый контроль маски его не
/// проходит и условие двух тонов выполнено в каждой ячейке. Иначе — ошибка с числами.
pub fn derive(plan: &Plan, measured: &Measured) -> Result<String, Error> {
    let case_keys: BTreeSet<String> = plan.cases.keys().cloned().collect();
    let control_keys: BTreeSet<String> = plan.icon_controls.iter().map(|c| c.id.clone()).collect();
    complete("ширина текста", plan, &measured.text, &case_keys)?;
    complete(
        "контроль ширины",
        plan,
        &measured.text_controls,
        &plan.text_controls,
    )?;
    complete("маска", plan, &measured.positives, &plan.primitives)?;
    complete(
        "контроль маски",
        plan,
        &measured.icon_controls,
        &control_keys,
    )?;
    cells_match("два тона", plan, &measured.two_tone)?;
    widths("ширина текста", &measured.text)?;
    widths("контроль ширины", &measured.text_controls)?;
    ratios("маска", &measured.positives)?;
    ratios("контроль маски", &measured.icon_controls)?;
    if let Some((cell, _)) = measured.two_tone.iter().find(|(_, holds)| !**holds) {
        return Err(Error::new(format!(
            "два тона: в {cell} тон двухцветного значка ближе к фону, чем порог переднего плана"
        )));
    }

    let half = |case: &str| plan.cases.get(case).copied();
    let mut fit: Vec<(&str, &str, Width)> = flat(&measured.text)
        .filter(|(_, case, _)| half(case) == Some(Half::Fit))
        .collect();
    fit.sort_by(|a, b| b.2.gap().total_cmp(&a.2.gap()));
    let fit_max = fit
        .first()
        .map(|(_, _, w)| w.gap())
        .ok_or_else(|| Error::new("ширина текста: половина fit пуста"))?;
    let positive_min = flat(&measured.positives)
        .map(|(_, _, iou)| iou)
        .min_by(f64::total_cmp)
        .ok_or_else(|| Error::new("маска: положительных пар нет"))?;
    let (tolerance, threshold) = (fit_max + plan.text_margin, positive_min - plan.mask_margin);

    let record = json!({
        "schema": 1,
        "text": {
            "tolerance_pt": tolerance,
            "margin_pt": plan.text_margin,
            "fit_max_pt": fit_max,
            "cases": nested(&measured.text, |case, w: Width| json!({
                "half": half(case).map(Half::name),
                "prototype": w.prototype,
                "product": w.product,
            })),
            "controls": nested(&measured.text_controls, |_, w: Width| json!({
                "prototype": w.prototype,
                "product": w.product,
            })),
        },
        "icons": {
            "threshold": threshold,
            "margin": plan.mask_margin,
            "positive_min": positive_min,
            "positive": nested(&measured.positives, |_, iou: f64| iou),
            "controls": nested(&measured.icon_controls, |_, iou: f64| iou),
            "two_tone": measured.two_tone,
        },
    });
    let text = serde_json::to_string_pretty(&record)
        .map_err(|e| Error::new(format!("калибровка: запись: {e}")))?
        + "\n";
    // Исход судится по записи, прочитанной так же, как её прочтёт судья.
    let read = Tolerances::from_calibration(&text)?;

    let mut broken = Vec::new();
    for (cell, case, w) in flat(&measured.text) {
        if half(case) == Some(Half::Holdout) && over(w.gap(), read.text_width) {
            broken.push(format!(
                "отложенный случай {cell} {case} вне допуска: разница {}",
                w.gap()
            ));
        }
    }
    for (cell, case, w) in flat(&measured.text_controls) {
        if !over(w.gap(), read.text_width_limit()) {
            broken.push(format!(
                "контроль ширины {cell} {case} прошёл: разница {} при допуске судьи {}",
                w.gap(),
                read.text_width_limit()
            ));
        }
    }
    for (cell, id, iou) in flat(&measured.positives) {
        if !mask_within(iou, read.mask_iou) {
            broken.push(format!("пара {cell} {id} ниже порога: мера {iou}"));
        }
    }
    for (cell, id, iou) in flat(&measured.icon_controls) {
        if mask_within(iou, read.mask_iou) {
            broken.push(format!("контроль маски {cell} {id} прошёл: мера {iou}"));
        }
    }
    if broken.is_empty() {
        return Ok(text);
    }
    let worst: Vec<String> = fit
        .iter()
        .take(WORST)
        .map(|(cell, case, w)| format!("{cell} {case} — {}", w.gap()))
        .collect();
    Err(Error::new(format!(
        "контроли калибровки не разделены: допуск ширины {}, порог маски {}; {}; худшие случаи \
         половины fit: {}",
        read.text_width,
        read.mask_iou,
        broken.join("; "),
        worst.join(", ")
    )))
}
