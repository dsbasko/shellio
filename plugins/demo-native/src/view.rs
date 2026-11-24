//! Вид «Демо: поле»: редактируемое поле и подпись на одной подложке. Кадры задают регионы
//! представления модели, текст поля и подписи — форма тела [`Form`]. `Return` шлёт `set` через
//! отправитель хоста, исход запроса возвращается в форму, сам вид данные панели не меняет.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use serde::Deserialize;
use serde_json::{Value, json};
use shellio_appkit::{
    App, FocusLayer, Frame, InspectNode, Piece, StatusLine, Surface, SurfaceFill, SurfaceRadius,
    SurfaceSpec, TextField, TextFieldIntent, TextFieldState,
};
use shellio_layout::Geometry;
use shellio_plugin_api::{Presentation, Refusal};
use shellio_plugin_view::{PanelRequest, PanelSink, PluginView, ViewCtx, ViewFactory};
use shellio_theme::AppTheme;

use crate::form::Form;
use crate::model::{CAPTION_REGION, FIELD_REGION, MANIFEST, SET, State};

/// Имя поля для доступности и его плейсхолдер.
pub const FIELD_LABEL: &str = "Значение";

/// Фабрика тела панели на главном потоке.
#[must_use]
pub fn factory() -> Box<dyn ViewFactory> {
    Box::new(Factory)
}

struct Factory;

impl ViewFactory for Factory {
    fn plugin(&self) -> &'static str {
        MANIFEST.id
    }

    fn build(&self, ctx: ViewCtx<'_>, state: &Value) -> Box<dyn PluginView> {
        Box::new(View::new(&ctx, state))
    }
}

/// Запрос хосту на `Return` с текстом поля `value`.
#[must_use]
pub fn request(value: &str) -> PanelRequest {
    PanelRequest::Command {
        name: SET.to_owned(),
        args: json!({"value": value}),
    }
}

/// Кадры поля и подписи по регионам представления. `None` — региона нет.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Frames {
    field: Option<Frame>,
    caption: Option<Frame>,
}

impl Frames {
    fn of(presentation: &Presentation) -> Frames {
        let frame = |name| {
            presentation
                .regions
                .iter()
                .find(|region| region.name == name)
                .map(|region| {
                    let frame = region.frame;
                    Frame::new(frame.x, frame.y, frame.width, frame.height)
                })
        };
        Frames {
            field: frame(FIELD_REGION),
            caption: frame(CAPTION_REGION),
        }
    }
}

fn field_state(form: &Form) -> TextFieldState {
    TextFieldState {
        value: form.draft().to_owned(),
        placeholder: FIELD_LABEL.to_owned(),
        editable: true,
    }
}

fn parse(state: &Value) -> Option<State> {
    State::deserialize(state).ok()
}

/// Часть тела, которую делят вид, обработчик намерений поля и исход запроса.
struct Shared {
    form: RefCell<Form>,
    caption: StatusLine,
    /// Обработчик поля держит на общую часть слабую ссылку.
    field: TextField,
    sink: PanelSink,
}

impl Shared {
    /// Поле и подпись получают форму. Текст, который поле уже показывает, оно не трогает.
    fn show(&self) {
        let (state, caption) = {
            let form = self.form.borrow();
            (field_state(&form), form.caption())
        };
        self.field.set_state(state);
        self.caption.set_message(Some(&caption));
    }

    fn done(&self, outcome: Result<(), Refusal>) {
        self.form.borrow_mut().done(outcome);
        self.show();
    }

    fn intent(this: &Rc<Shared>, intent: TextFieldIntent) {
        match intent {
            TextFieldIntent::Changed(text) => this.form.borrow_mut().edited(&text),
            TextFieldIntent::Submitted(text) => {
                let value = this.form.borrow_mut().submit(&text);
                let shared = Rc::downgrade(this);
                // Форма свободна: хост вправе вернуть исход раньше, чем вернётся отправка.
                this.sink.send(
                    request(&value),
                    Box::new(move |outcome| {
                        if let Some(shared) = shared.upgrade() {
                            shared.done(outcome);
                        }
                    }),
                );
            }
            TextFieldIntent::Cancelled => {
                this.form.borrow_mut().cancel();
                this.show();
            }
        }
    }
}

struct View {
    root: Surface,
    shared: Rc<Shared>,
    frames: Frames,
}

impl View {
    fn new(ctx: &ViewCtx<'_>, state: &Value) -> View {
        // Хост строит тело только для панели в `ready`, её данные разобраны моделью.
        let form = Form::new(parse(state).unwrap_or_default());
        let root = Surface::new(
            ctx.app,
            ctx.theme,
            SurfaceSpec {
                fill: SurfaceFill::Panel,
                line: false,
                radius: SurfaceRadius::None,
                layer: FocusLayer::Plain,
            },
        );
        let shared = Rc::new_cyclic(|weak: &Weak<Shared>| Shared {
            caption: StatusLine::new(ctx.app, ctx.theme),
            field: build_field(ctx.app, ctx.theme, field_state(&form), weak.clone()),
            sink: ctx.sink.clone(),
            form: RefCell::new(form),
        });
        // До первого представления поле и подпись стоят в подложке без площади.
        let nowhere = Frame::new(0.0, 0.0, 0.0, 0.0);
        root.place(&shared.field, nowhere);
        root.place(&shared.caption, nowhere);
        shared.show();
        View {
            root,
            shared,
            frames: Frames::default(),
        }
    }
}

fn build_field(
    app: &App,
    theme: &AppTheme,
    state: TextFieldState,
    shared: Weak<Shared>,
) -> TextField {
    TextField::new(app, theme, state, FIELD_LABEL, move |intent| {
        if let Some(shared) = shared.upgrade() {
            Shared::intent(&shared, intent);
        }
    })
}

impl PluginView for View {
    fn body(&self) -> Piece<'_> {
        Piece::from(&self.root)
    }

    fn adapt(&mut self, _geometry: &Geometry, presentation: &Presentation) {
        let frames = Frames::of(presentation);
        if let Some(frame) = frames.field
            && self.frames.field != Some(frame)
        {
            self.root.place(&self.shared.field, frame);
        }
        if let Some(frame) = frames.caption
            && self.frames.caption != Some(frame)
        {
            self.root.place(&self.shared.caption, frame);
        }
        self.frames = frames;
    }

    fn update(&mut self, state: &Value) {
        let Some(state) = parse(state) else {
            return;
        };
        let changed = self.shared.form.borrow_mut().update(state);
        if changed {
            self.shared.show();
        }
    }

    fn set_theme(&mut self, theme: &AppTheme) {
        self.root.set_theme(theme);
        self.shared.field.set_theme(theme);
        self.shared.caption.set_theme(theme);
    }

    fn set_active(&mut self, _active: bool) {}

    fn set_focused(&mut self, _focused: bool) {}

    fn focus_preferred(&self) {
        self.shared.field.focus();
    }

    fn inspect(&self, prefix: &str) -> Vec<InspectNode> {
        let parent = prefix.strip_suffix('.').unwrap_or(prefix);
        let body = format!("{prefix}body");
        let mut nodes = self.root.inspect(&body, Some(parent));
        nodes.extend(
            self.shared
                .field
                .inspect(&format!("{prefix}{FIELD_REGION}"), Some(&body)),
        );
        nodes.extend(
            self.shared
                .caption
                .inspect(&format!("{prefix}{CAPTION_REGION}"), Some(&body)),
        );
        nodes
    }

    fn close_view(&mut self) {}
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use shellio_layout::Zone;
    use shellio_plugin_api::{DynModel, PanelModel, Step};
    use shellio_plugin_view::Done;
    use shellio_theme::Cell;

    use super::*;
    use crate::model::{CENTER, LINE, Model, ROW, STACK};

    fn presentation(zone: Zone, width: f64, height: f64) -> Presentation {
        Model.adapt(&Geometry::of(zone, width, height), &State::default())
    }

    #[test]
    fn factory_names_the_plugin_of_the_manifest() {
        assert_eq!(factory().plugin(), "demo-native");
    }

    #[test]
    fn line_of_the_model_is_the_height_of_the_field_component() {
        for cell in Cell::ALL {
            let theme = AppTheme::for_cell(cell);
            assert!((shellio_appkit::text_field_height(&theme) - LINE).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn data_of_the_panel_parse_with_the_invariants_of_the_model() {
        assert_eq!(
            parse(&json!({"value": "a", "edits": 2})),
            Some(State {
                value: "a".into(),
                edits: 2
            })
        );
        assert_eq!(parse(&json!({"value": "я".repeat(257), "edits": 2})), None);
        assert_eq!(parse(&Value::Null), None);
    }

    #[test]
    fn field_shows_the_draft_and_stays_editable() {
        let mut form = Form::new(State {
            value: "a".into(),
            edits: 1,
        });
        form.edited("черновик");
        assert_eq!(
            field_state(&form),
            TextFieldState {
                value: "черновик".into(),
                placeholder: "Значение".into(),
                editable: true,
            }
        );
    }

    #[test]
    fn request_is_the_set_action_of_the_model() {
        let model = DynModel::new(Model);
        let data = model.init(None).unwrap();
        let PanelRequest::Command { name, args } = request("shellio-check") else {
            panic!("ожидалась команда панели");
        };
        assert_eq!(name, SET);
        assert_eq!(
            model.command(&data, &name, Some(args)).unwrap(),
            Step::Changed(r#"{"value":"shellio-check","edits":1}"#.into())
        );
        // Длину вид не проверяет: отказ приходит исходом запроса.
        let PanelRequest::Command { name, args } = request(&"x".repeat(257)) else {
            panic!("ожидалась команда панели");
        };
        assert!(model.command(&data, &name, Some(args)).is_err());
    }

    #[test]
    fn frames_follow_the_regions_of_every_variant() {
        assert_eq!(Frames::default().field, None, "до первого представления");
        let cases = [
            (Zone::Right, 300.0, 400.0, STACK),
            (Zone::Left, 300.0, 400.0, STACK),
            (Zone::Bottom, 700.0, 300.0, ROW),
            (Zone::Right, 300.0, 200.0, ROW),
            (Zone::Center, 900.0, 600.0, CENTER),
        ];
        for (zone, width, height, variant) in cases {
            let presentation = presentation(zone, width, height);
            assert_eq!(presentation.variant, variant, "{zone:?}");
            let frames = Frames::of(&presentation);
            let (field, caption) = (frames.field.unwrap(), frames.caption.unwrap());
            let region = |name| {
                presentation
                    .regions
                    .iter()
                    .find(|region| region.name == name)
                    .unwrap()
                    .frame
            };
            let of = |frame: shellio_layout::Rect| {
                Frame::new(frame.x, frame.y, frame.width, frame.height)
            };
            assert_eq!(field, of(region(FIELD_REGION)), "{variant}");
            assert_eq!(caption, of(region(CAPTION_REGION)), "{variant}");
        }
        let stack = Frames::of(&presentation(Zone::Right, 300.0, 400.0));
        assert_eq!(stack.field, Some(Frame::new(12.0, 12.0, 276.0, 32.0)));
        assert_eq!(stack.caption, Some(Frame::new(12.0, 50.0, 276.0, 32.0)));
    }

    /// Запросы тела и их исходы, которые хост ещё не отдал.
    type Inbox = Rc<RefCell<Vec<(PanelRequest, Done)>>>;

    /// Отправитель, который складывает запросы и исходы: хост отдаёт исход позже.
    fn inbox() -> (PanelSink, Inbox) {
        let sent = Rc::new(RefCell::new(Vec::new()));
        let sink = PanelSink::new({
            let sent = Rc::clone(&sent);
            move |request, done| sent.borrow_mut().push((request, done))
        });
        (sink, sent)
    }

    /// Путь `Return` без AppKit: форма отдаёт значение, запрос уходит отправителю, исход
    /// возвращается в форму.
    fn submit(form: &Rc<RefCell<Form>>, sink: &PanelSink, text: &str) {
        let value = form.borrow_mut().submit(text);
        let form = Rc::clone(form);
        sink.send(
            request(&value),
            Box::new(move |outcome| form.borrow_mut().done(outcome)),
        );
    }

    #[test]
    fn refused_set_shows_the_reason_and_keeps_the_draft() {
        let model = DynModel::new(Model);
        let data = model.init(None).unwrap();
        let form = Rc::new(RefCell::new(Form::new(State::default())));
        let (sink, sent) = inbox();

        let long = "x".repeat(257);
        submit(&form, &sink, &long);
        assert_eq!(
            form.borrow().caption(),
            "Сохранено:  · правок 0",
            "отправка не ждёт исхода"
        );
        let (request, done) = sent.borrow_mut().remove(0);
        let PanelRequest::Command { name, args } = request else {
            panic!("ожидалась команда панели");
        };
        let refusal = model.command(&data, &name, Some(args)).unwrap_err();
        done(Err(refusal));
        assert_eq!(
            form.borrow().caption(),
            "Не сохранено: значение длиннее 256 символов: 257"
        );
        assert_eq!(form.borrow().draft(), long);
        assert_eq!(form.borrow().saved(), &State::default());

        // Следующее успешное сохранение убирает причину.
        submit(&form, &sink, "shellio-check");
        let (_request, done) = sent.borrow_mut().remove(0);
        done(Ok(()));
        form.borrow_mut().update(State {
            value: "shellio-check".into(),
            edits: 1,
        });
        assert_eq!(
            form.borrow().caption(),
            "Сохранено: shellio-check · правок 1"
        );
    }
}
