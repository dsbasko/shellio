//! Моноширинный шрифт продукта — файл JetBrains Mono из `Contents/Resources/Fonts/` бандла
//! (ADR-0022, ADR-0024). Шрифт строится из дескриптора файла, а не по имени: поиск по имени
//! отдаёт одноимённый шрифт, установленный в системе, а не вложенный.

use std::cell::OnceCell;
use std::path::{Path, PathBuf};

use objc2::rc::Retained;
use objc2_app_kit::{NSFont, NSFontDescriptor, NSFontWeightRegular};
use objc2_core_foundation::{CFRetained, CFURL};
use objc2_core_text::CTFontManagerCreateFontDescriptorsFromURL;
use objc2_foundation::{NSArray, NSBundle, NSString, NSURL};
use shellio_theme::{MONO_FAMILIES, MONO_FONT_FILES};

/// Каталог шрифтов в ресурсах бандла, он же значение `ATSApplicationFontsPath`.
const BUNDLE_FONTS: &str = "Fonts";
const REGULAR: u16 = 400;

thread_local! {
    static REGULAR_FACE: OnceCell<Option<Retained<NSFontDescriptor>>> = const { OnceCell::new() };
}

/// Моноширинный шрифт обычного начертания. Вне бандла (тесты, владелец тестов) файла нет, и
/// шрифт берётся по стеку прототипа; бандл без файла отвергает `check-bundle.sh`.
pub(crate) fn mono(size: f64) -> Retained<NSFont> {
    REGULAR_FACE
        .with(|face| {
            face.get_or_init(|| bundle_fonts().and_then(|fonts| face_in(&fonts, REGULAR)))
                .clone()
        })
        .and_then(|face| NSFont::fontWithDescriptor_size(&face, size))
        .unwrap_or_else(|| installed(size))
}

/// Моноширинный шрифт начертания `weight` по шкале CSS из файлов бандла. Вне бандла файла нет, и
/// шрифт берётся по стеку прототипа обычным начертанием.
pub(crate) fn mono_weight(size: f64, weight: u16) -> Retained<NSFont> {
    if weight == REGULAR {
        return mono(size);
    }
    bundle_fonts()
        .and_then(|fonts| face_in(&fonts, weight))
        .and_then(|face| NSFont::fontWithDescriptor_size(&face, size))
        .unwrap_or_else(|| installed(size))
}

fn bundle_fonts() -> Option<PathBuf> {
    let resources = NSBundle::mainBundle().resourcePath()?;
    Some(PathBuf::from(resources.to_string()).join(BUNDLE_FONTS))
}

/// Дескриптор начертания из файла каталога. Он привязан к файлу и через поиск шрифтов системы
/// не проходит.
fn face_in(fonts: &Path, weight: u16) -> Option<Retained<NSFontDescriptor>> {
    let (_, file) = MONO_FONT_FILES.iter().find(|(w, _)| *w == weight)?;
    let path = fonts.join(file);
    let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str()?));
    // SAFETY: `NSURL` и `CFURL` — один объект (toll-free bridging), ссылка живёт не дольше `url`.
    let url: &CFURL = unsafe { &*Retained::as_ptr(&url).cast::<CFURL>() };
    // SAFETY: аргумент — живой URL файла; на файл без шрифта функция отвечает `NULL`.
    let faces = unsafe { CTFontManagerCreateFontDescriptorsFromURL(url) }?;
    // SAFETY: функция отдаёт `CFArray` из `CTFontDescriptor`, а они с `NSArray` и
    // `NSFontDescriptor` — одни объекты (toll-free bridging); ссылка живёт не дольше `faces`.
    let faces: &NSArray<NSFontDescriptor> = unsafe {
        &*CFRetained::as_ptr(&faces)
            .as_ptr()
            .cast::<NSArray<NSFontDescriptor>>()
    };
    faces.firstObject()
}

/// Первый установленный шрифт стека прототипа, иначе системный моноширинный.
fn installed(size: f64) -> Retained<NSFont> {
    MONO_FAMILIES
        .iter()
        .find_map(|name| NSFont::fontWithName_size(&NSString::from_str(name), size))
        .unwrap_or_else(|| {
            // SAFETY: вес шрифта — неизменяемая константа AppKit.
            NSFont::monospacedSystemFontOfSize_weight(size, unsafe { NSFontWeightRegular })
        })
}

#[cfg(test)]
mod tests {
    use objc2_core_text::kCTFontURLAttribute;

    use super::*;

    fn prototype_fonts() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../prototype/fonts")
            .canonicalize()
            .unwrap()
    }

    /// Файл, из которого взят шрифт (`kCTFontURLAttribute`).
    fn file_of(font: &NSFont) -> Option<PathBuf> {
        // SAFETY: ключ — неизменяемая константа CoreText; `CFString` и `NSString` — один объект.
        let key: &NSString =
            unsafe { &*std::ptr::from_ref(kCTFontURLAttribute).cast::<NSString>() };
        let url = font.fontDescriptor().objectForKey(key)?;
        let url = url.downcast::<NSURL>().ok()?;
        Some(PathBuf::from(url.path()?.to_string()))
    }

    #[test]
    fn face_is_drawn_from_the_given_file_not_from_an_installed_font() {
        let fonts = prototype_fonts();
        for (weight, file) in MONO_FONT_FILES {
            let face = face_in(&fonts, weight).unwrap();
            let font = NSFont::fontWithDescriptor_size(&face, 10.5).unwrap();
            assert_eq!(file_of(&font), Some(fonts.join(file)));
            assert_eq!(font.familyName().unwrap().to_string(), MONO_FAMILIES[0]);
            assert!((font.pointSize() - 10.5).abs() < f64::EPSILON);
        }
    }

    #[test]
    fn directory_without_the_file_gives_no_face() {
        assert!(face_in(Path::new("/nonexistent/Fonts"), REGULAR).is_none());
        assert!(face_in(&prototype_fonts(), 300).is_none());
        let licence_only = prototype_fonts().join("OFL.txt");
        assert!(face_in(&licence_only, REGULAR).is_none());
    }

    #[test]
    fn process_outside_a_bundle_falls_back_to_the_stack() {
        assert!(bundle_fonts().is_some_and(|fonts| !fonts.exists()));
        assert!((mono(10.5).pointSize() - 10.5).abs() < f64::EPSILON);
    }
}
