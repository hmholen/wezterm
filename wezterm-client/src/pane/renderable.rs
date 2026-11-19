use crate::domain::ClientInner;
use crate::pane::clientpane::ClientPane;
use crate::pane::predict::{self, Outcome, PredKey, Predictor, PENDING_SLACK};
use anyhow::anyhow;
use codec::*;
use config::{configuration, ConfigHandle};
use lru::LruCache;
use mux::pane::PaneId;
use mux::renderable::{RenderableDimensions, StableCursorPosition};
use mux::Mux;
use promise::BrokenPromise;
use rangeset::*;
use ratelim::RateLimiter;
use std::cell::RefCell;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use termwiz::cell::CellAttributes;
use termwiz::color::AnsiColor;
use termwiz::image::{ImageCell, ImageData};
use termwiz::surface::{CursorVisibility, SequenceNo, SEQ_ZERO};
use url::Url;
use wezterm_term::{KeyCode, KeyModifiers, Line, StableRowIndex};

const MAX_POLL_INTERVAL: Duration = Duration::from_secs(30);
const BASE_POLL_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug)]
enum LineEntry {
    // Up to date wrt. server and has been rendered at least once
    Line(Line),
    // Currently being downloaded from the server
    Fetching(Instant),
    // We have a version of the line locally and are treating it
    // as needing rendering because we are also in the process of
    // downloading a newer version from the server
    LineAndFetching(Line, Instant),
    // We have a local copy but it is stale and will need to be
    // fetched again
    Stale(Line),
}

impl LineEntry {
    fn kind(&self) -> (&'static str, Option<Instant>) {
        match self {
            Self::Line(_) => ("Line", None),
            Self::Fetching(since) => ("Fetching", Some(*since)),
            Self::LineAndFetching(_, since) => ("LineAndFetching", Some(*since)),
            Self::Stale(_) => ("Stale", None),
        }
    }
}

pub struct RenderableInner {
    pub client: Arc<ClientInner>,
    remote_pane_id: PaneId,
    local_pane_id: PaneId,
    last_poll: Instant,
    pub dead: bool,
    poll_in_progress: AtomicBool,
    poll_interval: Duration,

    cursor_position: StableCursorPosition,
    pub dimensions: RenderableDimensions,

    lines: LruCache<StableRowIndex, LineEntry>,
    pub title: String,
    pub working_dir: Option<Url>,
    pub seqno: SequenceNo,

    fetch_limiter: RateLimiter,

    last_send_time: Instant,
    pub last_recv_time: Instant,
    last_late_dirty: Instant,
    last_input_rtt: u64,

    pub input_serial: InputSerial,

    /// Local echo that has not been confirmed by the server yet.
    predictor: Option<Predictor>,
    /// The cursor as the server last reported it; `cursor_position` is
    /// what we display, which runs ahead of it while predicting.
    server_cursor: StableCursorPosition,
    /// Row the server cursor is on, and the leftmost column it has been
    /// seen at on that row: where user input starts (after the prompt).
    input_row: StableRowIndex,
    input_start_x: usize,
}

pub struct RenderableState {
    pub inner: RefCell<RenderableInner>,
}

impl RenderableInner {
    pub fn new(
        client: &Arc<ClientInner>,
        remote_pane_id: PaneId,
        local_pane_id: PaneId,
        dimensions: RenderableDimensions,
        title: &str,
        fetch_limiter: RateLimiter,
    ) -> Self {
        let now = Instant::now();

        Self {
            client: Arc::clone(client),
            remote_pane_id,
            local_pane_id,
            last_poll: now,
            dead: false,
            poll_in_progress: AtomicBool::new(false),
            poll_interval: BASE_POLL_INTERVAL,
            cursor_position: StableCursorPosition::default(),
            dimensions,
            lines: LruCache::new(
                NonZeroUsize::new(configuration().scrollback_lines.max(128)).unwrap(),
            ),
            title: title.to_string(),
            working_dir: None,
            fetch_limiter,
            last_send_time: now,
            last_recv_time: now,
            last_late_dirty: now,
            last_input_rtt: 0,
            input_serial: InputSerial::empty(),
            seqno: SEQ_ZERO,
            predictor: None,
            server_cursor: StableCursorPosition::default(),
            input_row: 0,
            input_start_x: 0,
        }
    }

    /// Returns true if we think we should display the laggy connection
    /// indicator.  If we're past our poll interval and more recently
    /// tried to send something than receive something, the UI is worth
    /// showing.
    pub fn is_tardy(&self) -> bool {
        let elapsed = self.last_recv_time.elapsed();
        if elapsed > self.poll_interval.max(Duration::from_secs(3)) {
            self.last_send_time > self.last_recv_time
        } else {
            false
        }
    }

    /// Predictive echo can be noisy when the link is working well,
    /// so we only employ it when it looks like the latency is high.
    fn should_predict(&self) -> bool {
        self.client
            .local_echo_threshold_ms
            .map(|thresh| self.last_input_rtt >= thresh)
            .unwrap_or(false)
    }

    fn prediction_rtt(&self) -> Duration {
        Duration::from_millis(self.last_input_rtt)
    }

    /// An input we cannot model was sent: stop predicting until the server
    /// has caught up with it.
    fn prediction_barrier(&mut self) {
        if let Some(pred) = self.predictor.as_mut() {
            pred.barrier = true;
        }
    }

    /// Abandon the prediction and show the most recent server state.
    fn clear_prediction(&mut self) {
        if let Some(pred) = self.predictor.take() {
            self.lines.put(pred.row, LineEntry::Line(pred.server_line));
            self.cursor_position = self.server_cursor;
        }
    }

    /// Called from a timer: abandon a prediction that the server has not
    /// confirmed in time. Returns true if the display changed.
    fn expire_prediction(&mut self) -> bool {
        let rtt = self.prediction_rtt();
        let reason = self
            .predictor
            .as_ref()
            .and_then(|pred| pred.expiry_reason(Instant::now(), rtt));
        match reason {
            Some(reason) => {
                if let Some(pred) = &self.predictor {
                    log::trace!(
                        target: "wezterm_client::pane::predict",
                        "expired ({}, rtt {:?}); history {}",
                        reason,
                        rtt,
                        pred.describe_history()
                    );
                }
                self.clear_prediction();
                true
            }
            None => false,
        }
    }

    fn schedule_prediction_check(&self, delay: Duration) {
        let local_pane_id = self.local_pane_id;
        promise::spawn::spawn(async move {
            smol::Timer::after(delay + Duration::from_millis(1)).await;
            let mux = Mux::get();
            if let Some(pane) = mux.get_pane(local_pane_id) {
                if let Some(client_pane) = pane.downcast_ref::<ClientPane>() {
                    let expired = {
                        let renderable = client_pane.renderable.lock();
                        let mut inner = renderable.inner.borrow_mut();
                        inner.expire_prediction()
                    };
                    if expired {
                        mux.notify(mux::MuxNotification::PaneOutput(local_pane_id));
                    }
                }
            }
        })
        .detach();
    }

    /// Draw the predicted effect of `key` into the cursor line. Returns
    /// false if the key cannot be predicted, which also stops prediction
    /// until the server catches up.
    fn predict_key(&mut self, key: PredKey) -> bool {
        let row = self.cursor_position.y;
        let cols = self.dimensions.cols;
        let x = self.cursor_position.x;

        if self.cursor_position.visibility != CursorVisibility::Visible {
            // Full screen applications that hide the cursor draw their
            // own; we would be predicting into the wrong place.
            self.prediction_barrier();
            return false;
        }
        if let Some(pred) = &self.predictor {
            if pred.row != row || !pred.can_predict(key, x, cols) {
                self.prediction_barrier();
                return false;
            }
        }

        let (mut line, fetching) = match self.lines.pop(&row) {
            Some(LineEntry::Line(line)) | Some(LineEntry::Stale(line)) => (line, None),
            Some(LineEntry::LineAndFetching(line, since)) => (line, Some(since)),
            Some(entry) => {
                self.lines.put(row, entry);
                self.prediction_barrier();
                return false;
            }
            None => {
                self.prediction_barrier();
                return false;
            }
        };

        let input_start_x = match &self.predictor {
            Some(pred) => pred.input_start_x,
            None if self.input_row == row => self.input_start_x.min(x),
            None => x,
        };

        // Don't echo after what looks like a prompt for a secret.
        // Applications that read one do not echo it, so a prediction would
        // show the secret until it expires. Only the prompt is checked, not
        // what the user typed after it.
        let prompt = predict::EditState::capture(&line, input_start_x)
            .text
            .to_lowercase();
        let mut ok = ![
            "password",
            "passwd",
            "passphrase",
            "passcode",
            "pin:",
            "pin for",
            "token:",
            "secret",
        ]
        .iter()
        .any(|word| prompt.contains(word));
        if ok && self.predictor.is_none() {
            let pred = Predictor::new(row, &line, x, input_start_x);
            ok = pred.can_predict(key, x, cols);
            if ok {
                self.predictor = Some(pred);
            }
        }

        if ok {
            let pred = self.predictor.as_mut().expect("predictor was just ensured");
            let mut new_x = x;
            pred.push(key, &mut line, &mut new_x, cols, Instant::now());
            self.cursor_position.x = new_x;
        } else {
            self.prediction_barrier();
        }

        let entry = match fetching {
            Some(since) => LineEntry::LineAndFetching(line, since),
            None => LineEntry::Line(line),
        };
        self.lines.put(row, entry);
        ok
    }

    /// Based on a keypress, apply a "prediction" of what the terminal
    /// content will look like once we receive the response from the
    /// remote system.  The prediction helps to reduce perceived latency
    /// when a user is typing at any reasonable velocity.
    pub fn predict_from_key_event(&mut self, key: KeyCode, mods: KeyModifiers) {
        if key.is_modifier() {
            return;
        }
        let mods = mods.remove_positional_mods();
        let plain = mods == KeyModifiers::NONE || mods == KeyModifiers::SHIFT;
        let pred_key = match key {
            KeyCode::Char(c) if plain && predict::is_simple_char(c) => Some(PredKey::Char(c)),
            KeyCode::Backspace if mods == KeyModifiers::NONE => Some(PredKey::Backspace),
            KeyCode::LeftArrow if mods == KeyModifiers::NONE => Some(PredKey::Left),
            _ => None,
        };
        match pred_key {
            Some(key) if self.should_predict() => {
                if self.predict_key(key) {
                    self.schedule_prediction_check(self.prediction_rtt() * 2 + PENDING_SLACK);
                }
            }
            _ => self.prediction_barrier(),
        }
    }

    pub fn predict_from_paste(&mut self, text: &str) {
        let simple = !text.is_empty()
            && text.chars().count() <= self.dimensions.cols
            && text.chars().all(predict::is_simple_char);
        if !simple || !self.should_predict() {
            self.prediction_barrier();
            return;
        }
        let mut predicted = false;
        for c in text.chars() {
            if !self.predict_key(PredKey::Char(c)) {
                break;
            }
            predicted = true;
        }
        if predicted {
            self.schedule_prediction_check(self.prediction_rtt() * 2 + PENDING_SLACK);
        }
    }

    pub fn update_last_send(&mut self) {
        self.last_send_time = Instant::now();
        self.poll_interval = BASE_POLL_INTERVAL;
    }

    pub fn apply_changes_to_surface(
        &mut self,
        delta: GetPaneRenderChangesResponse,
        bonus_lines: Vec<(StableRowIndex, Line)>,
    ) {
        log::trace!(
            "apply_changes_to_surface local={} remote={}",
            self.local_pane_id,
            self.remote_pane_id
        );
        let now = Instant::now();
        self.poll_interval = BASE_POLL_INTERVAL;
        self.last_recv_time = now;

        let mut dirty = RangeSet::new();
        for r in delta.dirty_lines {
            dirty.add_range(r.clone());
        }
        if delta.cursor_position != self.cursor_position {
            dirty.add(self.cursor_position.y);
            // But note that the server may have sent this in bonus_lines;
            // we'll address that below
            dirty.add(delta.cursor_position.y);
        }

        // Keep track of the approximate round trip time by recording how
        // long it took for this response to come back
        if let Some(serial) = delta.input_serial {
            self.last_input_rtt = serial.elapsed_millis();
        }

        // When it comes to updating the cursor position, if the update was tagged
        // with keyboard input, we'll only take the position if the update comes from
        // the most recent key event.  This helps to prevent the cursor wiggling if the
        // user is typing more than one character per roundtrip interval--the wiggle
        // manifests because we may have already predicted a local cursor move forwards
        // by one character, and we may receive the response to the prior update after
        // we have rendered that, and then shortly receive the most recent response.
        // The result of that is that the cursor moves right one, left one and then
        // finally right one in quick succession.
        // If the delta was not from an input event then we trust it; this is most
        // like due to a unilateral movement by the application on the other end.
        self.server_cursor = delta.cursor_position;
        if delta.cursor_position.y != self.input_row {
            self.input_row = delta.cursor_position.y;
            self.input_start_x = delta.cursor_position.x;
        } else {
            self.input_start_x = self.input_start_x.min(delta.cursor_position.x);
        }

        // A prediction is only meaningful while the server cursor is still
        // on the row we predicted into; otherwise the input was submitted or
        // output arrived, so show the server's view.
        if let Some(pred) = &self.predictor {
            if pred.row != delta.cursor_position.y || delta.dimensions.cols != self.dimensions.cols
            {
                self.clear_prediction();
            }
        }
        let was_predicting = self.predictor.is_some();

        if !was_predicting
            && (delta.input_serial.is_none()
                || delta.input_serial.unwrap_or(InputSerial::empty()) >= self.input_serial)
        {
            self.cursor_position = delta.cursor_position;
        }
        self.dimensions = delta.dimensions;
        self.title = delta.title;
        self.working_dir = delta.working_dir.map(Into::into);
        log::trace!(
            "server says: seqno from {} -> {} for local_pane_id={}",
            self.seqno,
            delta.seqno,
            self.local_pane_id
        );
        self.seqno = delta.seqno;

        let config = configuration();
        for (stable_row, line) in bonus_lines {
            log::trace!("bonus line {} seqno={}", stable_row, line.current_seqno());
            let line = match self.predictor.as_mut() {
                Some(pred) if pred.row == stable_row => {
                    match pred.on_server_line(&line, delta.cursor_position.x, now) {
                        Outcome::Done => {
                            self.predictor = None;
                            Some(line)
                        }
                        // Keep showing the prediction.
                        Outcome::Keep => None,
                        Outcome::Hold => {
                            self.schedule_prediction_check(predict::mismatch_grace(
                                self.prediction_rtt(),
                            ));
                            None
                        }
                    }
                }
                _ => Some(line),
            };
            if let Some(line) = line {
                self.put_line(stable_row, line, &config, None);
            }
            dirty.remove(stable_row);
        }

        match &self.predictor {
            Some(pred) => {
                self.cursor_position = StableCursorPosition {
                    x: pred.predicted_cursor_x(),
                    y: pred.row,
                    ..delta.cursor_position
                };
            }
            None if was_predicting => self.cursor_position = delta.cursor_position,
            None => {}
        }

        log::trace!(
            "apply_changes_to_surface: Generate PaneOutput event for local={}",
            self.local_pane_id
        );
        Mux::get().notify(mux::MuxNotification::PaneOutput(self.local_pane_id));

        let mut to_fetch = RangeSet::new();
        log::trace!("dirty as of seq {} -> {:?}", delta.seqno, dirty);
        for r in dirty.iter() {
            for stable_row in r.clone() {
                // If a line is in the (probable) viewport region,
                // then we'll likely want to fetch it.
                // If it is outside that region, remove it from our cache
                // so that we'll fetch it on demand later.
                let fetchable = stable_row >= delta.dimensions.physical_top;
                let prior = self.lines.pop(&stable_row);
                let prior_kind = prior.as_ref().map(|e| e.kind());
                if !fetchable {
                    log::trace!("make {} stale bcos not fetchable", stable_row);
                    self.make_stale(stable_row);
                    continue;
                }
                to_fetch.add(stable_row);
                let entry = match prior {
                    Some(LineEntry::Fetching(_)) | None => LineEntry::Fetching(now),
                    Some(LineEntry::LineAndFetching(old, ..))
                    | Some(LineEntry::Stale(old))
                    | Some(LineEntry::Line(old)) => LineEntry::LineAndFetching(old, now),
                };
                log::trace!(
                    "row {} {:?} -> {:?} due to dirty and IN viewport",
                    stable_row,
                    prior_kind,
                    entry.kind()
                );
                self.lines.put(stable_row, entry);
            }
        }
        if !to_fetch.is_empty() {
            if self.fetch_limiter.non_blocking_admittance_check(1) {
                self.schedule_fetch_lines(to_fetch, now);
            } else {
                log::warn!(
                    "exceeded fetch throttle, drop {:?} and mark stale",
                    to_fetch
                );
                for r in to_fetch.iter() {
                    for stable_row in r.clone() {
                        self.make_stale(stable_row);
                    }
                }
            }
        }
    }

    pub fn make_all_stale(&mut self) {
        self.predictor = None;
        let mut lines = LruCache::unbounded();
        while let Some((stable_row, entry)) = self.lines.pop_lru() {
            let entry = match entry {
                LineEntry::Stale(old) | LineEntry::Line(old) => LineEntry::Stale(old),
                entry => entry,
            };
            lines.put(stable_row, entry);
        }
        self.lines = lines;
    }

    fn make_stale(&mut self, stable_row: StableRowIndex) {
        match self.lines.pop(&stable_row) {
            Some(LineEntry::Stale(old))
            | Some(LineEntry::Line(old))
            | Some(LineEntry::LineAndFetching(old, _)) => {
                self.lines.put(stable_row, LineEntry::Stale(old));
            }
            Some(LineEntry::Fetching(_)) | None => {}
        }
    }

    fn put_line(
        &mut self,
        stable_row: StableRowIndex,
        mut line: Line,
        config: &ConfigHandle,
        fetch_start: Option<Instant>,
    ) {
        line.scan_and_create_hyperlinks(&config.hyperlink_rules);

        if fetch_start.is_some() && self.predictor.as_ref().map(|p| p.row) == Some(stable_row) {
            // Keep showing the prediction for this row; the cursor line
            // that comes with every server update drives reconciliation.
            match self.lines.pop(&stable_row) {
                Some(LineEntry::LineAndFetching(old, _)) => {
                    self.lines.put(stable_row, LineEntry::Line(old));
                    return;
                }
                Some(LineEntry::Fetching(then)) => {
                    // Nothing to keep; fall through and take the server copy.
                    self.lines.put(stable_row, LineEntry::Fetching(then));
                }
                Some(e) => {
                    self.lines.put(stable_row, e);
                    return;
                }
                None => return,
            }
        }

        let entry = if let Some(fetch_start) = fetch_start {
            // If we're completing a fetch, only replace entries that were
            // set to fetching as part of our fetch.  If they are now longer
            // tagged that way, then someone came along after us and changed
            // the state, so we should leave it alone

            match self.lines.pop(&stable_row) {
                Some(LineEntry::LineAndFetching(_, then)) | Some(LineEntry::Fetching(then))
                    if fetch_start == then =>
                {
                    log::trace!(
                        "row {} fetch done -> Line seq={} vs self.seq={}",
                        stable_row,
                        line.current_seqno(),
                        self.seqno
                    );
                    line.update_last_change_seqno(self.seqno);
                    LineEntry::Line(line)
                }
                Some(e) => {
                    // It changed since we started: leave it alone!
                    log::trace!(
                        "row {} {:?} changed since fetch started at {:?}, so leave it be",
                        stable_row,
                        e.kind(),
                        fetch_start
                    );
                    self.lines.put(stable_row, e);
                    return;
                }
                None => return,
            }
        } else {
            LineEntry::Line(line)
        };
        self.lines.put(stable_row, entry);
    }

    fn schedule_fetch_lines(&mut self, to_fetch: RangeSet<StableRowIndex>, now: Instant) {
        if to_fetch.is_empty() || self.dead {
            return;
        }

        let local_pane_id = self.local_pane_id;
        log::trace!(
            "will fetch lines {:?} for remote tab id {} at {:?}",
            to_fetch,
            self.remote_pane_id,
            now,
        );

        let client = Arc::clone(&self.client);
        let remote_pane_id = self.remote_pane_id;

        promise::spawn::spawn(async move {
            let result = client
                .client
                .get_lines(GetLines {
                    pane_id: remote_pane_id,
                    lines: to_fetch.clone().into(),
                })
                .await;

            let result = match result {
                Ok(result) => {
                    let lines =
                        hydrate_lines(Arc::clone(&client), remote_pane_id, result.lines).await;
                    Ok(lines)
                }
                Err(err) => Err(err),
            };
            Self::apply_lines(local_pane_id, result, to_fetch, now)
        })
        .detach();
    }

    fn apply_lines(
        local_pane_id: PaneId,
        result: anyhow::Result<Vec<(StableRowIndex, Line)>>,
        to_fetch: RangeSet<StableRowIndex>,
        now: Instant,
    ) -> anyhow::Result<()> {
        let mux = Mux::get();
        let pane = mux
            .get_pane(local_pane_id)
            .ok_or_else(|| anyhow!("no such tab {}", local_pane_id))?;
        if let Some(client_tab) = pane.downcast_ref::<ClientPane>() {
            let renderable = client_tab.renderable.lock();
            let mut inner = renderable.inner.borrow_mut();

            match result {
                Ok(lines) => {
                    let config = configuration();

                    log::trace!("fetch complete for {:?} at {:?}", to_fetch, now);
                    for (stable_row, line) in lines.into_iter() {
                        inner.put_line(stable_row, line, &config, Some(now));
                    }
                }
                Err(err) => {
                    log::error!("get_lines failed: {}", err);
                    for r in to_fetch.iter() {
                        for stable_row in r.clone() {
                            let entry = match inner.lines.pop(&stable_row) {
                                Some(LineEntry::Fetching(then)) if then == now => {
                                    // leave it popped
                                    continue;
                                }
                                Some(LineEntry::LineAndFetching(line, then)) if then == now => {
                                    // revert to just a line
                                    LineEntry::Line(line)
                                }
                                Some(entry) => entry,
                                None => continue,
                            };
                            inner.lines.put(stable_row, entry);
                        }
                    }
                }
            }
        }
        log::trace!(
            "Generate PaneOutput event for local_pane_id={}",
            local_pane_id
        );
        mux.notify(mux::MuxNotification::PaneOutput(local_pane_id));
        Ok(())
    }

    fn poll(&mut self) -> anyhow::Result<()> {
        if self.poll_in_progress.load(Ordering::SeqCst) {
            // We have a poll in progress
            return Ok(());
        }

        if self.last_poll.elapsed() < self.poll_interval {
            return Ok(());
        }

        let interval = self.poll_interval;
        let interval = (interval + interval).min(MAX_POLL_INTERVAL);
        self.poll_interval = interval;

        self.last_poll = Instant::now();
        self.poll_in_progress.store(true, Ordering::SeqCst);
        let remote_pane_id = self.remote_pane_id;
        let local_pane_id = self.local_pane_id;
        let client = Arc::clone(&self.client);
        promise::spawn::spawn(async move {
            let alive = match client
                .client
                .get_pane_render_changes(GetPaneRenderChanges {
                    pane_id: remote_pane_id,
                })
                .await
            {
                Ok(resp) => resp.is_alive,
                // if we got a timeout on a reconnectable, don't
                // consider the tab to be dead; that helps to
                // avoid having a tab get shuffled around
                Err(_) => client.client.is_reconnectable,
            };

            let mux = Mux::get();
            let tab = mux
                .get_pane(local_pane_id)
                .ok_or_else(|| anyhow!("no such tab {}", local_pane_id))?;
            if let Some(client_tab) = tab.downcast_ref::<ClientPane>() {
                let renderable = client_tab.renderable.lock();
                let mut inner = renderable.inner.borrow_mut();

                inner.dead = !alive;
                inner.last_recv_time = Instant::now();
                inner.poll_in_progress.store(false, Ordering::SeqCst);
            }
            Ok::<(), anyhow::Error>(())
        })
        .detach();
        Ok(())
    }
}

lazy_static::lazy_static! {
    static ref IMAGE_LRU: Mutex<LruCache<[u8;32], Arc<ImageData>>> = Mutex::new(LruCache::new(NonZeroUsize::new(128).unwrap()));
}

pub(crate) async fn hydrate_lines(
    client: Arc<ClientInner>,
    pane_id: PaneId,
    serialized_lines: SerializedLines,
) -> Vec<(StableRowIndex, Line)> {
    let (lines, image_cells) = serialized_lines.extract_data();

    if image_cells.is_empty() {
        return lines;
    }

    let mut requests = HashMap::new();
    let mut data_by_hash = HashMap::new();
    for im in &image_cells {
        if let Some(data) = IMAGE_LRU.lock().unwrap().get(&im.data_hash) {
            data_by_hash.insert(im.data_hash, Arc::clone(data));
        } else {
            requests
                .entry(&im.data_hash)
                .or_insert_with(|| GetImageCell {
                    pane_id,
                    line_idx: im.line_idx,
                    cell_idx: im.cell_idx,
                    data_hash: im.data_hash,
                });
        }
    }

    for (_, request) in requests {
        match client.client.get_image_cell(request).await {
            Ok(GetImageCellResponse {
                data: Some(data), ..
            }) => {
                IMAGE_LRU
                    .lock()
                    .unwrap()
                    .put(data.hash(), Arc::clone(&data));
                data_by_hash.insert(data.hash(), data);
            }
            Ok(GetImageCellResponse { data: None, .. }) => {
                log::error!("no image data!");
            }

            Err(err) => {
                log::error!("failed to retrieve image {err:#}");
            }
        }
    }

    let mut line_by_idx = HashMap::new();
    for (line_idx, line) in lines {
        line_by_idx.insert(line_idx, line);
    }

    for im in image_cells {
        if let Some(data) = data_by_hash.get(&im.data_hash) {
            if let Some(line) = line_by_idx.get_mut(&im.line_idx) {
                if let Some(cell) = line.cells_mut_for_attr_changes_only().get_mut(im.cell_idx) {
                    cell.attrs_mut()
                        .attach_image(Box::new(ImageCell::with_z_index(
                            im.top_left,
                            im.bottom_right,
                            Arc::clone(data),
                            im.z_index,
                            im.padding_left,
                            im.padding_top,
                            im.padding_right,
                            im.padding_bottom,
                            im.image_id,
                            im.placement_id,
                        )));
                }
            }
        }
    }

    line_by_idx.into_iter().collect()
}

impl RenderableState {
    pub fn get_cursor_position(&self) -> StableCursorPosition {
        self.inner.borrow().cursor_position
    }

    pub fn get_lines(&self, lines: Range<StableRowIndex>) -> (StableRowIndex, Vec<Line>) {
        let mut inner = self.inner.borrow_mut();
        let mut result = vec![];
        let mut to_fetch = RangeSet::new();
        let now = Instant::now();

        for idx in lines.clone() {
            let entry = match inner.lines.pop(&idx) {
                Some(LineEntry::Line(line)) => {
                    result.push(line.clone());
                    if line.changed_since(inner.seqno) {
                        to_fetch.add(idx);
                        LineEntry::Stale(line)
                    } else {
                        LineEntry::Line(line)
                    }
                }
                Some(LineEntry::LineAndFetching(line, then)) => {
                    result.push(line.clone());
                    LineEntry::LineAndFetching(line, then)
                }
                Some(LineEntry::Fetching(then)) => {
                    result.push(Line::with_width(inner.dimensions.cols, SEQ_ZERO));
                    LineEntry::Fetching(then)
                }
                Some(LineEntry::Stale(line)) => {
                    result.push(line.clone());
                    to_fetch.add(idx);
                    LineEntry::LineAndFetching(line, now)
                }
                None => {
                    result.push(Line::with_width(inner.dimensions.cols, SEQ_ZERO));
                    to_fetch.add(idx);
                    LineEntry::Fetching(now)
                }
            };

            if inner.client.overlay_lag_indicator && idx == inner.dimensions.physical_top {
                if inner.is_tardy() {
                    let status = format!(
                        "wezterm: {:.0?}⏳since last response",
                        inner.last_recv_time.elapsed()
                    );
                    // Right align it in the tab
                    let col = inner
                        .dimensions
                        .cols
                        .saturating_sub(wezterm_term::unicode_column_width(&status, None));

                    let mut attr = CellAttributes::default();
                    attr.set_foreground(AnsiColor::White);
                    attr.set_background(AnsiColor::Blue);

                    result
                        .last_mut()
                        .unwrap()
                        .overlay_text_with_attribute(col, &status, attr, SEQ_ZERO);
                }
            }

            inner.lines.put(idx, entry);
        }

        log::trace!(
            "get_lines: {:?}, num result lines={}, will fetch {:?}",
            lines,
            result.len(),
            to_fetch
        );

        inner.schedule_fetch_lines(to_fetch, now);
        (lines.start, result)
    }

    pub fn get_current_seqno(&self) -> SequenceNo {
        self.inner.borrow().seqno
    }

    pub fn get_changed_since(
        &self,
        lines: Range<StableRowIndex>,
        seqno: SequenceNo,
    ) -> RangeSet<StableRowIndex> {
        let mut inner = self.inner.borrow_mut();
        if let Err(err) = inner.poll() {
            // We allow for BrokenPromise here for now; for a TLS backed
            // session it indicates that we'll retry.  For a local unix
            // domain session it is terminal... but we will detect that
            // terminal condition elsewhere
            if let Err(err) = err.downcast::<BrokenPromise>() {
                log::error!("remote tab poll failed: {}, marking as dead", err);
                inner.dead = true;
            }
        }

        let mut result = RangeSet::new();
        for r in lines {
            match inner.lines.get(&r) {
                None => {
                    result.add(r);
                }
                Some(
                    LineEntry::Line(line)
                    | LineEntry::Stale(line)
                    | LineEntry::LineAndFetching(line, _),
                ) if line.changed_since(seqno) => {
                    result.add(r);
                }
                _ => {}
            }
        }

        // If we're behind receiving an update, invalidate the top row so
        // that the indicator will update in a more timely fashion
        if inner.is_tardy() {
            // ... but take care to avoid always reporting it as dirty, so
            // that we don't end up busy looping just to repaint it
            if inner.last_late_dirty.elapsed() >= Duration::from_secs(1) {
                result.add(inner.dimensions.physical_top);
                inner.last_late_dirty = Instant::now();
            }
        }

        if !result.is_empty() {
            log::trace!("get_changed_since: {} -> {:?}", seqno, result);
        }

        result
    }

    pub fn get_dimensions(&self) -> RenderableDimensions {
        self.inner.borrow().dimensions
    }
}
