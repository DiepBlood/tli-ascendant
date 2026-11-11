//! Map affix warnings (Netherrealm Resonance): reads the affixes listed on the map page and grades them
//! danger / warning / none, so the app can frame them in the game. An affix the user has not graded is framed in the
//! colour the game shows it in (since 1.2.0).
//!
//! Public since 1.2.0. Here: the reader, the sentence table and the frames' bitmap ([`frames`]); the window over the
//! game is `app::affix_overlay`, the live reads `app::affix_live` and the settings page `app::affix_page`.

pub mod bullets;
pub mod dict;
pub mod frames;
pub mod learned;
pub mod panel;
pub mod read;

pub use bullets::{Bullet, BulletColour};
pub use dict::{Dict, Grade, Grades, Lookup, Margin};
pub use frames::FrameStyle;
pub use learned::Learned;
pub use read::{Entry, Panel, Prep, read, read_with};
