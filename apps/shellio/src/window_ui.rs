//! Окно владельца на главном потоке: окно с заголовком, главное меню, зеркало состояния, монитор
//! цикла и единый обработчик [`UiCommand`].
//!
//! Состояние живёт только на главном потоке. Функции модуля вызываются из замыканий
//! [`MainThread::exec`]. Обратные вызовы AppKit (меню, кнопки заголовка, смена ячейки) не трогают
//! состояние синхронно: они ставят работу в очередь главного потока, поэтому повторного входа в
//! состояние изнутри вызова AppKit нет.

use std::cell::RefCell;
use std::mem::ManuallyDrop;

use shellio::launch::WindowMode;
use shellio::owner;
use shellio_appkit::{
    App, ChromeIntent, LoopMonitor, MainMenu, MainThread, MainWindow, MenuAction, ThemeSelection,
};
use shellio_core::{AppearanceChoice, Delivery, Mirror, MirrorStep, UiCommand};
use shellio_theme::AppearanceChoice as ThemeChoice;

thread_local! {
    /// Окно владельца; создаётся один раз после выбора роли и живёт до выхода процесса.
    /// `ManuallyDrop` не даёт слоту деструктора потока: `exit()` выполняет деструкторы главного
    /// потока, а выходы владельца деструкторов не выполняют (`owner.rs`), окно и монитор цикла
    /// при выходе не освобождаются.
    static WINDOW: RefCell<Option<ManuallyDrop<OwnerWindow>>> = const { RefCell::new(None) };
    /// Зеркало состояния ядра; ведёт его задача владельца доставками.
    static MIRROR: RefCell<Option<Mirror>> = const { RefCell::new(None) };
}

struct OwnerWindow {
    mode: WindowMode,
    window: MainWindow,
    menu: MainMenu,
    monitor: Option<LoopMonitor>,
    choice: AppearanceChoice,
    /// Поколение отрисовки: растёт с каждой видимой сменой состояния окна.
    generation: u64,
}

impl OwnerWindow {
    fn bump(&mut self) {
        self.generation += 1;
        if let Some(monitor) = &self.monitor {
            monitor.set_generation(self.generation);
        }
    }

    fn set_choice(&mut self, choice: AppearanceChoice) {
        self.choice = choice;
        let theme = theme_choice(choice);
        let overrides = self.window.overrides();
        self.window.set_overrides(shellio_theme::Overrides {
            choice: theme,
            ..overrides
        });
        self.window.chrome().set_choice(theme);
        self.menu.set_appearance(theme);
        self.bump();
    }

    /// Проекции зеркала: имя воркспейса в заголовке и системный заголовок окна.
    fn project(&mut self, projection: &Projection) {
        self.window.set_title(projection.title.as_deref());
        self.window
            .chrome()
            .set_workspace(projection.workspace.as_deref());
        self.bump();
    }
}

/// Проекции зеркала для окна.
struct Projection {
    workspace: Option<String>,
    title: Option<String>,
}

impl Projection {
    fn of(mirror: &Mirror) -> Projection {
        Projection {
            workspace: mirror.workspace_name().map(|name| name.as_str().to_owned()),
            title: mirror.window_title(),
        }
    }
}

/// Работа с окном владельца, если оно есть. Повторный вход не паникует: работа пропускается со
/// строкой в stderr.
fn with_window<R>(work: impl FnOnce(&mut OwnerWindow) -> R) -> Option<R> {
    WINDOW.with(|slot| match slot.try_borrow_mut() {
        Ok(mut window) => window.as_deref_mut().map(work),
        Err(_) => {
            owner::say_err("shellio: окно владельца занято, повторный вход пропущен");
            None
        }
    })
}

/// Отложить работу на следующий оборот очереди главного потока.
fn defer(work: impl FnOnce(&App) + Send + 'static) {
    MainThread::new().exec(work);
}

/// Создаёт окно владельца в режиме `mode`, не показывая его, ставит главное меню и монитор
/// цикла. Повторный вызов ничего не создаёт. Возвращает `windowNumber`.
pub fn create(app: &App, mode: WindowMode) -> i64 {
    if let Some(number) = with_window(|owner| owner.window.number()) {
        return number;
    }
    let window = MainWindow::new(
        app,
        |_: ThemeSelection| defer(|_| on_theme()),
        |intent| defer(move |app| on_intent(app, intent)),
    );
    let menu = MainMenu::install(app, |action| {
        defer(move |app| handle(app, command_of(action)));
    });
    let monitor = match LoopMonitor::install(app) {
        Ok(monitor) => Some(monitor),
        Err(error) => {
            owner::say_err(format_args!("shellio: монитор цикла не поставлен: {error}"));
            None
        }
    };
    let number = window.number();
    let projection = MIRROR.with(|mirror| mirror.borrow().as_ref().map(Projection::of));
    WINDOW.with(|slot| {
        *slot.borrow_mut() = Some(ManuallyDrop::new(OwnerWindow {
            mode,
            window,
            menu,
            monitor,
            choice: AppearanceChoice::System,
            generation: 0,
        }));
    });
    if let Some(projection) = projection {
        with_window(|owner| owner.project(&projection));
    }
    number
}

/// Режим окна владельца; `None` — окна нет.
#[expect(dead_code, reason = "режим читает ui.window в задаче 6 плана P1.3")]
pub fn mode() -> Option<WindowMode> {
    with_window(|owner| owner.mode)
}

/// Показывает окно владельца (`makeKeyAndOrderFront`). `false` — окна нет: процесс не владелец.
pub fn show(_app: &App) -> bool {
    with_window(|owner| owner.window.show()).is_some()
}

/// Приложение открыли снова: окно показывается, владелец просит передний план. Без окна ничего не
/// делается.
pub fn reopen(app: &App) {
    if show(app) {
        app.activate();
    }
}

/// Применить доставку к зеркалу. Применённая доставка обновляет проекции окна.
pub fn deliver(_app: &App, delivery: Delivery) -> MirrorStep {
    let (step, projection) = MIRROR.with(|slot| {
        let mut mirror = slot.borrow_mut();
        let step = match (&mut *mirror, delivery) {
            (Some(mirror), delivery) => mirror.apply(delivery),
            (None, Delivery::Snapshot(snapshot)) => {
                *mirror = Some(Mirror::from_snapshot(snapshot));
                MirrorStep::Applied
            }
            (None, Delivery::Event(_) | Delivery::Reset) => MirrorStep::NeedSnapshot,
        };
        let projection = match step {
            MirrorStep::Applied => mirror.as_ref().map(Projection::of),
            MirrorStep::Ignored | MirrorStep::NeedSnapshot => None,
        };
        (step, projection)
    });
    if let Some(projection) = projection {
        with_window(|owner| owner.project(&projection));
    }
    step
}

/// Единый обработчик действий интерфейса: меню, кнопка заголовка, с задачи 6 — CLI.
pub fn handle(app: &App, command: UiCommand) {
    match command {
        UiCommand::WindowClose => {
            with_window(|owner| owner.window.hide());
        }
        // Вне состояния окна: `terminate:` синхронно спрашивает делегата приложения.
        UiCommand::AppQuit => app.terminate(),
        UiCommand::AppearanceSet(choice) => {
            with_window(|owner| owner.set_choice(choice));
        }
    }
}

fn on_theme() {
    with_window(OwnerWindow::bump);
}

fn on_intent(app: &App, intent: ChromeIntent) {
    match intent {
        ChromeIntent::AppearanceCycle => {
            if let Some(choice) = with_window(|owner| owner.choice) {
                handle(app, UiCommand::AppearanceSet(choice.next()));
            }
        }
        // Меню воркспейса появится вместе с источником в P1.7.
        ChromeIntent::WorkspaceMenu => {}
    }
}

fn command_of(action: MenuAction) -> UiCommand {
    match action {
        MenuAction::Quit => UiCommand::AppQuit,
        MenuAction::CloseWindow => UiCommand::WindowClose,
        MenuAction::Appearance(choice) => UiCommand::AppearanceSet(core_choice(choice)),
    }
}

fn theme_choice(choice: AppearanceChoice) -> ThemeChoice {
    match choice {
        AppearanceChoice::System => ThemeChoice::System,
        AppearanceChoice::Light => ThemeChoice::Light,
        AppearanceChoice::Dark => ThemeChoice::Dark,
    }
}

fn core_choice(choice: ThemeChoice) -> AppearanceChoice {
    match choice {
        ThemeChoice::System => AppearanceChoice::System,
        ThemeChoice::Light => AppearanceChoice::Light,
        ThemeChoice::Dark => AppearanceChoice::Dark,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_actions_map_to_ui_commands() {
        assert_eq!(command_of(MenuAction::Quit), UiCommand::AppQuit);
        assert_eq!(command_of(MenuAction::CloseWindow), UiCommand::WindowClose);
        for choice in AppearanceChoice::ALL {
            assert_eq!(core_choice(theme_choice(choice)), choice);
            assert_eq!(
                command_of(MenuAction::Appearance(theme_choice(choice))),
                UiCommand::AppearanceSet(choice)
            );
        }
    }
}
