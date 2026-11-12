//! Live view: opens the GPU once, redraws in place once a second.

use std::io::Write;
use std::time::Duration;

use overrune_core::{Gpu, GpuError};

use crate::status::{render_human, render_json};

/// Runs until the process is interrupted. Human mode repaints from the home
/// position and clears only what is left over, so the screen never scrolls or
/// flashes; `--json` streams one object per line instead.
pub fn run(gpu: &Gpu, json: bool) -> Result<(), GpuError> {
    let mut out = std::io::stdout().lock();
    if !json {
        let _ = write!(out, "\x1b[2J");
    }
    loop {
        let s = gpu.sample()?;
        let frame = if json {
            format!("{}\n", render_json(&s))
        } else {
            let body = render_human(&s).replace('\n', "\x1b[K\n");
            format!("\x1b[H{body}\x1b[J")
        };
        // A closed pipe (status bar went away) ends the loop quietly.
        if out
            .write_all(frame.as_bytes())
            .and_then(|_| out.flush())
            .is_err()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
