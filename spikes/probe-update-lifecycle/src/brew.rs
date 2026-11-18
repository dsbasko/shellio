//! Клетка G (задача 15): разборы для сценария `brew` без вызова внешних программ — вывод
//! `xattr -l` и `xattr -lr`, вывод `brew`, записи `syspolicyd` о Gatekeeper, кнопка закрытия окна
//! Gatekeeper, сверка снимков настоящего Homebrew и шаблоны cask локального tap.

use serde::Serialize;

use crate::window::AxButton;

pub const QUARANTINE: &str = "com.apple.quarantine";
/// Локальный tap изолированного префикса и его каталог.
pub const TAP: &str = "shellio-probe/probe";
pub const TAP_DIR: &str = "Library/Taps/shellio-probe/homebrew-probe";
pub const CASK: &str = "probe-update";
pub const CONTROL_CASK: &str = "probe-update-control";
pub const CASK_TEMPLATE: &str = include_str!("../brew/Casks/probe-update.rb");
pub const CONTROL_TEMPLATE: &str = include_str!("../brew/Casks/probe-update-control.rb");
/// Кнопки действия окна Gatekeeper на macOS 27: `AXIdentifier` вида `action-button--992`.
pub const ACTION_BUTTON: &str = "action-button";
const GATEKEEPER_TERMINATED: &str = "Terminating process due to Gatekeeper rejection: ";

/// Расширенный атрибут из вывода `xattr -l`. `path` есть у строк вывода нескольких файлов
/// (`xattr -lr`), у вывода одного файла его нет.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Xattr {
    pub path: Option<String>,
    pub name: String,
    pub value: String,
}

fn is_attr_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '#'))
}

/// Имя атрибута в начале `rest`: до следующего `": "` или до `:` в конце строки.
fn leading_name(rest: &str) -> Option<(&str, &str)> {
    if let Some(at) = rest.find(": ") {
        let name = &rest[..at];
        return is_attr_name(name).then_some((name, &rest[at + 2..]));
    }
    rest.strip_suffix(':')
        .filter(|name| is_attr_name(name))
        .map(|name| (name, ""))
}

/// Разбор `xattr -l`: строки `<имя>: <значение>` одного файла и `<путь>: <имя>: <значение>`
/// нескольких. `xattr` печатает двоичное значение как C-строку до первого NUL, поэтому строки, не
/// подходящие ни под одну форму, — продолжения значения, они пропускаются.
pub fn parse_xattr(text: &str) -> Vec<Xattr> {
    let mut entries = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut found = None;
        let mut from = 0;
        while let Some(offset) = line[from..].find(": ") {
            let at = from + offset;
            let (path, rest) = (&line[..at], &line[at + 2..]);
            if !path.is_empty()
                && path.contains('/')
                && let Some((name, value)) = leading_name(rest)
            {
                found = Some(Xattr {
                    path: Some(path.to_owned()),
                    name: name.to_owned(),
                    value: value.to_owned(),
                });
                break;
            }
            from = at + 2;
        }
        if found.is_none()
            && let Some((name, value)) = leading_name(line)
        {
            found = Some(Xattr {
                path: None,
                name: name.to_owned(),
                value: value.to_owned(),
            });
        }
        entries.extend(found);
    }
    entries
}

/// Путь внутри бандла `bundle` (имя каталога `.app`): пустой для корня, `None` вне бандла.
pub fn bundle_relative(path: &str, bundle: &str) -> Option<String> {
    let marker = format!("/{bundle}");
    let at = path.rfind(&marker)?;
    let rest = &path[at + marker.len()..];
    if rest.is_empty() {
        Some(String::new())
    } else {
        rest.strip_prefix('/').map(str::to_owned)
    }
}

/// Карантин в выводе `xattr -lr` бандла.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Scan {
    /// Путей с атрибутами в выводе.
    pub listed: usize,
    /// Пути внутри бандла с карантином; корень — пустая строка.
    pub quarantined: Vec<String>,
    pub root: bool,
    pub executable: bool,
    /// Значение карантина корня, иначе первое встреченное.
    pub value: Option<String>,
}

pub fn scan(entries: &[Xattr], bundle: &str, executable: &str) -> Scan {
    let mut listed: Vec<&str> = entries.iter().filter_map(|e| e.path.as_deref()).collect();
    listed.sort_unstable();
    listed.dedup();
    let mut quarantined = Vec::new();
    let mut value = None;
    for entry in entries.iter().filter(|entry| entry.name == QUARANTINE) {
        let Some(relative) = entry
            .path
            .as_deref()
            .and_then(|path| bundle_relative(path, bundle))
        else {
            continue;
        };
        if relative.is_empty() || value.is_none() {
            value = Some(entry.value.clone());
        }
        quarantined.push(relative);
    }
    quarantined.sort();
    quarantined.dedup();
    Scan {
        listed: listed.len(),
        root: quarantined.iter().any(String::is_empty),
        executable: quarantined.iter().any(|path| path == executable),
        quarantined,
        value,
    }
}

/// Версия из `brew --version`: первая строка `Homebrew <версия>`.
pub fn parse_version(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Homebrew "))
        .map(|version| version.trim().to_owned())
        .filter(|version| !version.is_empty())
}

/// Итог `brew install --cask` по коду выхода и выводу.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Install {
    pub installed: bool,
    /// Пары «бандл → путь» из строк `==> Moving App '<бандл>' to '<путь>'`.
    pub moved: Vec<(String, String)>,
    /// Строки `Error: …`.
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

fn quoted_pair(rest: &str) -> Option<(String, String)> {
    let rest = rest.strip_prefix('\'')?;
    let (name, rest) = rest.split_once("' to '")?;
    let target = rest.strip_suffix('\'')?;
    Some((name.to_owned(), target.to_owned()))
}

pub fn parse_install(code: Option<i32>, stdout: &str, stderr: &str, token: &str) -> Install {
    let mut install = Install {
        installed: false,
        moved: Vec::new(),
        errors: Vec::new(),
        warnings: Vec::new(),
    };
    let success = format!("{token} was successfully installed!");
    let mut succeeded = false;
    for line in stdout.lines().chain(stderr.lines()) {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("==> Moving App ")
            && let Some(pair) = quoted_pair(rest)
        {
            install.moved.push(pair);
        } else if let Some(error) = line.strip_prefix("Error: ") {
            install.errors.push(error.to_owned());
        } else if let Some(warning) = line.strip_prefix("Warning: ") {
            install.warnings.push(warning.to_owned());
        } else if line.ends_with(&success) {
            succeeded = true;
        }
    }
    install.installed = code == Some(0) && succeeded && install.errors.is_empty();
    install
}

/// Причина отказа `brew` для `not_run`: строки `Error:` или хвост вывода.
pub fn refusal(code: Option<i32>, install: &Install, stdout: &str, stderr: &str) -> String {
    let detail = if install.errors.is_empty() {
        let text = format!("{stdout}{stderr}");
        let lines: Vec<&str> = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        lines[lines.len().saturating_sub(3)..].join(" | ")
    } else {
        install.errors.join(" | ")
    };
    format!("brew install: код {code:?}; {detail}")
}

/// Записи `syspolicyd` о решении Gatekeeper для идентификатора бандла.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Gatekeeper {
    /// `Prompt shown …, waiting for response` с идентификатором.
    pub prompt: bool,
    /// `Adding Gatekeeper denial breadcrumb` с идентификатором.
    pub breadcrumb: bool,
    /// `LSCodeEvaluation moveItemToTrashWithReply`: ответ окна — «Переместить в Корзину».
    pub trash: bool,
    /// PID из `Terminating process due to Gatekeeper rejection: <PID>`.
    pub terminated: Vec<i32>,
    pub lines: Vec<String>,
}

pub fn gatekeeper(log: &str, identifier: &str) -> Gatekeeper {
    let id = format!("(id: {identifier})");
    let mut gk = Gatekeeper::default();
    for line in log.lines() {
        let mine = line.contains(&id);
        let mut keep = false;
        if mine && line.contains("Prompt shown") {
            gk.prompt = true;
            keep = true;
        }
        if mine && line.contains("denial breadcrumb") {
            gk.breadcrumb = true;
            keep = true;
        }
        if mine && line.contains("evaluateScanResult") {
            keep = true;
        }
        if line.contains("moveItemToTrash") {
            gk.trash = true;
            keep = true;
        }
        if let Some(at) = line.find(GATEKEEPER_TERMINATED) {
            let digits: String = line[at + GATEKEEPER_TERMINATED.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            if let Ok(pid) = digits.parse() {
                gk.terminated.push(pid);
                keep = true;
            }
        }
        if keep {
            gk.lines.push(line.to_owned());
        }
    }
    gk
}

/// Кнопка, закрывающая окно Gatekeeper без перемещения бандла в Корзину: кнопка действия, которая
/// не является кнопкой по умолчанию. На macOS 27 по умолчанию — «Переместить в Корзину», и Escape
/// выбирает её (ранняя проверка задачи 15). Ровно одна такая кнопка, иначе отказ с причиной.
pub fn dismiss_button(buttons: &[AxButton]) -> Result<&AxButton, String> {
    let actions: Vec<&AxButton> = buttons
        .iter()
        .filter(|button| {
            button
                .identifier
                .as_deref()
                .is_some_and(|id| id.starts_with(ACTION_BUTTON))
        })
        .collect();
    if !actions.iter().any(|button| button.default) {
        return Err(format!(
            "у окна нет кнопки действия по умолчанию среди {}",
            describe_buttons(buttons)
        ));
    }
    let others: Vec<&AxButton> = actions.into_iter().filter(|b| !b.default).collect();
    match others.as_slice() {
        [one] => Ok(one),
        _ => Err(format!(
            "кнопок действия не по умолчанию {}, нужна одна: {}",
            others.len(),
            describe_buttons(buttons)
        )),
    }
}

pub fn describe_buttons(buttons: &[AxButton]) -> String {
    let list: Vec<String> = buttons
        .iter()
        .map(|button| {
            format!(
                "{}«{}»{}",
                button.identifier.as_deref().unwrap_or("?"),
                button.title.as_deref().unwrap_or(""),
                if button.default {
                    " (по умолчанию)"
                } else {
                    ""
                }
            )
        })
        .collect();
    format!("[{}]", list.join(", "))
}

/// Настоящий Homebrew в один момент клетки.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RealBrew {
    pub when: &'static str,
    pub head: Option<String>,
    /// `git --no-optional-locks status --porcelain` построчно.
    pub status: Vec<String>,
    /// `Library/Taps/<user>/<repo>`.
    pub taps: Vec<String>,
    /// `Caskroom/<token>/<запись>`.
    pub caskroom: Vec<String>,
    /// SHA-256 `~/.homebrew/trust.json`, если файл есть.
    pub trust_sha256: Option<String>,
}

/// Расхождения снимков с первым; пусто — настоящий Homebrew прежний.
pub fn real_changes(snapshots: &[RealBrew]) -> Vec<String> {
    let Some(first) = snapshots.first() else {
        return vec!["нет снимков настоящего Homebrew".to_owned()];
    };
    let mut changes = Vec::new();
    for snapshot in &snapshots[1..] {
        let mut differs = |what: &str, same: bool| {
            if !same {
                changes.push(format!("{} ({}): {what}", snapshot.when, first.when));
            }
        };
        differs("HEAD", snapshot.head == first.head);
        differs("git status", snapshot.status == first.status);
        differs("список tap", snapshot.taps == first.taps);
        differs("Caskroom", snapshot.caskroom == first.caskroom);
        differs("trust.json", snapshot.trust_sha256 == first.trust_sha256);
    }
    changes
}

/// Cask из шаблона: SHA-256 архива и `file://`-адрес. Ни одной метки `@…@` не остаётся.
pub fn fill_cask(template: &str, sha256: &str, url: &str) -> Result<String, String> {
    if sha256.len() != 64 || !sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "SHA-256 архива «{sha256}»: не 64 шестнадцатеричных знака"
        ));
    }
    if !url.starts_with("file:///") || url.contains(['"', '\\', '#', '\n']) {
        return Err(format!(
            "адрес архива «{url}»: нужен file:/// без кавычек и #"
        ));
    }
    let sha256 = sha256.to_ascii_lowercase();
    let filled = template.replace("@SHA256@", &sha256).replace("@URL@", url);
    if filled.contains("@SHA256@") || filled.contains("@URL@") || !filled.contains(&sha256) {
        return Err("шаблон cask без меток @SHA256@ и @URL@".to_owned());
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAGED: &str = include_str!("testdata/brew/xattr-l-staged.txt");
    const REMOVED: &str = include_str!("testdata/brew/xattr-l-removed.txt");
    const QUARANTINED: &str = include_str!("testdata/brew/xattr-lr-quarantined.txt");
    const CLEAN: &str = include_str!("testdata/brew/xattr-lr-clean.txt");
    const VERSION: &str = include_str!("testdata/brew/brew-version.txt");
    const INSTALL_OUT: &str = include_str!("testdata/brew/brew-install.stdout.txt");
    const INSTALL_ERR: &str = include_str!("testdata/brew/brew-install.stderr.txt");
    const BADSUM_OUT: &str = include_str!("testdata/brew/brew-install-badsum.stdout.txt");
    const BADSUM_ERR: &str = include_str!("testdata/brew/brew-install-badsum.stderr.txt");
    const ESCAPE_LOG: &str = include_str!("testdata/brew/syspolicyd-escape.txt");
    const DONE_LOG: &str = include_str!("testdata/brew/syspolicyd-done.txt");
    const ID: &str = "io.github.dsbasko.shellio.probe.update-lifecycle";
    const EXE: &str = "Contents/MacOS/probe-update-app";

    #[test]
    fn single_file_listing_has_no_path() {
        let entries = parse_xattr(STAGED);
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(entries.iter().all(|entry| entry.path.is_none()));
        assert_eq!(entries[0].name, "com.apple.provenance");
        assert_eq!(entries[1].name, QUARANTINE);
        assert_eq!(
            entries[1].value,
            "0181;6afd9982;;BCBE4EE0-E440-4629-AC05-63EC9AE77627"
        );
        let removed = parse_xattr(REMOVED);
        assert_eq!(removed.len(), 1);
        assert!(removed.iter().all(|entry| entry.name != QUARANTINE));
    }

    #[test]
    fn recursive_listing_finds_quarantine_on_every_file() {
        let entries = parse_xattr(QUARANTINED);
        assert!(entries.iter().all(|entry| entry.path.is_some()));
        let scan = scan(&entries, "probe-update.app", EXE);
        assert!(scan.root, "{scan:?}");
        assert!(scan.executable);
        assert_eq!(scan.quarantined.len(), scan.listed, "{scan:?}");
        assert!(
            scan.quarantined
                .contains(&"Contents/_CodeSignature".to_owned())
        );
        assert_eq!(
            scan.value.as_deref(),
            Some("0181;6afd9982;;BCBE4EE0-E440-4629-AC05-63EC9AE77627")
        );
        let clean = scan_text(CLEAN);
        assert!(clean.quarantined.is_empty(), "{clean:?}");
        assert!(!clean.root && !clean.executable);
        assert!(clean.listed > 5);
    }

    fn scan_text(text: &str) -> Scan {
        scan(&parse_xattr(text), "probe-update.app", EXE)
    }

    #[test]
    fn odd_lines_and_names_are_handled() {
        let text = "/a: b/x.app: com.apple.quarantine: 0083;1;;\n\
                    /t/x.app/Contents: com.apple.metadata:kMDItemWhereFroms: bplist\n\
                    \u{1}\u{2} continuation of a binary value\n\
                    /t/x.app/Contents/MacOS: com.apple.provenance:\n\
                    \n";
        let entries = parse_xattr(text);
        assert_eq!(entries.len(), 3, "{entries:?}");
        assert_eq!(entries[0].path.as_deref(), Some("/a: b/x.app"));
        assert_eq!(entries[0].value, "0083;1;;");
        assert_eq!(entries[1].name, "com.apple.metadata:kMDItemWhereFroms");
        assert_eq!(entries[2].name, "com.apple.provenance");
        assert_eq!(entries[2].value, "");
        let scan = scan(&entries, "x.app", EXE);
        assert!(scan.root);
        assert_eq!(scan.quarantined, [""]);
        assert_eq!(
            bundle_relative("/t/x.app/Contents/MacOS/a", "x.app").as_deref(),
            Some("Contents/MacOS/a")
        );
        assert_eq!(bundle_relative("/t/x.application", "x.app"), None);
        assert_eq!(bundle_relative("/t/y.app", "x.app"), None);
    }

    #[test]
    fn brew_version_and_install_outputs() {
        assert_eq!(
            parse_version(VERSION).as_deref(),
            Some("7.0.14-26-gb63354c")
        );
        assert_eq!(parse_version("brew: command not found\n"), None);
        let ok = parse_install(Some(0), INSTALL_OUT, INSTALL_ERR, "probe-update-fresh");
        assert!(ok.installed, "{ok:?}");
        assert_eq!(
            ok.moved,
            [(
                "probe-update.app".to_owned(),
                "/tmp/brew/apps4/probe-update.app".to_owned()
            )]
        );
        assert!(ok.errors.is_empty());
        assert!(ok.warnings[0].contains("prefix is not /opt/homebrew"));
        // Другой cask в той же строке успеха не засчитывается.
        assert!(!parse_install(Some(0), INSTALL_OUT, INSTALL_ERR, "probe-update").installed);
        assert!(!parse_install(Some(1), INSTALL_OUT, INSTALL_ERR, "probe-update-fresh").installed);
        let bad = parse_install(Some(1), BADSUM_OUT, BADSUM_ERR, "probe-update-badsum");
        assert!(!bad.installed);
        assert_eq!(bad.errors.len(), 2, "{bad:?}");
        assert!(bad.errors[0].contains("reports different checksum"));
        let reason = refusal(Some(1), &bad, BADSUM_OUT, BADSUM_ERR);
        assert!(reason.starts_with("brew install: код Some(1); Cask probe-update-badsum"));
        let silent = parse_install(Some(1), "", "a\nb\nc\nd\n", "x");
        assert_eq!(
            refusal(Some(1), &silent, "", "a\nb\nc\nd\n"),
            "brew install: код Some(1); b | c | d"
        );
    }

    #[test]
    fn syspolicyd_records_of_both_answers() {
        let escape = gatekeeper(ESCAPE_LOG, ID);
        assert!(escape.prompt);
        assert!(escape.trash);
        assert_eq!(escape.terminated, [26234]);
        let done = gatekeeper(DONE_LOG, ID);
        assert!(done.prompt && done.breadcrumb);
        assert!(!done.trash);
        assert_eq!(done.terminated, [34373]);
        assert!(
            done.lines
                .iter()
                .all(|line| !line.contains("libjnidispatch"))
        );
        let other = gatekeeper(DONE_LOG, "io.github.other");
        assert!(!other.prompt && !other.breadcrumb);
        assert_eq!(other.terminated, [34373]);
    }

    fn button(id: &str, title: &str, default: bool) -> AxButton {
        AxButton {
            identifier: Some(id.to_owned()),
            title: Some(title.to_owned()),
            default,
        }
    }

    #[test]
    fn dismiss_button_is_the_other_action() {
        let dialog = [
            button("_NS:30", "", false),
            button("action-button--992", "Переместить в Корзину", true),
            button("action-button--997", "Готово", false),
        ];
        assert_eq!(
            dismiss_button(&dialog).unwrap().identifier.as_deref(),
            Some("action-button--997")
        );
        let no_default = [
            button("action-button--1", "a", false),
            button("action-button--2", "b", false),
        ];
        assert!(
            dismiss_button(&no_default)
                .unwrap_err()
                .contains("по умолчанию")
        );
        let three = [
            button("action-button--1", "a", true),
            button("action-button--2", "b", false),
            button("action-button--3", "c", false),
        ];
        assert!(
            dismiss_button(&three)
                .unwrap_err()
                .contains("кнопок действия не по умолчанию 2")
        );
        assert!(dismiss_button(&[]).is_err());
    }

    fn real(when: &'static str) -> RealBrew {
        RealBrew {
            when,
            head: Some("b63354cf39".to_owned()),
            status: Vec::new(),
            taps: vec!["dsbasko/homebrew-tap".to_owned()],
            caskroom: vec!["ghostty/1.3.1".to_owned()],
            trust_sha256: Some("58f5".to_owned()),
        }
    }

    #[test]
    fn real_homebrew_changes_are_named() {
        assert!(real_changes(&[real("до"), real("после")]).is_empty());
        let mut after = real("после");
        after.caskroom.push("probe-update/1".to_owned());
        after.trust_sha256 = Some("0000".to_owned());
        after
            .status
            .push(" M Library/Homebrew/cmd/install.rb".to_owned());
        let changes = real_changes(&[real("до"), after]);
        assert_eq!(
            changes,
            [
                "после (до): git status",
                "после (до): Caskroom",
                "после (до): trust.json"
            ]
        );
        assert_eq!(real_changes(&[]).len(), 1);
    }

    #[test]
    fn casks_are_filled_from_templates() {
        let sha = "90d222a8902b43e8a0a3e22d8c450137d24a70037587678a931ba934ef72a4b6";
        let url = "file:///tmp/psu.n/brew/zip/probe-update.zip";
        let cask = fill_cask(CASK_TEMPLATE, sha, url).unwrap();
        assert!(cask.contains(&format!("sha256 \"{sha}\"")));
        assert!(cask.contains(&format!("url \"{url}\"")));
        assert!(cask.contains("cask \"probe-update\" do"));
        assert!(cask.contains("preflight_steps do"));
        assert!(cask.contains("\"-dr\", \"com.apple.quarantine\""));
        let control = fill_cask(CONTROL_TEMPLATE, sha, url).unwrap();
        assert!(control.contains("cask \"probe-update-control\" do"));
        assert!(!control.contains("preflight_steps"));
        assert!(fill_cask(CASK_TEMPLATE, "abc", url).is_err());
        assert!(fill_cask(CASK_TEMPLATE, sha, "https://example.invalid/x.zip").is_err());
        assert!(fill_cask(CASK_TEMPLATE, sha, "file:///tmp/a\"b.zip").is_err());
        assert!(fill_cask("cask \"x\" do\nend\n", sha, url).is_err());
    }
}
