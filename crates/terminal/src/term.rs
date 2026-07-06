//! `Terminal` Entity — the gpui state machine wrapping an alacritty `Term`.
//!
//! Stage 0 carries only a smoke test that pins the alacritty_terminal 0.24
//! API surface manox relies on. If upstream visibility shifts, this fails
//! before any real logic lands. The full Entity (PTY spawn, `write_pty_output`,
//! `sync`, `last_content`) is implemented in stage 1.

#[cfg(test)]
mod smoke {
    use alacritty_terminal::event::{Event, EventListener};
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};

    /// No-op listener — stage 1 replaces this with `ManoxListener`, which
    /// forwards `Event` variants to `TerminalEvent` over an `async_channel`.
    struct NullListener;
    impl EventListener for NullListener {
        fn send_event(&self, _event: Event) {}
    }

    struct MinDims {
        cols: usize,
        rows: usize,
    }
    impl Dimensions for MinDims {
        fn total_lines(&self) -> usize {
            self.rows
        }
        fn screen_lines(&self) -> usize {
            self.rows
        }
        fn columns(&self) -> usize {
            self.cols
        }
    }

    #[test]
    fn term_api_surface() {
        let cfg = Config::default();
        let dims = MinDims {
            cols: 80,
            rows: 24,
        };
        let mut term: Term<NullListener> = Term::new(cfg, &dims, NullListener);

        // Renderable snapshot — the read path the GPUI layer will use.
        let _content = term.renderable_content();
        let _mode = term.mode();
        let _grid = term.grid();
        let _ = term.selection_to_string();
        term.toggle_vi_mode();

        // `Term` implements `Handler` — feed bytes through the vte processor.
        let mut processor: Processor<StdSyncHandler> = Processor::new();
        for byte in b"hello\r\n" {
            processor.advance(&mut term, *byte);
        }
    }
}
