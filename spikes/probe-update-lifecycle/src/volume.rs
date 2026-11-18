//! Тома APFS из образа для B3 (задача 13; план P0.5, «B3. Ошибки среды»): `hdiutil create -fs
//! APFS`, `attach` в свою точку монтирования, `detach` в любом исходе, заполнение тома физической
//! записью и errno отказа по его тексту.
//!
//! Разбор вывода `hdiutil`, классификация errno и системного вызова — чистые функции с тестами в
//! `make check`. Команды `hdiutil` вызывают только сценарий `env-faults` и внешний запуск
//! ([`sweep`]): он отключает тома, оставшиеся под каталогом состояния, если процесс сценария не
//! дошёл до своей уборки. Заполнение отказывает на томе больше [`MAX_FILL_BYTES`]: так оно не
//! доберётся до настоящего диска.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::generation::{self, Run};
use crate::sys;

pub const HDIUTIL: &str = "/usr/bin/hdiutil";
/// Начало имени образа стенда.
pub const IMAGE_PREFIX: &str = "probe-update-b3-";
/// Наибольший том, который [`fill`] согласен заполнять.
pub const MAX_FILL_BYTES: u64 = 512 << 20;
const CHUNK: usize = 1 << 20;
const BLOCK: usize = 4096;

/// Строка устройства в выводе `hdiutil attach` и `hdiutil info`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Device {
    pub dev: String,
    pub hint: String,
    pub mount: Option<PathBuf>,
}

/// `/dev/diskN<TAB>подсказка<TAB>точка монтирования`; поля дополнены пробелами.
fn device_line(line: &str) -> Option<Device> {
    if !line.starts_with("/dev/") {
        return None;
    }
    let mut fields = line.split('\t').map(str::trim);
    let dev = fields.next()?.to_owned();
    let hint = fields.next().unwrap_or("").to_owned();
    let mount = fields
        .next()
        .filter(|mount| !mount.is_empty())
        .map(PathBuf::from);
    Some(Device { dev, hint, mount })
}

/// Подключённый образ: устройство образа (первое в выводе) и единственная точка монтирования.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Attachment {
    pub whole: String,
    pub mount: PathBuf,
    pub devices: Vec<Device>,
}

/// Вывод `hdiutil attach` (stdout; предупреждения об устаревании идут в stderr).
pub fn parse_attach(stdout: &str) -> Result<Attachment, String> {
    let devices: Vec<Device> = stdout.lines().filter_map(device_line).collect();
    let whole = devices
        .first()
        .map(|device| device.dev.clone())
        .ok_or("в выводе hdiutil attach нет устройств")?;
    let mounts: Vec<PathBuf> = devices
        .iter()
        .filter_map(|device| device.mount.clone())
        .collect();
    match mounts.as_slice() {
        [mount] => Ok(Attachment {
            whole,
            mount: mount.clone(),
            devices,
        }),
        [] => Err("hdiutil attach: том не смонтирован".to_owned()),
        many => Err(format!(
            "hdiutil attach: несколько точек монтирования {many:?}"
        )),
    }
}

/// Путь из строки `created: <путь>` вывода `hdiutil create`.
pub fn parse_created(stdout: &str) -> Option<PathBuf> {
    stdout.lines().find_map(|line| {
        line.trim()
            .strip_prefix("created:")
            .map(|path| PathBuf::from(path.trim()))
    })
}

/// Образ из вывода `hdiutil info`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InfoImage {
    pub image: PathBuf,
    pub devices: Vec<Device>,
}

/// Вывод `hdiutil info`: блок на образ, начинается строкой `image-path : <путь>`.
pub fn parse_info(stdout: &str) -> Vec<InfoImage> {
    let mut images: Vec<InfoImage> = Vec::new();
    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("image-path") {
            if let Some((_, path)) = rest.split_once(':') {
                images.push(InfoImage {
                    image: PathBuf::from(path.trim()),
                    devices: Vec::new(),
                });
            }
            continue;
        }
        if let (Some(image), Some(device)) = (images.last_mut(), device_line(line)) {
            image.devices.push(device);
        }
    }
    images
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn hdiutil(args: &[String]) -> Result<Run, String> {
    let run = generation::run(HDIUTIL, args)?;
    if run.code == Some(0) {
        Ok(run)
    } else {
        Err(format!(
            "hdiutil {}: код {:?}: {}",
            args.join(" "),
            run.code,
            run.text().trim()
        ))
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// Образ `path` с томом APFS размером `size_mib` МиБ без таблицы разделов.
pub fn create_image(path: &Path, size_mib: u64, volname: &str) -> Result<Run, String> {
    let size = format!("{size_mib}m");
    let path_arg = path.display().to_string();
    let run = hdiutil(&strings(&[
        "create", "-size", &size, "-fs", "APFS", "-volname", volname, "-layout", "NONE", &path_arg,
    ]))?;
    match parse_created(&run.stdout) {
        Some(created) if canonical(&created) == canonical(path) => Ok(run),
        other => Err(format!(
            "hdiutil create: создан {other:?}, ждали {}",
            path.display()
        )),
    }
}

fn detach_device(dev: &str, force: bool) -> Result<Run, String> {
    let mut args = vec!["detach".to_owned()];
    if force {
        args.push("-force".to_owned());
    }
    args.push(dev.to_owned());
    hdiutil(&args)
}

/// Подключённый образ; при выходе из области отключается, если сценарий не отключил его сам.
#[derive(Debug)]
pub struct Volume {
    pub image: PathBuf,
    pub attachment: Attachment,
    pub readonly: bool,
    attached: bool,
}

/// Как отключён том.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Detached {
    pub whole: String,
    pub mount: PathBuf,
    /// Обычный `detach` отказал и понадобился `-force`.
    pub forced: Option<String>,
}

impl Volume {
    /// `hdiutil attach` образа `image` в существующий каталог `mountpoint`: без Finder, с учётом
    /// владельцев файлов, `readonly` — только для чтения.
    pub fn attach(image: &Path, mountpoint: &Path, readonly: bool) -> Result<Volume, String> {
        let mut args = vec!["attach".to_owned()];
        if readonly {
            args.push("-readonly".to_owned());
        }
        args.extend(strings(&[
            "-nobrowse",
            "-noautoopen",
            "-owners",
            "on",
            "-mountpoint",
        ]));
        args.push(mountpoint.display().to_string());
        args.push(image.display().to_string());
        let run = hdiutil(&args)?;
        let attachment = parse_attach(&run.stdout).inspect_err(|_| {
            if let Some(device) = run.stdout.lines().find_map(device_line) {
                let _ = detach_device(&device.dev, true);
            }
        })?;
        let volume = Volume {
            image: image.to_path_buf(),
            attachment,
            readonly,
            attached: true,
        };
        if canonical(&volume.attachment.mount) != canonical(mountpoint) {
            return Err(format!(
                "hdiutil attach: том в {}, ждали {}",
                volume.attachment.mount.display(),
                mountpoint.display()
            ));
        }
        Ok(volume)
    }

    pub fn is_attached(&self) -> bool {
        self.attached
    }

    /// Точка монтирования так, как её назвал `hdiutil` (канонический путь).
    pub fn mount(&self) -> &Path {
        &self.attachment.mount
    }

    /// `hdiutil detach`, при отказе — `detach -force`.
    pub fn detach(&mut self) -> Result<Detached, String> {
        let whole = self.attachment.whole.clone();
        let forced = match detach_device(&whole, false) {
            Ok(_) => None,
            Err(reason) => {
                detach_device(&whole, true).map_err(|forced| format!("{reason}; {forced}"))?;
                Some(reason)
            }
        };
        self.attached = false;
        Ok(Detached {
            whole,
            mount: self.attachment.mount.clone(),
            forced,
        })
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        if self.attached {
            let _ = self.detach();
        }
    }
}

/// Отключение образов, лежащих под `root`, — внешняя уборка после процесса сценария. Вложенные
/// тома — первыми, по глубине точки монтирования. Описание каждого найденного образа.
pub fn sweep(root: &Path) -> Vec<String> {
    let Ok(run) = hdiutil(&strings(&["info"])) else {
        return Vec::new();
    };
    let root = canonical(root);
    let mut found: Vec<(usize, String, PathBuf)> = parse_info(&run.stdout)
        .into_iter()
        .filter(|image| canonical(&image.image).starts_with(&root))
        .filter_map(|image| {
            let depth = image
                .devices
                .iter()
                .filter_map(|device| device.mount.as_ref())
                .map(|mount| mount.components().count())
                .max()
                .unwrap_or(0);
            image
                .devices
                .first()
                .map(|device| (depth, device.dev.clone(), image.image.clone()))
        })
        .collect();
    found.sort_by_key(|(depth, _, _)| std::cmp::Reverse(*depth));
    found
        .into_iter()
        .map(|(_, dev, image)| {
            let outcome = match detach_device(&dev, true) {
                Ok(_) => "отключён внешней уборкой".to_owned(),
                Err(reason) => format!("не отключён: {reason}"),
            };
            format!("{} ({dev}) {outcome}", image.display())
        })
        .collect()
}

// --- заполнение ---

/// Псевдослучайные байты (xorshift64): том получает настоящие блоки данных.
fn pattern(buf: &mut [u8], mut seed: u64) {
    for chunk in buf.chunks_mut(8) {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let bytes = seed.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
}

/// Итог заполнения.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Fill {
    pub path: PathBuf,
    pub bytes: u64,
    /// errno последней записи: ENOSPC, если том заполнен до отказа.
    pub errno: Option<i32>,
    /// errno `fsync` после заполнения, если он отказал.
    pub fsync_errno: Option<i32>,
}

fn guard_size(dir: &Path) -> Result<(), String> {
    let (_, total) =
        sys::volume_space(dir).map_err(|err| format!("statfs {}: {err}", dir.display()))?;
    if total > MAX_FILL_BYTES {
        return Err(format!(
            "{}: том {total} байт больше {MAX_FILL_BYTES}: заполнение только для тома стенда",
            dir.display()
        ));
    }
    Ok(())
}

/// Запись в `path` до `limit` байт (`None` — пока том не откажет): блоками по 1 МиБ, после первого
/// отказа ENOSPC — по 4 КиБ до второго. Отказ, отличный от ENOSPC, — ошибка.
pub fn fill(path: &Path, limit: Option<u64>) -> Result<Fill, String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("у {} нет каталога", path.display()))?;
    guard_size(dir)?;
    let mut file = File::create(path).map_err(|err| format!("{}: {err}", path.display()))?;
    let mut buf = vec![0u8; CHUNK];
    pattern(&mut buf, 0x5eed_5eed_5eed_5eed);
    let mut bytes = 0u64;
    let mut errno = None;
    'sizes: for size in [CHUNK, BLOCK] {
        loop {
            let want = match limit {
                Some(limit) if bytes >= limit => break 'sizes,
                Some(limit) => size.min((limit - bytes) as usize),
                None => size,
            };
            match file.write(&buf[..want]) {
                Ok(0) => break,
                Ok(written) => bytes += written as u64,
                Err(err) if err.raw_os_error() == Some(libc::ENOSPC) => {
                    errno = Some(libc::ENOSPC);
                    break;
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(err) => return Err(format!("{}: запись: {err}", path.display())),
            }
        }
        if limit.is_some() {
            break;
        }
    }
    let fsync_errno = file.sync_all().err().and_then(|err| err.raw_os_error());
    Ok(Fill {
        path: path.to_path_buf(),
        bytes,
        errno,
        fsync_errno,
    })
}

/// Проба полноты: создание файла с записью 1500 байт и `fsync`, затем каталог. `None` — удалось.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Probe {
    pub file_errno: Option<i32>,
    pub mkdir_errno: Option<i32>,
    pub available: Option<u64>,
}

pub fn probe(dir: &Path) -> Probe {
    let file = dir.join(".probe-full");
    let file_errno = File::create(&file)
        .and_then(|mut handle| {
            handle.write_all(&[0x5a; 1500])?;
            handle.sync_all()
        })
        .err()
        .and_then(|err| err.raw_os_error());
    let _ = fs::remove_file(&file);
    let sub = dir.join(".probe-full-dir");
    let mkdir_errno = fs::create_dir(&sub)
        .err()
        .and_then(|err| err.raw_os_error());
    let _ = fs::remove_dir(&sub);
    Probe {
        file_errno,
        mkdir_errno,
        available: sys::volume_space(dir).ok().map(|(available, _)| available),
    }
}

impl Probe {
    /// Том полон: ни файл, ни каталог не создаются с ENOSPC.
    pub fn full(&self) -> bool {
        self.file_errno == Some(libc::ENOSPC) && self.mkdir_errno == Some(libc::ENOSPC)
    }

    /// Том пишется: и файл, и каталог создались.
    pub fn writable(&self) -> bool {
        self.file_errno.is_none() && self.mkdir_errno.is_none()
    }
}

// --- errno и системный вызов по тексту отказа ---

const KNOWN: [(i32, &str, &str); 7] = [
    (libc::EPERM, "EPERM", "Operation not permitted"),
    (libc::EIO, "EIO", "Input/output error"),
    (libc::EACCES, "EACCES", "Permission denied"),
    (libc::EBUSY, "EBUSY", "Resource busy"),
    (libc::EXDEV, "EXDEV", "Cross-device link"),
    (libc::ENOSPC, "ENOSPC", "No space left on device"),
    (libc::EROFS, "EROFS", "Read-only file system"),
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Errno {
    pub code: i32,
    pub name: String,
}

pub fn errno_name(code: i32) -> String {
    KNOWN
        .iter()
        .find(|(known, _, _)| *known == code)
        .map_or_else(
            || format!("errno {code}"),
            |(_, name, _)| (*name).to_owned(),
        )
}

/// Первое `(os error N)` в тексте: так `io::Error` Rust печатает errno.
fn os_error(text: &str) -> Option<i32> {
    const MARK: &str = "(os error ";
    let at = text.find(MARK)? + MARK.len();
    let rest = &text[at..];
    rest[..rest.find(')')?].parse().ok()
}

/// errno отказа по тексту: `(os error N)` ошибки Rust, иначе текст `strerror` внешней команды
/// (`ditto`). Незнакомый текст — `None`.
pub fn classify(text: &str) -> Option<Errno> {
    if let Some(code) = os_error(text) {
        return Some(Errno {
            code,
            name: errno_name(code),
        });
    }
    KNOWN
        .iter()
        .filter_map(|(code, name, message)| text.find(message).map(|at| (at, *code, *name)))
        .min_by_key(|(at, _, _)| *at)
        .map(|(_, code, name)| Errno {
            code,
            name: name.to_owned(),
        })
}

/// Метки шагов, которыми код пробы предваряет ошибку, и системный вызов за каждой. Более длинная
/// метка с тем же началом стоит раньше.
const LABELS: [(&str, &str); 13] = [
    (
        "rename журнала (injected)",
        "rename(2) временного файла журнала (injected)",
    ),
    (
        "обратный обмен (injected)",
        "renamex_np(2) RENAME_SWAP обратного обмена (injected)",
    ),
    (
        "запись временного файла",
        "write(2) временного файла журнала",
    ),
    (
        "fsync временного файла",
        "fsync(2) временного файла журнала",
    ),
    ("временный файл", "open(2) O_CREAT временного файла журнала"),
    ("rename журнала", "rename(2) временного файла журнала"),
    ("fsync каталога журнала", "fsync(2) каталога журнала"),
    ("каталог журнала", "mkdir(2) каталога журнала"),
    ("renamex_np(RENAME_SWAP)", "renamex_np(2) RENAME_SWAP"),
    (
        "удаление кандидата",
        "unlinkat(2) при удалении каталога кандидата",
    ),
    (
        "обратный обмен",
        "renamex_np(2) RENAME_SWAP обратного обмена",
    ),
    (
        "копия ",
        "fs::copy (fclonefileat(2), иначе fcopyfile(3)) в каталог подготовки рантайма",
    ),
    (
        "ditto:",
        "ditto: системный вызов внутри ditto не наблюдается",
    ),
];

/// Системный вызов по самой ранней метке шага в тексте; без метки — `None`.
pub fn syscall_of(text: &str) -> Option<&'static str> {
    LABELS
        .iter()
        .enumerate()
        .filter_map(|(order, (label, syscall))| text.find(label).map(|at| (at, order, *syscall)))
        .min_by_key(|(at, order, _)| (*at, *order))
        .map(|(_, _, syscall)| syscall)
}

/// Наблюдённый отказ: текст, errno, системный вызов, пометка инъекции.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Fault {
    pub observed: String,
    pub source: String,
    pub errno: Option<Errno>,
    pub syscall: Option<String>,
    pub injected: bool,
}

impl Fault {
    pub fn of(source: &str, observed: &str) -> Fault {
        Fault {
            observed: observed.to_owned(),
            source: source.to_owned(),
            errno: classify(observed),
            syscall: syscall_of(observed).map(str::to_owned),
            injected: observed.contains("(injected)"),
        }
    }

    pub fn is(&self, code: i32) -> bool {
        self.errno.as_ref().is_some_and(|errno| errno.code == code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ATTACH: &str = include_str!("testdata/hdiutil/attach.txt");
    const ATTACH_READONLY: &str = include_str!("testdata/hdiutil/attach-readonly.txt");
    const CREATE: &str = include_str!("testdata/hdiutil/create.txt");
    const INFO: &str = include_str!("testdata/hdiutil/info.txt");

    #[test]
    fn attach_output_gives_the_image_device_and_the_only_mount() {
        let attached = parse_attach(ATTACH).unwrap();
        assert_eq!(attached.whole, "/dev/disk6");
        assert_eq!(attached.mount, PathBuf::from("/private/tmp/psu.x/v4"));
        assert_eq!(attached.devices.len(), 3);
        assert_eq!(attached.devices[0].hint, "");
        assert_eq!(attached.devices[1].mount, None);
        let nested = parse_attach(ATTACH_READONLY).unwrap();
        assert_eq!(nested.whole, "/dev/disk8");
        assert_eq!(
            nested.mount,
            PathBuf::from("/private/tmp/psu.x/v4/s/install/.shellio-update-ab12")
        );
    }

    #[test]
    fn attach_output_without_one_mount_is_refused() {
        assert!(parse_attach("").is_err());
        assert!(parse_attach("/dev/disk6          \t                \t\n").is_err());
        let two = "/dev/disk6s1\tx\t/a\n/dev/disk6s2\ty\t/b\n";
        assert!(parse_attach(two).unwrap_err().contains("несколько"));
    }

    #[test]
    fn created_path_is_read_from_create_output() {
        assert_eq!(
            parse_created(CREATE),
            Some(PathBuf::from(
                "/tmp/psu.x/f4/probe-update-b3-exdev-swap.dmg"
            ))
        );
        assert_eq!(parse_created("hdiutil: create failed\n"), None);
    }

    #[test]
    fn info_output_lists_images_with_their_devices() {
        let images = parse_info(INFO);
        assert_eq!(images.len(), 3);
        assert!(images[0].image.starts_with("/System"));
        assert_eq!(
            images[1].image,
            PathBuf::from("/tmp/psu.x/f4/probe-update-b3-exdev-swap.dmg")
        );
        assert_eq!(images[1].devices[0].dev, "/dev/disk6");
        assert_eq!(
            images[2].devices[2].mount,
            Some(PathBuf::from(
                "/private/tmp/psu.x/v4/s/install/.shellio-update-ab12"
            ))
        );
        let under: Vec<_> = images
            .iter()
            .filter(|image| image.image.starts_with("/tmp/psu.x"))
            .collect();
        assert_eq!(under.len(), 2);
    }

    #[test]
    fn errno_is_classified_from_rust_errors_and_strerror_text() {
        let journal = "журнал: временный файл: No space left on device (os error 28)";
        assert_eq!(
            classify(journal),
            Some(Errno {
                code: libc::ENOSPC,
                name: "ENOSPC".to_owned()
            })
        );
        let ditto = "ditto: /v/s/install/.shellio-update-1/Contents/MacOS/probe-update-app: No space left on device";
        assert_eq!(classify(ditto).map(|e| e.code), Some(libc::ENOSPC));
        assert_eq!(
            classify("renamex_np(RENAME_SWAP): Cross-device link (os error 18)").map(|e| e.name),
            Some("EXDEV".to_owned())
        );
        assert_eq!(
            classify("ditto: /i/.shellio-update-1: Permission denied").map(|e| e.code),
            Some(libc::EACCES)
        );
        assert_eq!(
            classify("rename журнала (injected): Input/output error (os error 5)").map(|e| e.name),
            Some("EIO".to_owned())
        );
        assert_eq!(
            classify("x (os error 999)").map(|e| e.name),
            Some("errno 999".to_owned())
        );
        assert_eq!(classify("всё хорошо"), None);
        assert_eq!(
            classify("a: Permission denied; b: No space left on device").map(|e| e.code),
            Some(libc::EACCES),
            "первый по тексту"
        );
    }

    #[test]
    fn syscall_is_named_by_the_earliest_step_label() {
        assert_eq!(
            syscall_of("журнал: временный файл: No space left on device (os error 28)"),
            Some("open(2) O_CREAT временного файла журнала")
        );
        assert_eq!(
            syscall_of("журнал: запись временного файла: No space"),
            Some("write(2) временного файла журнала")
        );
        assert_eq!(
            syscall_of("журнал: rename журнала (injected): Input/output error (os error 5)"),
            Some("rename(2) временного файла журнала (injected)")
        );
        assert_eq!(
            syscall_of("восстановление: обратный обмен (injected): Input/output error"),
            Some("renamex_np(2) RENAME_SWAP обратного обмена (injected)")
        );
        assert_eq!(
            syscall_of("renamex_np(RENAME_SWAP): Cross-device link (os error 18)"),
            Some("renamex_np(2) RENAME_SWAP")
        );
        assert_eq!(
            syscall_of("восстановление: удаление кандидата: Permission denied (os error 13)"),
            Some("unlinkat(2) при удалении каталога кандидата")
        );
        assert!(
            syscall_of("копия /i/MacOS/probe-update-attach в /s/runtime/.prep-2/probe-update-attach: No space left on device (os error 28)")
                .is_some_and(|syscall| syscall.starts_with("fs::copy"))
        );
        assert!(syscall_of("ditto: /x: No space left on device").is_some());
        assert_eq!(syscall_of("нет меток"), None);
        let fault = Fault::of(
            "reply",
            "журнал: rename журнала (injected): Input/output error (os error 5)",
        );
        assert!(fault.injected && fault.is(libc::EIO));
        assert!(!Fault::of("reply", "ditto: x: Permission denied").injected);
    }

    #[test]
    fn fill_refuses_a_volume_larger_than_a_stand_volume() {
        let dir = std::env::temp_dir();
        let (_, total) = sys::volume_space(&dir).unwrap();
        if total <= MAX_FILL_BYTES {
            return;
        }
        let path = dir.join(format!("probe-update-fill-{}", std::process::id()));
        let refused = fill(&path, None).unwrap_err();
        assert!(refused.contains("больше"), "{refused}");
        assert!(!path.exists(), "файл не создаётся");
    }

    #[test]
    fn pattern_is_not_zero_and_not_constant() {
        let mut buf = vec![0u8; 4096];
        pattern(&mut buf, 1);
        assert!(buf.iter().any(|byte| *byte != 0));
        assert_ne!(buf[..8], buf[8..16]);
    }
}
