use std::{
    collections::{HashMap, VecDeque},
    fs,
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use directories::ProjectDirs;
use midir::{MidiInput, MidiInputConnection, MidiOutput, MidiOutputConnection};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Focus {
    Left,
    Right,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
enum MidiKind {
    Input,
    Output,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Hash)]
struct DeviceKey {
    name: String,
    kind: MidiKind,
}

#[derive(Clone, Debug)]
struct DeviceItem {
    key: DeviceKey,
    index: usize, // index within its kind (as provided by midir at collection time)
}

#[derive(Default, Serialize, Deserialize)]
struct Persisted {
    last_device: Option<DeviceKey>,
    last_focus: Option<Focus>,
}

struct App {
    devices: Vec<DeviceItem>,
    selected: usize,
    focus: Focus,
    last_refresh: Instant,

    // Persistence
    persist_path: Option<PathBuf>,

    // Multiple open connections, keyed by device
    in_conns: HashMap<DeviceKey, MidiInputConnection<()>>,
    out_conns: HashMap<DeviceKey, MidiOutputConnection>,

    // Live log (for input devices)
    log: VecDeque<String>,
    tx: Sender<String>,
    rx: Receiver<String>,
}

impl App {
    fn new() -> Result<Self> {
        let persist_path = persist_file_path();
        let persisted = load_persisted(&persist_path).unwrap_or_default();

        let devices = collect_devices()?;
        let (tx, rx) = mpsc::channel::<String>();

        // Restore selection by last_device if possible
        let mut selected = 0usize;
        if let Some(ref key) = persisted.last_device {
            if let Some(pos) = devices.iter().position(|d| &d.key == key) {
                selected = pos;
            }
        }

        Ok(Self {
            devices,
            selected,
            focus: persisted.last_focus.unwrap_or(Focus::Left),
            last_refresh: Instant::now(),
            persist_path,
            in_conns: HashMap::new(),
            out_conns: HashMap::new(),
            log: VecDeque::with_capacity(1024),
            tx,
            rx,
        })
    }

    fn refresh_devices(&mut self) {
        if let Ok(devs) = collect_devices() {
            let old_key = self.devices.get(self.selected).map(|d| d.key.clone());
            self.devices = devs;
            if let Some(key) = old_key {
                if let Some(pos) = self.devices.iter().position(|d| d.key == key) {
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
        if self.devices.is_empty() {
            return;
        }
        if self.selected == 0 {
            self.selected = self.devices.len() - 1;
        } else {
            self.selected -= 1;
        }
    }

    fn select_down(&mut self) {
        if self.devices.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.devices.len();
    }

    fn toggle_open_selected(&mut self) -> Result<()> {
        if self.devices.is_empty() {
            return Ok(());
        }
        let dev = self.devices[self.selected].clone();

        match dev.key.kind {
            MidiKind::Input => {
                if self.in_conns.remove(&dev.key).is_some() {
                    self.push_status(format!("Closed input: {}", dev.key.name));
                } else {
                    self.open_input(&dev)?;
                }
            }
            MidiKind::Output => {
                if self.out_conns.remove(&dev.key).is_some() {
                    self.push_status(format!("Closed output: {}", dev.key.name));
                } else {
                    self.open_output(&dev)?;
                }
            }
        }
        Ok(())
    }

    fn close_all(&mut self) {
        let in_count = self.in_conns.len();
        let out_count = self.out_conns.len();
        self.in_conns.clear();  // drop closes
        self.out_conns.clear(); // drop closes
        self.push_status(format!("Closed all ports (inputs: {in_count}, outputs: {out_count})"));
    }

    fn open_input(&mut self, dev: &DeviceItem) -> Result<()> {
        let mut inp = MidiInput::new("midir-tui-input").context("create MidiInput failed")?;
        inp.ignore(midir::Ignore::None);

        let ports = inp.ports();
        let port = ports.get(dev.index).context("input port index out of range")?;
        let port_name = inp
            .port_name(port)
            .unwrap_or_else(|_| format!("Input #{}", dev.index));

        let name_for_log = dev.key.name.clone();
        let tx = self.tx.clone();
        let conn = inp
            .connect(
                port,
                "midir-tui-in",
                move |_stamp, message, _| {
                    let s = format!("IN  {:02X?}  (len {})  [{}]", message, message.len(), name_for_log);
                    let _ = tx.send(s);
                },
                (),
            )
            .with_context(|| format!("Failed to open input: {port_name}"))?;

        self.in_conns.insert(dev.key.clone(), conn);
        self.push_status(format!("Opened input: {}", port_name));
        Ok(())
    }

    fn open_output(&mut self, dev: &DeviceItem) -> Result<()> {
        let out = MidiOutput::new("midir-tui-output").context("create MidiOutput failed")?;
        let ports = out.ports();
        let port = ports.get(dev.index).context("output port index out of range")?;
        let port_name = out
            .port_name(port)
            .unwrap_or_else(|_| format!("Output #{}", dev.index));

        let conn = out
            .connect(port, "midir-tui-out")
            .with_context(|| format!("Failed to open output: {port_name}"))?;

        self.out_conns.insert(dev.key.clone(), conn);
        self.push_status(format!("Opened output: {}", port_name));
        Ok(())
    }

    fn push_status(&mut self, msg: String) {
        if self.log.len() == self.log.capacity() {
            self.log.pop_front();
        }
        self.log.push_back(format!("· {}", msg));
    }

    fn drain_rx(&mut self) {
        while let Ok(s) = self.rx.try_recv() {
            if self.log.len() == self.log.capacity() {
                self.log.pop_front();
            }
            self.log.push_back(s);
        }
    }

    fn save_persisted(&self) {
        if let Some(path) = &self.persist_path {
            let key = self.devices.get(self.selected).map(|d| d.key.clone());
            let p = Persisted {
                last_device: key,
                last_focus: Some(self.focus),
            };
            if let Some(parent) = path.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = fs::write(path, serde_json::to_vec_pretty(&p).unwrap_or_default());
        }
    }
}

fn collect_devices() -> Result<Vec<DeviceItem>> {
    let inp = MidiInput::new("midir-tui").context("Failed to create MidiInput")?;
    let out = MidiOutput::new("midir-tui").context("Failed to create MidiOutput")?;

    let mut items: Vec<DeviceItem> = Vec::new();

    // Inputs
    for (idx, port) in inp.ports().iter().enumerate() {
        let name = inp
            .port_name(port)
            .unwrap_or_else(|_| format!("Input #{idx}"));
        items.push(DeviceItem {
            key: DeviceKey {
                name,
                kind: MidiKind::Input,
            },
            index: idx,
        });
    }

    // Outputs
    for (idx, port) in out.ports().iter().enumerate() {
        let name = out
            .port_name(port)
            .unwrap_or_else(|_| format!("Output #{idx}"));
        items.push(DeviceItem {
            key: DeviceKey {
                name,
                kind: MidiKind::Output,
            },
            index: idx,
        });
    }

    // Sort by kind then name
    items.sort_by(|a, b| match (&a.key.kind, &b.key.kind) {
        (MidiKind::Input, MidiKind::Output) => std::cmp::Ordering::Less,
        (MidiKind::Output, MidiKind::Input) => std::cmp::Ordering::Greater,
        _ => a.key.name.to_lowercase().cmp(&b.key.name.to_lowercase()),
    });

    Ok(items)
}

fn persist_file_path() -> Option<PathBuf> {
    ProjectDirs::from("dev", "example", "midir-tui").map(|pd| {
        let mut p = pd.config_dir().to_path_buf();
        p.push("state.json");
        p
    })
}

fn load_persisted(path: &Option<PathBuf>) -> Option<Persisted> {
    let p = path.as_ref()?;
    let bytes = fs::read(p).ok()?;
    serde_json::from_slice::<Persisted>(&bytes).ok()
}

fn main() -> Result<()> {
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

    let tick = Duration::from_millis(100);
    let refresh_every = Duration::from_secs(5);

    let mut list_state = ListState::default();
    list_state.select(Some(app.selected));

    let exit_result = loop {
        // Drain incoming MIDI messages to log
        app.drain_rx();

        // Auto refresh (hotplug-ish)
        if app.last_refresh.elapsed() >= refresh_every {
            app.refresh_devices();
            list_state.select(Some(app.selected));
        }

        terminal.draw(|f| {
            let size = f.size();
            let chunks = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(45), Constraint::Percentage(55)].as_ref())
                .split(size);

            // LEFT: list with OPEN marks
            let items: Vec<ListItem> = app
                .devices
                .iter()
                .map(|d| {
                    let kind_tag = match d.key.kind {
                        MidiKind::Input => "[IN] ",
                        MidiKind::Output => "[OUT]",
                    };
                    let mut spans = vec![
                        Span::styled(kind_tag, Style::default().fg(Color::Yellow)),
                        Span::raw(" "),
                        Span::raw(&d.key.name),
                    ];
                    let is_open = match d.key.kind {
                        MidiKind::Input => app.in_conns.contains_key(&d.key),
                        MidiKind::Output => app.out_conns.contains_key(&d.key),
                    };
                    if is_open {
                        spans.push(Span::raw(" "));
                        spans.push(Span::styled(
                            "●OPEN",
                            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                        ));
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();

            let (left_border_color, right_border_color) = match app.focus {
                Focus::Left => (Color::Cyan, Color::DarkGray),
                Focus::Right => (Color::DarkGray, Color::Cyan),
            };

            let left_block = Block::default()
                .title(format!(
                    " MIDI Devices  (open: in {}, out {}) ",
                    app.in_conns.len(),
                    app.out_conns.len()
                ))
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

            // RIGHT: details + recent MIDI
            let right_block = Block::default()
                .title(" Details ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(right_border_color));

            let detail_area = right_block.inner(chunks[1]);
            f.render_widget(right_block, chunks[1]);

            let mut lines: Vec<Line> = vec![];

            if let Some(dev) = app.devices.get(app.selected) {
                let kind_str = match dev.key.kind {
                    MidiKind::Input => "Input",
                    MidiKind::Output => "Output",
                };
                let is_open = match dev.key.kind {
                    MidiKind::Input => app.in_conns.contains_key(&dev.key),
                    MidiKind::Output => app.out_conns.contains_key(&dev.key),
                };
                let open_str = if is_open { "OPEN" } else { "CLOSED" };

                lines.extend([
                    Line::from(Span::styled(
                        "Selected Device",
                        Style::default().add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("Name: ", Style::default().fg(Color::Yellow)),
                        Span::raw(&dev.key.name),
                    ]),
                    Line::from(vec![
                        Span::styled("Kind: ", Style::default().fg(Color::Yellow)),
                        Span::raw(kind_str),
                    ]),
                    Line::from(vec![
                        Span::styled("Index: ", Style::default().fg(Color::Yellow)),
                        Span::raw(dev.index.to_string()),
                    ]),
                    Line::from(vec![
                        Span::styled("Status: ", Style::default().fg(Color::Yellow)),
                        Span::styled(
                            open_str,
                            if is_open {
                                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::Red)
                            },
                        ),
                    ]),
                    Line::from(""),
                ]);

                if dev.key.kind == MidiKind::Input {
                    lines.push(Line::from(Span::styled(
                        "Recent MIDI (latest first):",
                        Style::default().add_modifier(Modifier::BOLD),
                    )));
                    lines.push(Line::from(""));
                    for s in app.log.iter().rev().take(15) {
                        lines.push(Line::from(s.clone()));
                    }
                } else {
                    lines.extend([
                        Line::from("This is an OUTPUT device."),
                        Line::from("Press Enter to open/close this port."),
                        Line::from("Shift+C closes all open ports."),
                    ]);
                }
            } else {
                lines.extend([
                    Line::from("No devices detected."),
                    Line::from("Press r to refresh."),
                ]);
            }

            let details = Paragraph::new(lines).wrap(Wrap { trim: true });
            let detail_inner = detail_area.inner(&Margin {
                horizontal: 1,
                vertical: 1,
            });
            f.render_widget(details, detail_inner);

            // FOOTER
            let help = Paragraph::new(Line::from(vec![
                Span::styled("Keys: ", Style::default().fg(Color::Yellow)),
                Span::raw("↑/↓ select  "),
                Span::raw("←/→ focus  "),
                Span::raw("Enter open/close  "),
                Span::raw("Shift+C close-all  "),
                Span::raw("r refresh  "),
                Span::raw("q/Esc quit"),
            ]))
            .block(Block::default().borders(Borders::TOP));

            let size = f.size();
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
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                    KeyCode::Char('r') => {
                        app.refresh_devices();
                        list_state.select(Some(app.selected));
                    }
                    KeyCode::Left => app.focus = Focus::Left,
                    KeyCode::Right => app.focus = Focus::Right,
                    KeyCode::Enter => {
                        if app.focus == Focus::Left {
                            if let Err(e) = app.toggle_open_selected() {
                                app.push_status(format!("Error: {e:#}"));
                            }
                        }
                    }
                    KeyCode::Char('C') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                        app.close_all();
                    }
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
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break Ok(()),
                    _ => {}
                }
            }
        }
    };

    // Persist before exit
    let _ = exit_result.as_ref();
    let mut app_for_persist = app;
    app_for_persist.save_persisted();
    exit_result
}
