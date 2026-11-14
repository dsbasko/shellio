//! Сценарий `gpu-calibration` (задача 11): калибровка наблюдателя графической памяти на ресурсах
//! известного размера до любых выводов по видимости. Текстуры с хранилищем в `IOSurface`
//! создаются тем же способом, что цель рендера ghostty (`src/renderer/metal/Target.zig`), для
//! сравнения — `MTLBuffer` и приватная текстура. Каждой странице ресурса дают ненулевое
//! содержимое отдельной фазой: несозданные страницы счётчики могут не видеть. После каждой фазы
//! `gpu::PHASES` — барьер (autorelease, выполнение команд GPU, проход отображения) и серия
//! замеров всех счётчиков с шагом `SETTLE_PACE_NS`, пока наблюдаемые счётчики не устоятся
//! (`gpu::settle`), затем таблица «счётчик → видимые события» (`gpu::calibrate`). libghostty
//! сценарий не запускает.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::ptr;
use std::rc::Rc;
use std::time::Duration;

use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2_core_foundation::{
    CFDictionary, CFNumber, CFRetained, CFString, CGPoint, CGRect, CGSize,
};
use objc2_io_surface::{
    IOSurfaceLockOptions, IOSurfacePurgeabilityState, IOSurfaceRef, kIOSurfaceBytesPerElement,
    kIOSurfaceBytesPerRow, kIOSurfaceHeight, kIOSurfacePixelFormat, kIOSurfaceWidth,
};
use objc2_metal::{
    MTLBlitCommandEncoder, MTLBuffer, MTLClearColor, MTLCommandBuffer, MTLCommandBufferStatus,
    MTLCommandEncoder, MTLCommandQueue, MTLDevice, MTLLoadAction, MTLOrigin, MTLPixelFormat,
    MTLRenderPassDescriptor, MTLResource, MTLResourceOptions, MTLSize, MTLStorageMode,
    MTLStoreAction, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};
use objc2_quartz_core::CALayer;
use serde::Serialize;

use super::{Poll, Scenario, Stand, Step, events, run_steps};
use crate::clock;
use crate::gpu::{
    self, Action, Calibration, FootprintRequest, LAYER_SURFACE, PHASES, Phase, SURFACES, Sample,
    Settle, Sizes, TargetStorage,
};
use crate::results::{Report, Requirement, Verdict};

const SURFACE_WIDTH: usize = 2048;
const SURFACE_HEIGHT: usize = 1024;
const BYTES_PER_ELEMENT: usize = 4;
/// `kCVPixelFormatType_32BGRA`, как у цели ghostty.
const PIXEL_FORMAT_BGRA: i32 = 0x4247_5241;
const PRIVATE_SIDE: usize = 2048;
const BUFFER_LENGTH: usize = PRIVATE_SIDE * PRIVATE_SIDE * BYTES_PER_ELEMENT;
const FILL: u8 = 0x5a;
const ACTION_TIMEOUT: Duration = Duration::from_secs(2);
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(8);
const SAMPLE_STEP: &str = "барьер и замер";
const MIB: f64 = 1024.0 * 1024.0;

pub fn run(stand: Rc<Stand>) {
    let mut report = stand.report(Scenario::GpuCalibration);
    if let Err(reason) = stand.check_session(&mut report) {
        return stand.conclude(report, Err(reason));
    }
    let calib = match Calib::new(&stand) {
        Ok(calib) => Rc::new(calib),
        Err(reason) => return stand.conclude(report, Err(reason)),
    };
    let mut steps = Vec::with_capacity(PHASES.len() * 2);
    for (index, phase) in PHASES.iter().enumerate() {
        let acting = Rc::clone(&calib);
        steps.push(Step::new(
            phase.label,
            ACTION_TIMEOUT,
            || Poll::Ready(()),
            move || acting.perform(phase),
        ));
        let sampling = Rc::clone(&calib);
        let recording = Rc::clone(&calib);
        steps.push(Step::new(
            SAMPLE_STEP,
            SAMPLE_TIMEOUT,
            move || sampling.poll_sample(index, phase),
            move || recording.record(),
        ));
    }
    run_steps(steps, move |result| {
        calib.add_to(&mut report);
        if let Err(failure) = result {
            return stand.conclude(report, Err(failure.describe()));
        }
        let verdict = calib.conclude(&mut report);
        stand.conclude(report, verdict);
    });
}

/// Цель как у ghostty: `IOSurface` и текстура поверх неё.
struct Target {
    surface: Option<CFRetained<IOSurfaceRef>>,
    texture: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
}

/// Барьер после действия: конец прохода отображения после него и выполненные команды GPU.
struct Barrier {
    pass_after: u64,
    gpu: Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>,
}

struct Pending {
    request: FootprintRequest,
    counters: BTreeMap<String, i64>,
    at_ns: u64,
}

/// Известный объём ресурса и его расчёт.
#[derive(Clone, Debug, Serialize)]
struct Known {
    resource: String,
    width: usize,
    height: usize,
    /// `IOSurfaceGetAllocSize` у `IOSurface`, `allocatedSize` у ресурса Metal.
    allocated: u64,
    expected: u64,
    texture_allocated: Option<u64>,
}

#[derive(Default)]
struct State {
    targets: Vec<Target>,
    buffer: Option<Retained<ProtocolObject<dyn MTLBuffer>>>,
    private: Option<Retained<ProtocolObject<dyn MTLTexture>>>,
    barrier: Option<Barrier>,
    /// Конец прохода отображения, которым закончился барьер фазы: от него считается серия.
    barrier_done: Option<u64>,
    pending: Option<Pending>,
    /// Серия замеров текущей фазы.
    series: Vec<Sample>,
    next_request_ns: u64,
    reply: Option<Result<Sample, String>>,
    samples: Vec<Sample>,
    footprint_ms: Vec<f64>,
    sizes: Sizes,
    known: Vec<Known>,
}

struct Calib {
    out: PathBuf,
    pid: i32,
    devices: Vec<gpu::Device>,
    renderer: usize,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    storage: TargetStorage,
    row_alignment: u64,
    page: u64,
    layer: Retained<CALayer>,
    state: RefCell<State>,
}

impl Calib {
    fn new(stand: &Stand) -> Result<Calib, String> {
        let devices = gpu::devices();
        let traits: Vec<gpu::DeviceTraits> =
            devices.iter().map(|device| device.traits.clone()).collect();
        let renderer = gpu::renderer_choice(&traits).ok_or_else(|| {
            format!("предусловие: нет устройства Metal, которое выбрал бы рендер: {traits:?}")
        })?;
        let queue = devices[renderer]
            .device
            .newCommandQueue()
            .ok_or("newCommandQueue вернул nil")?;
        let storage = gpu::target_storage(traits[renderer].unified_memory);
        // SAFETY: ключ — неизменяемая константа IOSurface.
        let row_alignment = IOSurfaceRef::property_alignment(unsafe { kIOSurfaceBytesPerRow });
        // SAFETY: `sysconf` только читает параметр системы.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let page = u64::try_from(page).map_err(|_| format!("sysconf(_SC_PAGESIZE) = {page}"))?;
        let layer = CALayer::new();
        layer.setFrame(CGRect::new(
            CGPoint::new(20.0, 20.0),
            CGSize::new(256.0, 128.0),
        ));
        stand.view.setWantsLayer(true);
        stand
            .view
            .layer()
            .ok_or("у вида стенда нет слоя")?
            .addSublayer(&layer);
        Ok(Calib {
            out: stand.prelude.out.clone(),
            pid: std::process::id() as i32,
            devices,
            renderer,
            queue,
            storage,
            row_alignment: row_alignment as u64,
            page,
            layer,
            state: RefCell::new(State::default()),
        })
    }

    fn device(&self) -> &ProtocolObject<dyn MTLDevice> {
        &self.devices[self.renderer].device
    }

    fn perform(&self, phase: &'static Phase) -> Result<(), String> {
        events::note(format!("калибровка: {}", phase.label));
        let gpu = autoreleasepool(|_| self.act(phase.action))
            .map_err(|reason| format!("{}: {reason}", phase.label))?;
        let mut state = self.state.borrow_mut();
        state.barrier = Some(Barrier {
            pass_after: clock::uptime_ns(),
            gpu,
        });
        Ok(())
    }

    fn act(
        &self,
        action: Action,
    ) -> Result<Option<Retained<ProtocolObject<dyn MTLCommandBuffer>>>, String> {
        let mut state = self.state.borrow_mut();
        match action {
            Action::Nothing => {}
            Action::CreateSurfaces => {
                for index in 0..SURFACES {
                    let surface = create_surface()?;
                    let texture = surface_texture(self.device(), &surface, self.storage)?;
                    let allocated = surface.alloc_size() as u64;
                    state.sizes.surfaces[index] = allocated;
                    state.known.push(Known {
                        resource: format!("iosurface-{index}"),
                        width: SURFACE_WIDTH,
                        height: SURFACE_HEIGHT,
                        allocated,
                        expected: gpu::iosurface_expected(
                            SURFACE_WIDTH as u64,
                            SURFACE_HEIGHT as u64,
                            BYTES_PER_ELEMENT as u64,
                            self.row_alignment,
                            self.page,
                        ),
                        texture_allocated: Some(texture.allocatedSize() as u64),
                    });
                    state.targets.push(Target {
                        surface: Some(surface),
                        texture: Some(texture),
                    });
                }
            }
            Action::TouchSurfaces => {
                for target in &state.targets {
                    fill_surface(target.surface.as_deref().ok_or("IOSurface уже отпущена")?)?;
                }
            }
            Action::RenderSurfaces => {
                let textures = state
                    .targets
                    .iter()
                    .map(|target| target.texture.as_deref().ok_or("текстура уже отпущена"))
                    .collect::<Result<Vec<_>, _>>()?;
                return self.render_targets(&textures).map(Some);
            }
            Action::CreateBuffer => {
                let buffer = self
                    .device()
                    .newBufferWithLength_options(
                        BUFFER_LENGTH,
                        MTLResourceOptions::StorageModeShared,
                    )
                    .ok_or("newBufferWithLength вернул nil")?;
                let allocated = buffer.allocatedSize() as u64;
                state.sizes.buffer = allocated;
                state.known.push(Known {
                    resource: "mtl-buffer".to_owned(),
                    width: BUFFER_LENGTH,
                    height: 1,
                    allocated,
                    expected: gpu::buffer_expected(BUFFER_LENGTH as u64, self.page),
                    texture_allocated: None,
                });
                state.buffer = Some(buffer);
            }
            Action::TouchBuffer => {
                let buffer = state.buffer.as_ref().ok_or("MTLBuffer нет")?;
                // SAFETY: `contents` общего буфера указывает на `length` байт памяти процесса.
                unsafe {
                    ptr::write_bytes(
                        buffer.contents().as_ptr().cast::<u8>(),
                        FILL,
                        buffer.length(),
                    )
                };
            }
            Action::CreatePrivate => {
                let texture = private_texture(self.device())?;
                let allocated = texture.allocatedSize() as u64;
                state.sizes.private = allocated;
                state.known.push(Known {
                    resource: "private-texture".to_owned(),
                    width: PRIVATE_SIDE,
                    height: PRIVATE_SIDE,
                    allocated,
                    expected: gpu::texture_payload(
                        PRIVATE_SIDE as u64,
                        PRIVATE_SIDE as u64,
                        BYTES_PER_ELEMENT as u64,
                    ),
                    texture_allocated: None,
                });
                state.private = Some(texture);
            }
            Action::TouchPrivate => {
                let buffer = state.buffer.as_ref().ok_or("MTLBuffer нет")?;
                let texture = state.private.as_ref().ok_or("приватной текстуры нет")?;
                return self.copy_to_private(buffer, texture).map(Some);
            }
            Action::SetLayerContents => {
                let surface = state.targets[LAYER_SURFACE]
                    .surface
                    .as_deref()
                    .ok_or("IOSurface слоя уже отпущена")?;
                // SAFETY: `IOSurfaceRef` бесплатно мостится в объект `IOSurface`, который
                // `CALayer` принимает как `contents` и удерживает сам.
                let object: &AnyObject = unsafe { &*ptr::from_ref(surface).cast::<AnyObject>() };
                // SAFETY: `contents` слоя — `IOSurface`, допустимое содержимое.
                unsafe { self.layer.setContents(Some(object)) };
            }
            Action::ReleaseBuffer => {
                state.buffer.take().ok_or("MTLBuffer уже отпущен")?;
            }
            Action::ReleasePrivate => {
                state
                    .private
                    .take()
                    .ok_or("приватная текстура уже отпущена")?;
            }
            Action::ReleaseTexture(index) => {
                state.targets[index]
                    .texture
                    .take()
                    .ok_or("текстура уже отпущена")?;
            }
            Action::Purge(index) => {
                let surface = state.targets[index]
                    .surface
                    .as_deref()
                    .ok_or("IOSurface уже отпущена")?;
                // SAFETY: прежнее состояние не запрашивается, указатель NULL допустим.
                let status = unsafe {
                    surface.set_purgeable(
                        IOSurfacePurgeabilityState::PurgeableEmpty.0,
                        ptr::null_mut(),
                    )
                };
                if status != 0 {
                    return Err(format!("IOSurfaceSetPurgeable вернул {status}"));
                }
            }
            Action::ReleaseSurface(index) => {
                state.targets[index]
                    .surface
                    .take()
                    .ok_or("IOSurface уже отпущена")?;
            }
            Action::ClearLayerContents => {
                // SAFETY: пустое содержимое слоя допустимо.
                unsafe { self.layer.setContents(None) };
            }
        }
        Ok(None)
    }

    /// Копирует ненулевой буфер в приватную текстуру: очистка проходом рендера могла бы
    /// обойтись метаданными без записи страниц.
    fn copy_to_private(
        &self,
        buffer: &ProtocolObject<dyn MTLBuffer>,
        texture: &ProtocolObject<dyn MTLTexture>,
    ) -> Result<Retained<ProtocolObject<dyn MTLCommandBuffer>>, String> {
        let commands = self
            .queue
            .commandBuffer()
            .ok_or("commandBuffer вернул nil")?;
        let blit = commands
            .blitCommandEncoder()
            .ok_or("blitCommandEncoder вернул nil")?;
        let row = PRIVATE_SIDE * BYTES_PER_ELEMENT;
        // SAFETY: буфер вмещает `row × PRIVATE_SIDE` байт, текстура того же размера и формата;
        // буфер команд удерживает оба ресурса до выполнения.
        unsafe {
            blit.copyFromBuffer_sourceOffset_sourceBytesPerRow_sourceBytesPerImage_sourceSize_toTexture_destinationSlice_destinationLevel_destinationOrigin(
                buffer,
                0,
                row,
                row * PRIVATE_SIDE,
                MTLSize {
                    width: PRIVATE_SIDE,
                    height: PRIVATE_SIDE,
                    depth: 1,
                },
                texture,
                0,
                0,
                MTLOrigin { x: 0, y: 0, z: 0 },
            )
        };
        blit.endEncoding();
        commands.commit();
        Ok(commands)
    }

    /// Очищает каждую цель проходом рендера в одном буфере команд, как кадр ghostty пишет цель.
    fn render_targets(
        &self,
        textures: &[&ProtocolObject<dyn MTLTexture>],
    ) -> Result<Retained<ProtocolObject<dyn MTLCommandBuffer>>, String> {
        let commands = self
            .queue
            .commandBuffer()
            .ok_or("commandBuffer вернул nil")?;
        for texture in textures {
            let pass = MTLRenderPassDescriptor::renderPassDescriptor();
            // SAFETY: индекс 0 — первое цветовое вложение, оно есть у любого описания прохода.
            let attachment = unsafe { pass.colorAttachments().objectAtIndexedSubscript(0) };
            attachment.setTexture(Some(texture));
            attachment.setLoadAction(MTLLoadAction::Clear);
            attachment.setStoreAction(MTLStoreAction::Store);
            attachment.setClearColor(MTLClearColor {
                red: 0.2,
                green: 0.4,
                blue: 0.6,
                alpha: 1.0,
            });
            let encoder = commands
                .renderCommandEncoderWithDescriptor(&pass)
                .ok_or("renderCommandEncoderWithDescriptor вернул nil")?;
            encoder.endEncoding();
        }
        commands.commit();
        Ok(commands)
    }

    /// Ждёт барьер фазы, затем снимает серию замеров с шагом `SETTLE_PACE_NS`, пока наблюдаемые
    /// счётчики не устоятся. `footprint` снимает служба `run.sh`, счётчики Metal и собственного
    /// учёта читаются в момент запроса.
    fn poll_sample(&self, index: usize, phase: &'static Phase) -> Poll<()> {
        let mut state = self.state.borrow_mut();
        if state.reply.is_some() {
            return Poll::Ready(());
        }
        if let Some(since) = state.barrier_done {
            return self.poll_series(&mut state, index, phase, since);
        }
        if let Some(barrier) = &state.barrier
            && let Some(commands) = &barrier.gpu
        {
            let status = commands.status();
            if status == MTLCommandBufferStatus::Error {
                let error = commands
                    .error()
                    .map(|error| error.localizedDescription().to_string());
                state.reply = Some(Err(format!(
                    "{}: команды GPU завершились ошибкой: {error:?}",
                    phase.label
                )));
                return Poll::Ready(());
            }
            if status != MTLCommandBufferStatus::Completed {
                return Poll::Pending(format!(
                    "{}: команды GPU в состоянии {status:?}",
                    phase.label
                ));
            }
        }
        let pass = state
            .barrier
            .as_ref()
            .map_or(Some(clock::uptime_ns()), |barrier| {
                events::display_pass_after(barrier.pass_after)
            });
        let Some(pass) = pass else {
            return Poll::Pending(format!(
                "{}: нет прохода отображения после действия",
                phase.label
            ));
        };
        state.barrier_done = Some(pass);
        self.poll_series(&mut state, index, phase, pass)
    }

    fn poll_series(
        &self,
        state: &mut State,
        index: usize,
        phase: &'static Phase,
        since: u64,
    ) -> Poll<()> {
        if state.pending.is_some() {
            let taken = state
                .pending
                .as_ref()
                .and_then(|pending| pending.request.take());
            let Some(result) = taken else {
                return Poll::Pending(format!("{}: нет ответа службы footprint", phase.label));
            };
            let Some(pending) = state.pending.take() else {
                return Poll::Pending(format!("{}: запрос потерян", phase.label));
            };
            let reply = match result {
                Ok(reply) => reply,
                Err(reason) => {
                    state.reply = Some(Err(format!("{}: {reason}", phase.label)));
                    return Poll::Ready(());
                }
            };
            state
                .footprint_ms
                .push(clock::interval_ms(reply.requested_ns, reply.replied_ns));
            let mut counters = pending.counters;
            counters.extend(gpu::footprint_counters(&reply.footprint));
            state.series.push(Sample {
                label: phase.label.to_owned(),
                at_ns: pending.at_ns,
                counters,
                ..Sample::default()
            });
            let settled = match gpu::settle(&state.series, since) {
                Settle::Pending => {
                    state.next_request_ns = pending.at_ns + gpu::SETTLE_PACE_NS;
                    return Poll::Pending(format!(
                        "{}: замеров {}, счётчики не устоялись",
                        phase.label,
                        state.series.len()
                    ));
                }
                Settle::Settled => true,
                Settle::Capped => false,
            };
            let series = std::mem::take(&mut state.series);
            state.reply = Some(
                gpu::settled_sample(&series, since, settled)
                    .ok_or_else(|| format!("{}: пустая серия замеров", phase.label)),
            );
            return Poll::Ready(());
        }
        let now = clock::uptime_ns();
        if now < state.next_request_ns {
            return Poll::Pending(format!("{}: шаг серии замеров", phase.label));
        }
        let mut counters = gpu::allocated_counters(&self.devices, self.renderer);
        counters.extend(own_counters(state));
        let label = format!("{index:02}-{}-{:02}", phase.label, state.series.len());
        state.pending = Some(Pending {
            request: gpu::request_footprint(&self.out, self.pid, &label),
            counters,
            at_ns: now,
        });
        Poll::Pending(format!("{}: запрос footprint отправлен", phase.label))
    }

    fn record(&self) -> Result<(), String> {
        let mut state = self.state.borrow_mut();
        state.barrier = None;
        state.barrier_done = None;
        state.next_request_ns = 0;
        match state.reply.take() {
            Some(Ok(sample)) => {
                state.samples.push(sample);
                Ok(())
            }
            Some(Err(reason)) => Err(reason),
            None => Err("замера нет".to_owned()),
        }
    }

    /// Устройства, ресурсы и замеры — и при ошибке исполнения, для диагностики.
    fn add_to(&self, report: &mut Report) {
        let state = self.state.borrow();
        let traits: Vec<&gpu::DeviceTraits> =
            self.devices.iter().map(|device| &device.traits).collect();
        report.push_json("gpu_devices", &traits);
        report.push("gpu_renderer_device", &traits[self.renderer].name);
        report.push(
            "gpu_renderer_choice",
            "та же логика, что chooseDevice в src/renderer/metal/Device.zig, а не выбор рендера",
        );
        report.push_json("gpu_target_storage", &self.storage);
        report.push("iosurface_row_alignment", self.row_alignment);
        report.push("page_size", self.page);
        report.push_json("gpu_known_sizes", &state.known);
        report.push_json(
            "gpu_settle",
            &serde_json::json!({
                "pace_ms": gpu::SETTLE_PACE_NS as f64 / 1e6,
                "window_ms": gpu::SETTLE_WINDOW_NS as f64 / 1e6,
                "cap_ms": gpu::SETTLE_CAP_NS as f64 / 1e6,
                "epsilon_bytes": gpu::SETTLE_EPSILON,
                "counters": gpu::SETTLE_COUNTERS,
            }),
        );
        report.push_json("gpu_samples", &state.samples);
        report.metric("footprint_roundtrip", "ms", state.footprint_ms.clone());
    }

    fn conclude(&self, report: &mut Report) -> Result<(), String> {
        let state = self.state.borrow();
        let calibration = gpu::calibrate(&state.samples, &state.sizes)?;
        report.push_json("gpu_calibration", &calibration.rows);
        report.push_json("gpu_fit_counters", &calibration.fit);
        let renderer = &self.devices[self.renderer].traits;
        report.finding(
            "gpu-renderer-device",
            Requirement::Required,
            Verdict::Observed,
            format!(
                "устройств Metal {}; рендер по логике chooseDevice выбрал бы «{}» (registryID {:#x}, \
                 lowPower {}, unifiedMemory {}), ресурсы калибровки созданы на нём, хранение целей {:?}",
                self.devices.len(),
                renderer.name,
                renderer.registry_id,
                renderer.low_power,
                renderer.unified_memory,
                self.storage
            ),
        );
        report.finding(
            "gpu-known-sizes",
            Requirement::Required,
            Verdict::Observed,
            describe_known(&state.known),
        );
        report.finding(
            "gpu-deferred-changes",
            Requirement::Required,
            Verdict::Observed,
            describe_deferred(&state.samples),
        );
        for row in &calibration.rows {
            if row.own || !(gpu::is_key(&row.counter) || row.sees_iosurface) {
                continue;
            }
            report.finding(
                &format!("gpu-counter-{}", slug(&row.counter)),
                Requirement::Required,
                Verdict::Observed,
                format!("{}: {}", row.counter, row.describe()),
            );
        }
        report.finding(
            "h7b-calibration-counters",
            Requirement::Required,
            Verdict::Observed,
            describe_fit(&calibration),
        );
        Ok(())
    }
}

fn own_counters(state: &State) -> BTreeMap<String, i64> {
    let iosurface: u64 = state
        .targets
        .iter()
        .filter_map(|target| target.surface.as_deref())
        .map(|surface| surface.alloc_size() as u64)
        .sum();
    let textures = state
        .targets
        .iter()
        .filter_map(|target| target.texture.as_deref())
        .chain(state.private.as_deref())
        .map(|texture| texture.allocatedSize() as u64);
    let buffer = state
        .buffer
        .as_deref()
        .map(|buffer| buffer.allocatedSize() as u64);
    let metal: u64 = textures.chain(buffer).sum();
    BTreeMap::from([
        (
            "own.iosurface_alloc_size".to_owned(),
            gpu::to_i64(iosurface),
        ),
        ("own.metal_allocated_size".to_owned(), gpu::to_i64(metal)),
    ])
}

fn create_surface() -> Result<CFRetained<IOSurfaceRef>, String> {
    let int = |value: usize| {
        i32::try_from(value)
            .map(CFNumber::new_i32)
            .map_err(|_| format!("{value} не помещается в i32"))
    };
    let width = int(SURFACE_WIDTH)?;
    let height = int(SURFACE_HEIGHT)?;
    let format = CFNumber::new_i32(PIXEL_FORMAT_BGRA);
    let bytes = int(BYTES_PER_ELEMENT)?;
    // SAFETY: ключи — неизменяемые константы IOSurface.
    let keys: [&CFString; 4] = unsafe {
        [
            kIOSurfaceWidth,
            kIOSurfaceHeight,
            kIOSurfacePixelFormat,
            kIOSurfaceBytesPerElement,
        ]
    };
    let values: [&CFNumber; 4] = [&width, &height, &format, &bytes];
    let properties = CFDictionary::from_slices(&keys, &values);
    // SAFETY: словарь свойств `IOSurfaceCreate` с числами под ожидаемыми ключами, как у ghostty
    // (`pkg/macos/iosurface/iosurface.zig`); цветовое пространство на объём не влияет.
    unsafe { IOSurfaceRef::new(properties.as_opaque()) }
        .ok_or_else(|| "IOSurfaceCreate вернул NULL".to_owned())
}

/// Текстура поверх `IOSurface` с теми же параметрами, что у цели ghostty.
fn surface_texture(
    device: &ProtocolObject<dyn MTLDevice>,
    surface: &IOSurfaceRef,
    storage: TargetStorage,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, String> {
    // SAFETY: размеры в пределах устройства, одна мип-ступень.
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm,
            SURFACE_WIDTH,
            SURFACE_HEIGHT,
            false,
        )
    };
    descriptor.setUsage(MTLTextureUsage::RenderTarget);
    let storage = match storage {
        TargetStorage::Shared => MTLResourceOptions::StorageModeShared,
        TargetStorage::Managed => MTLResourceOptions::StorageModeManaged,
    };
    descriptor.setResourceOptions(MTLResourceOptions::CPUCacheModeWriteCombined | storage);
    device
        .newTextureWithDescriptor_iosurface_plane(&descriptor, surface, 0)
        .ok_or_else(|| "newTextureWithDescriptor:iosurface:plane: вернул nil".to_owned())
}

fn private_texture(
    device: &ProtocolObject<dyn MTLDevice>,
) -> Result<Retained<ProtocolObject<dyn MTLTexture>>, String> {
    // SAFETY: размеры в пределах устройства, одна мип-ступень.
    let descriptor = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
            MTLPixelFormat::BGRA8Unorm,
            PRIVATE_SIDE,
            PRIVATE_SIDE,
            false,
        )
    };
    descriptor.setStorageMode(MTLStorageMode::Private);
    descriptor.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget);
    device
        .newTextureWithDescriptor(&descriptor)
        .ok_or_else(|| "newTextureWithDescriptor вернул nil".to_owned())
}

/// Ненулевое содержимое во все строки `IOSurface` через процессор.
fn fill_surface(surface: &IOSurfaceRef) -> Result<(), String> {
    // SAFETY: значение seed не запрашивается, указатель NULL допустим.
    let status = unsafe { surface.lock(IOSurfaceLockOptions::empty(), ptr::null_mut()) };
    if status != 0 {
        return Err(format!("IOSurfaceLock вернул {status}"));
    }
    let len = surface.bytes_per_row() * surface.height();
    // SAFETY: между lock и unlock базовый адрес указывает на `bytes_per_row × height` байт
    // единственной плоскости.
    unsafe { ptr::write_bytes(surface.base_address().as_ptr().cast::<u8>(), FILL, len) };
    // SAFETY: как у lock.
    let status = unsafe { surface.unlock(IOSurfaceLockOptions::empty(), ptr::null_mut()) };
    if status != 0 {
        return Err(format!("IOSurfaceUnlock вернул {status}"));
    }
    Ok(())
}

fn describe_known(known: &[Known]) -> String {
    let parts: Vec<String> = known
        .iter()
        .map(|resource| {
            let matches = if resource.allocated == resource.expected {
                "совпадает с расчётом".to_owned()
            } else {
                format!("расчёт {:.2} МиБ", resource.expected as f64 / MIB)
            };
            let texture = resource.texture_allocated.map_or(String::new(), |size| {
                format!(", allocatedSize текстуры {:.2} МиБ", size as f64 / MIB)
            });
            format!(
                "{} {}×{}: {:.2} МиБ ({matches}){texture}",
                resource.resource,
                resource.width,
                resource.height,
                resource.allocated as f64 / MIB
            )
        })
        .collect();
    parts.join("; ")
}

/// Изменения, которые проявились позже барьера фазы, и фазы, не устоявшиеся до предела серии.
fn describe_deferred(samples: &[Sample]) -> String {
    let deferred: Vec<String> = samples
        .iter()
        .filter(|sample| !sample.deferred.is_empty())
        .map(|sample| {
            let changes: Vec<String> = sample
                .deferred
                .iter()
                .map(|(counter, change)| format!("{counter} {:+.2} МиБ", *change as f64 / MIB))
                .collect();
            format!(
                "{}: {} через {:.0} мс после барьера",
                sample.label,
                changes.join(", "),
                sample.last_change_ms.unwrap_or(0.0)
            )
        })
        .collect();
    let restless: Vec<&str> = samples
        .iter()
        .filter(|sample| !sample.settled)
        .map(|sample| sample.label.as_str())
        .collect();
    let requests: usize = samples.iter().map(|sample| sample.requests).sum();
    format!(
        "замеров footprint {requests}; отложенные изменения: {}; не устоялись за {:.0} мс: {}",
        if deferred.is_empty() {
            "нет".to_owned()
        } else {
            deferred.join("; ")
        },
        gpu::SETTLE_CAP_NS as f64 / 1e6,
        if restless.is_empty() {
            "нет".to_owned()
        } else {
            restless.join(", ")
        }
    )
}

fn describe_fit(calibration: &Calibration) -> String {
    if calibration.fit.is_empty() {
        return "ни один счётчик процесса не следит за выделением и освобождением хранилища \
                IOSurface: задача 12 строит только матрицу поведения и снимков, выводы \
                по ресурсам остаются за ручной записью Instruments"
            .to_owned();
    }
    let layer: Vec<String> = calibration
        .rows
        .iter()
        .filter(|row| calibration.fit.contains(&row.counter))
        .map(|row| {
            let group = |name: &str| {
                row.group(name).map_or("?".to_owned(), |group| {
                    format!("{} {:+.2} ({})", group.seen.name(), group.ratio, group.at)
                })
            };
            format!(
                "{}: порядок ghostty {}, последний кадр слоя {}, проход рендера по целям {}",
                row.counter,
                group("s3-release"),
                group("s0-release"),
                group("iosurface-render")
            )
        })
        .collect();
    format!(
        "годны для задачи 12 (следят за хранилищем IOSurface: выделение, три освобождения, в \
         порядке ghostty до текстуры): {}; {}",
        calibration.fit.join(", "),
        layer.join("; ")
    )
}

/// Имя счётчика для имени находки: строчные латинские буквы, цифры и дефисы.
fn slug(counter: &str) -> String {
    let mut slug = String::new();
    for ch in counter.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    slug.trim_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_keeps_letters_digits_and_single_dashes() {
        assert_eq!(
            slug("footprint.IOAccelerator (graphics).dirty"),
            "footprint-ioaccelerator-graphics-dirty"
        );
        assert_eq!(
            slug("metal.current_allocated_size"),
            "metal-current-allocated-size"
        );
    }

    #[test]
    fn calibration_resources_have_known_sizes() {
        assert_eq!(
            gpu::iosurface_expected(
                SURFACE_WIDTH as u64,
                SURFACE_HEIGHT as u64,
                BYTES_PER_ELEMENT as u64,
                64,
                16384
            ),
            8 * 1024 * 1024
        );
        assert_eq!(BUFFER_LENGTH, 16 * 1024 * 1024);
        assert_eq!(PIXEL_FORMAT_BGRA.to_be_bytes(), *b"BGRA");
    }
}
