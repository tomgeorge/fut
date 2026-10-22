use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{protocol::SelectedTarget, resources::ResourceSnapshot};

use super::{
    agents::{self, AgentItem},
    config::{AgentScope, SemanticStyle, SpinnerConfig, StylesConfig},
    dialog::{
        dialog_area, fill_row, frame_inner, render_footer, render_frame, render_list_scrollbar,
        render_title,
    },
    federation::{MachineId, State as FederationState},
    fuzzy,
    navigator::NavigatorSelection,
    notifications::NotificationState,
    presentation::{ItemState, apply_item_state},
};

const MAX_WIDTH: u16 = 80;
const MAX_HEIGHT: u16 = 20;
const MAX_QUERY_BYTES: usize = 512;

pub(super) struct AgentsDialog {
    rows: Vec<AgentItem>,
    filtered: Vec<usize>,
    query: String,
    selected: usize,
    scroll: usize,
    federated: bool,
}

pub(super) enum AgentsAction {
    Stay,
    Close,
    Select(NavigatorSelection),
}

impl AgentsDialog {
    pub(super) fn open(
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        notifications: &NotificationState,
    ) -> Self {
        let rows = agents::items(snapshot, focused, notifications, AgentScope::Global);
        let selected = rows.iter().position(|row| row.current).unwrap_or(0);
        let mut dialog = Self {
            rows,
            filtered: Vec::new(),
            query: String::new(),
            selected,
            scroll: 0,
            federated: false,
        };
        dialog.refilter();
        dialog
    }

    pub(super) fn accept_resources(
        &mut self,
        snapshot: &ResourceSnapshot,
        focused: &SelectedTarget,
        notifications: &NotificationState,
    ) {
        if self.federated {
            // Federated rows carry machine labels and remote agents; the
            // federation state delivers local updates with them intact.
            return;
        }
        let selected = self.rows.get(self.selected).map(|row| row.terminal_id);
        self.rows = agents::items(snapshot, focused, notifications, AgentScope::Global);
        self.refilter();
        self.selected = selected
            .and_then(|terminal_id| {
                self.rows
                    .iter()
                    .position(|row| row.terminal_id == terminal_id)
            })
            .or_else(|| self.rows.iter().position(|row| row.current))
            .unwrap_or(0);
        self.ensure_selected_match();
    }

    pub(super) fn accept_federation(
        &mut self,
        state: &FederationState,
        active: MachineId,
        focused: &SelectedTarget,
        active_notifications: &NotificationState,
    ) {
        if state.registry.iter().count() <= 1 {
            return;
        }
        self.federated = true;
        let selected = self
            .rows
            .get(self.selected)
            .map(|row| (row.machine, row.terminal_id));
        let mut rows = Vec::new();
        for (machine, endpoint) in state.registry.iter() {
            let Some(resources) = endpoint.metadata.resources.as_ref() else {
                continue;
            };
            let current = endpoint.is_current(resources);
            let mut notifications = NotificationState::default();
            if machine == active {
                notifications = active_notifications.clone();
            } else if let Some(alerts) = endpoint.metadata.alerts.as_ref() {
                notifications.accept_alerts(alerts.value.clone());
            }
            let label = endpoint.spec.label();
            let mut items = agents::items(
                &resources.value,
                focused,
                &notifications,
                AgentScope::Global,
            );
            for item in &mut items {
                item.machine = machine;
                item.generation = endpoint.generation;
                item.machine_label = Some(if current {
                    label.to_owned()
                } else {
                    format!("{label} · stale")
                });
                item.selectable = current;
                item.current &= machine == active;
            }
            rows.extend(items);
        }
        self.rows = rows;
        self.refilter();
        self.selected = selected
            .and_then(|selected| {
                self.rows
                    .iter()
                    .position(|row| (row.machine, row.terminal_id) == selected)
            })
            .or_else(|| self.rows.iter().position(|row| row.current))
            .unwrap_or(0);
        self.ensure_selected_match();
    }

    pub(super) fn key(&mut self, key: KeyEvent, visible_rows: usize) -> AgentsAction {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return AgentsAction::Stay;
        }
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => return AgentsAction::Close,
            (KeyCode::Enter, _) => {
                return self
                    .rows
                    .get(self.selected)
                    .filter(|row| row.selectable)
                    .map_or(AgentsAction::Stay, |row| {
                        AgentsAction::Select(NavigatorSelection {
                            machine: row.machine,
                            generation: row.generation,
                            selector: crate::resources::TargetSelector::Pane(row.pane_id),
                        })
                    });
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::CONTROL) => {
                self.move_selection(-1)
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                self.move_selection(1)
            }
            (KeyCode::Home, _) => self.select_filtered(0),
            (KeyCode::End, _) => self.select_filtered(self.filtered.len().saturating_sub(1)),
            (KeyCode::PageUp, _) => self.move_selection(-(visible_rows.max(1) as isize)),
            (KeyCode::PageDown, _) => self.move_selection(visible_rows.max(1) as isize),
            (KeyCode::Backspace | KeyCode::Delete, _) => self.remove_last_grapheme(),
            (KeyCode::Char(character), modifiers)
                if !character.is_control()
                    && !modifiers.intersects(
                        KeyModifiers::CONTROL
                            | KeyModifiers::ALT
                            | KeyModifiers::SUPER
                            | KeyModifiers::HYPER
                            | KeyModifiers::META,
                    )
                    && self.query.len() + character.len_utf8() <= MAX_QUERY_BYTES =>
            {
                self.query.push(character);
                self.refilter();
                self.ensure_selected_match();
            }
            _ => {}
        }
        self.keep_visible(visible_rows);
        AgentsAction::Stay
    }

    pub(super) fn paste(&mut self, value: &str) {
        for character in value.chars().filter(|character| !character.is_control()) {
            if self.query.len() + character.len_utf8() > MAX_QUERY_BYTES {
                break;
            }
            self.query.push(character);
        }
        self.refilter();
        self.ensure_selected_match();
    }

    pub(super) fn render(
        &mut self,
        host: Rect,
        elapsed_ms: usize,
        spinner: &SpinnerConfig,
        styles: &StylesConfig,
        buffer: &mut Buffer,
    ) {
        let area = render_frame(dialog_area(host, MAX_WIDTH, MAX_HEIGHT), buffer);
        if area.width == 0 || area.height == 0 {
            return;
        }
        let (header, footer) = chrome_rows(area.height);
        if header == 1 {
            let title = if self.query.is_empty() {
                " agents".to_owned()
            } else {
                format!(" agents › {}", self.query)
            };
            render_title(area, &title, buffer);
        }
        if footer == 1 {
            render_footer(
                area,
                " type search  ↑↓/C-jk move  enter switch  esc close",
                buffer,
            );
        }
        let body = Rect::new(
            area.x,
            area.y + header,
            area.width,
            area.height.saturating_sub(header + footer),
        );
        self.keep_visible(usize::from(body.height));
        if self.rows.is_empty() {
            buffer.set_stringn(
                body.x,
                body.y,
                " No agents",
                usize::from(body.width),
                Style::default(),
            );
        } else if self.filtered.is_empty() {
            buffer.set_stringn(
                body.x,
                body.y,
                " No matching agents",
                usize::from(body.width),
                Style::default(),
            );
        } else {
            for (line, index) in self
                .filtered
                .iter()
                .skip(self.scroll)
                .take(usize::from(body.height))
                .enumerate()
            {
                let row = &self.rows[*index];
                let selected = *index == self.selected;
                // The cursor is the strongest cue in a picker, so it takes the
                // current pill; the agent you're in keeps only bold and its marker.
                let mut style = styles.apply(SemanticStyle::Normal, Style::default());
                if row.current {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if selected {
                    style = apply_item_state(
                        styles,
                        ItemState {
                            current: true,
                            ..ItemState::default()
                        },
                        style,
                    );
                }
                // A status foreground on a reversed pill would become its
                // background, so the pill row stays one color.
                let (activity_style, status_style) = if selected {
                    (style, style)
                } else {
                    (
                        styles.apply(row.activity_style(), style),
                        styles.apply(row.status_style(), style),
                    )
                };
                let y = body.y + line as u16;
                fill_row(Rect::new(body.x, y, body.width, 1), style, buffer);
                buffer.set_line(
                    body.x,
                    y,
                    &row.line(
                        elapsed_ms,
                        spinner,
                        " › ",
                        agents::LineStyles {
                            title: style,
                            detail: style,
                            activity: activity_style,
                            status: status_style,
                        },
                    ),
                    body.width,
                );
            }
            render_list_scrollbar(self.scroll, self.filtered.len(), body, buffer);
        }
    }

    fn refilter(&mut self) {
        self.filtered = fuzzy::ranked(&self.query, self.rows.iter().map(AgentItem::search_text));
        self.scroll = 0;
    }

    fn remove_last_grapheme(&mut self) {
        if let Some((index, _)) = self.query.grapheme_indices(true).next_back() {
            self.query.truncate(index);
            self.refilter();
            self.ensure_selected_match();
        }
    }

    fn ensure_selected_match(&mut self) {
        if !self.filtered.contains(&self.selected) {
            self.selected = self.filtered.first().copied().unwrap_or(0);
        }
    }

    fn selected_filtered_position(&self) -> usize {
        self.filtered
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0)
    }

    fn select_filtered(&mut self, position: usize) {
        if let Some(index) = self.filtered.get(position) {
            self.selected = *index;
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let position = self
            .selected_filtered_position()
            .saturating_add_signed(delta)
            .min(self.filtered.len().saturating_sub(1));
        self.select_filtered(position);
    }

    fn keep_visible(&mut self, height: usize) {
        let height = height.max(1);
        let selected = self.selected_filtered_position();
        if selected < self.scroll {
            self.scroll = selected;
        } else if selected >= self.scroll + height {
            self.scroll = selected + 1 - height;
        }
    }
}

fn chrome_rows(height: u16) -> (u16, u16) {
    (u16::from(height >= 2), u16::from(height >= 3))
}

pub(super) fn dialog_body_rows(host: Rect) -> usize {
    let area = frame_inner(dialog_area(host, MAX_WIDTH, MAX_HEIGHT));
    let (header, footer) = chrome_rows(area.height);
    usize::from(area.height.saturating_sub(header + footer))
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;
    use crate::{
        client::notifications::ActivityIndicator,
        domain::{PaneId, StatusState, TerminalId},
    };

    fn dialog(current: bool, selected: usize) -> AgentsDialog {
        let row = |current| AgentItem {
            machine: MachineId::Local,
            generation: super::super::federation::Generation::default(),
            machine_label: None,
            selectable: true,
            terminal_id: TerminalId::new(),
            pane_id: PaneId::new(),
            session: "fut".into(),
            workspace: "agents-popup".into(),
            tab: "node".into(),
            app: "codex".into(),
            title: Some("Checking code".into()),
            message: Some("Running focused tests".into()),
            blocked_kind: None,
            state: StatusState::Working,
            progress: Some(42),
            current,
            indicator: Some(ActivityIndicator::Working),
        };
        AgentsDialog {
            rows: vec![row(false), row(current)],
            filtered: vec![0, 1],
            query: String::new(),
            selected,
            scroll: 0,
            federated: false,
        }
    }

    fn render(dialog: &mut AgentsDialog) -> Buffer {
        let host = Rect::new(0, 0, 80, 10);
        let mut buffer = Buffer::empty(host);
        dialog.render(
            host,
            0,
            &SpinnerConfig::default(),
            &StylesConfig::default(),
            &mut buffer,
        );
        buffer
    }

    #[test]
    fn selected_row_takes_the_current_pill_as_one_color() {
        let buffer = render(&mut dialog(false, 0));

        let app = &buffer[(8, 2)];
        let status = &buffer[(14, 2)];
        assert_eq!(app.fg, Color::Blue);
        assert!(app.modifier.contains(Modifier::REVERSED));
        assert_eq!(status.fg, app.fg);
        assert_eq!(status.bg, app.bg);
        assert_eq!(status.modifier, app.modifier);
        let rendered = (0..80)
            .map(|column| buffer[(column, 2)].symbol())
            .collect::<String>();
        assert!(rendered.contains("⠋ 42% codex working"), "{rendered:?}");
    }

    #[test]
    fn current_row_off_the_cursor_is_bold_without_the_pill() {
        let buffer = render(&mut dialog(true, 0));

        let app = &buffer[(8, 3)];
        let status = &buffer[(14, 3)];
        assert!(app.modifier.contains(Modifier::BOLD));
        assert!(!app.modifier.contains(Modifier::REVERSED));
        assert_eq!(status.fg, Color::LightCyan);
    }
}
