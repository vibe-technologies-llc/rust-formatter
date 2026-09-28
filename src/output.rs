use std::{
    io::{self, Write},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColorChoice {
    #[default]
    Auto,
    Always,
    Never,
}

impl ColorChoice {
    /// Resolve `Auto` against a concrete handle, honouring `NO_COLOR`,
    /// `CLICOLOR_FORCE` and terminal detection the same way `anstream` does.
    pub fn resolve(self, raw: &impl anstream::stream::RawStream) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => anstream::AutoStream::choice(raw) == anstream::ColorChoice::Always,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Emit {
    #[default]
    Files,
    Stdout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MessageFormat {
    #[default]
    Human,
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ListMode {
    #[default]
    None,
    /// Every file the selection resolves to.
    Files,
    /// Only files whose formatting differs.
    Different,
}

impl ListMode {
    pub fn is_listing(self) -> bool {
        self != Self::None
    }
}

struct Sink<'a> {
    writer: Box<dyn Write + Send + 'a>,
    color: bool,
}

impl Sink<'_> {
    fn put(&mut self, text: &str) -> io::Result<()> {
        let result = if self.color {
            self.writer.write_all(text.as_bytes())
        } else {
            self.writer
                .write_all(anstream::adapter::strip_str(text).to_string().as_bytes())
        };
        result.and_then(|()| self.writer.flush())
    }
}

/// The two streams the tool owns. stdout carries the product a caller can pipe
/// -- diffs, file lists, JSON, formatted source. stderr carries commentary.
///
/// Diffs are emitted from rustfmt worker threads, so every write takes a lock
/// for the whole message: without it eight parallel `-j 8` processes interleave
/// their diffs line by line.
pub struct Streams<'a> {
    out: Mutex<Sink<'a>>,
    err: Mutex<Sink<'a>>,
    color: ColorChoice,
}

impl<'a> Streams<'a> {
    pub fn new(
        out: impl Write + Send + 'a,
        out_color: bool,
        err: impl Write + Send + 'a,
        err_color: bool,
        color: ColorChoice,
    ) -> Self {
        Self {
            out: Mutex::new(Sink {
                writer: Box::new(out),
                color: out_color,
            }),
            err: Mutex::new(Sink {
                writer: Box::new(err),
                color: err_color,
            }),
            color,
        }
    }

    /// Plain, uncoloured sinks -- what `lib::format` and most tests want.
    pub fn plain(out: impl Write + Send + 'a, err: impl Write + Send + 'a) -> Self {
        Self::new(out, false, err, false, ColorChoice::Never)
    }

    pub fn discard() -> Streams<'static> {
        Streams::plain(io::sink(), io::sink())
    }

    pub fn color(&self) -> ColorChoice {
        self.color
    }

    /// Whether escapes written to stdout survive. rustfmt's own `auto` cannot be
    /// forwarded once its output is captured -- it would see a pipe and drop
    /// colour even when this process is writing to a terminal.
    pub fn product_color(&self) -> bool {
        self.out
            .lock()
            .map_or_else(|err| err.into_inner().color, |sink| sink.color)
    }

    pub fn product(&self, text: &str) -> io::Result<()> {
        Self::put(&self.out, text)
    }

    /// Formatted source, written through untouched. `product` strips ANSI
    /// escapes when colour is off, which would rewrite a source file that
    /// happens to contain one in a string literal.
    pub fn product_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        Self::put_bytes(&self.out, bytes)
    }

    pub fn product_line(&self, text: &str) -> io::Result<()> {
        self.product(&with_newline(text))
    }

    pub fn note(&self, text: &str) -> io::Result<()> {
        Self::put(&self.err, text)
    }

    pub fn note_line(&self, text: &str) -> io::Result<()> {
        self.note(&with_newline(text))
    }

    /// Render into a string with `paint`, then emit it as one locked write.
    pub fn paint_note(&self, paint: impl FnOnce(&mut Vec<u8>) -> io::Result<()>) -> io::Result<()> {
        let mut buf = Vec::new();
        paint(&mut buf)?;
        match String::from_utf8(buf) {
            Ok(text) => self.note(&text),
            Err(err) => Self::put_bytes(&self.err, err.as_bytes()),
        }
    }

    fn put(stream: &Mutex<Sink<'_>>, text: &str) -> io::Result<()> {
        let mut guard = stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tolerate_broken_pipe(guard.put(text))
    }

    fn put_bytes(stream: &Mutex<Sink<'_>>, bytes: &[u8]) -> io::Result<()> {
        let mut guard = stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = guard
            .writer
            .write_all(bytes)
            .and_then(|()| guard.writer.flush());
        tolerate_broken_pipe(result)
    }
}

fn with_newline(text: &str) -> String {
    let mut buf = String::with_capacity(text.len() + 1);
    buf.push_str(text);
    buf.push('\n');
    buf
}

/// A closed pipe is how `| head` ends; it is not a formatting failure.
fn tolerate_broken_pipe(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex as StdMutex};

    use super::*;

    /// A sink that records what reached it, and can be told to fail.
    #[derive(Clone)]
    struct Recorder {
        written: Arc<StdMutex<Vec<u8>>>,
        kind: Option<io::ErrorKind>,
    }

    impl Recorder {
        fn new() -> Self {
            Self {
                written: Arc::new(StdMutex::new(Vec::new())),
                kind: None,
            }
        }

        fn failing(kind: io::ErrorKind) -> Self {
            Self {
                written: Arc::new(StdMutex::new(Vec::new())),
                kind: Some(kind),
            }
        }

        fn text(&self) -> String {
            String::from_utf8(self.written.lock().unwrap().clone()).unwrap()
        }

        fn bytes(&self) -> Vec<u8> {
            self.written.lock().unwrap().clone()
        }
    }

    impl Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if let Some(kind) = self.kind {
                return Err(io::Error::new(kind, "recorder"));
            }
            self.written.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            match self.kind {
                Some(kind) => Err(io::Error::new(kind, "recorder")),
                None => Ok(()),
            }
        }
    }

    const RED: &str = "\u{1b}[31mred\u{1b}[0m";

    #[test]
    fn always_and_never_ignore_the_handle() {
        assert!(ColorChoice::Always.resolve(&io::stdout()));
        assert!(!ColorChoice::Never.resolve(&io::stdout()));
    }

    /// A captured handle is not a terminal, so `auto` has to resolve to off.
    /// Getting this backwards paints escapes into a pipe.
    #[test]
    fn auto_is_off_when_the_handle_is_not_a_terminal() {
        let file = tempfile::tempfile().expect("temp file");
        assert!(!ColorChoice::Auto.resolve(&file));
    }

    #[test]
    fn only_none_is_not_a_listing() {
        assert!(!ListMode::None.is_listing());
        assert!(ListMode::Files.is_listing());
        assert!(ListMode::Different.is_listing());
        assert_eq!(ListMode::default(), ListMode::None);
        assert_eq!(Emit::default(), Emit::Files);
        assert_eq!(MessageFormat::default(), MessageFormat::Human);
        assert_eq!(ColorChoice::default(), ColorChoice::Auto);
    }

    #[test]
    fn product_and_note_reach_their_own_stream() {
        let out = Recorder::new();
        let err = Recorder::new();
        let streams = Streams::plain(out.clone(), err.clone());

        streams.product_line("product").unwrap();
        streams.note_line("commentary").unwrap();

        assert_eq!(out.text(), "product\n");
        assert_eq!(err.text(), "commentary\n");
    }

    #[test]
    fn colour_off_strips_escapes_and_colour_on_keeps_them() {
        let plain = Recorder::new();
        Streams::plain(plain.clone(), io::sink())
            .product(RED)
            .unwrap();
        assert_eq!(plain.text(), "red");

        let painted = Recorder::new();
        Streams::new(
            painted.clone(),
            true,
            io::sink(),
            false,
            ColorChoice::Always,
        )
        .product(RED)
        .unwrap();
        assert_eq!(painted.text(), RED);
    }

    /// The reason `product_bytes` exists: formatted source is written through
    /// untouched, so a `\x1b` inside a string literal survives a run that has
    /// colour switched off.
    #[test]
    fn product_bytes_is_never_stripped() {
        let out = Recorder::new();
        Streams::plain(out.clone(), io::sink())
            .product_bytes(RED.as_bytes())
            .unwrap();
        assert_eq!(out.text(), RED);
    }

    #[test]
    fn product_bytes_carries_bytes_that_are_not_utf8() {
        let out = Recorder::new();
        Streams::plain(out.clone(), io::sink())
            .product_bytes(&[0xff, 0xfe, b'x'])
            .unwrap();
        assert_eq!(out.bytes(), vec![0xff, 0xfe, b'x']);
    }

    #[test]
    fn product_color_reports_the_stdout_sink_not_the_choice() {
        let coloured = Streams::new(io::sink(), true, io::sink(), false, ColorChoice::Auto);
        assert!(coloured.product_color());
        assert_eq!(coloured.color(), ColorChoice::Auto);

        assert!(!Streams::plain(io::sink(), io::sink()).product_color());
    }

    #[test]
    fn paint_note_emits_what_the_painter_wrote() {
        let err = Recorder::new();
        Streams::plain(io::sink(), err.clone())
            .paint_note(|buf| {
                buf.extend_from_slice(b"painted\n");
                Ok(())
            })
            .unwrap();
        assert_eq!(err.text(), "painted\n");
    }

    #[test]
    fn paint_note_forwards_a_painter_error() {
        let err = Streams::plain(io::sink(), io::sink())
            .paint_note(|_| Err(io::Error::other("painter")))
            .unwrap_err();
        assert_eq!(err.to_string(), "painter");
    }

    /// Non-UTF-8 from a painter still has to be emitted rather than dropped:
    /// rustfmt's diff can carry whatever bytes the source held.
    #[test]
    fn paint_note_falls_back_to_bytes() {
        let err = Recorder::new();
        Streams::plain(io::sink(), err.clone())
            .paint_note(|buf| {
                buf.extend_from_slice(&[b'a', 0xff, b'b']);
                Ok(())
            })
            .unwrap();
        assert_eq!(err.bytes(), vec![b'a', 0xff, b'b']);
    }

    /// `rust-formatter --check . | head -1` closes the pipe under us. That is
    /// how `head` ends, not a formatting failure, so it must not become one.
    #[test]
    fn a_broken_pipe_is_not_an_error() {
        let streams = Streams::plain(
            Recorder::failing(io::ErrorKind::BrokenPipe),
            Recorder::failing(io::ErrorKind::BrokenPipe),
        );
        streams.product("x").unwrap();
        streams.product_bytes(b"x").unwrap();
        streams.note("x").unwrap();
        streams
            .paint_note(|buf| {
                let _: () = buf.push(b'x');
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn any_other_io_error_is_reported() {
        let streams = Streams::plain(
            Recorder::failing(io::ErrorKind::PermissionDenied),
            io::sink(),
        );
        assert_eq!(
            streams.product("x").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            streams.product_bytes(b"x").unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn discard_swallows_everything() {
        let streams = Streams::discard();
        streams.product_line("x").unwrap();
        streams.note_line("y").unwrap();
        assert_eq!(streams.color(), ColorChoice::Never);
    }

    /// A poisoned lock must not take the run down: the summary still has to
    /// reach the user after a worker panicked mid-write.
    #[test]
    fn a_poisoned_stream_still_writes() {
        let out = Recorder::new();
        let streams = Streams::plain(out.clone(), io::sink());
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = streams.out.lock().unwrap();
            panic!("worker");
        }));
        std::panic::set_hook(hook);
        assert!(poisoned.is_err());

        assert!(streams.out.is_poisoned());
        assert!(!streams.product_color());
        streams.product("after").unwrap();
        assert_eq!(out.text(), "after");
    }
}
