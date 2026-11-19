//! Predictive local echo that stays stable while the server lags behind.
//!
//! The stock prediction in wezterm draws a typed key into the local copy of
//! the cursor line, but the next screen update from the server replaces that
//! line wholesale. On a slow link that update describes the screen as it was
//! one round trip ago, so fast typing visibly rewinds and replays.
//!
//! This module keeps every unconfirmed key together with the edit state it is
//! expected to produce (the text left of the cursor plus the cursor column).
//! When the server sends the cursor line, its edit state is compared with that
//! history:
//!
//! * It matches the newest prediction: everything is confirmed, show the
//!   server line as is.
//! * It matches an older state: the server is simply behind. Drop the keys it
//!   has caught up with and leave the display alone.
//! * It matches nothing: the prediction was wrong (or the server is mid
//!   redraw). Keep showing the prediction for a short grace period and fall
//!   back to the server state if nothing better arrives.
//!
//! Only the text left of the cursor is compared, so things that shells draw
//! to the right of it (zsh-autosuggestions, right prompts) and colouring
//! (syntax highlighting) do not count as a mismatch. Those only show up once
//! the server has caught up, which is also why lagging server lines are not
//! used for display at all: replaying the pending keys on top of them would
//! drag a stale autosuggestion around.

use std::time::{Duration, Instant};
use termwiz::cell::{Cell, CellAttributes, Intensity};
use termwiz::color::ColorAttribute;
use termwiz::surface::SEQ_ZERO;
use wezterm_term::{Line, StableRowIndex};

/// Shortest time an unrecognised server state may persist before the
/// prediction is abandoned in favour of what the server shows.
pub const MIN_MISMATCH_GRACE: Duration = Duration::from_millis(150);

/// How long an unrecognised server state may persist before the prediction
/// is abandoned. An unrecognised state is often a snapshot taken while the
/// application was half way through redrawing the line; the snapshot that
/// corrects it may only arrive with the next burst of updates, so give it
/// a round trip.
pub fn mismatch_grace(rtt: Duration) -> Duration {
    (rtt + Duration::from_millis(100)).max(MIN_MISMATCH_GRACE)
}

/// Extra time, on top of two round trips, before an unconfirmed key is
/// considered lost and the prediction abandoned.
pub const PENDING_SLACK: Duration = Duration::from_millis(400);

/// The keys we know how to predict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PredKey {
    Char(char),
    Backspace,
    Left,
}

/// Text left of the cursor (trailing blanks trimmed) and the cursor column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditState {
    pub cursor_x: usize,
    pub text: String,
}

impl EditState {
    pub fn capture(line: &Line, cursor_x: usize) -> Self {
        let mut text = String::new();
        for cell in line.visible_cells() {
            if cell.cell_index() >= cursor_x {
                break;
            }
            text.push_str(cell.str());
        }
        let trimmed = text.trim_end_matches(' ').len();
        text.truncate(trimmed);
        Self { cursor_x, text }
    }

    /// Compact form for logging: `x=<col> "<text left of cursor>"`.
    pub fn describe(&self) -> String {
        format!("x={} {:?}", self.cursor_x, self.text)
    }
}

/// Returns true if `c` occupies exactly one column, which is all we predict.
pub fn is_simple_char(c: char) -> bool {
    !c.is_control() && Cell::new(c, CellAttributes::default()).width() == 1
}

/// Dim text, or text in palette colour 8 ("bright black"), is how shells and
/// agents draw text that is not part of the input: zsh-autosuggestions,
/// fish suggestions, Claude Code's placeholder.
fn is_ghost(attrs: &CellAttributes) -> bool {
    attrs.intensity() == Intensity::Half || attrs.foreground() == ColorAttribute::PaletteIndex(8)
}

/// Blank out the run of ghost text starting at `x`. Once the input changes
/// the application replaces it anyway, and leaving it would have it pushed
/// around by the predicted keys.
fn clear_ghost_text(line: &mut Line, x: usize) {
    let mut x = x;
    while line
        .get_cell(x)
        .map(|cell| is_ghost(cell.attrs()))
        .unwrap_or(false)
    {
        line.set_cell(x, Cell::default(), SEQ_ZERO);
        x += 1;
    }
}

/// Apply the effect of `key` to `line`, moving `cursor_x` accordingly.
/// Typed characters are inserted (shells and agent prompts insert rather
/// than overwrite) unless the same character is already under the cursor,
/// which is what typing along an autosuggestion looks like. Otherwise any
/// ghost text at the cursor is removed. Typed characters take the
/// attributes of the character to their left when that is part of the
/// input, so colouring stays continuous.
pub fn apply_key(
    line: &mut Line,
    cursor_x: &mut usize,
    key: PredKey,
    input_start_x: usize,
    cols: usize,
) {
    match key {
        PredKey::Char(c) => {
            let attrs = if *cursor_x > input_start_x {
                line.get_cell(*cursor_x - 1)
                    .map(|cell| cell.attrs().clone())
                    .unwrap_or_default()
            } else {
                CellAttributes::default()
            };
            let mut buf = [0u8; 4];
            let same_under_cursor = line
                .get_cell(*cursor_x)
                .map(|cell| cell.str() == c.encode_utf8(&mut buf))
                .unwrap_or(false);
            let cell = Cell::new(c, attrs);
            if same_under_cursor {
                line.set_cell(*cursor_x, cell, SEQ_ZERO);
            } else {
                clear_ghost_text(line, *cursor_x);
                line.insert_cell(*cursor_x, cell, cols.max(*cursor_x + 1), SEQ_ZERO);
            }
            *cursor_x += 1;
        }
        PredKey::Backspace => {
            if *cursor_x > 0 {
                line.erase_cell(*cursor_x - 1, SEQ_ZERO);
                *cursor_x -= 1;
                clear_ghost_text(line, *cursor_x);
            }
        }
        PredKey::Left => {
            *cursor_x = cursor_x.saturating_sub(1);
        }
    }
}

#[derive(Debug)]
pub struct Pending {
    pub after: EditState,
    pub sent: Instant,
}

/// What to do with a server copy of the predicted row.
#[derive(Debug)]
pub enum Outcome {
    /// Everything predicted has been confirmed (or the prediction is moot):
    /// display the server line and server cursor, and drop the predictor.
    Done,
    /// The server is behind but on track: keep displaying the prediction.
    Keep,
    /// The server state is not one we predicted: keep the current display
    /// for now and re-check after `mismatch_grace`.
    Hold,
}

#[derive(Debug)]
pub struct Predictor {
    pub row: StableRowIndex,
    /// Column where user input starts on this row (just after the prompt).
    pub input_start_x: usize,
    /// Edit state the oldest pending key was applied to.
    pub base: EditState,
    pub pending: Vec<Pending>,
    /// Set once an unpredictable key (Enter, Tab, Ctrl+..., arrows we do
    /// not model) has been sent. No further keys are predicted, and any
    /// unrecognised server state ends the prediction immediately.
    pub barrier: bool,
    pub mismatch_since: Option<Instant>,
    /// The most recent server copy of `row`, restored when we give up.
    pub server_line: Line,
}

impl Predictor {
    pub fn new(row: StableRowIndex, line: &Line, cursor_x: usize, input_start_x: usize) -> Self {
        Self {
            row,
            input_start_x,
            base: EditState::capture(line, cursor_x),
            pending: vec![],
            barrier: false,
            mismatch_since: None,
            server_line: line.clone(),
        }
    }

    /// Where the cursor is once every pending key has been applied.
    pub fn predicted_cursor_x(&self) -> usize {
        self.pending
            .last()
            .map(|p| p.after.cursor_x)
            .unwrap_or(self.base.cursor_x)
    }

    /// Whether `key` can be predicted from the current predicted state.
    pub fn can_predict(&self, key: PredKey, cursor_x: usize, cols: usize) -> bool {
        if self.barrier {
            return false;
        }
        match key {
            PredKey::Char(c) => is_simple_char(c) && cursor_x + 1 < cols,
            // Never predict deleting or moving into the prompt.
            PredKey::Backspace | PredKey::Left => cursor_x > self.input_start_x,
        }
    }

    /// Apply `key` to the displayed `line` and record it as pending.
    pub fn push(
        &mut self,
        key: PredKey,
        line: &mut Line,
        cursor_x: &mut usize,
        cols: usize,
        now: Instant,
    ) {
        apply_key(line, cursor_x, key, self.input_start_x, cols);
        self.pending.push(Pending {
            after: EditState::capture(line, *cursor_x),
            sent: now,
        });
    }

    /// Reconcile with a fresh server copy of `self.row`, given the server's
    /// cursor column on that row.
    pub fn on_server_line(&mut self, line: &Line, server_cursor_x: usize, now: Instant) -> Outcome {
        self.server_line = line.clone();
        let state = EditState::capture(line, server_cursor_x);

        let caught_up = match self.pending.last() {
            None => true,
            Some(last) => last.after == state,
        };

        // Otherwise the server is behind: find how far it has got. Earliest
        // match wins, since the server passes through our states in order.
        let confirmed = if caught_up {
            None
        } else if self.base == state {
            Some(0)
        } else {
            self.pending
                .iter()
                .position(|p| p.after == state)
                .map(|idx| idx + 1)
        };

        let outcome = match confirmed {
            _ if caught_up => Outcome::Done,
            Some(n) => {
                if n > 0 {
                    self.base = self.pending[n - 1].after.clone();
                    self.pending.drain(..n);
                }
                self.mismatch_since = None;
                Outcome::Keep
            }
            None if self.barrier => Outcome::Done,
            None => {
                self.mismatch_since.get_or_insert(now);
                Outcome::Hold
            }
        };
        if log::log_enabled!(log::Level::Trace) {
            log::trace!(
                "server {} -> {:?}{}; history {}",
                state.describe(),
                outcome,
                if self.barrier { " (barrier)" } else { "" },
                self.describe_history()
            );
        }
        outcome
    }

    /// `base` and the state after each pending key, for logging.
    pub fn describe_history(&self) -> String {
        let mut out = self.base.describe();
        for p in &self.pending {
            out.push_str(" > ");
            out.push_str(&p.after.describe());
        }
        out
    }

    /// Why the prediction should be abandoned now, if it should.
    pub fn expiry_reason(&self, now: Instant, rtt: Duration) -> Option<&'static str> {
        if let Some(since) = self.mismatch_since {
            if now.duration_since(since) >= mismatch_grace(rtt) {
                return Some("server state matched no prediction");
            }
        }
        match self.pending.first() {
            Some(oldest) if now.duration_since(oldest.sent) >= rtt * 2 + PENDING_SLACK => {
                Some("keys not confirmed in time")
            }
            Some(_) => None,
            None => Some("nothing pending"),
        }
    }

    /// True once the prediction should be abandoned.
    #[cfg(test)]
    pub fn expired(&self, now: Instant, rtt: Duration) -> bool {
        self.expiry_reason(now, rtt).is_some()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    const COLS: usize = 80;
    const PROMPT: &str = "h:~% ";

    fn line(text: &str) -> Line {
        Line::from_text(text, &CellAttributes::default(), SEQ_ZERO, None)
    }

    fn text(line: &Line) -> String {
        line.as_str().trim_end().to_string()
    }

    /// Drive a predictor the way the client does: type `keys`, then deliver
    /// server snapshots (line text, cursor x) and record what is displayed.
    struct Sim {
        pred: Predictor,
        display: Line,
        cursor_x: usize,
        now: Instant,
    }

    impl Sim {
        fn new(initial: &str) -> Self {
            let l = line(initial);
            let x = initial.chars().count();
            Self {
                pred: Predictor::new(0, &l, x, PROMPT.len()),
                display: l,
                cursor_x: x,
                now: Instant::now(),
            }
        }

        fn key(&mut self, key: PredKey) -> bool {
            if !self.pred.can_predict(key, self.cursor_x, COLS) {
                self.pred.barrier = true;
                return false;
            }
            self.pred
                .push(key, &mut self.display, &mut self.cursor_x, COLS, self.now);
            true
        }

        fn type_str(&mut self, s: &str) {
            for c in s.chars() {
                assert!(self.key(PredKey::Char(c)));
            }
        }

        /// Returns false once the predictor is done.
        fn server(&mut self, server_text: &str, server_x: usize) -> bool {
            let l = line(server_text);
            match self.pred.on_server_line(&l, server_x, self.now) {
                Outcome::Done => {
                    self.display = l;
                    self.cursor_x = server_x;
                    false
                }
                Outcome::Keep | Outcome::Hold => true,
            }
        }

        fn shown(&self) -> (String, usize) {
            (text(&self.display), self.cursor_x)
        }
    }

    #[test]
    fn fast_typing_never_rewinds() {
        let mut sim = Sim::new(PROMPT);
        sim.type_str("git status");
        let want = ("h:~% git status".to_string(), 15);
        assert_eq!(sim.shown(), want);

        // Server echoes arrive one round trip late, one key at a time.
        let typed = "git status";
        for n in 1..typed.len() {
            let server = format!("{PROMPT}{}", &typed[..n]);
            assert!(sim.server(&server, PROMPT.len() + n));
            assert_eq!(sim.shown(), want, "after server echoed {n} keys");
            assert_eq!(sim.pred.pending.len(), typed.len() - n);
        }
        assert!(!sim.server("h:~% git status", 15));
        assert_eq!(sim.shown(), want);
    }

    #[test]
    fn backspacing_never_replays_deleted_text() {
        let mut sim = Sim::new("h:~% git status");
        for _ in 0..5 {
            assert!(sim.key(PredKey::Backspace));
        }
        let want = ("h:~% git s".to_string(), 10);
        assert_eq!(sim.shown(), want);
        for server in [
            "h:~% git statu",
            "h:~% git stat",
            "h:~% git sta",
            "h:~% git st",
        ] {
            assert!(sim.server(server, server.len()));
            assert_eq!(sim.shown(), want, "server at {server:?}");
        }
        assert!(!sim.server("h:~% git s", 10));
    }

    #[test]
    fn suggestion_text_right_of_cursor_is_ignored() {
        // zsh-autosuggestions draws the rest of a history entry after the
        // cursor; that must still count as a match.
        let mut sim = Sim::new(PROMPT);
        sim.type_str("git st");
        // Server has "git" with " status" suggested after the cursor.
        assert!(sim.server("h:~% git status", 8));
        assert!(sim.pred.mismatch_since.is_none());
        assert_eq!(sim.shown(), ("h:~% git st".to_string(), 11));
        assert_eq!(sim.pred.pending.len(), 3);
        // Caught up: now the suggestion is shown.
        assert!(!sim.server("h:~% git status", 11));
        assert_eq!(sim.shown(), ("h:~% git status".to_string(), 11));
    }

    #[test]
    fn backspacing_past_a_suggestion_shows_no_debris() {
        // Server lines while catching up carry the remainder of the
        // autosuggestion right of the cursor; none of it may be shown.
        let mut sim = Sim::new("h:~% git status");
        for _ in 0..10 {
            assert!(sim.key(PredKey::Backspace));
        }
        let want = ("h:~%".to_string(), 5);
        assert_eq!(sim.shown(), want);
        let typed = "git status";
        for n in (1..typed.len()).rev() {
            assert!(sim.server("h:~% git status", PROMPT.len() + n));
            assert_eq!(sim.shown(), want, "server at {n} chars");
        }
        assert!(!sim.server("h:~%", 5));
        assert_eq!(sim.shown(), want);
    }

    fn with_ghost(text: &str, ghost: &str) -> Line {
        let mut l = line(text);
        let mut dim = CellAttributes::default();
        dim.set_intensity(Intensity::Half);
        l.append_line(Line::from_text(ghost, &dim, SEQ_ZERO, None), SEQ_ZERO);
        l
    }

    #[test]
    fn typing_over_a_placeholder_removes_it() {
        // Claude Code shows a dim placeholder where the input goes.
        let l = with_ghost("❯ ", "Try \"fix lint errors\"");
        let mut sim = Sim::new("❯ ");
        sim.display = l.clone();
        sim.pred = Predictor::new(0, &l, 2, 2);
        sim.cursor_x = 2;
        sim.type_str("hi");
        assert_eq!(sim.shown(), ("❯ hi".to_string(), 4));
    }

    #[test]
    fn backspacing_removes_a_suggestion() {
        // zsh after "gi" with "t status" suggested in grey.
        let mut l = line("h:~% gi");
        let mut grey = CellAttributes::default();
        grey.set_foreground(ColorAttribute::PaletteIndex(8));
        l.append_line(Line::from_text("t status", &grey, SEQ_ZERO, None), SEQ_ZERO);
        let mut sim = Sim::new("h:~% gi");
        sim.display = l.clone();
        sim.pred = Predictor::new(0, &l, 7, 5);
        assert!(sim.key(PredKey::Backspace));
        assert_eq!(sim.shown(), ("h:~% g".to_string(), 6));
    }

    #[test]
    fn typing_along_a_suggestion_keeps_it() {
        let l = with_ghost("h:~% gi", "t status");
        let mut sim = Sim::new("h:~% gi");
        sim.display = l.clone();
        sim.pred = Predictor::new(0, &l, 7, 5);
        sim.type_str("t s");
        assert_eq!(sim.shown(), ("h:~% git status".to_string(), 10));
    }

    #[test]
    fn colouring_is_ignored() {
        let mut sim = Sim::new(PROMPT);
        sim.type_str("ls");
        let mut green = CellAttributes::default();
        green.set_foreground(termwiz::color::AnsiColor::Green);
        let mut l = line(PROMPT);
        l.append_line(Line::from_text("l", &green, SEQ_ZERO, None), SEQ_ZERO);
        match sim.pred.on_server_line(&l, 6, sim.now) {
            Outcome::Keep => {}
            other => panic!("expected keep, got {:?}", other),
        }
    }

    #[test]
    fn misprediction_holds_then_expires() {
        // e.g. a key that the remote application does not echo.
        let mut sim = Sim::new(PROMPT);
        sim.type_str("jj");
        assert!(sim.server("h:~% something else", 12));
        assert!(sim.pred.mismatch_since.is_some());
        assert_eq!(sim.shown(), ("h:~% jj".to_string(), 7), "held during grace");
        let rtt = Duration::from_millis(300);
        assert!(!sim.pred.expired(sim.now, rtt));
        // A follow-up snapshot may take a round trip to arrive.
        assert!(!sim.pred.expired(sim.now + MIN_MISMATCH_GRACE, rtt));
        assert!(sim.pred.expired(sim.now + mismatch_grace(rtt), rtt));
        assert_eq!(mismatch_grace(rtt), Duration::from_millis(400));
        assert_eq!(mismatch_grace(Duration::ZERO), MIN_MISMATCH_GRACE);
    }

    #[test]
    fn transient_mismatch_is_forgiven() {
        let mut sim = Sim::new(PROMPT);
        sim.type_str("abc");
        // Snapshot taken mid-redraw: cursor parked at the start of the input.
        assert!(sim.server("h:~% a", 0));
        assert!(sim.pred.mismatch_since.is_some());
        assert!(sim.server("h:~% ab", 7));
        assert!(sim.pred.mismatch_since.is_none());
        assert_eq!(sim.shown(), ("h:~% abc".to_string(), 8));
    }

    #[test]
    fn unconfirmed_keys_expire() {
        let mut sim = Sim::new(PROMPT);
        sim.type_str("a");
        let rtt = Duration::from_millis(300);
        assert!(!sim.pred.expired(sim.now + rtt, rtt));
        assert!(sim.pred.expired(sim.now + rtt * 2 + PENDING_SLACK, rtt));
    }

    #[test]
    fn barrier_stops_prediction_and_ends_on_new_state() {
        let mut sim = Sim::new(PROMPT);
        sim.type_str("ls");
        sim.pred.barrier = true; // Enter was pressed
        assert!(!sim.pred.can_predict(PredKey::Char('x'), sim.cursor_x, COLS));
        assert!(sim.server("h:~% l", 6), "still behind: keep predicting");
        assert_eq!(sim.shown(), ("h:~% ls".to_string(), 7));
        // Output of the command replaced the line: done, no grace period.
        assert!(!sim.server("file1 file2", 0));
    }

    #[test]
    fn does_not_delete_into_the_prompt() {
        let mut sim = Sim::new("h:~% ab");
        assert!(sim.key(PredKey::Backspace));
        assert!(sim.key(PredKey::Backspace));
        assert!(!sim.key(PredKey::Backspace), "would eat the prompt");
        assert_eq!(sim.shown(), (PROMPT.trim_end().to_string(), 5));
    }

    #[test]
    fn editing_mid_line_inserts_and_deletes() {
        let mut sim = Sim::new("h:~% gt status");
        sim.key(PredKey::Left);
        for _ in 0.."t status".len() - 1 {
            sim.key(PredKey::Left);
        }
        assert_eq!(sim.cursor_x, 6);
        sim.key(PredKey::Char('i'));
        assert_eq!(sim.shown(), ("h:~% git status".to_string(), 7));
        sim.key(PredKey::Backspace);
        assert_eq!(sim.shown(), ("h:~% gt status".to_string(), 6));
    }

    #[test]
    fn repeated_states_resolve_without_visible_change() {
        // "a", backspace, "a": states 1 and 3 are identical.
        let mut sim = Sim::new(PROMPT);
        sim.type_str("a");
        sim.key(PredKey::Backspace);
        sim.type_str("a");
        let want = ("h:~% a".to_string(), 6);
        assert_eq!(sim.shown(), want);
        // Server reports "a": earliest match keeps the backspace and "a"
        // pending, which redraws to the same thing.
        assert!(!sim.server("h:~% a", 6), "newest state matches first");
        assert_eq!(sim.shown(), want);
    }
}
