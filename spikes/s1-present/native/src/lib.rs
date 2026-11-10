//! M0 S1: a Rust render thread presenting into the NSView that Avalonia's
//! NativeControlHost owns, never touching the UI thread after attach.

#![allow(clippy::missing_safety_doc)]

use std::{
    ffi::c_void,
    panic::catch_unwind,
    ptr::NonNull,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering::Relaxed},
    },
    thread,
    time::{Duration, Instant},
};

use block2::RcBlock;
use objc2::{
    rc::{Retained, autoreleasepool},
    runtime::ProtocolObject,
};
use objc2_app_kit::NSView;
use objc2_core_foundation::CGSize;
use objc2_foundation::NSString;
use objc2_metal::*;
use objc2_quartz_core::{CACurrentMediaTime, CAMetalDrawable, CAMetalLayer};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {}

const SHADER: &str = r#"
#include <metal_stdlib>
using namespace metal;
struct V { float4 p [[position]]; float2 uv; };
vertex V vs(uint id [[vertex_id]]) {
    float2 uv = float2((id << 1) & 2, id & 2);
    V o; o.p = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1); o.uv = uv; return o;
}
fragment float4 fs(V in [[stage_in]], constant float& t [[buffer(0)]]) {
    float bar = abs(in.uv.x - fract(t * 0.5)) < 0.02 ? 1.0 : 0.0;
    return float4(mix(float3(0.08, 0.10, 0.16), float3(1, 1, 1), bar), 1);
}
"#;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct S1Stats {
    pub submitted: u64,
    pub presented: u64,
    pub max_interval_us: u64,
    pub present_latency_avg_us: u64,
    pub present_latency_max_us: u64,
    pub next_drawable_max_us: u64,
    pub drawable_w: u32,
    pub drawable_h: u32,
    pub handler_calls: u64,
}

#[derive(Default)]
struct Shared {
    handler_calls: AtomicU64,
    submitted: AtomicU64,
    presented: AtomicU64,
    max_interval_us: AtomicU64,
    latency_sum_us: AtomicU64,
    latency_max_us: AtomicU64,
    next_drawable_max_us: AtomicU64,
    w: AtomicU32,
    h: AtomicU32,
    stop: AtomicBool,
}

struct Unsafe<T>(T);
unsafe impl<T> Send for Unsafe<T> {}
unsafe impl<T> Sync for Unsafe<T> {}

struct Renderer {
    shared: Arc<Shared>,
    layer: Unsafe<Retained<CAMetalLayer>>,
    join: thread::JoinHandle<()>,
}

static STATE: Mutex<Option<Renderer>> = Mutex::new(None);
static EVENT_CB: AtomicUsize = AtomicUsize::new(0);
static TARGET_FPS: AtomicU32 = AtomicU32::new(120);

type EventCb = unsafe extern "C" fn(u64);

fn guarded(f: impl FnOnce() -> i32 + std::panic::UnwindSafe) -> i32 {
    catch_unwind(f).unwrap_or(-99)
}

/// Must run on the UI (main) thread: it touches the NSView.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn s1_attach(view: *mut c_void, w: u32, h: u32, scale: f32, sync: u8) -> i32 {
    let view = Unsafe(view);
    guarded(move || unsafe {
        let view = view;
        let view: &NSView = &*(view.0 as *const NSView);
        let Some(device) = MTLCreateSystemDefaultDevice() else { return -1 };
        let layer = CAMetalLayer::new();
        layer.setDevice(Some(&device));
        layer.setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        layer.setFramebufferOnly(true);
        layer.setMaximumDrawableCount(2);
        layer.setDisplaySyncEnabled(sync != 0);
        layer.setContentsScale(scale as f64);
        layer.setDrawableSize(CGSize::new(w.max(1) as f64, h.max(1) as f64));
        view.setWantsLayer(true);
        view.setLayer(Some(&layer));

        let Some(queue) = device.newCommandQueue() else { return -2 };
        let lib = match device.newLibraryWithSource_options_error(&NSString::from_str(SHADER), None) {
            Ok(l) => l,
            Err(_) => return -3,
        };
        let desc = MTLRenderPipelineDescriptor::new();
        desc.setVertexFunction(lib.newFunctionWithName(&NSString::from_str("vs")).as_deref());
        desc.setFragmentFunction(lib.newFunctionWithName(&NSString::from_str("fs")).as_deref());
        desc.colorAttachments().objectAtIndexedSubscript(0).setPixelFormat(MTLPixelFormat::BGRA8Unorm);
        let pipeline = match device.newRenderPipelineStateWithDescriptor_error(&desc) {
            Ok(p) => p,
            Err(_) => return -4,
        };

        let shared = Arc::new(Shared::default());
        shared.w.store(w, Relaxed);
        shared.h.store(h, Relaxed);
        let t_shared = shared.clone();
        let parts = Unsafe((layer.clone(), queue, pipeline));
        let join = thread::Builder::new()
            .name("render".into())
            .spawn(move || {
                let parts = parts;
                render_loop(parts.0.0, parts.0.1, parts.0.2, t_shared)
            })
            .unwrap();
        *STATE.lock().unwrap() = Some(Renderer { shared, layer: Unsafe(layer), join });
        0
    })
}

fn render_loop(
    layer: Retained<CAMetalLayer>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    pipeline: Retained<ProtocolObject<dyn MTLRenderPipelineState>>,
    s: Arc<Shared>,
) {
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }
    let t0 = Instant::now();
    let mut prev = Instant::now();
    let mut next = Instant::now();
    let mut n = 0u64;
    while !s.stop.load(Relaxed) {
        let interval = Duration::from_secs_f64(1.0 / TARGET_FPS.load(Relaxed).max(1) as f64);
        next += interval;
        let now = Instant::now();
        if next > now {
            thread::sleep(next - now);
        } else {
            next = now;
        }

        autoreleasepool(|_| unsafe {
            let t_nd = Instant::now();
            let Some(drawable) = layer.nextDrawable() else { return };
            s.next_drawable_max_us.fetch_max(t_nd.elapsed().as_micros() as u64, Relaxed);

            let Some(cb) = queue.commandBuffer() else { return };
            let pass = MTLRenderPassDescriptor::new();
            let att = pass.colorAttachments().objectAtIndexedSubscript(0);
            att.setTexture(Some(&drawable.texture()));
            att.setLoadAction(MTLLoadAction::Clear);
            att.setStoreAction(MTLStoreAction::Store);
            let Some(enc) = cb.renderCommandEncoderWithDescriptor(&pass) else { return };
            enc.setRenderPipelineState(&pipeline);
            let t: f32 = t0.elapsed().as_secs_f32();
            enc.setFragmentBytes_length_atIndex(NonNull::from(&t).cast(), 4, 0);
            enc.drawPrimitives_vertexStart_vertexCount(MTLPrimitiveType::Triangle, 0, 3);
            enc.endEncoding();

            let submitted_at = CACurrentMediaTime();
            let sh = s.clone();
            let handler = RcBlock::new(move |d: NonNull<ProtocolObject<dyn MTLDrawable>>| {
                sh.handler_calls.fetch_add(1, Relaxed);
                let shown = d.as_ref().presentedTime();
                if shown > 0.0 {
                    sh.presented.fetch_add(1, Relaxed);
                    let us = ((shown - submitted_at) * 1e6).max(0.0) as u64;
                    sh.latency_sum_us.fetch_add(us, Relaxed);
                    sh.latency_max_us.fetch_max(us, Relaxed);
                }
            });
            let as_drawable: &ProtocolObject<dyn MTLDrawable> = ProtocolObject::from_ref(&*drawable);
            as_drawable.addPresentedHandler(RcBlock::as_ptr(&handler).cast());
            cb.presentDrawable(as_drawable);
            cb.commit();
            s.submitted.fetch_add(1, Relaxed);
            let _: &ProtocolObject<dyn CAMetalDrawable> = &drawable;
        });

        let t = Instant::now();
        s.max_interval_us.fetch_max((t - prev).as_micros() as u64, Relaxed);
        prev = t;
        n += 1;
        if n.is_multiple_of(60) {
            let f = EVENT_CB.load(Relaxed);
            if f != 0 {
                let f: EventCb = unsafe { std::mem::transmute(f) };
                unsafe { f(n) };
            }
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn s1_set_event_cb(cb: Option<EventCb>) {
    EVENT_CB.store(cb.map_or(0, |f| f as usize), Relaxed);
}

#[unsafe(no_mangle)]
pub extern "C" fn s1_set_target_fps(fps: u32) {
    TARGET_FPS.store(fps, Relaxed);
}

#[unsafe(no_mangle)]
pub extern "C" fn s1_resize(w: u32, h: u32, scale: f32) -> i32 {
    guarded(move || unsafe {
        let g = STATE.lock().unwrap();
        let Some(r) = g.as_ref() else { return -1 };
        r.layer.0.setContentsScale(scale as f64);
        r.layer.0.setDrawableSize(CGSize::new(w.max(1) as f64, h.max(1) as f64));
        r.shared.w.store(w, Relaxed);
        r.shared.h.store(h, Relaxed);
        0
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn s1_set_sync(sync: u8) -> i32 {
    guarded(move || unsafe {
        let g = STATE.lock().unwrap();
        let Some(r) = g.as_ref() else { return -1 };
        r.layer.0.setDisplaySyncEnabled(sync != 0);
        0
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn s1_reset_stats() {
    if let Some(r) = STATE.lock().unwrap().as_ref() {
        let s = &r.shared;
        for a in [&s.handler_calls, &s.submitted, &s.presented, &s.max_interval_us, &s.latency_sum_us, &s.latency_max_us, &s.next_drawable_max_us] {
            a.store(0, Relaxed);
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn s1_get_stats(out: *mut S1Stats) -> i32 {
    let g = STATE.lock().unwrap();
    let Some(r) = g.as_ref() else { return -1 };
    let s = &r.shared;
    let presented = s.presented.load(Relaxed);
    let size = unsafe { r.layer.0.drawableSize() };
    unsafe {
        *out = S1Stats {
            submitted: s.submitted.load(Relaxed),
            presented,
            max_interval_us: s.max_interval_us.load(Relaxed),
            present_latency_avg_us: s.latency_sum_us.load(Relaxed) / presented.max(1),
            present_latency_max_us: s.latency_max_us.load(Relaxed),
            next_drawable_max_us: s.next_drawable_max_us.load(Relaxed),
            drawable_w: size.width as u32,
            drawable_h: size.height as u32,
            handler_calls: s.handler_calls.load(Relaxed),
        };
    }
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn s1_detach() {
    if let Some(r) = STATE.lock().unwrap().take() {
        r.shared.stop.store(true, Relaxed);
        let _ = r.join.join();
    }
}
