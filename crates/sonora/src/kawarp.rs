//! P3c — the dynamic immersive background: a native port of
//! `ui/vendor/kawarp.js` driven through an `egui_glow` paint callback.
//!
//! The old UI rendered "dynamic" into a WebGL canvas: on each cover change
//! the source was tinted and Kawase-blurred 8× at 128² into a crossfade
//! pair; every animation frame blended the pair, domain-warped it to full
//! resolution (two-octave simplex noise) and applied vignette/saturation/
//! dither on output. The animation speed rides the backend energy report —
//! onsets punch it up, it eases back (`updateDynamicSpeed` in `app.js`).
//!
//! The pipeline, tuning constants and easing math are ported one-to-one,
//! with two deliberate structural differences:
//!
//! * warp + output run as a single full-screen pass. The output pass only
//!   re-samples the warp pass's own pixels (at `scale`), so evaluating the
//!   warp at the output's sample position is mathematically identical —
//!   and saves a full-resolution intermediate FBO.
//! * GL state lives in thread-locals and the callback captures nothing:
//!   `CallbackFn::new` demands `Fn(PaintCallbackInfo, &Painter) + Send +
//!   Sync + 'static`, and glow objects are not thread-safe to share.
//!
//! Every failure — legacy shader interface, compile/link error, incomplete
//! framebuffer, or a runtime GL error — latches `failed()` for the whole
//! session and the overlay falls back to the always-drawn base gradient,
//! mirroring the old `kawarpFailed` park. GL objects are never deleted
//! explicitly: glow handles are plain names, the driver frees them when
//! the context dies, and thread-local destructors must not touch a dead
//! context.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use eframe::egui_glow::{CallbackFn, ShaderVersion};
use eframe::glow::{self, HasContext as _};
use egui::PaintCallbackInfo;

/// Blur/crossfade working size — the old `BLUR_SIZE`.
const BLUR_SIZE: i32 = 128;
/// Kawase iterations per cover change — the old `blurPasses`.
const BLUR_PASSES: i32 = 8;
/// Cover crossfade length, wall-clock — the old `transitionDuration`.
const TRANSITION: Duration = Duration::from_millis(1000);
/// UI-side speed ease cadence: one step per backend status poll (the old
/// `updateDynamicSpeed` ran on the 250 ms status poll, not per frame).
const SPEED_TICK: Duration = Duration::from_millis(250);

// Instance options, from the old `new Kawarp(canvas, {...})` in
// `applyDynamicBg`.
const OPT_WARP_INTENSITY: f32 = 1.0;
const OPT_ANIMATION_SPEED: f32 = 0.4;
const OPT_SATURATION: f32 = 1.5;
const OPT_DITHERING: f32 = 0.008;
const OPT_SCALE: f32 = 1.0;
const OPT_TINT_INTENSITY: f32 = 0.0;
const OPT_TINT_COLOR: [f32; 3] = [0.157, 0.157, 0.235];

// -- GLSL (old sources, ported from `varying`/`texture2D`/`gl_FragColor`
//    to the modern interface; precision qualifiers only matter on ES) ----

const VERT_SRC: &str = r#"
in vec2 a_position;
in vec2 a_texCoord;
out vec2 v_texCoord;
void main() {
    gl_Position = vec4(a_position, 0.0, 1.0);
    v_texCoord = a_texCoord;
}
"#;

const TINT_SRC: &str = r#"
in vec2 v_texCoord;
out vec4 fragColor;

uniform sampler2D u_texture;
uniform vec3 u_tintColor;
uniform float u_tintIntensity;

void main() {
    vec4 color = texture(u_texture, v_texCoord);
    float luma = dot(color.rgb, vec3(0.299, 0.587, 0.114));
    // darkMask: 1.0 for black, 0.0 for luma >= 0.5
    float darkMask = 1.0 - smoothstep(0.0, 0.5, luma);
    color.rgb = mix(color.rgb, u_tintColor, darkMask * u_tintIntensity);
    fragColor = color;
}
"#;

const BLUR_SRC: &str = r#"
in vec2 v_texCoord;
out vec4 fragColor;

uniform sampler2D u_texture;
uniform vec2 u_resolution;
uniform float u_offset;

void main() {
    vec2 texelSize = 1.0 / u_resolution;
    vec4 color = vec4(0.0);
    color += texture(u_texture, v_texCoord + vec2(-u_offset, -u_offset) * texelSize);
    color += texture(u_texture, v_texCoord + vec2(u_offset, -u_offset) * texelSize);
    color += texture(u_texture, v_texCoord + vec2(-u_offset, u_offset) * texelSize);
    color += texture(u_texture, v_texCoord + vec2(u_offset, u_offset) * texelSize);
    fragColor = color * 0.25;
}
"#;

const BLEND_SRC: &str = r#"
in vec2 v_texCoord;
out vec4 fragColor;

uniform sampler2D u_texture1;
uniform sampler2D u_texture2;
uniform float u_blend;

void main() {
    vec4 color1 = texture(u_texture1, v_texCoord);
    vec4 color2 = texture(u_texture2, v_texCoord);
    fragColor = mix(color1, color2, u_blend);
}
"#;

/// Domain warp fused with output (old `DOMAIN_WARP_SHADER` + `OUTPUT_SHADER`).
///
/// The output pass originally re-sampled the warp pass's full-res texture
/// at `(v_texCoord - 0.5) / u_scale + 0.5`; evaluating the warp at that
/// same position yields the identical color without the intermediate.
const OUT_SRC: &str = r#"
in vec2 v_texCoord;
out vec4 fragColor;

uniform sampler2D u_texture;
uniform float u_time;
uniform float u_intensity;
uniform float u_saturation;
uniform float u_dithering;
uniform float u_scale;
uniform vec2 u_resolution;

vec3 mod289(vec3 x) { return x - floor(x * (1.0 / 289.0)) * 289.0; }
vec2 mod289(vec2 x) { return x - floor(x * (1.0 / 289.0)) * 289.0; }
vec3 permute(vec3 x) { return mod289(((x * 34.0) + 1.0) * x); }

float snoise(vec2 v) {
    const vec4 C = vec4(0.211324865405187, 0.366025403784439,
                        -0.577350269189626, 0.024390243902439);
    vec2 i  = floor(v + dot(v, C.yy));
    vec2 x0 = v - i + dot(i, C.xx);
    vec2 i1 = (x0.x > x0.y) ? vec2(1.0, 0.0) : vec2(0.0, 1.0);
    vec4 x12 = x0.xyxy + C.xxzz;
    x12.xy -= i1;
    i = mod289(i);
    vec3 p = permute(permute(i.y + vec3(0.0, i1.y, 1.0)) + i.x + vec3(0.0, i1.x, 1.0));
    vec3 m = max(0.5 - vec3(dot(x0, x0), dot(x12.xy, x12.xy), dot(x12.zw, x12.zw)), 0.0);
    m = m * m;
    m = m * m;
    vec3 x = 2.0 * fract(p * C.www) - 1.0;
    vec3 h = abs(x) - 0.5;
    vec3 ox = floor(x + 0.5);
    vec3 a0 = x - ox;
    m *= 1.79284291400159 - 0.85373472095314 * (a0 * a0 + h * h);
    vec3 g;
    g.x = a0.x * x0.x + h.x * x0.y;
    g.yz = a0.yz * x12.xz + h.yz * x12.yw;
    return 130.0 * dot(m, g);
}

float hash(vec3 p) {
    p = fract(p * 0.1031);
    p += dot(p, p.zyx + 31.32);
    return fract((p.x + p.y) * p.z);
}

void main() {
    // Output's scale zoom, evaluated first: the warp used to render at
    // `uv` and output sampled warp at this same `uv`.
    vec2 uv = (v_texCoord - 0.5) / u_scale + 0.5;
    uv = clamp(uv, 0.0, 1.0);

    float t = u_time * 0.05;
    vec2 center = uv - 0.5;
    float centerWeight = 1.0 - smoothstep(0.0, 0.7, length(center));

    // Large-scale movement (slow, big blobs)
    float n1 = snoise(uv * 0.35 + vec2(t, t * 0.7));
    float n2 = snoise(uv * 0.35 + vec2(-t * 0.8, t * 0.5) + vec2(50.0, 50.0));

    // Medium-scale detail (adds organic movement)
    float n3 = snoise(uv * 0.9 + vec2(t * 1.2, -t) + vec2(100.0, 0.0));
    float n4 = snoise(uv * 0.9 + vec2(-t, t * 1.1) + vec2(0.0, 100.0));

    vec2 warp = vec2(
        n1 * 0.65 + n3 * 0.35,
        n2 * 0.65 + n4 * 0.35
    ) * centerWeight;

    vec2 warpedUV = clamp(uv + warp * u_intensity, 0.0, 1.0);
    vec4 color = texture(u_texture, warpedUV);

    vec2 vc = v_texCoord - 0.5;
    color.rgb *= 1.0 - dot(vc, vc) * 0.3;

    float gray = dot(color.rgb, vec3(0.299, 0.587, 0.114));
    color.rgb = mix(vec3(gray), color.rgb, u_saturation);

    vec2 pixelPos = floor(v_texCoord * u_resolution);
    float noise = hash(vec3(pixelPos, floor(u_time * 60.0)));
    color.rgb += (noise - 0.5) * u_dithering;

    fragColor = color;
}
"#;

/// One decoded cover awaiting GPU upload: RGBA8 rows, top-down, exactly
/// what the old `texImage2D` of an `ImageBitmap` fed the source texture.
struct Cover {
    key: String,
    w: i32,
    h: i32,
    rgba: Vec<u8>,
}

/// The energy → animation-speed machine: the old `updateDynamicSpeed`
/// plus the `dynBg*` globals, isolated so it can be unit-tested without
/// a GL context. One `step` per status poll (250 ms), NOT per frame —
/// the pulse decay is tuned to that cadence.
#[derive(Debug)]
struct Speed {
    /// Old `dynBgSpeed`: eased toward the energy target at 0.25/step.
    ui: f32,
    /// Old `dynBgAppliedSpeed`: what was last pushed to the engine
    /// (`-1` forces the first push).
    applied: f32,
    /// Old kawarp `_targetAnimationSpeed`: the engine eases at 0.05/frame.
    target: f32,
    /// Old `dynBgPulse`: onset punch that decays 0.7/step.
    pulse: f32,
    /// Old `dynBgLastEnergy`: onset detection baseline.
    last_energy: f32,
}

impl Speed {
    /// Old initial globals: `dynBgSpeed = 0.4`, `dynBgAppliedSpeed = -1`.
    fn new() -> Self {
        Self {
            ui: OPT_ANIMATION_SPEED,
            applied: -1.0,
            target: OPT_ANIMATION_SPEED,
            pulse: 0.0,
            last_energy: 0.0,
        }
    }

    /// New cover: drop the previous track's transient (no onset spike
    /// from stale energy); the base speed carries over for a smooth
    /// crossfade — the old `applyDynamicBg` reset.
    fn on_cover(&mut self) {
        self.pulse = 0.0;
        self.last_energy = 0.0;
        self.applied = -1.0;
    }

    /// Mode off / overlay gone: the old `stopDynamicBg` reset.
    fn stop(&mut self) {
        *self = Self::new();
    }

    /// One status-poll step (old `updateDynamicSpeed`).
    fn step(&mut self, energy: f32) {
        let e = energy.max(0.0);
        let onset = (e - self.last_energy).max(0.0);
        self.last_energy = e;
        self.pulse = (self.pulse * 0.7 + if onset > 0.05 { 0.45 } else { 0.0 }).min(0.7);
        let target = (0.2 + e * 1.2 + self.pulse).clamp(0.15, 1.4);
        self.ui += (target - self.ui) * 0.25;
        if (self.ui - self.applied).abs() > 0.03 {
            self.applied = self.ui;
            self.target = self.ui;
        }
    }
}

/// Driver-side state (cover delivery + speed machine): no GL, alive for
/// the whole session.
struct Meta {
    /// Current artwork template — a change fires `Speed::on_cover`.
    key: String,
    /// Undelivered cover pixels (taken by `paint` once per frame).
    pixels: Option<Cover>,
    /// Whether time advances this frame (old `dynamicRenderWanted`:
    /// playing or still loading — paused freezes the last frame).
    advance: bool,
    last_tick: Instant,
    speed: Speed,
}

impl Meta {
    fn new() -> Self {
        Self {
            key: String::new(),
            pixels: None,
            advance: false,
            last_tick: Instant::now(),
            speed: Speed::new(),
        }
    }
}

/// A color attachment (texture + FBO) at the 128² working size.
struct Target {
    fbo: glow::Framebuffer,
    tex: glow::Texture,
}

struct TintProg {
    prog: glow::Program,
    texture: glow::UniformLocation,
    color: glow::UniformLocation,
    intensity: glow::UniformLocation,
}

struct BlurProg {
    prog: glow::Program,
    texture: glow::UniformLocation,
    resolution: glow::UniformLocation,
    offset: glow::UniformLocation,
}

struct BlendProg {
    prog: glow::Program,
    texture1: glow::UniformLocation,
    texture2: glow::UniformLocation,
    blend: glow::UniformLocation,
}

struct OutProg {
    prog: glow::Program,
    texture: glow::UniformLocation,
    time: glow::UniformLocation,
    intensity: glow::UniformLocation,
    saturation: glow::UniformLocation,
    dithering: glow::UniformLocation,
    scale: glow::UniformLocation,
    resolution: glow::UniformLocation,
}

struct Progs {
    tint: TintProg,
    blur: BlurProg,
    blend: BlendProg,
    out: OutProg,
}

/// GL objects + per-frame animation state, created lazily on the first
/// paint callback (the only place with a live context).
struct Gfx {
    progs: Progs,
    vao: glow::VertexArray,
    _vbo: glow::Buffer,
    blur1: Target,
    blur2: Target,
    cur: Target,
    next: Target,
    src: glow::Texture,
    /// Whether any cover has ever been uploaded (`hasImage`).
    has_image: bool,
    /// Key of the uploaded cover (skip identical re-uploads).
    cur_key: String,
    /// Crossfade in progress (`isTransitioning`).
    transitioning: bool,
    transition_at: Instant,
    /// Engine-side actual speed, eased toward `Speed::target` at 0.05/frame.
    engine_speed: f32,
    last_frame: Instant,
    /// Warped time (`accumulatedTime`), dt × speed.
    accum: f32,
}

thread_local! {
    static META: RefCell<Meta> = RefCell::new(Meta::new());
    static GFX: RefCell<Option<Gfx>> = const { RefCell::new(None) };
    static FAILED: Cell<bool> = const { Cell::new(false) };
}

/// Whether the port gave up for the session (old `kawarpFailed`): the
/// caller then parks on the base gradient without retrying.
pub fn failed() -> bool {
    FAILED.with(Cell::get)
}

/// Whether a cover is on screen — kept rendering when the track itself
/// has no art (the old `applyDynamicBg` early-return left the running
/// instance alone).
pub fn has_image() -> bool {
    GFX.with(|slot| slot.borrow().as_ref().is_some_and(|g| g.has_image))
}

/// Old `stopDynamicBg` transient reset: back to the defaults when the
/// background is off, ineligible or the overlay closed. GL objects stay
/// cached (old disposed them; reusing them only skips the gradient gap).
pub fn reset() {
    META.with(|m| {
        let mut m = m.borrow_mut();
        m.speed.stop();
        m.pixels = None;
        m.advance = false;
    });
}

/// Feed one frame's inputs. Called every frame the dynamic background is
/// wanted, before the paint callback is queued:
///
/// * `key` — current artwork template (a change drops stale transients);
/// * `pixels` — freshly decoded cover (one delivery per track, from
///   [`crate::artwork::Covers::dyn_pixels`]), swizzled to RGBA here;
/// * `energy` — `PlayerReport.energy` 0..1 (drives the speed machine);
/// * `advance` — playing or loading: paused freezes the warped time.
pub fn update(key: &str, pixels: Option<egui::ColorImage>, energy: f32, advance: bool) {
    META.with(|m| {
        let mut m = m.borrow_mut();
        if m.key != key {
            m.key = key.to_owned();
            m.speed.on_cover();
            m.pixels = None;
            m.last_tick = Instant::now();
        }
        if let Some(image) = pixels {
            let [w, h] = image.size;
            let mut rgba = Vec::with_capacity(w * h * 4);
            for px in &image.pixels {
                rgba.extend_from_slice(&px.to_srgba_unmultiplied());
            }
            m.pixels = Some(Cover {
                key: key.to_owned(),
                w: w as i32,
                h: h as i32,
                rgba,
            });
        }
        m.advance = advance;
        if m.last_tick.elapsed() >= SPEED_TICK {
            m.last_tick = Instant::now();
            m.speed.step(energy);
        }
    });
}

/// The `egui_glow` callback body: init once, ingest this frame's inputs,
/// render. Draws nothing until a cover exists, so the base gradient
/// underneath shows through — the old `render()` early-return on
/// `!hasImage` left the gradient visible too.
pub fn paint(info: PaintCallbackInfo, painter: &eframe::egui_glow::Painter) {
    let gl = painter.gl();
    if failed() {
        return;
    }
    // Lazy GL init: the callback is the only place with a live context.
    GFX.with(|slot| {
        let initialized = slot.borrow().is_some();
        if initialized {
            return;
        }
        // SAFETY: `gl` is the live context egui is painting with.
        match unsafe { init(gl) } {
            Ok(g) => *slot.borrow_mut() = Some(g),
            Err(why) => {
                log::warn!("dynamic bg disabled: {why}");
                FAILED.with(|f| f.set(true));
            }
        }
    });
    if failed() {
        return;
    }

    // Inputs first: never hold a thread-local borrow across GL work.
    let (cover, advance, target) = META.with(|m| {
        let mut m = m.borrow_mut();
        (m.pixels.take(), m.advance, m.speed.target)
    });

    GFX.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(g) = slot.as_mut() else {
            return;
        };
        if let Some(px) = cover {
            if !g.has_image || g.cur_key != px.key {
                // SAFETY: `gl` belongs to `g`'s context.
                unsafe { g.upload(gl, px) };
            }
        }
        if !g.has_image {
            return;
        }
        // SAFETY: same context; egui restores its own state after us.
        unsafe { g.render(gl, &info, advance, target) };
    });

    // Any GL error means the port is broken on this driver — park on the
    // gradient for the session (egui logs the same error right after us).
    // SAFETY: reading the error flag is always valid.
    let err = unsafe { gl.get_error() };
    if err != glow::NO_ERROR {
        log::warn!("dynamic bg GL error 0x{err:x}: falling back to the gradient");
        FAILED.with(|f| f.set(true));
    }
}

/// The callback to queue over the base gradient while the dynamic
/// background is wanted. The capture is empty by design — all state
/// lives in thread-locals (see the module docs).
pub fn paint_callback(rect: egui::Rect) -> egui::PaintCallback {
    egui::PaintCallback {
        rect,
        callback: std::sync::Arc::new(CallbackFn::new(paint)),
    }
}

// -- GL setup ---------------------------------------------------------------

fn header(sv: &ShaderVersion) -> String {
    let mut h = sv.version_declaration().to_string();
    if sv.is_embedded() {
        h.push_str("precision highp float;\n");
    }
    h
}

/// Compile one shader stage. `Err` carries the info log.
unsafe fn compile(gl: &glow::Context, kind: u32, src: &str) -> Result<glow::Shader, String> {
    let sh = gl.create_shader(kind)?;
    gl.shader_source(sh, src);
    gl.compile_shader(sh);
    if !gl.get_shader_compile_status(sh) {
        let log = gl.get_shader_info_log(sh);
        gl.delete_shader(sh);
        return Err(log);
    }
    Ok(sh)
}

/// Link a fragment stage against the shared quad vertex shader, pinning
/// the attribute locations so one VAO serves every program.
unsafe fn link(
    gl: &glow::Context,
    vert: glow::Shader,
    frag_src: &str,
) -> Result<glow::Program, String> {
    let frag = compile(gl, glow::FRAGMENT_SHADER, frag_src)?;
    let prog = gl.create_program()?;
    gl.attach_shader(prog, vert);
    gl.attach_shader(prog, frag);
    gl.bind_attrib_location(prog, 0, "a_position");
    gl.bind_attrib_location(prog, 1, "a_texCoord");
    gl.link_program(prog);
    let ok = gl.get_program_link_status(prog);
    let log = gl.get_program_info_log(prog);
    gl.detach_shader(prog, vert);
    gl.detach_shader(prog, frag);
    gl.delete_shader(frag);
    if !ok {
        gl.delete_program(prog);
        return Err(log);
    }
    Ok(prog)
}

unsafe fn uni(
    gl: &glow::Context,
    prog: glow::Program,
    name: &str,
) -> Result<glow::UniformLocation, String> {
    gl.get_uniform_location(prog, name)
        .ok_or_else(|| format!("uniform {name} optimized out"))
}

/// Texture + framebuffer at `w × h` with LINEAR/CLAMP filtering, trying
/// half-float storage first (the old `useHighPrecision` targets) and
/// falling back to RGBA8 when the driver can't render to it — the old
/// `createSizedFramebuffer` completeness fallback.
unsafe fn create_target(gl: &glow::Context, w: i32, h: i32) -> Result<Target, String> {
    let tex = gl.create_texture()?;
    gl.bind_texture(glow::TEXTURE_2D, Some(tex));
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_S,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_T,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MIN_FILTER,
        glow::LINEAR as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MAG_FILTER,
        glow::LINEAR as i32,
    );
    let mut internal = glow::RGBA16F as i32;
    let mut ty = glow::HALF_FLOAT;
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        internal,
        w,
        h,
        0,
        glow::RGBA,
        ty,
        glow::PixelUnpackData::Slice(None),
    );
    let fbo = gl.create_framebuffer()?;
    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
    gl.framebuffer_texture_2d(
        glow::FRAMEBUFFER,
        glow::COLOR_ATTACHMENT0,
        glow::TEXTURE_2D,
        Some(tex),
        0,
    );
    let mut status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
    if status != glow::FRAMEBUFFER_COMPLETE {
        internal = glow::RGBA8 as i32;
        ty = glow::UNSIGNED_BYTE;
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            internal,
            w,
            h,
            0,
            glow::RGBA,
            ty,
            glow::PixelUnpackData::Slice(None),
        );
        status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
    }
    gl.bind_framebuffer(glow::FRAMEBUFFER, None);
    gl.bind_texture(glow::TEXTURE_2D, None);
    if status != glow::FRAMEBUFFER_COMPLETE {
        return Err(format!("framebuffer incomplete: 0x{status:x}"));
    }
    Ok(Target { fbo, tex })
}

/// Build every GL object. On `Err` partial objects are left for the
/// driver to reclaim with the context — the session is parked anyway.
unsafe fn init(gl: &glow::Context) -> Result<Gfx, String> {
    let sv = ShaderVersion::get(gl);
    if !sv.is_new_shader_interface() {
        return Err(format!("legacy GLSL interface ({sv:?})"));
    }
    let head = header(&sv);
    let vert = compile(gl, glow::VERTEX_SHADER, &format!("{head}{VERT_SRC}"))?;

    let prog = |src: &str| link(gl, vert, &format!("{head}{src}"));
    let tint = prog(TINT_SRC)?;
    let blur = prog(BLUR_SRC)?;
    let blend = prog(BLEND_SRC)?;
    let out = prog(OUT_SRC)?;
    gl.delete_shader(vert);

    let progs = Progs {
        tint: TintProg {
            texture: uni(gl, tint, "u_texture")?,
            color: uni(gl, tint, "u_tintColor")?,
            intensity: uni(gl, tint, "u_tintIntensity")?,
            prog: tint,
        },
        blur: BlurProg {
            texture: uni(gl, blur, "u_texture")?,
            resolution: uni(gl, blur, "u_resolution")?,
            offset: uni(gl, blur, "u_offset")?,
            prog: blur,
        },
        blend: BlendProg {
            texture1: uni(gl, blend, "u_texture1")?,
            texture2: uni(gl, blend, "u_texture2")?,
            blend: uni(gl, blend, "u_blend")?,
            prog: blend,
        },
        out: OutProg {
            texture: uni(gl, out, "u_texture")?,
            time: uni(gl, out, "u_time")?,
            intensity: uni(gl, out, "u_intensity")?,
            saturation: uni(gl, out, "u_saturation")?,
            dithering: uni(gl, out, "u_dithering")?,
            scale: uni(gl, out, "u_scale")?,
            resolution: uni(gl, out, "u_resolution")?,
            prog: out,
        },
    };

    // The old quad: two triangles, uv (0,0) at bottom-left (WebGL fed
    // images top-row-first, so covers render exactly as flipped there —
    // imperceptible under eight blur passes, kept for fidelity).
    let vao = gl.create_vertex_array()?;
    let vbo = gl.create_buffer()?;
    gl.bind_vertex_array(Some(vao));
    gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
    const QUAD: [f32; 24] = [
        -1.0, -1.0, 0.0, 0.0, //
        1.0, -1.0, 1.0, 0.0, //
        -1.0, 1.0, 0.0, 1.0, //
        -1.0, 1.0, 0.0, 1.0, //
        1.0, -1.0, 1.0, 0.0, //
        1.0, 1.0, 1.0, 1.0,
    ];
    // SAFETY: QUAD is a live stack array for the duration of the call.
    let bytes =
        std::slice::from_raw_parts(QUAD.as_ptr().cast::<u8>(), std::mem::size_of_val(&QUAD));
    gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
    gl.enable_vertex_attrib_array(0);
    gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 16, 0);
    gl.enable_vertex_attrib_array(1);
    gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 16, 8);
    gl.bind_vertex_array(None);
    gl.bind_buffer(glow::ARRAY_BUFFER, None);

    let src = gl.create_texture()?;
    gl.bind_texture(glow::TEXTURE_2D, Some(src));
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_S,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_WRAP_T,
        glow::CLAMP_TO_EDGE as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MIN_FILTER,
        glow::LINEAR as i32,
    );
    gl.tex_parameter_i32(
        glow::TEXTURE_2D,
        glow::TEXTURE_MAG_FILTER,
        glow::LINEAR as i32,
    );
    gl.tex_image_2d(
        glow::TEXTURE_2D,
        0,
        glow::RGBA8 as i32,
        1,
        1,
        0,
        glow::RGBA,
        glow::UNSIGNED_BYTE,
        glow::PixelUnpackData::Slice(Some(&[0, 0, 0, 255])),
    );
    gl.bind_texture(glow::TEXTURE_2D, None);

    let blur1 = create_target(gl, BLUR_SIZE, BLUR_SIZE)?;
    let blur2 = create_target(gl, BLUR_SIZE, BLUR_SIZE)?;
    let cur = create_target(gl, BLUR_SIZE, BLUR_SIZE)?;
    let next = create_target(gl, BLUR_SIZE, BLUR_SIZE)?;

    Ok(Gfx {
        progs,
        vao,
        _vbo: vbo,
        blur1,
        blur2,
        cur,
        next,
        src,
        has_image: false,
        cur_key: String::new(),
        transitioning: false,
        transition_at: Instant::now(),
        engine_speed: OPT_ANIMATION_SPEED,
        last_frame: Instant::now(),
        accum: 0.0,
    })
}

impl Gfx {
    /// Old `processNewImage`: upload, blur into the album pair (first
    /// cover fills both, no fade from garbage), otherwise swap and start
    /// the 1000 ms wall-clock crossfade.
    unsafe fn upload(&mut self, gl: &glow::Context, px: Cover) {
        gl.bind_texture(glow::TEXTURE_2D, Some(self.src));
        gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            px.w,
            px.h,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&px.rgba)),
        );
        gl.bind_texture(glow::TEXTURE_2D, None);
        if self.has_image {
            std::mem::swap(&mut self.cur, &mut self.next);
            self.blur_source_into(gl, &self.next);
            self.transitioning = true;
            self.transition_at = Instant::now();
        } else {
            self.blur_source_into(gl, &self.next);
            self.blur_source_into(gl, &self.cur);
            self.has_image = true;
        }
        self.cur_key = px.key;
    }

    /// Old `blurSourceInto`: tint (identity at our intensity 0 — but it
    /// performs the 600² → 128² downsample), then 8 Kawase passes
    /// ping-ponging blur1 ⇄ blur2 with offset `i + 0.5`, then a copy
    /// (offset 0) into the album target.
    unsafe fn blur_source_into(&self, gl: &glow::Context, target: &Target) {
        gl.bind_vertex_array(Some(self.vao));
        gl.use_program(Some(self.progs.tint.prog));
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.blur1.fbo));
        gl.viewport(0, 0, BLUR_SIZE, BLUR_SIZE);
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(self.src));
        gl.uniform_1_i32(Some(&self.progs.tint.texture), 0);
        gl.uniform_3_f32(
            Some(&self.progs.tint.color),
            OPT_TINT_COLOR[0],
            OPT_TINT_COLOR[1],
            OPT_TINT_COLOR[2],
        );
        gl.uniform_1_f32(Some(&self.progs.tint.intensity), OPT_TINT_INTENSITY);
        gl.draw_arrays(glow::TRIANGLES, 0, 6);

        gl.use_program(Some(self.progs.blur.prog));
        gl.uniform_1_i32(Some(&self.progs.blur.texture), 0);
        gl.uniform_2_f32(
            Some(&self.progs.blur.resolution),
            BLUR_SIZE as f32,
            BLUR_SIZE as f32,
        );
        let fbos = [&self.blur1, &self.blur2];
        let mut read = 0usize;
        for i in 0..BLUR_PASSES {
            let write = 1 - read;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbos[write].fbo));
            gl.viewport(0, 0, BLUR_SIZE, BLUR_SIZE);
            gl.bind_texture(glow::TEXTURE_2D, Some(fbos[read].tex));
            gl.uniform_1_f32(Some(&self.progs.blur.offset), i as f32 + 0.5);
            gl.draw_arrays(glow::TRIANGLES, 0, 6);
            read = write;
        }
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(target.fbo));
        gl.viewport(0, 0, BLUR_SIZE, BLUR_SIZE);
        gl.bind_texture(glow::TEXTURE_2D, Some(fbos[read].tex));
        gl.uniform_1_f32(Some(&self.progs.blur.offset), 0.0);
        gl.draw_arrays(glow::TRIANGLES, 0, 6);

        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        gl.bind_vertex_array(None);
        gl.active_texture(glow::TEXTURE0);
    }

    /// One animation frame — the old `render()`, with warp and output
    /// fused into the screen pass. egui set the viewport to the callback
    /// rect on entry; our FBO passes clobber it, so the screen pass
    /// restores it from `info`. Blend, scissor and program state are
    /// restored by egui's `prepare_painting` right after the callback.
    unsafe fn render(
        &mut self,
        gl: &glow::Context,
        info: &PaintCallbackInfo,
        advance: bool,
        target: f32,
    ) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;
        // Old renderLoop: ease the actual speed toward the pushed target
        // at 0.05/frame, accumulate warped time. Paused keeps `accum`
        // frozen (and `last_frame` current, so resume has no jump).
        self.engine_speed += (target - self.engine_speed) * 0.05;
        if advance {
            self.accum += dt.min(0.1) * self.engine_speed;
        }

        let mut blend_factor = 1.0f32;
        if self.transitioning {
            let elapsed = now.duration_since(self.transition_at).as_secs_f32();
            blend_factor = (elapsed / TRANSITION.as_secs_f32()).min(1.0);
            if blend_factor >= 1.0 {
                self.transitioning = false;
            }
        }

        gl.disable(glow::SCISSOR_TEST);
        gl.disable(glow::BLEND);
        gl.bind_vertex_array(Some(self.vao));

        let source = if self.transitioning {
            // Blend pair → blur1, cosine-eased (old crossfade).
            gl.use_program(Some(self.progs.blend.prog));
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.blur1.fbo));
            gl.viewport(0, 0, BLUR_SIZE, BLUR_SIZE);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.cur.tex));
            gl.uniform_1_i32(Some(&self.progs.blend.texture1), 0);
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(self.next.tex));
            gl.uniform_1_i32(Some(&self.progs.blend.texture2), 1);
            gl.active_texture(glow::TEXTURE0);
            let eased = 0.5 - 0.5 * (blend_factor * std::f32::consts::PI).cos();
            gl.uniform_1_f32(Some(&self.progs.blend.blend), eased);
            gl.draw_arrays(glow::TRIANGLES, 0, 6);
            self.blur1.tex
        } else {
            self.next.tex
        };

        let vp = info.viewport_in_pixels();
        gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        gl.viewport(vp.left_px, vp.from_bottom_px, vp.width_px, vp.height_px);
        gl.use_program(Some(self.progs.out.prog));
        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(glow::TEXTURE_2D, Some(source));
        gl.uniform_1_i32(Some(&self.progs.out.texture), 0);
        gl.uniform_1_f32(Some(&self.progs.out.time), self.accum);
        gl.uniform_1_f32(Some(&self.progs.out.intensity), OPT_WARP_INTENSITY);
        gl.uniform_1_f32(Some(&self.progs.out.saturation), OPT_SATURATION);
        gl.uniform_1_f32(Some(&self.progs.out.dithering), OPT_DITHERING);
        gl.uniform_1_f32(Some(&self.progs.out.scale), OPT_SCALE);
        gl.uniform_2_f32(
            Some(&self.progs.out.resolution),
            vp.width_px as f32,
            vp.height_px as f32,
        );
        gl.draw_arrays(glow::TRIANGLES, 0, 6);

        gl.bind_texture(glow::TEXTURE_2D, None);
        gl.bind_vertex_array(None);
        gl.active_texture(glow::TEXTURE0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fresh state matches the old globals: 0.4 speed, unapplied (-1 so
    /// the first step pushes), no pulse.
    #[test]
    fn speed_starts_at_defaults() {
        let s = Speed::new();
        assert_eq!(s.ui, OPT_ANIMATION_SPEED);
        assert_eq!(s.applied, -1.0);
        assert_eq!(s.target, OPT_ANIMATION_SPEED);
        assert_eq!(s.pulse, 0.0);
        assert_eq!(s.last_energy, 0.0);
    }

    /// Silence eases the UI-side speed toward 0.2 (the energy base) and
    /// pushes it to the engine only when it moved more than 0.03 from
    /// the last pushed value.
    #[test]
    fn speed_eases_and_pushes_on_change() {
        let mut s = Speed::new();
        s.step(0.0); // no onset → target 0.2, ui eases 0.4 → 0.35, pushes
        assert_eq!(s.applied, 0.35);
        assert_eq!(s.target, 0.35);
        let first = s.target;
        for _ in 0..20 {
            s.step(0.0);
        }
        // Kept easing toward 0.2; the last push happened while the gap
        // to the applied value was still above the 0.03 gate.
        assert!((s.ui - 0.2).abs() < 1e-3);
        assert!(s.target >= 0.2 && s.target < 0.25);
        assert!(s.target < first);
        assert_eq!(s.applied, s.target);
    }

    /// An onset above 0.05 punches the pulse up by 0.45 (capped 0.7) and
    /// the eased target follows `clamp(0.2 + e*1.2 + pulse, 0.15, 1.4)`.
    #[test]
    fn speed_onset_punches() {
        let mut s = Speed::new();
        s.step(0.8); // onset 0.8 > 0.05 → pulse 0.45, energy target 1.61 → 1.4
        assert!((s.pulse - 0.45).abs() < 1e-6);
        // One step eases the UI value 0.4 → 0.4 + (1.4 - 0.4) * 0.25.
        assert!((s.ui - 0.65).abs() < 1e-6);
        assert_eq!(s.target, s.ui); // pushed on first step (applied was -1)
                                    // No new onset → the pulse decays 0.7 per step.
        s.step(0.8);
        assert!((s.pulse - 0.45 * 0.7).abs() < 1e-6);
        // Negative energy (bad report) clamps to zero, never onset.
        let mut s2 = Speed::new();
        s2.step(-1.0);
        assert_eq!(s2.last_energy, 0.0);
        assert_eq!(s2.pulse, 0.0);
    }

    /// A new cover drops the transient but keeps the base speed; stop()
    /// restores every default (old `stopDynamicBg`).
    #[test]
    fn speed_cover_change_and_stop_resets() {
        let mut s = Speed::new();
        s.step(0.9);
        assert!(s.pulse > 0.0);
        s.on_cover();
        assert_eq!(s.pulse, 0.0);
        assert_eq!(s.last_energy, 0.0);
        assert_eq!(s.applied, -1.0);
        // The eased base speed carries over for a smooth crossfade.
        assert_ne!(s.ui, OPT_ANIMATION_SPEED);
        s.stop();
        assert_eq!(s.ui, OPT_ANIMATION_SPEED);
        assert_eq!(s.applied, -1.0);
        assert_eq!(s.target, OPT_ANIMATION_SPEED);
    }

    /// Every shader source declares a modern interface fragment output
    /// and the header injection prepends the driver's version line.
    #[test]
    fn shader_sources_are_modern_interface() {
        for src in [VERT_SRC, TINT_SRC, BLUR_SRC, BLEND_SRC, OUT_SRC] {
            assert!(src.contains("out vec4") || src.contains("out vec2"));
        }
        for src in [TINT_SRC, BLUR_SRC, BLEND_SRC, OUT_SRC] {
            assert!(src.contains("fragColor"));
            assert!(!src.contains("gl_FragColor"));
            assert!(!src.contains("texture2D"));
        }
        let head = header(&ShaderVersion::Gl140);
        assert!(head.starts_with("#version 140"));
        assert!(!head.contains("precision"));
        let es = header(&ShaderVersion::Es300);
        assert!(es.contains("#version 300 es"));
        assert!(es.contains("precision highp float;"));
    }
}
