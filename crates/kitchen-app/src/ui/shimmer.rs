//! The animated glow around the selected tab and workspace: an OpenGL
//! fragment shader, run as an egui paint callback. Around the outline it
//! draws a soft glow that breathes, a bright spot that runs round the
//! outline, and a slanted sheen that sweeps across it now and then.
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

/// Frame interval while the window has focus: 30 fps. Every frame redraws
/// the whole window (about 10% of a core at 30 fps, twice that at 60), and
/// the motion is slow enough for 30. Without focus the animation stops.
const FRAME: Duration = Duration::from_millis(33);

/// The shader's clock wraps here: a multiple of every period in it (3 s,
/// 2.5 s), so the wrap doesn't show, and small enough for f32.
const CLOCK_WRAP: f64 = 600.0;

/// Paint the glow around `outline` (corner radius `radius`) in `color`.
/// Call it after the outline is painted and before the text inside it.
pub fn paint(ui: &Ui, outline: Rect, radius: f32, color: Color32) {
    let time = (ui.input(|i| i.time) % CLOCK_WRAP) as f32;
    let params = Params { center: outline.center(), half: outline.size() / 2.0, radius, color: color.to_normalized_gamma_f32(), time };
    let callback = egui_glow::CallbackFn::new(move |info, painter| draw(painter, &info, &params));
    ui.painter().add(egui::PaintCallback { rect: outline.expand(REACH), callback: Arc::new(callback) });
    if ui.input(|i| i.viewport().focused.unwrap_or(true)) {
        ui.ctx().request_repaint_after(FRAME);
    }
}

#[derive(Clone, Copy)]
struct Params {
    center: egui::Pos2,
    half: egui::Vec2,
    radius: f32,
    color: [f32; 4],
    time: f32,
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
    time: Option<glow::UniformLocation>,
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
        gl.uniform_1_f32(prog.time.as_ref(), p.time);
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
            time: at("u_time"),
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
uniform float u_time;   // seconds

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

    // Breathing: the whole glow swells and fades every 3 s.
    float breathe = 0.75 + 0.25 * sin(u_time * TAU / 3.0);
    // A bright spot running round the outline every 2.5 s.
    float angle = atan(p.y / u_half.y, p.x / u_half.x);
    float spot = pow(0.5 + 0.5 * cos(angle - u_time * TAU / 2.5), 10.0);
    // A slanted sheen crossing left to right, then a pause, every 3 s.
    float travel = 2.0 * u_half.x + 2.0 * u_reach;
    float head = -u_half.x - u_reach + fract(u_time / 3.0) * travel * 1.6;
    float sheen = exp(-pow((p.x + 0.7 * p.y - head) / 12.0, 2.0));

    float outside = max(d, 0.0);
    float glow = step(0.0, d) * exp(-outside / 4.0) * (1.0 - smoothstep(0.5 * u_reach, u_reach, outside));
    float ring = 1.0 - smoothstep(0.6, 0.6 + 1.5 * px, abs(d + 0.9));
    float inside = 1.0 - smoothstep(-px, 0.0, d);

    vec3 light = u_color * (glow * (0.32 * breathe + 0.55 * spot)
                            + ring * (0.35 + 1.1 * spot)
                            + inside * 0.05 * breathe)
               + vec3(1.0) * (ring * (0.8 * sheen + 0.35 * spot) + inside * 0.07 * sheen);
    f_color = vec4(light, 0.0);
}
";
