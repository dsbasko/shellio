//! Нажатие вида окна по идентичности узла `ui.inspect` (метки `f`, правило «Метки» плана
//! наложений): тем же обработчиком, что щелчок, без синтетического события мыши. Событие по
//! координатам попало бы в то, что окажется сверху, а у вкладки завело бы перетаскивание.

use objc2::Message;
use objc2::rc::Retained;
use objc2_app_kit::{NSAccessibility, NSButton, NSView, NSWindowButton};

use crate::components::virtual_list;
use crate::inspect;
use crate::window::MainWindow;

/// Вид окна, найденный по идентичности узла, вместе с цепочкой предков от вида содержимого.
pub struct Pressable {
    path: Vec<Retained<NSView>>,
}

impl Pressable {
    /// Нажимает вид тем же обработчиком, что щелчок: строка списка — выбором через таблицу, как
    /// `mouseDown:` таблицы, `NSButton` — `performClick:` с его действием, остальные — `AXPress`
    /// (у собственных видов — та же функция, что их `mouseDown:`). `false` — вид нажатие не принял.
    pub fn press(&self) -> bool {
        let Some(target) = self.path.last() else {
            return false;
        };
        if let Some(pressed) = virtual_list::press_row(&self.path) {
            return pressed;
        }
        // `AXPress` у `NSButton` выполняет действие, но возвращает `false`: отказ был бы ложным.
        if let Ok(button) = target.clone().downcast::<NSButton>() {
            if !button.isEnabled() {
                return false;
            }
            // SAFETY: отправитель не передаётся, `performClick:` зовёт действие кнопки на главном
            // потоке, как щелчок.
            unsafe { button.performClick(None) };
            return true;
        }
        target.accessibilityPerformPress()
    }
}

impl MainWindow {
    /// Вид окна с идентичностью `object` (поле `object` узла `ui.inspect`): в дереве вида
    /// содержимого или системная кнопка окна. `None` — такого вида в окне больше нет.
    pub fn pressable(&self, object: u64) -> Option<Pressable> {
        let root = self.root_view();
        let mut path = vec![root.retain()];
        if inspect::object_id(root) == object || find(root, object, &mut path) {
            return Some(Pressable { path });
        }
        let window = self.ns_window();
        [
            NSWindowButton::CloseButton,
            NSWindowButton::MiniaturizeButton,
            NSWindowButton::ZoomButton,
        ]
        .into_iter()
        .filter_map(|kind| window.standardWindowButton(kind))
        .find(|button| inspect::object_id(&**button) == object)
        .map(|button| Pressable {
            path: vec![Retained::into_super(Retained::into_super(button))],
        })
    }
}

/// Ищет в поддереве `view` вид с идентичностью `object`, дописывая в `path` цепочку до него.
fn find(view: &NSView, object: u64, path: &mut Vec<Retained<NSView>>) -> bool {
    for child in view.subviews().iter() {
        let hit = inspect::object_id(&*child) == object;
        path.push(child.clone());
        if hit || find(&child, object, path) {
            return true;
        }
        path.pop();
    }
    false
}
