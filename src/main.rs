use arboard::Clipboard;
use chrono::{Local, DateTime, Duration};
use directories::UserDirs;
use image::{ImageBuffer, Rgba};
use iced::widget::{button, column, container, scrollable, text, space, image as iced_image, text_input, slider, row, checkbox};
use iced::{time, Color, Element, Length, Subscription, Task, window, Alignment};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tray_icon::{
    menu::{Menu, MenuItem, MenuEvent},
    TrayIconBuilder, TrayIcon, MouseButton, TrayIconEvent, Icon,
};
use winreg::enums::*;
use winreg::RegKey;
use std::env;

const APP_NAME: &str = "MegaClipboard";

struct AppState {
    last_text: String,
    last_image_hash: Vec<u8>,
    history: Vec<HistoryItem>,
    _tray: Arc<TrayIcon>,
    search_query: String,
    days_filter: f32,
    auto_start: bool,
}

#[derive(Debug, Clone)]
struct HistoryItem {
    datetime: DateTime<Local>,
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
    DeleteItem(usize),
    CloseRequested,
    TrayEvent(TrayIconEvent),
    MenuEvent(MenuEvent),
    SetVisibility(bool),
    SearchChanged(String),
    FilterChanged(f32),
    ToggleAutoStart(bool),
}

#[derive(Debug, Clone)]
enum ClipboardContent {
    Text(String),
    Image(Vec<u8>, u32, u32),
    Empty,
}

fn load_icon() -> Icon {
    if let Ok(img) = image::open("src/icon.ico") {
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        Icon::from_rgba(rgba.into_raw(), w, h).unwrap_or_else(|_| Icon::from_rgba(vec![0; 32 * 32 * 4], 32, 32).unwrap())
    } else {
        Icon::from_rgba(vec![0; 32 * 32 * 4], 32, 32).unwrap()
    }
}

fn get_app_dir() -> PathBuf {
    UserDirs::new()
        .and_then(|dirs| Some(dirs.document_dir()?.join("MegaClipboard")))
        .unwrap_or_else(|| PathBuf::from("MegaClipboard"))
}

fn check_auto_start() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok(run_key) = hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run") {
        let val: Result<String, _> = run_key.get_value(APP_NAME);
        return val.is_ok();
    }
    false
}

fn set_auto_start(enable: bool) {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if let Ok((run_key, _)) = hkcu.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run") {
        if enable {
            if let Ok(exe_path) = env::current_exe() {
                let _ = run_key.set_value(APP_NAME, &exe_path.to_string_lossy().to_string());
            }
        } else {
            let _ = run_key.delete_value(APP_NAME);
        }
    }
}

fn rewrite_log(app_dir: &Path, history: &[HistoryItem]) {
    let log_path = app_dir.join("history.log");
    if let Ok(mut file) = std::fs::File::create(log_path) {
        for item in history {
            let kind = if item.is_image { "IMAGE" } else { "TEXT" };
            let _ = writeln!(file, "[{}] {}: {}", item.datetime.format("%Y-%m-%d %H:%M:%S"), kind, item.content.replace('\n', " "));
        }
    }
}

fn load_history() -> Vec<HistoryItem> {
    let app_dir = get_app_dir();
    let log_path = app_dir.join("history.log");
    let mut items = Vec::new();
    if let Ok(file) = fs::File::open(&log_path) {
        let reader = BufReader::new(file);
        for line in reader.lines().flatten() {
            if let (Some(t_start), Some(t_end)) = (line.find('['), line.find(']')) {
                let time_str = &line[t_start+1..t_end];
                let dt = DateTime::parse_from_str(&(time_str.to_owned() + " +0000"), "%Y-%m-%d %H:%M:%S %z")
                    .map(|d| d.with_timezone(&Local))
                    .unwrap_or_else(|_| Local::now());

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
                    items.push(HistoryItem { datetime: dt, content: content.to_string(), is_image, image_handle: handle, expanded: false });
                }
            }
        }
    }
    items
}

fn update(state: &mut AppState, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::Click { button: MouseButton::Left, .. } = event {
                    return Task::done(Message::SetVisibility(true));
                }
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id.as_ref() == "open" { return Task::done(Message::SetVisibility(true)); }
                if event.id.as_ref() == "quit" { std::process::exit(0); }
            }
            return Task::perform(async { check_clipboard_async() }, Message::ClipboardChecked);
        }
        Message::TrayEvent(_) | Message::MenuEvent(_) => {}
        Message::SearchChanged(q) => state.search_query = q,
        Message::FilterChanged(f) => state.days_filter = f,
        Message::ToggleAutoStart(enabled) => {
            state.auto_start = enabled;
            set_auto_start(enabled);
        }
        Message::SetVisibility(show) => {
            use windows_sys::Win32::UI::WindowsAndMessaging::{FindWindowA, ShowWindow, SW_HIDE, SW_SHOW};
            unsafe {
                let title = b"Mega Clipboard\0";
                let hwnd = FindWindowA(std::ptr::null(), title.as_ptr());
                if !hwnd.is_null() { ShowWindow(hwnd, if show { SW_SHOW } else { SW_HIDE }); }
            }
        }
        Message::CloseRequested => return Task::done(Message::SetVisibility(false)),
        Message::ToggleExpand(idx) => if let Some(item) = state.history.get_mut(idx) { item.expanded = !item.expanded; },
        Message::DeleteItem(idx) => {
            let app_dir = get_app_dir();
            if let Some(item) = state.history.get(idx) {
                if item.is_image { let _ = fs::remove_file(app_dir.join("captures").join(&item.content)); }
            }
            state.history.remove(idx);
            rewrite_log(&app_dir, &state.history);
        }
        Message::ClipboardChecked(Ok(content)) => {
            let app_dir = get_app_dir();
            match content {
                ClipboardContent::Text(new_text) => {
                    if !new_text.is_empty() && new_text != state.last_text {
                        state.last_text = new_text.clone();
                        state.history.push(HistoryItem { datetime: Local::now(), content: new_text, is_image: false, image_handle: None, expanded: false });
                        rewrite_log(&app_dir, &state.history);
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
                                state.history.push(HistoryItem { datetime: Local::now(), content: filename, is_image: true, image_handle: Some(iced_image::Handle::from_path(&full_path)), expanded: false });
                                rewrite_log(&app_dir, &state.history);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        Message::ClipboardChecked(Err(_)) => {} // Игнорируем ошибки чтения буфера
        Message::CopyToClipboard(content, is_image) => {
            if is_image {
                let img_path = get_app_dir().join("captures").join(&content);
                if let Some(img) = image::ImageReader::open(img_path).ok().and_then(|r| r.decode().ok()) {
                    let rgba = img.to_rgba8();
                    let data = arboard::ImageData { width: rgba.width() as usize, height: rgba.height() as usize, bytes: std::borrow::Cow::from(rgba.into_raw()) };
                    let _ = Clipboard::new().and_then(|mut cb| cb.set_image(data));
                }
            } else { let _ = Clipboard::new().and_then(|mut cb| cb.set_text(content)); }
        }
    }
    Task::none()
}

fn view(state: &AppState) -> Element<'_, Message> {
    let now = Local::now();
    let search_bar = text_input("Поиск по истории...", &state.search_query).on_input(Message::SearchChanged).padding(10);
    let filter_label = if state.days_filter < 1.0 { "За всё время".to_string() } else { format!("За последние {} дн.", state.days_filter as i32) };
    let date_slider = row![text(filter_label).width(Length::Fixed(150.0)), slider(0.0..=30.0, state.days_filter, Message::FilterChanged)].spacing(10).align_y(Alignment::Center);

    let mut history_column = column![].spacing(10);
    for (i, item) in state.history.iter().enumerate().rev() {
        let query_match = state.search_query.is_empty() || item.content.to_lowercase().contains(&state.search_query.to_lowercase());
        let date_match = state.days_filter < 1.0 || (now - item.datetime) < Duration::days(state.days_filter as i64);

        if query_match && date_match {
            let mut card_column = column![text(item.datetime.format("%d.%m %H:%M:%S").to_string()).size(11).color(Color::from_rgb(0.5, 0.5, 0.5))].spacing(5);
            if item.is_image {
                if let Some(handle) = &item.image_handle {
                    let (w, h) = if item.expanded { (400, 300) } else { (150, 100) };
                    card_column = card_column.push(button(iced_image(handle.clone()).width(w).height(h)).on_press(Message::ToggleExpand(i)).style(|_, _| button::Style::default()));
                }
            } else {
                let displayed = if item.expanded { item.content.clone() } else { item.content.chars().take(100).collect::<String>() + if item.content.len() > 100 { "..." } else { "" } };
                card_column = card_column.push(button(text(displayed).size(14)).on_press(Message::ToggleExpand(i)).style(|_, _| button::Style::default()));
            }
            let buttons = row![button("Копировать").on_press(Message::CopyToClipboard(item.content.clone(), item.is_image)).padding(5), button("Удалить").on_press(Message::DeleteItem(i)).padding(5)].spacing(10);
            history_column = history_column.push(container(column![card_column, buttons].spacing(10)).padding(12).width(Length::Fill).style(|_| container::Style { background: Some(iced::Background::Color(Color::from_rgb(0.18, 0.18, 0.18))), border: iced::Border { radius: 8.0.into(), ..Default::default() }, ..Default::default() }));
        }
    }

    container(column![
        row![text("Mega Clipboard Pro").size(26).color(Color::from_rgb(0.4, 0.6, 1.0)), space().width(Length::Fill), row![text("Автозапуск").size(14), checkbox(state.auto_start).on_toggle(Message::ToggleAutoStart)].spacing(5).align_y(Alignment::Center)].align_y(Alignment::Center),
        space().height(10), search_bar, date_slider, space().height(10), scrollable(history_column).height(Length::Fill)
    ].spacing(10)).padding(25).style(|_| container::Style { background: Some(iced::Background::Color(Color::from_rgb(0.08, 0.08, 0.08))), ..Default::default() }).into()
}

fn check_clipboard_async() -> Result<ClipboardContent, String> {
    let mut clipboard = Clipboard::new().map_err(|e| e.to_string())?;
    if let Ok(img) = clipboard.get_image() { return Ok(ClipboardContent::Image(img.bytes.into_owned(), img.width as u32, img.height as u32)); }
    if let Ok(txt) = clipboard.get_text() { return Ok(ClipboardContent::Text(txt)); }
    Ok(ClipboardContent::Empty)
}

fn subscription(_state: &AppState) -> Subscription<Message> {
    Subscription::batch(vec![
        time::every(std::time::Duration::from_millis(200)).map(|_| Message::Tick),
        iced::event::listen_with(|event, _status, _id| if let iced::Event::Window(window::Event::CloseRequested) = event { Some(Message::CloseRequested) } else { None }),
    ])
}

fn main() -> iced::Result {
    let tray_menu = Menu::new();
    let _ = tray_menu.append(&MenuItem::with_id("open", "Открыть", true, None));
    let _ = tray_menu.append(&MenuItem::with_id("quit", "Выйти", true, None));
    let tray = Arc::new(TrayIconBuilder::new().with_menu(Box::new(tray_menu)).with_tooltip("Mega Clipboard").with_icon(load_icon()).build().unwrap());
    
    iced::application(move || AppState { history: load_history(), _tray: tray.clone(), last_text: String::new(), last_image_hash: Vec::new(), search_query: String::new(), days_filter: 0.0, auto_start: check_auto_start() }, update, view)
        .subscription(subscription).title("Mega Clipboard").window(window::Settings { exit_on_close_request: false, ..Default::default() }).run()
}
