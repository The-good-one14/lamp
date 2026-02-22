# Lightweight Audio Manager Panel (lamp)

## Project Overview
Lamp is a Rust-based Terminal User Interface (TUI) music player. It features metadata extraction from MP3 files, automatic album-based playlist creation, and comprehensive playlist management including renaming and reordering. It is fully compatible with external MPRIS controllers like `playerctl`.

### Key Technologies
- **Rust (2024 Edition):** Core language.
- **Ratatui:** Framework for the terminal user interface.
- **Rodio:** Audio playback and management.
- **ID3:** Metadata extraction for MP3 tags.
- **Serde & Serde_json:** Playlist persistence.
- **MPRIS (mpris-server & zbus):** System-wide media control.
- **Tokio:** Asynchronous runtime for MPRIS integration.

### Architecture
- **`App` Struct:** Orchestrates application state (Library, Playlists, Content).
- **Metadata Strategy:** 
  1.  Reads internal ID3 tags.
  2.  Fallbacks to directory structure (e.g., `Artist/Album/Song.mp3`).
  3.  Defaults to "N/A" for missing data.
- **Auto-Discovery:** On startup, automatically creates playlists for newly discovered albums.
- **Three-Pane UI:** Separates Playlists, Library, and the specific Content of selected playlists for high visibility.

## Building and Running
The project uses standard Cargo commands:

- **Build (Release):** `cargo build --release`
- **Run:** `cargo run -- [music_directory]`
- **Check:** `cargo check`

## Keyboard Shortcuts
- **Navigation:**
  - `Tab`: Cycle focus between **Playlists**, **Library**, and **Playlist Content**.
  - `j` / `Down`: Move selection down.
  - `k` / `Up`: Move selection up.
  - `Enter`: 
    - In **Playlists**: Activate the selected playlist.
    - In **Library**: Play selected song.
    - In **Playlist Content**: Play song from that specific playlist.
- **Management:**
  - `c`: Create a new named playlist.
  - `r`: Rename the selected playlist (in Playlists pane).
  - `d`: Delete the selected playlist (in Playlists pane).
  - `a`: Add selected library song to selected playlist.
  - `[`: Move song UP within a playlist (in Playlist Content pane).
  - `]`: Move song DOWN within a playlist (in Playlist Content pane).
- **Playback:**
  - `Space`: Toggle Play/Pause.
  - `n`: Next track.
  - `p`: Previous track.
  - `l`: Toggle Loop.
  - `s`: Toggle Shuffle.
  - `q`: Quit.

## Key Files
- `src/main.rs`: Core application logic and MPRIS server.
- `Cargo.toml`: Dependency management.
- `playlists.json`: Persistent user data.
