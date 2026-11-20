//! The glow around the selected tab and workspace: an OpenGL fragment
//! shader, run as an egui paint callback. It lays a soft glow around the
//! outline. When the selection changes, a bright spot runs once round the
//! outline and a slanted sheen sweeps across it, for `BURST`; then it holds
//! still, so it costs nothing until the next switch.
//!
//! A tab or workspace whose agent is working sends out a ripple every
//! `RIPPLE_EVERY`: a thin ring that leaves the outline and fades as it
//! spreads, for `RIPPLE`. Every ripple runs on one clock, so one burst of
//! frames covers them all, and only while the kitchen has focus; between
//! ripples nothing is drawn. On a selected item the ripple rises out of its
//! glow.
//!
//! When the shader can't be built (no GLSL 1.40), nothing is drawn and the
//! plain egui outline stays.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui_glow::{self, ShaderVersion};
use eframe::glow::{self, HasContext as _};
use egui::{Color32, Rect, Ui};

/// How far the glow reaches past the outline, in points.
const REACH: f32 = 12.0;

/// How long the shimmer plays after the selection changes, in seconds.
/// Every frame of it redraws the whole window, so it is kept short.
const BURST: f64 = 1.5;

/// Frame interval during the shimmer.
const FRAME: Duration = Duration::from_millis(16);

/// How often working items ripple, and how long one ripple takes, in seconds.
const RIPPLE_EVERY: f64 = 10.0;
const RIPPLE: f64 = 0.8;

/// Frame interval during a ripple: half the shimmer's rate, as a ripple
/// recurs (24 frames each, ~2.5% of a core every 10 s).
const RIPPLE_FRAME: Duration = Duration::from_millis(33);

/// Paint the glow around `outline` (corner radius `radius`) in `color`,
/// for `selection` (the selected item's id) in `slot` (one per place that
/// has a selection). `working`: its agent is working, so it ripples too.
/// Call it after the outline is painted and before the text inside it.
pub fn paint(ui: &Ui, slot: &str, selection: &str, outline: Rect, radius: f32, color: Color32, working: bool) {
    let now = ui.input(|i| i.time);
    let key = egui::Id::new(("shimmer", slot));
    let started = ui.data_mut(|d| match d.get_temp::<(String, f64)>(key) {
        Some((id, at)) if id == selection => at,
        _ => {
            d.insert_temp(key, (selection.to_string(), now));
            now
        }
    });
    let progress = ((now - started) / BURST).clamp(0.0, 1.0) as f32;
    if progress < 1.0 {
        ui.ctx().request_repaint_after(FRAME);
    }
    let ripple = if working { ripple(ui) } else { None };
    add(ui, outline, radius, color, Light { glow: 1.0, progress, ripple: ripple.unwrap_or(-1.0) });
}

/// The ripple of an item that isn't selected but whose agent is working:
/// nothing at all between ripples.
pub fn ripple_only(ui: &Ui, outline: Rect, radius: f32, color: Color32) {
    if let Some(ripple) = ripple(ui) {
        add(ui, outline, radius, color, Light { glow: 0.0, progress: 1.0, ripple });
    }
}

/// Where the shared ripple clock is: Some(0 to 1) during a ripple, None
/// between them or while the kitchen isn't focused. Asks for the frames the
/// ripple needs: the next one during it, else the start of the next ripple.
fn ripple(ui: &Ui) -> Option<f32> {
    // A hidden (test) kitchen never has focus; it shows what a focused one would.
    let (now, focused) = ui.input(|i| (i.time, i.focused || crate::hidden::active()));
    if !focused {
        return None; // regaining focus repaints, and that frame asks again
    }
    let phase = now % RIPPLE_EVERY;
    if phase < RIPPLE {
        ui.ctx().request_repaint_after(RIPPLE_FRAME);
        Some((phase / RIPPLE) as f32)
    } else {
        ui.ctx().request_repaint_after(Duration::from_secs_f64(RIPPLE_EVERY - phase));
        None
    }
}

/// How much of each effect to draw.
#[derive(Clone, Copy)]
struct Light {
    /// The steady glow and the selection shimmer: 1 selected, 0 not.
    glow: f32,
    /// Through the selection shimmer, 0 to 1; 1 once it is over.
    progress: f32,
    /// Through a ripple, 0 to 1; below 0 for none.
    ripple: f32,
}

fn add(ui: &Ui, outline: Rect, radius: f32, color: Color32, light: Light) {
    let params = Params { center: outline.center(), half: outline.size() / 2.0, radius, color: color.to_normalized_gamma_f32(), light };
    let callback = egui_glow::CallbackFn::new(move |info, painter| draw(painter, &info, &params));
    ui.painter().add(egui::PaintCallback { rect: outline.expand(REACH), callback: Arc::new(callback) });
}

#[derive(Clone, Copy)]
struct Params {
    center: egui::Pos2,
    half: egui::Vec2,
    radius: f32,
    color: [f32; 4],
    light: Light,
}

/// The compiled shader, built on first use; `None` inside when it failed
/// to build, so it isn't tried again every frame.
static PROGRAM: Mutex<Option<Option<Program>>> = Mutex::new(None);

struct Program {
    program: glow::Program,
    vao: glow::VertexArray,
    center: Option<glow::UniformLocation>,
    scale: Option<glow::UniformLocation>,
    half: Option<glow::UniformLocation>,
    radius: Option<glow::UniformLocation>,
    reach: Option<glow::UniformLocation>,
    color: Option<glow::UniformLocation>,
    progress: Option<glow::UniformLocation>,
    glow: Option<glow::UniformLocation>,
    ripple: Option<glow::UniformLocation>,
}

fn draw(painter: &egui_glow::Painter, info: &egui::PaintCallbackInfo, p: &Params) {
    let gl = painter.gl();
    let mut cache = PROGRAM.lock().unwrap_or_else(|e| e.into_inner());
    let Some(prog) = cache.get_or_insert_with(|| build(gl).inspect_err(|e| log::warn!("shimmer shader: {e}")).ok()) else { return };
    // SAFETY: egui_glow calls this on the thread that owns the current GL
    // context, with its framebuffer bound and premultiplied-alpha blending
    // on; it restores its own state afterwards.
    unsafe {
        gl.use_program(Some(prog.program));
        gl.bind_vertex_array(Some(prog.vao));
        // egui clamps the GL viewport to the window, so the shader finds
        // its place from gl_FragCoord (pixels from the bottom left).
        let ppp = info.pixels_per_point;
        gl.uniform_2_f32(prog.center.as_ref(), p.center.x * ppp, info.screen_size_px[1] as f32 - p.center.y * ppp);
        gl.uniform_1_f32(prog.scale.as_ref(), ppp);
        gl.uniform_2_f32(prog.half.as_ref(), p.half.x, p.half.y);
        gl.uniform_1_f32(prog.radius.as_ref(), p.radius);
        gl.uniform_1_f32(prog.reach.as_ref(), REACH);
        gl.uniform_3_f32(prog.color.as_ref(), p.color[0], p.color[1], p.color[2]);
        gl.uniform_1_f32(prog.progress.as_ref(), p.light.progress);
        gl.uniform_1_f32(prog.glow.as_ref(), p.light.glow);
        gl.uniform_1_f32(prog.ripple.as_ref(), p.light.ripple);
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.bind_vertex_array(None);
    }
}

/// Compile and link the shader. Needs GLSL 1.40 (or ES 3.00) for
/// `gl_VertexID`; macOS's core profile has it.
fn build(gl: &glow::Context) -> Result<Program, String> {
    let version = ShaderVersion::get(gl);
    if !version.is_new_shader_interface() {
        return Err(format!("{version:?} has no gl_VertexID"));
    }
    let precision = if version.is_embedded() { "precision highp float;\n" } else { "" };
    let header = format!("{}{precision}", version.version_declaration());
    // SAFETY: plain GL object creation on the current context; every object
    // made here is deleted again on the error paths.
    unsafe {
        let program = gl.create_program()?;
        let mut shaders = Vec::new();
        for (kind, src) in [(glow::VERTEX_SHADER, VERTEX), (glow::FRAGMENT_SHADER, FRAGMENT)] {
            let shader = gl.create_shader(kind)?;
            gl.shader_source(shader, &format!("{header}{src}"));
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                let log = gl.get_shader_info_log(shader);
                gl.delete_shader(shader);
                shaders.into_iter().for_each(|s| gl.delete_shader(s));
                gl.delete_program(program);
                return Err(log);
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }
        gl.link_program(program);
        for s in shaders {
            gl.detach_shader(program, s);
            gl.delete_shader(s);
        }
        if !gl.get_program_link_status(program) {
            let log = gl.get_program_info_log(program);
            gl.delete_program(program);
            return Err(log);
        }
        let vao = gl.create_vertex_array()?;
        let at = |name| gl.get_uniform_location(program, name);
        Ok(Program {
            program,
            vao,
            center: at("u_center"),
            scale: at("u_scale"),
            half: at("u_half"),
            radius: at("u_radius"),
            reach: at("u_reach"),
            color: at("u_color"),
            progress: at("u_progress"),
            glow: at("u_glow"),
            ripple: at("u_ripple"),
        })
    }
}

/// A quad over the whole viewport egui set for the callback.
const VERTEX: &str = r"
void main() {
    vec2 corner = vec2(float(gl_VertexID & 1), float(gl_VertexID >> 1));
    gl_Position = vec4(corner * 2.0 - 1.0, 0.0, 1.0);
}
";

/// Lengths are in points. The output is pure light (alpha 0), added onto
/// what egui drew below; egui works in gamma space, and so does this.
const FRAGMENT: &str = r"
out vec4 f_color;
uniform vec2 u_center;  // the outline's centre, in pixels from the bottom left
uniform float u_scale;  // pixels per point
uniform vec2 u_half;    // half the outline's size
uniform float u_radius; // the outline's corner radius
uniform float u_reach;  // how far the glow reaches past the outline
uniform vec3 u_color;
uniform float u_progress; // through the shimmer, 0 to 1
uniform float u_glow;     // the steady glow and the shimmer: 1 selected, 0 not
uniform float u_ripple;   // through a ripple, 0 to 1; below 0 for none

const float TAU = 6.2831853;

// Signed distance from p to a rounded rectangle centred on 0.
float rounded_rect(vec2 p, vec2 half_size, float r) {
    vec2 q = abs(p) - half_size + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

void main() {
    vec2 p = (gl_FragCoord.xy - u_center) / u_scale;
    p.y = -p.y;
    float d = rounded_rect(p, u_half, u_radius);
    float px = 1.0 / u_scale;

    // The shimmer fades out over its last 30%.
    float motion = 1.0 - smoothstep(0.7, 1.0, u_progress);
    // A bright spot running once round the outline, from the left.
    float angle = atan(p.y / u_half.y, p.x / u_half.x);
    float spot = motion * pow(0.5 + 0.5 * cos(angle - 3.1415927 - u_progress * TAU), 10.0);
    // A slanted sheen crossing once, left to right.
    float slant = 0.7 * u_half.y;
    float travel = 2.0 * (u_half.x + u_reach + slant);
    float head = -u_half.x - u_reach - slant + u_progress * travel;
    float sheen = motion * exp(-pow((p.x + 0.7 * p.y - head) / 12.0, 2.0));

    float outside = max(d, 0.0);
    float glow = step(0.0, d) * exp(-outside / 4.0) * (1.0 - smoothstep(0.5 * u_reach, u_reach, outside));
    float ring = 1.0 - smoothstep(0.6, 0.6 + 1.5 * px, abs(d + 0.9));
    float inside = 1.0 - smoothstep(-px, 0.0, d);

    vec3 light = u_glow * (u_color * (glow * (0.3 + 0.2 * motion + 0.55 * spot)
                                      + ring * (0.35 + 1.1 * spot)
                                      + inside * 0.04)
                           + vec3(1.0) * (ring * (0.8 * sheen + 0.35 * spot) + inside * 0.07 * sheen));

    // The ripple: the outline flares, then a thin ring leaves it and spreads
    // to most of the reach, fading as it goes.
    if (u_ripple >= 0.0) {
        float fade = (1.0 - u_ripple) * (1.0 - u_ripple);
        float at = u_ripple * 0.85 * u_reach;
        float wave = step(0.0, d) * fade * exp(-pow((d - at) / 1.4, 2.0));
        float flare = (1.0 - smoothstep(0.0, 0.35, u_ripple)) * ring;
        light += u_color * (0.9 * wave + 0.7 * flare) + vec3(1.0) * 0.25 * wave * fade;
    }
    f_color = vec4(light, 0.0);
}
";
