//! Persisted host appearance and the UI/VT palette boundary.
use gpui_kit::component::theme::Theme;
use gpui_kit::*;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
static LIGHT: AtomicBool = AtomicBool::new(false);
pub fn mode() -> pod_ui::Mode {
    if LIGHT.load(Ordering::Relaxed) { pod_ui::Mode::Light } else { pod_ui::Mode::Dark }
}
pub fn tokens() -> pod_ui::Tokens {
    pod_ui::Tokens::for_mode(mode())
}

/// Windows "Contrast themes" is on (`SPI_GETHIGHCONTRAST`, `HCF_HIGHCONTRASTON`).
/// Read only; Pod never changes the system setting.
pub fn os_high_contrast() -> bool {
    #[repr(C)]
    struct HighContrastW {
        cb_size: u32,
        flags: u32,
        default_scheme: *mut u16,
    }
    const HCF_HIGHCONTRASTON: u32 = 1;
    let mut hc = HighContrastW { cb_size: std::mem::size_of::<HighContrastW>() as u32, flags: 0, default_scheme: std::ptr::null_mut() };
    // SAFETY: `hc` is a correctly sized HIGHCONTRASTW that outlives the call.
    let ok = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::SystemParametersInfoW(
            windows_sys::Win32::UI::WindowsAndMessaging::SPI_GETHIGHCONTRAST,
            hc.cb_size,
            (&mut hc as *mut HighContrastW).cast(),
            0,
        )
    };
    ok != 0 && hc.flags & HCF_HIGHCONTRASTON != 0
}

/// The OS contrast theme forces the high-contrast tokens; the in-app
/// preview can turn them on without it.
pub fn effective_high_contrast(setting: bool, os: bool) -> bool {
    setting || os
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub collapsed: std::collections::BTreeSet<String>,
    pub theme: String,
    pub font: String,
    pub font_size: f32,
    pub terminal_font: String,
    pub terminal_font_size: f32,
    pub recents: Vec<String>,
    pub comfortable: bool,
    pub reduced_motion: bool,
    pub high_contrast: bool,
    pub jump_shortcut: String,
    /// Agent hook prompt dismissed ("나중에") for this app + pod-hook build.
    pub hooks_dismissed: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            collapsed: Default::default(),
            theme: "system".into(),
            font: "Segoe UI".into(),
            font_size: 13.,
            terminal_font: "Cascadia Mono".into(),
            terminal_font_size: 14.,
            recents: Vec::new(),
            comfortable: false,
            reduced_motion: false,
            high_contrast: false,
            jump_shortcut: "ctrl-k".into(),
            hooks_dismissed: String::new(),
        }
    }
}
impl Global for Settings {}
/// The OS contrast flag the current tokens were built with.
#[derive(Clone, Copy, Default)]
pub struct OsHighContrast(pub bool);
impl Global for OsHighContrast {}

/// Re-applies the settings when the OS contrast flag changed (Windows sends
/// no dedicated GPUI event: checked on appearance change and activation).
/// Returns whether the tokens changed.
pub fn refresh_os_high_contrast(query: impl Fn() -> bool, cx: &mut App) -> bool {
    let now = query();
    if cx.try_global::<OsHighContrast>().is_some_and(|g| g.0 == now) {
        return false;
    }
    let settings = cx.try_global::<Settings>().cloned().unwrap_or_default();
    settings.apply_with(now, cx);
    true
}
impl Settings {
    pub fn load() -> Self {
        std::fs::read(crate::layout::config_dir().join("settings.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    pub fn save(&self) -> std::io::Result<()> {
        let dir = crate::layout::config_dir();
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join("settings.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, dir.join("settings.json"))
    }
    pub fn apply(&self, cx: &mut App) {
        self.apply_with(os_high_contrast(), cx);
    }
    /// [`Self::apply`] with the OS contrast flag given (tests inject it).
    pub fn apply_with(&self, os_high_contrast: bool, cx: &mut App) {
        crate::strings::init_toolkit_locale();
        let light = match self.theme.as_str() {
            "light" => true,
            "dark" => false,
            _ => matches!(cx.window_appearance(), WindowAppearance::Light | WindowAppearance::VibrantLight),
        };
        LIGHT.store(light, Ordering::Relaxed);
        pod_ui::tokens::set_high_contrast(effective_high_contrast(self.high_contrast, os_high_contrast));
        cx.set_global(OsHighContrast(os_high_contrast));
        pod_ui::apply_theme(mode(), self.reduced_motion, cx);
        Theme::update(cx, |t| {
            t.font_family = self.font.clone().into();
            t.font_size = px(self.font_size.clamp(10., 24.));
            t.mono_font_family = self.terminal_font.clone().into();
            t.mono_font_size = px(self.terminal_font_size.clamp(10., 28.));
        });
        cx.set_global(self.clone());
    }
}
pub fn terminal_palette() -> pod_terminal_view::TerminalPalette {
    let p = pod_ui::terminal::TerminalPalette::for_mode(mode());
    pod_terminal_view::TerminalPalette { foreground: p.foreground, background: p.background, cursor: p.cursor, selection_bg: p.selection, ansi: p.ansi }
}

#[cfg(test)]
mod tests {
    use super::Settings;

    #[test]
    fn os_contrast_theme_forces_high_contrast_tokens() {
        use super::effective_high_contrast as eff;
        assert!(!eff(false, false));
        assert!(eff(false, true), "OS contrast theme alone switches tokens");
        assert!(eff(true, false), "in-app preview alone switches tokens");
        assert!(eff(true, true));
    }

    /// The OS query is injected: the real one is never changed by tests.
    #[test]
    fn app_switches_tokens_when_the_os_reports_high_contrast() {
        use ::gpui::{TestAppContext, TestDispatcher};
        let cx = TestAppContext::build(TestDispatcher::new(0), None);
        cx.update(|cx| {
            gpui_kit::init(cx);
            let settings = Settings { theme: "dark".into(), ..Default::default() };
            settings.apply_with(false, cx);
            let normal = super::tokens();
            assert_ne!(normal.bg_canvas, 0x000000);
            // OS turns a contrast theme on: next activation/appearance check.
            assert!(super::refresh_os_high_contrast(|| true, cx), "tokens rebuilt");
            let hc = super::tokens();
            assert_eq!((hc.bg_canvas, hc.text_primary, hc.accent_fg), (0x000000, 0xFFFFFF, 0xFFFF00), "dark high-contrast tokens");
            assert!(!super::refresh_os_high_contrast(|| true, cx), "unchanged flag: no rebuild");
            // The user's preview setting is untouched by the OS flag.
            assert!(!cx.global::<Settings>().high_contrast);
            // OS turns it off again.
            assert!(super::refresh_os_high_contrast(|| false, cx));
            assert_eq!(super::tokens().bg_canvas, normal.bg_canvas);
        });
        cx.update(|cx| cx.quit());
    }
    #[test]
    fn old_settings_get_independent_font_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"theme":"dark","font":"Malgun Gothic"}"#).unwrap();
        assert_eq!(settings.font, "Malgun Gothic");
        assert_eq!(settings.font_size, 13.);
        assert_eq!(settings.terminal_font, "Cascadia Mono");
        assert_eq!(settings.terminal_font_size, 14.);
        assert!(settings.recents.is_empty());
    }
}
