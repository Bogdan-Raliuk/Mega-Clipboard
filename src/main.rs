use arboard::Clipboard;
use chrono::Local;
use directories::UserDirs;
use image::{ImageBuffer, Rgba};
use iced::widget::{button, column, container, scrollable, text, space, image as iced_image};
use iced::{time, Color, Element, Length, Subscription, Task, window};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tray_icon::{
    menu::{Menu, MenuItem},
    TrayIconBuilder, TrayIcon, MouseButton, MouseButtonState,
};

// TrayIcon не реализует Debug, поэтому убираем derive
struct AppState {
    last_text: String,
    last_image_hash: Vec<u8>,
    history: Vec<HistoryItem>,
    _tray: Arc<TrayIcon>, 
}

#[derive(Debug, Clone)]
struct HistoryItem {
    time: String,
    content: String,
    is_image: bool,
    image_handle: Option<iced_image::Handle>,
    expanded: bool,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    ClipboardChecked(Result<ClipboardContent, String>),
    CopyToClipboard(String, bool),
    ToggleExpand(usize),
    CloseRequested,
    TrayEvent(tray_icon::TrayIconEvent),
}

#[derive(Debug, Clone)]
enum ClipboardContent {
    Text(String),
    Image(Vec<u8>, u32, u32),
    Empty,
}

fn get_app_dir() -> PathBuf {
    UserDirs::new()
        .and_then(|dirs| Some(dirs.document_dir()?.join("MegaClipboard")))
        .unwrap_or_else(|| PathBuf::from("MegaClipboard"))
}

fn load_history() -> Vec<HistoryItem> {
    let app_dir = get_app_dir();
    let log_path = app_dir.join("history.log");
    let mut items = Vec::new();
    if let Ok(file) = fs::File::open(&log_path) {
        let reader = BufReader::new(file);
        for line in reader.lines().flatten() {
            if let (Some(t_start), Some(t_end)) = (line.find('['), line.find(']')) {
                let time = line[t_start+1..t_end].to_string();
                let rest = &line[t_end+1..];
                if let Some(sep) = rest.find(": ") {
                    let kind_part = &rest[..sep].trim();
                    let content = &rest[sep+2..];
                    let is_image = kind_part.contains("IMAGE");
                    let mut handle = None;
                    if is_image {
                        let img_path = app_dir.join("captures").join(content);
                        if img_path.exists() { handle = Some(iced_image::Handle::from_path(img_path)); }
                    }
                    items.push(HistoryItem { time, content: content.to_string(), is_image, image_handle: handle, expanded: false });
                }
            }
        }
    }
    items
}

fn update(state: &mut AppState, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            if let Ok(event) = tray_icon::TrayIconEvent::receiver().try_recv() {
                return Task::done(Message::TrayEvent(event));
            }
            return Task::perform(async { check_clipboard_async() }, Message::ClipboardChecked);
        }
        Message::TrayEvent(event) => {
            if let tray_icon::TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
                // В iced 0.14 восстанавливаем окно через minimize(false)
                return window::get_latest().and_then(|id| window::minimize(id, false));
            }
        }
        Message::ToggleExpand(index) => {
            if let Some(item) = state.history.get_mut(index) { item.expanded = !item.expanded; }
        }
        Message::CloseRequested => {
            // Сворачиваем окно при закрытии
            return window::get_latest().and_then(|id| window::minimize(id, true));
        }
        Message::ClipboardChecked(Ok(content)) => {
            let app_dir = get_app_dir();
            match content {
                ClipboardContent::Text(new_text) => {
                    if !new_text.is_empty() && new_text != state.last_text {
                        state.last_text = new_text.clone();
                        state.history.push(HistoryItem { 
                            time: Local::now().format("%H:%M:%S").to_string(), 
                            content: new_text.clone(), is_image: false, image_handle: None, expanded: false 
                        });
                        log_to_file(&app_dir, "TEXT", &new_text);
                    }
                }
                ClipboardContent::Image(bytes, w, h) => {
                    if bytes != state.last_image_hash {
                        state.last_image_hash = bytes.clone();
                        let filename = format!("img_{}.png", Local::now().format("%Y%m%d_%H%M%S"));
                        let full_path = app_dir.join("captures").join(&filename);
                        let _ = fs::create_dir_all(app_dir.join("captures"));
                        if let Some(img) = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(w, h, bytes) {
                            if img.save(&full_path).is_ok() {
                                state.history.push(HistoryItem { 
                                    time: Local::now().format("%H:%M:%S").to_string(), 
                                    content: filename.clone(), is_image: true, 
                                    image_handle: Some(iced_image::Handle::from_path(&full_path)), expanded: false 
                                });
                                log_to_file(&app_dir, "IMAGE", &filename);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Message::CopyToClipboard(content, is_image) => {
            if is_image {
                let img_path = get_app_dir().join("captures").join(&content);
                if let Ok(img) = image::ImageReader::open(img_path).map_err(|e| e.to_string()).and_then(|r| r.decode().map_err(|e| e.to_string())) {
                    let rgba = img.to_rgba8();
                    let data = arboard::ImageData { 
                        width: rgba.width() as usize, height: rgba.height() as usize, 
                        bytes: std::borrow::Cow::from(rgba.into_raw()) 
                    };
                    let _ = Clipboard::new().and_then(|mut cb| cb.set_image(data));
                }
            } else {
                let _ = Clipboard::new().and_then(|mut cb| cb.set_text(content));
            }
        }
    }
    Task::none()
}

fn view(state: &AppState) -> Element<'_, Message> {
    let mut history_column = column![].spacing(10);
    for (i, item) in state.history.iter().enumerate().rev() {
        let mut card_column = column![text(&item.time).size(11).color(Color::from_rgb(0.5, 0.5, 0.5))].spacing(5);
        if item.is_image {
            if let Some(handle) = &item.image_handle {
                let (w, h) = if item.expanded { (400, 300) } else { (150, 100) };
                card_column = card_column.push(button(iced_image(handle.clone()).width(w).height(h)).on_press(Message::ToggleExpand(i)).style(|_, _| button::Style::default()));
            }
        } else {
            let displayed = if item.expanded { 
                item.content.clone() 
            } else { 
                let base: String = item.content.chars().take(100).collect();
                if item.content.len() > 100 { format!("{}...", base) } else { base }
            };
            card_column = card_column.push(button(text(displayed).size(14)).on_press(Message::ToggleExpand(i)).style(|_, _| button::Style::default()));
        }
        let card = container(column![card_column, button("Копировать").on_press(Message::CopyToClipboard(item.content.clone(), item.is_image)).padding(5)].spacing(10))
            .padding(12).width(Length::Fill).style(|_| container::Style { background: Some(iced::Background::Color(Color::from_rgb(0.18, 0.18, 0.18))), border: iced::Border { radius: 8.0.into(), ..Default::default() }, ..Default::default() });
        history_column = history_column.push(card);
    }
    container(column![text("Mega Clipboard Pro").size(26).color(Color::from_rgb(0.4, 0.6, 1.0)), space().height(15), scrollable(history_column).height(Length::Fill)])
        .padding(25).style(|_| container::Style { background: Some(iced::Background::Color(Color::from_rgb(0.08, 0.08, 0.08))), ..Default::default() }).into()
}

fn log_to_file(app_dir: &Path, entry_type: &str, content: &str) {
    let log_path = app_dir.join("history.log");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(log_path) {
        let _ = writeln!(file, "[{}] {}: {}", Local::now().format("%Y-%m-%d %H:%M:%S"), entry_type, content.replace('\n', " "));
    }
}

fn check_clipboard_async() -> Result<ClipboardContent, String> {
    let mut clipboard = Clipboard::new().map_err(|e| e.to_string())?;
    if let Ok(img) = clipboard.get_image() { return Ok(ClipboardContent::Image(img.bytes.into_owned(), img.width as u32, img.height as u32)); }
    if let Ok(txt) = clipboard.get_text() { return Ok(ClipboardContent::Text(txt)); }
    Ok(ClipboardContent::Empty)
}

fn subscription(_state: &AppState) -> Subscription<Message> {
    Subscription::batch(vec![
        time::every(std::time::Duration::from_millis(500)).map(|_| Message::Tick),
        iced::event::listen_with(|event, _, _| if let iced::Event::Window(window::Event::CloseRequested) = event { Some(Message::CloseRequested) } else { None }),
    ])
}

fn main() -> iced::Result {
    let tray_menu = Menu::new();
    let _ = tray_menu.append(&MenuItem::with_id("quit", "Выйти", true, None));
    
    let tray = Arc::new(TrayIconBuilder::new()
        .with_menu(Box::new(tray_menu))
        .with_tooltip("Mega Clipboard")
        .build()
        .unwrap());

    let tray_for_app = tray.clone();

    iced::application(move || AppState { 
        history: load_history(), 
        _tray: tray_for_app.clone(), // Клонируем Arc, чтобы удовлетворить Fn
        last_text: String::new(),
        last_image_hash: Vec::new(),
    }, update, view)
        .subscription(subscription)
        .title("Mega Clipboard")
        .run()
}
