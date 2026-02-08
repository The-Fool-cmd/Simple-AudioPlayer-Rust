use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line as TextLine, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
};

/// Simple Ratatui UI:
/// - Browse a directory and pick a file.
/// - Space = pause, arrows = seek.
/// - Timeline shows progress.
/// - Layout adapts to terminal size.
pub fn run(start_path: &Path) -> Result<()> {
    // Initialize terminal UI.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // UI/application state.
    let mut player: Option<(crate::player::Player, crate::player::InputSpec)> = None;
    let mut ui_offset_secs: f64 = 0.0;
    let mut last_tick = Instant::now();
    let tick_rate = Duration::from_millis(100);

    // File browser state.
    let mut current_dir = if start_path.is_dir() {
        PathBuf::from(start_path)
    } else {
        start_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf()
    };
    let mut entries = read_directory(&current_dir);
    let mut selected_idx: usize = 0;
    // Persist list state so selection/highlight and scroll offset are kept.
    let mut file_list_state = ratatui::widgets::ListState::default();
    if !entries.is_empty() {
        file_list_state.select(Some(selected_idx));
    }
    // Non-intrusive error message for failed playback attempts.
    let mut error_msg: Option<String> = None;

    // Main loop.
    loop {
        let size = terminal.size()?;

        // Render.
        terminal.draw(|f| {
            let layout = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3), // Header / status
                    Constraint::Min(7),    // Files
                    Constraint::Length(3), // Timeline
                    Constraint::Length(3), // Controls
                ])
                .split(size);

            // Header/status.
            let mut header_text = match &player {
                Some((p, input)) => {
                    let played_samples = p
                        .state
                        .samples_played
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let paused = p.state.paused.load(std::sync::atomic::Ordering::Relaxed);
                    let underruns = p.state.underruns.load(std::sync::atomic::Ordering::Relaxed);

                    let total_samples = p
                        .state
                        .total_samples
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let current_secs =
                        samples_to_seconds(played_samples, input.sample_rate, 2) + ui_offset_secs;
                    let total_secs = if total_samples > 0 {
                        Some(samples_to_seconds(total_samples, input.sample_rate, 2))
                    } else {
                        None
                    };

                    if let Some(ts) = total_secs {
                        format!(
                            "{} / {}  {}ch {}Hz{}  underruns={}",
                            format_mm_ss(current_secs.max(0.0)),
                            format_mm_ss(ts),
                            input.channels,
                            input.sample_rate,
                            if paused { " [Paused]" } else { "" },
                            underruns
                        )
                    } else {
                        format!(
                            "{} / --:--  {}ch {}Hz{}  underruns={}",
                            format_mm_ss(current_secs.max(0.0)),
                            input.channels,
                            input.sample_rate,
                            if paused { " [Paused]" } else { "" },
                            underruns
                        )
                    }
                }
                None => {
                    let canon =
                        std::fs::canonicalize(&current_dir).unwrap_or_else(|_| current_dir.clone());
                    format!("Select a file to play in: {}", canon.display())
                }
            };
            if let Some(err) = &error_msg {
                header_text.push_str(&format!("   Error: {err}"));
            }

            let header = Paragraph::new(TextLine::raw(header_text))
                .block(Block::default().borders(Borders::ALL).title("Status"))
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::White));
            f.render_widget(header, layout[0]);

            // Layout: files + visualizer.
            // Wide: side-by-side. Narrow: stacked.
            let (files_area, viz_area) = if size.width >= 100 {
                let split = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([
                        Constraint::Percentage(60), // files
                        Constraint::Percentage(40), // visualizer
                    ])
                    .split(layout[1]);
                (split[0], split[1])
            } else {
                let split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([
                        Constraint::Min(5),    // files
                        Constraint::Length(3), // visualizer
                    ])
                    .split(layout[1]);
                (split[0], split[1])
            };

            render_file_list(
                f,
                files_area,
                &entries,
                selected_idx,
                &current_dir,
                &mut file_list_state,
            );

            render_visualizer(f, viz_area, &player);

            render_timeline(f, layout[2], &player, ui_offset_secs);

            // Controls footer.
            let controls = Paragraph::new(TextLine::from(vec![
                Span::raw("Enter: Play/Select   "),
                Span::raw("Up/Down: Move   "),
                Span::raw("Left/Right: Seek -/+5s   "),
                Span::raw("Space: Pause   "),
                Span::styled("q: Quit   ", Style::default().add_modifier(Modifier::BOLD)),
            ]))
            .block(Block::default().borders(Borders::ALL).title("Controls"))
            .alignment(Alignment::Left);
            f.render_widget(controls, layout[3]);
        })?;

        // Input handling.
        let timeout = tick_rate
            .checked_sub(last_tick.elapsed())
            .unwrap_or_else(|| Duration::from_millis(0));
        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') => {
                            if let Some((p, _)) = &player {
                                p.state.request_quit();
                            }
                            break;
                        }
                        KeyCode::Up => {
                            if entries.is_empty() { /* noop */
                            } else if selected_idx > 0 {
                                selected_idx -= 1;
                                file_list_state.select(Some(selected_idx));
                            }
                        }
                        KeyCode::Down => {
                            if entries.is_empty() { /* noop */
                            } else if selected_idx + 1 < entries.len() {
                                selected_idx += 1;
                                file_list_state.select(Some(selected_idx));
                            }
                        }
                        KeyCode::Enter => {
                            if !entries.is_empty() {
                                let path = current_dir.join(&entries[selected_idx].name);
                                if entries[selected_idx].is_dir {
                                    current_dir = path;
                                    entries = read_directory(&current_dir);
                                    selected_idx = 0;
                                    if entries.is_empty() {
                                        file_list_state.select(None);
                                    } else {
                                        file_list_state.select(Some(selected_idx));
                                    }
                                } else {
                                    // Start or switch player to new file.
                                    if let Some((p, _)) = &player {
                                        p.state.request_quit();
                                    }
                                    match crate::player::Player::start(&path) {
                                        Ok(newp) => {
                                            player = Some(newp);
                                            ui_offset_secs = 0.0;
                                            error_msg = None;
                                        }
                                        Err(e) => {
                                            // Do not overwrite files list; show error in header/status.
                                            error_msg = Some(e.to_string());
                                            // Keep browsing; player remains None.
                                        }
                                    }
                                }
                            }
                        }
                        KeyCode::Left => {
                            if let Some((p, input)) = &player {
                                // real seek
                                p.seek_seconds(-5.0);
                                ui_offset_secs = (ui_offset_secs - 5.0).max(-samples_to_seconds(
                                    p.state
                                        .samples_played
                                        .load(std::sync::atomic::Ordering::Relaxed),
                                    input.sample_rate,
                                    2,
                                ));
                            }
                        }
                        KeyCode::Right => {
                            if let Some((p, _)) = &player {
                                p.seek_seconds(5.0);
                                ui_offset_secs += 5.0;
                            }
                        }
                        KeyCode::Char(' ') => {
                            if let Some((p, _)) = &player {
                                let now = p.state.paused.load(std::sync::atomic::Ordering::Relaxed);
                                p.state
                                    .paused
                                    .store(!now, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        KeyCode::Char('h') => {
                            // Go to parent dir.
                            if let Some(parent) = current_dir.parent() {
                                current_dir = parent.to_path_buf();
                                entries = read_directory(&current_dir);
                                selected_idx = 0;
                                if entries.is_empty() {
                                    file_list_state.select(None);
                                } else {
                                    file_list_state.select(Some(selected_idx));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        if last_tick.elapsed() >= tick_rate {
            last_tick = Instant::now();
        }

        // If current player requested quit (e.g., file ended), clear player.
        if let Some((p, _)) = &player {
            if p.state.quit.load(std::sync::atomic::Ordering::Relaxed) {
                player = None;
                ui_offset_secs = 0.0;
            }
        }
    }

    // Restore terminal.
    disable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, LeaveAlternateScreen)?;
    Ok(())
}

#[derive(Clone, Debug)]
struct FileEntry {
    name: String,
    is_dir: bool,
}

fn read_directory(dir: &Path) -> Vec<FileEntry> {
    let mut out = Vec::new();

    // Add parent entry if possible.
    if dir.parent().is_some() {
        out.push(FileEntry {
            name: String::from(".."),
            is_dir: true,
        });
    }

    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            // skip hidden entries like . and ..
            if name.starts_with('.') {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            out.push(FileEntry { name, is_dir });
        }
    }
    // Sort dirs first, then files, alphabetically, keeping ".." at top.
    out.sort_by(|a, b| {
        if a.name == ".." {
            return std::cmp::Ordering::Less;
        }
        if b.name == ".." {
            return std::cmp::Ordering::Greater;
        }
        match (a.is_dir, b.is_dir) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        }
    });
    out
}

fn render_file_list(
    f: &mut ratatui::Frame,
    area: Rect,
    entries: &[FileEntry],
    selected_idx: usize,
    dir: &Path,
    state: &mut ratatui::widgets::ListState,
) {
    let items: Vec<ListItem> = entries
        .iter()
        .map(|e| {
            let prefix = if e.is_dir { "[D] " } else { "    " };
            let style = if !e.is_dir && is_supported_audio(&e.name) {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(TextLine::from(vec![
                Span::styled(prefix, Style::default().fg(Color::Blue)),
                Span::styled(&e.name, style),
            ]))
        })
        .collect();

    if items.is_empty() {
        let empty = Paragraph::new(TextLine::raw("Directory is empty"))
            .block(Block::default().borders(Borders::ALL).title({
                let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
                format!("Files: {}", canon.display())
            }))
            .alignment(Alignment::Center);
        f.render_widget(empty, area);
        return;
    }

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title({
            let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
            format!("Files: {}", canon.display())
        }))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("➤ ");

    // Persisted state keeps selection and scroll position.
    state.select(Some(selected_idx));
    f.render_stateful_widget(list, area, state);
}

fn is_supported_audio(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.ends_with(".mp3")
        || lower.ends_with(".mp1")
        || lower.ends_with(".wav")
        || lower.ends_with(".aiff")
        || lower.ends_with(".aif")
        || lower.ends_with(".flac")
        || lower.ends_with(".ogg")
        || lower.ends_with(".opus")
        || lower.ends_with(".m4a")
        || lower.ends_with(".mp4")
        || lower.ends_with(".aac")
        || lower.ends_with(".alac")
}

fn render_timeline(
    f: &mut ratatui::Frame,
    area: Rect,
    player: &Option<(crate::player::Player, crate::player::InputSpec)>,
    ui_offset_secs: f64,
) {
    let (p, input) = match player {
        Some((p, input)) => (p, input),
        None => {
            let placeholder = Paragraph::new(TextLine::raw("No file playing"))
                .block(Block::default().borders(Borders::ALL).title("Timeline"))
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::DarkGray));
            f.render_widget(placeholder, area);
            return;
        }
    };

    let played_samples = p
        .state
        .samples_played
        .load(std::sync::atomic::Ordering::Relaxed);
    let total_samples = p
        .state
        .total_samples
        .load(std::sync::atomic::Ordering::Relaxed);

    let current_secs = samples_to_seconds(played_samples, input.sample_rate, 2) + ui_offset_secs;
    let total_secs = if total_samples > 0 {
        Some(samples_to_seconds(total_samples, input.sample_rate, 2))
    } else {
        None
    };

    let width = area.width.saturating_sub(2) as usize;
    let played_ratio = if let Some(ts) = total_secs {
        (current_secs / ts).clamp(0.0, 1.0)
    } else {
        // fallback: a slow animation to show progress
        (current_secs % 10.0) / 10.0
    };
    let played_len = (played_ratio * width as f64).round() as usize;

    let mut timeline = String::with_capacity(width);
    for i in 0..width {
        if i < played_len {
            timeline.push('█');
        } else {
            timeline.push('-');
        }
    }

    let timeline_para = Paragraph::new(TextLine::raw(timeline))
        .block(Block::default().borders(Borders::ALL).title("Timeline"))
        .alignment(Alignment::Left)
        .style(Style::default().fg(Color::Green));
    f.render_widget(timeline_para, area);
}

fn format_mm_ss(secs: f64) -> String {
    let secs = secs.max(0.0);
    let total = secs.floor() as u64;
    let m = total / 60;
    let s = total % 60;
    format!("{m:02}:{s:02}")
}

// Simple audio visualizer block: shows a horizontal bar with current peak level.
fn render_visualizer(
    f: &mut ratatui::Frame,
    area: Rect,
    player: &Option<(crate::player::Player, crate::player::InputSpec)>,
) {
    // Visualizer orientation:
    // - Stacked: horizontal single-bar peak.
    // - Side-by-side: vertical sliding window bars.
    let level = if let Some((p, _)) = player {
        p.state
            .level_peak
            .load(std::sync::atomic::Ordering::Relaxed)
            .min(1000)
    } else {
        0
    };

    // Heuristic: stacked layout uses a short visualizer height (e.g., 3),
    // so render the horizontal bar only in that case.
    if area.height <= 4 {
        let width = area.width.saturating_sub(2) as usize; // account for borders
        let filled = ((level as f64 / 1000.0) * width as f64).round() as usize;

        let mut bar = String::with_capacity(width);
        for i in 0..width {
            if i < filled {
                bar.push('█');
            } else {
                bar.push('-');
            }
        }

        let viz = Paragraph::new(TextLine::raw(bar))
            .block(Block::default().borders(Borders::ALL).title("Visualizer"))
            .alignment(Alignment::Left)
            .style(Style::default().fg(Color::Cyan));
        f.render_widget(viz, area);
        return;
    }

    // Vertical sliding-window visualizer (side-by-side layout).
    // Maintain a global sliding window of recent peaks.
    static LEVEL_HISTORY: std::sync::OnceLock<std::sync::Mutex<std::collections::VecDeque<u64>>> =
        std::sync::OnceLock::new();
    let history = LEVEL_HISTORY
        .get_or_init(|| std::sync::Mutex::new(std::collections::VecDeque::with_capacity(512)));

    {
        let mut hist = history.lock().unwrap();
        hist.push_back(level);
        // Limit history length to the visualizer width in columns to fit the block.
        let max_cols = area.width.saturating_sub(2) as usize;
        while hist.len() > max_cols {
            hist.pop_front();
        }
    }

    // Build vertical bars: each column is one recent level; height mapped to block height.
    let inner_height = area.height.saturating_sub(2) as usize; // account for borders
    let inner_width = area.width.saturating_sub(2) as usize;

    // Snapshot history to avoid holding the lock during render.
    let hist_snapshot: Vec<u64> = {
        let hist = history.lock().unwrap();
        let v = hist.iter().cloned().collect::<Vec<u64>>();
        // If fewer columns than width, left-pad with zeros so columns align right.
        if v.len() < inner_width {
            let pad = inner_width - v.len();
            let mut padded = Vec::with_capacity(inner_width);
            padded.extend(std::iter::repeat(0).take(pad));
            padded.extend(v);
            padded
        } else {
            v
        }
    };

    // Create multiple lines from top to bottom.
    let mut lines: Vec<TextLine> = Vec::with_capacity(inner_height);
    for row in (0..inner_height).rev() {
        // Row threshold in normalized [0,1]
        let thresh = (row as f64 + 1.0) / (inner_height as f64);
        let mut line = String::with_capacity(inner_width);
        for &lvl in &hist_snapshot {
            let norm = (lvl as f64) / 1000.0;
            if norm >= thresh {
                line.push('█');
            } else {
                line.push(' ');
            }
        }
        lines.push(TextLine::raw(line));
    }

    let viz = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title("Visualizer"))
        .alignment(Alignment::Left)
        .style(Style::default().fg(Color::Cyan));
    f.render_widget(viz, area);
}

fn samples_to_seconds(samples: u64, sample_rate: u32, channels: u16) -> f64 {
    let denom = (sample_rate as f64) * (channels as f64);
    (samples as f64) / denom
}
