//! Тело панели у хоста: вид плагина вместе с источником токенов его работы.

use serde_json::Value;
use shellio_appkit::App;
use shellio_theme::AppTheme;

use crate::work::ViewWork;
use crate::{PanelSink, PluginView, ViewCtx, ViewFactory};

/// Построенное тело панели. Закрытие — [`Body::close`] или освобождение — отменяет работу вида,
/// затем зовёт `close_view` и освобождает вид; владение делает это ровно один раз.
pub struct Body {
    view: Box<dyn PluginView>,
    work: ViewWork,
}

impl Body {
    /// Строит тело фабрикой плагина на главном потоке.
    pub fn build(
        factory: &dyn ViewFactory,
        app: &App,
        theme: &AppTheme,
        sink: PanelSink,
        state: &Value,
    ) -> Self {
        let work = ViewWork::new();
        let ctx = ViewCtx {
            app,
            theme,
            sink,
            work: work.clone(),
        };
        Self::new(factory.build(ctx, state), work)
    }

    /// Тело из готового вида и источника токенов, который получила его фабрика.
    pub fn new(view: Box<dyn PluginView>, work: ViewWork) -> Self {
        Self { view, work }
    }

    #[must_use]
    pub fn view(&self) -> &dyn PluginView {
        self.view.as_ref()
    }

    pub fn view_mut(&mut self) -> &mut dyn PluginView {
        self.view.as_mut()
    }

    /// Закрывает вид: работа по токенам отменена, подписки сняты, вид освобождён.
    pub fn close(self) {}
}

impl Drop for Body {
    fn drop(&mut self) {
        self.work.cancel();
        self.view.close_view();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::mpsc;
    use std::thread;

    use shellio_appkit::{InspectNode, Piece};
    use shellio_layout::Geometry;
    use shellio_plugin_api::Presentation;

    use super::*;
    use crate::WorkToken;

    /// Что вид увидел в `close_view`: отменена ли к этому времени его работа.
    type Closes = Rc<RefCell<Vec<bool>>>;

    struct FakeView {
        token: WorkToken,
        closes: Closes,
    }

    impl PluginView for FakeView {
        fn body(&self) -> Piece<'_> {
            unimplemented!("тесты без NSApplication тело не размещают")
        }
        fn adapt(&mut self, _geometry: &Geometry, _presentation: &Presentation) {}
        fn update(&mut self, _state: &Value) {}
        fn set_theme(&mut self, _theme: &AppTheme) {}
        fn set_active(&mut self, _active: bool) {}
        fn set_focused(&mut self, _focused: bool) {}
        fn focus_preferred(&self) {}
        fn inspect(&self, _prefix: &str) -> Vec<InspectNode> {
            Vec::new()
        }
        fn close_view(&mut self) {
            self.closes.borrow_mut().push(self.token.is_cancelled());
        }
    }

    fn body() -> (Body, ViewWork, Closes) {
        let work = ViewWork::new();
        let closes = Closes::default();
        let view = FakeView {
            token: work.token(),
            closes: Rc::clone(&closes),
        };
        (Body::new(Box::new(view), work.clone()), work, closes)
    }

    #[derive(Debug, PartialEq, Eq)]
    enum Report {
        Delivered(&'static str),
        Cancelled(&'static str),
    }

    /// Работа на своём потоке: ждёт разрешения шага и отдаёт результат, если токена нет или он не
    /// отменён.
    struct Job {
        step: mpsc::Sender<()>,
        thread: thread::JoinHandle<()>,
    }

    impl Job {
        fn start(
            name: &'static str,
            token: Option<WorkToken>,
            reports: mpsc::Sender<Report>,
        ) -> Self {
            let (step, steps) = mpsc::channel::<()>();
            let thread = thread::spawn(move || {
                while steps.recv().is_ok() {
                    let cancelled = token.as_ref().is_some_and(WorkToken::is_cancelled);
                    let report = if cancelled {
                        Report::Cancelled(name)
                    } else {
                        Report::Delivered(name)
                    };
                    if reports.send(report).is_err() || cancelled {
                        return;
                    }
                }
            });
            Self { step, thread }
        }

        fn step(&self) {
            self.step.send(()).unwrap();
        }

        fn finish(self) {
            drop(self.step);
            self.thread.join().unwrap();
        }
    }

    #[test]
    fn close_view_cancels_view_work_and_spares_independent_work() {
        let (body, work, closes) = body();
        let (reports, inbox) = mpsc::channel();
        let bound = Job::start("view", Some(work.token()), reports.clone());
        let independent = Job::start("independent", None, reports);

        bound.step();
        assert_eq!(inbox.recv().unwrap(), Report::Delivered("view"));
        independent.step();
        assert_eq!(inbox.recv().unwrap(), Report::Delivered("independent"));
        assert!(closes.borrow().is_empty());

        body.close();
        assert_eq!(
            *closes.borrow(),
            [true],
            "close_view один раз, работа вида к нему уже отменена"
        );

        bound.step();
        assert_eq!(inbox.recv().unwrap(), Report::Cancelled("view"));
        independent.step();
        assert_eq!(
            inbox.recv().unwrap(),
            Report::Delivered("independent"),
            "работа без токена живёт и после close_view"
        );

        bound.finish();
        independent.finish();
    }

    #[test]
    fn dropped_body_closes_the_view_once() {
        let (body, work, closes) = body();
        let token = work.token();
        assert!(!token.is_cancelled());
        drop(body);
        assert_eq!(*closes.borrow(), [true]);
        assert!(token.is_cancelled());
    }

    #[test]
    fn view_without_intents_refuses_them_and_gives_no_args() {
        use crate::{Handled, ListMove, ViewIntent};

        let (mut body, _work, _closes) = body();
        for step in ListMove::ALL {
            assert_eq!(
                body.view_mut().intent(&ViewIntent::List(step)),
                Handled::No,
                "{step:?}"
            );
        }
        let filter = ViewIntent::Filter {
            text: "аль".to_owned(),
        };
        assert_eq!(body.view_mut().intent(&filter), Handled::No);
        assert_eq!(body.view().resolve_args("fake.fill"), None);
    }

    #[test]
    fn open_body_keeps_its_work_and_reaches_the_view() {
        let (mut body, work, closes) = body();
        body.view_mut().set_active(true);
        body.view().focus_preferred();
        assert!(!work.is_cancelled());
        assert!(closes.borrow().is_empty());
    }
}
