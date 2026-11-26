---
worth: yes
where: crates/shellio-keymap/src/exit_chord.rs
added: 2026-11-26
---
# Публичный API `shellio-keymap` и `KeyStroke` без вызовов вне тестов

`ExitChord::is_armed` (`exit_chord.rs:59`) вызывают только тесты этого файла.
`SequenceTree::get` и `SequenceTree::remove` (`sequence.rs`) вызывают только тесты `sequence.rs`, а сам `SequenceTree` реэкспортируется из `lib.rs:24`, хотя нужен только `keymap.rs`.
`KeyStroke::characters` (`shellio-appkit/src/key_monitor.rs:61`) не вызывается нигде.

Нужно сделать их `pub(crate)` или `#[cfg(test)]` либо удалить, `SequenceTree` из `lib.rs` не реэкспортировать.
