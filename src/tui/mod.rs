use std::time::Duration;

mod app;
mod event;
mod input;
mod markdown;
mod render;
mod run;
mod text;

pub use app::UiConfig;
pub use run::run;

const EXPANDED_MAX_LINES: usize = 200;
const MAX_INPUT_LINES: usize = 8;
const SPINNER_TICK: Duration = Duration::from_millis(100);
const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
