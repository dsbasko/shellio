//! Безопасный слой shellio над AppKit (ADR-0006, ADR-0020).
//!
//! Продуктовый `unsafe` вокруг Objective-C живёт здесь, типы objc2 наружу не выходят. P1.2 даёт
//! `NSApplication` без окна: [`App::run`] крутит цикл главного потока с политикой `.accessory` (без
//! Dock, но с правом стать активным), [`App::become_regular`] переводит владельца в `.regular`,
//! [`App::activate`] просит ему передний план. Второй запуск просит передний план себе
//! ([`App::request_activation`]), узнаёт об активности событием [`Delegate::did_become_active`] и
//! передаёт её владельцу по PID ([`App::hand_off_activation_to`]) с фактами [`Transfer`].
//! [`App::finish_terminate`] отпускает приложение после порядка завершения. Другие потоки попадают на
//! главный только через [`MainThread`] и не ждут его. Окно, компоненты и тема — P1.3.

#![deny(clippy::undocumented_unsafe_blocks)]

mod app;

pub use app::{App, Delegate, MainThread, RunError, Transfer};
