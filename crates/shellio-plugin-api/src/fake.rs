//! Подставная модель тестов контракта.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use shellio_layout::{Geometry, LayoutMetrics, MinSize, Rect, SizeClass, Zone};

use crate::contract::{SizeVariants, VariantTable};
use crate::manifest::{Cardinality, CommandSpec, KeyDefault, Manifest, ZoneSet};
use crate::model::{PanelModel, Presentation, Reach, Refusal, Region, Via};

pub(crate) const METRICS: LayoutMetrics = LayoutMetrics {
    titlebar: 40,
    tabbar: 34,
    gap: 6,
    line: 1,
    status_bar: 26,
    drop_edge: 24,
    side_pad: 8,
};

pub(crate) const MANIFEST: Manifest = Manifest {
    id: "fake",
    title: "Подставной",
    icon: "square",
    placements: ZoneSet::ALL,
    default_zone: Zone::Right,
    cardinality: Cardinality::PerWorkbench,
    min_size: MinSize::DEFAULT,
    commands: &[
        CommandSpec {
            name: "fake.fill",
            args: r#"{"text":"a"}"#,
        },
        CommandSpec {
            name: "fake.bump",
            args: "{}",
        },
    ],
    keymap: &[KeyDefault {
        action: "fake.bump",
        key: "b",
    }],
    state_version: 2,
    builtin: true,
};

/// Таблица вариантов подставной модели.
pub(crate) const VARIANTS: VariantTable = VariantTable {
    left: SizeVariants {
        narrow: "drill",
        regular: "drill",
        wide: "split",
        short_bar: "bar",
    },
    right: SizeVariants {
        narrow: "drill",
        regular: "drill",
        wide: "split",
        short_bar: "bar",
    },
    center: SizeVariants {
        narrow: "drill",
        regular: "split",
        wide: "split",
        short_bar: "bar",
    },
    bottom: SizeVariants {
        narrow: "drill",
        regular: "bar",
        wide: "bar",
        short_bar: "bar",
    },
};

/// Манифест подставной модели с правкой. Манифест живёт до конца процесса тестов.
pub(crate) fn manifest_with(edit: impl FnOnce(&mut Manifest)) -> &'static Manifest {
    let mut manifest = MANIFEST;
    edit(&mut manifest);
    Box::leak(Box::new(manifest))
}

const MAX_BUMPS: u32 = 1000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawState")]
pub(crate) struct FakeState {
    pub text: String,
    pub bumps: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawState {
    text: String,
    bumps: u32,
}

impl TryFrom<RawState> for FakeState {
    type Error = String;

    fn try_from(raw: RawState) -> Result<Self, Self::Error> {
        if raw.bumps > MAX_BUMPS {
            return Err(format!("bumps {} больше {MAX_BUMPS}", raw.bumps));
        }
        Ok(Self {
            text: raw.text,
            bumps: raw.bumps,
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FakeArgs {
    text: String,
}

#[derive(Deserialize)]
#[serde(tag = "name", content = "args", deny_unknown_fields)]
pub(crate) enum FakeCommand {
    #[serde(rename = "fake.fill")]
    Fill { text: String },
    #[serde(rename = "fake.bump")]
    Bump {},
    /// Вариант без записи в манифесте.
    #[serde(rename = "fake.hidden")]
    Hidden {},
}

/// Чем `adapt` подставной модели нарушает контракт.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Flaw {
    None,
    /// Один вариант во всех зонах и размерах.
    OneVariant,
    /// Регион выходит за тело.
    Outside,
    /// Регион начинается левее тела.
    Negative,
    /// Два региона перекрываются.
    Overlap,
    /// Регион нулевой ширины.
    ZeroArea,
    /// Два региона с одним именем.
    SameName,
    /// Действия манифеста нет в `reach`.
    Unreachable,
    /// В `reach` действие не из манифеста.
    Unlisted,
    /// Действие в `reach` дважды.
    ReachTwice,
    /// Паника в нижней зоне.
    PanicInBottom,
}

pub(crate) struct Fake {
    manifest: &'static Manifest,
    flaw: Flaw,
    adapt_calls: Arc<AtomicUsize>,
}

impl Fake {
    pub(crate) fn new() -> Self {
        Self::with_manifest(&MANIFEST)
    }

    pub(crate) fn with_manifest(manifest: &'static Manifest) -> Self {
        Self {
            manifest,
            flaw: Flaw::None,
            adapt_calls: Arc::default(),
        }
    }

    pub(crate) fn flawed(flaw: Flaw) -> Self {
        Self {
            flaw,
            ..Self::new()
        }
    }

    /// Счётчик фактических вызовов `adapt`.
    pub(crate) fn adapt_calls(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.adapt_calls)
    }
}

fn frame(x: f64, width: f64, height: f64) -> Rect {
    Rect {
        x,
        y: 0.0,
        width,
        height,
    }
}

impl PanelModel for Fake {
    type State = FakeState;
    type Args = FakeArgs;
    type Command = FakeCommand;

    fn manifest(&self) -> &'static Manifest {
        self.manifest
    }

    fn init(&self, args: Option<FakeArgs>) -> Result<FakeState, Refusal> {
        Ok(FakeState {
            text: args.map(|args| args.text).unwrap_or_default(),
            bumps: 0,
        })
    }

    fn reopen(&self, state: &FakeState, args: Option<FakeArgs>) -> Result<FakeState, Refusal> {
        Ok(match args {
            Some(args) => FakeState {
                text: args.text,
                bumps: state.bumps,
            },
            None => state.clone(),
        })
    }

    fn apply(&self, state: &FakeState, command: FakeCommand) -> Result<FakeState, Refusal> {
        match command {
            FakeCommand::Fill { text } if text == "!" => {
                Err(Refusal::new("text", "text_forbidden", "текст «!» запрещён"))
            }
            FakeCommand::Fill { text } => Ok(FakeState {
                text,
                bumps: state.bumps,
            }),
            FakeCommand::Bump {} => Ok(FakeState {
                text: state.text.clone(),
                bumps: state.bumps + 1,
            }),
            FakeCommand::Hidden {} => Ok(state.clone()),
        }
    }

    fn migrate(&self, from: u32, data: Value) -> Result<FakeState, Refusal> {
        if from != 1 {
            return Err(Refusal::new(
                "version",
                "version_unknown",
                format!("версия {from} неизвестна"),
            ));
        }
        let text = data
            .get("note")
            .and_then(Value::as_str)
            .ok_or_else(|| Refusal::new("note", "note_invalid", "нет строки note"))?;
        Ok(FakeState {
            text: text.to_owned(),
            bumps: 0,
        })
    }

    fn adapt(&self, geometry: &Geometry, _state: &FakeState) -> Presentation {
        self.adapt_calls.fetch_add(1, Ordering::Relaxed);
        let (width, height) = (geometry.width, geometry.height);
        let variant = match geometry.zone {
            Zone::Left | Zone::Right => match geometry.size {
                SizeClass::ShortBar => "bar",
                SizeClass::Narrow | SizeClass::Regular => "drill",
                SizeClass::Wide => "split",
            },
            Zone::Center => match geometry.size {
                SizeClass::ShortBar => "bar",
                SizeClass::Narrow => "drill",
                SizeClass::Regular | SizeClass::Wide => "split",
            },
            Zone::Bottom => match geometry.size {
                SizeClass::Narrow => "drill",
                SizeClass::ShortBar | SizeClass::Regular | SizeClass::Wide => "bar",
            },
        };
        let half = (width / 2.0).floor();
        let mut regions = match variant {
            "drill" => vec![Region {
                name: "list",
                frame: frame(0.0, width, height),
            }],
            _ => vec![
                Region {
                    name: "list",
                    frame: frame(0.0, half, height),
                },
                Region {
                    name: "details",
                    frame: frame(half, width - half, height),
                },
            ],
        };
        let via = if variant == "drill" {
            Via::Drill
        } else {
            Via::Direct
        };
        let mut reach: Vec<Reach> = self
            .manifest
            .commands
            .iter()
            .map(|spec| Reach {
                action: spec.name,
                via,
            })
            .collect();
        let mut variant = variant;
        match self.flaw {
            Flaw::None => {}
            Flaw::OneVariant => variant = "drill",
            Flaw::Outside => regions[0].frame.height = height + 1.0,
            Flaw::Negative => regions[0].frame.x = -1.0,
            Flaw::Overlap => regions.push(Region {
                name: "extra",
                frame: frame(width - 10.0, 10.0, 10.0),
            }),
            Flaw::ZeroArea => regions[0].frame.width = 0.0,
            Flaw::SameName => {
                regions = vec![
                    Region {
                        name: "list",
                        frame: frame(0.0, half, height),
                    },
                    Region {
                        name: "list",
                        frame: frame(half, width - half, height),
                    },
                ];
            }
            Flaw::Unreachable => {
                reach.pop();
            }
            Flaw::Unlisted => reach.push(Reach {
                action: "fake.hidden",
                via: Via::Zoom,
            }),
            Flaw::ReachTwice => reach.push(reach[0]),
            Flaw::PanicInBottom => match geometry.zone {
                Zone::Bottom => panic!("нижняя зона не поддержана"),
                Zone::Left | Zone::Right | Zone::Center => {}
            },
        }
        Presentation {
            variant,
            regions,
            reach,
        }
    }
}
