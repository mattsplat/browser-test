//! Time-travel replay: record what crosses the host boundary into a page's pack, then run
//! the page again in a fresh pack and feed it the same inputs, so it ends in the same state.
//!
//! Principle 4 routes every input through the host, so the host is where it's recorded:
//! - **Steps**, in order: events dispatched to the page's listeners (type, target, details),
//!   timer batches (which timers fired, with their arguments), and console or AI eval.
//! - **The tape**: every nondeterministic value the pack read, in order: `Date.now()`
//!   (`Host::now`), `Math.random`, and the interpreters' WASI clock and random bytes (now
//!   answered by the host too, through `Host::clock` and `Host::random_u64`).
//! - **The start**: the page's HTML, its site CSS, and the program it ran: Dart's wasm, or the
//!   interpreter's scripts with every module import that was fetched.
//!
//! Replaying parses the same HTML, starts the pack on the recorded program (nothing is
//! fetched), answers its nondeterministic reads from the tape, and applies the steps. A step
//! that can't apply (its target or timer isn't there) or a read that doesn't match the tape
//! marks the replay as diverged. After the last step the page is live again.
//!
//! A recording starts with a page load (`arm`, then reload), because a page's state from
//! before can't be replayed. Recordings stay in memory (the last few), never on disk, and
//! AI access can't read them: they hold what the page received, including console input.

use std::collections::VecDeque;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use blitz_dom::{BaseDocument, DocumentConfig, NodeId};
use blitz_html::HtmlDocument;

use crate::host::EventDetails;
use crate::interpreter::Script;
use crate::page_document::PackDocument;

/// What a nondeterministic read was, so a replay can tell when it has gone off the tape.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Read {
    Now,
    Random,
    Clock,
    Bits,
    /// Whether an uncaught error paused the page (pause on exceptions).
    Pause,
    /// `document.cookie` (text: the value is an index into `Recording::texts`).
    Cookie,
    /// A read of `localStorage` or `sessionStorage`, as JSON (text, like `Cookie`).
    Storage,
    /// What the page read from its tab: `history.state` and `history.length` (text).
    State,
    /// The window's size, pixel ratio or scroll position (`innerWidth`, `scrollY`...).
    Viewport,
    /// A form field's `value` or `checked` (text): the user changes them by typing and
    /// clicking, Blitz's own default actions, which a replay doesn't repeat.
    Field,
    /// An element's box (`getBoundingClientRect`, as JSON text): layout depends on the window
    /// and the fonts.
    Layout,
    /// An IndexedDB operation's outcome (JSON text): another tab may change the databases.
    Idb,
    /// A canvas's pixels as read back (`toDataURL`): text drawn depends on the system's fonts.
    Pixels,
}

impl Read {
    /// Whether the tape value is an index into the recording's texts.
    fn is_text(self) -> bool {
        matches!(self, Read::Cookie | Read::Storage | Read::State | Read::Field | Read::Layout | Read::Idb | Read::Pixels)
    }
}

/// One input to the page, in the order it arrived.
#[derive(Clone, Debug)]
pub enum Step {
    /// An event delivered to the page's listeners. The target is a path of child indexes
    /// from the root: node ids aren't stable between runs (Blitz's layout allocates nodes).
    Event { kind: String, target: Vec<usize>, details: EventDetails },
    /// A batch of timers firing: (timer handle, the animation frame timestamp if it's one).
    Timers(Vec<(u32, Option<f64>)>),
    /// Console or AI input, by whom.
    Eval { source: String, by: &'static str },
    /// The user resumed the page after it paused on an exception.
    Resume,
    /// The tab moved to another entry of this page's document (back, forward, a fragment):
    /// its URL and `history.state`. The `popstate` that follows is a step of its own.
    Traverse { url: String, state: Option<String> },
    /// A `fetch` finished: its promise (a handle) and what it resolved with.
    Fetch { promise: u32, result: Result<crate::gate::FetchResponse, String> },
    /// Something happened on a WebSocket (by handle): opened, a message, an error, closed.
    Socket { socket: u32, event: crate::websocket::SocketEvent },
    /// An event the page's own code caused on an object that isn't a node, by handle (an
    /// IndexedDB request's `success`, a transaction's `complete`).
    ObjectEvent { target: u32, kind: String },
}

impl Step {
    /// A short description for the Recording view.
    pub fn describe(&self) -> String {
        match self {
            Step::Event { kind, target, .. } => format!("{kind} on node {target:?}"),
            Step::Timers(t) => format!("{} timer(s)", t.len()),
            Step::Eval { by, .. } => format!("{by} input"),
            Step::Resume => "resumed after an exception".into(),
            Step::Traverse { url, .. } => format!("moved to {url}"),
            Step::Fetch { result: Ok(r), .. } => format!("fetch answered ({})", r.status),
            Step::Fetch { result: Err(_), .. } => "fetch failed".into(),
            Step::ObjectEvent { kind, .. } => format!("{kind} (IndexedDB)"),
            Step::Socket { event, .. } => format!("WebSocket {}", match event {
                crate::websocket::SocketEvent::Open { .. } => "opened",
                crate::websocket::SocketEvent::Message(_) => "message",
                crate::websocket::SocketEvent::Error => "error",
                crate::websocket::SocketEvent::Close { .. } => "closed",
            }),
        }
    }
}

/// The program a page ran, as it was loaded.
#[derive(Clone, Debug)]
pub enum Program {
    Dart(Vec<u8>),
    /// "js", "python" or "php": its scripts, and (JS) each module import fetched, URL → source.
    Scripts { pack: &'static str, scripts: Vec<Script>, imports: Vec<(String, String)> },
}

/// Roughly the most memory one recording takes, in bytes. Past it the recording stops, at the
/// start of the step that went over, so what it has still replays.
const MAX_BYTES: usize = 64 << 20;
/// What a tape value costs against it.
const VALUE_BYTES: usize = 16;

impl Step {
    /// Roughly how much memory the step takes: a step holds as many timers or as long an
    /// input as the page produced.
    fn bytes(&self) -> usize {
        64 + match self {
            Step::Event { kind, target, .. } => kind.len() + target.len() * 8,
            Step::Timers(fired) => fired.len() * 16,
            Step::Eval { source, .. } => source.len(),
            Step::Resume => 0,
            Step::Traverse { url, state } => url.len() + state.as_ref().map_or(0, String::len),
            Step::Fetch { result, .. } => match result {
                Ok(r) => r.url.len() + r.body.len() + r.headers.iter().map(|(n, v)| n.len() + v.len()).sum::<usize>(),
                Err(e) => e.len(),
            },
            Step::ObjectEvent { kind, .. } => kind.len(),
            Step::Socket { event, .. } => match event {
                crate::websocket::SocketEvent::Message(text) => text.len(),
                crate::websocket::SocketEvent::Open { protocol } => protocol.len(),
                crate::websocket::SocketEvent::Close { reason, .. } => reason.len(),
                crate::websocket::SocketEvent::Error => 0,
            },
        }
    }
}

/// A recorded session of one page.
#[derive(Clone, Debug)]
pub struct Recording {
    pub id: u64,
    pub url: String,
    pub pack: &'static str,
    pub html: String,
    pub site_css: Option<String>,
    /// None until the loader attaches it (right after the pack starts).
    pub program: Option<Program>,
    pub steps: Vec<Step>,
    pub tape: Vec<(Read, u64)>,
    /// Text the pack read (cookies), indexed by tape values of text reads.
    pub texts: Vec<String>,
    /// Hit the size limit; steps from the one that went over weren't kept.
    pub full: bool,
    /// Bytes counted against `MAX_BYTES`.
    bytes: usize,
    /// The tape's length when the last step began: its reads come after.
    step_starts_at: usize,
    /// The page's DOM when recording stopped, to check a replay against.
    pub final_dom: String,
}

/// A page's nondeterminism: live, being recorded, or replayed from a tape. Lives in the
/// page's `Objects`, so the host (which answers the reads) and the document (which delivers
/// events and timers) share it.
#[derive(Default)]
pub enum Tape {
    #[default]
    Off,
    Record(Box<Recording>),
    Replay { values: VecDeque<(Read, u64)>, texts: Vec<String>, diverged: Option<String> },
}

impl Tape {
    /// A nondeterministic read: `live` is what it is now. Recording keeps it; replaying
    /// answers from the tape instead (or `live`, once the replay has diverged).
    pub fn read(&mut self, kind: Read, live: u64) -> u64 {
        match self {
            Tape::Off => live,
            Tape::Record(r) => {
                if !r.full {
                    r.bytes += VALUE_BYTES;
                    r.tape.push((kind, live));
                    if r.bytes > MAX_BYTES {
                        r.truncate_current_step();
                    }
                }
                live
            }
            Tape::Replay { values, diverged, .. } => match values.pop_front() {
                Some((k, v)) if k == kind && diverged.is_none() => v,
                other => {
                    diverged.get_or_insert_with(|| format!("the page read {kind:?} where the recording has {:?}", other.map(|(k, _)| k)));
                    live
                }
            },
        }
    }

    /// A nondeterministic read of text (a cookie, a stored value), like `read`.
    pub fn read_text(&mut self, kind: Read, live: String) -> String {
        match self {
            Tape::Off => live,
            Tape::Record(r) => {
                if !r.full {
                    r.bytes += VALUE_BYTES + live.len();
                    r.tape.push((kind, r.texts.len() as u64));
                    r.texts.push(live.clone());
                    if r.bytes > MAX_BYTES {
                        r.truncate_current_step();
                    }
                }
                live
            }
            Tape::Replay { .. } => {
                let at = self.read(kind, u64::MAX);
                match self {
                    Tape::Replay { texts, diverged: None, .. } => texts.get(at as usize).cloned().unwrap_or(live),
                    _ => live,
                }
            }
        }
    }

    /// Record an input step, if recording.
    pub fn step(&mut self, step: impl FnOnce() -> Step) {
        if let Tape::Record(r) = self {
            if r.full {
                return;
            }
            let step = step();
            r.bytes += step.bytes();
            r.steps.push(step);
            r.step_starts_at = r.tape.len();
            if r.bytes > MAX_BYTES {
                r.truncate_current_step();
            }
        }
    }

    pub fn replaying(&self) -> bool {
        matches!(self, Tape::Replay { .. })
    }

    pub fn recording(&self) -> Option<&Recording> {
        match self {
            Tape::Record(r) => Some(r),
            _ => None,
        }
    }
}

impl Recording {
    /// Over the size limit: stop, dropping the step under way and its reads, so every step
    /// kept has all its reads. (Over the limit during start-up, before any step, the
    /// start's reads go too and a replay reports that it diverged.)
    fn truncate_current_step(&mut self) {
        self.full = true;
        // Reads after `step_starts_at` are the last step's.
        self.steps.pop();
        self.tape.truncate(self.step_starts_at);
        // Texts the dropped reads pointed at go too.
        let kept = self.tape.iter().filter(|(k, _)| k.is_text()).count();
        self.texts.truncate(kept);
    }
}

// ---- Starting a page with a tape

/// Tapes waiting for a page at a URL to start (`PackDocument::starting` takes them).
static ARMED: Mutex<Vec<(String, Tape)>> = Mutex::new(Vec::new());

/// Record the next page that starts at `url`. The loader arms it just before starting the
/// tab's pack, when the tab asked to record (`DocumentLoader::record_next`).
pub fn arm(url: &str) {
    let recording = Recording {
        id: 0,
        url: url.to_string(),
        pack: "",
        html: String::new(),
        site_css: None,
        program: None,
        steps: Vec::new(),
        tape: Vec::new(),
        texts: Vec::new(),
        full: false,
        bytes: 0,
        step_starts_at: 0,
        final_dom: String::new(),
    };
    let mut armed = ARMED.lock().unwrap();
    armed.retain(|(u, _)| u != url);
    armed.push((url.to_string(), Tape::Record(Box::new(recording))));
}

/// The tape for a page starting at `url`: one armed for it, or none.
pub fn take_armed(url: &str) -> Tape {
    let mut armed = ARMED.lock().unwrap();
    match armed.iter().position(|(u, _)| u == url) {
        Some(at) => armed.remove(at).1,
        None => Tape::Off,
    }
}

// ---- Finished recordings

/// Recordings kept, newest last; older ones are dropped.
const KEEP: usize = 5;
static RECORDINGS: Mutex<Vec<Recording>> = Mutex::new(Vec::new());

/// Keep a finished recording; returns its id.
pub fn keep(mut recording: Recording) -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    recording.id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let id = recording.id;
    let mut all = RECORDINGS.lock().unwrap();
    if all.len() == KEEP {
        all.remove(0);
    }
    all.push(recording);
    id
}

/// Site `origin`'s recordings, newest first.
pub fn recordings(origin: &str) -> Vec<Recording> {
    RECORDINGS.lock().unwrap().iter().rev().filter(|r| crate::debug::same_site(&r.url, origin)).cloned().collect()
}

pub fn forget(id: u64) {
    RECORDINGS.lock().unwrap().retain(|r| r.id != id);
}

// ---- Node paths

/// Node `id` as child indexes from the root, or None if it isn't in the tree.
pub fn path_of(doc: &BaseDocument, mut id: NodeId) -> Option<Vec<usize>> {
    let root = doc.root_node().id;
    let mut path = Vec::new();
    while id != root {
        let parent = doc.get_node(id)?.parent?;
        path.push(doc.get_node(parent)?.children.iter().position(|&c| c == id)?);
        id = parent;
    }
    path.reverse();
    Some(path)
}

pub fn node_at(doc: &BaseDocument, path: &[usize]) -> Option<NodeId> {
    let mut id = doc.root_node().id;
    for &i in path {
        id = *doc.get_node(id)?.children.get(i)?;
    }
    Some(id)
}

/// The page's DOM as text: every element with its attributes, and text, as the Inspector
/// sees it. What a replay is checked against.
pub fn dom_text(doc: &BaseDocument) -> String {
    use crate::debug::{NodeKind, Privacy};
    let mut out = String::new();
    let mut stack = vec![(doc.root_node().id, 0usize)];
    while let Some((id, depth)) = stack.pop() {
        let Some(view) = crate::debug::node(doc, id, Privacy::Full) else { continue };
        let line = match &view.kind {
            NodeKind::Element { tag, attrs, .. } => format!("<{tag} {attrs:?}>"),
            NodeKind::Text(t) => format!("{t:?}"),
            NodeKind::Document => String::new(),
        };
        out.push_str(&format!("{}{line}\n", "  ".repeat(depth)));
        stack.extend(view.children.iter().rev().map(|&c| (c, depth + 1)));
    }
    out
}

// ---- Replaying

/// How a replay went.
#[derive(Clone, Debug, PartialEq)]
pub struct Outcome {
    /// Steps applied.
    pub steps: usize,
    /// Why the replay stopped matching the recording, if it did.
    pub diverged: Option<String>,
}

/// Run `recording` again in a fresh pack, applying its first `upto` steps (all if None), on a
/// document made with `config` (its base URL must be the recording's). The page is live
/// afterwards. Nothing is fetched: the program and imports come from the recording.
pub async fn replay(recording: &Recording, config: DocumentConfig, upto: Option<usize>) -> Result<(PackDocument, Outcome)> {
    let program = recording.program.as_ref().context("this recording has no program (the page didn't finish loading)")?;
    let mut doc = HtmlDocument::from_html(&recording.html, config).into_inner();
    if doc.url().as_str() != recording.url {
        bail!("replaying {} on a document at {}", recording.url, doc.url());
    }
    // The same nodes as the recorded load, in the same order.
    crate::site_css::apply(&mut doc, recording.site_css.as_deref());
    let values = recording.tape.iter().copied().collect();
    {
        let mut armed = ARMED.lock().unwrap();
        armed.retain(|(u, _)| *u != recording.url);
        armed.push((recording.url.clone(), Tape::Replay { values, texts: recording.texts.clone(), diverged: None }));
    }
    let started = match program {
        Program::Dart(wasm) => crate::dart_pack::start(doc, Default::default(), wasm),
        Program::Scripts { pack: "python", scripts, .. } => crate::python_pack::start(doc, Default::default(), scripts),
        Program::Scripts { pack: "php", scripts, .. } => crate::php_pack::start(doc, Default::default(), scripts),
        Program::Scripts { scripts, imports, .. } => {
            let fetch = |url: String| {
                let found = imports.iter().find(|(u, _)| *u == url).map(|(_, s)| s.clone()).with_context(|| format!("{url} wasn't fetched when this was recorded"));
                async move { found }
            };
            crate::js_pack::start(doc, Default::default(), scripts, fetch).await
        }
    };
    // A start that failed early may not have taken its tape.
    take_armed(&recording.url);
    let page = started?;
    let mut applied = 0;
    let mut diverged = None;
    for step in recording.steps.iter().take(upto.unwrap_or(usize::MAX)) {
        if let Err(e) = page.replay_step(step) {
            diverged = Some(format!("step {}: {e:#}", applied + 1));
            break;
        }
        applied += 1;
    }
    // The tape's own divergence, and back to live.
    let tape_diverged = page.end_replay();
    Ok((page, Outcome { steps: applied, diverged: diverged.or(tape_diverged) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recording(tape: &mut Tape) -> &mut Recording {
        match tape {
            Tape::Record(r) => r,
            _ => panic!("not recording"),
        }
    }

    /// Past the size limit the recording stops at the start of the step that went over, so
    /// every step it keeps has all of its reads; a step's size counts its timers.
    #[test]
    fn going_over_the_size_limit_drops_the_step_under_way() {
        arm("https://size-limit.test/");
        let mut tape = take_armed("https://size-limit.test/");
        tape.step(|| Step::Resume);
        tape.read(Read::Now, 1);
        recording(&mut tape).bytes = MAX_BYTES - 100;
        tape.step(|| Step::Timers(vec![(1, None)]));
        tape.read(Read::Now, 2);
        tape.read(Read::Now, 3);
        tape.read(Read::Now, 4);
        let r = recording(&mut tape);
        assert!(r.full);
        assert_eq!(r.steps.len(), 1, "the step under way went");
        assert_eq!(r.tape, [(Read::Now, 1)], "and its reads");
        tape.step(|| Step::Resume);
        assert_eq!(recording(&mut tape).steps.len(), 1, "nothing more once full");

        // One step with very many timers is too big by itself.
        arm("https://size-limit.test/");
        let mut tape = take_armed("https://size-limit.test/");
        tape.step(|| Step::Timers((0..5_000_000).map(|h| (h, None)).collect()));
        assert!(recording(&mut tape).full && recording(&mut tape).steps.is_empty());
    }
}
