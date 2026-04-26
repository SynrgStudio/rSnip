pub mod app;
pub mod clipboard;
pub mod config;
pub mod editor;
pub mod errors;
pub mod hotkeys;
pub mod ipc;
pub mod ocr;
pub mod overlay;
pub mod paths;
pub mod recording;
pub mod screen;
pub mod single_instance;

use crate::errors::Result;

fn main() -> Result<()> {
    app::run()
}
