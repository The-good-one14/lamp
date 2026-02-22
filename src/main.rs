use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, BorderType},
    Terminal,
};
use rodio::{Decoder, OutputStream, OutputStreamHandle, Sink};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, BufReader, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    sync::mpsc::{self, Receiver, Sender},
    time::{Duration, Instant},
};
use walkdir::WalkDir;
use rand::seq::SliceRandom;
use rand::rng;
use serde::{Deserialize, Serialize};
use sha2::{Sha256, Digest};
use id3::Tag;

// MPRIS related imports
use mpris_server::{
    Metadata as MprisMetadata, PlayerInterface, RootInterface,
    Server, Time, LoopStatus, PlaybackStatus, PlaybackRate, TrackId, Volume,
    Property,
};
use zbus::fdo;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Playlists,
    Library,
    PlaylistContent,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    NamingPlaylist,
    RenamingPlaylist,
    Searching,
}

#[derive(Debug)]
enum ExternalCommand {
    PlayPause,
    Next,
    Previous,
    Stop,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct SongMetadata {
    title: String,
    artist: String,
    album: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Song {
    path: PathBuf,
    metadata: SongMetadata,
}

#[derive(Serialize, Deserialize, Default)]
struct SavedPlaylists {
    playlists: HashMap<String, Vec<Song>>,
}

#[derive(Clone)]
struct SharedState {
    is_playing: bool,
    current_song: Option<Song>,
    loop_mode: bool,
    shuffle_mode: bool,
}

struct App {
    library: Vec<Song>,
    library_state: ListState,
    playlists: HashMap<String, Vec<Song>>,
    playlist_names: Vec<String>,
    playlist_state: ListState,
    
    current_list: Vec<Song>,
    current_list_state: ListState,
    active_playlist_name: String,

    sink: Sink,
    _stream: OutputStream,
    _stream_handle: OutputStreamHandle,
    current_index: Option<usize>,
    playing_song: Option<Song>, // Decoupled "Now Playing" state
    is_playing: bool,
    loop_mode: bool,
    shuffle_mode: bool,
    volume: f32,
    
    focus: Focus,
    input_mode: InputMode,
    input: String,
    search_query: String,
    
    shared_state: Arc<Mutex<SharedState>>,
    status_tx: Sender<SharedState>,
    
    selected_playlist_content_state: ListState,
}

impl App {
    fn new(root_path: &Path, shared_state: Arc<Mutex<SharedState>>, status_tx: Sender<SharedState>) -> Result<Self> {
        let (_stream, _stream_handle) = OutputStream::try_default()?;
        let sink = Sink::try_new(&_stream_handle)?;

        let mut library = Vec::new();
        let mut seen_hashes = HashSet::new();

        for entry in WalkDir::new(root_path).into_iter().filter_map(|e| e.ok()) {
            if entry.path().extension().map_or(false, |ext| ext == "mp3") {
                let p = entry.path().to_path_buf();
                if let Ok(hash) = App::calculate_hash(&p) {
                    if !seen_hashes.contains(&hash) {
                        seen_hashes.insert(hash.clone());
                        let metadata = App::extract_metadata(&p, root_path);
                        library.push(Song { path: p, metadata });
                    }
                }
            }
        }
        library.sort_by(|a, b| a.metadata.title.cmp(&b.metadata.title));

        let mut library_state = ListState::default();
        if !library.is_empty() {
            library_state.select(Some(0));
        }

        let mut playlists = HashMap::new();
        if let Ok(content) = fs::read_to_string("playlists.json") {
            if let Ok(saved) = serde_json::from_str::<SavedPlaylists>(&content) {
                playlists = saved.playlists;
            }
        }
        
        // Automatic Album Playlists
        for song in &library {
            let album = &song.metadata.album;
            if album != "N/A" && album != "Single" {
                if !playlists.contains_key(album) {
                    playlists.insert(album.clone(), Vec::new());
                }
                let album_list = playlists.get_mut(album).unwrap();
                if !album_list.contains(song) {
                    album_list.push(song.clone());
                }
            }
        }
        
        let mut playlist_names: Vec<String> = playlists.keys().cloned().collect();
        playlist_names.sort();

        let mut playlist_state = ListState::default();
        if !playlist_names.is_empty() {
            playlist_state.select(Some(0));
        }

        let current_list = library.clone();
        let mut current_list_state = ListState::default();
        if !current_list.is_empty() {
            current_list_state.select(Some(0));
        }

        Ok(App {
            library,
            library_state,
            playlists,
            playlist_names,
            playlist_state,
            current_list,
            current_list_state,
            active_playlist_name: "Library".to_string(),
            sink,
            _stream,
            _stream_handle,
            current_index: None,
            playing_song: None,
            is_playing: false,
            loop_mode: false,
            shuffle_mode: false,
            volume: 1.0,
            focus: Focus::Library,
            input_mode: InputMode::Normal,
            input: String::new(),
            search_query: String::new(),
            shared_state,
            status_tx,
            selected_playlist_content_state: ListState::default(),
        })
    }

    fn extract_metadata(path: &Path, root: &Path) -> SongMetadata {
        let mut artist = None;
        let mut album = None;
        let mut title = None;

        if let Ok(tag) = Tag::read_from_path(path) {
            artist = tag.artist().map(|s| s.to_string());
            album = tag.album().map(|s| s.to_string());
            title = tag.title().map(|s| s.to_string());
        }

        if artist.is_none() || album.is_none() || title.is_none() {
            if let Ok(rel_path) = path.strip_prefix(root) {
                let components: Vec<_> = rel_path.components().collect();
                let depth = components.len();
                if depth >= 3 {
                    if artist.is_none() { artist = Some(components[depth - 3].as_os_str().to_string_lossy().to_string()); }
                    if album.is_none() { album = Some(components[depth - 2].as_os_str().to_string_lossy().to_string()); }
                } else if depth == 2 {
                    if artist.is_none() { artist = Some(components[depth - 2].as_os_str().to_string_lossy().to_string()); }
                    if album.is_none() { album = Some("Single".to_string()); }
                }
            }
        }

        SongMetadata {
            title: title.unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().to_string()),
            artist: artist.unwrap_or_else(|| "N/A".to_string()),
            album: album.unwrap_or_else(|| "N/A".to_string()),
        }
    }

    fn calculate_hash(path: &Path) -> Result<String> {
        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0; 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 { break; }
            hasher.update(&buffer[..count]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    fn save_playlists(&self) -> Result<()> {
        let saved = SavedPlaylists {
            playlists: self.playlists.clone(),
        };
        let content = serde_json::to_string_pretty(&saved)?;
        fs::write("playlists.json", content)?;
        Ok(())
    }

    fn add_to_playlist(&mut self, name: &str, song: Song) {
        if let Some(p_list) = self.playlists.get_mut(name) {
            if !p_list.contains(&song) {
                p_list.push(song);
                let _ = self.save_playlists();
            }
        }
    }

    fn create_playlist(&mut self, name: String) {
        if !name.is_empty() && !self.playlists.contains_key(&name) {
            self.playlists.insert(name.clone(), Vec::new());
            self.playlist_names.push(name);
            self.playlist_names.sort();
            let _ = self.save_playlists();
        }
    }

    fn rename_selected_playlist(&mut self, new_name: String) {
        if new_name.is_empty() { return; }
        if let Some(idx) = self.playlist_state.selected() {
            let old_name = self.playlist_names[idx].clone();
            if let Some(songs) = self.playlists.remove(&old_name) {
                self.playlists.insert(new_name.clone(), songs);
                self.playlist_names[idx] = new_name.clone();
                self.playlist_names.sort();
                if self.active_playlist_name == old_name {
                    self.active_playlist_name = new_name;
                }
                let _ = self.save_playlists();
            }
        }
    }

    fn delete_selected_playlist(&mut self) {
        if let Some(idx) = self.playlist_state.selected() {
            let name = self.playlist_names.remove(idx);
            self.playlists.remove(&name);
            let _ = self.save_playlists();
            if self.playlist_names.is_empty() {
                self.playlist_state.select(None);
            } else {
                self.playlist_state.select(Some(idx.min(self.playlist_names.len() - 1)));
            }
        }
    }

    fn move_song_in_playlist(&mut self, up: bool) {
        if let Some(p_idx) = self.playlist_state.selected() {
            let p_name = &self.playlist_names[p_idx];
            if let Some(songs) = self.playlists.get_mut(p_name) {
                if let Some(c_idx) = self.selected_playlist_content_state.selected() {
                    let new_idx = if up {
                        if c_idx == 0 { return; }
                        c_idx - 1
                    } else {
                        if c_idx >= songs.len() - 1 { return; }
                        c_idx + 1
                    };
                    
                    songs.swap(c_idx, new_idx);
                    self.selected_playlist_content_state.select(Some(new_idx));
                    
                    if self.active_playlist_name == *p_name {
                         self.current_list = songs.clone();
                         // Fix index desync if we are reordering the playlist that is CURRENTLY PLAYING
                         if let Some(playing) = &self.playing_song {
                             if let Some(new_playing_idx) = self.current_list.iter().position(|s| s.path == playing.path) {
                                 self.current_index = Some(new_playing_idx);
                             }
                         }
                    }
                    let _ = self.save_playlists();
                }
            }
        }
    }

    fn switch_active_list(&mut self) {
        if let Some(idx) = self.playlist_state.selected() {
            let name = &self.playlist_names[idx];
            if let Some(p_list) = self.playlists.get(name) {
                self.current_list = p_list.clone();
                self.active_playlist_name = name.clone();
                self.current_list_state.select(Some(0));
                if self.shuffle_mode {
                    let mut r = rng();
                    self.current_list.shuffle(&mut r);
                }
                // Important: Do NOT change playing_song or current_index here! 
                // Browsing/Switching lists shouldn't break the current "Now Playing" display.
                // We only update current_index if the playing_song is actually in the new list.
                if let Some(playing) = &self.playing_song {
                    self.current_index = self.current_list.iter().position(|s| s.path == playing.path);
                } else {
                    self.current_index = None;
                }
            }
        }
    }

    fn play_selected(&mut self) {
        if let Some(index) = self.current_list_state.selected() {
            self.play_index(index);
        }
    }

    fn sync_shared_state(&self) {
        let state = {
            let mut state = self.shared_state.lock().unwrap();
            state.is_playing = self.is_playing;
            state.loop_mode = self.loop_mode;
            state.shuffle_mode = self.shuffle_mode;
            state.current_song = self.playing_song.clone();
            state.clone()
        };
        let _ = self.status_tx.send(state);
    }

    fn play_index(&mut self, index: usize) {
        if index >= self.current_list.len() {
            return;
        }

        let song = &self.current_list[index];
        let file = match File::open(&song.path) {
            Ok(f) => f,
            Err(_) => return,
        };
        let source = match Decoder::new(BufReader::new(file)) {
            Ok(s) => s,
            Err(_) => return,
        };

        self.sink.stop();
        self.sink.append(source);
        self.sink.play();
        self.current_index = Some(index);
        self.playing_song = Some(song.clone());
        self.is_playing = true;
        self.sync_shared_state();
    }

    fn toggle_playback(&mut self) {
        if self.is_playing {
            self.sink.pause();
            self.is_playing = false;
        } else {
            if self.sink.empty() && self.current_index.is_none() {
                self.play_selected();
            } else {
                self.sink.play();
                self.is_playing = true;
            }
        }
        self.sync_shared_state();
    }

    fn next(&mut self) {
        if self.current_list.is_empty() {
            return;
        }
        let next_index = match self.current_index {
            Some(i) => (i + 1) % self.current_list.len(),
            None => 0,
        };
        self.play_index(next_index);
        self.current_list_state.select(Some(next_index));
    }

    fn previous(&mut self) {
        if self.current_list.is_empty() {
            return;
        }
        let prev_index = match self.current_index {
            Some(i) => if i == 0 { self.current_list.len() - 1 } else { i - 1 },
            None => 0,
        };
        self.play_index(prev_index);
        self.current_list_state.select(Some(prev_index));
    }

    fn toggle_loop(&mut self) {
        self.loop_mode = !self.loop_mode;
        self.sync_shared_state();
    }

    fn toggle_shuffle(&mut self) {
        self.shuffle_mode = !self.shuffle_mode;
        if self.shuffle_mode {
            let mut r = rng();
            self.current_list.shuffle(&mut r);
        } else {
            if self.active_playlist_name == "Library" {
                self.current_list = self.library.clone();
            } else if let Some(p_list) = self.playlists.get(&self.active_playlist_name) {
                self.current_list = p_list.clone();
            }
        }
        
        // Stabilize index after shuffle
        if let Some(playing) = &self.playing_song {
            if let Some(new_idx) = self.current_list.iter().position(|s| s.path == playing.path) {
                self.current_index = Some(new_idx);
            }
        }
        
        self.sync_shared_state();
    }

    fn increase_volume(&mut self) {
        self.volume = (self.volume + 0.1).min(2.0);
        self.sink.set_volume(self.volume);
    }

    fn decrease_volume(&mut self) {
        self.volume = (self.volume - 0.1).max(0.0);
        self.sink.set_volume(self.volume);
    }

    fn update(&mut self) {
        if self.is_playing && self.sink.empty() {
            if self.loop_mode {
                if let Some(i) = self.current_index {
                    self.play_index(i);
                }
            } else {
                self.next();
            }
        }
    }
}

struct MprisPlayer {
    tx: Sender<ExternalCommand>,
    state: Arc<Mutex<SharedState>>,
}

impl RootInterface for MprisPlayer {
    async fn identity(&self) -> fdo::Result<String> { Ok("Lamp".into()) }
    async fn desktop_entry(&self) -> fdo::Result<String> { Ok("lamp".into()) }
    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> { Ok(vec![]) }
    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> { Ok(vec!["audio/mpeg".into()]) }
    async fn has_track_list(&self) -> fdo::Result<bool> { Ok(false) }
    async fn can_quit(&self) -> fdo::Result<bool> { Ok(false) }
    async fn quit(&self) -> fdo::Result<()> { Ok(()) }
    async fn can_raise(&self) -> fdo::Result<bool> { Ok(false) }
    async fn raise(&self) -> fdo::Result<()> { Ok(()) }
    async fn fullscreen(&self) -> fdo::Result<bool> { Ok(false) }
    async fn set_fullscreen(&self, _fullscreen: bool) -> zbus::Result<()> { Ok(()) }
    async fn can_set_fullscreen(&self) -> fdo::Result<bool> { Ok(false) }
}

impl PlayerInterface for MprisPlayer {
    async fn next(&self) -> fdo::Result<()> { let _ = self.tx.send(ExternalCommand::Next); Ok(()) }
    async fn previous(&self) -> fdo::Result<()> { let _ = self.tx.send(ExternalCommand::Previous); Ok(()) }
    async fn pause(&self) -> fdo::Result<()> { let _ = self.tx.send(ExternalCommand::PlayPause); Ok(()) }
    async fn play_pause(&self) -> fdo::Result<()> { let _ = self.tx.send(ExternalCommand::PlayPause); Ok(()) }
    async fn stop(&self) -> fdo::Result<()> { let _ = self.tx.send(ExternalCommand::Stop); Ok(()) }
    async fn play(&self) -> fdo::Result<()> { let _ = self.tx.send(ExternalCommand::PlayPause); Ok(()) }
    async fn seek(&self, _offset: Time) -> fdo::Result<()> { Ok(()) }
    async fn set_position(&self, _track_id: TrackId, _position: Time) -> fdo::Result<()> { Ok(()) }
    async fn open_uri(&self, _uri: String) -> fdo::Result<()> { Ok(()) }

    async fn playback_status(&self) -> fdo::Result<PlaybackStatus> {
        let state = self.state.lock().unwrap();
        if state.is_playing { Ok(PlaybackStatus::Playing) } else { Ok(PlaybackStatus::Paused) }
    }
    async fn loop_status(&self) -> fdo::Result<LoopStatus> { 
        let state = self.state.lock().unwrap();
        if state.loop_mode { Ok(LoopStatus::Track) } else { Ok(LoopStatus::None) }
    }
    async fn set_loop_status(&self, _loop_status: LoopStatus) -> zbus::Result<()> { Ok(()) }
    async fn rate(&self) -> fdo::Result<PlaybackRate> { Ok(1.0) }
    async fn set_rate(&self, _rate: PlaybackRate) -> zbus::Result<()> { Ok(()) }
    async fn shuffle(&self) -> fdo::Result<bool> { 
        let state = self.state.lock().unwrap();
        Ok(state.shuffle_mode)
    }
    async fn set_shuffle(&self, _shuffle: bool) -> zbus::Result<()> { Ok(()) }
    async fn metadata(&self) -> fdo::Result<MprisMetadata> {
        let state = self.state.lock().unwrap();
        let mut m = MprisMetadata::new();
        if let Some(song) = &state.current_song {
            m.set_title(Some(song.metadata.title.clone()));
            m.set_artist(Some(vec![song.metadata.artist.clone()]));
            m.set_album(Some(song.metadata.album.clone()));
        }
        let track_id = TrackId::from(zbus::zvariant::ObjectPath::from_str_unchecked("/org/mpris/MediaPlayer2/Track/0"));
        m.set_trackid(Some(track_id));
        Ok(m)
    }
    async fn volume(&self) -> fdo::Result<Volume> { Ok(1.0) }
    async fn set_volume(&self, _volume: Volume) -> zbus::Result<()> { Ok(()) }
    async fn position(&self) -> fdo::Result<Time> { Ok(Time::ZERO) }
    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> { Ok(1.0) }
    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> { Ok(1.0) }
    async fn can_go_next(&self) -> fdo::Result<bool> { Ok(true) }
    async fn can_go_previous(&self) -> fdo::Result<bool> { Ok(true) }
    async fn can_play(&self) -> fdo::Result<bool> { Ok(true) }
    async fn can_pause(&self) -> fdo::Result<bool> { Ok(true) }
    async fn can_seek(&self) -> fdo::Result<bool> { Ok(false) }
    async fn can_control(&self) -> fdo::Result<bool> { Ok(true) }
}

async fn run_mpris_server(tx: Sender<ExternalCommand>, status_rx: Receiver<SharedState>, state: Arc<Mutex<SharedState>>) -> Result<()> {
    let player = MprisPlayer { tx, state: state.clone() };
    let server = Server::new("lamp", player).await?;
    
    loop {
        if let Ok(new_state) = status_rx.recv() {
            let mut props = Vec::new();
            props.push(Property::PlaybackStatus(if new_state.is_playing { PlaybackStatus::Playing } else { PlaybackStatus::Paused }));
            
            let mut m = MprisMetadata::new();
            if let Some(song) = &new_state.current_song {
                m.set_title(Some(song.metadata.title.clone()));
                m.set_artist(Some(vec![song.metadata.artist.clone()]));
                m.set_album(Some(song.metadata.album.clone()));
            }
            let track_id = TrackId::from(zbus::zvariant::ObjectPath::from_str_unchecked("/org/mpris/MediaPlayer2/Track/0"));
            m.set_trackid(Some(track_id));
            props.push(Property::Metadata(m));
            
            props.push(Property::LoopStatus(if new_state.loop_mode { LoopStatus::Track } else { LoopStatus::None }));
            props.push(Property::Shuffle(new_state.shuffle_mode));
            
            let _ = server.properties_changed(props).await;
        }
    }
}

fn main() -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let args: Vec<String> = std::env::args().collect();
    let path_str = args.get(1).cloned().unwrap_or_else(|| ".".to_string());
    let path = PathBuf::from(&path_str);
    
    let shared_state = Arc::new(Mutex::new(SharedState {
        is_playing: false,
        current_song: None,
        loop_mode: false,
        shuffle_mode: false,
    }));

    let (status_tx, status_rx) = mpsc::channel();
    let (cmd_tx, cmd_rx) = mpsc::channel();

    let mut app = match App::new(&path, shared_state.clone(), status_tx) {
        Ok(a) => a,
        Err(e) => {
            disable_raw_mode()?;
            execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
            return Err(anyhow::anyhow!("Failed to initialize app at {}: {}", path_str, e));
        }
    };

    let ss_for_mpris = shared_state.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(run_mpris_server(cmd_tx, status_rx, ss_for_mpris)).unwrap();
    });

    let res = run_app(&mut terminal, &mut app, cmd_rx);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = res {
        eprintln!("Error: {:?}", err);
    }

    Ok(())
}

fn run_app<B: Backend>(terminal: &mut Terminal<B>, app: &mut App, rx: Receiver<ExternalCommand>) -> anyhow::Result<()> 
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let tick_rate = Duration::from_millis(100);
    let mut last_tick = Instant::now();

    loop {
        terminal.draw(|f| ui(f, app))?;

        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                ExternalCommand::PlayPause => app.toggle_playback(),
                ExternalCommand::Next => app.next(),
                ExternalCommand::Previous => app.previous(),
                ExternalCommand::Stop => {
                    app.sink.stop();
                    app.is_playing = false;
                    app.playing_song = None;
                    app.sync_shared_state();
                }
            }
        }

        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_secs(0));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if app.input_mode != InputMode::Normal {
                    match key.code {
                        KeyCode::Enter => {
                            if app.input_mode == InputMode::NamingPlaylist {
                                let name = std::mem::take(&mut app.input);
                                app.create_playlist(name);
                            } else if app.input_mode == InputMode::RenamingPlaylist {
                                let name = std::mem::take(&mut app.input);
                                app.rename_selected_playlist(name);
                            }
                            app.input_mode = InputMode::Normal;
                        }
                        KeyCode::Char(c) => app.input.push(c),
                        KeyCode::Backspace => { app.input.pop(); }
                        KeyCode::Esc => {
                            if app.input_mode == InputMode::Searching {
                                app.search_query.clear();
                            }
                            app.input_mode = InputMode::Normal;
                        }
                        _ => {}
                    }
                    if app.input_mode == InputMode::Searching {
                        app.search_query = app.input.clone();
                    }
                    continue;
                }

                match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Tab => {
                        app.focus = match app.focus {
                            Focus::Playlists => Focus::Library,
                            Focus::Library => Focus::PlaylistContent,
                            Focus::PlaylistContent => Focus::Playlists,
                        };
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        match app.focus {
                            Focus::Library => {
                                let filtered_len = app.library.iter()
                                    .filter(|s| s.metadata.title.to_lowercase().contains(&app.search_query.to_lowercase()) || 
                                                s.metadata.artist.to_lowercase().contains(&app.search_query.to_lowercase()))
                                    .count();
                                if filtered_len > 0 {
                                    let i = match app.library_state.selected() {
                                        Some(i) => (i + 1) % filtered_len,
                                        None => 0,
                                    };
                                    app.library_state.select(Some(i));
                                }
                            }
                            Focus::Playlists => {
                                if !app.playlist_names.is_empty() {
                                    let i = match app.playlist_state.selected() {
                                        Some(i) => (i + 1) % app.playlist_names.len(),
                                        None => 0,
                                    };
                                    app.playlist_state.select(Some(i));
                                }
                            }
                            Focus::PlaylistContent => {
                                if let Some(idx) = app.playlist_state.selected() {
                                    let name = &app.playlist_names[idx];
                                    if let Some(content) = app.playlists.get(name) {
                                        if !content.is_empty() {
                                            let i = match app.selected_playlist_content_state.selected() {
                                                Some(i) => (i + 1) % content.len(),
                                                None => 0,
                                            };
                                            app.selected_playlist_content_state.select(Some(i));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        match app.focus {
                            Focus::Library => {
                                let filtered_len = app.library.iter()
                                    .filter(|s| s.metadata.title.to_lowercase().contains(&app.search_query.to_lowercase()) || 
                                                s.metadata.artist.to_lowercase().contains(&app.search_query.to_lowercase()))
                                    .count();
                                if filtered_len > 0 {
                                    let i = match app.library_state.selected() {
                                        Some(i) => if i == 0 { filtered_len - 1 } else { i - 1 },
                                        None => 0,
                                    };
                                    app.library_state.select(Some(i));
                                }
                            }
                            Focus::Playlists => {
                                if !app.playlist_names.is_empty() {
                                    let i = match app.playlist_state.selected() {
                                        Some(i) => if i == 0 { app.playlist_names.len() - 1 } else { i - 1 },
                                        None => 0,
                                    };
                                    app.playlist_state.select(Some(i));
                                }
                            }
                            Focus::PlaylistContent => {
                                if let Some(idx) = app.playlist_state.selected() {
                                    let name = &app.playlist_names[idx];
                                    if let Some(content) = app.playlists.get(name) {
                                        if !content.is_empty() {
                                            let i = match app.selected_playlist_content_state.selected() {
                                                Some(i) => if i == 0 { content.len() - 1 } else { i - 1 },
                                                None => 0,
                                            };
                                            app.selected_playlist_content_state.select(Some(i));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Enter => {
                        match app.focus {
                            Focus::Playlists => app.switch_active_list(),
                            Focus::Library => {
                                let filtered: Vec<_> = app.library.iter()
                                    .filter(|s| s.metadata.title.to_lowercase().contains(&app.search_query.to_lowercase()) || 
                                                s.metadata.artist.to_lowercase().contains(&app.search_query.to_lowercase()))
                                    .collect();
                                if let Some(idx) = app.library_state.selected() {
                                    if let Some(song) = filtered.get(idx) {
                                        app.active_playlist_name = "Library".to_string();
                                        app.current_list = app.library.clone();
                                        if app.shuffle_mode {
                                            let mut r = rng();
                                            app.current_list.shuffle(&mut r);
                                        }
                                        if let Some(new_idx) = app.current_list.iter().position(|s| s.path == song.path) {
                                            app.play_index(new_idx);
                                            app.current_list_state.select(Some(new_idx));
                                        }
                                    }
                                }
                            }
                            Focus::PlaylistContent => {
                                if let Some(p_idx) = app.playlist_state.selected() {
                                    let p_name = &app.playlist_names[p_idx];
                                    if let Some(c_idx) = app.selected_playlist_content_state.selected() {
                                        if let Some(p_list) = app.playlists.get(p_name) {
                                            app.active_playlist_name = p_name.clone();
                                            app.current_list = p_list.clone();
                                            if app.shuffle_mode {
                                                let mut r = rng();
                                                app.current_list.shuffle(&mut r);
                                            }
                                            let song = &p_list[c_idx];
                                            if let Some(new_idx) = app.current_list.iter().position(|s| s.path == song.path) {
                                                app.play_index(new_idx);
                                                app.current_list_state.select(Some(new_idx));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    KeyCode::Char(' ') => app.toggle_playback(),
                    KeyCode::Char('n') => app.next(),
                    KeyCode::Char('p') => app.previous(),
                    KeyCode::Char('l') => app.toggle_loop(),
                    KeyCode::Char('s') => app.toggle_shuffle(),
                    KeyCode::Char('=') | KeyCode::Char('+') => app.increase_volume(),
                    KeyCode::Char('-') => app.decrease_volume(),
                    KeyCode::Char('/') => {
                        app.input_mode = InputMode::Searching;
                        app.input.clear();
                        app.search_query.clear();
                    }
                    KeyCode::Char('c') => app.input_mode = InputMode::NamingPlaylist,
                    KeyCode::Char('r') => {
                        if app.focus == Focus::Playlists {
                            app.input_mode = InputMode::RenamingPlaylist;
                        }
                    }
                    KeyCode::Char('a') => {
                        let filtered: Vec<_> = app.library.iter()
                            .filter(|s| s.metadata.title.to_lowercase().contains(&app.search_query.to_lowercase()) || 
                                        s.metadata.artist.to_lowercase().contains(&app.search_query.to_lowercase()))
                            .collect();
                        if let (Some(l_idx), Some(p_idx)) = (app.library_state.selected(), app.playlist_state.selected()) {
                            if let Some(song) = filtered.get(l_idx) {
                                let p_name = app.playlist_names[p_idx].clone();
                                app.add_to_playlist(&p_name, (*song).clone());
                            }
                        }
                    }
                    KeyCode::Char('d') => {
                        if app.focus == Focus::Playlists {
                            app.delete_selected_playlist();
                        }
                    }
                    KeyCode::Char('[') => {
                        if app.focus == Focus::PlaylistContent {
                            app.move_song_in_playlist(true);
                        }
                    }
                    KeyCode::Char(']') => {
                        if app.focus == Focus::PlaylistContent {
                            app.move_song_in_playlist(false);
                        }
                    }
                    _ => {}
                }
            }
        }
        if last_tick.elapsed() >= tick_rate {
            app.update();
            last_tick = Instant::now();
        }
    }
}

fn ui(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(6),
        ])
        .split(f.area());

    // Color definitions
    let active_color = Color::Yellow;
    let inactive_color = Color::DarkGray;
    let highlight_color = Color::Blue;
    
    let header_text = match app.input_mode {
        InputMode::NamingPlaylist => format!("New Playlist: {}_", app.input),
        InputMode::RenamingPlaylist => format!("Rename Playlist: {}_", app.input),
        InputMode::Searching => format!("Search Library: {}_", app.input),
        InputMode::Normal => "Lightweight Audio Manager Panel, aka lamp".to_string(),
    };
    
    let header = Paragraph::new(header_text)
        .style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        .block(Block::default().borders(Borders::ALL).border_type(BorderType::Rounded));
    f.render_widget(header, chunks[0]);

    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(40),
            Constraint::Percentage(40),
        ])
        .split(chunks[1]);

    // Playlists View
    let p_items: Vec<ListItem> = app.playlist_names.iter()
        .map(|name| {
            let style = if app.active_playlist_name == *name {
                Style::default().fg(Color::Green).add_modifier(Modifier::ITALIC)
            } else {
                Style::default()
            };
            ListItem::new(name.as_str()).style(style)
        })
        .collect();
    let p_list = List::new(p_items)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(" Playlists ")
            .border_style(Style::default().fg(if app.focus == Focus::Playlists { active_color } else { inactive_color }))
            .border_type(if app.focus == Focus::Playlists { BorderType::Thick } else { BorderType::Plain }))
        .highlight_style(Style::default().bg(highlight_color).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");
    f.render_stateful_widget(p_list, body_chunks[0], &mut app.playlist_state);

    // Library View (Filtered)
    let search_q = app.search_query.to_lowercase();
    let lib_items: Vec<ListItem> = app.library.iter()
        .filter(|song| {
            song.metadata.title.to_lowercase().contains(&search_q) || 
            song.metadata.artist.to_lowercase().contains(&search_q)
        })
        .map(|song| {
            let label = format!("{} - {} ({})", song.metadata.artist, song.metadata.title, song.metadata.album);
            ListItem::new(label)
        })
        .collect();
    let lib_list = List::new(lib_items)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(" Library ")
            .border_style(Style::default().fg(if app.focus == Focus::Library { active_color } else { inactive_color }))
            .border_type(if app.focus == Focus::Library { BorderType::Thick } else { BorderType::Plain }))
        .highlight_style(Style::default().bg(highlight_color).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");
    f.render_stateful_widget(lib_list, body_chunks[1], &mut app.library_state);

    // Playlist Content View
    let mut pc_title = " Playlist Content ".to_string();
    let mut pc_items = Vec::new();
    if let Some(idx) = app.playlist_state.selected() {
        let name = &app.playlist_names[idx];
        pc_title = format!(" Content: {} ", name);
        if let Some(content) = app.playlists.get(name) {
            pc_items = content.iter()
                .map(|song| {
                    let label = format!("{} - {}", song.metadata.artist, song.metadata.title);
                    ListItem::new(label)
                })
                .collect();
        }
    }
    
    let pc_list = List::new(pc_items)
        .block(Block::default()
            .borders(Borders::ALL)
            .title(pc_title)
            .border_style(Style::default().fg(if app.focus == Focus::PlaylistContent { active_color } else { inactive_color }))
            .border_type(if app.focus == Focus::PlaylistContent { BorderType::Thick } else { BorderType::Plain }))
        .highlight_style(Style::default().bg(highlight_color).fg(Color::White).add_modifier(Modifier::BOLD))
        .highlight_symbol(">> ");
    f.render_stateful_widget(pc_list, body_chunks[2], &mut app.selected_playlist_content_state);

    let playing_text = if let Some(s) = &app.playing_song {
        format!("{} - {}", s.metadata.artist, s.metadata.title)
    } else {
        "None".to_string()
    };

    let status_color = if app.is_playing { Color::Green } else { Color::Red };
    let status_symbol = if app.is_playing { "▶" } else { "⏸" };

    let status_line = Line::from(vec![
        Span::raw("Now Playing: "),
        Span::styled(playing_text, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(" | Active: "),
        Span::styled(&app.active_playlist_name, Style::default().fg(Color::Green)),
        Span::raw(" | Loop: "),
        Span::styled(if app.loop_mode { "ON" } else { "OFF" }, Style::default().fg(if app.loop_mode { Color::Yellow } else { Color::DarkGray })),
        Span::raw(" | Shuffle: "),
        Span::styled(if app.shuffle_mode { "ON" } else { "OFF" }, Style::default().fg(if app.shuffle_mode { Color::Yellow } else { Color::DarkGray })),
        Span::raw(" | Volume: "),
        Span::styled(format!("{:.0}%", app.volume * 100.0), Style::default().fg(Color::Yellow)),
        Span::raw(" | Status: "),
        Span::styled(format!("{} {}", status_symbol, if app.is_playing { "Playing" } else { "Paused" }), Style::default().fg(status_color)),
    ]);

    let controls = Line::from(vec![
        Span::styled("[Tab]", Style::default().fg(Color::Magenta)), Span::raw(" Switch Focus | "),
        Span::styled("[Enter]", Style::default().fg(Color::Magenta)), Span::raw(" Play/Select | "),
        Span::styled("[/]", Style::default().fg(Color::Magenta)), Span::raw(" Search | "),
        Span::styled("[c]", Style::default().fg(Color::Magenta)), Span::raw(" New PL | "),
        Span::styled("[r]", Style::default().fg(Color::Magenta)), Span::raw(" Rename PL | "),
        Span::styled("[a]", Style::default().fg(Color::Magenta)), Span::raw(" Add | "),
        Span::styled("[d]", Style::default().fg(Color::Magenta)), Span::raw(" Delete PL")
    ]);
    
    let controls_line2 = Line::from(vec![
        Span::styled("[[]", Style::default().fg(Color::Magenta)), Span::raw(" Move Up | "),
        Span::styled("[]]", Style::default().fg(Color::Magenta)), Span::raw(" Move Down | "),
        Span::styled("[Space]", Style::default().fg(Color::Magenta)), Span::raw(" Play/Pause | "),
        Span::styled("[n/p]", Style::default().fg(Color::Magenta)), Span::raw(" Next/Prev | "),
        Span::styled("[+/-]", Style::default().fg(Color::Magenta)), Span::raw(" Volume | "),
        Span::styled("[q]", Style::default().fg(Color::Magenta)), Span::raw(" Quit")
    ]);
    
    let footer = Paragraph::new(vec![status_line, Line::raw(""), controls, controls_line2])
        .block(Block::default().borders(Borders::ALL).title(" Status & Controls ").border_type(BorderType::Rounded));
    
    f.render_widget(footer, chunks[2]);
}
