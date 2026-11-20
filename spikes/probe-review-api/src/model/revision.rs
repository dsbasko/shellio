//! Ревизия ревью и смена head.
//!
//! [`Revision`] — тройка `base_sha`, `start_sha`, `head_sha` из одного ответа провайдера целиком
//! (`diff_refs` MR GitLab, base и head PR GitHub). Отдельного SHA ревизия не меняет: сеттеров нет,
//! заменить в ней один `head_sha` нельзя. Смена head определяется сравнением ревизии сессии со
//! свежей и классифицируется по якорям черновиков: остаться, перейти, перепривязать.

use std::fmt;

use serde::{Serialize, Serializer};

use super::ModelError;
use super::anchor::Anchor;
use super::diff::FileDiff;
use super::{DraftId, Placement};

/// SHA коммита: 40 (SHA-1) или 64 (SHA-256) шестнадцатеричных знака, хранится в нижнем регистре.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Sha(String);

impl Sha {
    pub fn parse(value: &str) -> Result<Sha, ModelError> {
        let valid_len = matches!(value.len(), 40 | 64);
        if !valid_len || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ModelError::Sha {
                value: value.to_owned(),
            });
        }
        Ok(Sha(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Serialize for Sha {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RevisionField {
    Base,
    Start,
    Head,
}

impl RevisionField {
    pub const ALL: [RevisionField; 3] = [
        RevisionField::Base,
        RevisionField::Start,
        RevisionField::Head,
    ];

    pub fn name(self) -> &'static str {
        match self {
            RevisionField::Base => "base_sha",
            RevisionField::Start => "start_sha",
            RevisionField::Head => "head_sha",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Revision {
    base_sha: Sha,
    start_sha: Sha,
    head_sha: Sha,
}

impl Revision {
    /// Все три SHA должны прийти из одного ответа провайдера.
    pub fn new(base_sha: Sha, start_sha: Sha, head_sha: Sha) -> Revision {
        Revision {
            base_sha,
            start_sha,
            head_sha,
        }
    }

    pub fn base_sha(&self) -> &Sha {
        &self.base_sha
    }

    pub fn start_sha(&self) -> &Sha {
        &self.start_sha
    }

    pub fn head_sha(&self) -> &Sha {
        &self.head_sha
    }

    pub fn field(&self, field: RevisionField) -> &Sha {
        match field {
            RevisionField::Base => &self.base_sha,
            RevisionField::Start => &self.start_sha,
            RevisionField::Head => &self.head_sha,
        }
    }

    /// Поля, в которых ревизии расходятся, в порядке base, start, head.
    pub fn differing_fields(&self, other: &Revision) -> Vec<RevisionField> {
        RevisionField::ALL
            .into_iter()
            .filter(|f| self.field(*f) != other.field(*f))
            .collect()
    }
}

/// Откуда взята тройка SHA исходящего запроса относительно удерживаемой и свежей ревизии.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TripleOrigin {
    /// Тройка целиком совпадает с удерживаемой ревизией.
    Held,
    /// Тройка целиком совпадает со свежей ревизией.
    Fresh,
    /// Часть полей только из свежей ревизии, часть только из удерживаемой — запрещённое смешение.
    Mixed {
        fresh: Vec<RevisionField>,
        held: Vec<RevisionField>,
    },
    /// Хотя бы одно поле не совпадает ни с одной из ревизий.
    Foreign,
}

/// Оракул смешения: поле, совпавшее с обеими ревизиями (не изменилось), не делает тройку смешанной.
pub fn triple_origin(held: &Revision, fresh: &Revision, triple: &Revision) -> TripleOrigin {
    let mut only_fresh = Vec::new();
    let mut only_held = Vec::new();
    for field in RevisionField::ALL {
        let value = triple.field(field);
        match (value == held.field(field), value == fresh.field(field)) {
            (true, true) => {}
            (true, false) => only_held.push(field),
            (false, true) => only_fresh.push(field),
            (false, false) => return TripleOrigin::Foreign,
        }
    }
    match (only_fresh.is_empty(), only_held.is_empty()) {
        (true, _) => TripleOrigin::Held,
        (false, true) => TripleOrigin::Fresh,
        (false, false) => TripleOrigin::Mixed {
            fresh: only_fresh,
            held: only_held,
        },
    }
}

/// Итог сравнения ревизии сессии со свежей (шаг 1 отправки ADR-0015).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadCheck {
    Same,
    /// Предупреждение: ревизия сменилась, нужен выбор одного из путей [`HeadPath`].
    Changed(HeadChange),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadChange {
    from: Revision,
    to: Revision,
    fates: Vec<(DraftId, AnchorFate)>,
}

impl HeadChange {
    pub fn from(&self) -> &Revision {
        &self.from
    }

    pub fn to(&self) -> &Revision {
        &self.to
    }

    /// Судьба якоря каждого действующего черновика на строке, поставленного на ревизии `from`.
    pub fn fates(&self) -> &[(DraftId, AnchorFate)] {
        &self.fates
    }

    pub fn stale(&self) -> impl Iterator<Item = DraftId> + '_ {
        self.fates
            .iter()
            .filter(|(_, fate)| matches!(fate, AnchorFate::Stale(_)))
            .map(|(id, _)| *id)
    }

    fn fate(&self, id: DraftId) -> Option<&AnchorFate> {
        self.fates
            .iter()
            .find(|(draft, _)| *draft == id)
            .map(|(_, fate)| fate)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorFate {
    /// Строки якоря в свежем diff те же: вид, оба счётчика, текст, тот же блок.
    Kept,
    Stale(StaleReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleReason {
    /// Файла с теми же старым и новым путём в свежем diff нет.
    FileMissing,
    /// Файл с тем же новым путём есть, но старый путь другой.
    PathChanged,
    /// Строка якоря изменилась или сдвинулась: номер молча не подбирается.
    LinesChanged,
    /// Якорь не найден в diff своей же ревизии: diff ревизии сессии передан не тот.
    NotInOwnDiff,
}

impl StaleReason {
    pub fn describe(self) -> &'static str {
        match self {
            StaleReason::FileMissing => "файла нет в свежем diff",
            StaleReason::PathChanged => "у файла сменился старый путь",
            StaleReason::LinesChanged => "строки якоря изменились или сдвинулись",
            StaleReason::NotInOwnDiff => "якорь не найден в diff своей ревизии",
        }
    }
}

/// Путь после предупреждения о смене head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HeadPath {
    /// Сессия и черновики остаются на прежней ревизии.
    Stay,
    /// Сессия переходит на свежую ревизию. Сохранившиеся якоря переходят с ней, устаревшие
    /// помечаются и остаются на прежней ревизии: отправить их нельзя до перепривязки.
    Move,
    /// Как [`HeadPath::Move`], и устаревшие якоря получают явно заданные новые якоря свежего diff.
    Rebind(Vec<(DraftId, Anchor)>),
}

/// Классифицирует смену head по якорям черновиков. Учитываются только действующие черновики на
/// строке, поставленные на ревизии `held`; diff передаёт вызывающий для обеих ревизий.
pub fn check_head(
    held: &Revision,
    fresh: &Revision,
    held_diff: &[FileDiff],
    fresh_diff: &[FileDiff],
    drafts: &[super::DraftComment],
) -> HeadCheck {
    if held == fresh {
        return HeadCheck::Same;
    }
    let fates = drafts
        .iter()
        .filter(|d| d.revision() == held && d.is_current())
        .filter_map(|d| match &d.placement {
            Placement::Line(anchor) => Some((d.id, anchor.fate(held_diff, fresh_diff))),
            Placement::Reply { .. } => None,
        })
        .collect();
    HeadCheck::Changed(HeadChange {
        from: held.clone(),
        to: fresh.clone(),
        fates,
    })
}

/// Применяет выбранный путь к ревизии сессии и черновикам. Изменение возможно только для той
/// ревизии, на которой оно определено, иначе — расхождение ревизий без изменений.
pub(super) fn apply_head(
    revision: &mut Revision,
    change: &HeadChange,
    path: HeadPath,
    drafts: &mut [super::DraftComment],
    fresh_diff: &[FileDiff],
) -> Result<(), ModelError> {
    if *revision != change.from {
        return Err(ModelError::RevisionMismatch {
            fields: revision.differing_fields(&change.from),
        });
    }
    let rebinds = match path {
        HeadPath::Stay => return Ok(()),
        HeadPath::Move => Vec::new(),
        HeadPath::Rebind(rebinds) => rebinds,
    };
    for (id, anchor) in &rebinds {
        if !matches!(change.fate(*id), Some(AnchorFate::Stale(_))) {
            return Err(ModelError::RebindNotStale { draft: *id });
        }
        anchor.validate(fresh_diff)?;
    }
    for draft in drafts.iter_mut() {
        if draft.revision() != &change.from || !draft.is_current() {
            continue;
        }
        match change.fate(draft.id) {
            Some(AnchorFate::Stale(reason)) => {
                match rebinds.iter().find(|(id, _)| *id == draft.id) {
                    Some((_, anchor)) => {
                        draft.placement = Placement::Line(anchor.clone());
                        draft.rebase(change.to.clone());
                    }
                    None => draft.mark_stale(*reason),
                }
            }
            Some(AnchorFate::Kept) => draft.rebase(change.to.clone()),
            None => match draft.placement {
                Placement::Reply { .. } => draft.rebase(change.to.clone()),
                Placement::Line(_) => draft.mark_stale(StaleReason::NotInOwnDiff),
            },
        }
    }
    *revision = change.to.clone();
    Ok(())
}
