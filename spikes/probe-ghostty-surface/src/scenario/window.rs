//! Сценарий `window`: окно без поверхности открывается и становится key, контрольная клавиша
//! проходит через таблицу стенда, процесс — исполняемый файл подписанного бандла пробы.
//!
//! Здесь же чтение постоянной части окружения: система, железо, сборка из `Info.plist`, бандл,
//! дисплей, окно и источник ввода.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::{CString, c_void};
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2_app_kit::{NSScreen, NSWindow};
use objc2_core_foundation::{CFRetained, CFString, CFType};
use objc2_core_graphics::CGDisplayIsBuiltin;
use objc2_foundation::{NSBundle, NSProcessInfo, NSRect, NSString};

use super::events::{self, Key};
use super::{Poll, Report, Scenario, Stand, Step, poll, run_steps};
use crate::clock;
use crate::results::{
    Build, BundleInfo, DisplayInfo, Environment, Hardware, ProcessInfo, Rect, Size, WindowInfo,
};

/// Идентификатор подписи и `CFBundleIdentifier` бандла пробы (ADR-0034, «Уточнено в P0.3»).
pub const BUNDLE_ID: &str = "io.github.dsbasko.shellio.probe.ghostty-surface";
/// Designated requirement продукта: из него берётся SHA-1 сертификата `Shellio Dev`.
const PRODUCT_REQUIREMENT: &str = include_str!("../../../../scripts/signing/requirement.txt");
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);
const KEY_TIMEOUT: Duration = Duration::from_secs(3);
/// Запись `xterm-ghostty` в terminfo бандла: каталог назван шестнадцатеричным кодом буквы.
const TERMINFO_ENTRY: &str = "78/xterm-ghostty";

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::Window);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let (bundle, problems) = verify_bundle();
    report.bundle = Some(bundle);
    if !problems.is_empty() {
        return stand.conclude(
            report,
            Err(format!("не бандл пробы: {}", problems.join("; "))),
        );
    }

    let posted: Rc<RefCell<Vec<u64>>> = Rc::default();
    let focus = Rc::clone(&stand);
    let posting = Rc::clone(&posted);
    let arrived = Rc::clone(&stand);
    let numbers = Rc::clone(&posted);
    let passed = Rc::clone(&posted);
    let steps = vec![
        Step::new(
            "активность, key window и first responder",
            ACTIVATION_TIMEOUT,
            move || focused(&focus),
            move || {
                *posting.borrow_mut() = events::press(Key::RETURN)?;
                Ok(())
            },
        ),
        Step::new(
            "контрольная клавиша вернулась из sendEvent и дошла до вида",
            KEY_TIMEOUT,
            move || {
                let returned = numbers
                    .borrow()
                    .iter()
                    .all(|number| events::record(*number).is_some_and(|r| r.send_return.is_some()));
                let arrivals = arrived.view.arrivals();
                let kinds: Vec<&str> = arrivals.iter().map(|arrival| arrival.kind).collect();
                poll(returned && kinds.len() >= 2, || {
                    format!("sendEvent вернулся у всех: {returned}; дошло до вида: {kinds:?}")
                })
            },
            || Ok(()),
        ),
        Step::new(
            "проход отображения после клавиши",
            KEY_TIMEOUT,
            move || {
                let last = passed
                    .borrow()
                    .last()
                    .and_then(|number| events::record(*number))
                    .and_then(|record| record.send_return);
                let pass = last.and_then(events::display_pass_after);
                poll(pass.is_some(), || {
                    format!("возврат keyUp {last:?}, прохода после него нет")
                })
            },
            || Ok(()),
        ),
    ];
    run_steps(steps, move |result| {
        if let Err(failure) = result {
            return stand.conclude(report, Err(failure.describe()));
        }
        let verdict = control_key(&stand, &posted.borrow(), &mut report);
        stand.conclude(report, verdict);
    });
}

fn focused(stand: &Stand) -> Poll<()> {
    let key = stand.app.keyWindow();
    let key_is_main = key
        .as_deref()
        .is_some_and(|key| std::ptr::eq(key, &*stand.window));
    let responder = stand.window.firstResponder();
    let view_is_responder = responder
        .as_deref()
        .is_some_and(|responder| std::ptr::eq(responder, stand.view.as_ref()));
    poll(
        stand.app.isActive() && key_is_main && view_is_responder,
        || events::focus_state(&stand.app),
    )
}

/// Контрольная клавиша дошла до вида ровно одной парой `keyDown` и `keyUp`.
fn control_key(stand: &Stand, numbers: &[u64], report: &mut Report) -> Result<(), String> {
    let arrivals = stand.view.arrivals();
    let kinds: Vec<(&str, u16)> = arrivals.iter().map(|a| (a.kind, a.key_code)).collect();
    report.push("control_key_arrivals", format!("{kinds:?}"));
    let expected = [("keyDown", Key::RETURN.code), ("keyUp", Key::RETURN.code)];
    if kinds != expected {
        return Err(format!(
            "контрольная клавиша дошла до вида как {kinds:?}, ожидалось {expected:?}"
        ));
    }
    let records: Vec<events::Record> = numbers.iter().filter_map(|n| events::record(*n)).collect();
    let mut dispatch = Vec::new();
    let mut to_view = Vec::new();
    for (record, arrival) in records.iter().zip(&arrivals) {
        if let Some(back) = record.send_return {
            dispatch.push(clock::interval_ms(record.t_post, back));
        }
        to_view.push(clock::interval_ms(record.t_post, arrival.at_ns));
    }
    report.metric("control_key_t_post_to_return", "ms", dispatch);
    report.metric("control_key_t_post_to_view", "ms", to_view);
    if let Some(pass) = records
        .last()
        .and_then(|record| record.send_return)
        .and_then(events::display_pass_after)
        && let Some(first) = records.first()
    {
        report.metric(
            "control_key_t_post_to_display_pass",
            "ms",
            vec![clock::interval_ms(first.t_post, pass)],
        );
    }
    Ok(())
}

/// Бандл, из которого запущен процесс, и причины, по которым это не подписанный бандл пробы.
pub fn verify_bundle() -> (BundleInfo, Vec<String>) {
    let mut bundle = bundle_info();
    let mut problems = Vec::new();
    if bundle.identifier.as_deref() != Some(BUNDLE_ID) {
        problems.push(format!(
            "CFBundleIdentifier {:?}, ожидался {BUNDLE_ID}",
            bundle.identifier
        ));
    }
    let main = NSBundle::mainBundle();
    match bundle.path.as_deref().map(PathBuf::from) {
        Some(path) if path.extension().is_some_and(|ext| ext == "app") => {
            let macos = path.join("Contents/MacOS");
            match std::env::current_exe().and_then(std::fs::canonicalize) {
                Ok(exe) if exe.parent() == std::fs::canonicalize(&macos).ok().as_deref() => {}
                Ok(exe) => problems.push(format!(
                    "исполняемый файл {} не в {}",
                    exe.display(),
                    macos.display()
                )),
                Err(err) => problems.push(format!("путь исполняемого файла: {err}")),
            }
        }
        path => problems.push(format!("главный бандл {path:?} не .app")),
    }
    match main
        .resourcePath()
        .map(|path| PathBuf::from(path.to_string()))
    {
        Some(resources) => problems.extend(missing_resources(&resources)),
        None => problems.push("у бандла нет Resources".to_owned()),
    }
    bundle.signature = match probe_requirement(PRODUCT_REQUIREMENT) {
        Some(requirement) => match check_signature(&requirement) {
            Ok(()) => format!("valid: {requirement}"),
            Err(reason) => {
                problems.push(format!("подпись: {reason}"));
                format!("invalid: {reason}")
            }
        },
        None => {
            problems.push("в requirement.txt нет SHA-1 сертификата".to_owned());
            "unchecked: нет требования".to_owned()
        }
    };
    (bundle, problems)
}

/// Ресурсы ghostty из набора, которых нет в `Resources` бандла.
pub fn missing_resources(resources: &Path) -> Vec<String> {
    let mut missing = Vec::new();
    for dir in ["ghostty", "ghostty/shell-integration", "terminfo"] {
        if !resources.join(dir).is_dir() {
            missing.push(format!("нет Resources/{dir}"));
        }
    }
    if !resources.join("terminfo").join(TERMINFO_ENTRY).is_file() {
        missing.push(format!("нет Resources/terminfo/{TERMINFO_ENTRY}"));
    }
    missing
}

/// Designated requirement бандла пробы: свой идентификатор и тот же сертификат, что у продукта.
pub fn probe_requirement(product: &str) -> Option<String> {
    let marker = "certificate leaf = H\"";
    let start = product.find(marker)? + marker.len();
    let sha1 = product.get(start..start + 40)?;
    if !sha1.bytes().all(|byte| byte.is_ascii_hexdigit()) || !product[start + 40..].starts_with('"')
    {
        return None;
    }
    Some(format!(
        "identifier \"{BUNDLE_ID}\" and certificate leaf = H\"{sha1}\""
    ))
}

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecCodeCopySelf(flags: u32, code: *mut *const c_void) -> i32;
    fn SecRequirementCreateWithString(
        text: *const CFString,
        flags: u32,
        requirement: *mut *const c_void,
    ) -> i32;
    fn SecCodeCheckValidity(code: *const c_void, flags: u32, requirement: *const c_void) -> i32;
}

/// Владеет объектом Security, который вернула функция с `Copy` или `Create` в имени.
fn owned(pointer: *const c_void) -> Option<CFRetained<CFType>> {
    // SAFETY: функции Security с `Copy`/`Create` возвращают объект с +1, владение переходит сюда.
    NonNull::new(pointer.cast_mut().cast::<CFType>())
        .map(|ptr| unsafe { CFRetained::from_raw(ptr) })
}

/// Подпись запущенного кода удовлетворяет требованию пробы.
fn check_signature(requirement: &str) -> Result<(), String> {
    let mut code: *const c_void = std::ptr::null();
    // SAFETY: указатель на локальную переменную для результата, флаги по умолчанию.
    let status = unsafe { SecCodeCopySelf(0, &mut code) };
    let code = owned(code)
        .filter(|_| status == 0)
        .ok_or(format!("SecCodeCopySelf: OSStatus {status}"))?;
    let text = CFString::from_str(requirement);
    let mut parsed: *const c_void = std::ptr::null();
    // SAFETY: строка требования жива на время вызова, результат пишется в локальную переменную.
    let status = unsafe { SecRequirementCreateWithString(&*text, 0, &mut parsed) };
    let parsed = owned(parsed)
        .filter(|_| status == 0)
        .ok_or(format!("SecRequirementCreateWithString: OSStatus {status}"))?;
    let code_ref: *const CFType = &*code;
    let requirement_ref: *const CFType = &*parsed;
    // SAFETY: оба объекта живы на время вызова.
    let status = unsafe { SecCodeCheckValidity(code_ref.cast(), 0, requirement_ref.cast()) };
    if status == 0 {
        Ok(())
    } else {
        Err(format!("SecCodeCheckValidity: OSStatus {status}"))
    }
}

fn bundle_info() -> BundleInfo {
    let main = NSBundle::mainBundle();
    BundleInfo {
        path: Some(main.bundlePath().to_string()),
        identifier: main.bundleIdentifier().map(|id| id.to_string()),
        executable: main.executablePath().map(|path| path.to_string()),
        signature: "не проверялась".to_owned(),
    }
}

/// Строка из `Info.plist` главного бандла.
fn info_string(key: &str) -> Option<String> {
    let value = NSBundle::mainBundle().objectForInfoDictionaryKey(&NSString::from_str(key))?;
    value
        .downcast::<NSString>()
        .ok()
        .map(|value| value.to_string())
}

/// Постоянная часть окружения процесса сценария.
pub fn static_environment(process: ProcessInfo) -> Environment {
    Environment {
        os: NSProcessInfo::processInfo()
            .operatingSystemVersionString()
            .to_string(),
        hardware: Hardware {
            model: sysctl_string("hw.model"),
            cpu: sysctl_string("machdep.cpu.brand_string"),
            memory_bytes: sysctl_u64("hw.memsize"),
        },
        build: Build::from_info(info_string),
        process,
        bundle: Some(bundle_info()),
        display: None,
        window: None,
        input_source: None,
    }
}

/// Сведения о процессе пробы: PID, маршрут, nonce и окружение на входе в `main`.
pub fn process_info(
    route: &str,
    nonce: Option<String>,
    env: &[(String, String)],
    extra: Vec<String>,
) -> ProcessInfo {
    ProcessInfo {
        pid: std::process::id(),
        route: route.to_owned(),
        nonce,
        env: env.iter().cloned().collect::<BTreeMap<_, _>>(),
        env_extra: extra,
    }
}

fn sysctl_raw(name: &str) -> Option<Vec<u8>> {
    let name = CString::new(name).ok()?;
    let mut len = 0usize;
    // SAFETY: первый вызов узнаёт размер значения, буфер не передаётся.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || len == 0 {
        return None;
    }
    let mut buffer = vec![0u8; len];
    // SAFETY: буфер длины `len` жив на время вызова.
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    buffer.truncate(len);
    Some(buffer)
}

fn sysctl_string(name: &str) -> Option<String> {
    let bytes = sysctl_raw(name)?;
    let text = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
    Some(String::from_utf8_lossy(text).trim().to_owned()).filter(|text| !text.is_empty())
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let bytes: [u8; 8] = sysctl_raw(name)?.try_into().ok()?;
    Some(u64::from_ne_bytes(bytes))
}

fn rect(frame: NSRect) -> Rect {
    Rect {
        x: frame.origin.x,
        y: frame.origin.y,
        width: frame.size.width,
        height: frame.size.height,
    }
}

pub fn display_info(screen: &NSScreen, chosen_builtin: bool) -> DisplayInfo {
    let id = screen.CGDirectDisplayID();
    DisplayInfo {
        id,
        name: screen.localizedName().to_string(),
        builtin: CGDisplayIsBuiltin(id),
        chosen: if chosen_builtin { "builtin" } else { "main" }.to_owned(),
        frame: rect(screen.frame()),
        scale: screen.backingScaleFactor(),
        max_fps: screen.maximumFramesPerSecond(),
    }
}

pub fn window_info(window: &NSWindow) -> Option<WindowInfo> {
    let view = window.contentView()?;
    let bounds = view.bounds();
    let pixels = view.convertRectToBacking(bounds).size;
    Some(WindowInfo {
        frame: rect(window.frame()),
        content_points: Size {
            width: bounds.size.width,
            height: bounds.size.height,
        },
        bitmap_pixels: Size {
            width: pixels.width,
            height: pixels.height,
        },
        backing_scale: window.backingScaleFactor(),
    })
}

/// Источник ввода по field editor окна: у вида без `NSTextInputClient` контекста ввода нет.
pub fn input_source(window: &NSWindow) -> Option<String> {
    // SAFETY: `nil` вместо объекта — документированный запрос общего field editor окна.
    let editor: Retained<_> = unsafe { window.fieldEditor_forObject(true, None) }?;
    let source = editor.inputContext()?.selectedKeyboardInputSource()?;
    Some(source.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn requirement_takes_certificate_from_product() {
        let product = "designated => identifier \"io.github.dsbasko.shellio\" and certificate leaf = H\"406f897fd84d95069d9a50abd81ab09b7afb7e55\"\n";
        assert_eq!(
            probe_requirement(product).as_deref(),
            Some(
                "identifier \"io.github.dsbasko.shellio.probe.ghostty-surface\" and certificate leaf = H\"406f897fd84d95069d9a50abd81ab09b7afb7e55\""
            )
        );
        assert!(probe_requirement(PRODUCT_REQUIREMENT).is_some());
        assert_eq!(probe_requirement("identifier \"x\""), None);
        assert_eq!(
            probe_requirement("certificate leaf = H\"406f897fd84d95069d9a50abd81ab09b7afb7e5\""),
            None,
            "39 знаков"
        );
        assert_eq!(
            probe_requirement("certificate leaf = H\"zz6f897fd84d95069d9a50abd81ab09b7afb7e55\""),
            None
        );
    }

    #[test]
    fn identifier_matches_run_script_and_info_plist() {
        assert_eq!(super::super::tests::script_value("BUNDLE_ID"), BUNDLE_ID);
        let plist = include_str!("../../Info.plist");
        assert!(
            plist.contains(&format!("<string>{BUNDLE_ID}</string>")),
            "Info.plist без {BUNDLE_ID}"
        );
        assert!(plist.contains(&format!("<string>{}</string>", crate::launch::EXECUTABLE)));
        assert_ne!(BUNDLE_ID, "io.github.dsbasko.shellio");
    }

    #[test]
    fn missing_resources_are_listed() {
        let root = std::env::temp_dir().join(format!(
            "probe-ghostty-surface-resources-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("ghostty/shell-integration")).unwrap();
        assert_eq!(
            missing_resources(&root),
            [
                "нет Resources/terminfo",
                "нет Resources/terminfo/78/xterm-ghostty"
            ]
        );
        fs::create_dir_all(root.join("terminfo/78")).unwrap();
        fs::write(root.join("terminfo").join(TERMINFO_ENTRY), b"entry").unwrap();
        assert!(missing_resources(&root).is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn process_info_keeps_entry_environment() {
        let env = [("LANG".to_owned(), "en_US.UTF-8".to_owned())];
        let info = process_info("direct", Some("ab".to_owned()), &env, vec!["X".to_owned()]);
        assert_eq!(info.pid, std::process::id());
        assert_eq!(info.env["LANG"], "en_US.UTF-8");
        assert_eq!(info.env_extra, ["X"]);
    }
}
