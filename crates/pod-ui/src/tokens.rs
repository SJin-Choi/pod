//! Pod design language v1. All colors are opaque sRGB except the modal scrim.
use gpui_kit::{Hsla, rgb, rgba};
use std::sync::atomic::{AtomicBool, Ordering};
static HIGH_CONTRAST: AtomicBool = AtomicBool::new(false);
/// App-local preview; never changes the Windows accessibility setting.
pub fn set_high_contrast(enabled: bool) {
    HIGH_CONTRAST.store(enabled, Ordering::Relaxed);
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Dark,
    Light,
}
#[derive(Clone, Copy, Debug)]
pub struct Tokens {
    pub bg_canvas: u32,
    pub bg_chrome: u32,
    pub bg_surface: u32,
    pub bg_raised: u32,
    pub bg_hover: u32,
    pub bg_pressed: u32,
    pub bg_selected: u32,
    pub border_subtle: u32,
    pub border_control: u32,
    pub text_primary: u32,
    pub text_secondary: u32,
    pub text_muted: u32,
    pub text_disabled: u32,
    pub accent_fg: u32,
    pub action_primary_bg: u32,
    pub action_primary_hover: u32,
    pub action_primary_pressed: u32,
    pub action_primary_fg: u32,
    pub success_fg: u32,
    pub success_bg: u32,
    pub warning_fg: u32,
    pub warning_bg: u32,
    pub danger_fg: u32,
    pub danger_bg: u32,
    pub info_fg: u32,
    pub info_bg: u32,
    pub overlay_scrim: u32,
    pub terminal_bg: u32,
    pub terminal_fg: u32,
    pub terminal_selection: u32,
    pub terminal_cursor: u32,
}
impl Tokens {
    pub fn for_mode(mode: Mode) -> Self {
        let mut tokens = match mode {
            Mode::Dark => Self {
                bg_canvas: 0x14181E,
                bg_chrome: 0x1B2129,
                bg_surface: 0x202833,
                bg_raised: 0x293340,
                bg_hover: 0x303D4D,
                bg_pressed: 0x3B4B5E,
                bg_selected: 0x263F5E,
                border_subtle: 0x354150,
                border_control: 0x73849B,
                text_primary: 0xE8EDF5,
                text_secondary: 0xB7C3D4,
                text_muted: 0xA5B3C7,
                text_disabled: 0x66758B,
                accent_fg: 0x89BAFF,
                action_primary_bg: 0x3374C6,
                action_primary_hover: 0x2D69B5,
                action_primary_pressed: 0x255B9D,
                action_primary_fg: 0xFFFFFF,
                success_fg: 0x70D6B2,
                success_bg: 0x183D33,
                warning_fg: 0xF2C078,
                warning_bg: 0x41331E,
                danger_fg: 0xFF9DA7,
                danger_bg: 0x462630,
                info_fg: 0x89BAFF,
                info_bg: 0x223B57,
                overlay_scrim: 0x080D1680,
                terminal_bg: 0x11161D,
                terminal_fg: 0xE0E8F2,
                terminal_selection: 0x304F73,
                terminal_cursor: 0xB8D4FF,
            },
            Mode::Light => Self {
                bg_canvas: 0xF6F8FB,
                bg_chrome: 0xEDF1F6,
                bg_surface: 0xFFFFFF,
                bg_raised: 0xFFFFFF,
                bg_hover: 0xE3EAF4,
                bg_pressed: 0xD4DEED,
                bg_selected: 0xDDEBFC,
                border_subtle: 0xD7DEE8,
                border_control: 0x7B8BA1,
                text_primary: 0x202B3B,
                text_secondary: 0x475970,
                text_muted: 0x58687F,
                text_disabled: 0x99A5B6,
                accent_fg: 0x235FA8,
                action_primary_bg: 0x2464AC,
                action_primary_hover: 0x1E5798,
                action_primary_pressed: 0x194B85,
                action_primary_fg: 0xFFFFFF,
                success_fg: 0x136C53,
                success_bg: 0xE2F3EC,
                warning_fg: 0x865100,
                warning_bg: 0xFFF1DA,
                danger_fg: 0xAD2940,
                danger_bg: 0xFBE8EC,
                info_fg: 0x235FA8,
                info_bg: 0xE4EFFC,
                overlay_scrim: 0x18253B40,
                terminal_bg: 0xFCFDFE,
                terminal_fg: 0x243246,
                terminal_selection: 0xCFE2F8,
                terminal_cursor: 0x235FA8,
            },
        };
        if HIGH_CONTRAST.load(Ordering::Relaxed) {
            let (bg, fg, accent, selected) = match mode {
                Mode::Dark => (0x000000, 0xFFFFFF, 0xFFFF00, 0x263F5E),
                Mode::Light => (0xFFFFFF, 0x000000, 0x003E80, 0xDDEBFC),
            };
            tokens.bg_canvas = bg;
            tokens.bg_chrome = bg;
            tokens.bg_surface = bg;
            tokens.bg_raised = bg;
            tokens.text_primary = fg;
            tokens.text_secondary = fg;
            tokens.text_muted = fg;
            tokens.border_control = fg;
            tokens.border_subtle = fg;
            tokens.accent_fg = accent;
            tokens.bg_selected = selected;
            tokens.bg_hover = selected;
            tokens.terminal_bg = bg;
            tokens.terminal_fg = fg;
            tokens.terminal_cursor = accent;
        }
        tokens
    }
}
pub fn color(hex: u32) -> Hsla {
    rgb(hex).into()
}
pub fn alpha(hex: u32) -> Hsla {
    rgba(hex).into()
}
pub const SPACE: [f32; 8] = [0., 4., 8., 12., 16., 24., 32., 48.];
pub const HALF_SPACE: f32 = 2.;
pub const RADII: [f32; 6] = [0., 4., 6., 8., 12., 999.];
pub const TREE_INDENT: f32 = 16.;
pub const ICON_SIZE: f32 = 16.;
pub const HIT_AREA: f32 = 28.;
pub const FOCUS_RING: f32 = 2.;
pub const FOCUS_OFFSET: f32 = 2.;
pub const HEIGHTS: [f32; 5] = [36., 32., 28., 24., 32.];
pub const TYPE: [(f32, f32, u16); 6] = [
    (12., 16., 400),
    (13., 20., 400),
    (13., 20., 600),
    (15., 24., 600),
    (20., 28., 600),
    (28., 36., 600),
];
pub const UI_FONTS: [&str; 4] = [
    "Segoe UI Variable Text",
    "Segoe UI",
    "Malgun Gothic",
    "Noto Sans CJK KR",
];
pub const MONO_FONTS: [&str; 2] = ["Cascadia Mono", "Consolas"];
pub fn contrast(a: u32, b: u32) -> f64 {
    fn lum(c: u32) -> f64 {
        let channel = |shift| {
            let v = ((c >> shift) & 255u32) as f64 / 255.;
            if v <= 0.04045 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(16) + 0.7152 * channel(8) + 0.0722 * channel(0)
    }
    let (a, b) = (lum(a), lum(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// Durations in ms: instant, fast, normal, slow. Reduced motion resolves all to zero.
pub const MOTION_MS: [u64; 4] = [0, 100, 160, 220];
pub const ACTIVITY_MS: u64 = 1000;
pub const EASING_ENTER: [f32; 4] = [0.16, 1., 0.3, 1.];
pub const EASING_EXIT: [f32; 4] = [0.4, 0., 1., 1.];
pub const EASING_MOVE: [f32; 4] = [0.2, 0., 0., 1.];
/// (offset y, blur, dark color, light color), no spread and no dock shadow.
pub const ELEVATION: [(f32, f32, u32, u32); 3] = [
    (0., 0., 0, 0),
    (4., 12., 0x00000040, 0x2030471f),
    (12., 32., 0x00000066, 0x2030472e),
];
pub fn elevation(level: usize, mode: Mode) -> Vec<gpui_kit::BoxShadow> {
    if level == 0 {
        return Vec::new();
    }
    let (y, blur, dark, light) = ELEVATION[level.min(2)];
    vec![gpui_kit::BoxShadow {
        inset: false,
        color: alpha(if mode == Mode::Dark { dark } else { light }),
        offset: gpui_kit::point(gpui_kit::px(0.), gpui_kit::px(y)),
        blur_radius: gpui_kit::px(blur),
        spread_radius: gpui_kit::px(0.),
    }]
}
