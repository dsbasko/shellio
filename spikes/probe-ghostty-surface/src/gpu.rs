//! Наблюдатель графической памяти и разбор его калибровки (задача 11).
//!
//! Счётчики процесса: `currentAllocatedSize` каждого устройства Metal (с `registryID`) и
//! `footprint` процесса пробы по категориям. Документация Apple не описывает, как
//! `currentAllocatedSize` учитывает текстуры с хранилищем в `IOSurface`, поэтому годность каждого
//! счётчика для задачи 12 устанавливает калибровка на ресурсах известного размера
//! (`scenario/gpu_calibration.rs`), а не предположение.
//!
//! `footprint` снимает служба `run.sh`, а не сам процесс: сценарий пишет строку «<pid> <метка>» в
//! FIFO `footprint.fifo` в `--out`, служба пишет JSON в `footprint/<метка>.json` (публикует
//! переименованием) и отвечает строкой «<метка> <код>» в FIFO `footprint.reply`. Запрос, ожидание
//! ответа и чтение файла идут в рабочем потоке, главный поток опрашивает только итог.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLCopyAllDevices, MTLDevice};
use serde::Serialize;
use serde_json::Value;

use crate::clock;

pub const FOOTPRINT_FIFO: &str = "footprint.fifo";
pub const FOOTPRINT_REPLY: &str = "footprint.reply";
pub const FOOTPRINT_DIR: &str = "footprint";
const LABEL_LIMIT: usize = 64;
const MIB: f64 = 1024.0 * 1024.0;

/// Свойства устройства Metal, от которых зависит выбор рендера.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeviceTraits {
    pub name: String,
    pub registry_id: u64,
    pub low_power: bool,
    pub headless: bool,
    pub removable: bool,
    pub unified_memory: bool,
}

/// Устройство, которое выбрал бы рендер ghostty. Это та же логика, что `chooseDevice`
/// (`src/renderer/metal/Device.zig:108–137`), а не выбор, прочитанный из рендера: безголовые
/// пропускаются, запоминается последнее подходящее, съёмное или энергоэффективное останавливает
/// перебор.
pub fn renderer_choice(devices: &[DeviceTraits]) -> Option<usize> {
    let mut chosen = None;
    for (index, device) in devices.iter().enumerate() {
        if device.headless {
            continue;
        }
        chosen = Some(index);
        if device.removable || device.low_power {
            break;
        }
    }
    chosen
}

/// Режим хранения цели рендера ghostty (`Device.zig:44–48`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TargetStorage {
    Shared,
    Managed,
}

pub fn target_storage(unified_memory: bool) -> TargetStorage {
    if unified_memory {
        TargetStorage::Shared
    } else {
        TargetStorage::Managed
    }
}

pub struct Device {
    pub traits: DeviceTraits,
    pub device: Retained<ProtocolObject<dyn MTLDevice>>,
}

/// Все устройства Metal в порядке `MTLCopyAllDevices`, как их перебирает рендер.
pub fn devices() -> Vec<Device> {
    MTLCopyAllDevices()
        .iter()
        .map(|device| Device {
            traits: DeviceTraits {
                name: device.name().to_string(),
                registry_id: device.registryID(),
                low_power: device.isLowPower(),
                headless: device.isHeadless(),
                removable: device.isRemovable(),
                unified_memory: device.hasUnifiedMemory(),
            },
            device,
        })
        .collect()
}

/// Имя счётчика `currentAllocatedSize`: у устройства рендера без суффикса, у остальных с
/// `registryID`.
pub fn allocated_counter(traits: &DeviceTraits, renderer: bool) -> String {
    if renderer {
        "metal.current_allocated_size".to_owned()
    } else {
        format!("metal.current_allocated_size.{:x}", traits.registry_id)
    }
}

/// `currentAllocatedSize` всех устройств в момент вызова.
pub fn allocated_counters(devices: &[Device], renderer: usize) -> BTreeMap<String, i64> {
    devices
        .iter()
        .enumerate()
        .map(|(index, device)| {
            (
                allocated_counter(&device.traits, index == renderer),
                to_i64(device.device.currentAllocatedSize() as u64),
            )
        })
        .collect()
}

pub fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// Категория `footprint` в байтах.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Category {
    pub dirty: u64,
    pub swapped: u64,
    pub clean: u64,
    pub reclaimable: u64,
    pub wired: u64,
    pub regions: u64,
}

/// Вывод `footprint -j` для одного процесса, в байтах.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Footprint {
    pub pid: i32,
    pub footprint: u64,
    pub phys_footprint: Option<u64>,
    pub categories: BTreeMap<String, Category>,
}

/// Разбирает `footprint -j`: единица из «bytes per unit», непустой `errors` и чужой PID —
/// ошибки.
pub fn parse_footprint(json: &str, pid: i32) -> Result<Footprint, String> {
    let root: Value =
        serde_json::from_str(json).map_err(|err| format!("JSON footprint не разобран: {err}"))?;
    let unit = root
        .get("bytes per unit")
        .and_then(Value::as_u64)
        .filter(|unit| *unit > 0)
        .ok_or("в выводе footprint нет «bytes per unit»")?;
    if let Some(errors) = root.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        return Err(format!(
            "footprint сообщил об ошибках: {}",
            Value::Array(errors.clone())
        ));
    }
    let processes = root
        .get("processes")
        .and_then(Value::as_array)
        .ok_or("в выводе footprint нет processes")?;
    let process = processes
        .iter()
        .find(|process| process.get("pid").and_then(Value::as_i64) == Some(i64::from(pid)))
        .ok_or_else(|| {
            let pids: Vec<i64> = processes
                .iter()
                .filter_map(|process| process.get("pid").and_then(Value::as_i64))
                .collect();
            format!("в выводе footprint нет процесса {pid}, есть {pids:?}")
        })?;
    let scale = |value: u64, what: &str| {
        value
            .checked_mul(unit)
            .ok_or_else(|| format!("переполнение в «{what}»"))
    };
    let footprint = process
        .get("footprint")
        .and_then(Value::as_u64)
        .ok_or("у процесса в выводе footprint нет «footprint»")?;
    let footprint = scale(footprint, "footprint")?;
    let phys_footprint = match process
        .get("auxiliary")
        .and_then(|aux| aux.get("phys_footprint"))
        .and_then(Value::as_u64)
    {
        Some(value) => Some(scale(value, "phys_footprint")?),
        None => None,
    };
    let map = process
        .get("categories")
        .and_then(Value::as_object)
        .ok_or("у процесса в выводе footprint нет categories")?;
    let mut categories = BTreeMap::new();
    for (name, value) in map {
        let field = |key: &str| -> Result<u64, String> {
            scale(
                value.get(key).and_then(Value::as_u64).unwrap_or(0),
                &format!("{name}.{key}"),
            )
        };
        if value.get("dirty").and_then(Value::as_u64).is_none() {
            return Err(format!("у категории «{name}» нет dirty"));
        }
        categories.insert(
            name.clone(),
            Category {
                dirty: field("dirty")?,
                swapped: field("swapped")?,
                clean: field("clean")?,
                reclaimable: field("reclaimable")?,
                wired: field("wired")?,
                regions: value.get("regions").and_then(Value::as_u64).unwrap_or(0),
            },
        );
    }
    Ok(Footprint {
        pid,
        footprint,
        phys_footprint,
        categories,
    })
}

/// Счётчики из вывода `footprint`: итог, `phys_footprint`, `dirty` и `clean` каждой категории.
pub fn footprint_counters(footprint: &Footprint) -> BTreeMap<String, i64> {
    let mut counters = BTreeMap::new();
    counters.insert(
        "footprint.footprint".to_owned(),
        to_i64(footprint.footprint),
    );
    if let Some(phys) = footprint.phys_footprint {
        counters.insert("footprint.phys_footprint".to_owned(), to_i64(phys));
    }
    for (name, category) in &footprint.categories {
        counters.insert(format!("footprint.{name}.dirty"), to_i64(category.dirty));
        counters.insert(format!("footprint.{name}.clean"), to_i64(category.clean));
    }
    counters
}

/// Метка запроса входит в имя файла службы.
pub fn valid_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= LABEL_LIMIT
        && label
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

pub fn request_line(pid: i32, label: &str) -> String {
    format!("{pid} {label}\n")
}

/// Код `footprint` из ответа службы «<метка> <код>».
pub fn parse_reply(line: &str, label: &str) -> Result<i32, String> {
    let mut parts = line.split_whitespace();
    match (parts.next(), parts.next(), parts.next()) {
        (Some(got), Some(code), None) if got == label => code
            .parse::<i32>()
            .map_err(|_| format!("код в ответе службы footprint не число: {line:?}")),
        (Some(got), Some(_), None) => Err(format!(
            "ответ службы footprint на «{got}», ожидался «{label}»"
        )),
        _ => Err(format!("ответ службы footprint не разобран: {line:?}")),
    }
}

#[derive(Clone, Debug)]
pub struct FootprintReply {
    pub footprint: Footprint,
    pub requested_ns: u64,
    pub replied_ns: u64,
}

type FootprintSlot = Arc<Mutex<Option<Result<FootprintReply, String>>>>;

/// Запрос `footprint` в работе.
pub struct FootprintRequest {
    slot: FootprintSlot,
}

impl FootprintRequest {
    pub fn take(&self) -> Option<Result<FootprintReply, String>> {
        self.slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }
}

/// Просит службу `run.sh` снять `footprint` процесса `pid` под меткой `label`.
pub fn request_footprint(out: &Path, pid: i32, label: &str) -> FootprintRequest {
    let slot: FootprintSlot = Arc::new(Mutex::new(None));
    let filled = Arc::clone(&slot);
    let out = out.to_path_buf();
    let owned = label.to_owned();
    let spawned = thread::Builder::new()
        .name("probe-footprint".to_owned())
        .spawn(move || {
            let result = exchange(&out, pid, &owned);
            *filled
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(result);
        });
    if let Err(err) = spawned {
        *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(Err(format!("рабочий поток не запущен: {err}")));
    }
    FootprintRequest { slot }
}

fn exchange(out: &Path, pid: i32, label: &str) -> Result<FootprintReply, String> {
    if !valid_label(label) {
        return Err(format!("метка footprint «{label}» недопустима"));
    }
    let fifo = out.join(FOOTPRINT_FIFO);
    // Без читателя открытие FIFO на запись без блокировки даёт ENXIO: службы run.sh нет.
    let mut request = OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)
        .map_err(|err| match err.raw_os_error() {
            Some(libc::ENXIO) => format!("служба footprint run.sh не слушает {}", fifo.display()),
            _ => format!("{}: {err}", fifo.display()),
        })?;
    // Служба держит ответный FIFO открытым и на запись, поэтому открытие на чтение не ждёт.
    let reply_path = out.join(FOOTPRINT_REPLY);
    let reply = OpenOptions::new()
        .read(true)
        .open(&reply_path)
        .map_err(|err| format!("{}: {err}", reply_path.display()))?;
    let requested_ns = clock::uptime_ns();
    request
        .write_all(request_line(pid, label).as_bytes())
        .map_err(|err| format!("запрос footprint не записан: {err}"))?;
    drop(request);
    let mut line = String::new();
    BufReader::new(reply)
        .read_line(&mut line)
        .map_err(|err| format!("ответ службы footprint не прочитан: {err}"))?;
    let replied_ns = clock::uptime_ns();
    let code = parse_reply(&line, label)?;
    let dir = out.join(FOOTPRINT_DIR);
    if code != 0 {
        let log = fs::read_to_string(dir.join(format!("{label}.txt"))).unwrap_or_default();
        return Err(format!(
            "footprint {pid} завершился с кодом {code}: {}",
            log.trim()
        ));
    }
    let path = dir.join(format!("{label}.json"));
    let json = fs::read_to_string(&path).map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(FootprintReply {
        footprint: parse_footprint(&json, pid)?,
        requested_ns,
        replied_ns,
    })
}

pub fn align_up(value: u64, alignment: u64) -> u64 {
    if alignment <= 1 {
        value
    } else {
        value.div_ceil(alignment) * alignment
    }
}

/// Ожидаемый объём однослойной `IOSurface`: строка выравнивается по `row_alignment`
/// (`IOSurfaceGetPropertyAlignment(kIOSurfaceBytesPerRow)`), весь объём — по странице.
pub fn iosurface_expected(
    width: u64,
    height: u64,
    bytes_per_element: u64,
    row_alignment: u64,
    page: u64,
) -> u64 {
    align_up(
        align_up(width * bytes_per_element, row_alignment) * height,
        page,
    )
}

/// Ожидаемый объём `MTLBuffer`: длина, выровненная по странице.
pub fn buffer_expected(length: u64, page: u64) -> u64 {
    align_up(length, page)
}

/// Полезный объём текстуры без выравнивания и служебных данных.
pub fn texture_payload(width: u64, height: u64, bytes_per_element: u64) -> u64 {
    width * height * bytes_per_element
}

/// Действие фазы калибровки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Nothing,
    CreateSurfaces,
    TouchSurfaces,
    RenderSurfaces,
    CreateBuffer,
    TouchBuffer,
    CreatePrivate,
    TouchPrivate,
    SetLayerContents,
    ReleaseBuffer,
    ReleasePrivate,
    ReleaseTexture(usize),
    Purge(usize),
    ReleaseSurface(usize),
    ClearLayerContents,
}

/// Ресурс фазы, объём которого служит мерой изменения счётчика.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Subject {
    None,
    Surfaces,
    Surface(usize),
    Buffer,
    Private,
}

pub struct Phase {
    pub label: &'static str,
    pub action: Action,
    pub subject: Subject,
}

const fn phase(label: &'static str, action: Action, subject: Subject) -> Phase {
    Phase {
        label,
        action,
        subject,
    }
}

/// Текстур с хранилищем в `IOSurface`; первая держится как `contents` слоя.
pub const SURFACES: usize = 4;
pub const LAYER_SURFACE: usize = 0;

/// Фазы калибровки по порядку. После каждой — барьер (проход отображения, выполнение команд GPU)
/// и устоявшийся замер (`settle`). `baseline` → `noise` и предпоследняя → `final` дают шум без
/// действия. `iosurface-rendered` проводит цели через команды GPU, как рендер ghostty, чтобы их
/// освобождение шло тем же путём.
pub static PHASES: [Phase; 25] = [
    phase("baseline", Action::Nothing, Subject::None),
    phase("noise", Action::Nothing, Subject::None),
    phase(
        "iosurface-created",
        Action::CreateSurfaces,
        Subject::Surfaces,
    ),
    phase(
        "iosurface-touched",
        Action::TouchSurfaces,
        Subject::Surfaces,
    ),
    phase(
        "iosurface-rendered",
        Action::RenderSurfaces,
        Subject::Surfaces,
    ),
    phase("buffer-created", Action::CreateBuffer, Subject::Buffer),
    phase("buffer-touched", Action::TouchBuffer, Subject::Buffer),
    phase("private-created", Action::CreatePrivate, Subject::Private),
    phase("private-touched", Action::TouchPrivate, Subject::Private),
    phase(
        "layer-contents-set",
        Action::SetLayerContents,
        Subject::Surface(LAYER_SURFACE),
    ),
    phase("buffer-released", Action::ReleaseBuffer, Subject::Buffer),
    phase("private-released", Action::ReleasePrivate, Subject::Private),
    phase(
        "s0-texture-released",
        Action::ReleaseTexture(0),
        Subject::Surface(0),
    ),
    phase(
        "s1-texture-released",
        Action::ReleaseTexture(1),
        Subject::Surface(1),
    ),
    phase("s1-purgeable-empty", Action::Purge(1), Subject::Surface(1)),
    phase(
        "s1-released",
        Action::ReleaseSurface(1),
        Subject::Surface(1),
    ),
    phase(
        "s2-texture-released",
        Action::ReleaseTexture(2),
        Subject::Surface(2),
    ),
    phase("s2-purgeable-empty", Action::Purge(2), Subject::Surface(2)),
    phase(
        "s2-released",
        Action::ReleaseSurface(2),
        Subject::Surface(2),
    ),
    // Порядок `Target.deinit` ghostty: purgeable и освобождение `IOSurface`, затем текстура.
    phase("s3-purgeable-empty", Action::Purge(3), Subject::Surface(3)),
    phase(
        "s3-released",
        Action::ReleaseSurface(3),
        Subject::Surface(3),
    ),
    phase(
        "s3-texture-released",
        Action::ReleaseTexture(3),
        Subject::Surface(3),
    ),
    phase(
        "s0-released-held-by-layer",
        Action::ReleaseSurface(0),
        Subject::Surface(0),
    ),
    phase(
        "s0-layer-contents-cleared",
        Action::ClearLayerContents,
        Subject::Surface(0),
    ),
    phase("final", Action::Nothing, Subject::None),
];

/// Группы фаз для вывода: суммарное изменение счётчика по фазам группы против объёма ресурса.
pub struct Group {
    pub name: &'static str,
    pub subject: Subject,
    pub phases: &'static [&'static str],
}

pub static GROUPS: [Group; 10] = [
    Group {
        name: "iosurface-alloc",
        subject: Subject::Surfaces,
        phases: &["iosurface-created", "iosurface-touched"],
    },
    Group {
        name: "iosurface-render",
        subject: Subject::Surfaces,
        phases: &["iosurface-rendered"],
    },
    Group {
        name: "buffer-alloc",
        subject: Subject::Buffer,
        phases: &["buffer-created", "buffer-touched"],
    },
    Group {
        name: "private-alloc",
        subject: Subject::Private,
        phases: &["private-created", "private-touched"],
    },
    Group {
        name: "buffer-release",
        subject: Subject::Buffer,
        phases: &["buffer-released"],
    },
    Group {
        name: "private-release",
        subject: Subject::Private,
        phases: &["private-released"],
    },
    Group {
        name: "s1-release",
        subject: Subject::Surface(1),
        phases: &["s1-texture-released", "s1-purgeable-empty", "s1-released"],
    },
    Group {
        name: "s2-release",
        subject: Subject::Surface(2),
        phases: &["s2-texture-released", "s2-purgeable-empty", "s2-released"],
    },
    Group {
        name: "s3-release",
        subject: Subject::Surface(3),
        phases: &["s3-purgeable-empty", "s3-released", "s3-texture-released"],
    },
    Group {
        name: "s0-release",
        subject: Subject::Surface(0),
        phases: &[
            "s0-texture-released",
            "s0-released-held-by-layer",
            "s0-layer-contents-cleared",
        ],
    },
];

/// Группы освобождения `IOSurface` без слоя: по ним судится пропорциональность.
const SURFACE_RELEASES: [&str; 3] = ["s1-release", "s2-release", "s3-release"];
/// В порядке ghostty хранилище уходит раньше текстуры: счётчик хранилища падает до этой фазы,
/// счётчик объектов Metal — на ней.
const STORAGE_ORDER_GROUP: &str = "s3-release";
const TEXTURE_AFTER_STORAGE: &str = "s3-texture-released";

/// Известные объёмы ресурсов, байты: `IOSurfaceGetAllocSize` и `allocatedSize`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Sizes {
    pub surfaces: [u64; SURFACES],
    pub buffer: u64,
    pub private: u64,
}

impl Sizes {
    pub fn of(&self, subject: Subject) -> u64 {
        match subject {
            Subject::None => 0,
            Subject::Surfaces => self.surfaces.iter().sum(),
            Subject::Surface(index) => self.surfaces.get(index).copied().unwrap_or(0),
            Subject::Buffer => self.buffer,
            Subject::Private => self.private,
        }
    }
}

/// Замер после фазы.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Sample {
    pub label: String,
    pub at_ns: u64,
    pub counters: BTreeMap<String, i64>,
    /// Замеров в серии фазы до устоявшегося.
    pub requests: usize,
    /// `false` — серия упёрлась в `SETTLE_CAP_NS`, не устоявшись.
    pub settled: bool,
    /// Изменения наблюдаемых счётчиков от первого замера серии до устоявшегося: то, что
    /// проявилось позже барьера.
    pub deferred: BTreeMap<String, i64>,
    /// От барьера до замера, в котором наблюдаемые счётчики изменились последний раз, мс.
    pub last_change_ms: Option<f64>,
}

/// Шаг замеров в серии фазы.
pub const SETTLE_PACE_NS: u64 = 100_000_000;
/// Серия устоялась, когда наблюдаемые счётчики не менялись столько. Временную память драйвера
/// после буфера команд Metal отпускает через 1,1–1,4 с (калибровка задачи 11), окно её покрывает.
pub const SETTLE_WINDOW_NS: u64 = 1_500_000_000;
/// Предел серии от барьера.
pub const SETTLE_CAP_NS: u64 = 5_000_000_000;
/// Изменение меньше этого — шум аллокатора пробы, а не событие.
pub const SETTLE_EPSILON: i64 = 1024 * 1024;
/// Счётчики, по которым судится, устоялась ли серия.
pub const SETTLE_COUNTERS: [&str; 2] = ["footprint.footprint", "metal.current_allocated_size"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settle {
    Pending,
    Settled,
    Capped,
}

fn close(a: &Sample, b: &Sample) -> bool {
    SETTLE_COUNTERS.iter().all(|counter| {
        value(a, counter).abs_diff(value(b, counter)) < SETTLE_EPSILON.unsigned_abs()
    })
}

/// Состояние серии замеров фазы, начатой барьером в `since_ns`. Освобождение ресурсов, которые
/// прошли через команды GPU, Metal выполняет позже и асинхронно, поэтому один замер после
/// барьера приписал бы его следующей фазе.
pub fn settle(series: &[Sample], since_ns: u64) -> Settle {
    let Some(last) = series.last() else {
        return Settle::Pending;
    };
    let mut start = series.len() - 1;
    while start > 0 && close(&series[start - 1], last) {
        start -= 1;
    }
    if last.at_ns.saturating_sub(series[start].at_ns) >= SETTLE_WINDOW_NS {
        Settle::Settled
    } else if last.at_ns.saturating_sub(since_ns) >= SETTLE_CAP_NS {
        Settle::Capped
    } else {
        Settle::Pending
    }
}

/// Итоговый замер серии: последний, с отложенными изменениями наблюдаемых счётчиков и временем
/// последнего изменения от барьера.
pub fn settled_sample(series: &[Sample], since_ns: u64, settled: bool) -> Option<Sample> {
    let first = series.first()?;
    let last = series.last()?;
    let deferred = SETTLE_COUNTERS
        .iter()
        .filter_map(|counter| {
            let change = value(last, counter).saturating_sub(value(first, counter));
            (change.unsigned_abs() >= SETTLE_EPSILON.unsigned_abs())
                .then(|| ((*counter).to_owned(), change))
        })
        .collect();
    let last_change_ms = series
        .windows(2)
        .rev()
        .find(|pair| !close(&pair[0], &pair[1]))
        .map(|pair| clock::interval_ms(since_ns, pair[1].at_ns));
    Some(Sample {
        requests: series.len(),
        settled,
        deferred,
        last_change_ms,
        ..last.clone()
    })
}

/// Изменение счётчика относительно объёма ресурса события.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Seen {
    /// Рост от `SEES` до `EXCESS` объёма.
    Up,
    /// Падение от `SEES` до `EXCESS` объёма.
    Down,
    /// Изменение больше `EXCESS` объёма в любую сторону: событие видно, но вместе с чужим.
    Excess,
    /// Изменение не больше `FLAT` объёма.
    Flat,
    Partial,
    /// Шум без действия больше `FLAT` объёма: вывод не выносится.
    Noisy,
}

impl Seen {
    pub fn name(self) -> &'static str {
        match self {
            Seen::Up => "up",
            Seen::Down => "down",
            Seen::Excess => "excess",
            Seen::Flat => "flat",
            Seen::Partial => "partial",
            Seen::Noisy => "noisy",
        }
    }
}

pub const SEES: f64 = 0.75;
pub const EXCESS: f64 = 1.5;
pub const FLAT: f64 = 0.25;
/// Счётчик без ключевого имени попадает в таблицу, если хоть одна фаза сдвинула его на столько.
pub const ROW_THRESHOLD: i64 = 1024 * 1024;

pub fn classify(delta: i64, size: u64, noise: u64) -> Seen {
    let size = size as f64;
    if size <= 0.0 || noise as f64 > FLAT * size {
        return Seen::Noisy;
    }
    let ratio = delta as f64 / size;
    if ratio.abs() > EXCESS {
        Seen::Excess
    } else if ratio >= SEES {
        Seen::Up
    } else if ratio <= -SEES {
        Seen::Down
    } else if ratio.abs() <= FLAT {
        Seen::Flat
    } else {
        Seen::Partial
    }
}

fn ratio(delta: i64, size: u64) -> f64 {
    if size == 0 {
        0.0
    } else {
        delta as f64 / size as f64
    }
}

/// Счётчик собственного учёта пробы, а не процесса: для задачи 12 не годится по построению.
pub fn is_own(counter: &str) -> bool {
    counter.starts_with("own.")
}

/// Счётчики, которые попадают в таблицу всегда.
pub fn is_key(counter: &str) -> bool {
    counter.starts_with("metal.")
        || counter.starts_with("own.")
        || counter == "footprint.footprint"
        || counter == "footprint.phys_footprint"
        || counter.starts_with("footprint.IOSurface.")
        || counter.starts_with("footprint.IOAccelerator")
        || counter.starts_with("footprint.Owned physical footprint")
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Cell {
    pub phase: &'static str,
    pub delta: i64,
    pub ratio: f64,
    pub seen: Seen,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GroupCell {
    pub group: &'static str,
    pub delta: i64,
    pub ratio: f64,
    pub seen: Seen,
    /// Фаза группы с наибольшим по модулю изменением.
    pub at: &'static str,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CounterRow {
    pub counter: String,
    pub own: bool,
    pub noise: u64,
    pub phases: Vec<Cell>,
    pub groups: Vec<GroupCell>,
    /// Видит выделение хранилища `IOSurface` и каждое из трёх освобождений без слоя, причём в
    /// порядке ghostty — до освобождения текстуры: следит за хранилищем, а не за объектом Metal.
    pub sees_iosurface: bool,
}

impl CounterRow {
    pub fn group(&self, name: &str) -> Option<&GroupCell> {
        self.groups.iter().find(|group| group.group == name)
    }

    /// Строка для вывода: группы с классом и фазой, шум.
    pub fn describe(&self) -> String {
        let groups: Vec<String> = self
            .groups
            .iter()
            .map(|group| {
                format!(
                    "{} {} {:.2} ({})",
                    group.group,
                    group.seen.name(),
                    group.ratio,
                    group.at
                )
            })
            .collect();
        format!(
            "{}; шум {:.2} МиБ",
            groups.join(", "),
            self.noise as f64 / MIB
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Calibration {
    pub rows: Vec<CounterRow>,
    /// Счётчики процесса, годные для задачи 12.
    pub fit: Vec<String>,
}

fn value(sample: &Sample, counter: &str) -> i64 {
    sample.counters.get(counter).copied().unwrap_or(0)
}

/// Таблица «счётчик → видимые события» по замерам после каждой фазы `PHASES`.
pub fn calibrate(samples: &[Sample], sizes: &Sizes) -> Result<Calibration, String> {
    let labels: Vec<&str> = samples.iter().map(|sample| sample.label.as_str()).collect();
    let expected: Vec<&str> = PHASES.iter().map(|phase| phase.label).collect();
    if labels != expected {
        return Err(format!(
            "замеры калибровки {labels:?} не совпадают с фазами {expected:?}"
        ));
    }
    let at = |label: &str| PHASES.iter().position(|phase| phase.label == label);
    let delta = |counter: &str, index: usize| -> i64 {
        if index == 0 {
            0
        } else {
            value(&samples[index], counter).saturating_sub(value(&samples[index - 1], counter))
        }
    };
    let counters: BTreeSet<&str> = samples
        .iter()
        .flat_map(|sample| sample.counters.keys().map(String::as_str))
        .collect();
    let noise_phases = [
        at("noise").unwrap_or(1),
        at("final").unwrap_or(PHASES.len() - 1),
    ];
    let mut rows = Vec::new();
    for counter in counters {
        let noise = noise_phases
            .iter()
            .map(|index| delta(counter, *index).unsigned_abs())
            .max()
            .unwrap_or(0);
        let phases: Vec<Cell> = PHASES
            .iter()
            .enumerate()
            .filter(|(_, phase)| phase.subject != Subject::None)
            .map(|(index, phase)| {
                let size = sizes.of(phase.subject);
                let change = delta(counter, index);
                Cell {
                    phase: phase.label,
                    delta: change,
                    ratio: ratio(change, size),
                    seen: classify(change, size, noise),
                }
            })
            .collect();
        let moved = phases
            .iter()
            .any(|cell| cell.delta.unsigned_abs() >= ROW_THRESHOLD.unsigned_abs());
        if !is_key(counter) && !moved {
            continue;
        }
        let groups: Vec<GroupCell> = GROUPS
            .iter()
            .map(|group| {
                let size = sizes.of(group.subject);
                let mut sum = 0i64;
                let mut largest = (group.phases.first().copied().unwrap_or(""), 0u64);
                for label in group.phases {
                    let change = at(label).map_or(0, |index| delta(counter, index));
                    sum = sum.saturating_add(change);
                    if change.unsigned_abs() > largest.1 {
                        largest = (label, change.unsigned_abs());
                    }
                }
                GroupCell {
                    group: group.name,
                    delta: sum,
                    ratio: ratio(sum, size),
                    seen: classify(sum, size, noise),
                    at: largest.0,
                }
            })
            .collect();
        let seen_in = |name: &str, seen: Seen| {
            groups
                .iter()
                .any(|group| group.group == name && group.seen == seen)
        };
        let follows_storage = groups
            .iter()
            .any(|group| group.group == STORAGE_ORDER_GROUP && group.at != TEXTURE_AFTER_STORAGE);
        let sees_iosurface = seen_in("iosurface-alloc", Seen::Up)
            && SURFACE_RELEASES
                .iter()
                .all(|name| seen_in(name, Seen::Down))
            && follows_storage;
        rows.push(CounterRow {
            counter: counter.to_owned(),
            own: is_own(counter),
            noise,
            phases,
            groups,
            sees_iosurface,
        });
    }
    let fit = rows
        .iter()
        .filter(|row| !row.own && row.sees_iosurface)
        .map(|row| row.counter.clone())
        .collect();
    Ok(Calibration { rows, fit })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB_U: u64 = 1024 * 1024;

    fn device(name: &str, low_power: bool, headless: bool, removable: bool) -> DeviceTraits {
        DeviceTraits {
            name: name.to_owned(),
            registry_id: 0,
            low_power,
            headless,
            removable,
            unified_memory: true,
        }
    }

    #[test]
    fn renderer_choice_follows_ghostty() {
        assert_eq!(renderer_choice(&[]), None);
        assert_eq!(renderer_choice(&[device("h", false, true, false)]), None);
        assert_eq!(
            renderer_choice(&[device("a", false, false, false)]),
            Some(0)
        );
        // Без энергоэффективного и съёмного остаётся последнее подходящее.
        assert_eq!(
            renderer_choice(&[
                device("a", false, false, false),
                device("b", false, false, false)
            ]),
            Some(1)
        );
        assert_eq!(
            renderer_choice(&[
                device("igpu", true, false, false),
                device("dgpu", false, false, false)
            ]),
            Some(0)
        );
        assert_eq!(
            renderer_choice(&[
                device("dgpu", false, false, false),
                device("egpu", false, false, true),
                device("igpu", true, false, false)
            ]),
            Some(1)
        );
        assert_eq!(
            renderer_choice(&[
                device("headless-low", true, true, false),
                device("dgpu", false, false, false)
            ]),
            Some(1)
        );
    }

    #[test]
    fn target_storage_follows_unified_memory() {
        assert_eq!(target_storage(true), TargetStorage::Shared);
        assert_eq!(target_storage(false), TargetStorage::Managed);
    }

    const FOOTPRINT_JSON: &str = r#"{"unit":"byte","bytes per unit":1,
        "processes":[{"name":"probe","pid":4242,"footprint":104857600,
            "categories":{
                "IOSurface":{"dirty":33554432,"swapped":0,"clean":0,"reclaimable":0,"wired":32768,"regions":4},
                "IOAccelerator (graphics)":{"dirty":2408448,"swapped":2408448,"clean":0,"reclaimable":0,"wired":0,"regions":91},
                "__TEXT":{"dirty":0,"swapped":0,"clean":16384,"reclaimable":0,"wired":0,"regions":82}},
            "auxiliary":{"phys_footprint_peak":161759832,"phys_footprint":104857600}}],
        "errors":[],"warnings":[]}"#;

    #[test]
    fn footprint_is_parsed_in_bytes() {
        let footprint = parse_footprint(FOOTPRINT_JSON, 4242).unwrap();
        assert_eq!(footprint.footprint, 100 * MIB_U);
        assert_eq!(footprint.phys_footprint, Some(100 * MIB_U));
        assert_eq!(footprint.categories["IOSurface"].dirty, 32 * MIB_U);
        assert_eq!(footprint.categories["IOSurface"].wired, 32768);
        assert_eq!(footprint.categories["IOSurface"].regions, 4);
        assert_eq!(footprint.categories["__TEXT"].clean, 16384);
        let counters = footprint_counters(&footprint);
        assert_eq!(counters["footprint.footprint"], 100 * 1024 * 1024);
        assert_eq!(counters["footprint.IOSurface.dirty"], 32 * 1024 * 1024);
        assert_eq!(
            counters["footprint.IOAccelerator (graphics).dirty"],
            2408448
        );
        assert_eq!(counters["footprint.__TEXT.clean"], 16384);
    }

    #[test]
    fn footprint_unit_scales_values() {
        let pages = FOOTPRINT_JSON.replace("\"bytes per unit\":1", "\"bytes per unit\":16384");
        let footprint = parse_footprint(&pages, 4242).unwrap();
        assert_eq!(footprint.categories["__TEXT"].clean, 16384 * 16384);
        assert_eq!(footprint.footprint, 100 * MIB_U * 16384);
    }

    #[test]
    fn footprint_rejects_errors_foreign_pid_and_broken_output() {
        assert!(parse_footprint(FOOTPRINT_JSON, 1).is_err_and(|e| e.contains("4242")));
        let failed = FOOTPRINT_JSON.replace("\"errors\":[]", "\"errors\":[\"no task\"]");
        assert!(parse_footprint(&failed, 4242).is_err_and(|e| e.contains("no task")));
        let unitless = FOOTPRINT_JSON.replace("\"bytes per unit\":1,", "");
        assert!(parse_footprint(&unitless, 4242).is_err_and(|e| e.contains("bytes per unit")));
        let dirtless = FOOTPRINT_JSON.replace(
            "{\"dirty\":0,\"swapped\":0,\"clean\":16384",
            "{\"clean\":16384",
        );
        assert!(parse_footprint(&dirtless, 4242).is_err_and(|e| e.contains("__TEXT")));
        assert!(parse_footprint("not json", 4242).is_err());
    }

    #[test]
    fn request_and_reply_protocol() {
        assert!(valid_label("03-iosurface-touched"));
        assert!(!valid_label(""));
        assert!(!valid_label("a b"));
        assert!(!valid_label("../x"));
        assert!(!valid_label(&"a".repeat(65)));
        assert_eq!(request_line(42, "01-noise"), "42 01-noise\n");
        assert_eq!(parse_reply("01-noise 0\n", "01-noise"), Ok(0));
        assert_eq!(parse_reply("01-noise 1", "01-noise"), Ok(1));
        assert!(parse_reply("02-other 0", "01-noise").is_err_and(|e| e.contains("02-other")));
        assert!(parse_reply("01-noise", "01-noise").is_err());
        assert!(parse_reply("01-noise x", "01-noise").is_err());
        assert!(parse_reply("01-noise 0 extra", "01-noise").is_err());
    }

    #[test]
    fn expected_sizes_of_known_resources() {
        assert_eq!(align_up(0, 16384), 0);
        assert_eq!(align_up(1, 16384), 16384);
        assert_eq!(align_up(16384, 16384), 16384);
        assert_eq!(align_up(7, 1), 7);
        // Цель калибровки: 2048×1024 BGRA — 8 МиБ без выравнивания.
        assert_eq!(iosurface_expected(2048, 1024, 4, 64, 16384), 8 * MIB_U);
        // Строка 4004 байта выравнивается до 4032, объём 12096 — до страницы.
        assert_eq!(iosurface_expected(1001, 3, 4, 64, 16384), 16384);
        assert_eq!(iosurface_expected(1001, 100, 4, 64, 4096), 405_504);
        assert_eq!(buffer_expected(16 * MIB_U, 16384), 16 * MIB_U);
        assert_eq!(buffer_expected(16 * MIB_U + 1, 16384), 16 * MIB_U + 16384);
        assert_eq!(texture_payload(2048, 2048, 4), 16 * MIB_U);
    }

    #[test]
    fn classify_against_size_and_noise() {
        let size = 32 * MIB_U;
        let mib = MIB_U as i64;
        assert_eq!(classify(32 * mib, size, 0), Seen::Up);
        assert_eq!(classify(25 * mib, size, 0), Seen::Up);
        assert_eq!(classify(-30 * mib, size, mib as u64), Seen::Down);
        assert_eq!(classify(8 * mib, size, 0), Seen::Flat);
        assert_eq!(classify(-8 * mib, size, 0), Seen::Flat);
        assert_eq!(classify(16 * mib, size, 0), Seen::Partial);
        assert_eq!(classify(32 * mib, size, 9 * MIB_U), Seen::Noisy);
        assert_eq!(classify(0, 0, 0), Seen::Noisy);
        assert_eq!(classify(48 * mib, size, 0), Seen::Up);
        assert_eq!(classify(49 * mib, size, 0), Seen::Excess);
        assert_eq!(classify(-80 * mib, size, 0), Seen::Excess);
    }

    fn point(at_ms: u64, footprint: i64, metal: i64) -> Sample {
        Sample {
            label: "phase".to_owned(),
            at_ns: at_ms * 1_000_000,
            counters: BTreeMap::from([
                ("footprint.footprint".to_owned(), footprint),
                ("metal.current_allocated_size".to_owned(), metal),
            ]),
            ..Sample::default()
        }
    }

    const PACE_MS: u64 = 100;

    fn window_ms() -> u64 {
        SETTLE_WINDOW_NS / 1_000_000
    }

    #[test]
    fn settle_waits_for_a_quiet_window() {
        let mib = MIB_U as i64;
        let w = window_ms();
        assert_eq!(settle(&[], 0), Settle::Pending);
        let mut quiet: Vec<Sample> = (0..w / PACE_MS)
            .map(|i| point(10 + i * PACE_MS, 100 * mib, 0))
            .collect();
        assert_eq!(settle(&quiet, 0), Settle::Pending);
        quiet.push(point(10 + w, 100 * mib, 0));
        assert_eq!(settle(&quiet, 0), Settle::Settled);
        // Отложенное освобождение на 210 мс начинает окно заново; шум меньше порога — нет.
        let mut deferred = vec![
            point(10, 100 * mib, 0),
            point(110, 100 * mib + 4096, 0),
            point(210, 84 * mib, 0),
        ];
        deferred.extend((1..w / PACE_MS).map(|i| point(210 + i * PACE_MS, 84 * mib, 0)));
        assert_eq!(settle(&deferred, 0), Settle::Pending);
        deferred.push(point(210 + w, 84 * mib, 0));
        assert_eq!(settle(&deferred, 0), Settle::Settled);
        let sample = settled_sample(&deferred, 0, true).unwrap();
        assert_eq!(sample.requests, (w / PACE_MS + 3) as usize);
        assert!(sample.settled);
        assert_eq!(
            sample.deferred,
            BTreeMap::from([("footprint.footprint".to_owned(), -16 * mib)])
        );
        assert_eq!(sample.last_change_ms, Some(210.0));
        assert_eq!(sample.at_ns, (210 + w) * 1_000_000);
        // Изменение `currentAllocatedSize` тоже сбрасывает окно.
        let metal = [
            point(10, 0, 0),
            point(400, 0, 8 * mib),
            point(600, 0, 8 * mib),
        ];
        assert_eq!(settle(&metal, 0), Settle::Pending);
    }

    #[test]
    fn settle_gives_up_at_the_cap() {
        let mib = MIB_U as i64;
        let cap = SETTLE_CAP_NS / 1_000_000;
        assert!(cap > window_ms());
        let restless: Vec<Sample> = (0..=cap / PACE_MS)
            .map(|i| point(i * PACE_MS, i as i64 * 2 * mib, 0))
            .collect();
        assert_eq!(settle(&restless, 0), Settle::Capped);
        assert_eq!(settle(&restless[..restless.len() - 1], 0), Settle::Pending);
        let sample = settled_sample(&restless, 0, false).unwrap();
        assert!(!sample.settled);
        assert_eq!(sample.last_change_ms, Some(cap as f64));
        assert!(settled_sample(&[], 0, true).is_none());
        let single = settled_sample(&[point(5, mib, 0)], 0, true).unwrap();
        assert!(single.deferred.is_empty());
        assert_eq!(single.last_change_ms, None);
    }

    #[test]
    fn phases_and_groups_are_consistent() {
        let labels: Vec<&str> = PHASES.iter().map(|phase| phase.label).collect();
        let unique: BTreeSet<&str> = labels.iter().copied().collect();
        assert_eq!(unique.len(), labels.len());
        assert!(labels.iter().all(|label| valid_label(label)));
        for group in &GROUPS {
            for label in group.phases {
                let phase = PHASES.iter().find(|phase| phase.label == *label).unwrap();
                assert_eq!(phase.subject, group.subject, "{label} в {}", group.name);
            }
        }
        for index in 0..SURFACES {
            let actions: Vec<Action> = PHASES.iter().map(|phase| phase.action).collect();
            assert!(actions.contains(&Action::ReleaseTexture(index)));
            assert!(actions.contains(&Action::ReleaseSurface(index)));
        }
        assert_eq!(PHASES.first().map(|p| p.label), Some("baseline"));
        assert_eq!(PHASES.last().map(|p| p.label), Some("final"));
    }

    /// Счётчик, видящий хранилище `IOSurface`, и слепой к нему; `S` — объём одной поверхности.
    fn synthetic(sees: bool) -> Vec<Sample> {
        const S: i64 = 8 * 1024 * 1024;
        let mut level = 100 * S;
        let mut blind = 50 * S;
        let mut samples = Vec::new();
        for phase in &PHASES {
            match phase.label {
                "iosurface-touched" if sees => level += 4 * S,
                "s1-purgeable-empty" | "s2-purgeable-empty" | "s3-purgeable-empty" if sees => {
                    level -= S
                }
                "s0-layer-contents-cleared" if sees => level -= S,
                "buffer-touched" | "private-touched" => blind += 2 * S,
                "buffer-released" | "private-released" => blind -= 2 * S,
                "noise" => level += 1024,
                _ => {}
            }
            let mut counters = BTreeMap::new();
            counters.insert("footprint.IOSurface.dirty".to_owned(), level);
            counters.insert("metal.current_allocated_size".to_owned(), blind);
            samples.push(Sample {
                label: phase.label.to_owned(),
                counters,
                ..Sample::default()
            });
        }
        samples
    }

    fn sizes() -> Sizes {
        Sizes {
            surfaces: [8 * MIB_U; SURFACES],
            buffer: 16 * MIB_U,
            private: 16 * MIB_U,
        }
    }

    #[test]
    fn calibration_finds_counters_that_see_iosurface() {
        let calibration = calibrate(&synthetic(true), &sizes()).unwrap();
        assert_eq!(
            calibration.fit,
            vec!["footprint.IOSurface.dirty".to_owned()]
        );
        let surface = calibration
            .rows
            .iter()
            .find(|row| row.counter == "footprint.IOSurface.dirty")
            .unwrap();
        assert_eq!(surface.noise, 1024);
        let alloc = surface.group("iosurface-alloc").unwrap();
        assert_eq!((alloc.seen, alloc.at), (Seen::Up, "iosurface-touched"));
        let s1 = surface.group("s1-release").unwrap();
        assert_eq!((s1.seen, s1.at), (Seen::Down, "s1-purgeable-empty"));
        let s0 = surface.group("s0-release").unwrap();
        assert_eq!((s0.seen, s0.at), (Seen::Down, "s0-layer-contents-cleared"));
        assert_eq!(surface.group("buffer-alloc").unwrap().seen, Seen::Flat);
        let metal = calibration
            .rows
            .iter()
            .find(|row| row.counter == "metal.current_allocated_size")
            .unwrap();
        assert!(!metal.sees_iosurface);
        assert_eq!(metal.group("buffer-alloc").unwrap().seen, Seen::Up);
        assert_eq!(metal.group("private-release").unwrap().seen, Seen::Down);
        assert_eq!(metal.group("iosurface-alloc").unwrap().seen, Seen::Flat);
        assert!(
            surface
                .describe()
                .contains("iosurface-alloc up 1.00 (iosurface-touched)")
        );
    }

    /// Счётчик объектов Metal: растёт при создании текстур и падает при их освобождении, хранилище
    /// `IOSurface` ему безразлично.
    #[test]
    fn texture_object_counter_does_not_fit() {
        const S: i64 = 8 * 1024 * 1024;
        let mut samples = synthetic(true);
        let mut level = 0;
        for (sample, phase) in samples.iter_mut().zip(&PHASES) {
            match phase.action {
                Action::CreateSurfaces => level += 4 * S,
                Action::ReleaseTexture(_) => level -= S,
                _ => {}
            }
            sample.counters.insert("metal.objects".to_owned(), level);
        }
        let calibration = calibrate(&samples, &sizes()).unwrap();
        assert_eq!(
            calibration.fit,
            vec!["footprint.IOSurface.dirty".to_owned()]
        );
        let objects = calibration
            .rows
            .iter()
            .find(|row| row.counter == "metal.objects")
            .unwrap();
        assert!(!objects.sees_iosurface);
        let s3 = objects.group("s3-release").unwrap();
        assert_eq!((s3.seen, s3.at), (Seen::Down, "s3-texture-released"));
        assert_eq!(
            objects.group("iosurface-alloc").unwrap().at,
            "iosurface-created"
        );
    }

    #[test]
    fn owned_unmapped_footprint_is_key() {
        assert!(is_key(
            "footprint.Owned physical footprint (unmapped).dirty"
        ));
        assert!(is_key(
            "footprint.Owned physical footprint (unmapped) (graphics).dirty"
        ));
        assert!(!is_key("footprint.Malloc Small.dirty"));
    }

    #[test]
    fn calibration_without_sighted_counters_and_with_wrong_phases() {
        let calibration = calibrate(&synthetic(false), &sizes()).unwrap();
        assert!(calibration.fit.is_empty());
        let mut broken = synthetic(true);
        broken.pop();
        assert!(calibrate(&broken, &sizes()).is_err());
    }

    #[test]
    fn unkeyed_counters_appear_only_when_moved() {
        let mut samples = synthetic(true);
        for (index, sample) in samples.iter_mut().enumerate() {
            sample.counters.insert(
                "footprint.Malloc Small.dirty".to_owned(),
                4096 * index as i64,
            );
            sample.counters.insert(
                "footprint.__DATA.dirty".to_owned(),
                if index > 3 { 2 * ROW_THRESHOLD } else { 0 },
            );
        }
        let calibration = calibrate(&samples, &sizes()).unwrap();
        let names: Vec<&str> = calibration
            .rows
            .iter()
            .map(|row| row.counter.as_str())
            .collect();
        assert!(!names.contains(&"footprint.Malloc Small.dirty"));
        assert!(names.contains(&"footprint.__DATA.dirty"));
    }

    #[test]
    fn own_counters_never_fit() {
        let mut samples = synthetic(true);
        for sample in &mut samples {
            let level = sample.counters["footprint.IOSurface.dirty"];
            sample
                .counters
                .insert("own.iosurface_alloc_size".to_owned(), level);
        }
        let calibration = calibrate(&samples, &sizes()).unwrap();
        assert_eq!(
            calibration.fit,
            vec!["footprint.IOSurface.dirty".to_owned()]
        );
        assert!(
            calibration
                .rows
                .iter()
                .any(|row| row.own && row.sees_iosurface)
        );
    }
}
