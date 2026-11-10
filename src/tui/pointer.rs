//! Owned-screen pointer state. Rendering owns hit geometry; wheel fractions never
//! leak across direction changes or presentation transitions.
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

#[derive(Default)]
pub(super) struct Wheel {
    pending: f64,
}

impl Wheel {
    pub fn rows(&mut self, direction: f64, speed: f64) -> i64 {
        if self.pending.signum() != direction {
            self.pending = 0.0;
        }
        self.pending += direction * speed;
        let rounded = self.pending.round();
        if rounded != 0.0 && (self.pending - rounded).abs() < 1e-9 {
            self.pending = rounded;
        }
        let rows = self.pending as i64;
        self.pending = self.pending.fract();
        rows
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Target {
    row: usize,
    start: usize,
    end: usize,
}

#[derive(Default)]
pub(super) struct Follow {
    target: Option<Target>,
    pointer: Option<(usize, usize)>,
    pressed: bool,
}

impl Follow {
    pub fn render(&mut self, cols: usize, row: usize, visible: bool) -> Option<String> {
        let label = [" ↓ Follow output · Esc ", " ↓ Follow output ", " ↓ Bottom ", " ↓ "].into_iter().find(|label| super::text::visible_length(label) <= cols);
        let Some(label) = label.filter(|_| visible) else {
            *self = Self::default();
            return None;
        };
        let width = super::text::visible_length(label);
        let start = (cols - width) / 2;
        let target = Target { row, start, end: start + width };
        if self.target != Some(target) {
            self.pressed = false;
        }
        self.target = Some(target);
        let hovered = self.pointer.is_some_and(|(col, y)| y == row && col >= start && col < start + width);
        Some(format!("{}{}{}\x1b[0m", " ".repeat(start), if hovered { "\x1b[7;1m" } else { "\x1b[2m" }, label))
    }

    /// Returns Some(jump) only when the control consumes this event.
    pub fn handle(&mut self, event: MouseEvent) -> Option<bool> {
        let target = self.target?;
        let col = event.column as usize;
        let row = event.row as usize;
        let inside = row == target.row && col >= target.start && col < target.end;
        let was_inside = self.pointer.is_some_and(|(x, y)| y == target.row && x >= target.start && x < target.end);
        self.pointer = Some((col, row));
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if inside => {
                self.pressed = true;
                Some(false)
            }
            MouseEventKind::Up(MouseButton::Left) if self.pressed => {
                self.pressed = false;
                Some(inside)
            }
            MouseEventKind::Drag(MouseButton::Left) if self.pressed => Some(false),
            MouseEventKind::Moved if inside != was_inside => Some(false),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    #[test]
    fn fractions_accumulate_exactly_and_reverse_without_old_remainder() {
        let mut wheel = Wheel::default();
        for _ in 0..9 {
            assert_eq!(wheel.rows(1.0, 0.1), 0);
        }
        assert_eq!(wheel.rows(1.0, 0.1), 1);
        assert_eq!(wheel.rows(1.0, 0.75), 0);
        assert_eq!(wheel.rows(-1.0, 0.75), 0);
        assert_eq!(wheel.rows(-1.0, 0.75), -1);
        assert_eq!(wheel.rows(1.0, 2.5), 2);
    }

    #[test]
    fn follow_requires_press_and_release_inside_rendered_target() {
        let mut follow = Follow::default();
        let event = |kind, col| MouseEvent { kind, column: col, row: 5, modifiers: KeyModifiers::NONE };
        assert!(follow.render(80, 5, true).is_some());
        assert_eq!(follow.handle(event(MouseEventKind::Down(MouseButton::Left), 40)), Some(false));
        assert_eq!(follow.handle(event(MouseEventKind::Up(MouseButton::Left), 0)), Some(false));
        assert_eq!(follow.handle(event(MouseEventKind::Up(MouseButton::Left), 40)), None);
        follow.handle(event(MouseEventKind::Down(MouseButton::Left), 40));
        assert_eq!(follow.handle(event(MouseEventKind::Up(MouseButton::Left), 40)), Some(true));
        follow.handle(event(MouseEventKind::Down(MouseButton::Left), 40));
        follow.render(80, 5, false);
        assert_eq!(follow.handle(event(MouseEventKind::Up(MouseButton::Left), 40)), None);
        follow.render(80, 5, true);
        follow.handle(event(MouseEventKind::Down(MouseButton::Left), 40));
        follow.render(20, 5, true);
        assert_eq!(follow.handle(event(MouseEventKind::Up(MouseButton::Left), 10)), None, "layout changes invalidate the old press");
        assert!(follow.render(2, 5, true).is_none());
    }
}
