//! Сцены моста от эталона (функция `vt`): исходник сцены и суффиксы шагов идут в `libghostty-vt`
//! ревизии поверхности, после каждого шага записываются вид и кодирование проб ввода.

use std::fs;
use std::path::{Path, PathBuf};

use super::{
    CLICK, MOUSE_CELL_PX, PASTE_TEXT, Probes, SCENES, SCENES_FILE, SceneSource, SceneSpec, Scenes,
    Step, View, paste_bytes, read_text_lines,
};
use crate::fixture::channel::hex;
use crate::matrix::report_policy::SCENES_DIR;
use crate::oracle::model::PROFILE_VERSION;
use crate::vt::{
    Area, Key, KeyAction, KeyEncoder, KeyInput, Mode, MouseAction, MouseButton, MouseEncoder,
    MouseGeometry, MouseInput, Screen, Terminal,
};

/// Сцены в `<out>/scenes/bridge.json`. Возвращает путь файла.
pub fn build_scenes(out: &Path) -> Result<PathBuf, String> {
    let dir = out.join(SCENES_DIR);
    fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    let scenes = SCENES
        .iter()
        .map(|source| scene(source).map_err(|err| format!("сцена {}: {err}", source.name)))
        .collect::<Result<Vec<_>, _>>()?;
    let manifest = Scenes {
        profile: PROFILE_VERSION,
        vt_stamp: std::env::var("SHELLIO_PROBE_STAMP_VT")
            .ok()
            .filter(|value| !value.is_empty()),
        scenes,
    };
    let path = dir.join(SCENES_FILE);
    let text =
        serde_json::to_string_pretty(&manifest).map_err(|err| format!("сцены моста: {err}"))?;
    fs::write(&path, text).map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(path)
}

fn scene(source: &SceneSource) -> Result<SceneSpec, String> {
    let bytes = (source.source)();
    let mut term = Terminal::new(source.cols, source.rows).map_err(|err| err.to_string())?;
    term.write(&bytes);
    let mut steps = Vec::new();
    for (name, suffix) in source.steps {
        term.write(suffix.as_bytes());
        steps.push(Step {
            name: (*name).to_owned(),
            suffix_hex: hex(suffix.as_bytes()),
            view: view(&term)?,
            probes: if source.probes {
                Some(probes(&term, source)?)
            } else {
                None
            },
        });
    }
    if source.probes {
        distinct_levels(&steps)?;
    }
    Ok(SceneSpec {
        name: source.name.to_owned(),
        cols: source.cols,
        rows: source.rows,
        source_hex: hex(&bytes),
        steps,
    })
}

/// Сцена различает уровни стека Kitty только если «a» кодируется на них по-разному: иначе суффикс
/// `pop` прошёл бы пусто.
fn distinct_levels(steps: &[Step]) -> Result<(), String> {
    let keys: Vec<&str> = steps
        .iter()
        .take(3)
        .filter_map(|step| step.probes.as_ref().map(|probes| probes.key_a_hex.as_str()))
        .collect();
    let distinct =
        keys.len() == 3 && keys[0] != keys[1] && keys[1] != keys[2] && keys[0] != keys[2];
    if distinct {
        Ok(())
    } else {
        Err(format!(
            "кодирование «a» не различает уровни стека Kitty: {keys:?}"
        ))
    }
}

fn view(term: &Terminal) -> Result<View, String> {
    let rows = term.rows().map_err(|err| err.to_string())?;
    let mut lines = Vec::new();
    for y in 0..rows {
        let text = term.row_text(y).map_err(|err| err.to_string())?;
        let wrapped = term
            .cell(Area::Active, 0, u32::from(y))
            .map_err(|err| err.to_string())?
            .row_wrap;
        lines.push((text, wrapped));
    }
    let cursor = term.cursor().map_err(|err| err.to_string())?;
    let screen = term.active_screen().map_err(|err| err.to_string())?;
    Ok(View {
        lines: read_text_lines(&lines),
        cursor_x: cursor.x,
        cursor_y: cursor.y,
        alternate: screen == Screen::Alternate,
    })
}

fn probes(term: &Terminal, source: &SceneSource) -> Result<Probes, String> {
    let mut keys = KeyEncoder::new().map_err(|err| err.to_string())?;
    keys.sync_from(term);
    let key_a = keys
        .encode(&KeyInput::press(Key::A, "a"))
        .map_err(|err| err.to_string())?;
    let key_up = keys
        .encode(&KeyInput {
            key: Key::ARROW_UP,
            action: KeyAction::Press,
            mods: 0,
            text: String::new(),
            unshifted: None,
        })
        .map_err(|err| err.to_string())?;
    let (cell_width, cell_height) = MOUSE_CELL_PX;
    let mut mouse = MouseEncoder::new(MouseGeometry {
        screen_width: u32::from(source.cols) * cell_width,
        screen_height: u32::from(source.rows) * cell_height,
        cell_width,
        cell_height,
    })
    .map_err(|err| err.to_string())?;
    mouse.sync_from(term);
    let at = |action| MouseInput {
        action,
        button: Some(MouseButton::Left),
        mods: 0,
        x: (f32::from(CLICK.0) + 0.5) * cell_width as f32,
        y: (f32::from(CLICK.1) + 0.5) * cell_height as f32,
    };
    let mut click = mouse
        .encode(&at(MouseAction::Press))
        .map_err(|err| err.to_string())?;
    mouse.set_any_button_pressed(true);
    click.extend(
        mouse
            .encode(&at(MouseAction::Release))
            .map_err(|err| err.to_string())?,
    );
    let bracketed = term
        .mode(Mode::BRACKETED_PASTE)
        .map_err(|err| err.to_string())?;
    Ok(Probes {
        key_a_hex: hex(&key_a),
        key_up_hex: hex(&key_up),
        click_hex: hex(&click),
        paste_hex: hex(&paste_bytes(PASTE_TEXT, bracketed)),
    })
}
