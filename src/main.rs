#![windows_subsystem = "windows"]

use arboard::Clipboard;
use chrono::{DateTime, Duration, Local, TimeZone};
use directories::UserDirs;
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager,
};
use iced::widget::{
    button, checkbox, column, container, image as iced_image, row, scrollable, slider, space, text,
    text_input,
};
use iced::{time, window, Alignment, Color, Element, Length, Point, Size, Subscription, Task};
use image::{ImageBuffer, Rgba};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration as StdDuration, Instant};
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent,
};
use winreg::enums::*;
use winreg::RegKey;

use windows_sys::Win32::UI::WindowsAndMessaging::{
    FindWindowA, SetForegroundWindow, ShowWindow, SystemParametersInfoA, SPI_GETWORKAREA, SW_HIDE,
    SW_SHOW,
};

const APP_NAME: &str = "MegaClipboard";
const WIN_WIDTH: f32 = 650.0;
const WIN_HEIGHT: f32 = 750.0;

struct AppState {
    last_text: String,
    last_image_hash: Vec<u8>,
    history: Vec<HistoryItem>,
    _tray: TrayIcon,
    _hotkey_manager: GlobalHotKeyManager,
    is_visible: bool,
    last_toggle_time: Instant,
    search_query: String,
    auto_start: bool,
    days_filter: f32,
    only_favorites: bool,
    only_images: bool,
}

#[derive(Debug, Clone)]
struct HistoryItem {
    datetime: DateTime<Local>,
    content: String,
    is_image: bool,
    image_handle: Option<iced_image::Handle>,
    expanded: bool,
    is_favorite: bool,
}

#[derive(Debug, Clone)]
enum Message {
    Tick,
    ClipboardChecked(Result<ClipboardContent, String>),
    CopyToClipboard(String, bool),
    ToggleExpand(usize),
    DeleteItem(usize),
    ToggleFavorite(usize),
    SetVisibility(bool),
    ToggleVisibility,
    SearchChanged(String),
    ToggleAutoStart(bool),
    FilterChanged(f32),
    ToggleFavoriteFilter(bool),
    ToggleImageFilter(bool),
    ClearHistory,
}

#[derive(Debug, Clone)]
enum ClipboardContent {
    Text(String),
    Image(Vec<u8>, u32, u32),
    Empty,
}

fn load_icon() -> Icon {
    let icon_bytes = include_bytes!("icon.ico");
    if let Ok(img) = image::load_from_memory(icon_bytes) {
        let rgba = img.to_rgba8();
        let (w, h) = rgba.dimensions();
        Icon::from_rgba(rgba.into_raw(), w, h)
            .unwrap_or_else(|_| Icon::from_rgba(vec![0; 32 * 32 * 4], 32, 32).unwrap())
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
    if let Ok((run_key, _)) =
        hkcu.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")
    {
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
            let fav = if item.is_favorite { "FAV" } else { "REG" };
            let _ = writeln!(
                file,
                "[{}] {} {}: {}",
                item.datetime.format("%Y-%m-%d %H:%M:%S"),
                fav,
                kind,
                item.content.replace('\n', " ")
            );
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
                let time_str = &line[t_start + 1..t_end];
                let dt = chrono::NaiveDateTime::parse_from_str(time_str, "%Y-%m-%d %H:%M:%S")
                    .ok()
                    .and_then(|naive| Local.from_local_datetime(&naive).single())
                    .unwrap_or_else(|| Local::now());

                let rest = &line[t_end + 1..].trim();
                let is_favorite = rest.starts_with("FAV");

                if let Some(sep) = rest.find(": ") {
                    let kind_part = &rest[..sep];
                    let content = &rest[sep + 2..];
                    let is_image = kind_part.contains("IMAGE");
                    let mut handle = None;
                    if is_image {
                        let img_path = app_dir.join("captures").join(content);
                        if img_path.exists() {
                            handle = Some(iced_image::Handle::from_path(img_path));
                        }
                    }
                    items.push(HistoryItem {
                        datetime: dt,
                        content: content.to_string(),
                        is_image,
                        image_handle: handle,
                        expanded: false,
                        is_favorite,
                    });
                }
            }
        }
    }
    items
}

fn update(state: &mut AppState, message: Message) -> Task<Message> {
    match message {
        Message::Tick => {
            let mut hotkey_detected = false;
            while let Ok(_) = GlobalHotKeyEvent::receiver().try_recv() {
                hotkey_detected = true;
            }
            if hotkey_detected {
                if state.last_toggle_time.elapsed() > StdDuration::from_millis(300) {
                    state.last_toggle_time = Instant::now();
                    return Task::done(Message::ToggleVisibility);
                }
            }
            while let Ok(event) = TrayIconEvent::receiver().try_recv() {
                if let TrayIconEvent::Click {
                    button: MouseButton::Left,
                    ..
                } = event
                {
                    return Task::done(Message::SetVisibility(true));
                }
            }
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                if event.id.as_ref() == "open" {
                    return Task::done(Message::SetVisibility(true));
                }
                if event.id.as_ref() == "quit" {
                    std::process::exit(0);
                }
            }
            static mut LAST_CLIP_CHECK: Option<Instant> = None;
            unsafe {
                if LAST_CLIP_CHECK.is_none()
                    || LAST_CLIP_CHECK.unwrap().elapsed() > StdDuration::from_millis(500)
                {
                    LAST_CLIP_CHECK = Some(Instant::now());
                    return Task::perform(
                        async { check_clipboard_async() },
                        Message::ClipboardChecked,
                    );
                }
            }
        }
        Message::ToggleVisibility => return Task::done(Message::SetVisibility(!state.is_visible)),
        Message::SetVisibility(show) => {
            state.is_visible = show;
            unsafe {
                let title = b"Mega Clipboard\0";
                let hwnd = FindWindowA(std::ptr::null(), title.as_ptr());
                if !hwnd.is_null() {
                    if show {
                        ShowWindow(hwnd, SW_SHOW);
                        SetForegroundWindow(hwnd);
                    } else {
                        ShowWindow(hwnd, SW_HIDE);
                    }
                }
            }
        }
        Message::SearchChanged(q) => state.search_query = q,
        Message::FilterChanged(f) => state.days_filter = f,
        Message::ToggleFavoriteFilter(val) => state.only_favorites = val,
        Message::ToggleImageFilter(val) => state.only_images = val,
        Message::ToggleAutoStart(enabled) => {
            state.auto_start = enabled;
            set_auto_start(enabled);
        }
        Message::ToggleExpand(idx) => {
            if let Some(item) = state.history.get_mut(idx) {
                item.expanded = !item.expanded;
            }
        }
        Message::ToggleFavorite(idx) => {
            if let Some(item) = state.history.get_mut(idx) {
                item.is_favorite = !item.is_favorite;
                rewrite_log(&get_app_dir(), &state.history);
            }
        }
        Message::DeleteItem(idx) => {
            let app_dir = get_app_dir();
            if let Some(item) = state.history.get(idx) {
                if item.is_image {
                    let _ = fs::remove_file(app_dir.join("captures").join(&item.content));
                }
            }
            state.history.remove(idx);
            rewrite_log(&app_dir, &state.history);
        }
        Message::ClearHistory => {
            let app_dir = get_app_dir();
            for item in state
                .history
                .iter()
                .filter(|i| !i.is_favorite && i.is_image)
            {
                let _ = fs::remove_file(app_dir.join("captures").join(&item.content));
            }
            state.history.retain(|item| item.is_favorite);
            rewrite_log(&app_dir, &state.history);
        }
        Message::ClipboardChecked(result) => {
            let app_dir = get_app_dir();
            match result {
                Ok(ClipboardContent::Text(new_text)) => {
                    if !new_text.is_empty() && new_text != state.last_text {
                        state.last_text = new_text.clone();
                        if !state
                            .history
                            .iter()
                            .any(|i| !i.is_image && i.content == new_text)
                        {
                            state.history.push(HistoryItem {
                                datetime: Local::now(),
                                content: new_text,
                                is_image: false,
                                image_handle: None,
                                expanded: false,
                                is_favorite: false,
                            });
                            rewrite_log(&app_dir, &state.history);
                        }
                    }
                }
                Ok(ClipboardContent::Image(bytes, w, h)) => {
                    if bytes != state.last_image_hash {
                        state.last_image_hash = bytes.clone();
                        let filename = format!("img_{}.png", Local::now().format("%Y%m%d_%H%M%S"));
                        let full_path = app_dir.join("captures").join(&filename);
                        let _ = fs::create_dir_all(app_dir.join("captures"));
                        if let Some(img) = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(w, h, bytes) {
                            if img.save(&full_path).is_ok() {
                                state.history.push(HistoryItem {
                                    datetime: Local::now(),
                                    content: filename,
                                    is_image: true,
                                    image_handle: Some(iced_image::Handle::from_path(&full_path)),
                                    expanded: false,
                                    is_favorite: false,
                                });
                                rewrite_log(&app_dir, &state.history);
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
                if let Ok(img) = image::ImageReader::open(img_path).and_then(|r| {
                    r.decode()
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
                }) {
                    let rgba = img.to_rgba8();
                    let data = arboard::ImageData {
                        width: rgba.width() as usize,
                        height: rgba.height() as usize,
                        bytes: std::borrow::Cow::from(rgba.into_raw()),
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
    let now = Local::now();
    let search_bar = text_input("Поиск (текст или дата ДД.ММ)...", &state.search_query)
        .on_input(Message::SearchChanged)
        .padding(10);

    let filter_label = if state.days_filter < 1.0 {
        "За всё время".to_string()
    } else {
        format!("За последние {} дн.", state.days_filter as i32)
    };

    let date_filter_row = row![
        text(filter_label)
            .size(14)
            .color(Color::WHITE)
            .width(Length::Fixed(150.0)),
        slider(0.0..=90.0, state.days_filter, Message::FilterChanged),
        space().width(Length::Fill),
        row![
            text("★").size(14).color(Color::WHITE),
            checkbox(state.only_favorites).on_toggle(Message::ToggleFavoriteFilter)
        ]
        .spacing(5)
        .align_y(Alignment::Center),
        row![
            text("🖼").size(14).color(Color::WHITE),
            checkbox(state.only_images).on_toggle(Message::ToggleImageFilter)
        ]
        .spacing(5)
        .align_y(Alignment::Center)
    ]
    .spacing(15)
    .align_y(Alignment::Center);

    let mut history_column = column![].spacing(10);
    for (i, item) in state.history.iter().enumerate().rev() {
        let query = state.search_query.to_lowercase();
        let query_match = query.is_empty()
            || item.content.to_lowercase().contains(&query)
            || item.datetime.format("%d.%m").to_string().contains(&query)
            || item
                .datetime
                .format("%d.%m.%Y")
                .to_string()
                .contains(&query);

        let date_match = state.days_filter < 1.0
            || (now - item.datetime) < Duration::days(state.days_filter as i64);
        let fav_match = !state.only_favorites || item.is_favorite;
        let img_match = !state.only_images || item.is_image;

        if query_match && date_match && fav_match && img_match {
            let mut card_column = column![row![
                text(item.datetime.format("%d.%m %H:%M:%S").to_string())
                    .size(11)
                    .color(Color::from_rgb(0.7, 0.7, 0.7)),
                space().width(Length::Fill),
                button(
                    text(if item.is_favorite { "★" } else { "☆" }).color(if item.is_favorite {
                        Color::from_rgb(1.0, 0.8, 0.0)
                    } else {
                        Color::WHITE
                    })
                )
                .on_press(Message::ToggleFavorite(i))
                .style(|_, _| button::Style::default())
            ]
            .align_y(Alignment::Center)]
            .spacing(5);

            if item.is_image {
                if let Some(handle) = &item.image_handle {
                    let (w, h) = if item.expanded {
                        (400, 300)
                    } else {
                        (150, 100)
                    };
                    card_column = card_column.push(
                        button(iced_image(handle.clone()).width(w).height(h))
                            .on_press(Message::ToggleExpand(i))
                            .style(|_, _| button::Style::default()),
                    );
                }
            } else {
                let displayed = if item.expanded {
                    item.content.clone()
                } else {
                    item.content.chars().take(100).collect::<String>()
                        + if item.content.len() > 100 { "..." } else { "" }
                };
                card_column = card_column.push(
                    button(text(displayed).size(14).color(Color::WHITE))
                        .on_press(Message::ToggleExpand(i))
                        .style(|_, _| button::Style::default()),
                );
            }

            let buttons = row![
                button(text("Копировать").color(Color::WHITE))
                    .on_press(Message::CopyToClipboard(
                        item.content.clone(),
                        item.is_image
                    ))
                    .padding(5),
                button(text("Удалить").color(Color::WHITE))
                    .on_press(Message::DeleteItem(i))
                    .padding(5)
            ]
            .spacing(10);

            history_column = history_column.push(
                container(column![card_column, buttons].spacing(10))
                    .padding(12)
                    .width(Length::Fill)
                    .style(|_| container::Style {
                        background: Some(iced::Background::Color(Color::from_rgb(
                            0.18, 0.18, 0.18,
                        ))),
                        border: iced::Border {
                            radius: 8.0.into(),
                            ..Default::default()
                        },
                        ..Default::default()
                    }),
            );
        }
    }

    container(
        column![
            row![
                text("Mega Clipboard").size(26).color(Color::WHITE),
                space().width(Length::Fill),
                button(
                    text("Очистить историю")
                        .size(12)
                        .color(Color::from_rgb(0.8, 0.4, 0.4))
                )
                .on_press(Message::ClearHistory)
                .style(|_, _| button::Style::default()),
                space().width(10),
                row![
                    text("Автозапуск").size(14).color(Color::WHITE),
                    checkbox(state.auto_start).on_toggle(Message::ToggleAutoStart)
                ]
                .spacing(5)
                .align_y(Alignment::Center)
            ]
            .align_y(Alignment::Center),
            space().height(10),
            search_bar,
            date_filter_row,
            space().height(10),
            scrollable(history_column).height(Length::Fill)
        ]
        .spacing(10),
    )
    .padding(25)
    .style(|_| container::Style {
        background: Some(iced::Background::Color(Color::from_rgb(0.08, 0.08, 0.08))),
        border: iced::Border {
            radius: 12.0.into(),
            ..Default::default()
        },
        ..Default::default()
    })
    .into()
}

fn check_clipboard_async() -> Result<ClipboardContent, String> {
    let mut clipboard = Clipboard::new().map_err(|e| e.to_string())?;
    if let Ok(img) = clipboard.get_image() {
        return Ok(ClipboardContent::Image(
            img.bytes.into_owned(),
            img.width as u32,
            img.height as u32,
        ));
    }
    if let Ok(txt) = clipboard.get_text() {
        return Ok(ClipboardContent::Text(txt));
    }
    Ok(ClipboardContent::Empty)
}

fn subscription(_state: &AppState) -> Subscription<Message> {
    time::every(StdDuration::from_millis(30)).map(|_| Message::Tick)
}

fn main() -> iced::Result {
    let (x, y) = unsafe {
        let mut work_area: [i32; 4] = [0; 4];
        SystemParametersInfoA(SPI_GETWORKAREA, 0, work_area.as_mut_ptr() as *mut _, 0);
        let wa_width = work_area[2] - work_area[0];
        let wa_bottom = work_area[3];
        ((work_area[0] + (wa_width - WIN_WIDTH as i32) / 2), (wa_bottom - WIN_HEIGHT as i32 - 10))
    };

    let icon_bytes = include_bytes!("icon.ico");

    let mut initial_text = String::new();
    let mut initial_image_hash = Vec::new();
    if let Ok(mut cb) = Clipboard::new() {
        if let Ok(txt) = cb.get_text() { initial_text = txt; }
        if let Ok(img) = cb.get_image() { initial_image_hash = img.bytes.into_owned(); }
    }

    iced::application(
        move || {
            let tray_menu = Menu::new();
            let _ = tray_menu.append(&MenuItem::with_id("open", "Открыть", true, None));
            let _ = tray_menu.append(&MenuItem::with_id("quit", "Выйти", true, None));
            let tray = TrayIconBuilder::new().with_menu(Box::new(tray_menu)).with_tooltip("Mega Clipboard").with_icon(load_icon()).build().unwrap();
            let hotkey_manager = GlobalHotKeyManager::new().unwrap();
            let _ = hotkey_manager.register(HotKey::new(Some(Modifiers::ALT), Code::KeyV));

            (AppState { 
                history: load_history(), _tray: tray, _hotkey_manager: hotkey_manager, is_visible: false,
                last_toggle_time: Instant::now(), 
                last_text: initial_text.clone(),           // <- Вызываем clone()
                last_image_hash: initial_image_hash.clone(), // <- Вызываем clone()
                search_query: String::new(), auto_start: check_auto_start(), days_filter: 0.0, 
                only_favorites: false, only_images: false
            }, Task::none())
        }, 
        update, 
        view
    )
    .subscription(subscription)
    .title("Mega Clipboard")
    .window(window::Settings { 
        size: Size::new(WIN_WIDTH, WIN_HEIGHT),
        position: window::Position::Specific(Point::new(x as f32, y as f32)),
        resizable: false, decorations: false, transparent: true, visible: false,
        icon: Some(window::icon::from_file_data(icon_bytes, None).unwrap()),
        exit_on_close_request: false,
        ..Default::default() 
    })
    .run()
}
