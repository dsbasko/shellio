//! Поток ScreenCaptureKit окна пробы — запасной путь замера кадров по правилу «Способ замера
//! кадров». Поток живёт на рабочем потоке стенда, кадры приходят на свою последовательную очередь
//! dispatch, не на главный поток: отрезок `stall` занимает главный поток, и наблюдатель сам терял бы
//! кадры. Обработчик кадра только читает маркер поколения ([`super::marker`]) и пишет журнал
//! доставки: статус, `displayTime`, вход и длительность обработчика.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_core_graphics::CGPreflightScreenCaptureAccess;
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags,
    CVPixelBufferUnlockBaseAddress, kCVPixelFormatType_32BGRA,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSNumber, NSObject, NSObjectProtocol, NSString,
};
use objc2_screen_capture_kit::{
    SCContentFilter, SCFrameStatus, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamFrameInfoDisplayTime, SCStreamFrameInfoStatus, SCStreamOutput, SCStreamOutputType,
};
use serde::Serialize;

use super::marker;
use super::sampler;

/// Сроки запуска и остановки потока, записаны в отчёт до первого прогона.
pub const START_TIMEOUT: Duration = Duration::from_secs(10);
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Глубина очереди кадров потока — наибольшая допустимая.
pub const QUEUE_DEPTH: isize = 8;

/// Доставка одного кадра потока.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Sample {
    /// `SCFrameStatus`: 0 — кадр с новым содержимым, 1 — простой без изображения.
    pub status: isize,
    /// `displayTime`, наносекунды `CLOCK_UPTIME_RAW`.
    pub display_ns: u64,
    pub entry_ns: u64,
    pub handler_ns: u64,
    /// Поколение по маркеру у кадра со статусом `Complete`.
    pub marker: Option<Result<u16, String>>,
}

/// Наносекунды по тикам `mach_absolute_time`.
pub fn mach_to_ns(ticks: u64, numer: u32, denom: u32) -> u64 {
    if denom == 0 {
        return ticks;
    }
    (u128::from(ticks) * u128::from(numer) / u128::from(denom)) as u64
}

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
}

fn timebase() -> (u32, u32) {
    let mut info = Timebase { numer: 0, denom: 0 };
    // SAFETY: функция заполняет переданную структуру.
    let status = unsafe { mach_timebase_info(&mut info) };
    if status == 0 {
        (info.numer, info.denom)
    } else {
        (1, 1)
    }
}

pub struct OutputIvars {
    samples: Arc<Mutex<Vec<Sample>>>,
    /// Файл для первого кадра `Complete`: снимок кадра потока, BGRA с заголовком размеров.
    first_frame: Mutex<Option<PathBuf>>,
    window_w: f64,
    window_h: f64,
    numer: u32,
    denom: u32,
}

define_class!(
    /// Получатель кадров потока.
    #[unsafe(super(NSObject))]
    #[name = "ProbeDiffStreamOutput"]
    #[ivars = OutputIvars]
    pub struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn did_output(
            &self,
            _stream: &SCStream,
            buffer: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            let entry_ns = sampler::uptime_ns().unwrap_or(0);
            let ivars = self.ivars();
            let (status, display_mach) = frame_info(buffer).unwrap_or((-1, 0));
            let marker = (status == SCFrameStatus::Complete.0).then(|| {
                let dump = ivars
                    .first_frame
                    .lock()
                    .ok()
                    .and_then(|mut path| path.take());
                if let Some(path) = &dump {
                    let _ = std::fs::write(path.with_extension("info.txt"), describe(buffer));
                }
                read_marker(buffer, ivars.window_w, ivars.window_h, dump.as_deref())
            });
            let sample = Sample {
                status,
                display_ns: mach_to_ns(display_mach, ivars.numer, ivars.denom),
                entry_ns,
                handler_ns: sampler::uptime_ns().unwrap_or(0).saturating_sub(entry_ns),
                marker,
            };
            if let Ok(mut samples) = ivars.samples.lock() {
                samples.push(sample);
            }
        }
    }
);

/// Статус и `displayTime` (тики mach) из вложений кадра.
fn frame_info(buffer: &CMSampleBuffer) -> Option<(isize, u64)> {
    // SAFETY: буфер действителен на время вызова, массив вложений не создаётся.
    let array = unsafe { buffer.sample_attachments_array(false) }?;
    // SAFETY: CFArray вложений бесшовно соединён с NSArray словарей.
    let array: &NSArray<NSDictionary<NSString, AnyObject>> =
        unsafe { &*(&*array as *const _ as *const NSArray<NSDictionary<NSString, AnyObject>>) };
    let info = array.firstObject()?;
    let number = |key: &NSString| -> Option<Retained<NSNumber>> {
        info.objectForKey(key)?.downcast::<NSNumber>().ok()
    };
    // SAFETY: ключи — постоянные строки ScreenCaptureKit.
    let (status_key, time_key) = unsafe { (SCStreamFrameInfoStatus, SCStreamFrameInfoDisplayTime) };
    let status = number(status_key)?.integerValue();
    let display = number(time_key).map_or(0, |value| value.unsignedLongLongValue());
    Some((status, display))
}

/// Вложения кадра текстом для снимка первого кадра.
fn describe(buffer: &CMSampleBuffer) -> String {
    // SAFETY: буфер действителен на время вызова, массив вложений не создаётся.
    let Some(array) = (unsafe { buffer.sample_attachments_array(false) }) else {
        return "нет вложений".to_owned();
    };
    // SAFETY: CFArray вложений бесшовно соединён с NSArray.
    let array: &NSArray<NSObject> = unsafe { &*(&*array as *const _ as *const NSArray<NSObject>) };
    array
        .firstObject()
        .map(|info| {
            // SAFETY: `description` у NSObject возвращает строку.
            let text: Retained<NSString> = unsafe { msg_send![&*info, description] };
            text.to_string()
        })
        .unwrap_or_default()
}

fn read_marker(
    buffer: &CMSampleBuffer,
    window_w: f64,
    window_h: f64,
    dump: Option<&Path>,
) -> Result<u16, String> {
    // SAFETY: буфер действителен на время вызова.
    let image = unsafe { buffer.image_buffer() }.ok_or("кадр без изображения")?;
    // SAFETY: блокировка буфера кадра только для чтения, снимается ниже.
    if unsafe { CVPixelBufferLockBaseAddress(&image, CVPixelBufferLockFlags::ReadOnly) } != 0 {
        return Err("буфер кадра не заблокирован".to_owned());
    }
    let base = CVPixelBufferGetBaseAddress(&image);
    let bytes_per_row = CVPixelBufferGetBytesPerRow(&image);
    let width = CVPixelBufferGetWidth(&image);
    let height = CVPixelBufferGetHeight(&image);
    let result = if base.is_null() {
        Err("у буфера кадра нет адреса".to_owned())
    } else {
        // SAFETY: адрес заблокирован только для чтения, размер — строки на высоту буфера.
        let pixels =
            unsafe { std::slice::from_raw_parts(base as *const u8, bytes_per_row * height) };
        let frame = marker::Frame {
            pixels,
            width,
            height,
            bytes_per_row,
        };
        if let Some(path) = dump {
            let header = format!("{width} {height} {bytes_per_row}\n");
            let _ = std::fs::write(path, [header.as_bytes(), pixels].concat());
        }
        marker::sample(&frame, window_w, window_h).and_then(|luma| marker::decode(&luma))
    };
    // SAFETY: снимается блокировка, поставленная выше.
    unsafe { CVPixelBufferUnlockBaseAddress(&image, CVPixelBufferLockFlags::ReadOnly) };
    result
}

/// Объект ScreenCaptureKit, переданный из обработчика завершения на рабочий поток.
struct Sent<T>(T);
// SAFETY: `SCShareableContent` неизменяем после выдачи, ScreenCaptureKit отдаёт его на
// произвольной очереди и допускает чтение с любого потока.
unsafe impl<T> Send for Sent<T> {}

fn error_text(error: *mut NSError) -> Option<String> {
    // SAFETY: ненулевой указатель ошибки действителен на время обработчика.
    unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string())
}

/// Идущий поток окна пробы. Поток, получатель и очередь держит владелец до [`Capture::stop`].
pub struct Capture {
    /// Размер окна в точках по ScreenCaptureKit на запуске потока.
    pub window_w: f64,
    pub window_h: f64,
    stream: Retained<SCStream>,
    output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
    samples: Arc<Mutex<Vec<Sample>>>,
}

impl Capture {
    /// Запускает поток окна с номером `window_id` размером `window_w` × `window_h` точек. Кадр —
    /// окно без тени в точках, не чаще 120 раз в секунду.
    pub fn start(window_id: u32, first_frame: Option<PathBuf>) -> Result<Capture, String> {
        if !CGPreflightScreenCaptureAccess() {
            return Err("нет разрешения Screen Recording".to_owned());
        }
        let (tx, rx) = mpsc::channel::<Result<Sent<Retained<SCShareableContent>>, String>>();
        let block = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                // SAFETY: ненулевой объект действителен в обработчике, `retain` продлевает его жизнь.
                let content = unsafe { Retained::retain(content) };
                let _ = tx.send(match content {
                    Some(content) => Ok(Sent(content)),
                    None => Err(error_text(error).unwrap_or_else(|| "без описания".to_owned())),
                });
            },
        );
        // SAFETY: обработчик держит только канал.
        unsafe {
            SCShareableContent::getCurrentProcessShareableContentWithCompletionHandler(&block)
        };
        let content = wait(&rx, START_TIMEOUT, "список окон")?
            .map_err(|err| format!("список окон: {err}"))?
            .0;
        // SAFETY: чтение списка окон выданного объекта.
        let window = unsafe { content.windows() }
            .iter()
            .find(|window| unsafe { window.windowID() } == window_id)
            .ok_or_else(|| format!("окна {window_id} нет среди окон процесса"))?;
        // Размер окна — тот, что видит ScreenCaptureKit: менеджер окон мог изменить его после
        // показа, и `NSWindow::frame` на рабочем потоке не читается.
        // SAFETY: чтение рамки окна из выданного списка.
        let frame = unsafe { window.frame() };
        let (window_w, window_h) = (frame.size.width, frame.size.height);
        // SAFETY: фильтр одного окна из выданного списка.
        let filter = unsafe {
            SCContentFilter::initWithDesktopIndependentWindow(SCContentFilter::alloc(), &window)
        };
        // SAFETY: настройка нового объекта конфигурации до запуска потока.
        let config = unsafe {
            let config = SCStreamConfiguration::new();
            config.setWidth(window_w.round() as usize);
            config.setHeight(window_h.round() as usize);
            config.setMinimumFrameInterval(CMTime::new(1, 120));
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setShowsCursor(false);
            config.setQueueDepth(QUEUE_DEPTH);
            config.setIgnoreShadowsSingleWindow(true);
            config.setCapturesAudio(false);
            config
        };
        // SAFETY: поток без делегата с фильтром и конфигурацией выше.
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                None,
            )
        };
        let samples = Arc::new(Mutex::new(Vec::with_capacity(4096)));
        let (numer, denom) = timebase();
        let output = StreamOutput::alloc().set_ivars(OutputIvars {
            samples: Arc::clone(&samples),
            first_frame: Mutex::new(first_frame),
            window_w,
            window_h,
            numer,
            denom,
        });
        // SAFETY: `init` у NSObject без аргументов.
        let output: Retained<StreamOutput> = unsafe { msg_send![super(output), init] };
        let queue = DispatchQueue::new("probe-diff-capture", None);
        // SAFETY: получатель и очередь живут дольше потока: их держит `Capture`.
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                ProtocolObject::from_ref(&*output),
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|err| format!("получатель кадров: {}", err.localizedDescription()))?;
        completion(START_TIMEOUT, "запуск потока", |block| unsafe {
            stream.startCaptureWithCompletionHandler(Some(block))
        })?;
        Ok(Capture {
            window_w,
            window_h,
            stream,
            output,
            _queue: queue,
            samples,
        })
    }

    /// Останавливает поток и отдаёт журнал доставки.
    pub fn stop(self) -> Result<Vec<Sample>, String> {
        completion(STOP_TIMEOUT, "остановка потока", |block| unsafe {
            self.stream.stopCaptureWithCompletionHandler(Some(block))
        })?;
        // SAFETY: получатель был добавлен этому потоку.
        let _ = unsafe {
            self.stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*self.output),
                SCStreamOutputType::Screen,
            )
        };
        let samples = self
            .samples
            .lock()
            .map(|mut samples| std::mem::take(&mut *samples))
            .map_err(|_| "журнал кадров отравлен".to_owned())?;
        Ok(samples)
    }
}

fn wait<T>(rx: &mpsc::Receiver<T>, timeout: Duration, what: &str) -> Result<T, String> {
    rx.recv_timeout(timeout).map_err(|err| match err {
        RecvTimeoutError::Timeout => format!("{what}: нет ответа за {} с", timeout.as_secs()),
        RecvTimeoutError::Disconnected => format!("{what}: обработчик не вызван"),
    })
}

fn completion(
    timeout: Duration,
    what: &str,
    call: impl FnOnce(&block2::DynBlock<dyn Fn(*mut NSError)>),
) -> Result<(), String> {
    let (tx, rx) = mpsc::channel::<Option<String>>();
    let block = RcBlock::new(move |error: *mut NSError| {
        let _ = tx.send(error_text(error));
    });
    call(&block);
    match wait(&rx, timeout, what)? {
        None => Ok(()),
        Some(err) => Err(format!("{what}: {err}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mach_ticks_convert_with_timebase() {
        assert_eq!(mach_to_ns(3, 125, 3), 125);
        assert_eq!(mach_to_ns(24_000_000, 125, 3), 1_000_000_000);
        assert_eq!(mach_to_ns(7, 1, 1), 7);
        assert_eq!(
            mach_to_ns(u64::MAX / 2, 125, 3),
            (u128::from(u64::MAX / 2) * 125 / 3) as u64
        );
    }
}
