//! Безопасный слой shellio над AppKit (ADR-0006, ADR-0020).
//!
//! Продуктовый `unsafe` вокруг Objective-C живёт здесь, типы objc2 наружу не выходят. P1.2 даёт
//! `NSApplication` без окна: [`App::run`] крутит цикл главного потока с политикой `.prohibited`,
//! [`App::become_regular`] переводит владельца в `.regular`, [`App::activate`] просит передний план,
//! [`App::finish_terminate`] отпускает приложение после порядка завершения. Другие потоки попадают на
//! главный только через [`MainThread`] и не ждут его. Окно, компоненты и тема — P1.3.

#![deny(clippy::undocumented_unsafe_blocks)]

mod app;

pub use app::{App, Delegate, MainThread, RunError};
