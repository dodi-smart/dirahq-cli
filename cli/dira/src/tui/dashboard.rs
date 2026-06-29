//! Pure layout/derivation helpers + the ratatui render pass for the `dira
//! watch` dashboard. Everything that turns a [`StatusView`] into displayable
//! numbers lives here (and is unit-tested) so the draw code stays dumb.

use crate::format::{bar, hms, project_label, repo_short, truncate};
use dira_core::protocol::{SessionView, StatusView};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

/// What the connection to the daemon currently looks like.
pub enum Conn<'a> {
    /// We have a fresh `Status` from the daemon.
    Up(&'a StatusView),
    /// The daemon could not be reached on the last poll. We keep retrying.
    Down(&'a str),
}

/// The de-duped, dashboard-ready headline numbers for "today".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Headline {
    pub human_seconds: i64,
    pub agent_seconds: i64,
    pub sync_pending: u64,
}

impl Headline {
    pub fn from_status(s: &StatusView) -> Self {
        Self {
            // `today` is already de-duplicated across concurrent sessions by the
            // accounting layer, so we just read its totals.
            human_seconds: s.today.total_human_seconds,
            agent_seconds: s.today.total_agent_seconds,
            sync_pending: s.sync_pending,
        }
    }

    /// Parallel multiplier = agent wall-clock / deduped human time. Mirrors the
    /// status renderer's semantics: with no human time (`eng == 0`) there is no
    /// meaningful multiplier, so we return `None` rather than dividing by zero.
    pub fn multiplier(&self) -> Option<f64> {
        if self.human_seconds <= 0 {
            None
        } else {
            Some(self.agent_seconds as f64 / self.human_seconds as f64)
        }
    }

    pub fn multiplier_label(&self) -> String {
        match self.multiplier() {
            Some(m) => format!("{m:.1}x parallel"),
            None => "—x parallel".to_string(),
        }
    }
}

/// One lane in the parallel view: a label, its seconds, and whether it is the
/// deduped human baseline (rendered distinctly).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub label: String,
    pub seconds: i64,
    pub is_human: bool,
}

/// Build the parallel lanes: one per active *engaged* agent session plus the
/// single deduped human baseline lane. Idle/zero agent sessions are dropped so
/// the chart shows actual concurrency. Lanes are returned human-first.
pub fn lanes(s: &StatusView) -> Vec<Lane> {
    let mut lanes = vec![Lane {
        label: "human (deduped)".to_string(),
        seconds: s.today.total_human_seconds,
        is_human: true,
    }];
    for sess in &s.active {
        // An agent lane is only interesting once it has accrued wall time.
        if sess.kind == "manual" || sess.agent_seconds <= 0 {
            continue;
        }
        lanes.push(Lane {
            label: format!("{} · {}", sess.handle, repo_short(&sess.project)),
            seconds: sess.agent_seconds,
            is_human: false,
        });
    }
    lanes
}

/// Parse an RFC3339 timestamp from the daemon; `None` if absent/unparseable.
fn parse_ts(s: Option<&str>) -> Option<time::OffsetDateTime> {
    time::OffsetDateTime::parse(s?, &time::format_description::well_known::Rfc3339).ok()
}

/// Seconds elapsed from `ts` to `now`, clamped to `[0, idle]`. The clamp matches
/// the accounting idle-trim: once a session has been quiet longer than the idle
/// threshold its live tail stops growing (the daemon will report it idle, and the
/// open gap would not count toward active time anyway).
fn live_tail(now: time::OffsetDateTime, ts: Option<time::OffsetDateTime>, idle: i64) -> i64 {
    match ts {
        Some(t) => (now - t).whole_seconds().clamp(0, idle),
        None => 0,
    }
}

/// Grow a snapshot's engaged-session timers by their live tail — the time elapsed
/// since each session's last activity, clamped to the idle window — so the
/// dashboard ticks smoothly between (and during) polls. `now` advances every
/// render frame, so the displayed seconds climb monotonically and reconcile to
/// the daemon's settled values whenever a fresh snapshot arrives.
///
/// This is display-only: the daemon's `human_seconds`/`agent_seconds` stay the
/// settled snapshot values; we add the open tail here. Idle sessions are frozen.
/// The "today" totals + per-project rollup are advanced consistently so the rows
/// still sum to the total: agent grows by the sum of per-session agent tails;
/// the de-duplicated human total grows by the single largest (most-recent) human
/// tail, attributed to that session's project.
pub fn tick(s: &StatusView, now: time::OffsetDateTime, idle: i64) -> StatusView {
    let mut s = s.clone();
    let mut agent_inc: std::collections::HashMap<Option<String>, i64> =
        std::collections::HashMap::new();
    // The most-recent human signal across engaged sessions → the deduped tail.
    let mut human_tail = 0i64;
    let mut human_project: Option<Option<String>> = None;

    for sess in &mut s.active {
        if sess.idle {
            continue;
        }
        let h = live_tail(now, parse_ts(sess.last_human_at.as_deref()), idle);
        sess.human_seconds += h;
        if h > human_tail || human_project.is_none() {
            human_tail = h;
            human_project = Some(sess.project.clone());
        }
        if sess.kind != "manual" {
            let a = live_tail(now, parse_ts(sess.last_activity_at.as_deref()), idle);
            sess.agent_seconds += a;
            *agent_inc.entry(sess.project.clone()).or_insert(0) += a;
        }
    }

    s.today.total_agent_seconds += agent_inc.values().sum::<i64>();
    for p in &mut s.today.projects {
        if let Some(inc) = agent_inc.get(&p.project) {
            p.agent_wall_seconds += inc;
        }
    }
    if human_tail > 0 {
        s.today.total_human_seconds += human_tail;
        if let Some(proj) = human_project {
            if let Some(p) = s.today.projects.iter_mut().find(|p| p.project == proj) {
                p.human_seconds += human_tail;
            }
        }
    }
    s
}

/// The colour for a session row / lane based on engagement.
fn engaged_style(idle: bool) -> Style {
    if idle {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Green)
    }
}

/// Top-level draw entry point.
pub fn draw(frame: &mut Frame, conn: &Conn) {
    let area = frame.area();
    let chunks = Layout::new(
        Direction::Vertical,
        [
            Constraint::Length(3), // header
            Constraint::Min(6),    // body (sessions + lanes + rollup)
            Constraint::Length(1), // footer
        ],
    )
    .split(area);

    match conn {
        Conn::Up(s) => {
            draw_header(frame, chunks[0], s);
            draw_body(frame, chunks[1], s);
        }
        Conn::Down(err) => {
            draw_header_down(frame, chunks[0]);
            draw_down_body(frame, chunks[1], err);
        }
    }
    draw_footer(frame, chunks[2]);
}

fn header_block() -> Block<'static> {
    Block::default().borders(Borders::ALL).title(Span::styled(
        " Dira · Right Now ",
        Style::default().add_modifier(Modifier::BOLD),
    ))
}

fn draw_header(frame: &mut Frame, area: Rect, s: &StatusView) {
    let h = Headline::from_status(s);
    let mut spans = vec![
        Span::raw("today  "),
        Span::styled(
            format!("human {}", hms(h.human_seconds)),
            Style::default().fg(Color::Cyan),
        ),
        Span::raw("   "),
        Span::styled(
            format!("agent {}", hms(h.agent_seconds)),
            Style::default().fg(Color::Magenta),
        ),
        Span::raw("   "),
        Span::styled(
            h.multiplier_label(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ];
    if h.sync_pending > 0 {
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!("⇡ {} pending sync", h.sync_pending),
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)).block(header_block()),
        area,
    );
}

fn draw_header_down(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "daemon not running — retrying…",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )))
        .block(header_block()),
        area,
    );
}

fn draw_body(frame: &mut Frame, area: Rect, s: &StatusView) {
    // Size each section to its content and let any leftover space fall to the
    // bottom (the trailing `Min(0)`), so a couple of sessions on a tall screen
    // don't leave a giant hole in the middle. lanes + rollup are kept visible;
    // a very long session list is what gets capped + clipped on small screens.
    let lanes_h = (lanes(s).len() as u16 + 2).max(3); // rows + borders
    let rollup_h = if s.today.projects.is_empty() {
        3
    } else {
        s.today.projects.len() as u16 + 4 // projects + header + total + borders
    };
    let sessions_need = s.active.len().max(1) as u16 + 3; // rows + header + borders
    let sessions_cap = area.height.saturating_sub(lanes_h + rollup_h).max(3);
    let sessions_h = sessions_need.min(sessions_cap).max(3);

    let chunks = Layout::new(
        Direction::Vertical,
        [
            Constraint::Length(sessions_h), // active sessions table (content-sized)
            Constraint::Length(lanes_h),    // parallel lanes
            Constraint::Length(rollup_h),   // per-project rollup
            Constraint::Min(0),             // slack falls to the bottom
        ],
    )
    .split(area);
    draw_sessions(frame, chunks[0], &s.active);
    draw_lanes(frame, chunks[1], s);
    draw_rollup(frame, chunks[2], s);
}

fn draw_sessions(frame: &mut Frame, area: Rect, active: &[SessionView]) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Active sessions ");
    if active.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "no active sessions",
                Style::default().fg(Color::DarkGray),
            ))
            .block(block),
            area,
        );
        return;
    }
    let header = Row::new([
        Cell::from("HANDLE"),
        Cell::from("HARNESS"),
        Cell::from("PROJECT"),
        Cell::from("HUMAN"),
        Cell::from("AGENT"),
        Cell::from("STATE"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    // The PROJECT column flexes to fill whatever's left after the fixed columns
    // (handle 11 + harness 11 + human 9 + agent 9 + state 8 = 48), the 5 inter-
    // column gaps, and 2 borders — so wide terminals show the full repo path.
    let project_w = (area.width as usize)
        .saturating_sub(48 + 5 + 2)
        .clamp(12, 200);
    let rows = active.iter().map(|sess| {
        let style = engaged_style(sess.idle);
        Row::new([
            Cell::from(truncate(&sess.handle, 10)),
            Cell::from(truncate(&sess.harness, 10)),
            Cell::from(truncate(&project_label(&sess.project), project_w)),
            Cell::from(hms(sess.human_seconds)),
            Cell::from(hms(sess.agent_seconds)),
            Cell::from(if sess.idle { "idle" } else { "engaged" }),
        ])
        .style(style)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Length(11),
            Constraint::Length(11),
            Constraint::Min(12),
            Constraint::Length(9),
            Constraint::Length(9),
            Constraint::Length(8),
        ],
    )
    .header(header)
    .block(block);
    frame.render_widget(table, area);
}

fn draw_lanes(frame: &mut Frame, area: Rect, s: &StatusView) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Parallel lanes ");
    let lanes = lanes(s);
    let max = lanes.iter().map(|l| l.seconds).max().unwrap_or(0);
    // Reserve room for label + duration; the remaining inner width is the bar.
    let inner_w = area.width.saturating_sub(2) as usize;
    let label_w = 22usize.min(inner_w.saturating_sub(12));
    let bar_w = inner_w.saturating_sub(label_w + 12);

    let lines: Vec<Line> = lanes
        .iter()
        .map(|l| {
            let color = if l.is_human {
                Color::Cyan
            } else {
                Color::Green
            };
            Line::from(vec![
                Span::styled(
                    format!("{:<width$}", truncate(&l.label, label_w), width = label_w),
                    Style::default().fg(color),
                ),
                Span::raw(" "),
                Span::styled(bar(l.seconds, max, bar_w), Style::default().fg(color)),
                Span::raw(format!(" {:>9}", hms(l.seconds))),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_rollup(frame: &mut Frame, area: Rect, s: &StatusView) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Today by project ");
    if s.today.projects.is_empty() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "no time tracked",
                Style::default().fg(Color::DarkGray),
            ))
            .block(block),
            area,
        );
        return;
    }
    let header = Row::new([
        Cell::from("PROJECT"),
        Cell::from("HUMAN"),
        Cell::from("AGENT"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    // PROJECT flexes to fill the width left after HUMAN (11) + AGENT (11), the 2
    // inter-column gaps and 2 borders.
    let project_w = (area.width as usize)
        .saturating_sub(22 + 2 + 2)
        .clamp(12, 200);
    let mut rows: Vec<Row> = s
        .today
        .projects
        .iter()
        .map(|p| {
            Row::new([
                Cell::from(truncate(&project_label(&p.project), project_w)),
                Cell::from(hms(p.human_seconds)),
                Cell::from(hms(p.agent_wall_seconds)),
            ])
        })
        .collect();
    rows.push(
        Row::new([
            Cell::from("— total —"),
            Cell::from(hms(s.today.total_human_seconds)),
            Cell::from(hms(s.today.total_agent_seconds)),
        ])
        .style(Style::default().add_modifier(Modifier::BOLD)),
    );
    let table = Table::new(
        rows,
        [
            Constraint::Min(12),
            Constraint::Length(11),
            Constraint::Length(11),
        ],
    )
    .header(header)
    .block(block);
    frame.render_widget(table, area);
}

fn draw_down_body(frame: &mut Frame, area: Rect, err: &str) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Active sessions ");
    let lines = vec![
        Line::from(Span::styled(
            "Can't reach the daemon.",
            Style::default().fg(Color::Red),
        )),
        Line::from(Span::raw(
            "Start it with `dira daemon start`. Polling continues.",
        )),
        Line::from(Span::styled(
            truncate(err, 80),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_footer(frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("q", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw("/"),
            Span::styled("Esc", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" quit   "),
            Span::styled("Ctrl-C", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" quit   live"),
        ]))
        .style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use dira_core::report::{ProjectReport, Report};

    fn report(human: i64, agent: i64, projects: Vec<ProjectReport>) -> Report {
        Report {
            total_human_seconds: human,
            total_agent_seconds: agent,
            session_count: projects.len(),
            projects,
        }
    }

    fn sess(handle: &str, project: Option<&str>, agent: i64, idle: bool) -> SessionView {
        SessionView {
            handle: handle.to_string(),
            session_id: format!("id-{handle}"),
            harness: "claude".to_string(),
            kind: "agent".to_string(),
            project: project.map(|p| p.to_string()),
            label: None,
            started_at: "now".to_string(),
            human_seconds: 0,
            agent_seconds: agent,
            idle,
            last_activity_at: None,
            last_human_at: None,
        }
    }

    #[test]
    fn headline_reads_totals() {
        let s = StatusView {
            active: vec![],
            today: report(600, 1800, vec![]),
            sync_pending: 3,
            hydrating: false,
        };
        let h = Headline::from_status(&s);
        assert_eq!(h.human_seconds, 600);
        assert_eq!(h.agent_seconds, 1800);
        assert_eq!(h.sync_pending, 3);
        assert_eq!(h.multiplier(), Some(3.0));
        assert_eq!(h.multiplier_label(), "3.0x parallel");
    }

    #[test]
    fn no_human_means_no_multiplier() {
        let h = Headline {
            human_seconds: 0,
            agent_seconds: 1800,
            sync_pending: 0,
        };
        assert_eq!(h.multiplier(), None);
        assert_eq!(h.multiplier_label(), "—x parallel");
    }

    #[test]
    fn lanes_put_human_first_and_drop_idle_agents() {
        let s = StatusView {
            active: vec![
                sess("a1", Some("acme/api"), 1200, false),
                sess("a2", Some("acme/web"), 0, false), // no agent wall → dropped
                {
                    let mut m = sess("m1", Some("acme/api"), 999, false);
                    m.kind = "manual".to_string(); // manual → dropped from lanes
                    m
                },
            ],
            today: report(600, 1200, vec![]),
            sync_pending: 0,
            hydrating: false,
        };
        let lanes = lanes(&s);
        assert_eq!(lanes.len(), 2);
        assert!(lanes[0].is_human);
        assert_eq!(lanes[0].seconds, 600);
        assert_eq!(lanes[1].label, "a1 · api");
        assert_eq!(lanes[1].seconds, 1200);
        assert!(!lanes[1].is_human);
    }

    #[test]
    fn tick_grows_engaged_by_live_tail_and_keeps_totals_consistent() {
        use time::macros::datetime;
        let now = datetime!(2026-06-27 10:00:10 UTC);
        let mut a1 = sess("a1", Some("acme/api"), 100, false); // engaged, 5s tail
        a1.last_activity_at = Some("2026-06-27T10:00:05Z".into());
        a1.last_human_at = Some("2026-06-27T10:00:05Z".into());
        let mut a2 = sess("a2", Some("acme/web"), 50, true); // idle → frozen
        a2.last_activity_at = Some("2026-06-27T10:00:09Z".into());
        let s = StatusView {
            active: vec![a1, a2],
            today: report(
                10,
                150,
                vec![
                    ProjectReport {
                        project: Some("acme/api".into()),
                        human_seconds: 10,
                        agent_wall_seconds: 100,
                    },
                    ProjectReport {
                        project: Some("acme/web".into()),
                        human_seconds: 0,
                        agent_wall_seconds: 50,
                    },
                ],
            ),
            sync_pending: 0,
            hydrating: false,
        };
        let out = tick(&s, now, 300);
        // Engaged agent grows by its 5s tail; idle session is untouched.
        assert_eq!(out.active[0].agent_seconds, 105);
        assert_eq!(out.active[0].human_seconds, 5);
        assert_eq!(out.active[1].agent_seconds, 50);
        assert_eq!(out.active[1].human_seconds, 0);
        // Totals: agent +5, human +5 (deduped via the most-recent signal).
        assert_eq!(out.today.total_agent_seconds, 155);
        assert_eq!(out.today.total_human_seconds, 15);
        // Per-project rows still sum to the totals.
        let agent_sum: i64 = out
            .today
            .projects
            .iter()
            .map(|p| p.agent_wall_seconds)
            .sum();
        let human_sum: i64 = out.today.projects.iter().map(|p| p.human_seconds).sum();
        assert_eq!(agent_sum, out.today.total_agent_seconds);
        assert_eq!(human_sum, out.today.total_human_seconds);
        // The tail is clamped to idle: a tiny idle window caps the growth.
        assert_eq!(tick(&s, now, 2).active[0].agent_seconds, 102);
    }
}
