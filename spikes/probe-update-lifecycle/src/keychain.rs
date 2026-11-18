//! Клетка F «Keychain» (задача 14): разовые операции `probe-update-app --keychain` через Security
//! API файловой (legacy) keychain, разборы `security dump-keychain -a`, `security list-keychains`
//! и `security default-keychain`, классификация `OSStatus`, SHA-256 значения и уборка временной
//! keychain после `SIGKILL` процесса сценария.
//!
//! - Временная keychain лежит в `~/Library/Keychains/` под именем с nonce. Вне этого каталога
//!   `security create-keychain` создаёт keychain версии 256, у item которой нет partition list, а
//!   предмет клетки — partition ID `cdhash:` (опыт исполнителя до первого прогона). В список
//!   поиска keychain не добавляется, keychain по умолчанию не меняется.
//! - Каждая операция называет keychain явно: `kSecUseKeychain` при записи, `kSecMatchSearchList`
//!   при чтении. Чтение — сначала запрос атрибутов (item виден в поиске), затем данных: отсутствие
//!   item не путается с отказом по подписи.
//! - Item создаётся с доступом только для собственного кода: `SecAccessCreate` со списком из одного
//!   `SecTrustedApplicationCreateFromPath(NULL)`. Item через CLI `security` не создаётся, partition
//!   list не правится.
//! - Взаимодействие запрещено одним из двух способов [`NoUi`]. Какой действует, решает
//!   отрицательный контроль сценария `keychain`.

use std::collections::BTreeMap;
use std::ffi::{CString, c_char, c_void};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::Instant;

use objc2_core_foundation::{
    CFArray, CFBoolean, CFData, CFDictionary, CFRetained, CFString, CFType, kCFBooleanTrue,
};
use serde::{Deserialize, Serialize};

use crate::sys;

/// Сервис item клетки.
pub const SERVICE: &str = "io.github.dsbasko.shellio.probe.update-lifecycle.f";
/// Account item, который создаёт GUI N.
pub const ACCOUNT_N: &str = "n";
/// Account item отрицательного контроля, который создаёт бандл `.denied`.
pub const ACCOUNT_DENIED: &str = "denied";
/// Префикс имени временной keychain; за ним nonce процесса сценария.
pub const KEYCHAIN_PREFIX: &str = "psu-f-";
pub const KEYCHAIN_SUFFIX: &str = ".keychain-db";
pub const SECURITY: &str = "/usr/bin/security";
/// Версия файловой keychain, у item которой есть partition list.
pub const PARTITIONED_VERSION: u32 = 512;
/// Авторизация записи ACL с partition list.
pub const PARTITION_AUTH: &str = "partition_id";

/// Каталог keychain пользователя.
pub fn keychains_dir(home: &Path) -> PathBuf {
    home.join("Library/Keychains")
}

/// Путь временной keychain процесса сценария с этим nonce.
pub fn keychain_path(home: &Path, nonce: &str) -> PathBuf {
    keychains_dir(home).join(format!("{KEYCHAIN_PREFIX}{nonce}{KEYCHAIN_SUFFIX}"))
}

// --- OSStatus ---

/// Вид итога вызова Security.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    Success,
    /// Item не найден в поиске.
    NotFound,
    /// Отказ доступа или взаимодействия: ACL, partition list, запрет окна.
    Refused,
    Duplicate,
    Other,
}

const STATUSES: [(i32, &str, Class); 15] = [
    (0, "errSecSuccess", Class::Success),
    (-25300, "errSecItemNotFound", Class::NotFound),
    (-25308, "errSecInteractionNotAllowed", Class::Refused),
    (-25293, "errSecAuthFailed", Class::Refused),
    (-128, "errSecUserCanceled", Class::Refused),
    (-25315, "errSecInteractionRequired", Class::Refused),
    (-25243, "errSecNoAccessForItem", Class::Refused),
    (-25299, "errSecDuplicateItem", Class::Duplicate),
    (-25294, "errSecNoSuchKeychain", Class::Other),
    (-25292, "errSecReadOnly", Class::Other),
    (-25291, "errSecNotAvailable", Class::Other),
    (-34018, "errSecMissingEntitlement", Class::Other),
    (-61, "errSecWrPerm", Class::Other),
    (-50, "errSecParam", Class::Other),
    (-4, "errSecUnimplemented", Class::Other),
];

pub fn classify(status: i32) -> Class {
    STATUSES
        .iter()
        .find(|(code, _, _)| *code == status)
        .map_or(Class::Other, |(_, _, class)| *class)
}

/// Имя кода из `SecBase.h`; неизвестный код — число.
pub fn status_name(status: i32) -> String {
    STATUSES
        .iter()
        .find(|(code, _, _)| *code == status)
        .map_or_else(
            || format!("OSStatus {status}"),
            |(_, name, _)| (*name).to_owned(),
        )
}

// --- способ запрета взаимодействия ---

/// Как операция запрещает окно запроса.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NoUi {
    /// `SecKeychainSetUserInteractionAllowed(false)` на процесс до первого вызова.
    ProcessFlag,
    /// `kSecUseAuthenticationUI = kSecUseAuthenticationUIFail` в каждом запросе.
    QueryFail,
}

impl NoUi {
    /// Порядок проверки отрицательным контролем.
    pub const ALL: [NoUi; 2] = [NoUi::ProcessFlag, NoUi::QueryFail];

    pub fn name(self) -> &'static str {
        match self {
            NoUi::ProcessFlag => "process-flag",
            NoUi::QueryFail => "query-fail",
        }
    }

    pub fn parse(text: &str) -> Option<NoUi> {
        NoUi::ALL.into_iter().find(|no_ui| no_ui.name() == text)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Op {
    Add,
    Read,
}

impl Op {
    pub fn name(self) -> &'static str {
        match self {
            Op::Add => "add",
            Op::Read => "read",
        }
    }
}

// --- аргументы разовой операции ---

/// Аргументы `probe-update-app --keychain`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpArgs {
    pub op: Op,
    pub keychain: PathBuf,
    pub account: String,
    pub no_ui: NoUi,
    /// Файл со значением item; только у `add`.
    pub secret_file: Option<PathBuf>,
    /// Куда записать [`OpReport`].
    pub out: PathBuf,
    /// Метка запуска: по ней сценарий и внешний сторож находят процесс.
    pub tag: String,
    pub window: bool,
}

pub const USAGE: &str = "\
       probe-update-app --keychain add|read --keychain-path <keychain> --account <account>
       --no-ui process-flag|query-fail --keychain-out <файл> --tag <метка>
       [--secret-file <файл>] [--window]";

fn absolute(flag: &str, value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(format!("{flag} {value}: нужен абсолютный путь"))
    }
}

impl OpArgs {
    /// Разбор аргументов, первый из которых — `--keychain`.
    pub fn parse(args: &[String]) -> Result<OpArgs, String> {
        let mut slots: BTreeMap<&str, String> = BTreeMap::new();
        let mut window = false;
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            if flag == "--window" {
                window = true;
                continue;
            }
            let name = match flag.as_str() {
                "--keychain" | "--keychain-path" | "--account" | "--no-ui" | "--keychain-out"
                | "--tag" | "--secret-file" => flag.as_str(),
                other => return Err(format!("--keychain: неизвестный аргумент «{other}»")),
            };
            let value = rest.next().ok_or_else(|| format!("{flag}: нет значения"))?;
            if slots.insert(name, value.clone()).is_some() {
                return Err(format!("{flag} задан дважды"));
            }
        }
        let mut take = |name: &str| slots.remove(name);
        let op = match take("--keychain").as_deref() {
            Some("add") => Op::Add,
            Some("read") => Op::Read,
            Some(other) => return Err(format!("--keychain {other}: ожидается add или read")),
            None => return Err("нет --keychain".to_owned()),
        };
        let keychain = absolute(
            "--keychain-path",
            &take("--keychain-path").ok_or("нет --keychain-path")?,
        )?;
        let account = take("--account").ok_or("нет --account")?;
        if account.is_empty() {
            return Err("--account пуст".to_owned());
        }
        let no_ui_text = take("--no-ui").ok_or("нет --no-ui")?;
        let no_ui = NoUi::parse(&no_ui_text).ok_or_else(|| {
            format!("--no-ui {no_ui_text}: ожидается process-flag или query-fail")
        })?;
        let out = absolute(
            "--keychain-out",
            &take("--keychain-out").ok_or("нет --keychain-out")?,
        )?;
        let tag = take("--tag").ok_or("нет --tag")?;
        if tag.is_empty() {
            return Err("--tag пуст".to_owned());
        }
        let secret_file = take("--secret-file")
            .map(|file| absolute("--secret-file", &file))
            .transpose()?;
        match (op, &secret_file) {
            (Op::Add, None) => return Err("add: нет --secret-file".to_owned()),
            (Op::Read, Some(_)) => return Err("read: --secret-file не нужен".to_owned()),
            _ => {}
        }
        Ok(OpArgs {
            op,
            keychain,
            account,
            no_ui,
            secret_file,
            out,
            tag,
            window,
        })
    }

    pub fn to_args(&self) -> Vec<String> {
        let mut args = vec![
            "--keychain".to_owned(),
            self.op.name().to_owned(),
            "--keychain-path".to_owned(),
            self.keychain.display().to_string(),
            "--account".to_owned(),
            self.account.clone(),
            "--no-ui".to_owned(),
            self.no_ui.name().to_owned(),
            "--keychain-out".to_owned(),
            self.out.display().to_string(),
            "--tag".to_owned(),
            self.tag.clone(),
        ];
        if let Some(file) = &self.secret_file {
            args.extend(["--secret-file".to_owned(), file.display().to_string()]);
        }
        if self.window {
            args.push("--window".to_owned());
        }
        args
    }
}

// --- итог разовой операции ---

/// Итог `probe-update-app --keychain`: кто выполнил операцию и что ответил Security.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpReport {
    pub op: Op,
    pub account: String,
    pub no_ui: NoUi,
    pub pid: i32,
    /// Поколение сборки исполняемого файла.
    pub generation: u32,
    /// CDHash процесса из ядра (`csops`).
    pub cdhash: Option<String>,
    /// Исполняемый файл по `proc_pidpath`.
    pub executable: Option<String>,
    pub window: bool,
    /// `SecKeychainGetUserInteractionAllowed` до и после запрета.
    pub interaction_before: Option<bool>,
    pub interaction_after: Option<bool>,
    /// `SecKeychainSetUserInteractionAllowed(false)`, если способ — флаг процесса.
    pub set_interaction_status: Option<i32>,
    pub open_status: i32,
    /// Биты `SecKeychainGetStatus`: 1 — разблокирована, 2 — чтение, 4 — запись.
    pub keychain_status: Option<u32>,
    pub add_status: Option<i32>,
    /// Запрос только атрибутов: item виден в поиске.
    pub attributes_status: Option<i32>,
    /// Запрос данных.
    pub data_status: Option<i32>,
    /// SHA-256 записанного (`add`) или прочитанного (`read`) значения.
    pub value_sha256: Option<String>,
    pub value_len: Option<usize>,
    pub elapsed_ms: u64,
    pub error: Option<String>,
}

impl OpReport {
    /// Итог вызова, по которому судит сценарий: запись у `add`, данные у `read`.
    pub fn status(&self) -> Option<i32> {
        match self.op {
            Op::Add => self.add_status,
            Op::Read => self.data_status,
        }
    }
}

// --- Security и CommonCrypto ---

type OSStatus = i32;
type Raw = *mut c_void;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    static kSecClass: &'static CFString;
    static kSecClassGenericPassword: &'static CFString;
    static kSecAttrService: &'static CFString;
    static kSecAttrAccount: &'static CFString;
    static kSecAttrLabel: &'static CFString;
    static kSecAttrAccess: &'static CFString;
    static kSecValueData: &'static CFString;
    static kSecUseKeychain: &'static CFString;
    static kSecMatchSearchList: &'static CFString;
    static kSecMatchLimit: &'static CFString;
    static kSecMatchLimitOne: &'static CFString;
    static kSecReturnData: &'static CFString;
    static kSecReturnAttributes: &'static CFString;
    static kSecUseAuthenticationUI: &'static CFString;
    static kSecUseAuthenticationUIFail: &'static CFString;

    fn SecKeychainOpen(path: *const c_char, keychain: *mut Raw) -> OSStatus;
    fn SecKeychainGetStatus(keychain: *const c_void, status: *mut u32) -> OSStatus;
    fn SecKeychainSetUserInteractionAllowed(state: u8) -> OSStatus;
    fn SecKeychainGetUserInteractionAllowed(state: *mut u8) -> OSStatus;
    fn SecTrustedApplicationCreateFromPath(path: *const c_char, app: *mut Raw) -> OSStatus;
    fn SecAccessCreate(
        descriptor: *const c_void,
        trusted: *const c_void,
        access: *mut Raw,
    ) -> OSStatus;
    fn SecItemAdd(attributes: *const c_void, result: *mut Raw) -> OSStatus;
    fn SecItemCopyMatching(query: *const c_void, result: *mut Raw) -> OSStatus;
}

unsafe extern "C" {
    fn CC_SHA256(data: *const c_void, len: u32, md: *mut u8) -> *mut u8;
}

/// SHA-256 строчными шестнадцатеричными знаками; `None` — данные длиннее `u32::MAX`.
pub fn sha256_hex(bytes: &[u8]) -> Option<String> {
    let len = u32::try_from(bytes.len()).ok()?;
    let mut digest = [0u8; 32];
    // SAFETY: срез и буфер на 32 байта живут на время вызова.
    unsafe { CC_SHA256(bytes.as_ptr().cast(), len, digest.as_mut_ptr()) };
    Some(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// # Safety
/// `raw` — объект Core Foundation с правом владения или NULL.
unsafe fn owned(raw: Raw) -> Option<CFRetained<CFType>> {
    // SAFETY: по контракту функции.
    NonNull::new(raw.cast::<CFType>()).map(|ptr| unsafe { CFRetained::from_raw(ptr) })
}

fn ptr_of<T: ?Sized>(value: &T) -> *const c_void {
    std::ptr::from_ref(value).cast()
}

fn c_path(path: &Path) -> Result<CString, String> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("{}: нулевой байт в пути", path.display()))
}

fn interaction_allowed() -> Option<bool> {
    let mut state = 0u8;
    // SAFETY: `state` живёт на время вызова.
    let status = unsafe { SecKeychainGetUserInteractionAllowed(&mut state) };
    (status == 0).then_some(state != 0)
}

fn true_value() -> Result<&'static CFBoolean, String> {
    // SAFETY: чтение константы Core Foundation.
    unsafe { kCFBooleanTrue }.ok_or_else(|| "kCFBooleanTrue недоступен".to_owned())
}

/// Пара ключа и значения словаря запроса: аргументы функции приводят `&CFString` и другие типы
/// CF к `&CFType`.
fn pair<'a>(key: &'a CFString, value: &'a CFType) -> (&'a CFString, &'a CFType) {
    (key, value)
}

fn dictionary(pairs: &[(&CFString, &CFType)]) -> CFRetained<CFDictionary<CFString, CFType>> {
    let keys: Vec<&CFString> = pairs.iter().map(|(key, _)| *key).collect();
    let values: Vec<&CFType> = pairs.iter().map(|(_, value)| *value).collect();
    CFDictionary::from_slices(&keys, &values)
}

/// Доступ только для вызывающего кода.
fn own_access(label: &CFString) -> Result<CFRetained<CFType>, String> {
    let mut app: Raw = std::ptr::null_mut();
    // SAFETY: NULL-путь — вызывающее приложение; `app` получает объект с правом владения.
    let status = unsafe { SecTrustedApplicationCreateFromPath(std::ptr::null(), &mut app) };
    // SAFETY: Create-функция отдаёт объект с правом владения.
    let app = match (status, unsafe { owned(app) }) {
        (0, Some(app)) => app,
        (status, _) => {
            return Err(format!(
                "SecTrustedApplicationCreateFromPath: {}",
                status_name(status)
            ));
        }
    };
    let trusted = CFArray::<CFType>::from_objects(&[&*app]);
    let mut access: Raw = std::ptr::null_mut();
    // SAFETY: описание и список — живые объекты CF; `access` получает объект с правом владения.
    let status = unsafe { SecAccessCreate(ptr_of(label), ptr_of(&*trusted), &mut access) };
    // SAFETY: Create-функция отдаёт объект с правом владения.
    match (status, unsafe { owned(access) }) {
        (0, Some(access)) => Ok(access),
        (status, _) => Err(format!("SecAccessCreate: {}", status_name(status))),
    }
}

/// Разовая операция: запрет взаимодействия, открытие keychain, запись или чтение.
pub fn perform(args: &OpArgs) -> OpReport {
    let started = Instant::now();
    let pid = std::process::id() as i32;
    let mut report = OpReport {
        op: args.op,
        account: args.account.clone(),
        no_ui: args.no_ui,
        pid,
        generation: crate::GENERATION,
        cdhash: sys::process_cdhash(pid).ok(),
        executable: sys::process_path(pid)
            .ok()
            .map(|path| path.display().to_string()),
        window: args.window,
        interaction_before: interaction_allowed(),
        interaction_after: None,
        set_interaction_status: None,
        open_status: 0,
        keychain_status: None,
        add_status: None,
        attributes_status: None,
        data_status: None,
        value_sha256: None,
        value_len: None,
        elapsed_ms: 0,
        error: None,
    };
    if let Err(reason) = run_op(args, &mut report) {
        report.error = Some(reason);
    }
    report.elapsed_ms = started.elapsed().as_millis() as u64;
    report
}

fn run_op(args: &OpArgs, report: &mut OpReport) -> Result<(), String> {
    if args.no_ui == NoUi::ProcessFlag {
        // SAFETY: вызов без указателей.
        report.set_interaction_status = Some(unsafe { SecKeychainSetUserInteractionAllowed(0) });
    }
    report.interaction_after = interaction_allowed();
    let path = c_path(&args.keychain)?;
    let mut raw: Raw = std::ptr::null_mut();
    // SAFETY: путь оканчивается нулём, `raw` получает объект с правом владения.
    report.open_status = unsafe { SecKeychainOpen(path.as_ptr(), &mut raw) };
    // SAFETY: Open-функция отдаёт объект с правом владения.
    let keychain = match (report.open_status, unsafe { owned(raw) }) {
        (0, Some(keychain)) => keychain,
        (status, _) => return Err(format!("SecKeychainOpen: {}", status_name(status))),
    };
    let mut bits = 0u32;
    // SAFETY: keychain — живой объект, `bits` живёт на время вызова.
    if unsafe { SecKeychainGetStatus(ptr_of(&*keychain), &mut bits) } == 0 {
        report.keychain_status = Some(bits);
    }
    // SAFETY: чтение констант Security.
    let (class, generic, service_key, account_key) = unsafe {
        (
            kSecClass,
            kSecClassGenericPassword,
            kSecAttrService,
            kSecAttrAccount,
        )
    };
    let service = CFString::from_str(SERVICE);
    let account = CFString::from_str(&args.account);
    match args.op {
        Op::Add => {
            let file = args.secret_file.as_ref().ok_or("add без --secret-file")?;
            let secret = fs::read(file).map_err(|err| format!("{}: {err}", file.display()))?;
            report.value_sha256 = sha256_hex(&secret);
            report.value_len = Some(secret.len());
            let label = CFString::from_str(&format!("{SERVICE} {}", args.account));
            let access = own_access(&label)?;
            let data = CFData::from_bytes(&secret);
            // SAFETY: чтение констант Security.
            let (label_key, access_key, data_key, use_keychain) = unsafe {
                (
                    kSecAttrLabel,
                    kSecAttrAccess,
                    kSecValueData,
                    kSecUseKeychain,
                )
            };
            let mut pairs = vec![
                pair(class, generic),
                pair(service_key, &service),
                pair(account_key, &account),
                pair(label_key, &label),
                pair(data_key, &data),
                pair(use_keychain, &keychain),
                pair(access_key, &access),
            ];
            if args.no_ui == NoUi::QueryFail {
                // SAFETY: чтение констант Security.
                let (key, value) =
                    unsafe { (kSecUseAuthenticationUI, kSecUseAuthenticationUIFail) };
                pairs.push(pair(key, value));
            }
            let attributes = dictionary(&pairs);
            // SAFETY: словарь жив на время вызова, результат не запрашивается.
            report.add_status =
                Some(unsafe { SecItemAdd(ptr_of(&*attributes), std::ptr::null_mut()) });
        }
        Op::Read => {
            let list = CFArray::<CFType>::from_objects(&[&*keychain]);
            let truth = true_value()?;
            // SAFETY: чтение констант Security.
            let (search, limit, one, return_attributes, return_data) = unsafe {
                (
                    kSecMatchSearchList,
                    kSecMatchLimit,
                    kSecMatchLimitOne,
                    kSecReturnAttributes,
                    kSecReturnData,
                )
            };
            let base = vec![
                pair(class, generic),
                pair(service_key, &service),
                pair(account_key, &account),
                pair(search, &list),
                pair(limit, one),
            ];
            // SAFETY: чтение констант Security.
            let (ui_key, ui_fail) =
                unsafe { (kSecUseAuthenticationUI, kSecUseAuthenticationUIFail) };
            let mut base = base;
            if args.no_ui == NoUi::QueryFail {
                base.push(pair(ui_key, ui_fail));
            }
            let mut attributes_query = base.clone();
            attributes_query.push(pair(return_attributes, truth));
            let query = dictionary(&attributes_query);
            let mut found: Raw = std::ptr::null_mut();
            // SAFETY: запрос жив на время вызова, `found` получает объект с правом владения.
            let status = unsafe { SecItemCopyMatching(ptr_of(&*query), &mut found) };
            // SAFETY: Copy-функция отдаёт объект с правом владения.
            drop(unsafe { owned(found) });
            report.attributes_status = Some(status);
            let mut data_query = base;
            data_query.push(pair(return_data, truth));
            let query = dictionary(&data_query);
            let mut found: Raw = std::ptr::null_mut();
            // SAFETY: запрос жив на время вызова, `found` получает объект с правом владения.
            let status = unsafe { SecItemCopyMatching(ptr_of(&*query), &mut found) };
            report.data_status = Some(status);
            // SAFETY: Copy-функция отдаёт объект с правом владения.
            if let Some(value) = unsafe { owned(found) } {
                let data = value
                    .downcast_ref::<CFData>()
                    .ok_or("SecItemCopyMatching вернул не CFData")?
                    .to_vec();
                report.value_sha256 = sha256_hex(&data);
                report.value_len = Some(data.len());
            }
        }
    }
    Ok(())
}

/// Запись итога через временный файл и `rename`: сценарий не читает половину.
pub fn write_report(path: &Path, report: &OpReport) -> Result<(), String> {
    let text = serde_json::to_string_pretty(report).map_err(|err| err.to_string())?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, format!("{text}\n")).map_err(|err| format!("{}: {err}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|err| format!("{}: {err}", path.display()))
}

pub fn read_report(path: &Path) -> Result<OpReport, String> {
    let text = fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
    serde_json::from_str(&text).map_err(|err| format!("{}: {err}", path.display()))
}

// --- разборы вывода `security` ---

/// Строки `security list-keychains -d user` и `security default-keychain -d user`: пути без
/// кавычек и отступов.
pub fn parse_keychain_list(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.strip_prefix('"')
                .and_then(|line| line.strip_suffix('"'))
                .unwrap_or(line)
                .to_owned()
        })
        .collect()
}

/// Приложение записи ACL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AclApp {
    pub path: String,
    /// Отметка проверки рядом с путём, например `OK`.
    pub mark: Option<String>,
    pub requirement: Option<String>,
}

/// Запись ACL item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AclEntry {
    pub index: usize,
    pub authorizations: Vec<String>,
    /// Строки без ключа, например `don't-require-password`.
    pub flags: Vec<String>,
    pub description: Option<String>,
    /// `None` — `applications: <null>` (любое приложение), иначе список.
    pub applications: Option<Vec<AclApp>>,
}

/// Item из `security dump-keychain -a`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DumpItem {
    pub keychain: String,
    pub version: Option<u32>,
    pub class: Option<String>,
    /// Именованные атрибуты в кавычках (`acct`, `svce`, `mdat` …) с сырым значением.
    pub attributes: BTreeMap<String, String>,
    pub access: Vec<AclEntry>,
    /// Строки, которые разбор не узнал.
    pub unparsed: Vec<String>,
}

impl DumpItem {
    fn quoted(&self, name: &str) -> Option<String> {
        let raw = self.attributes.get(name)?;
        let value = raw.split_once("=\"")?.1;
        Some(value.strip_suffix('"').unwrap_or(value).to_owned())
    }

    pub fn account(&self) -> Option<String> {
        self.quoted("acct")
    }

    pub fn service(&self) -> Option<String> {
        self.quoted("svce")
    }

    /// Partition list: описание записи с авторизацией `partition_id`; `None` — записи нет.
    pub fn partitions(&self) -> Option<Vec<String>> {
        self.access
            .iter()
            .find(|entry| {
                entry
                    .authorizations
                    .iter()
                    .any(|auth| auth == PARTITION_AUTH)
            })
            .map(|entry| {
                entry
                    .description
                    .as_deref()
                    .unwrap_or("")
                    .split(',')
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
    }

    /// Записи ACL без отметок приложений: отметку `security dump-keychain` вычисляет при выводе,
    /// проверяя код, который лежит по пути приложения сейчас, и в item она не хранится.
    pub fn stored_access(&self) -> Vec<AclEntry> {
        let mut access = self.access.clone();
        for entry in &mut access {
            for app in entry.applications.iter_mut().flatten() {
                app.mark = None;
            }
        }
        access
    }

    /// Отметки проверки путей приложений ACL в порядке записей.
    pub fn app_marks(&self) -> Vec<String> {
        self.access
            .iter()
            .flat_map(|entry| entry.applications.iter().flatten())
            .map(|app| app.mark.clone().unwrap_or_else(|| "—".to_owned()))
            .collect()
    }

    /// Требования приложений записи, которой разрешено `decrypt` (чтение значения).
    pub fn decrypt_requirements(&self) -> Option<Vec<String>> {
        let entry = self
            .access
            .iter()
            .find(|entry| entry.authorizations.iter().any(|auth| auth == "decrypt"))?;
        Some(
            entry
                .applications
                .as_ref()?
                .iter()
                .filter_map(|app| app.requirement.clone())
                .collect(),
        )
    }
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Разбор `security dump-keychain -a`: item по строкам `keychain:`, атрибуты и записи ACL.
pub fn parse_dump(text: &str) -> Result<Vec<DumpItem>, String> {
    #[derive(PartialEq)]
    enum Section {
        Head,
        Attributes,
        Access,
    }
    let mut items: Vec<DumpItem> = Vec::new();
    let mut section = Section::Head;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("keychain: ") {
            items.push(DumpItem {
                keychain: rest.trim().trim_matches('"').to_owned(),
                version: None,
                class: None,
                attributes: BTreeMap::new(),
                access: Vec::new(),
                unparsed: Vec::new(),
            });
            section = Section::Head;
            continue;
        }
        let item = items
            .last_mut()
            .ok_or_else(|| format!("строка до первой «keychain:»: {line}"))?;
        if indent(line) == 0 {
            if let Some(rest) = line.strip_prefix("version: ") {
                item.version = Some(
                    rest.trim()
                        .parse()
                        .map_err(|_| format!("версия keychain «{rest}»"))?,
                );
            } else if let Some(rest) = line.strip_prefix("class: ") {
                item.class = Some(rest.trim().trim_matches('"').to_owned());
            } else if line.starts_with("attributes:") {
                section = Section::Attributes;
            } else if line.starts_with("access:") {
                section = Section::Access;
            } else {
                item.unparsed.push(line.to_owned());
            }
            continue;
        }
        let trimmed = line.trim();
        match section {
            Section::Head => item.unparsed.push(line.to_owned()),
            Section::Attributes => {
                if let Some(rest) = trimmed.strip_prefix('"')
                    && let Some((name, value)) = rest.split_once('"')
                {
                    item.attributes.insert(name.to_owned(), value.to_owned());
                } else if !trimmed.starts_with("0x") {
                    item.unparsed.push(line.to_owned());
                }
            }
            Section::Access => parse_access_line(item, line, trimmed)?,
        }
    }
    Ok(items)
}

fn parse_access_line(item: &mut DumpItem, line: &str, trimmed: &str) -> Result<(), String> {
    if let Some(rest) = trimmed.strip_prefix("entry ") {
        let index = rest
            .trim_end_matches(':')
            .parse()
            .map_err(|_| format!("номер записи ACL «{trimmed}»"))?;
        item.access.push(AclEntry {
            index,
            authorizations: Vec::new(),
            flags: Vec::new(),
            description: None,
            applications: None,
        });
        return Ok(());
    }
    let Some(entry) = item.access.last_mut() else {
        item.unparsed.push(line.to_owned());
        return Ok(());
    };
    if let Some(rest) = trimmed.strip_prefix("authorizations") {
        let list = rest.split_once(':').map_or("", |(_, list)| list);
        entry.authorizations = list.split_whitespace().map(str::to_owned).collect();
    } else if let Some(rest) = trimmed.strip_prefix("description: ") {
        entry.description = Some(rest.to_owned());
    } else if trimmed == "description:" {
        entry.description = Some(String::new());
    } else if let Some(rest) = trimmed.strip_prefix("applications") {
        entry.applications = if rest.trim() == ": <null>" {
            None
        } else {
            Some(Vec::new())
        };
    } else if let Some(rest) = trimmed.strip_prefix("requirement: ") {
        match entry.applications.as_mut().and_then(|apps| apps.last_mut()) {
            Some(app) => app.requirement = Some(rest.to_owned()),
            None => item.unparsed.push(line.to_owned()),
        }
    } else if let Some(apps) = entry.applications.as_mut()
        && let Some((number, rest)) = trimmed.split_once(": ")
        && number.chars().all(|c| c.is_ascii_digit())
    {
        let (path, mark) = match rest.rsplit_once(" (") {
            Some((path, mark)) if mark.ends_with(')') => {
                (path.to_owned(), Some(mark.trim_end_matches(')').to_owned()))
            }
            _ => (rest.to_owned(), None),
        };
        apps.push(AclApp {
            path,
            mark,
            requirement: None,
        });
    } else if !trimmed.contains(": ") {
        entry.flags.push(trimmed.to_owned());
    } else {
        item.unparsed.push(line.to_owned());
    }
    Ok(())
}

/// Item по сервису и account в разобранном выводе.
pub fn find_item<'a>(items: &'a [DumpItem], service: &str, account: &str) -> Option<&'a DumpItem> {
    items.iter().find(|item| {
        item.service().as_deref() == Some(service) && item.account().as_deref() == Some(account)
    })
}

// --- уборка ---

/// Удаляет временную keychain процесса сценария, если она осталась: после `SIGKILL` процесса
/// сценария это делает внешний запуск. Возвращает описания найденных keychain.
pub fn sweep(home: &Path, nonce: &str) -> Vec<String> {
    let path = keychain_path(home, nonce);
    if !path.exists() {
        return Vec::new();
    }
    let outcome = match crate::spawn::output(
        Path::new(SECURITY),
        &["delete-keychain".to_owned(), path.display().to_string()],
    ) {
        Ok(output) if output.exit.success() => "удалена внешней уборкой".to_owned(),
        Ok(output) => format!(
            "не удалена: security delete-keychain {}; {}",
            output.exit,
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(err) => format!("не удалена: security: {err}"),
    };
    if path.exists() {
        let _ = fs::remove_file(&path);
    }
    vec![format!("keychain {} {outcome}", path.display())]
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP_512: &str = include_str!("testdata/security/dump-512.txt");
    const DUMP_256: &str = include_str!("testdata/security/dump-256.txt");
    const LIST: &str = include_str!("testdata/security/list-keychains.txt");
    const DEFAULT: &str = include_str!("testdata/security/default-keychain.txt");

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn statuses_classify_by_kind() {
        assert_eq!(classify(0), Class::Success);
        assert_eq!(classify(-25300), Class::NotFound);
        for refused in [-25308, -25293, -128, -25315, -25243] {
            assert_eq!(classify(refused), Class::Refused, "{refused}");
        }
        assert_eq!(classify(-25299), Class::Duplicate);
        assert_eq!(classify(-50), Class::Other);
        assert_eq!(classify(-34018), Class::Other);
        assert_eq!(classify(12345), Class::Other);
        assert_eq!(status_name(-25308), "errSecInteractionNotAllowed");
        assert_eq!(status_name(12345), "OSStatus 12345");
        let mut codes: Vec<i32> = STATUSES.iter().map(|(code, _, _)| *code).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), STATUSES.len(), "коды не повторяются");
    }

    #[test]
    fn keychain_lists_lose_quotes_and_indent() {
        let list = parse_keychain_list(LIST);
        assert_eq!(
            list,
            strings(&["/Users/u/Library/Keychains/login.keychain-db"])
        );
        assert_eq!(parse_keychain_list(DEFAULT), list);
        assert_eq!(
            parse_keychain_list("    \"/a b/x.keychain-db\"\n\n    \"/y\"\n"),
            strings(&["/a b/x.keychain-db", "/y"])
        );
        assert!(parse_keychain_list("").is_empty());
    }

    #[test]
    fn dump_of_partitioned_keychain_has_partition_entries() {
        let items = parse_dump(DUMP_512).unwrap();
        assert_eq!(items.len(), 2);
        let n = find_item(&items, "probe.f", "n").unwrap();
        assert_eq!(n.version, Some(PARTITIONED_VERSION));
        assert_eq!(n.class.as_deref(), Some("genp"));
        assert_eq!(n.service().as_deref(), Some("probe.f"));
        assert_eq!(n.access.len(), 5);
        assert!(n.unparsed.is_empty(), "{:?}", n.unparsed);
        assert_eq!(
            n.partitions(),
            Some(strings(&[
                "cdhash:0123456789abcdef0123456789abcdef01234567",
                "teamid:ABCDE12345"
            ]))
        );
        assert_eq!(
            n.decrypt_requirements(),
            Some(strings(&[
                "identifier \"com.apple.security\" and anchor apple"
            ]))
        );
        let decrypt = &n.access[0];
        assert_eq!(
            decrypt.authorizations,
            strings(&[
                "decrypt",
                "derive",
                "export_clear",
                "export_wrapped",
                "mac",
                "sign"
            ])
        );
        assert_eq!(decrypt.flags, strings(&["don't-require-password"]));
        let apps = decrypt.applications.as_ref().unwrap();
        assert_eq!(apps[0].path, "/usr/bin/security");
        assert_eq!(apps[0].mark.as_deref(), Some("OK"));
        assert_eq!(n.access[1].applications, None);
        assert_eq!(n.access[4].applications, Some(Vec::new()));
        let denied = find_item(&items, "probe.f", "denied").unwrap();
        assert_eq!(denied.partitions(), Some(strings(&["apple-tool:"])));
        assert_eq!(denied.decrypt_requirements().unwrap().len(), 2);
        assert_ne!(n.access, denied.access);
        assert_eq!(n.app_marks(), strings(&["OK"]));
        assert!(
            n.stored_access()
                .iter()
                .flat_map(|entry| entry.applications.iter().flatten())
                .all(|app| app.mark.is_none())
        );
    }

    #[test]
    fn dump_of_unpartitioned_keychain_has_no_partition_list() {
        let items = parse_dump(DUMP_256).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].version, Some(256));
        assert_eq!(items[0].partitions(), None);
        assert!(items[0].unparsed.is_empty(), "{:?}", items[0].unparsed);
        assert!(find_item(&items, "probe.f", "n").is_some());
        assert!(find_item(&items, SERVICE, "n").is_none());
    }

    #[test]
    fn dump_rejects_text_before_first_item() {
        assert!(parse_dump("version: 512\n").is_err());
        assert!(parse_dump("").unwrap().is_empty());
        let unknown = "keychain: \"/k\"\nversion: 512\nweird: 1\naccess: 1 entries\n    entry 0:\n        something odd: x\n";
        let items = parse_dump(unknown).unwrap();
        assert_eq!(items[0].unparsed.len(), 2);
        assert!(parse_dump("keychain: \"/k\"\nversion: x\n").is_err());
    }

    #[test]
    fn op_args_round_trip_and_reject_mistakes() {
        let add = OpArgs {
            op: Op::Add,
            keychain: PathBuf::from("/k/psu-f-1.keychain-db"),
            account: ACCOUNT_N.to_owned(),
            no_ui: NoUi::ProcessFlag,
            secret_file: Some(PathBuf::from("/s/secret")),
            out: PathBuf::from("/o/n-add.json"),
            tag: "nonce.n-add".to_owned(),
            window: true,
        };
        assert_eq!(OpArgs::parse(&add.to_args()), Ok(add.clone()));
        let read = OpArgs {
            op: Op::Read,
            no_ui: NoUi::QueryFail,
            secret_file: None,
            window: false,
            ..add.clone()
        };
        assert_eq!(OpArgs::parse(&read.to_args()), Ok(read.clone()));
        let mut no_secret = add.clone();
        no_secret.secret_file = None;
        assert!(OpArgs::parse(&no_secret.to_args()).is_err());
        let mut extra = read.to_args();
        extra.extend(strings(&["--secret-file", "/s"]));
        assert!(OpArgs::parse(&extra).is_err());
        let mut twice = read.to_args();
        twice.extend(strings(&["--tag", "again"]));
        assert!(OpArgs::parse(&twice).unwrap_err().contains("дважды"));
        let relative = read
            .to_args()
            .iter()
            .map(|arg| arg.replace("/k/", "k/"))
            .collect::<Vec<_>>();
        assert!(OpArgs::parse(&relative).is_err());
        let bad_ui = read
            .to_args()
            .iter()
            .map(|arg| arg.replace("query-fail", "none"))
            .collect::<Vec<_>>();
        assert!(OpArgs::parse(&bad_ui).is_err());
        assert!(OpArgs::parse(&strings(&["--keychain", "delete"])).is_err());
    }

    #[test]
    fn no_ui_names_round_trip() {
        for no_ui in NoUi::ALL {
            assert_eq!(NoUi::parse(no_ui.name()), Some(no_ui));
        }
        assert_eq!(NoUi::parse("x"), None);
    }

    #[test]
    fn sha256_matches_known_digest() {
        assert_eq!(
            sha256_hex(b"abc").as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
    }

    #[test]
    fn report_status_follows_operation() {
        let mut report = OpReport {
            op: Op::Read,
            account: ACCOUNT_N.to_owned(),
            no_ui: NoUi::ProcessFlag,
            pid: 1,
            generation: 2,
            cdhash: None,
            executable: None,
            window: true,
            interaction_before: Some(true),
            interaction_after: Some(false),
            set_interaction_status: Some(0),
            open_status: 0,
            keychain_status: Some(7),
            add_status: Some(-25299),
            attributes_status: Some(0),
            data_status: Some(-25308),
            value_sha256: None,
            value_len: None,
            elapsed_ms: 1,
            error: None,
        };
        assert_eq!(report.status(), Some(-25308));
        report.op = Op::Add;
        assert_eq!(report.status(), Some(-25299));
        let dir = std::env::temp_dir().join(format!("psu-kc-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.json");
        write_report(&path, &report).unwrap();
        assert_eq!(read_report(&path), Ok(report));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn keychain_path_carries_nonce() {
        let path = keychain_path(Path::new("/Users/u"), "0011");
        assert_eq!(
            path,
            PathBuf::from("/Users/u/Library/Keychains/psu-f-0011.keychain-db")
        );
        assert!(sweep(Path::new("/nonexistent-home"), "0011").is_empty());
    }
}
