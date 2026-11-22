//! Вид галереи на главном потоке: компоненты `shellio-appkit` по фикстурам чистой модели
//! [`shellio::gallery`], маркер поколения, перестройка дерева и слабое наблюдение за видами
//! прежних деревьев. Намерения компонентов уходят получателю, который ставит их в очередь главного
//! потока: вид не трогает состояние окна изнутри вызова AppKit.

use std::collections::VecDeque;
use std::rc::Rc;

use shellio::gallery::{self, Gallery, Intent, MenuFixtureItem, ids};
use shellio_appkit::{
    App, FocusLayer, Frame, GenerationMarker, InspectNode, MainWindow, MenuButton, MenuEntry,
    MenuIntent, MenuItem, MenuKey, Piece, Surface, SurfaceFill, SurfaceRadius, SurfaceSpec,
    TextField, TextFieldIntent, TextFieldState, ViewWatch, text_field_height,
};
use shellio_control::{ClassAliveView, GalleryFocus, GalleryTreeView};
use shellio_theme::{AppTheme, KeyMode};

/// Сколько прежних деревьев держит `ui.stats.gallery_trees`.
const RETIRED_TREES: usize = 64;

/// Дерево компонентов одного построения. Освобождение убирает виды из окна.
struct Tree {
    number: u64,
    // Порядок полей — порядок освобождения: сначала содержимое подложки.
    field: TextField,
    menu: MenuButton,
    focus: Surface,
    plain: Surface,
    marker: GenerationMarker,
    watch: ViewWatch,
}

/// Освобождённое дерево: слабые ссылки на его объекты.
struct Retired {
    number: u64,
    watch: ViewWatch,
}

/// Галерея окна владельца.
pub struct GalleryUi {
    model: Gallery,
    tree: Option<Tree>,
    retired: VecDeque<Retired>,
    on_intent: Rc<dyn Fn(Intent)>,
}

impl GalleryUi {
    /// Строит первое дерево в окне `window`. `on_intent` вызывается на главном потоке изнутри
    /// AppKit и не должен синхронно трогать галерею.
    pub fn new(
        app: &App,
        window: &MainWindow,
        generation: u64,
        on_intent: impl Fn(Intent) + 'static,
    ) -> GalleryUi {
        let mut gallery = GalleryUi {
            model: Gallery::new(),
            tree: None,
            retired: VecDeque::new(),
            on_intent: Rc::new(on_intent),
        };
        gallery.build(app, window, generation);
        gallery
    }

    pub fn model(&self) -> &Gallery {
        &self.model
    }

    pub fn model_mut(&mut self) -> &mut Gallery {
        &mut self.model
    }

    /// Строит дерево с номером модели по текущему состоянию галереи.
    pub fn build(&mut self, app: &App, window: &MainWindow, generation: u64) {
        let theme = window.selection().theme();
        let (width, height) = window.content_size();
        let layout = gallery::layout(
            width,
            height,
            theme.metrics.titlebar,
            text_field_height(&theme),
        );
        let fixture = gallery::field_fixture();
        let state = TextFieldState {
            value: self.model.state().field_value.clone(),
            placeholder: fixture.placeholder,
            editable: fixture.editable,
        };
        let on_intent = Rc::clone(&self.on_intent);
        let field = TextField::new(app, &theme, state, fixture.label, move |intent| {
            on_intent(match intent {
                TextFieldIntent::Changed(value) => Intent::FieldChanged(value),
                TextFieldIntent::Submitted(value) => Intent::FieldSubmitted(value),
                TextFieldIntent::Cancelled => Intent::FieldCancelled,
            });
        });
        let menu_fixture = gallery::menu_fixture();
        let on_intent = Rc::clone(&self.on_intent);
        let menu = match MenuButton::new(
            app,
            &theme,
            menu_fixture.label,
            menu_fixture.title,
            menu_entries(&menu_fixture.items),
            move |intent| match intent {
                MenuIntent::Chosen { id } => on_intent(Intent::MenuChosen(id)),
            },
        ) {
            Ok(menu) => menu,
            Err(error) => {
                // Фикстура закреплена тестом уникальности id: отказ — нарушенный инвариант.
                shellio::owner::say_err(format_args!(
                    "shellio: меню галереи не построено: {error}"
                ));
                return;
            }
        };
        let panel = |layer| SurfaceSpec {
            fill: SurfaceFill::Panel,
            line: true,
            radius: SurfaceRadius::Plate,
            layer,
        };
        let focus = Surface::new(app, &theme, panel(FocusLayer::Tone(KeyMode::Navigation)));
        let plain = Surface::new(app, &theme, panel(FocusLayer::Veil));
        let marker = GenerationMarker::new(app, generation);
        window.place(&marker, frame(layout.marker));
        window.place(&focus, frame(layout.focus));
        window.place(&plain, frame(layout.plain));
        focus.place(&field, frame(layout.field));
        focus.place(&menu, frame(layout.menu));
        let mut watch = ViewWatch::new();
        for piece in [
            Piece::from(&marker),
            Piece::from(&focus),
            Piece::from(&plain),
        ] {
            watch.watch(piece);
        }
        self.tree = Some(Tree {
            number: self.model.tree(),
            field,
            menu,
            focus,
            plain,
            marker,
            watch,
        });
        // Новое дерево получает фокус последнего запроса `gallery.focus`.
        self.focus(window, self.model.state().focus);
    }

    /// Освобождает дерево и строит новое в том же окне.
    pub fn rebuild(&mut self, app: &App, window: &MainWindow, generation: u64) {
        self.retire(window, false);
        self.build(app, window, generation);
    }

    /// Освобождает дерево перед заменой окна: наблюдение получает и окно.
    pub fn retire_with_window(&mut self, window: &MainWindow) {
        self.retire(window, true);
    }

    fn retire(&mut self, window: &MainWindow, with_window: bool) {
        let Some(tree) = self.tree.take() else {
            return;
        };
        // Ввод в поле заканчивается до того, как поле уходит из окна: вид поля, снятый посреди ввода,
        // оставляет клип-вид field editor живым после окна (`window-cycle` (в), P1.3).
        window.clear_focus();
        let Tree {
            number, mut watch, ..
        } = tree;
        if with_window {
            watch.watch_window(window);
        }
        if self.retired.len() == RETIRED_TREES {
            self.retired.pop_front();
        }
        self.retired.push_back(Retired { number, watch });
    }

    pub fn set_theme(&self, theme: &AppTheme) {
        if let Some(tree) = &self.tree {
            tree.focus.set_theme(theme);
            tree.plain.set_theme(theme);
            tree.field.set_theme(theme);
            tree.menu.set_theme(theme);
        }
    }

    /// Маркер показывает поколение `generation` в том же проходе отображения, что и состояние.
    pub fn set_generation(&self, generation: u64) {
        if let Some(tree) = &self.tree {
            tree.marker.set_generation(generation);
        }
    }

    pub fn focus(&self, window: &MainWindow, focus: GalleryFocus) {
        match (focus, &self.tree) {
            (GalleryFocus::Field, Some(tree)) => {
                if !tree.field.focus() {
                    shellio::owner::say_err("shellio: поле галереи не приняло фокус");
                }
            }
            (GalleryFocus::Field, None) => {}
            (GalleryFocus::Window, _) => {
                window.clear_focus();
            }
        }
    }

    pub fn open_menu(&self) {
        if let Some(tree) = &self.tree {
            tree.menu.open();
        }
    }

    /// Вид приводится к состоянию фикстуры: меню закрыто, поле с её значением, фокус у окна.
    pub fn reset_view(&self, window: &MainWindow) {
        if let Some(tree) = &self.tree {
            tree.menu.menu().close();
            let fixture = gallery::field_fixture();
            tree.field.set_state(TextFieldState {
                value: self.model.state().field_value.clone(),
                placeholder: fixture.placeholder,
                editable: fixture.editable,
            });
        }
        window.clear_focus();
    }

    /// Узлы галереи для `ui.inspect`.
    pub fn inspect(&self) -> Vec<InspectNode> {
        let Some(tree) = &self.tree else {
            return Vec::new();
        };
        let mut nodes = tree.marker.inspect(ids::MARKER, None);
        nodes.extend(tree.focus.inspect(ids::SURFACE_FOCUS, None));
        nodes.extend(tree.field.inspect(ids::FIELD, Some(ids::SURFACE_FOCUS)));
        nodes.extend(tree.menu.inspect(ids::MENU, Some(ids::SURFACE_FOCUS)));
        nodes.extend(tree.plain.inspect(ids::SURFACE_PLAIN, None));
        nodes
    }

    /// Номер текущего дерева.
    pub fn tree_number(&self) -> Option<u64> {
        self.tree.as_ref().map(|tree| tree.number)
    }

    /// Прежние деревья для `ui.stats`.
    pub fn retired_trees(&self) -> Vec<GalleryTreeView> {
        self.retired
            .iter()
            .map(|retired| GalleryTreeView {
                tree: retired.number,
                views: retired.watch.len(),
                alive: retired.watch.alive(),
                alive_classes: retired
                    .watch
                    .alive_classes()
                    .into_iter()
                    .map(|(class, alive)| ClassAliveView { class, alive })
                    .collect(),
            })
            .collect()
    }
}

fn frame(rect: gallery::Rect) -> Frame {
    Frame::new(rect.x, rect.y, rect.width, rect.height)
}

fn menu_entries(items: &[MenuFixtureItem]) -> Vec<MenuEntry> {
    items
        .iter()
        .map(|item| match *item {
            MenuFixtureItem::Item {
                id,
                title,
                key,
                enabled,
                checked,
            } => MenuEntry::Item(MenuItem {
                key: key.map(MenuKey::command),
                enabled,
                checked,
                ..MenuItem::new(id, title)
            }),
            MenuFixtureItem::Separator { id } => MenuEntry::Separator { id: id.to_owned() },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use shellio_appkit::check_menu_ids;

    use super::*;

    #[test]
    fn menu_fixture_becomes_a_valid_menu() {
        let entries = menu_entries(&gallery::menu_fixture().items);
        assert_eq!(check_menu_ids(&entries), Ok(()));
        assert_eq!(entries.len(), gallery::menu_fixture().items.len());
        let MenuEntry::Item(copy) = &entries[0] else {
            panic!("первый пункт — разделитель: {entries:?}");
        };
        assert_eq!(copy.id, "copy");
        assert_eq!(copy.key, Some(MenuKey::command("c")));
        assert!(matches!(&entries[3], MenuEntry::Separator { id } if id == "sep"));
    }
}
