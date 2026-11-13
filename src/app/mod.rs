//! The app window beside the game: a hotkey captures the game, the reader
//! reads the board and the solver ranks the moves.

pub mod fps;
pub mod health;
pub mod latency;
pub mod runlog;
pub mod session;
pub mod settings;
pub mod settle;
pub mod vorax_session;
pub mod vorax_settle;
pub mod vorax_learn;
#[cfg(windows)]
pub mod share;
#[cfg(windows)]
pub mod capture;
#[cfg(windows)]
pub mod mark;
#[cfg(windows)]
pub mod report;
#[cfg(windows)]
pub mod run_review;
#[cfg(windows)]
pub mod elevation;
#[cfg(windows)]
pub mod gamelog;
#[cfg(windows)]
pub mod hotkey;
#[cfg(windows)]
pub mod http;
#[cfg(windows)]
pub mod install;
#[cfg(windows)]
pub mod theme;
#[cfg(windows)]
pub mod through;
#[cfg(windows)]
pub mod ui;
// The window's switch to the page of a game mode that just came up (1.3.0).
#[cfg(windows)]
pub mod auto_tab;
#[cfg(windows)]
pub mod update;
#[cfg(windows)]
pub mod vorax_view;
// The affix frames' window, the frames live on the game, the affix page (public since 1.2.0).
#[cfg(windows)]
pub mod affix_overlay;
#[cfg(windows)]
pub mod affix_live;
#[cfg(windows)]
pub mod affix_page;
// Private builds only: map page frames as samples for the affix reader (not exported to the public repository).
#[cfg(all(windows, feature = "dev-samples"))]
pub mod affix_samples;

/// Opens the window, with `preload` read first when given.
#[cfg(windows)]
pub fn run(preload: Option<&std::path::Path>) -> Result<(), String> {
    use eframe::egui;
    update::wait_for_previous();
    let current = std::env::current_exe().ok();
    let mut installed = false;
    if let Some(current) = &current {
        update::clean_old(current);
        installed = install::installed_here(current);
        if installed {
            let _ = install::register(current);
        }
    }
    let mut settings = settings::Settings::load();
    // "Start as administrator": start again as administrator and end here once that copy started; declined at
    // Windows' prompt (or failed), carry on without and let the window say so. A copy started again never does this.
    if elevation::should_relaunch(settings.start_as_admin, elevation::own_elevated(), elevation::relaunched()) {
        match elevation::relaunch() {
            elevation::Relaunch::Started => return Ok(()),
            other => elevation::set_at_start(other),
        }
    }
    // Before any run log is made (the window's RunLog and the Vorax session's are made in AscendantApp::new):
    // until now every run log is kept.
    runlog::set_keep_all(settings.keep_all_logs);
    // the settings each run log's window line names (and a `settings` row on a change, see runlog::set_settings)
    runlog::set_settings(settings.log_summary());
    crate::lang::set(crate::lang::Lang::from_key(&settings.language));
    if let Some(current) = current.as_ref().filter(|_| installed && !settings.desktop_shortcut) {
        let _ = install::desktop_shortcut(current);
        settings.desktop_shortcut = true;
        let _ = settings.save();
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Ascendant")
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon.png")).expect("assets/icon.png is a PNG"))
            // Default 296×520 when no size was saved: the window no longer auto-grows, and this height fits the whole
            // Tartarus board page.
            .with_inner_size(settings.window_size.filter(|&[w, h]| w >= 200.0 && h >= 150.0).unwrap_or([296.0, 520.0]))
            .with_always_on_top()
            .with_resizable(true),
        ..Default::default()
    };
    eframe::run_native("Ascendant", options, Box::new(move |cc| Ok(Box::new(ui::AscendantApp::new(cc, settings, preload, current, installed)))))
        .map_err(|e| e.to_string())
}
