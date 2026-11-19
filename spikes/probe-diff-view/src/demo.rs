//! Демонстрация без стенда и `result.json` (задача 16): фикстура открывается в окне варианта и
//! режима, клавиши и мышь идут в те же обработчики сцены, что у сценариев. Стенд событий,
//! перенос курсора и учёт покрытия не ставятся. Фикстура генерируется или читается на рабочем
//! потоке, главный поток в это время обслуживает окно. `o` открывает фикстуру заново.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver};
use std::thread;

use objc2::MainThreadMarker;
use objc2_app_kit::{NSResponder, NSWindow};
use objc2_foundation::NSSize;

use probe_diff_view::fixtures::{self, Fixture};
use probe_diff_view::model::Mode;
use probe_diff_view::model::layout::{MIN_HEIGHT, MIN_WIDTH};
use probe_diff_view::open::OpenFixture;
use probe_diff_view::real::{self, RealSource};

use crate::args::{Demo, ViewMode};
use crate::scenario::drawmark;
use crate::scenario::stage::View;
use crate::scene::{OpenAt, Scene};

/// Генерация синтетической фикстуры или чтение подготовленной `real` (её активный файл — первый
/// в порядке diff).
pub fn read(fixture: OpenFixture, fixtures: Option<&Path>) -> Result<Fixture, String> {
    if let Some(name) = fixture.synthetic() {
        return Ok(fixtures::generate(name));
    }
    let Some(dir) = fixtures else {
        return Err("фикстуре real нужен каталог фикстур".to_owned());
    };
    let dir = real::dir(dir);
    match real::load(&dir)? {
        None => Err(format!("фикстура real не подготовлена в {}", dir.display())),
        Some(RealSource::NotRun(reason)) => Err(format!("фикстура real: {reason}")),
        Some(RealSource::Prepared(prepared)) => prepared
            .fixture(0)
            .ok_or_else(|| "фикстура real без файлов".to_owned()),
    }
}

/// Фикстура демонстрации на рабочем потоке, итог — в канал.
pub fn load(demo: &Demo) -> Result<Receiver<Result<Fixture, String>>, String> {
    let (tx, rx) = mpsc::channel();
    let fixture = demo.fixture;
    let fixtures: Option<PathBuf> = demo.fixtures.clone();
    thread::Builder::new()
        .name("probe-diff-demo-load".to_owned())
        .spawn(move || {
            let _ = tx.send(read(fixture, fixtures.as_deref()));
        })
        .map_err(|err| format!("поток загрузки не запущен: {err}"))?;
    Ok(rx)
}

/// Открытый diff демонстрации: вид и сцена живут, пока открыто окно.
pub struct Opened {
    _view: View,
    scene: Rc<Scene>,
    path: String,
}

impl Opened {
    pub fn errors(&self) -> Vec<String> {
        self.scene.errors()
    }

    /// Путь активного файла фикстуры: до разбора diff сцена его ещё не знает.
    pub fn path(&self) -> &str {
        &self.path
    }
}

/// Вид варианта содержимым окна и first responder, сцена с фикстурой, открытие без клавиши.
pub fn mount(
    mtm: MainThreadMarker,
    window: &NSWindow,
    demo: &Demo,
    fixture: Fixture,
) -> Result<Opened, String> {
    let theme = crate::text::current_theme(mtm);
    let bounds = window
        .contentView()
        .map(|content| content.bounds())
        .unwrap_or_else(|| window.contentLayoutRect());
    let view = View::new(demo.variant, mtm, bounds, theme);
    let mode = match demo.mode {
        ViewMode::Unified => Mode::Unified,
        ViewMode::Side => Mode::SideBySide,
    };
    let path = fixture.path.clone();
    let scene = Scene::new(fixture, mode, view.font())?;
    view.bind(&scene);
    scene.attach(view.surface());
    window.setContentView(Some(view.scroll_view()));
    // Ниже `min_size` сцена раскладку не меняет: заглушку показывает только хост зон.
    window.setContentMinSize(NSSize::new(MIN_WIDTH, MIN_HEIGHT));
    let responder: &NSResponder = view.responder();
    if !window.makeFirstResponder(Some(responder)) {
        return Err("document view не стал first responder".to_owned());
    }
    // Журналы отметок читает только стенд. Наблюдатель забирает журнал перед каждой записью,
    // поэтому в нём остаётся одна последняя запись, а не след всего показа.
    drawmark::on_mark(|_| drop(drawmark::take()));
    drawmark::on_declare(|_| drop(drawmark::take_declared()));
    scene.set_open_at(if demo.fixture.at_last_hunk() {
        OpenAt::LastHunk
    } else {
        OpenAt::Top
    });
    scene.open();
    Ok(Opened {
        _view: view,
        scene,
        path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_needs_a_prepared_dir() {
        assert!(read(OpenFixture::Real, None).is_err());
        let dir = std::env::temp_dir().join(format!("probe-diff-demo-{}", std::process::id()));
        let reason = read(OpenFixture::Real, Some(&dir))
            .err()
            .unwrap_or_default();
        assert!(reason.contains("не подготовлена"), "{reason}");
    }

    #[test]
    fn synthetic_fixture_is_generated() {
        let fixture = read(OpenFixture::Wrap, None).unwrap();
        assert_eq!(fixture.name, "wrap");
        assert!(!fixture.diff.is_empty());
    }
}
