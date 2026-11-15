//! Start and end sides that follow the layout direction, the native
//! counterpart of the web client's logical CSS properties (`ps-`, `me-`,
//! `inset-s-`). English stays as measured; right-to-left locales mirror it.

use gpui_kit::{JustifyContent, Pixels, Styled, prelude::FluentBuilder};

use crate::locale::Direction;

/// Logical padding, margin, inset and flow on any styled element.
pub trait LogicalStyledExt: Styled + Sized + FluentBuilder {
    /// A flex row from the start side.
    fn flex_row_from(self, direction: Direction) -> Self {
        self.flex()
            .when(direction == Direction::RightToLeft, |this| {
                this.flex_row_reverse()
            })
    }

    /// Pack a `flex_row_from` row toward the line end. `FlexEnd` follows
    /// row reversal; `justify_end` does not.
    fn justify_line_end(mut self) -> Self {
        self.style().justify_content = Some(JustifyContent::FlexEnd);
        self
    }

    fn padding_start(self, direction: Direction, value: Pixels) -> Self {
        match direction {
            Direction::LeftToRight => self.pl(value),
            Direction::RightToLeft => self.pr(value),
        }
    }

    fn padding_end(self, direction: Direction, value: Pixels) -> Self {
        match direction {
            Direction::LeftToRight => self.pr(value),
            Direction::RightToLeft => self.pl(value),
        }
    }

    fn margin_start(self, direction: Direction, value: Pixels) -> Self {
        match direction {
            Direction::LeftToRight => self.ml(value),
            Direction::RightToLeft => self.mr(value),
        }
    }

    fn margin_end(self, direction: Direction, value: Pixels) -> Self {
        match direction {
            Direction::LeftToRight => self.mr(value),
            Direction::RightToLeft => self.ml(value),
        }
    }

    /// Distance of an absolutely placed element from the start side.
    fn inset_start(self, direction: Direction, value: Pixels) -> Self {
        match direction {
            Direction::LeftToRight => self.left(value),
            Direction::RightToLeft => self.right(value),
        }
    }

    /// Distance of an absolutely placed element from the end side.
    fn inset_end(self, direction: Direction, value: Pixels) -> Self {
        match direction {
            Direction::LeftToRight => self.right(value),
            Direction::RightToLeft => self.left(value),
        }
    }
}

impl<T: Styled + Sized + FluentBuilder> LogicalStyledExt for T {}
