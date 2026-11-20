use super::TerminalEvent;
use anyhow::Result;
use tokio::{
    io::{AsyncWrite, AsyncWriteExt},
    sync::{mpsc, watch},
};
use tokio_util::sync::CancellationToken;
use unicode_width::UnicodeWidthChar;

#[cfg(target_os = "windows")]
struct ConsoleModeGuard {
    original: Option<::windows::Win32::System::Console::CONSOLE_MODE>,
    usable: bool,
}

#[cfg(target_os = "windows")]
impl ConsoleModeGuard {
    fn enable(requested: bool) -> Self {
        use ::windows::Win32::System::Console::{
            CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle,
            STD_ERROR_HANDLE, SetConsoleMode,
        };
        let mut guard = Self {
            original: None,
            usable: false,
        };
        if !requested {
            return guard;
        }
        // ANSI is only used on stderr. If this console cannot enable virtual
        // terminal sequences, fall back to permanent final lines without partials.
        unsafe {
            let Ok(handle) = GetStdHandle(STD_ERROR_HANDLE) else {
                return guard;
            };
            let mut original = CONSOLE_MODE::default();
            if GetConsoleMode(handle, &mut original).is_ok()
                && SetConsoleMode(handle, original | ENABLE_VIRTUAL_TERMINAL_PROCESSING).is_ok()
            {
                guard.original = Some(original);
                guard.usable = true;
            }
        }
        guard
    }
}

#[cfg(target_os = "windows")]
impl Drop for ConsoleModeGuard {
    fn drop(&mut self) {
        use ::windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, SetConsoleMode};
        if let Some(original) = self.original {
            unsafe {
                if let Ok(handle) = GetStdHandle(STD_ERROR_HANDLE) {
                    let _ = SetConsoleMode(handle, original);
                }
            }
        }
    }
}

/// Strip terminal control characters, preserving multilingual text.
pub fn safe_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn partial_line(text: &str) -> String {
    let mut width = 0;
    safe_text(text)
        .chars()
        .take_while(|c| {
            width += c.width().unwrap_or(0);
            width <= 72
        })
        .collect()
}

async fn clear<W: AsyncWrite + Unpin>(stderr: &mut W, visible: &mut bool) -> Result<()> {
    if *visible {
        stderr.write_all(b"\r\x1b[2K").await?;
        *visible = false;
    }
    Ok(())
}

pub async fn run<O: AsyncWrite + Unpin, E: AsyncWrite + Unpin>(
    mut stdout: O,
    mut stderr: E,
    mut rx: mpsc::Receiver<TerminalEvent>,
    mut partial: watch::Receiver<String>,
    stop: CancellationToken,
    interactive: bool,
) -> Result<()> {
    #[cfg(target_os = "windows")]
    let console_mode = ConsoleModeGuard::enable(interactive);
    #[cfg(target_os = "windows")]
    let interactive = interactive && console_mode.usable;
    let mut visible = false;
    let mut stopping = false;
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled(), if !stopping => { stopping = true; rx.close(); }
            event = rx.recv() => match event {
                Some(event) => {
                    clear(&mut stderr, &mut visible).await?;
                    match event {
                        TerminalEvent::Final(record) => {
                            let text = super::transcript_text(&record);
                            for (index, line) in text.lines().enumerate() {
                                if index > 0 { stdout.write_all(b"\n").await?; }
                                stdout.write_all(safe_text(line).as_bytes()).await?;
                            }
                            stdout.write_all(b"\n").await?;
                            stdout.flush().await?;
                        }
                        TerminalEvent::Log(text) => {
                            // tracing output already has a newline; do not allow injected escape sequences.
                            stderr.write_all(safe_text(text.trim_end()).as_bytes()).await?;
                            stderr.write_all(b"\n").await?;
                        }
                    }
                    stderr.flush().await?;
                }
                None => break,
            },
            changed = partial.changed(), if interactive && !stopping => {
                if changed.is_err() { break; }
                let text = partial.borrow_and_update().clone();
                clear(&mut stderr, &mut visible).await?;
                if !text.is_empty() {
                    stderr.write_all(format!("\r… {}", partial_line(&text)).as_bytes()).await?;
                    visible = true;
                }
                stderr.flush().await?;
            }
        }
    }
    clear(&mut stderr, &mut visible).await?;
    stdout.flush().await?;
    stderr.flush().await?;
    Ok(())
}
