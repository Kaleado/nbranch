use crate::paths::Root;
use crate::ticket::{State, Store, Ticket};
use crate::{git, report, util};
use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;
use std::time::{Duration, Instant};

const REFRESH: Duration = Duration::from_millis(500);
const TRUNK_COMMITS: usize = 50;
const RECENT_LANDS: usize = 200;

pub struct CommitRow {
    pub sha: String,
    pub time: u64,
    pub author: String,
    pub agent: Option<String>,
    pub ticket: Option<String>,
    pub subject: String,
}

pub struct Snapshot {
    pub root: String,
    pub trunk: String,
    pub trunk_sha: String,
    pub commits: Vec<CommitRow>,
    /// Unfinished lands: the one being processed, then the queue in order,
    /// then those still in pre-queue review.
    pub active: Vec<Ticket>,
    /// Finished lands, newest first.
    pub recent: Vec<Ticket>,
    pub now: u64,
}

impl Snapshot {
    pub fn load(root: &Root) -> Result<Snapshot> {
        let store = Store::new(root);
        let tickets = store.locked(|| {
            store.reap()?;
            store.all()
        })?;
        let (mut active, mut recent): (Vec<Ticket>, Vec<Ticket>) =
            tickets.into_iter().partition(|t| !t.state.is_terminal());
        active.sort_by_key(|t| match t.state {
            State::Picking | State::Testing => (0, 0),
            State::Queued => (1, t.queue_seq.unwrap_or(0)),
            _ => (2, t.id),
        });
        recent.sort_by_key(|t| std::cmp::Reverse(t.id));
        recent.truncate(RECENT_LANDS);

        let golden = root.golden();
        let trunk = root.trunk()?;
        let trunk_ref = format!("refs/heads/{trunk}");
        let trunk_sha = git::rev_parse(&golden, &trunk_ref)?;
        let log = git::run(
            &golden,
            &[
                "log",
                &format!("-n{TRUNK_COMMITS}"),
                "--format=%h%x1f%ct%x1f%an%x1f%s%x1f%(trailers:key=Nbranch-Agent,valueonly,separator=%x2C)%x1f%(trailers:key=Nbranch-Ticket,valueonly,separator=%x2C)%x1e",
                &trunk_ref,
            ],
        )?;
        let commits = log
            .split('\x1e')
            .map(|r| r.trim_start_matches('\n'))
            .filter(|r| !r.is_empty())
            .map(|r| {
                let f: Vec<&str> = r.split('\x1f').collect();
                let field = |i: usize| f.get(i).map(|s| s.trim()).unwrap_or("");
                let opt = |i: usize| Some(field(i).to_string()).filter(|s| !s.is_empty());
                CommitRow {
                    sha: field(0).into(),
                    time: field(1).parse().unwrap_or(0),
                    author: field(2).into(),
                    subject: field(3).into(),
                    agent: opt(4),
                    ticket: opt(5),
                }
            })
            .collect();
        Ok(Snapshot {
            root: root.dir.display().to_string(),
            trunk,
            trunk_sha,
            commits,
            active,
            recent,
            now: util::now(),
        })
    }
}

pub struct App {
    pub snap: Snapshot,
    pub selected: usize,
    pub scroll: u16,
    pub failures_only: bool,
}

impl App {
    pub fn new(snap: Snapshot) -> App {
        App { snap, selected: 0, scroll: 0, failures_only: false }
    }

    fn visible(&self) -> Vec<&Ticket> {
        self.snap
            .recent
            .iter()
            .filter(|t| !self.failures_only || t.state.is_failure())
            .collect()
    }

    fn clamp(&mut self) {
        let n = self.visible().len();
        self.selected = self.selected.min(n.saturating_sub(1));
    }
}

pub fn run(root: &Root) -> Result<()> {
    let mut app = App::new(Snapshot::load(root)?);
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, root, &mut app);
    ratatui::restore();
    result
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, root: &Root, app: &mut App) -> Result<()> {
    let mut refreshed = Instant::now();
    loop {
        if refreshed.elapsed() >= REFRESH {
            if let Ok(snap) = Snapshot::load(root) {
                app.snap = snap;
                app.clamp();
            }
            refreshed = Instant::now();
        }
        terminal.draw(|f| draw(f, app))?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else { continue };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return Ok(()),
            KeyCode::Down | KeyCode::Char('j') => {
                app.selected += 1;
                app.scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                app.selected = app.selected.saturating_sub(1);
                app.scroll = 0;
            }
            KeyCode::PageDown | KeyCode::Char('J') => app.scroll = app.scroll.saturating_add(10),
            KeyCode::PageUp | KeyCode::Char('K') => app.scroll = app.scroll.saturating_sub(10),
            KeyCode::Char('f') => {
                app.failures_only = !app.failures_only;
                app.selected = 0;
                app.scroll = 0;
            }
            _ => {}
        }
        app.clamp();
    }
}

fn state_style(state: State) -> Style {
    let color = match state {
        State::Landed => Color::Green,
        State::Rejected | State::Conflict | State::Error => Color::Red,
        State::ReviewerError | State::Abandoned => Color::Yellow,
        State::Cancelled => Color::DarkGray,
        State::Picking | State::Testing => Color::Cyan,
        State::Queued | State::Reviewing => Color::Blue,
    };
    Style::default().fg(color)
}

fn block(title: &str) -> Block<'_> {
    Block::default().borders(Borders::ALL).title(Span::styled(
        format!(" {title} "),
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

pub fn draw(f: &mut Frame, app: &App) {
    let snap = &app.snap;
    let [header, body, footer] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
    let [left, right] = Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(body);
    let [queue_area, trunk_area] =
        Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(left);
    let [recent_area, detail_area] =
        Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)]).areas(right);

    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("nbranch ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("{}  ", snap.root)),
            Span::styled(
                format!("{} @ {}", snap.trunk, git::short(&snap.trunk_sha)),
                Style::default().fg(Color::Green),
            ),
        ])),
        header,
    );

    let mut position = 0;
    let queue: Vec<ListItem> = snap
        .active
        .iter()
        .map(|t| {
            let detail = match t.state {
                State::Queued => {
                    position += 1;
                    format!("position {position}")
                }
                _ => t.detail.clone().unwrap_or_default(),
            };
            ListItem::new(Line::from(vec![
                Span::raw(format!("#{:<4} {:<18} ", t.id, t.agent)),
                Span::styled(format!("{:<10}", t.state.as_str()), state_style(t.state)),
                Span::raw(format!(
                    " {:>4}  {detail}",
                    util::age(snap.now.saturating_sub(t.created))
                )),
            ]))
        })
        .collect();
    let queue_title = format!("Queue ({})", snap.active.len());
    if queue.is_empty() {
        f.render_widget(Paragraph::new("  (empty)").block(block(&queue_title)), queue_area);
    } else {
        f.render_widget(List::new(queue).block(block(&queue_title)), queue_area);
    }

    let trunk: Vec<ListItem> = snap
        .commits
        .iter()
        .map(|c| {
            let who = match (&c.ticket, &c.agent) {
                (Some(t), Some(a)) => format!("#{t} {a}"),
                (None, Some(a)) => a.clone(),
                _ => c.author.clone(),
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", c.sha), Style::default().fg(Color::Yellow)),
                Span::raw(format!("{:>4} ", util::age(snap.now.saturating_sub(c.time)))),
                Span::styled(format!("{who:<20} "), Style::default().fg(Color::Cyan)),
                Span::raw(c.subject.clone()),
            ]))
        })
        .collect();
    f.render_widget(List::new(trunk).block(block(&format!("Trunk: {}", snap.trunk))), trunk_area);

    let visible = app.visible();
    let recent: Vec<ListItem> = visible
        .iter()
        .map(|t| {
            let summary = match t.state {
                State::Rejected => {
                    let names: Vec<&str> = t
                        .reviews
                        .iter()
                        .filter(|r| r.verdict == crate::ticket::Verdict::Rejected)
                        .map(|r| r.name.as_str())
                        .collect();
                    format!("by {}", names.join(", "))
                }
                State::Conflict => t
                    .conflict
                    .as_ref()
                    .map(|c| c.files.join(", "))
                    .unwrap_or_default(),
                State::Landed => format!("{} commit(s)", t.landed.len()),
                _ => t.error.clone().unwrap_or_default(),
            };
            ListItem::new(Line::from(vec![
                Span::raw(format!("#{:<4} {:<18} ", t.id, t.agent)),
                Span::styled(format!("{:<14}", t.state.as_str()), state_style(t.state)),
                Span::raw(format!(
                    " {:>4}  {summary}",
                    util::age(snap.now.saturating_sub(t.updated))
                )),
            ]))
        })
        .collect();
    let recent_title = if app.failures_only { "Rejections" } else { "Recent lands" };
    let mut state = ListState::default().with_selected((!visible.is_empty()).then_some(app.selected));
    f.render_stateful_widget(
        List::new(recent)
            .block(block(recent_title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED)),
        recent_area,
        &mut state,
    );

    let detail: Vec<Line> = match visible.get(app.selected) {
        Some(t) => report::ticket_lines(t, Some(200)).into_iter().map(Line::from).collect(),
        None => vec![Line::from("  no finished lands yet")],
    };
    f.render_widget(
        Paragraph::new(detail)
            .block(block("Details and reviewer output"))
            .wrap(Wrap { trim: false })
            .scroll((app.scroll, 0)),
        detail_area,
    );

    f.render_widget(
        Paragraph::new(format!(
            " q quit  ↑/↓ select  PgUp/PgDn scroll details  f rejections only [{}]",
            if app.failures_only { "on" } else { "off" }
        ))
        .style(Style::default().fg(Color::DarkGray)),
        footer,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Stage;
    use crate::ticket::{Comment, ReviewResult, Severity, Verdict};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn ticket(id: u64, agent: &str, state: State) -> Ticket {
        let mut t = Ticket::new(id, agent, &format!("nbranch/{agent}"), "bbbbbbbbbbbb", "aaaaaaaaaaaa", 1);
        t.state = state;
        t
    }

    fn render(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(160, 40)).unwrap();
        terminal.draw(|f| draw(f, app)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn renders_queue_trunk_and_rejection_log() {
        let mut testing = ticket(3, "brisk-otter", State::Testing);
        testing.detail = Some("in-queue: running build".into());
        let mut queued = ticket(4, "calm-heron", State::Queued);
        queued.queue_seq = Some(2);
        let mut rejected = ticket(2, "witty-heron", State::Rejected);
        rejected.reviews.push(ReviewResult {
            name: "lint".into(),
            stage: Stage::PreQueue,
            attempt: 1,
            verdict: Verdict::Rejected,
            exit_code: Some(1),
            duration_ms: 1200,
            reviewer: None,
            comments: vec![Comment {
                file: Some("src/main.rs".into()),
                line: Some(7),
                severity: Severity::Error,
                body: "unused import".into(),
            }],
            error: None,
            stdout: None,
            stderr: None,
        });
        let landed = ticket(1, "brisk-otter", State::Landed);
        let snap = Snapshot {
            root: "/work/root".into(),
            trunk: "master".into(),
            trunk_sha: "0123456789abcdef".into(),
            commits: vec![CommitRow {
                sha: "abc1234".into(),
                time: util::now(),
                author: "Alice".into(),
                agent: Some("brisk-otter".into()),
                ticket: Some("1".into()),
                subject: "Add the frobnicator".into(),
            }],
            active: vec![testing, queued],
            recent: vec![rejected, landed],
            now: util::now(),
        };
        let mut app = App::new(snap);
        let screen = render(&app);
        assert!(screen.contains("master @ 0123456789"), "{screen}");
        assert!(screen.contains("in-queue: running build"), "{screen}");
        assert!(screen.contains("position 1"), "{screen}");
        assert!(screen.contains("#1 brisk-otter"), "{screen}");
        assert!(screen.contains("Add the frobnicator"), "{screen}");
        assert!(screen.contains("by lint"), "{screen}");
        assert!(screen.contains("src/main.rs:7: error: unused import"), "{screen}");

        app.failures_only = true;
        let screen = render(&app);
        assert!(screen.contains("Rejections"), "{screen}");
        assert!(!screen.contains("landed        "), "{screen}");
    }
}
