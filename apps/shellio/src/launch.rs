//! Аргументы запуска: режим процесса, исходящий `launch` второго запуска, разбор аргументов
//! владельцем и тексты квитанции (ADR-0006).
//!
//! В P1.2 владелец отказывает каждому аргументу: открытие папки появится в P1.7. С P1.3
//! `--gallery` выбирает режим окна процесса, который станет владельцем.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::json;
use shellio_control::{Activation, Launch, LaunchReceipt, RefuseReason, RefusedArg};
use shellio_store::{Profile, ProfileError};

/// Флаг самопроверки бандла P0.1.
pub const SELF_CHECK: &str = "--self-check";

/// Флаг режима галереи компонентов (P1.3).
pub const GALLERY: &str = "--gallery";

/// Режим окна владельца. Его задают аргументы процесса, взявшего блокировку, и до выхода
/// процесса он не меняется.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WindowMode {
    /// Обычное содержимое окна.
    #[default]
    Normal,
    /// Галерея компонентов.
    Gallery,
}

impl WindowMode {
    /// Имя режима в строке готовности и в `ui.window`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Gallery => "gallery",
        }
    }
}

/// Аргументы владельца или второго запуска: флаг режима отдельно от остальных.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Run {
    /// Аргументы без флага режима.
    pub args: Vec<OsString>,
    /// Режим окна, если процесс станет владельцем.
    pub window: WindowMode,
}

impl Run {
    /// Кадр `launch`, который второй запуск пересылает владельцу: флаг режима снова среди
    /// аргументов, чтобы владелец ответил на него отказом. Собственный `launch` владельца идёт без
    /// флага.
    pub fn forwarded(&self, own: &Launch) -> Launch {
        match self.window {
            WindowMode::Normal => own.clone(),
            WindowMode::Gallery => Launch {
                args: std::iter::once(GALLERY.to_owned())
                    .chain(own.args.iter().cloned())
                    .collect(),
                cwd: own.cwd.clone(),
            },
        }
    }
}

/// Что делает процесс по своим аргументам.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Самопроверка: до профиля, блокировки и хранилища.
    SelfCheck,
    /// Владелец или второй запуск.
    Run(Run),
    /// Ошибка использования, код 2.
    Usage(String),
}

/// Режим по аргументам без имени программы. `--self-check` допустим только один,
/// `--gallery` в любом месте и любое число раз выбирает галерею.
pub fn mode(args: Vec<OsString>) -> Mode {
    if args.iter().any(|arg| arg == SELF_CHECK) {
        return if args.len() == 1 {
            Mode::SelfCheck
        } else {
            Mode::Usage(format!("{SELF_CHECK} не сочетается с другими аргументами"))
        };
    }
    let (flags, args): (Vec<_>, Vec<_>) = args.into_iter().partition(|arg| arg == GALLERY);
    let window = if flags.is_empty() {
        WindowMode::Normal
    } else {
        WindowMode::Gallery
    };
    Mode::Run(Run { args, window })
}

/// Отказ галерее на профиле по умолчанию: текст ошибки использования, код 2. Проверка идёт после
/// вычисления профиля и до каталога состояния, блокировки и AppKit. Невычисленный профиль здесь
/// не отказ: его отвергает старт владельца прежним отказом.
pub fn gallery_refusal(
    window: WindowMode,
    profile: &Result<Profile, ProfileError>,
) -> Option<String> {
    match (window, profile) {
        (WindowMode::Gallery, Ok(profile)) if !profile.is_explicit() => Some(format!(
            "{GALLERY}: галерея запускается только на отдельном профиле, задайте SHELLIO_STATE_DIR"
        )),
        _ => None,
    }
}

/// Аргументы для владельца и отказы, поставленные локально.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    /// Кадр `launch`: флаги как есть, пути абсолютные.
    pub launch: Launch,
    /// Аргументы, которые владельцу не отправляются: `arg_not_utf8`.
    pub local: Vec<RefusedArg>,
}

/// Флаг или пустой аргумент: путём не считается.
fn is_flag(arg: &str) -> bool {
    arg.is_empty() || arg.starts_with('-')
}

/// Собрать `launch` из аргументов процесса и его рабочего каталога `cwd`.
///
/// Флаг уходит как есть, путь — абсолютным от `cwd` без разрешения ссылок и `..`. Аргумент
/// не в UTF-8 и путь, который становится не UTF-8 вместе с `cwd`, получают локальный отказ
/// `arg_not_utf8`. Рабочий каталог не в UTF-8 уходит с заменой символов: путей от него
/// владелец не получает.
pub fn outgoing(args: Vec<OsString>, cwd: &Path) -> Outgoing {
    let mut sent = Vec::new();
    let mut local = Vec::new();
    for arg in args {
        let refused = match arg.into_string() {
            Ok(arg) if is_flag(&arg) => {
                sent.push(arg);
                continue;
            }
            Ok(arg) => match absolute(cwd, &arg).into_os_string().into_string() {
                Ok(path) => {
                    sent.push(path);
                    continue;
                }
                Err(_) => arg,
            },
            Err(raw) => raw.to_string_lossy().into_owned(),
        };
        local.push(RefusedArg {
            arg: refused,
            reason: RefuseReason::ArgNotUtf8,
        });
    }
    Outgoing {
        launch: Launch {
            args: sent,
            cwd: PathBuf::from(cwd.to_string_lossy().into_owned()),
        },
        local,
    }
}

/// Собрать `launch` по результату чтения рабочего каталога. Каталог нужен только
/// относительным путям: если он не прочитан, запуск без них уходит с `cwd` `/`, а с ними —
/// отказ старта с причиной.
pub fn outgoing_from(args: Vec<OsString>, cwd: io::Result<PathBuf>) -> Result<Outgoing, String> {
    let error = match cwd {
        Ok(cwd) => return Ok(outgoing(args, &cwd)),
        Err(error) => error,
    };
    let relative = args.iter().any(|arg| {
        arg.to_str()
            .is_some_and(|arg| !is_flag(arg) && Path::new(arg).is_relative())
    });
    if relative {
        return Err(format!(
            "рабочий каталог не прочитан, относительный путь не передать: {error}"
        ));
    }
    Ok(outgoing(args, Path::new("/")))
}

/// Путь `arg` от `cwd`: `.` и повторные разделители убраны, `..` и ссылки остаются.
fn absolute(cwd: &Path, arg: &str) -> PathBuf {
    let joined = cwd.join(arg);
    // От абсолютного пути `absolute` текущий каталог не читает и не отказывает.
    std::path::absolute(&joined).unwrap_or(joined)
}

/// Отказ владельца каждому аргументу `launch`. Читает атрибуты файлов, поэтому идёт вне
/// главного потока и рабочих потоков runtime.
pub fn judge(launch: &Launch) -> Vec<RefusedArg> {
    launch
        .args
        .iter()
        .map(|arg| RefusedArg {
            arg: arg.clone(),
            reason: reason(&launch.cwd, arg),
        })
        .collect()
}

fn reason(cwd: &Path, arg: &str) -> RefuseReason {
    // Режим окна выбирают только аргументы процесса, взявшего блокировку.
    if arg == GALLERY {
        return RefuseReason::GalleryNotAtStart;
    }
    if is_flag(arg) {
        return RefuseReason::UnknownArgument;
    }
    // Ссылка разрешается: ссылка на папку — папка.
    match fs::metadata(cwd.join(arg)) {
        Ok(metadata) if metadata.is_dir() => RefuseReason::FolderOpenUnsupported,
        Ok(_) => RefuseReason::NotADirectory,
        // Недоступный путь, как и отсутствующий, не открыть.
        Err(_) => RefuseReason::PathNotFound,
    }
}

/// Строка отказанного аргумента: код причины и текст для человека.
pub fn refusal_line(refused: &RefusedArg) -> String {
    format!(
        "shellio: {}: {} — {}",
        refused.arg,
        refused.reason,
        refused.reason.message()
    )
}

/// Квитанция второго запуска: владелец, исход активации и каждый отказанный аргумент, сначала
/// отказы владельца, затем локальные.
pub fn receipt_text(pid: u32, receipt: &LaunchReceipt, local: &[RefusedArg]) -> String {
    let activation = match receipt.activation {
        Activation::Requested => "активация запрошена",
        Activation::NoUi => "активации нет: владелец без UI",
    };
    let mut text = format!("shellio: запуск передан владельцу (pid {pid}), {activation}");
    for refused in receipt.refused.iter().chain(local) {
        text.push('\n');
        text.push_str(&refusal_line(refused));
    }
    text
}

/// Строка журнала владельца об обработанном запуске: кадр `launch` и квитанция в JSON.
pub fn handled_line(launch: &Launch, receipt: &LaunchReceipt) -> String {
    format!(
        "shellio: запуск обработан {}",
        json!({ "launch": launch, "receipt": receipt })
    )
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn run(args: &[&str], window: WindowMode) -> Mode {
        Mode::Run(Run {
            args: os(args),
            window,
        })
    }

    #[test]
    fn self_check_only_alone() {
        assert_eq!(mode(os(&["--self-check"])), Mode::SelfCheck);
        assert_eq!(mode(Vec::new()), run(&[], WindowMode::Normal));
        assert_eq!(
            mode(os(&["/a", "-x"])),
            run(&["/a", "-x"], WindowMode::Normal)
        );
        for args in [
            &["--self-check", "/a"][..],
            &["/a", "--self-check"],
            &["--self-check", "--gallery"],
            &["--gallery", "--self-check"],
        ] {
            assert!(matches!(mode(os(args)), Mode::Usage(text) if text.contains(SELF_CHECK)));
        }
    }

    #[test]
    fn gallery_flag_is_taken_apart_from_arguments() {
        assert_eq!(mode(os(&["--gallery"])), run(&[], WindowMode::Gallery));
        assert_eq!(
            mode(os(&["/a", "--gallery", "-x", "--gallery"])),
            run(&["/a", "-x"], WindowMode::Gallery)
        );
        assert_eq!(
            mode(os(&["--gallery=1", "--galleryx"])),
            run(&["--gallery=1", "--galleryx"], WindowMode::Normal),
            "только точное имя флага"
        );
        assert_eq!(WindowMode::Normal.as_str(), "normal");
        assert_eq!(WindowMode::Gallery.as_str(), "gallery");
    }

    #[test]
    fn second_launch_forwards_gallery_flag_and_owner_launch_goes_without_it() {
        let own = Launch {
            args: vec!["/w/a".to_owned(), "-x".to_owned()],
            cwd: PathBuf::from("/w"),
        };
        let gallery = Run {
            args: os(&["a", "-x"]),
            window: WindowMode::Gallery,
        };
        assert_eq!(
            gallery.forwarded(&own),
            Launch {
                args: vec!["--gallery".to_owned(), "/w/a".to_owned(), "-x".to_owned()],
                cwd: PathBuf::from("/w"),
            }
        );
        let normal = Run {
            args: os(&["a", "-x"]),
            window: WindowMode::Normal,
        };
        assert_eq!(normal.forwarded(&own), own);
        assert_eq!(
            outgoing(gallery.args.clone(), Path::new("/w")).launch,
            own,
            "собственный launch владельца без флага"
        );
    }

    #[test]
    fn gallery_needs_explicit_profile() {
        let explicit = Ok(Profile::isolated("/tmp/shellio-state"));
        let default =
            Profile::resolve(|name| (name == "HOME").then(|| OsString::from("/Users/test")));
        assert!(default.as_ref().is_ok_and(|profile| !profile.is_explicit()));
        assert_eq!(gallery_refusal(WindowMode::Gallery, &explicit), None);
        assert_eq!(gallery_refusal(WindowMode::Normal, &default), None);
        let refusal = gallery_refusal(WindowMode::Gallery, &default).unwrap();
        assert!(refusal.contains("отдельном профиле"), "{refusal}");
        assert!(refusal.contains("SHELLIO_STATE_DIR"), "{refusal}");
        let unresolved = Err(ProfileError::HomeMissing);
        assert_eq!(
            gallery_refusal(WindowMode::Gallery, &unresolved),
            None,
            "невычисленный профиль отвергает старт владельца"
        );
    }

    #[test]
    fn paths_become_absolute_flags_stay() {
        let out = outgoing(
            os(&["dir", "./a//b/", "/abs", "../up", "-x", "--flag=1", ""]),
            Path::new("/work"),
        );
        assert_eq!(
            out.launch,
            Launch {
                args: vec![
                    "/work/dir".to_owned(),
                    "/work/a/b/".to_owned(),
                    "/abs".to_owned(),
                    "/work/../up".to_owned(),
                    "-x".to_owned(),
                    "--flag=1".to_owned(),
                    String::new(),
                ],
                cwd: PathBuf::from("/work"),
            }
        );
        assert!(out.local.is_empty());
    }

    #[test]
    fn non_utf8_is_refused_locally_and_not_sent() {
        let bad = OsString::from_vec(b"x\xff".to_vec());
        let out = outgoing(vec![bad, OsString::from("ok")], Path::new("/w"));
        assert_eq!(out.launch.args, vec!["/w/ok".to_owned()]);
        assert_eq!(
            out.local,
            vec![RefusedArg {
                arg: "x\u{fffd}".to_owned(),
                reason: RefuseReason::ArgNotUtf8,
            }]
        );

        let cwd = PathBuf::from(OsString::from_vec(b"/w\xff".to_vec()));
        let out = outgoing(os(&["rel", "/abs", "-f"]), &cwd);
        assert_eq!(out.launch.args, vec!["/abs".to_owned(), "-f".to_owned()]);
        assert_eq!(out.launch.cwd, PathBuf::from("/w\u{fffd}"));
        assert_eq!(
            out.local,
            vec![RefusedArg {
                arg: "rel".to_owned(),
                reason: RefuseReason::ArgNotUtf8,
            }]
        );
    }

    #[test]
    fn unreadable_cwd_blocks_only_relative_paths() {
        let gone = || Err(io::Error::from(io::ErrorKind::NotFound));
        let bad = OsString::from_vec(b"x\xff".to_vec());
        let out = outgoing_from(
            vec![OsString::from("/abs"), OsString::from("-x"), bad],
            gone(),
        )
        .unwrap();
        assert_eq!(out.launch.args, vec!["/abs".to_owned(), "-x".to_owned()]);
        assert_eq!(out.launch.cwd, PathBuf::from("/"));
        assert_eq!(out.local.len(), 1);
        assert_eq!(
            outgoing_from(Vec::new(), gone()).unwrap().launch.cwd,
            PathBuf::from("/")
        );

        let refused = outgoing_from(os(&["/abs", "rel"]), gone()).unwrap_err();
        assert!(
            refused.starts_with("рабочий каталог не прочитан"),
            "{refused}"
        );

        let read = outgoing_from(os(&["rel"]), Ok(PathBuf::from("/w"))).unwrap();
        assert_eq!(read, outgoing(os(&["rel"]), Path::new("/w")));
    }

    #[test]
    fn owner_refuses_every_argument_by_kind() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("folder");
        let file = dir.path().join("file");
        fs::create_dir(&folder).unwrap();
        fs::write(&file, b"").unwrap();
        std::os::unix::fs::symlink(&folder, dir.path().join("link")).unwrap();
        let launch = Launch {
            args: vec![
                folder.display().to_string(),
                file.display().to_string(),
                dir.path().join("missing").display().to_string(),
                "-x".to_owned(),
                String::new(),
                "link".to_owned(),
            ],
            cwd: dir.path().to_path_buf(),
        };
        let reasons: Vec<_> = judge(&launch).into_iter().map(|r| r.reason).collect();
        assert_eq!(
            reasons,
            [
                RefuseReason::FolderOpenUnsupported,
                RefuseReason::NotADirectory,
                RefuseReason::PathNotFound,
                RefuseReason::UnknownArgument,
                RefuseReason::UnknownArgument,
                RefuseReason::FolderOpenUnsupported,
            ]
        );
    }

    #[test]
    fn gallery_from_socket_is_refused_not_unknown() {
        let launch = Launch {
            args: vec!["--gallery".to_owned(), "--gallery=1".to_owned()],
            cwd: PathBuf::from("/"),
        };
        let refused = judge(&launch);
        assert_eq!(refused[0].reason, RefuseReason::GalleryNotAtStart);
        assert_eq!(refused[1].reason, RefuseReason::UnknownArgument);
        assert_eq!(
            refusal_line(&refused[0]),
            "shellio: --gallery: gallery_not_at_start — галерея открывается только при запуске \
             владельца"
        );
    }

    #[test]
    fn receipt_names_owner_and_every_refusal() {
        let receipt = LaunchReceipt {
            activation: Activation::Requested,
            refused: vec![RefusedArg {
                arg: "/p".to_owned(),
                reason: RefuseReason::FolderOpenUnsupported,
            }],
        };
        let local = [RefusedArg {
            arg: "x\u{fffd}".to_owned(),
            reason: RefuseReason::ArgNotUtf8,
        }];
        assert_eq!(
            receipt_text(7, &receipt, &local),
            "shellio: запуск передан владельцу (pid 7), активация запрошена\n\
             shellio: /p: folder_open_unsupported — папка не открыта: открытие папок пока не поддерживается\n\
             shellio: x\u{fffd}: arg_not_utf8 — не передан: аргумент не в UTF-8"
        );
        let bare = LaunchReceipt {
            activation: Activation::NoUi,
            refused: Vec::new(),
        };
        assert_eq!(
            receipt_text(8, &bare, &[]),
            "shellio: запуск передан владельцу (pid 8), активации нет: владелец без UI"
        );
    }

    #[test]
    fn handled_line_carries_launch_and_receipt_json() {
        let launch = Launch {
            args: vec!["/p".to_owned()],
            cwd: PathBuf::from("/w"),
        };
        let receipt = LaunchReceipt {
            activation: Activation::NoUi,
            refused: Vec::new(),
        };
        let line = handled_line(&launch, &receipt);
        let body = line.strip_prefix("shellio: запуск обработан ").unwrap();
        let value: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            value,
            json!({
                "launch": {"args": ["/p"], "cwd": "/w"},
                "receipt": {"activation": "no_ui", "refused": []}
            })
        );
    }
}
