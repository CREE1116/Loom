//! Animate visible progress cells without invalidating the transcript layout cache.
use super::*;

pub(super) const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(super) fn paint(line: &mut Canvas, first: bool, frame: usize) {
    let end = line
        .cells
        .iter()
        .rposition(|cell| !cell.text.trim().is_empty())
        .map_or(2, |x| x + 1);
    let length = end.saturating_sub(2).max(1);
    let head = (frame * 2) % (length + 4);
    for (x, cell) in line.cells.iter_mut().enumerate().take(end).skip(2) {
        let distance = head.abs_diff(x - 2);
        cell.tone = if distance < 3 {
            Tone::Accent
        } else {
            Tone::Muted
        };
    }
    if first {
        line.text(0, 0, SPINNER[frame % SPINNER.len()], Tone::Accent);
    }
}
