//! tmux-aware process explorer.
//!
//! Test fixtures build a `Snapshot` with `..Default::default()` then assign the
//! two or three fields the test cares about. Clippy flags that as
//! `field_reassign_with_default`, but the alternative — spelling out eight
//! irrelevant fields per fixture — makes the tests harder to read than the
//! pattern it objects to.
#![cfg_attr(test, allow(clippy::field_reassign_with_default))]

pub mod collect;
pub mod model;
pub mod palette;
pub mod plain;
pub mod tree;
pub mod ui;
