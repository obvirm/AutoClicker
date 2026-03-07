use eframe::egui;
use image::GenericImageView;
use rand::Rng;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};
use std::path::PathBuf;
use walkdir::WalkDir;
use rdev::{listen, EventType, Key};
use xcap::Window;


#[cfg(windows)]
use windows_sys::Win32::Foundation::*;
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    ChildWindowFromPoint,
    GetClientRect,
    SendNotifyMessageW,
    WM_LBUTTONDOWN, WM_LBUTTONUP,
    ShowWindowAsync, SW_RESTORE,
    FindWindowW, SetWindowPos, GetSystemMetrics,
    SWP_NOACTIVATE, SWP_ASYNCWINDOWPOS,
    LWA_ALPHA,
    HWND_TOPMOST, SetLayeredWindowAttributes,
    GetWindowLongW, SetWindowLongW, 
    GWL_STYLE, GWL_EXSTYLE,
    WS_POPUP, WS_VISIBLE,
    WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
};
#[cfg(windows)]
use windows_sys::Win32::Graphics::Gdi::*;
#[cfg(windows)]
use windows_sys::Win32::Graphics::Dwm::*;
#[cfg(windows)]
use windows_sys::Win32::UI::Controls::MARGINS;

type WindowId = u64;

#[cfg(windows)]
type HDC = isize;

#[cfg(windows)]
#[link(name = "user32")]
unsafe extern "system" {
    fn PrintWindow(hwnd: HWND, hdc_blt: HDC, n_flags: u32) -> BOOL;
    fn ScreenToClient(hwnd: HWND, lp_point: *mut POINT) -> BOOL;
    fn ClientToScreen(hwnd: HWND, lp_point: *mut POINT) -> BOOL;
}

// --- Communication Types ---

#[derive(Clone, Copy)]
enum LogLevel {
    Info,
    Success,
    Warning,
}

struct LogEntry {
    level: LogLevel,
    message: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RoiRect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

enum WorkerCommand {
    Start(WindowId, PathBuf, f32, u64, u64, Vec<RoiRect>),
    UpdateConfig(f32, u64, u64, Vec<RoiRect>),
    RestoreAndSnapshot(WindowId),
    Stop,
}

struct PreviewImage {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    window_rect: Option<egui::Rect>,
}

// --- Windows API Helpers ---

struct WindowInfo {
    id: WindowId,
    title: String,
}



#[cfg(windows)]
fn find_deepest_child(parent: HWND, x: i32, y: i32) -> (HWND, i32, i32) {
    unsafe {
        let point = POINT { x, y };
        let child = ChildWindowFromPoint(parent, point);
        if child != 0 && child != parent {
            let mut pt = POINT { x, y };
            ClientToScreen(parent, &mut pt);
            ScreenToClient(child, &mut pt);
            return find_deepest_child(child, pt.x, pt.y);
        }
        (parent, x, y)
    }
}

#[cfg(windows)]
fn virtual_click(id: WindowId, local_x: u32, local_y: u32) -> String {
    #[cfg(windows)]
    unsafe {
        let hwnd = id as HWND;
        let (target_hwnd, target_x, target_y) = find_deepest_child(hwnd, local_x as i32, local_y as i32);
        let lparam = (target_y << 16 | (target_x & 0xFFFF)) as isize;
        SendNotifyMessageW(target_hwnd, WM_LBUTTONDOWN, 0, lparam);
        thread::sleep(Duration::from_millis(10));
        SendNotifyMessageW(target_hwnd, WM_LBUTTONUP, 0, lparam);
        format!("Click di HWND: {} ({}, {})", target_hwnd, target_x, target_y)
    }
    #[cfg(not(windows))]
    {
        use enigo::{Enigo, Settings, Mouse, Button, Coordinate, Direction};
        // Fallback to Enigo for other platforms
        let mut enigo = Enigo::new(&Settings::default()).unwrap();
        // Note: Generic click might need screen coordinates, but for now we follow the API
        enigo.move_mouse(local_x as i32, local_y as i32, Coordinate::Abs).unwrap(); 
        enigo.button(Button::Left, Direction::Click).unwrap();
        format!("Click generic at ({}, {})", local_x, local_y)
    }
}

fn capture_window(id: WindowId) -> (Option<image::DynamicImage>, String) {
    // Try cross-platform xcap first
    if let Ok(windows) = Window::all() {
        if let Some(win) = windows.into_iter().find(|w| w.id() as u64 == id) {
            if let Ok(img) = win.capture_image() {
                return (Some(image::DynamicImage::ImageRgba8(img)), String::new());
            }
        }
    }

    #[cfg(windows)]
    unsafe {
        let hwnd = id as HWND;
        let mut client_rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
        if GetClientRect(hwnd, &mut client_rect) == 0 { return (None, "Gagal GetClientRect".to_string()); }
        let (width, height) = (client_rect.right - client_rect.left, client_rect.bottom - client_rect.top);
        if width <= 0 || height <= 0 { return (None, "Dimensi 0".to_string()); }

        let mut pt = POINT { x: 0, y: 0 };
        ClientToScreen(hwnd, &mut pt);

        let hdc_desktop = GetDC(0);
        let hdc_mem = CreateCompatibleDC(hdc_desktop);
        let hbitmap = CreateCompatibleBitmap(hdc_desktop, width, height);
        let old_obj = SelectObject(hdc_mem, hbitmap);

        let mut captured = BitBlt(hdc_mem, 0, 0, width, height, hdc_desktop, pt.x, pt.y, SRCCOPY | 0x40000000) != 0;

        let mut bitmap_info: BITMAPINFOHEADER = std::mem::zeroed();
        bitmap_info.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bitmap_info.biWidth = width;
        bitmap_info.biHeight = -height;
        bitmap_info.biPlanes = 1;
        bitmap_info.biBitCount = 32;
        bitmap_info.biCompression = BI_RGB as u32;
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        
        if captured {
            GetDIBits(hdc_mem, hbitmap, 0, height as u32, pixels.as_mut_ptr() as *mut _, &mut bitmap_info as *mut _ as *mut _, DIB_RGB_COLORS);
        }

        if !captured || pixels.iter().all(|&p| p == 0) {
            if PrintWindow(hwnd, hdc_mem, 2) != 0 {
                GetDIBits(hdc_mem, hbitmap, 0, height as u32, pixels.as_mut_ptr() as *mut _, &mut bitmap_info as *mut _ as *mut _, DIB_RGB_COLORS);
                captured = pixels.iter().any(|&p| p != 0);
            }
        }

        SelectObject(hdc_mem, old_obj); 
        let _ = DeleteObject(hbitmap); 
        DeleteDC(hdc_mem); 
        ReleaseDC(std::mem::zeroed(), hdc_desktop);

        if !captured || pixels.iter().all(|&p| p == 0) {
            return (None, "Capture Failed (Black Screen)".to_string());
        }

        for chunk in pixels.chunks_exact_mut(4) {
            chunk.swap(0, 2); // BGR -> RGB
            chunk[3] = 255;   // Opaque
        }

        let img = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(width as u32, height as u32, pixels).map(image::DynamicImage::ImageRgba8);
        (img, String::new())
    }
    #[cfg(not(windows))]
    (None, "Windows capture not available on this platform.".to_string())
}

// --- Matching Logic ---

fn find_template_in_image(source: &image::DynamicImage, template: &image::DynamicImage, threshold: f32) -> Option<(u32, u32, f32)> {
    let (sw, sh) = source.dimensions();
    let (tw, th) = template.dimensions();
    if sw < tw || sh < th { return None; }
    let source_rgba = source.to_rgba8();
    let template_rgba = template.to_rgba8();
    let mut best_x = 0; let mut best_y = 0; let mut max_corr = 0.0;
    for y in (0..sh - th).step_by(3) {
        for x in (0..sw - tw).step_by(3) {
            let mut sum_sq_diff = 0u64;
            let s_pixel = source_rgba.get_pixel(x, y);
            let t_pixel = template_rgba.get_pixel(0, 0);
            if (s_pixel[0] as i32 - t_pixel[0] as i32).abs() + (s_pixel[1] as i32 - t_pixel[1] as i32).abs() > 150 { continue; }
            for i in 0..64 {
                let rx = (i % 8) * (tw / 8); let ry = (i / 8) * (th / 8);
                let sp = source_rgba.get_pixel(x + rx, y + ry);
                let tp = template_rgba.get_pixel(rx, ry);
                sum_sq_diff += ((sp[0] as i32 - tp[0] as i32).pow(2) + (sp[1] as i32 - tp[1] as i32).pow(2) + (sp[2] as i32 - tp[2] as i32).pow(2)) as u64;
            }
            let score = 1.0 - (sum_sq_diff as f32 / (64.0 * 255.0 * 255.0 * 3.0));
            if score > max_corr { max_corr = score; best_x = x; best_y = y; }
        }
    }
    if max_corr >= threshold { Some((best_x, best_y, max_corr)) } else { None }
}

// --- Worker ---

fn worker_loop(rx: Receiver<WorkerCommand>, log_tx: Sender<LogEntry>, img_tx: Sender<PreviewImage>, conf_tx: Sender<f32>) {
    let mut target_id: Option<WindowId> = None;
    let mut is_running = false;
    let mut current_threshold = 0.85f32;
    let mut current_delay = 1000u64;
    let mut current_jitter = 0u64;
    let mut current_rois: Vec<RoiRect> = Vec::new();
    let mut targets_path;
    let mut templates: Vec<PathBuf> = Vec::new();
    loop {
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                WorkerCommand::Start(id, path, thresh, delay, jitter, rois) => {
                    target_id = Some(id); targets_path = path; current_threshold = thresh; current_delay = delay; current_jitter = jitter; current_rois = rois;
                    is_running = true; templates.clear();
                    for entry in WalkDir::new(&targets_path).into_iter().filter_map(|e| e.ok()) {
                        if entry.path().extension().and_then(|s| s.to_str()) == Some("png") { templates.push(entry.path().to_path_buf()); }
                    }
                    let _ = log_tx.send(LogEntry { level: LogLevel::Info, message: format!("Start. {} targs.", templates.len()) });
                }
                WorkerCommand::UpdateConfig(thresh, delay, jitter, rois) => {
                    current_threshold = thresh; current_delay = delay; current_jitter = jitter; current_rois = rois;
                }
                WorkerCommand::RestoreAndSnapshot(id) => {
                    #[cfg(windows)]
                    unsafe { ShowWindowAsync(id as HWND, SW_RESTORE as i32); }
                    thread::sleep(Duration::from_millis(400)); // Wait for restore animation
                    let (img, _) = capture_window(id);
                    if let Some(i) = img {
                        let rgba = i.to_rgba8(); let (w, h) = rgba.dimensions();
                        let mut wr = None;
                        
                        // Try xcap for generic rect first
                        if let Ok(windows) = Window::all() {
                            if let Some(win) = windows.into_iter().find(|w| w.id() as u64 == id) {
                                wr = Some(egui::Rect::from_min_max(
                                    egui::pos2(win.x() as f32, win.y() as f32),
                                    egui::pos2((win.x() + win.width() as i32) as f32, (win.y() + win.height() as i32) as f32)
                                ));
                            }
                        }

                        #[cfg(windows)]
                        if wr.is_none() {
                            unsafe {
                                let mut client_rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                                let hwnd = id as HWND;
                                if GetClientRect(hwnd, &mut client_rect) != 0 {
                                    let mut pt = POINT { x: 0, y: 0 };
                                    ClientToScreen(hwnd, &mut pt);
                                    wr = Some(egui::Rect::from_min_max(
                                        egui::pos2(pt.x as f32, pt.y as f32),
                                        egui::pos2((pt.x + client_rect.right) as f32, (pt.y + client_rect.bottom) as f32)
                                    ));
                                }
                            }
                        }
                        let _ = img_tx.send(PreviewImage { pixels: rgba.into_raw(), width: w, height: h, window_rect: wr });
                    }
                }
                WorkerCommand::Stop => { is_running = false; let _ = log_tx.send(LogEntry { level: LogLevel::Info, message: "Stop.".to_string() }); }
            }
        }
        if is_running {
            if let Some(id) = target_id {
                let (img_opt, cap_msg) = capture_window(id);
                if let Some(full_img) = img_opt {
                    let (fw, fh) = full_img.dimensions();
                    let mut max_scan_conf = 0.0f32;
                    let mut found_match = false;

                    // If no ROIs, scan full screen once. If ROIs exist, scan each area.
                    let scan_areas = if current_rois.is_empty() {
                        vec![RoiRect { x: 0, y: 0, width: fw, height: fh }]
                    } else {
                        current_rois.clone()
                    };

                    for roi in scan_areas {
                        if found_match { break; }
                        
                        let rw = roi.width.min(fw.saturating_sub(roi.x)); 
                        let rh = roi.height.min(fh.saturating_sub(roi.y));
                        if rw == 0 || rh == 0 { continue; }
                        
                        let captured_img = full_img.crop_imm(roi.x, roi.y, rw, rh);
                        let (cw, ch) = (rw, rh);
                        let (ox, oy) = (roi.x, roi.y);

                        for template_path in &templates {
                            if let Ok(template_img) = image::open(template_path) {
                                let (tw, th) = template_img.dimensions();
                                if let Some((lx, ly, conf)) = find_template_in_image(&captured_img, &template_img, 0.40) {
                                    if conf > max_scan_conf { max_scan_conf = conf; }

                                    if conf >= current_threshold && !found_match {
                                        let mx = lx + tw / 2; let my = ly + th / 2;
                                        if mx < cw && my < ch {
                                            let pixel = captured_img.get_pixel(mx, my);
                                            // Blue check (keep existing logic)
                                            if pixel[2] > pixel[0] && pixel[2] > pixel[1] && pixel[2] > 100 {
                                                 let msg = virtual_click(id, mx + ox, my + oy);
                                                 let _ = log_tx.send(LogEntry { level: LogLevel::Success, message: format!("MATCH ({:.2}): {}", conf, msg) });
                                                found_match = true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    let _ = conf_tx.send(max_scan_conf);
                    if found_match {
                        let jitter = if current_jitter > 0 { rand::thread_rng().gen_range(0..=current_jitter) } else { 0 };
                        thread::sleep(Duration::from_millis(current_delay + jitter));
                    }
                } else if !cap_msg.is_empty() { let _ = log_tx.send(LogEntry { level: LogLevel::Warning, message: cap_msg }); }
            }
        }
        thread::sleep(Duration::from_millis(500));
    }
}

fn spawn_hotkey_listener(gui_tx: Sender<String>) {
    thread::spawn(move || {
        let _ = listen(move |event| {
            if let EventType::KeyPress(key) = event.event_type {
                match key { Key::F5 => { let _ = gui_tx.send("START".to_string()); } Key::F6 => { let _ = gui_tx.send("STOP".to_string()); } _ => {} }
            }
        });
    });
}

// --- Egui App ---

struct AutoclickerApp {
    is_running: bool,
    windows: Vec<WindowInfo>,
    selected_window_id: WindowId,
    target_image_folder: PathBuf,
    threshold: f32,
    delay_ms: u64,
    jitter_ms: u64,
    use_roi: bool,
    rois: Vec<RoiRect>,
    preview: Option<egui::TextureHandle>,
    raw_preview_dims: (u32, u32),
    logs: Vec<LogEntry>,
    total_clicks: u64,
    start_time: Option<Instant>,
    template_files: Vec<String>,
    tx: Sender<WorkerCommand>,
    log_rx: Receiver<LogEntry>,
    img_rx: Receiver<PreviewImage>,
    conf_rx: Receiver<f32>,
    current_conf: f32,
    hotkey_rx: Receiver<String>,
    waiting_for_roi: bool,
    show_roi_on_screen: bool,
    roi_drag_start: Option<egui::Pos2>,
    show_global_roi_overlay: bool,
    global_roi_win_rect: egui::Rect,
    #[cfg(windows)]
    last_physical_rect: (i32, i32, i32, i32), // (x, y, w, h)
    #[cfg(windows)]
    viewport_modified_id: Option<egui::ViewportId>,
    #[cfg(windows)]
    cached_overlay_hwnd: HWND,
}

impl AutoclickerApp {
    fn new(tx: Sender<WorkerCommand>, log_rx: Receiver<LogEntry>, img_rx: Receiver<PreviewImage>, hotkey_rx: Receiver<String>, conf_rx: Receiver<f32>) -> Self {
        let mut app = Self {
            is_running: false, windows: Vec::new(), selected_window_id: 0,
            target_image_folder: PathBuf::from("targets"), threshold: 0.85, delay_ms: 1000, jitter_ms: 0,
            use_roi: false, rois: Vec::new(),
            preview: None, raw_preview_dims: (0, 0), logs: Vec::new(), total_clicks: 0,
            start_time: None, template_files: Vec::new(),
            tx, log_rx, img_rx, hotkey_rx,
            conf_rx, current_conf: 0.0,
            waiting_for_roi: false, 
            show_roi_on_screen: false,
            roi_drag_start: None,
            show_global_roi_overlay: false,
            global_roi_win_rect: egui::Rect::NOTHING,
            #[cfg(windows)]
            last_physical_rect: (0, 0, 0, 0),
            #[cfg(windows)]
            viewport_modified_id: None,
            #[cfg(windows)]
            cached_overlay_hwnd: 0,
        };
        app.refresh_windows(); app.refresh_templates(); app
    }
    fn refresh_windows(&mut self) {
        let mut list = Vec::new();
        if let Ok(windows) = Window::all() {
            for win in windows {
                let title = win.title().to_string();
                let low_title = title.to_lowercase();
                if !low_title.is_empty() && !low_title.contains("program manager") && !low_title.contains("settings") {
                   list.push(WindowInfo {
                       id: win.id() as u64,
                       title: title,
                   });
                }
            }
        }
        self.windows = list;
        // Verify current selection still exists, else pick first visible
        if self.selected_window_id != 0 {
            if !self.windows.iter().any(|w| w.id == self.selected_window_id) {
                if let Some(first) = self.windows.first() { self.selected_window_id = first.id; }
                else { self.selected_window_id = 0; }
            }
        } else if let Some(first) = self.windows.first() {
            self.selected_window_id = first.id;
        }
    }
    fn refresh_templates(&mut self) { self.template_files.clear(); if let Ok(rd) = std::fs::read_dir(&self.target_image_folder) { for e in rd.filter_map(|e| e.ok()) { if e.path().extension().and_then(|s| s.to_str()) == Some("png") { self.template_files.push(e.file_name().to_string_lossy().to_string()); } } } }
    fn current_roi_opt(&self) -> Vec<RoiRect> { if self.use_roi { self.rois.clone() } else { Vec::new() } }
}

impl eframe::App for AutoclickerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Handle Inbound
        while let Ok(log) = self.log_rx.try_recv() { if log.message.contains("Click") { self.total_clicks += 1; } self.logs.push(log); if self.logs.len() > 50 { self.logs.remove(0); } }
        while let Ok(img) = self.img_rx.try_recv() {
            let color_image = egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.pixels);
            self.preview = Some(ctx.load_texture("preview", color_image, Default::default()));
            self.raw_preview_dims = (img.width, img.height);
            if let Some(wr) = img.window_rect {
                // Convert physical rect (from win32) to logical rect (for egui)
                let ppp = ctx.pixels_per_point();
                self.global_roi_win_rect = egui::Rect::from_min_max(
                    egui::pos2(wr.min.x / ppp, wr.min.y / ppp),
                    egui::pos2(wr.max.x / ppp, wr.max.y / ppp)
                );
                self.show_global_roi_overlay = true;
                if self.waiting_for_roi { self.waiting_for_roi = false; }
            }
        }
        while let Ok(conf) = self.conf_rx.try_recv() { self.current_conf = conf; }
        while let Ok(hk) = self.hotkey_rx.try_recv() {
            if hk == "START" && !self.is_running {
                if self.selected_window_id != 0 {
                    self.is_running = true; self.start_time = Some(Instant::now());
                    let _ = self.tx.send(WorkerCommand::Start(self.selected_window_id, self.target_image_folder.clone(), self.threshold, self.delay_ms, self.jitter_ms, self.current_roi_opt()));
                }
            } else if hk == "STOP" && self.is_running {
                self.is_running = false; let _ = self.tx.send(WorkerCommand::Stop);
            }
        }

        // --- STYLING (THEME & TRANSPARENCY) ---
        // Global panel_fill must be transparent for secondary viewports to be see-through.
        // We handle opacity explicitly in each window's frame.
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = egui::Color32::from_rgba_premultiplied(0, 0, 0, 0);
        visuals.window_fill = egui::Color32::from_rgb(20, 20, 20); // Opaque background for popups/dropdowns
        visuals.window_shadow = egui::epaint::Shadow::NONE;
        visuals.widgets.noninteractive.bg_fill = egui::Color32::from_rgb(5, 5, 5);
        visuals.widgets.inactive.bg_fill = egui::Color32::from_rgb(10, 10, 10);
        visuals.selection.bg_fill = egui::Color32::from_rgb(0, 255, 0);
        visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(0, 80, 0);
        visuals.widgets.active.bg_fill = egui::Color32::from_rgb(0, 120, 0);
        ctx.set_visuals(visuals);

        // REMOVED FULL-SCREEN OVERLAY TO PREVENT LOCKOUT

        egui::CentralPanel::default().frame(egui::Frame::none().fill(egui::Color32::from_rgb(10, 10, 10)).inner_margin(12.0)).show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading(egui::RichText::new("AUTOCLICKER").color(egui::Color32::from_rgb(0, 255, 0)));
                ui.label("Control Panel"); // Simple placeholder to balance the space if needed
            });

            ui.add_space(4.0);

                ui.vertical(|ui| {
                ui.set_width(360.0);
                egui::Grid::new("g").num_columns(2).spacing([10.0, 8.0]).show(ui, |ui| {
                    ui.label("Target Window:");
                    ui.horizontal(|ui| {
                        if ui.button("Refresh").clicked() { self.refresh_windows(); }
                        let text = self.windows.iter().find(|w| w.id == self.selected_window_id).map(|w| w.title.clone()).unwrap_or_else(|| "...".to_string());
                        let display_text = if text.chars().count() > 25 { text.chars().take(22).collect::<String>() + "..." } else { text };
                        egui::ComboBox::from_id_salt("w").width(180.0).selected_text(display_text).show_ui(ui, |ui| { 
                            for w in &self.windows { ui.selectable_value(&mut self.selected_window_id, w.id, &w.title); } 
                        });
                    }); ui.end_row();

                    ui.label("Target Image Folder:");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.target_image_folder.to_string_lossy().to_string()).desired_width(180.0));
                        if ui.button("Browse").clicked() { if let Some(p) = rfd::FileDialog::new().pick_folder() { self.target_image_folder = p; self.refresh_templates(); } }
                    }); ui.end_row();
                    
                    ui.label("Match Threshold:");
                    ui.horizontal(|ui| {
                        if ui.add(egui::Slider::new(&mut self.threshold, 0.1..=1.0).step_by(0.01)).changed() {
                            let _ = self.tx.send(WorkerCommand::UpdateConfig(self.threshold, self.delay_ms, self.jitter_ms, self.current_roi_opt()));
                        }
                    });
                    ui.end_row();
                    
                    ui.label("Current Match:");
                    ui.horizontal(|ui| {
                        let color = if self.current_conf >= self.threshold { egui::Color32::from_rgb(0, 255, 0) } else { egui::Color32::from_rgb(200, 50, 50) };
                        ui.add_sized([180.0, 16.0], egui::ProgressBar::new(self.current_conf).text(format!("{:.1}%", self.current_conf * 100.0)).fill(color));
                    });
                    ui.end_row();

                    ui.label("Speed (ms):");
                    if ui.add(egui::Slider::new(&mut self.delay_ms, 100..=5000)).changed() { let _ = self.tx.send(WorkerCommand::UpdateConfig(self.threshold, self.delay_ms, self.jitter_ms, self.current_roi_opt())); }
                    ui.end_row();
                    
                    ui.label("Random Jitter (ms):");
                    if ui.add(egui::Slider::new(&mut self.jitter_ms, 0..=1000)).changed() { let _ = self.tx.send(WorkerCommand::UpdateConfig(self.threshold, self.delay_ms, self.jitter_ms, self.current_roi_opt())); }
                    ui.end_row();
                });

                ui.add_space(8.0);
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        ui.label("Scan Areas:");
                        if ui.checkbox(&mut self.use_roi, "Active").changed() { let _ = self.tx.send(WorkerCommand::UpdateConfig(self.threshold, self.delay_ms, self.jitter_ms, self.current_roi_opt())); }
                        ui.separator();
                        let btn_text = if self.show_roi_on_screen { "Hide Area" } else { "Show Area" };
                        if ui.selectable_label(self.show_roi_on_screen, btn_text).clicked() {
                            self.show_roi_on_screen = !self.show_roi_on_screen;
                            if self.show_roi_on_screen {
                                if self.selected_window_id != 0 {
                                    let _ = self.tx.send(WorkerCommand::RestoreAndSnapshot(self.selected_window_id));
                                }
                            } else {
                                self.show_global_roi_overlay = false;
                            }
                        }
                        if ui.button("Clear All").clicked() { self.rois.clear(); let _ = self.tx.send(WorkerCommand::UpdateConfig(self.threshold, self.delay_ms, self.jitter_ms, self.current_roi_opt())); }
                    });
                    if ui.add(egui::Button::new("SELECT ON SCREEN (Add Area)")).clicked() { 
                        if self.selected_window_id != 0 {
                            let _ = self.tx.send(WorkerCommand::RestoreAndSnapshot(self.selected_window_id)); 
                            self.waiting_for_roi = true;
                            self.show_roi_on_screen = false;
                        }
                    }
                    ui.small(format!("Active Areas: {}", self.rois.len()));
                });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if !self.is_running { if ui.add_sized([100.0, 32.0], egui::Button::new("START").fill(egui::Color32::from_rgb(0, 150, 0))).clicked() { if self.selected_window_id != 0 { self.is_running = true; self.start_time = Some(Instant::now()); let _ = self.tx.send(WorkerCommand::Start(self.selected_window_id, self.target_image_folder.clone(), self.threshold, self.delay_ms, self.jitter_ms, self.current_roi_opt())); } } }
                    else { if ui.add_sized([100.0, 32.0], egui::Button::new("STOP").fill(egui::Color32::from_rgb(150, 0, 0))).clicked() { self.is_running = false; let _ = self.tx.send(WorkerCommand::Stop); } }
                    ui.separator();
                    ui.vertical(|ui| { ui.label(format!("Clicks: {}", self.total_clicks)); let up = self.start_time.map(|t| t.elapsed().as_secs()).unwrap_or(0); ui.label(format!("Uptime: {}s", up)); });
                });
            });

            ui.add_space(5.0); ui.separator();
            ui.label("Templates:");
            egui::ScrollArea::vertical().id_salt("ts").max_height(80.0).show(ui, |ui| { for f in &self.template_files { ui.small(format!("• {}", f)); } });
            ui.add_space(5.0);
            ui.label("Logs:");
            egui::ScrollArea::vertical().id_salt("ls").max_height(80.0).stick_to_bottom(true).show(ui, |ui| { for log in &self.logs { let c = match log.level { LogLevel::Success => egui::Color32::LIGHT_GREEN, LogLevel::Warning => egui::Color32::KHAKI, _ => egui::Color32::GRAY }; ui.colored_label(c, &log.message); } });
        });

        #[cfg(windows)]
        if self.show_global_roi_overlay {
            ctx.request_repaint(); // Keep repainting to track window movements smoothly!
            if let Some(win) = self.windows.iter().find(|w| w.id == self.selected_window_id) {
                let mut client_rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
                unsafe {
                    if GetClientRect(win.id as HWND, &mut client_rect) != 0 {
                        let mut pt = POINT { x: 0, y: 0 };
                        ClientToScreen(win.id as HWND, &mut pt);
                        
                        let current_physical = (pt.x, pt.y, client_rect.right, client_rect.bottom);
                        if self.last_physical_rect != current_physical {
                            self.last_physical_rect = current_physical;
                            
                            if self.cached_overlay_hwnd == 0 {
                                let title: Vec<u16> = "AUTOCLICKER_ROI_OVERLAY\0".encode_utf16().collect();
                                self.cached_overlay_hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
                                
                                if self.cached_overlay_hwnd != 0 {
                                    let sw = GetSystemMetrics(0); 
                                    let sh = GetSystemMetrics(1); 
                                    SetWindowPos(self.cached_overlay_hwnd, HWND_TOPMOST, 0, 0, sw, sh, 
                                        SWP_NOACTIVATE | SWP_ASYNCWINDOWPOS);
                                    SetLayeredWindowAttributes(self.cached_overlay_hwnd, 0, 255, LWA_ALPHA);
                                }
                            }

                            // Keep the logical rect updated for ROI drawing math (relative to screen)
                            let ppp = ctx.pixels_per_point();
                            self.global_roi_win_rect = egui::Rect::from_min_max(
                                egui::pos2(pt.x as f32 / ppp, pt.y as f32 / ppp),
                                egui::pos2((pt.x + client_rect.right) as f32 / ppp, (pt.y + client_rect.bottom) as f32 / ppp),
                            );
                        }
                    }
                }
            }
        } else {
            self.viewport_modified_id = None;
            self.cached_overlay_hwnd = 0;
            self.last_physical_rect = (0, 0, 0, 0); // Reset tracking so it re-initializes
        }

        // --- GLOBAL ROI OVERLAY ---
        if self.show_global_roi_overlay {
            let is_selecting = !self.show_roi_on_screen;
            
            ctx.show_viewport_immediate(
                egui::ViewportId::from_hash_of("roi_overlay"),
                egui::ViewportBuilder::default()
                    .with_title("ANTIGRAVITY_ROI_OVERLAY")
                    .with_inner_size([2000.0, 2000.0]) // Start large, will be resized by SetWindowPos
                    .with_position([0.0, 0.0])
                    .with_decorations(false)
                    .with_transparent(true)
                    .with_always_on_top()
                    .with_taskbar(false) 
                    .with_mouse_passthrough(!is_selecting && self.show_roi_on_screen),
                |ctx, class| {
                    if class == egui::ViewportClass::Immediate {
                        #[cfg(windows)]
                        {
                            // Capture context pointer safely if possible, but simpler is to use a static or check style
                            // We use the viewport ID to ensure we only style once per session.
                            let title: Vec<u16> = "ANTIGRAVITY_ROI_OVERLAY\0".encode_utf16().collect();
                            unsafe {
                                let hwnd = FindWindowW(std::ptr::null(), title.as_ptr());
                                if hwnd != 0 {
                                    // Check if already styled to avoid flickering
                                    let style = GetWindowLongW(hwnd, GWL_STYLE);
                                    let borderless_style = WS_POPUP as i32 | WS_VISIBLE as i32;
                                    
                                    if style != borderless_style {
                                        SetWindowLongW(hwnd, GWL_STYLE, borderless_style);
                                        
                                        // Disable DWM window transition animations
                                        let disable_anim: i32 = 1;
                                        let _ = DwmSetWindowAttribute(hwnd, DWMWA_TRANSITIONS_FORCEDISABLED as u32, &disable_anim as *const _ as *const _, 4);

                                        // Set DWM margins to -1 to enable per-pixel alpha composition
                                        let margins = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
                                        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
                                        
                                        // BlurBehind forces DWM composition on problematic drivers
                                        let bb = DWM_BLURBEHIND {
                                            dwFlags: DWM_BB_ENABLE,
                                            fEnable: 1,
                                            hRgnBlur: 0,
                                            fTransitionOnMaximized: 0,
                                        };
                                        let _ = DwmEnableBlurBehindWindow(hwnd, &bb);

                                         // Ensure WS_EX_LAYERED and WS_EX_TOOLWINDOW
                                        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
                                        let mut target_ex = ex_style | WS_EX_LAYERED as i32 | WS_EX_TOOLWINDOW as i32;
                                        
                                        // Selective Click-Through: Add WS_EX_TRANSPARENT only when NOT selecting
                                        if !is_selecting {
                                            target_ex |= WS_EX_TRANSPARENT as i32;
                                        } else {
                                            target_ex &= !(WS_EX_TRANSPARENT as i32);
                                        }

                                        if ex_style != target_ex {
                                            SetWindowLongW(hwnd, GWL_EXSTYLE, target_ex);
                                        }
                                    } else {
                                        // If already borderless, we still need to toggle WS_EX_TRANSPARENT dynamically
                                        let ex_style = GetWindowLongW(hwnd, GWL_EXSTYLE);
                                        let is_currently_transparent = (ex_style & WS_EX_TRANSPARENT as i32) != 0;
                                        if is_currently_transparent == is_selecting { // We want transparent when NOT selecting
                                            let mut target_ex = ex_style | WS_EX_LAYERED as i32 | WS_EX_TOOLWINDOW as i32;
                                            if !is_selecting { target_ex |= WS_EX_TRANSPARENT as i32; }
                                            else { target_ex &= !(WS_EX_TRANSPARENT as i32); }
                                            SetWindowLongW(hwnd, GWL_EXSTYLE, target_ex);
                                        }
                                    }
                                }
                            }
                        }

                        // Use a very low alpha (1/255) for the background.
                        // On a DWM-transparent window, this is effectively invisible but still interceptions mouse events!
                        // During monitoring, we use 0 alpha to allow passthrough.
                        // Use purely transparent background. 
                        // DWM composition will handle the alpha blending with the desktop.
                        let bg_color = egui::Color32::from_rgba_unmultiplied(0, 0, 0, 0);

                        // Ensure egui itself doesn't use a background color for the viewport's clear color.
                        ctx.set_visuals(egui::Visuals {
                            panel_fill: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 0),
                            window_fill: egui::Color32::from_rgba_unmultiplied(0, 0, 0, 0),
                            ..egui::Visuals::default()
                        });

                        egui::CentralPanel::default()
                            .frame(egui::Frame::none().fill(bg_color)) 
                            .show(ctx, |ui| {
                                let rect = ui.max_rect();
                                let response = ui.interact(rect, ui.id(), egui::Sense::drag());
                                
                                // FORCE transparency clear at the painter level
                                ui.painter().rect_filled(rect, 0.0, bg_color);

                                if is_selecting {
                                    // Live Selection Mode: Subtle crosshair and help text
                                    ui.painter().text(
                                        rect.center() + egui::vec2(0.0, -50.0),
                                        egui::Align2::CENTER_CENTER,
                                        "LIVE SELECT: DRAG ROI (ESC to cancel)",
                                        egui::FontId::proportional(22.0),
                                        egui::Color32::GREEN
                                    );

                                    if response.drag_started() {
                                        self.roi_drag_start = response.interact_pointer_pos();
                                    }

                                    if let (Some(start), Some(curr)) = (self.roi_drag_start, ctx.input(|i| i.pointer.latest_pos())) {
                                        let min = egui::pos2(start.x.min(curr.x), start.y.min(curr.y));
                                        let max = egui::pos2(start.x.max(curr.x), start.y.max(curr.y));
                                        let selection_rect = egui::Rect::from_min_max(min, max);
                                        
                                        ui.painter().rect_stroke(selection_rect, 2.0, egui::Stroke::new(2.5, egui::Color32::GREEN));
                                        ui.painter().rect_filled(selection_rect, 0.0, egui::Color32::from_rgba_unmultiplied(0, 255, 0, 30));

                                        if response.drag_stopped() {
                                            let ppp = ctx.pixels_per_point();
                                            // Convert logical screen coords to window-relative physical pixels
                                            let new_roi = RoiRect {
                                                x: ((selection_rect.min.x - self.global_roi_win_rect.min.x).max(0.0) * ppp) as u32,
                                                y: ((selection_rect.min.y - self.global_roi_win_rect.min.y).max(0.0) * ppp) as u32,
                                                width: (selection_rect.width() * ppp) as u32,
                                                height: (selection_rect.height() * ppp) as u32,
                                            };
                                            self.rois.push(new_roi);
                                            self.use_roi = true;
                                            self.show_global_roi_overlay = false;
                                            self.roi_drag_start = None;
                                            let _ = self.tx.send(WorkerCommand::UpdateConfig(self.threshold, self.delay_ms, self.jitter_ms, self.current_roi_opt()));
                                        }
                                    }

                                    // Interactive crosshair (very subtle)
                                    if let Some(mouse_pos) = ctx.input(|i| i.pointer.latest_pos()) {
                                        let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgba_unmultiplied(0, 255, 0, 100));
                                        ui.painter().line_segment([egui::pos2(mouse_pos.x, 0.0), egui::pos2(mouse_pos.x, rect.height())], stroke);
                                        ui.painter().line_segment([egui::pos2(0.0, mouse_pos.y), egui::pos2(rect.width(), mouse_pos.y)], stroke);
                                    }
                                } else if self.show_roi_on_screen {
                                    // Live Viewer Mode: Just the ROI box outline
                                    let ppp = ctx.pixels_per_point();
                                    for roi in &self.rois {
                                        // Convert window-relative physical pixels to logical screen coords
                                        let sel_rect = egui::Rect::from_min_max(
                                            self.global_roi_win_rect.min + egui::vec2(roi.x as f32 / ppp, roi.y as f32 / ppp),
                                            self.global_roi_win_rect.min + egui::vec2((roi.x + roi.width) as f32 / ppp, (roi.y + roi.height) as f32 / ppp)
                                        );
                                        ui.painter().rect_stroke(sel_rect, 2.0, egui::Stroke::new(3.0, egui::Color32::from_rgba_unmultiplied(0, 255, 0, 200)));
                                        ui.painter().rect_filled(sel_rect, 0.0, egui::Color32::from_rgba_unmultiplied(0, 255, 0, 15));
                                    }
                                }

                                if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                                    self.show_global_roi_overlay = false;
                                    self.show_roi_on_screen = false;
                                    self.waiting_for_roi = false;
                                }
                            });
                    }
                },
            );
        }

        ctx.request_repaint_after(Duration::from_millis(100));
    }
}

fn main() -> eframe::Result {
    let (tx, rx) = mpsc::channel();
    let (log_tx, log_rx) = mpsc::channel();
    let (img_tx, img_rx) = mpsc::channel();
    let (hk_tx, hk_rx) = mpsc::channel();
    let (conf_tx, conf_rx) = mpsc::channel();

    thread::spawn(move || worker_loop(rx, log_tx, img_tx, conf_tx));
    spawn_hotkey_listener(hk_tx);

    let options = eframe::NativeOptions { 
        renderer: eframe::Renderer::Glow, 
        multisampling: 0,
        depth_buffer: 0,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([380.0, 480.0])
            .with_title("Autoclicker")
            .with_transparent(true)
            .with_resizable(false),
        ..Default::default() 
    };
    eframe::run_native("autoclicker", options, Box::new(|_| Ok(Box::new(AutoclickerApp::new(tx, log_rx, img_rx, hk_rx, conf_rx)))))
}
