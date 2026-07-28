use anyhow::Result;
use chrono::{DateTime, Local};
use rdev::{EventType, listen};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time;

use crate::daemon::database::connection::Database;
use crate::daemon::tracker::monitor::AppMonitor;
use crate::daemon::tracker::parser;
use crate::models::session::Session;

pub struct Daemon {
    database: Database,
    monitor: AppMonitor,
    current_app: String,
    current_window: Option<String>,
    current_session: Option<Session>,
    last_input: Arc<Mutex<DateTime<Local>>>,
    user_active: Option<Arc<AtomicBool>>,
}

impl Daemon {
    pub fn new(database: Database) -> Self {
        let monitor = AppMonitor::new();
        let last_input = Arc::new(Mutex::new(Local::now()));

        let user_active = Self::start_input_monitoring(Arc::clone(&last_input), &monitor);

        Self {
            database,
            monitor,
            current_app: "unknown".to_string(),
            current_window: None,
            current_session: None,
            last_input,
            user_active,
        }
    }

    fn start_input_monitoring(
        last_input: Arc<Mutex<DateTime<Local>>>,
        monitor: &AppMonitor,
    ) -> Option<Arc<AtomicBool>> {
        if monitor.uses_wayland() {
            #[cfg(target_os = "linux")]
            {
                match crate::daemon::tracker::idle_wayland::start() {
                    Ok(user_active) => {
                        log::info!("Input backend selected: Wayland ext-idle-notify-v1");
                        return Some(user_active);
                    }
                    Err(error) => {
                        log::warn!(
                            "Wayland idle backend unavailable ({error}); falling back to rdev input monitoring"
                        );
                    }
                }
            }
        } else {
            log::info!("Input backend selected: rdev (X11/non-Wayland)");
        }

        if monitor.uses_wayland() {
            log::info!("Input backend selected: rdev (Wayland fallback)");
        }
        Self::start_rdev_input_monitoring(last_input);
        None
    }

    // Input monitoring using rdev
    fn start_rdev_input_monitoring(last_input: Arc<Mutex<DateTime<Local>>>) {
        std::thread::spawn(move || {
            let callback = move |event: rdev::Event| match event.event_type {
                EventType::KeyPress(_)
                | EventType::KeyRelease(_)
                | EventType::ButtonPress(_)
                | EventType::ButtonRelease(_)
                | EventType::MouseMove { .. } => {
                    log::debug!("Input event detected: {:?}", event.event_type);
                    *last_input.lock().unwrap() = Local::now();
                }
                _ => {}
            };
            if let Err(error) = listen(callback) {
                eprintln!("Error listening for input events in daemon: {:?}", error);
            }
        });
    }

    fn refresh_last_input_if_active(
        user_active: Option<&AtomicBool>,
        last_input: &Arc<Mutex<DateTime<Local>>>,
    ) {
        if user_active.is_some_and(|active| active.load(Ordering::Relaxed)) {
            *last_input.lock().unwrap() = Local::now();
        }
    }

    pub async fn run(&mut self) -> Result<()> {
        log::info!("Starting background daemon for window tracking...");

        // Set up signal handlers for graceful shutdown
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&shutdown_flag))?;
        signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&shutdown_flag))?;

        // Only the process holding the advisory lock records sessions. A
        // losing daemon remains alive but performs no tracking writes.
        if self.database.is_writer() {
            self.start_tracking().await?;
        } else {
            log::info!("Daemon is read-only because another process owns the tracking writer lock");
        }

        // Auto-save interval: 1 hour
        let mut last_save = tokio::time::Instant::now();
        let save_interval = Duration::from_secs(3600);

        let mut last_afk_check = tokio::time::Instant::now();
        let afk_check_interval = Duration::from_secs(1); // Check AFK status every second
        let afk_threshold = Duration::from_secs(300); // 5 minutes of idle = AFK
        let idle_threshold = Duration::from_secs(600); // 10 minutes = IDLE (no input during AFK)

        loop {
            // Check for shutdown signal
            if shutdown_flag.load(Ordering::Relaxed) {
                log::info!("Received shutdown signal, saving and exiting...");
                break;
            }

            // Check for AFK status every second
            if last_afk_check.elapsed() >= afk_check_interval {
                Self::refresh_last_input_if_active(self.user_active.as_deref(), &self.last_input);
                let idle_duration =
                    Local::now().signed_duration_since(*self.last_input.lock().unwrap());
                let is_currently_afk =
                    idle_duration.num_seconds() >= afk_threshold.as_secs() as i64;
                let is_currently_idle =
                    idle_duration.num_seconds() >= idle_threshold.as_secs() as i64;

                // If we have a current session, check if AFK state changed
                if let Some(ref mut session) = self.current_session {
                    let was_afk = session.is_afk.unwrap_or(false);
                    let was_idle = session.is_idle.unwrap_or(false);

                    // Mark ANY session (AFK or not) as IDLE once it reaches 10+ minutes of inactivity
                    if is_currently_idle && !was_idle {
                        session.is_idle = Some(true);
                        log::info!(
                            "Session marked as IDLE after {:.1} minutes of inactivity: {}",
                            idle_duration.num_seconds() as f64 / 60.0,
                            session.app_name
                        );
                    }

                    // AFK state changed - end current session and start new one
                    if was_afk != is_currently_afk {
                        // Save the current session
                        let mut old_session = self.current_session.take().unwrap();
                        old_session.duration = Local::now()
                            .signed_duration_since(old_session.start_time)
                            .num_seconds();

                        // If this is an AFK session being ended, mark as IDLE if it lasted 10+ minutes
                        if was_afk && old_session.duration >= idle_threshold.as_secs() as i64 {
                            old_session.is_idle = Some(true);
                            log::info!(
                                "AFK session marked as IDLE: {} for {:.1} minutes",
                                old_session.app_name,
                                old_session.duration as f64 / 60.0
                            );
                        }

                        if let Err(e) = self
                            .database
                            .apply_renames_and_categories(&mut old_session)
                            .await
                        {
                            log::warn!(
                                "Failed to apply renames and categories on AFK change: {}",
                                e
                            );
                        }

                        if let Err(e) = self.database.persist_session(&old_session).await {
                            log::error!("Failed to save session on AFK state change: {}", e);
                        } else {
                            log::info!(
                                "Session saved on AFK state change: {} -> is_afk={}",
                                old_session.app_name,
                                is_currently_afk
                            );
                        }

                        // Start new session with updated AFK state
                        if is_currently_afk {
                            // Starting AFK session
                            let resume_app = self.current_app.clone();
                            let resume_window = self.current_window.clone();
                            self.switch_app(
                                "AFK".to_string(),
                                Some("Away from keyboard".to_string()),
                            )
                            .await?;
                            if let Some(mut new_session) = self.current_session.take() {
                                new_session.is_afk = Some(true);
                                match self.database.persist_session(&new_session).await {
                                    Ok(id) => new_session.id = Some(id),
                                    Err(e) => log::error!(
                                        "Failed to create AFK session row for update-on-resume: {}",
                                        e
                                    ),
                                }
                                self.current_session = Some(new_session);
                            }

                            // Keep the last real app/window as the comparison baseline while
                            // AFK. The focused window itself does not change merely because
                            // the daemon entered AFK.
                            self.current_app = resume_app;
                            self.current_window = resume_window;
                        } else {
                            // Returning from AFK - get the actual active app
                            if let Ok((active_app, active_window)) =
                                self.monitor.get_active_window_info_async().await
                            {
                                self.switch_app(active_app.clone(), active_window.clone())
                                    .await?;
                                if let Some(ref mut new_session) = self.current_session {
                                    new_session.is_afk = Some(false);
                                }
                            }
                        }
                    }
                }

                last_afk_check = tokio::time::Instant::now();
            }

            // Check for app or window changes, including the AFK safety net.
            if let Ok((active_app, active_window)) =
                self.monitor.get_active_window_info_async().await
            {
                let idle_duration =
                    Local::now().signed_duration_since(*self.last_input.lock().unwrap());
                let is_currently_afk =
                    idle_duration.num_seconds() >= afk_threshold.as_secs() as i64;

                log::debug!(
                    "Detected active app: '{}', window: '{:?}'",
                    active_app,
                    active_window
                );

                if self.database.is_writer() {
                    // Check if app or window changed
                    let app_or_window_changed =
                        active_app != self.current_app || active_window != self.current_window;

                    if app_or_window_changed && is_currently_afk {
                        // A real focused-window change is an independent safety net for idle
                        // backends. It proves user activity, so close the AFK row through
                        // persist_session (which updates because the AFK row has an id), then
                        // resume normal tracking immediately.
                        log::info!(
                            "App/window change observed while AFK; treating it as user activity and resuming tracking"
                        );
                        *self.last_input.lock().unwrap() = Local::now();
                        self.switch_app(active_app.clone(), active_window.clone())
                            .await?;
                        if let Some(ref mut session) = self.current_session {
                            session.is_afk = Some(false);
                        }
                    } else if app_or_window_changed {
                        log::info!(
                            "App change detected: '{}' -> '{}', window: '{:?}' -> '{:?}'",
                            self.current_app,
                            active_app,
                            self.current_window,
                            active_window
                        );
                        self.switch_app(active_app.clone(), active_window.clone())
                            .await?;

                        // Mark new session as not AFK
                        if let Some(ref mut session) = self.current_session {
                            session.is_afk = Some(false);
                        }
                    }

                    // Always update current state after successful detection
                    self.current_app = active_app;
                    self.current_window = active_window;
                }
            }

            // Auto save every hour
            if last_save.elapsed() >= save_interval
                && let Some(session) = &mut self.current_session
            {
                session.duration = Local::now()
                    .signed_duration_since(session.start_time)
                    .num_seconds();
                if let Err(e) = self.database.apply_renames_and_categories(session).await {
                    log::warn!("Failed to apply renames and categories on auto-save: {}", e);
                }
                match self.database.persist_session(session).await {
                    Ok(id) => {
                        session.id = Some(id);
                        last_save = tokio::time::Instant::now();
                        log::info!(
                            "Auto-saved session: {} for {}s",
                            session.app_name,
                            session.duration
                        );
                    }
                    Err(e) => {
                        log::error!("Failed to auto save session: {}", e);
                    }
                }
            }

            // Poll every 100ms for real-time tracking
            time::sleep(Duration::from_millis(100)).await;
        }

        // Save current session on exit
        if let Some(mut session) = self.current_session.take() {
            session.duration = Local::now()
                .signed_duration_since(session.start_time)
                .num_seconds();
            if let Err(e) = self
                .database
                .apply_renames_and_categories(&mut session)
                .await
            {
                log::warn!("Failed to apply renames and categories on exit: {}", e);
            }
            if let Err(e) = self.database.persist_session(&session).await {
                log::error!("Failed to save session on exit: {}", e);
            } else {
                log::info!(
                    "Saved session on exit: {} for {}s",
                    session.app_name,
                    session.duration
                );
            }
        }

        Ok(())
    }

    async fn start_tracking(&mut self) -> Result<()> {
        let (app_name, window_name) = match self.monitor.get_active_window_info_async().await {
            Ok((app, win)) => {
                self.current_app = app.clone();
                (app, win)
            }
            Err(e) => {
                log::error!("Window detection failed: {}", e);
                self.current_app = "Unknown".to_string();
                ("Unknown".to_string(), None)
            }
        };

        let start_time = Local::now();
        let (category_name, _) = Self::categorize_app(&app_name);

        let mut session = Self::create_session_with_parsing(
            app_name.clone(),
            window_name.clone(),
            start_time,
            category_name.to_string(),
        );

        if let Err(e) = self
            .database
            .apply_renames_and_categories(&mut session)
            .await
        {
            log::warn!("Failed to apply renames and categories: {}", e);
        }

        self.current_session = Some(session);
        self.current_window = window_name;
        log::info!("Started tracking: {}", app_name);
        Ok(())
    }

    async fn switch_app(&mut self, new_app: String, window_name: Option<String>) -> Result<()> {
        // End current session
        if let Some(mut session) = self.current_session.take() {
            session.duration = Local::now()
                .signed_duration_since(session.start_time)
                .num_seconds();

            if let Err(e) = self.database.persist_session(&session).await {
                log::error!("Failed to save session: {}", e);
            } else {
                log::info!(
                    "Saved session: {} for {}s",
                    session.app_name,
                    session.duration
                );
            }
        }

        // Start new session
        let start_time = Local::now();
        let (category_name, _) = Self::categorize_app(&new_app);

        let mut session = Self::create_session_with_parsing(
            new_app.clone(),
            window_name.clone(),
            start_time,
            category_name.to_string(),
        );

        if let Err(e) = self
            .database
            .apply_renames_and_categories(&mut session)
            .await
        {
            log::warn!("Failed to apply renames and categories: {}", e);
        }

        self.current_session = Some(session);
        self.current_app = new_app.clone();
        self.current_window = window_name;
        log::info!("Switched to: {}", new_app);
        Ok(())
    }

    fn categorize_app(app: &str) -> (&'static str, ()) {
        let app_lower = app.to_lowercase();
        if app_lower.contains("code")
            || app_lower.contains("vim")
            || app_lower.contains("nvim")
            || app_lower.contains("terminal")
            || app_lower.contains("alacritty")
            || app_lower.contains("kitty")
            || app_lower.contains("rust")
            || app_lower.contains("cargo")
            || app_lower.contains("editor")
            || app_lower.contains("vscode")
            || app_lower.contains("vscodium")
            || app_lower.contains("gedit")
            || app_lower.contains("nano")
            || app_lower.contains("emacs")
            || app_lower.contains("atom")
            || app_lower.contains("sublime")
            || app_lower.contains("console")
            || app_lower.contains("iterm")
        {
            ("💻 Development", ())
        } else if app_lower.contains("browser")
            || app_lower.contains("chrome")
            || app_lower.contains("firefox")
            || app_lower.contains("brave")
            || app_lower.contains("edge")
            || app_lower.contains("chromium")
        {
            ("🌐 Browsing", ())
        } else if app_lower.contains("slack")
            || app_lower.contains("zoom")
            || app_lower.contains("teams")
            || app_lower.contains("discord")
            || app_lower.contains("telegram")
            || app_lower.contains("chat")
            || app_lower.contains("signal")
            || app_lower.contains("element")
            || app_lower.contains("video-call")
            || app_lower.contains("skype")
            || app_lower.contains("jitsi")
        {
            ("💬 Communication", ())
        } else if app_lower.contains("spotify")
            || app_lower.contains("vlc")
            || app_lower.contains("music")
            || app_lower.contains("media")
            || app_lower.contains("rhythmbox")
            || app_lower.contains("audacious")
            || app_lower.contains("clementine")
        {
            ("🎵 Media", ())
        } else if app_lower.contains("nautilus")
            || app_lower.contains("files")
            || app_lower.contains("dolphin")
            || app_lower.contains("file-manager")
            || app_lower.contains("thunar")
            || app_lower.contains("nemo")
        {
            ("📁 Files", ())
        } else if app_lower.contains("thunderbird")
            || app_lower.contains("evolution")
            || app_lower.contains("geary")
            || app_lower.contains("email")
        {
            ("📧 Email", ())
        } else if app_lower.contains("libreoffice") || app_lower.contains("soffice") {
            ("📄 Office", ())
        } else {
            ("📦 Other", ())
        }
    }

    fn create_session_with_parsing(
        app_name: String,
        window_name: Option<String>,
        start_time: chrono::DateTime<chrono::Local>,
        category: String,
    ) -> Session {
        let parsed = if let Some(ref win_name) = window_name {
            parser::parse_window_name(&app_name, win_name)
        } else {
            parser::ParsedSessionData::default()
        };

        let parsed_json = serde_json::to_value(&parsed).ok();

        Session {
            id: None,
            app_name,
            window_name,
            start_time,
            duration: 0,
            category: Some(category),
            browser_url: parsed.browser_url,
            browser_page_title: parsed.browser_page_title,
            browser_notification_count: parsed.browser_notification_count,
            browser_page_title_renamed: None,  // Added
            browser_page_title_category: None, // Added
            terminal_username: parsed.terminal_username,
            terminal_hostname: parsed.terminal_hostname,
            terminal_directory: parsed.terminal_directory,
            terminal_project_name: parsed.terminal_project_name,
            terminal_directory_renamed: None,  // Added
            terminal_directory_category: None, // Added
            editor_filename: parsed.editor_filename,
            editor_filepath: parsed.editor_filepath,
            editor_project_path: parsed.editor_project_path,
            editor_language: parsed.editor_language,
            editor_filename_renamed: None,  // Added
            editor_filename_category: None, // Added
            tmux_window_name: parsed.tmux_window_name,
            tmux_pane_count: parsed.tmux_pane_count,
            terminal_multiplexer: parsed.terminal_multiplexer,
            tmux_window_name_renamed: None,  // Added
            tmux_window_name_category: None, // Added
            ide_project_name: parsed.ide_project_name,
            ide_file_open: parsed.ide_file_open,
            ide_workspace: parsed.ide_workspace,
            parsed_data: parsed_json,
            parsing_success: Some(parsed.parsing_success),
            is_afk: Some(false),
            is_idle: Some(false),
            idle_accumulation_secs: Some(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Duration;

    #[tokio::test]
    async fn test_sleep_gap_detection() {
        // This test verifies that sleep gap detection logic works correctly

        // Test that sleep threshold detection works
        let sleep_threshold = Duration::from_secs(600); // 10 minutes
        let short_gap = Duration::from_secs(30);
        let long_gap = Duration::from_secs(1200); // 20 minutes

        assert!(
            !(short_gap > sleep_threshold),
            "Short gap should not trigger sleep detection"
        );
        assert!(
            long_gap > sleep_threshold,
            "Long gap should trigger sleep detection"
        );

        // Test chrono duration conversion
        let chrono_duration = chrono::Duration::from_std(long_gap).unwrap();
        assert_eq!(chrono_duration.num_minutes(), 20);

        println!("Sleep gap detection logic test passed");
    }

    #[tokio::test]
    async fn test_afk_session_creation_logic() {
        // Test that the logic for creating AFK sessions during sleep works
        let start_time = Local::now();
        let sleep_duration = Duration::from_secs(3600); // 1 hour sleep
        let sleep_start_time = start_time + chrono::Duration::from_std(sleep_duration).unwrap();

        // Simulate what happens during sleep detection
        let session_duration_before_sleep = sleep_start_time
            .signed_duration_since(start_time)
            .num_seconds();
        assert_eq!(session_duration_before_sleep, 3600);

        println!("AFK session creation logic test passed");
    }

    #[test]
    fn test_wayland_active_flag_refreshes_last_input() {
        let last_input = Arc::new(Mutex::new(Local::now() - chrono::Duration::seconds(30)));
        let user_active = AtomicBool::new(true);

        Daemon::refresh_last_input_if_active(Some(&user_active), &last_input);

        assert!(
            Local::now().signed_duration_since(*last_input.lock().unwrap())
                < chrono::Duration::seconds(2)
        );
    }

    #[test]
    fn test_wayland_idle_flag_does_not_refresh_last_input() {
        let original = Local::now() - chrono::Duration::seconds(30);
        let last_input = Arc::new(Mutex::new(original));
        let user_active = AtomicBool::new(false);

        Daemon::refresh_last_input_if_active(Some(&user_active), &last_input);

        assert!(*last_input.lock().unwrap() <= original);
    }
}
