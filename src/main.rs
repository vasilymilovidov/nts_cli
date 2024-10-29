// A simple CLI radio player that allows you to listen to NTS radio live stations and mixtapes.
//

//
// DEPENDENCIES
//

mod mp3_decoder;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use mp3_decoder::Mp3StreamDecoder;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Terminal,
};
use reqwest::blocking::Client;
use rodio::{OutputStream, Sink};
use serde_json::Value;
use std::{
    env,
    fs::OpenOptions,
    io::{self, BufReader, Read},
    path::PathBuf,
    process::{Child, Command},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use std::io::Write;
use tempfile::tempdir;

//
// CONSTANTS
//

const HISTORY_FILE_PATH: &str = "./nts_cli_song_history.txt";
const STREAM_URL_1: &str = "https://stream-mixtape-geo.ntslive.net/stream";
const STREAM_URL_2: &str = "https://stream-mixtape-geo.ntslive.net/stream2";
const DEFAULT_DURATION_SEC: u64 = 5;
const DEFAULT_VOLUME: f32 = 1.0;

//
// MAIN
//

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (ui_tx, ui_rx): (Sender<UIMessage>, Receiver<UIMessage>) = mpsc::channel();
    let ui_tx_clone = ui_tx.clone();

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut radio = Radio::new(ui_tx_clone);

    ui_tx.send(UIMessage::UpdateUI).unwrap();

    let ui_tx_clone = ui_tx.clone();
    thread::spawn(move || loop {
        if let Event::Key(key) = event::read().unwrap() {
            ui_tx.send(UIMessage::KeyPress(key)).unwrap();
        } else if let Event::Resize(_, _) = event::read().unwrap() {
            ui_tx.send(UIMessage::UpdateUI).unwrap();
        }
    });

    thread::spawn(move || loop {
        let duration = duration_until_next_hour();
        thread::sleep(duration);
        ui_tx_clone
            .send(UIMessage::UpdateStreamsCollection)
            .unwrap();
    });

    loop {
        match ui_rx.recv()? {
            UIMessage::UpdateUI => radio.render_ui(&mut terminal)?,
            UIMessage::KeyPress(key) => {
                radio.handle_key_press(key)?;
                radio.render_ui(&mut terminal)?
            }
            UIMessage::RecognitionResult => {
                radio.handle_recognition_result();
                radio.render_ui(&mut terminal)?
            }
            UIMessage::UpdateStreamsCollection => {
                radio.update_collection();
                radio.render_ui(&mut terminal)?
            }
        }
    }
}

//
// STRUCTURES AND METHODS
//

// DEALING WITH STREAMS

#[derive(Default, Clone, Debug)]
struct Stream {
    title: String,
    subtitle: String,
    description: String,
    audio_stream_endpoint: String,
}

#[derive(Clone, Debug)]
enum StreamType {
    Mixtape,
    Station,
}

#[derive(Default, Clone, Debug)]
struct StreamsCollection {
    mixtapes: Vec<Stream>,
    stations: Vec<Stream>,
}

impl StreamsCollection {
    fn populate_collection() -> Result<StreamsCollection, Box<dyn std::error::Error>> {
        let mixtapes =
            Self::fetch_streams("https://www.nts.live/api/v2/mixtapes", |item| Stream {
                title: item["title"].as_str().unwrap_or_default().to_string(),
                subtitle: item["subtitle"].as_str().unwrap_or_default().to_string(),
                description: item["description"].as_str().unwrap_or_default().to_string(),
                audio_stream_endpoint: item["audio_stream_endpoint"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            })?;

        let mut stations =
            Self::fetch_streams("https://www.nts.live/api/v2/live", |item| Stream {
                title: "NTS Live 1".to_string(),
                subtitle: item["now"]["broadcast_title"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                description: item["now"]["embeds"]["details"]["description"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
                audio_stream_endpoint: STREAM_URL_1.to_string(),
            })?;

        if let Some(second_station) = stations.get_mut(1) {
            second_station.title = "NTS Live 2".to_string();
            second_station.audio_stream_endpoint = STREAM_URL_2.to_string();
        }

        Ok(StreamsCollection { mixtapes, stations })
    }

    fn fetch_streams<F>(url: &str, parse_item: F) -> Result<Vec<Stream>, Box<dyn std::error::Error>>
    where
        F: Fn(&Value) -> Stream,
    {
        let client = Client::new();
        let response = client.get(url).send()?.text()?;

        let json: Value = serde_json::from_str(&response)?;
        let collection: Vec<Stream> = json["results"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .map(parse_item)
            .collect();

        Ok(collection)
    }
}

// DEALING WITH THE UI AND EVENTS

enum UIMessage {
    UpdateUI,
    KeyPress(KeyEvent),
    RecognitionResult,
    UpdateStreamsCollection,
}

struct Radio {
    streams_collection: StreamsCollection,
    selected_stream_index: usize,
    playing_process: Option<Child>,
    sink: Option<Sink>,
    current_stream_url: Option<String>,
    recognition_result: Option<String>,
    duration: u64,
    recognition_result_tx: Sender<String>,
    recognition_result_rx: Receiver<String>,
    ui_tx: Sender<UIMessage>,
    _stream: Option<OutputStream>,
    volume: f32,
    volume_display_timeout: Option<SystemTime>,
}

impl Radio {
    fn new(ui_tx: Sender<UIMessage>) -> Self {
        let streams_collection = StreamsCollection::populate_collection().unwrap();
        let selected_stream_index = 0;
        let (recognition_result_tx, recognition_result_rx) = mpsc::channel();
        Radio {
            streams_collection,
            selected_stream_index,
            playing_process: None,
            sink: None,
            current_stream_url: None,
            recognition_result: Some("No song recognized".to_string()),
            duration: DEFAULT_DURATION_SEC,
            recognition_result_tx,
            recognition_result_rx,
            ui_tx,
            _stream: None,
            volume: DEFAULT_VOLUME,
            volume_display_timeout: None,
        }
    }

    fn update_collection(&mut self) {
        self.streams_collection = StreamsCollection::populate_collection().unwrap();
    }

    fn stop_playing_process(&mut self) {
        if let Some(mut child) = self.playing_process.take() {
            let _ = child.kill();
        }
    }

    fn play(&mut self, stream_type: StreamType) {
        let selected_stream = match stream_type {
            StreamType::Mixtape => {
                &self.streams_collection.mixtapes[self.selected_stream_index - 2]
            }
            StreamType::Station => {
                &self.streams_collection.stations[self.selected_stream_index % 2]
            }
        };

        let stream_url = selected_stream.audio_stream_endpoint.clone();
        self.stop_playing_process();

        let (_stream, stream_handle) = OutputStream::try_default().unwrap();
        let sink = Sink::try_new(&stream_handle).unwrap();

        let response = reqwest::blocking::get(&stream_url).unwrap();
        let source = Mp3StreamDecoder::new(BufReader::new(response), 8096).unwrap();

        thread::sleep(Duration::from_millis(200));

        sink.append(source);
        sink.set_volume(self.volume);

        self.sink = Some(sink);
        self.current_stream_url = Some(stream_url);
        self._stream = Some(_stream);
    }

    fn stop(&mut self) {
        self.stop_playing_process();
        self.current_stream_url = None;
    }

    fn start_recognition(&mut self) {
        self.recognition_result = None;
        let stream_url = self.current_stream_url.clone();
        let duration = self.duration;
        let recognition_result_tx = self.recognition_result_tx.clone();
        let ui_tx = self.ui_tx.clone();
    
        thread::spawn(move || {
            let dir = tempdir().unwrap();
            let temp_file_path = dir.path().join("sample.mp3");
    
            // Create a temporary file to save the audio stream
            let mut temp_file = std::fs::File::create(&temp_file_path).unwrap();
    
            // Stream the audio and save it to the temporary file
            let response = reqwest::blocking::get(&stream_url.unwrap()).unwrap();
            let mut reader = BufReader::new(response);
    
            let mut buffer = [0; 4096];
            let mut total_bytes_written = 0;
            let max_bytes = duration as usize * 128 * 1024; // Approximate bytes for the given duration
    
            while let Ok(bytes_read) = reader.read(&mut buffer) {
                if bytes_read == 0 {
                    break;
                }
                temp_file.write_all(&buffer[..bytes_read]).unwrap();
                total_bytes_written += bytes_read;
                if total_bytes_written >= max_bytes {
                    break;
                }
            }
    
            // Use the saved audio file for recognition
            if let Ok(vibra_output) = Command::new("vibra")
                .args(["-R", "--file", temp_file_path.to_str().unwrap()])
                .output()
            {
                if vibra_output.status.success() {
                    let recognition_json = String::from_utf8_lossy(&vibra_output.stdout);
                    let recognition_value: Value = serde_json::from_str(&recognition_json).unwrap();
    
                    let recognition_text = recognition_value
                        .get("track")
                        .map(|track| {
                            let title = track
                                .get("title")
                                .and_then(Value::as_str)
                                .unwrap_or("Unknown Title");
                            let subtitle = track
                                .get("subtitle")
                                .and_then(Value::as_str)
                                .unwrap_or("Unknown Artist");
                            format!("{} - {}", title, subtitle)
                        })
                        .unwrap_or_else(|| "No song recognized".to_string());
    
                    if recognition_text != "No song recognized" {
                        append_to_recognition_history(&recognition_text).unwrap();
                    }
    
                    recognition_result_tx.send(recognition_text).unwrap();
                    ui_tx.send(UIMessage::RecognitionResult).unwrap();
                }
            }
        });
    }

    fn handle_recognition_result(&mut self) {
        if let Ok(result) = self.recognition_result_rx.try_recv() {
            self.recognition_result = Some(result);
        }
    }

    fn render_ui(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        terminal.draw(|f| {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(1)
                .constraints(
                    [
                        Constraint::Fill(1),
                        Constraint::Fill(3),
                        Constraint::Fill(1),
                    ]
                    .as_ref(),
                )
                .split(f.area());

            let top_chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(25), Constraint::Percentage(50)].as_ref())
                .split(chunks[1]);

            let bottom_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Fill(1), Constraint::Fill(1)].as_ref())
                .split(chunks[2]);

            let create_list_item = |title: &str, is_selected: bool| {
                let style = if is_selected {
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Red)
                };
                ListItem::new(vec![Line::from(Span::styled(title.to_string(), style))])
            };

            let stream_items_mixtapes: Vec<ListItem> = self.streams_collection
                .mixtapes
                .iter()
                .enumerate()
                .map(|(i, mixtape)| create_list_item(&mixtape.title, i + 2 == self.selected_stream_index))
                .collect();

            let stream_items_stations: Vec<ListItem> = self.streams_collection
                .stations
                .iter()
                .enumerate()
                .map(|(i, station)| create_list_item(&station.title, i == self.selected_stream_index))
                .collect();

            let live_stations_list = List::new(stream_items_stations)
                .block(create_block("Stations"))
                .highlight_style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                );

            f.render_widget(live_stations_list, chunks[0]);

            let mixtape_list = List::new(stream_items_mixtapes)
                .block(create_block("Mixtapes"))
                .highlight_style(
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                );

            f.render_widget(mixtape_list, top_chunks[0]);

            let (description, subtitle) = if self.selected_stream_index < 2 {
                let station = &self.streams_collection.stations[self.selected_stream_index];
                (station.description.clone(), station.subtitle.clone())
            } else {
                let mixtape_index = (self.selected_stream_index - 2) % self.streams_collection.mixtapes.len();
                let mixtape = &self.streams_collection.mixtapes[mixtape_index];
                (mixtape.description.clone(), mixtape.subtitle.clone())
            };

            let description = Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(subtitle, Style::new().green().italic()),
                ]),
                Line::from(Span::styled("", Style::new().green())),
                Line::from(Span::styled(description, Style::new().green())),
            ])
            .block(create_block("Description"))
            .wrap(Wrap { trim: true });

            f.render_widget(description, top_chunks[1]);

            let mut recognition_text = self.recognition_result
                .clone()
                .unwrap_or_else(|| "Recognizing...".to_string());
            let current_volume = self.volume;
            let volume_percentage = (current_volume as f32 * 100.0).round();
            if let Some(timeout) = self.volume_display_timeout {
                 if timeout.elapsed().unwrap() < Duration::from_secs(2) {
                     recognition_text = format!("{}\nVolume: {}%", recognition_text, volume_percentage);
                 } else {
                     self.volume_display_timeout = None;
                 }
             }
            let duration_text = format!("{} seconds \nResult: {}", self.duration, recognition_text);
            let recognition = Paragraph::new(duration_text)
                .block(create_block("Track Recognition")).style(Style::default().fg(Color::Blue))
                .wrap(Wrap { trim: true });
            f.render_widget(recognition, bottom_chunks[0]);

            let controls_text = format!("j/k: Move up/down | Enter: Play | s: Stop | </>: Volume | r: Recognise | +/-: Change duration | q: Quit");
            let controls = Paragraph::new(controls_text).block(create_block("Controls")).style(Style::default().fg(Color::DarkGray)).wrap(Wrap { trim: true });
            f.render_widget(controls, bottom_chunks[1]);
        })?;
        Ok(())
    }

    fn handle_key_press(&mut self, key: KeyEvent) -> Result<(), Box<dyn std::error::Error>> {
        match key.code {
            KeyCode::Char('q') => {
                self.stop_playing_process();
                disable_raw_mode()?;
                execute!(io::stdout(), LeaveAlternateScreen)?;
                std::process::exit(0);
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected_stream_index =
                    (self.selected_stream_index + 1) % (self.streams_collection.mixtapes.len() + 2)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected_stream_index =
                    (self.selected_stream_index + self.streams_collection.mixtapes.len() + 1)
                        % (self.streams_collection.mixtapes.len() + 2)
            }
            KeyCode::Enter => {
                if self.selected_stream_index <= 1 {
                    self.play(StreamType::Station);
                } else {
                    self.play(StreamType::Mixtape);
                }
                self.start_recognition();
            }
            KeyCode::Char('s') => self.stop(),
            KeyCode::Char('r') => {
                if let Some(_) = &self.current_stream_url {
                    self.start_recognition();
                }
            }
            KeyCode::Char('+') => {
                self.duration += 1;
            }
            KeyCode::Char('-') => {
                if self.duration > 1 {
                    self.duration -= 1;
                }
            }
            KeyCode::Char('<') => {
                if self.volume > 0.0 {
                    self.volume -= 0.1;
                    if let Some(sink) = &self.sink {
                        sink.set_volume(self.volume);
                        self.volume_display_timeout = Some(SystemTime::now());
                    }
                }
            }
            KeyCode::Char('>') => {
                if self.volume < 1.0 {
                    self.volume += 0.1;
                    if let Some(sink) = &self.sink {
                        sink.set_volume(self.volume);
                        self.volume_display_timeout = Some(SystemTime::now());
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

//
// UTILS
//

fn get_home_dir() -> Option<PathBuf> {
    if cfg!(target_os = "windows") {
        env::var("USERPROFILE").ok().map(PathBuf::from)
    } else {
        env::var("HOME").ok().map(PathBuf::from)
    }
}

fn get_history_file_path() -> PathBuf {
    let mut home_dir = get_home_dir().expect("Could not find home directory");
    home_dir.push(HISTORY_FILE_PATH);
    home_dir
}

fn append_to_recognition_history(text: &str) -> io::Result<()> {
    let history_file_path = get_history_file_path();
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(history_file_path)?
        .write_all(format!("{}\n", text).as_bytes())
}

fn duration_until_next_hour() -> Duration {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let secs_since_epoch = now.as_secs();
    let secs_in_hour = 3600;
    let next_hour = (secs_since_epoch / secs_in_hour + 1) * secs_in_hour;
    let duration_until_next_hour = (next_hour - secs_since_epoch) + 240;
    Duration::from_secs(duration_until_next_hour)
}

fn create_block(title: &str) -> Block {
    Block::default().borders(Borders::NONE).title(Span::styled(
        title,
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    ))
}
