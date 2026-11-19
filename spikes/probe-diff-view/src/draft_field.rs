//! Поле черновика: одно `NSTextView` на вид, оно переиспользуется для каждого черновика. Поле
//! перехватывает `Cmd-Enter` (сохранить) и `Escape` (отменить) и после каждой правки отдаёт текст
//! модели сцены. Текст черновика живёт в модели, поле его только показывает. Общее для вариантов
//! A и B.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send};
use objc2_app_kit::{NSColor, NSEvent, NSEventModifierFlags, NSFont, NSResponder, NSTextView};
use objc2_foundation::{NSRect, NSString};

use probe_diff_view::tokens::{Rgb, Theme};

use crate::scenario::counters::{Count, Live, counter};
use crate::scene::Scene;

counter!(DRAFT_FIELD, "ProbeDiffDraftField");

/// Клавиши поля, которые оно не отдаёт тексту.
const KEY_RETURN: u16 = 36;
const KEY_ENTER: u16 = 76;
const KEY_ESCAPE: u16 = 53;

/// Созданные и освобождённые поля черновика процесса.
pub fn count() -> Count {
    DRAFT_FIELD.count()
}

pub struct FieldIvars {
    _live: Live,
    scene: RefCell<Weak<Scene>>,
}

fn color(rgb: Rgb) -> Retained<NSColor> {
    let [r, g, b, a] = rgb.components();
    NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a)
}

define_class!(
    /// Поле черновика.
    #[unsafe(super(NSTextView))]
    #[thread_kind = MainThreadOnly]
    #[name = "ProbeDiffDraftField"]
    #[ivars = FieldIvars]
    pub struct DraftField;

    impl DraftField {
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            if !self.intercept(event) {
                // SAFETY: обычная обработка клавиши текстом.
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
            }
        }

        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> Bool {
            if self.is_focused() && self.intercept(event) {
                return Bool::YES;
            }
            // SAFETY: обычная обработка клавиатурного эквивалента.
            unsafe { msg_send![super(self), performKeyEquivalent: event] }
        }

        #[unsafe(method(didChangeText))]
        fn did_change_text(&self) {
            // SAFETY: обычное уведомление о правке текста.
            let _: () = unsafe { msg_send![super(self), didChangeText] };
            if let Some(scene) = self.scene() {
                scene.draft_text(self.string().to_string());
            }
        }
    }
);

impl DraftField {
    /// Скрытое поле без рамки и без автозамен: моноширинный шрифт вида и цвета темы.
    pub fn new(mtm: MainThreadMarker, theme: &Theme, font: &str, size: f64) -> Retained<Self> {
        let this = DraftField::alloc(mtm).set_ivars(FieldIvars {
            _live: DRAFT_FIELD.live(),
            scene: RefCell::new(Weak::new()),
        });
        // SAFETY: назначенный инициализатор NSTextView с прямоугольником.
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO] };
        this.setRichText(false);
        this.setVerticallyResizable(false);
        this.setHorizontallyResizable(false);
        this.setAutomaticQuoteSubstitutionEnabled(false);
        this.setAutomaticDashSubstitutionEnabled(false);
        this.setAutomaticTextReplacementEnabled(false);
        this.setAutomaticSpellingCorrectionEnabled(false);
        this.setContinuousSpellCheckingEnabled(false);
        this.setFont(NSFont::fontWithName_size(&NSString::from_str(font), size).as_deref());
        this.setDrawsBackground(true);
        this.setBackgroundColor(&color(theme.thread_background));
        this.setTextColor(Some(&color(theme.text)));
        this.setInsertionPointColor(Some(&color(theme.text)));
        this.setHidden(true);
        this
    }

    pub fn bind(&self, scene: &Rc<Scene>) {
        *self.ivars().scene.borrow_mut() = Rc::downgrade(scene);
    }

    pub fn unbind(&self) {
        *self.ivars().scene.borrow_mut() = Weak::new();
    }

    fn scene(&self) -> Option<Rc<Scene>> {
        self.ivars().scene.borrow().upgrade()
    }

    pub fn is_focused(&self) -> bool {
        let responder: &NSResponder = self;
        self.window()
            .and_then(|window| window.firstResponder())
            .is_some_and(|first| std::ptr::eq(&*first, responder))
    }

    /// `Cmd-Enter` сохраняет черновик, `Escape` отменяет. Остальное идёт в текст.
    fn intercept(&self, event: &NSEvent) -> bool {
        let Some(scene) = self.scene() else {
            return false;
        };
        let flags = event.modifierFlags();
        match event.keyCode() {
            KEY_RETURN | KEY_ENTER if flags.contains(NSEventModifierFlags::Command) => {
                scene.save_draft();
                true
            }
            KEY_ESCAPE => {
                scene.cancel_draft();
                true
            }
            _ => false,
        }
    }
}
