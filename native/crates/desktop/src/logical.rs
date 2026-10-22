//! Start and end sides that follow the layout direction, the native
//! counterpart of the web client's logical CSS properties (`ps-`, `me-`,
//! `inset-s-`). English stays as measured; right-to-left locales mirror it.

use gpui_kit::{Pixels, Styled, prelude::FluentBuilder};

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

    /// Distance of an absolutely placed element from the end side.
    fn inset_end(self, direction: Direction, value: Pixels) -> Self {
        match direction {
            Direction::LeftToRight => self.right(value),
            Direction::RightToLeft => self.left(value),
        }
    }
}

impl<T: Styled + Sized + FluentBuilder> LogicalStyledExt for T {}
