use super::*;
use crate::test_backend::VT100Backend;
use pretty_assertions::assert_eq;
use ratatui::layout::Rect;
use ratatui::style::Style;

fn draw_live_status(terminal: &mut crate::custom_terminal::Terminal<VT100Backend>, text: &str) {
    terminal
        .draw(|frame| {
            let top = frame.area().top();
            let buffer = frame.buffer_mut();
            buffer.set_string(0, top, text, Style::default());
            buffer.set_string(0, top + 2, "branch work remains active", Style::default());
        })
        .expect("draw live status");
}

#[test]
fn constrained_history_survives_live_redraw() {
    let width = 64;
    let height = 8;
    for viewport_top in [0, 1] {
        let backend = VT100Backend::with_scrollback(width, height, /*scrollback_len*/ 64);
        let mut terminal =
            crate::custom_terminal::Terminal::with_options(backend).expect("terminal");
        terminal.set_viewport_area(Rect::new(0, viewport_top, width, height - viewport_top));
        draw_live_status(&mut terminal, "live: Emerging task");
        let message = "history remains visible during branch work";
        insert_history_lines(&mut terminal, vec![message.into()]).expect("insert history");
        draw_live_status(&mut terminal, "live: Kindling task");

        let max_scrollback = {
            let screen = terminal.backend_mut().vt100_mut().screen_mut();
            screen.set_scrollback(usize::MAX);
            screen.scrollback()
        };
        let mut rows = Vec::new();
        for offset in (1..=max_scrollback).rev() {
            let screen = terminal.backend_mut().vt100_mut().screen_mut();
            screen.set_scrollback(offset);
            rows.push(
                screen
                    .rows(/*start*/ 0, width)
                    .next()
                    .expect("first screen row"),
            );
        }
        terminal
            .backend_mut()
            .vt100_mut()
            .screen_mut()
            .set_scrollback(0);
        rows.extend(terminal.backend().vt100().screen().rows(/*start*/ 0, width));
        assert_eq!(rows.iter().filter(|row| row.contains(message)).count(), 1);
        insta::assert_snapshot!(
            format!("constrained_history_top_{viewport_top}"),
            rows.join("\n")
        );
    }
}
