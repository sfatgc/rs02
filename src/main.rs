use std::io;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use midir::{MidiInput, MidiOutput};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
    Left,
    Right,
}

#[derive(Clone, Debug)]
enum MidiKind {
    Input,
    Output,
}

#[derive(Clone, Debug)]
struct DeviceItem {
    name: String,
    kind: MidiKind,
    index: usize, // index within its kind (as provided by midir)
}

struct App {
    devices: Vec<DeviceItem>,
    selected: usize,
    focus: Focus,
    last_refresh: Instant,
}

impl App {
    fn new() -> Result<Self> {
        let devices = collect_devices()?;
        Ok(Self {
            devices,
            selected: 0,
            focus: Focus::Left,
            last_refresh: Instant::now(),
        })
    }

    fn refresh_devices(&mut self) {
        if let Ok(devs) = collect_devices() {
            // Try to keep selection on the same device name if possible
            let old_name = self.devices.get(self.selected).map(|d| d.name.clone());
            self.devices = devs;
            if let Some(name) = old_name {
                if let Some(pos) = self.devices.iter().position(|d| d.name == name) {
                    self.selected = pos;
                } else {
                    self.selected = 0;
                }
            } else {
                self.selected = 0;
            }
            self.last_refresh = Instant::now();
        }
    }

    fn select_up(&mut self) {
        if self.devices.is_empty() { return; }
        if self.selected == 0 {
            self.selected = self.devices.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    fn select_down(&mut self) {
        if self.devices.is_empty() { return; }
        self.selected = (self.selected + 1) % self.devices.len();
    }
}

fn collect_devices() -> Result<Vec<DeviceItem>> {
    let inp = MidiInput::new("midir-tui").context("Failed to create MidiInput")?;
    let out = MidiOutput::new("midir-tui").context("Failed to create MidiOutput")?;

    let mut items: Vec<DeviceItem> = Vec::new();

    // Inputs
    for (idx, port) in inp.ports().iter().enumerate() {
        let name = inp.port_name(port).unwrap_or_else(|_| format!("Input #{idx}"));
        items.push(DeviceItem {
            name,
            kind: MidiKind::Input,
            index: idx,
        });
    }

    // Outputs
    for (idx, port) in out.ports().iter().enumerate() {
        let name = out.port_name(port).unwrap_or_else(|_| format!("Output #{idx}"));
        items.push(DeviceItem {
            name,
            kind: MidiKind::Output,
            index: idx,
        });
    }

    // Sort by kind then name for a stable, readable list
    items.sort_by(|a, b| match ( &a.kind, &b.kind ) {
        (MidiKind::Input, MidiKind::Output) => std::cmp::Ordering::Less,
        (MidiKind::Output, MidiKind::Input) => std::cmp::Ordering::Greater,
        _ => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
    });

    Ok(items)
}

fn main() -> Result<()> {
    // Setup terminal
    enable_raw_mode().context("enable_raw_mode failed")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).context("EnterAlternateScreen failed")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("Create terminal failed")?;
    terminal.clear()?;

    let res = run_app(&mut terminal);

    // Restore terminal
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).ok();
    terminal.show_cursor().ok();

    res
}

fn run_app(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let mut app = App::new()?;

    // Ticker/polling
    let tick = Duration::from_millis(100);

    // Optional: auto-refresh device list every few seconds (hotplug-ish).
    let refresh_every = Duration::from_secs(5);

    let mut list_state = ListState::default();
    list_state.select(Some(app.selected));

    loop {
        // Auto refresh
        if app.last_refresh.elapsed() >= refresh_every {
            app.refresh_devices();
            // Keep the list state aligned with app.selected
            list_state.select(Some(app.selected));
        }

        terminal.draw(|f| {
            let size = f.size();

            // Outer layout: horizontal split
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(45), Constraint::Percentage(55)].as_ref())
                .split(size);

            // LEFT: device list
            let items: Vec<ListItem> = app
                .devices
                .iter()
                .map(|d| {
                    let kind_tag = match d.kind {
                        MidiKind::Input => "[IN] ",
                        MidiKind::Output => "[OUT]",
                    };
                    ListItem::new(Line::from(vec![
                        Span::styled(kind_tag, Style::default().fg(Color::Yellow)),
                        Span::raw(" "),
                        Span::raw(&d.name),
                    ]))
                })
                .collect();

            // Focus/selection styles
            let (left_border_color, right_border_color) = match app.focus {
                Focus::Left => (Color::Cyan, Color::DarkGray),
                Focus::Right => (Color::DarkGray, Color::Cyan),
            };

            let left_block = Block::default()
                .title(" MIDI Devices ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(left_border_color));

            let list = List::new(items)
                .block(left_block)
                .highlight_style(
                    Style::default()
                        .bg(Color::Blue)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ");

            f.render_stateful_widget(list, chunks[0], &mut list_state);

            // RIGHT: details of selected device
            let right_block = Block::default()
                .title(" Details ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(right_border_color));

            let detail_area = right_block.inner(chunks[1]);
            f.render_widget(right_block, chunks[1]);

            let detail_text = if let Some(dev) = app.devices.get(app.selected) {
                let kind = match dev.kind {
                    MidiKind::Input => "Input",
                    MidiKind::Output => "Output",
                };
                // midir doesn’t expose manufacturer/product IDs; we show what we can.
                vec![
                    Line::from(Span::styled("Selected Device", Style::default().add_modifier(Modifier::BOLD))),
                    Line::from(""),
                    Line::from(vec![Span::styled("Name: ", Style::default().fg(Color::Yellow)), Span::raw(&dev.name)]),
                    Line::from(vec![Span::styled("Kind: ", Style::default().fg(Color::Yellow)), Span::raw(kind)]),
                    Line::from(vec![Span::styled("Index: ", Style::default().fg(Color::Yellow)), Span::raw(dev.index.to_string())]),
                    Line::from(""),
                    Line::from("Notes:"),
                    Line::from("• This info comes from midir’s port names."),
                    Line::from("• Connect/open ports in your app logic if needed."),
                ]
            } else {
                vec![
                    Line::from("No devices detected."),
                    Line::from("Press r to refresh."),
                ]
            };

            let details = Paragraph::new(detail_text)
                .wrap(Wrap { trim: true });

            // Add a little inner margin
            let detail_inner = detail_area.inner(&Margin { horizontal: 1, vertical: 1 });
            f.render_widget(details, detail_inner);

            // FOOTER / help bar
            let help = Paragraph::new(Line::from(vec![
                Span::styled("Keys: ", Style::default().fg(Color::Yellow)),
                Span::raw("↑/↓ select  "),
                Span::raw("←/→ focus  "),
                Span::raw("r refresh  "),
                Span::raw("q/Esc quit"),
            ]))
            .block(Block::default().borders(Borders::TOP));

            let footer_rect = Rect {
                x: size.x,
                y: size.y + size.height.saturating_sub(1),
                width: size.width,
                height: 1,
            };
            f.render_widget(help, footer_rect);
        })?;

        // Input handling
        if event::poll(tick)? {
            if let Event::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('r') => {
                        app.refresh_devices();
                        list_state.select(Some(app.selected));
                    }
                    KeyCode::Left => app.focus = Focus::Left,
                    KeyCode::Right => app.focus = Focus::Right,
                    KeyCode::Up => {
                        if app.focus == Focus::Left {
                            app.select_up();
                            list_state.select(Some(app.selected));
                        }
                    }
                    KeyCode::Down => {
                        if app.focus == Focus::Left {
                            app.select_down();
                            list_state.select(Some(app.selected));
                        }
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    _ => {}
                }
            }
        }
    }

    Ok(())
}
