use chrono::Local;
use eframe::egui;
use parking_lot::RwLock;
use rdev::{listen, Event, EventType, Key};
use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::System;
use xcap::Monitor;

// --- Quality Profiles ---
#[derive(Clone, Copy, PartialEq)]
pub enum VideoQuality {
    Low,    // Fast encoding, lower bitrate
    Medium, // Standard balance
    High,   // Near visually lossless
}

impl VideoQuality {
    fn to_crf(self) -> &'static str {
        match self {
            VideoQuality::Low => "28",
            VideoQuality::Medium => "23",
            VideoQuality::High => "18",
        }
    }
}

// --- App Settings ---
#[derive(Clone)]
pub struct AppConfig {
    pub replay_duration_secs: u64,
    pub target_fps: u32,
    pub quality: VideoQuality,
    pub clip_hotkey: Key,
    pub record_toggle_hotkey: Key,
    pub screenshot_hotkey: Key,
    pub game_targets: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            replay_duration_secs: 15,
            target_fps: 30,
            quality: VideoQuality::Medium,
            clip_hotkey: Key::F9,
            record_toggle_hotkey: Key::F10,
            screenshot_hotkey: Key::F11,
            game_targets: vec![
                "minecraft".to_string(),
                "javaw.exe".to_string(),
                "sober".to_string(),
                "cs2.exe".to_string(),
                "VALORANT-Win64-Shipping.exe".to_string(),
            ],
        }
    }
}

// --- Raw Frame Storage in RAM ---
struct StoredFrame {
    timestamp: Instant,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

#[derive(PartialEq)]
enum ActiveTab {
    Settings,
    Library,
}

// --- Main App State ---
pub struct LightSnipApp {
    config: Arc<RwLock<AppConfig>>,
    is_recording: Arc<AtomicBool>,
    game_detected: Arc<AtomicBool>,
    detected_game_name: Arc<RwLock<String>>,
    buffer: Arc<RwLock<VecDeque<StoredFrame>>>,
    trigger_save_clip: Arc<AtomicBool>,
    trigger_screenshot: Arc<AtomicBool>,
    new_game_input: String,
    active_tab: ActiveTab,
    library_items: Vec<PathBuf>,
}

impl LightSnipApp {
    pub fn new(_cc: &eframe::CreationContext) -> Self {
        let config = Arc::new(RwLock::new(AppConfig::default()));
        let is_recording = Arc::new(AtomicBool::new(false));
        let game_detected = Arc::new(AtomicBool::new(false));
        let detected_game_name = Arc::new(RwLock::new(String::new()));
        let trigger_save_clip = Arc::new(AtomicBool::new(false));
        let trigger_screenshot = Arc::new(AtomicBool::new(false));
        let buffer = Arc::new(RwLock::new(VecDeque::new()));

        // 1. Lightweight Capture Loop (RAM Only)
        let cfg_clone = Arc::clone(&config);
        let rec_clone = Arc::clone(&is_recording);
        let save_clip_clone = Arc::clone(&trigger_save_clip);
        let save_shot_clone = Arc::clone(&trigger_screenshot);
        let buf_clone = Arc::clone(&buffer);

        thread::spawn(move || {
            Self::capture_loop(
                cfg_clone,
                rec_clone,
                save_clip_clone,
                save_shot_clone,
                buf_clone,
            );
        });

        // 2. Global Keybind Hooks
        let cfg_hook = Arc::clone(&config);
        let rec_hook = Arc::clone(&is_recording);
        let save_clip_hook = Arc::clone(&trigger_save_clip);
        let shot_hook = Arc::clone(&trigger_screenshot);

        thread::spawn(move || {
            let _ = listen(move |event: Event| {
                if let EventType::KeyPress(key) = event.event_type {
                    let cfg = cfg_hook.read();
                    if key == cfg.clip_hotkey {
                        println!("⚡ Clip hotkey pressed! Exporting MP4...");
                        save_clip_hook.store(true, Ordering::SeqCst);
                    } else if key == cfg.record_toggle_hotkey {
                        let state = !rec_hook.load(Ordering::SeqCst);
                        rec_hook.store(state, Ordering::SeqCst);
                        println!("🎥 Recording toggle state changed to: {}", state);
                    } else if key == cfg.screenshot_hotkey {
                        println!("📸 Screenshot hotkey pressed!");
                        shot_hook.store(true, Ordering::SeqCst);
                    }
                }
            });
        });

        // 3. Game Process Monitor
        let cfg_detect = Arc::clone(&config);
        let rec_detect = Arc::clone(&is_recording);
        let game_det_flag = Arc::clone(&game_detected);
        let game_name_store = Arc::clone(&detected_game_name);

        thread::spawn(move || {
            let mut sys = System::new_all();
            loop {
                sys.refresh_processes();
                let targets = cfg_detect.read().game_targets.clone();
                let mut matched_game: Option<String> = None;

                for process in sys.processes().values() {
                    let proc_name = process.name().to_lowercase();
                    for target in &targets {
                        if proc_name.contains(&target.to_lowercase()) {
                            matched_game = Some(process.name().to_string());
                            break;
                        }
                    }
                    if matched_game.is_some() {
                        break;
                    }
                }

                if let Some(game) = matched_game {
                    if !game_det_flag.load(Ordering::Relaxed) {
                        println!("🎮 Auto-detected game process: {}", game);
                        game_det_flag.store(true, Ordering::Relaxed);
                        rec_detect.store(true, Ordering::Relaxed);
                        *game_name_store.write() = game;
                    }
                } else if game_det_flag.load(Ordering::Relaxed) {
                    println!("⚪ Game process closed. Disabling auto-recording.");
                    game_det_flag.store(false, Ordering::Relaxed);
                    rec_detect.store(false, Ordering::Relaxed);
                    *game_name_store.write() = String::new();
                }

                thread::sleep(Duration::from_secs(2));
            }
        });

        let mut app = Self {
            config,
            is_recording,
            game_detected,
            detected_game_name,
            buffer,
            trigger_save_clip,
            trigger_screenshot,
            new_game_input: String::new(),
            active_tab: ActiveTab::Settings,
            library_items: Vec::new(),
        };

        app.refresh_library();
        app
    }

    fn refresh_library(&mut self) {
        let mut items = Vec::new();
        if let Ok(entries) = fs::read_dir("captures") {
            for entry in entries.flatten() {
                items.push(entry.path());
            }
        }
        items.sort_by(|a, b| b.cmp(a));
        self.library_items = items;
    }

    fn capture_loop(
        config: Arc<RwLock<AppConfig>>,
        is_recording: Arc<AtomicBool>,
        trigger_save_clip: Arc<AtomicBool>,
        trigger_screenshot: Arc<AtomicBool>,
        buffer: Arc<RwLock<VecDeque<StoredFrame>>>,
    ) {
        let monitors = Monitor::all().unwrap_or_default();
        let primary = match monitors.into_iter().find(|m| m.is_primary()) {
            Some(m) => m,
            None => return,
        };

        loop {
            let cfg = config.read().clone();
            let frame_delay = Duration::from_millis(1000 / cfg.target_fps as u64);
            let start_time = Instant::now();

            if is_recording.load(Ordering::Relaxed) {
                if let Ok(img) = primary.capture_image() {
                    let now = Instant::now();
                    let width = img.width();
                    let height = img.height();
                    let pixels = img.into_raw();

                    let frame = StoredFrame {
                        timestamp: now,
                        width,
                        height,
                        pixels,
                    };

                    let mut buf = buffer.write();
                    buf.push_back(frame);

                    let max_age = Duration::from_secs(cfg.replay_duration_secs);
                    while let Some(front) = buf.front() {
                        if now.duration_since(front.timestamp) > max_age {
                            buf.pop_front();
                        } else {
                            break;
                        }
                    }
                }
            } else {
                let mut buf = buffer.write();
                if !buf.is_empty() {
                    buf.clear();
                }
            }

            // Save Replay Buffer into MP4 file asynchronously
            if trigger_save_clip.swap(false, Ordering::SeqCst) {
                let frames_to_export: Vec<_> = {
                    let buf = buffer.read();
                    buf.iter()
                        .map(|f| (f.width, f.height, f.pixels.clone()))
                        .collect()
                };

                let target_fps = cfg.target_fps;
                let crf = cfg.quality.to_crf().to_string();

                thread::spawn(move || {
                    Self::encode_buffer_to_mp4(frames_to_export, target_fps, &crf);
                });
            }

            // Screenshot Trigger
            if trigger_screenshot.swap(false, Ordering::SeqCst) {
                if let Ok(img) = primary.capture_image() {
                    let _ = fs::create_dir_all("captures");
                    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
                    let shot_path = format!("captures/screenshot_{}.png", timestamp);
                    let _ = img.save(&shot_path);
                    println!("📸 Saved screenshot: {}", shot_path);
                }
            }

            let elapsed = start_time.elapsed();
            if elapsed < frame_delay {
                thread::sleep(frame_delay - elapsed);
            }
        }
    }

    // Pipe raw RGBA frame buffers into FFmpeg stdin to write MP4
    fn encode_buffer_to_mp4(frames: Vec<(u32, u32, Vec<u8>)>, fps: u32, crf: &str) {
        if frames.is_empty() {
            return;
        }

        let width = frames[0].0;
        let height = frames[0].1;

        let _ = fs::create_dir_all("captures");
        let timestamp = Local::now().format("%Y%m%d_%H%M%S");
        let output_mp4 = format!("captures/clip_{}.mp4", timestamp);

        let mut child = match Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgba",
                "-s",
                &format!("{}x{}", width, height),
                "-r",
                &fps.to_string(),
                "-i",
                "-", // Read frames from stdin
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast", // Minimal CPU impact for realtime encoding
                "-crf",
                crf,
                "-pix_fmt",
                "yuv420p",
                &output_mp4,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                eprintln!(
                    "Failed to launch FFmpeg: {}. Ensure FFmpeg is installed!",
                    e
                );
                return;
            }
        };

        if let Some(mut stdin) = child.stdin.take() {
            for (_, _, pixels) in frames {
                if stdin.write_all(&pixels).is_err() {
                    break;
                }
            }
        }

        let _ = child.wait();
        println!("🎬 Exported MP4 Clip: {}", output_mp4);
    }
}

// --- Control GUI Interface ---
impl eframe::App for LightSnipApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.active_tab, ActiveTab::Settings, "⚙ Settings");
                ui.selectable_value(&mut self.active_tab, ActiveTab::Library, "📁 Library");
            });

            ui.separator();

            match self.active_tab {
                ActiveTab::Settings => {
                    let mut cfg = self.config.write();

                    ui.horizontal(|ui| {
                        ui.label("Replay Buffer (Seconds):");
                        ui.add(egui::Slider::new(&mut cfg.replay_duration_secs, 5..=60));
                    });

                    ui.horizontal(|ui| {
                        ui.label("Target FPS:");
                        ui.selectable_value(&mut cfg.target_fps, 15, "15 FPS");
                        ui.selectable_value(&mut cfg.target_fps, 30, "30 FPS");
                        ui.selectable_value(&mut cfg.target_fps, 60, "60 FPS");
                    });

                    ui.horizontal(|ui| {
                        ui.label("Quality Profile:");
                        ui.selectable_value(&mut cfg.quality, VideoQuality::Low, "Low");
                        ui.selectable_value(&mut cfg.quality, VideoQuality::Medium, "Medium");
                        ui.selectable_value(&mut cfg.quality, VideoQuality::High, "High");
                    });

                    ui.separator();

                    ui.label("🎮 Monitored Executables:");
                    ui.horizontal(|ui| {
                        let response = ui.add(
                            egui::TextEdit::singleline(&mut self.new_game_input)
                                .hint_text("e.g. hl2.exe, sober, javaw"),
                        );
                        if (ui.button("Add").clicked()
                            || (response.lost_focus()
                                && ui.input(|i| i.key_pressed(egui::Key::Enter))))
                            && !self.new_game_input.trim().is_empty()
                        {
                            cfg.game_targets
                                .push(self.new_game_input.trim().to_string());
                            self.new_game_input.clear();
                        }
                    });

                    let mut to_remove = None;
                    egui::ScrollArea::vertical()
                        .max_height(60.0)
                        .show(ui, |ui| {
                            for (idx, game) in cfg.game_targets.iter().enumerate() {
                                ui.horizontal(|ui| {
                                    ui.label(format!("• {}", game));
                                    if ui.button("❌").clicked() {
                                        to_remove = Some(idx);
                                    }
                                });
                            }
                        });

                    if let Some(idx) = to_remove {
                        cfg.game_targets.remove(idx);
                    }

                    ui.separator();

                    ui.label(format!(
                        "• Clip Replay: [ F9 ] ({}s)",
                        cfg.replay_duration_secs
                    ));
                    ui.label("• Record Toggle: [ F10 ]");
                    ui.label("• Take Screenshot: [ F11 ]");

                    ui.separator();

                    let rec_status = self.is_recording.load(Ordering::Relaxed);
                    let game_status = self.game_detected.load(Ordering::Relaxed);
                    let active_game = self.detected_game_name.read().clone();

                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "State: {}",
                            if rec_status {
                                "🔴 RECORDING"
                            } else {
                                "⚪ IDLE"
                            }
                        ));
                        ui.separator();
                        if game_status {
                            ui.label(format!("🎮 GAME: {}", active_game));
                        } else {
                            ui.label("⚪ NO GAME MATCH");
                        }
                    });
                }
                ActiveTab::Library => {
                    ui.horizontal(|ui| {
                        ui.heading("Captured MP4s & Screenshots");
                        if ui.button("🔄 Refresh").clicked() {
                            self.refresh_library();
                        }
                    });

                    ui.separator();

                    let mut to_delete = None;
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        if self.library_items.is_empty() {
                            ui.label("No captures recorded yet.");
                        }

                        for path in &self.library_items {
                            let filename = path
                                .file_name()
                                .map(|n| n.to_string_lossy())
                                .unwrap_or_default();

                            ui.horizontal(|ui| {
                                if filename.ends_with(".mp4") {
                                    ui.label(format!("🎬 [MP4 Clip] {}", filename));
                                } else {
                                    ui.label(format!("🖼 [Image] {}", filename));
                                }

                                if ui.button("Open").clicked() {
                                    let _ = open::that(path);
                                }

                                if ui.button("🗑").clicked() {
                                    to_delete = Some(path.clone());
                                }
                            });
                        }
                    });

                    if let Some(path) = to_delete {
                        let _ = fs::remove_file(&path);
                        self.refresh_library();
                    }
                }
            }
        });

        ctx.request_repaint_after(Duration::from_millis(200));
    }
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([460.0, 440.0]),
        ..Default::default()
    };

    eframe::run_native(
        "LightSnip",
        options,
        Box::new(|cc| Box::new(LightSnipApp::new(cc))),
    )
}
