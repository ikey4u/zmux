//! Crossterm backend that always emits `\x1b[49m` for default-background cells.
//! Ratatui's stock backend skips `SetBackgroundColor(Reset)` when its per-draw
//! tracker already holds Reset, leaving stale SGR backgrounds on the physical
//! terminal after scrolls, splits, and other repaints.

use std::io::{self, Write};

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    execute, queue,
    style::{
        Attribute as CrosstermAttribute, Color as CrosstermColor,
        Colors as CrosstermColors, Print, SetAttribute, SetBackgroundColor,
        SetColors, SetForegroundColor,
    },
    terminal::{self, Clear},
};
use ratatui::{
    backend::{Backend, ClearType, IntoCrossterm, WindowSize},
    buffer::Cell,
    layout::{Position, Size},
    style::{Color, Modifier},
};

pub struct TerminalBackend<W: Write> {
    writer: W,
}

impl<W: Write> TerminalBackend<W> {
    pub fn new(writer: W) -> Self {
        Self { writer }
    }
}

impl<W: Write> Write for TerminalBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.writer.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl<W: Write> Backend for TerminalBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let mut fg = Color::Reset;
        let mut bg = Color::Reset;
        let mut modifier = Modifier::empty();
        let mut last_pos: Option<Position> = None;

        for (x, y, cell) in content {
            if !matches!(last_pos, Some(p) if x == p.x + 1 && y == p.y) {
                queue!(self.writer, MoveTo(x, y))?;
            }
            last_pos = Some(Position { x, y });

            if cell.modifier != modifier {
                ModifierDiff {
                    from: modifier,
                    to: cell.modifier,
                }
                .queue(&mut self.writer)?;
                modifier = cell.modifier;
            }

            if cell.bg == Color::Reset {
                queue!(self.writer, SetBackgroundColor(CrosstermColor::Reset))?;
                bg = Color::Reset;
                if cell.fg != fg {
                    queue!(
                        self.writer,
                        SetForegroundColor(cell.fg.into_crossterm())
                    )?;
                    fg = cell.fg;
                }
            } else if cell.fg != fg || cell.bg != bg {
                queue!(
                    self.writer,
                    SetColors(CrosstermColors::new(
                        cell.fg.into_crossterm(),
                        cell.bg.into_crossterm(),
                    ))
                )?;
                fg = cell.fg;
                bg = cell.bg;
            }

            queue!(self.writer, Print(cell.symbol()))?;
        }

        queue!(
            self.writer,
            SetForegroundColor(CrosstermColor::Reset),
            SetBackgroundColor(CrosstermColor::Reset),
            SetAttribute(CrosstermAttribute::Reset),
        )
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        execute!(self.writer, Hide)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(self.writer, Show)
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        crossterm::cursor::position()
            .map(|(x, y)| Position { x, y })
            .map_err(io::Error::other)
    }

    fn set_cursor_position<P: Into<Position>>(
        &mut self,
        position: P,
    ) -> io::Result<()> {
        let Position { x, y } = position.into();
        execute!(self.writer, MoveTo(x, y))
    }

    fn clear(&mut self) -> io::Result<()> {
        // Ratatui calls this on every viewport resize. Pane cells are drawn by
        // server ANSI and skipped in Ratatui's buffer, so a physical ED2 would
        // erase them until a later server frame arrives. Ratatui still resets
        // its own diff buffers; chrome repaints normally and the server owns
        // clearing/repainting its viewport after the matching RESIZE.
        Ok(())
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        if clear_type == ClearType::All {
            return self.clear();
        }
        execute!(
            self.writer,
            Clear(match clear_type {
                ClearType::All => terminal::ClearType::All,
                ClearType::AfterCursor => terminal::ClearType::FromCursorDown,
                ClearType::BeforeCursor => terminal::ClearType::FromCursorUp,
                ClearType::CurrentLine => terminal::ClearType::CurrentLine,
                ClearType::UntilNewLine => terminal::ClearType::UntilNewLine,
            })
        )
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        for _ in 0..n {
            queue!(self.writer, Print("\n"))?;
        }
        self.writer.flush()
    }

    fn size(&self) -> io::Result<Size> {
        let (width, height) = terminal::size()?;
        Ok(Size { width, height })
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        let terminal::WindowSize {
            columns,
            rows,
            width,
            height,
        } = terminal::window_size()?;
        Ok(WindowSize {
            columns_rows: Size {
                width: columns,
                height: rows,
            },
            pixels: Size { width, height },
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

struct ModifierDiff {
    from: Modifier,
    to: Modifier,
}

impl ModifierDiff {
    fn queue<W: Write>(self, w: &mut W) -> io::Result<()> {
        let removed = self.from - self.to;
        if removed.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CrosstermAttribute::NoReverse))?;
        }

        let reset_intensity =
            removed.contains(Modifier::BOLD) || removed.contains(Modifier::DIM);
        if reset_intensity {
            queue!(w, SetAttribute(CrosstermAttribute::NormalIntensity))?;
            if self.to.contains(Modifier::DIM) {
                queue!(w, SetAttribute(CrosstermAttribute::Dim))?;
            }
            if self.to.contains(Modifier::BOLD) {
                queue!(w, SetAttribute(CrosstermAttribute::Bold))?;
            }
        }

        if removed.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CrosstermAttribute::NoItalic))?;
        }
        if removed.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CrosstermAttribute::NoUnderline))?;
        }
        if removed.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CrosstermAttribute::NotCrossedOut))?;
        }
        if removed.contains(Modifier::HIDDEN) {
            queue!(w, SetAttribute(CrosstermAttribute::NoHidden))?;
        }
        if removed.contains(Modifier::SLOW_BLINK)
            || removed.contains(Modifier::RAPID_BLINK)
        {
            queue!(w, SetAttribute(CrosstermAttribute::NoBlink))?;
        }

        let added = self.to - self.from;
        if added.contains(Modifier::REVERSED) {
            queue!(w, SetAttribute(CrosstermAttribute::Reverse))?;
        }
        if added.contains(Modifier::BOLD) && !reset_intensity {
            queue!(w, SetAttribute(CrosstermAttribute::Bold))?;
        }
        if added.contains(Modifier::ITALIC) {
            queue!(w, SetAttribute(CrosstermAttribute::Italic))?;
        }
        if added.contains(Modifier::UNDERLINED) {
            queue!(w, SetAttribute(CrosstermAttribute::Underlined))?;
        }
        if added.contains(Modifier::DIM) && !reset_intensity {
            queue!(w, SetAttribute(CrosstermAttribute::Dim))?;
        }
        if added.contains(Modifier::CROSSED_OUT) {
            queue!(w, SetAttribute(CrosstermAttribute::CrossedOut))?;
        }
        if added.contains(Modifier::HIDDEN) {
            queue!(w, SetAttribute(CrosstermAttribute::Hidden))?;
        }
        if added.contains(Modifier::SLOW_BLINK) {
            queue!(w, SetAttribute(CrosstermAttribute::SlowBlink))?;
        }
        if added.contains(Modifier::RAPID_BLINK) {
            queue!(w, SetAttribute(CrosstermAttribute::RapidBlink))?;
        }

        Ok(())
    }
}
