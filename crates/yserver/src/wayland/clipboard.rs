//! The clipboard: X's `CLIPBOARD` and `PRIMARY` selections and the Wayland
//! compositor's clipboard and primary selection, one in both directions
//! and nothing but text.
//!
//! The Wayland side is `ext_data_control_v1`, the protocol of a clipboard
//! manager: it is told every selection whether or not one of the server's
//! windows has the keyboard, and it needs no input serial to set one. The
//! X side is a window of the server's own (`selection_window_for_server`),
//! which owns an X selection on the compositor's behalf and asks X clients
//! for theirs; what the selection protocol sends it reaches this module
//! through `ServerState::server_selection_events`.
//!
//! **X to Wayland.** When an X client owns a selection, a data source
//! offering text is set as the compositor's. When a Wayland program pastes
//! from it, the server asks the X owner for `UTF8_STRING` (then `STRING`),
//! reads the answer off its window and writes it to the Wayland program's
//! pipe.
//!
//! **Wayland to X.** When the compositor names a selection offering text
//! that is not the server's own source, the server's window takes the X
//! selection. An X client's `ConvertSelection` then becomes a `receive` on
//! the compositor's offer, read from a pipe into the requestor's property,
//! and its `SelectionNotify`. `TARGETS` is answered at once.
//!
//! Text only, as the design says, and no `INCR`: a selection larger than
//! one property is refused in both directions. Every transfer is read and
//! written without blocking, a little each loop iteration, and gives up
//! after [`PATIENCE`].

use std::{
    collections::VecDeque,
    os::fd::{AsFd as _, OwnedFd},
    time::{Duration, Instant},
};

use yserver_core::{
    backend::Backend,
    core_loop::process_request as requests,
    resources::SERVER_OWNER,
    server::{ServerSelectionEvent, ServerState},
};
use yserver_protocol::x11::{AtomId, ResourceId};

use super::transport::{Event, Offer, Selection, Source, Transport};

/// The types text is offered and asked for as on the Wayland side, the
/// preferred first.
const TEXT_TYPES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];

/// A type only the server's own sources offer, by which it knows its own
/// selection coming back from the compositor.
const OWN: &str = "application/x-yserver-selection";

/// How long a transfer may take before it is given up.
const PATIENCE: Duration = Duration::from_secs(5);

/// The most a selection may hold: what one property of the server's takes.
const LIMIT: usize = 16 << 20;

/// The property on the server's window an X owner answers into.
const PROPERTY: &str = "_YSERVER_SELECTION";

/// The two selections, by index: X's `CLIPBOARD` and `PRIMARY`.
const NAMES: [&str; 2] = ["CLIPBOARD", "PRIMARY"];

/// A Wayland program's paste of an X selection, waiting its turn.
#[derive(Debug)]
struct Send {
    which: usize,
    fd: OwnedFd,
    asked: Instant,
}

/// The paste the server is asking the X owner for now.
#[derive(Debug)]
struct Converting {
    send: Send,
    /// The type asked for.
    target: AtomId,
}

/// An X client's paste of the compositor's selection, read from the pipe.
#[derive(Debug)]
struct Answer {
    which: usize,
    reader: OwnedFd,
    data: Vec<u8>,
    time: u32,
    requestor: ResourceId,
    selection: AtomId,
    target: AtomId,
    property: AtomId,
    asked: Instant,
}

/// Data on its way to a Wayland program's pipe.
#[derive(Debug)]
struct Writing {
    which: usize,
    fd: OwnedFd,
    data: Vec<u8>,
    at: usize,
    asked: Instant,
}

/// The atoms the bridge uses, interned at its first sync.
#[derive(Clone, Copy, Debug)]
struct Atoms {
    selections: [AtomId; 2],
    utf8: AtomId,
    string: AtomId,
    text: AtomId,
    plain: AtomId,
    plain_utf8: AtomId,
    targets: AtomId,
    atom: AtomId,
    incr: AtomId,
    property: AtomId,
}

impl Atoms {
    fn intern(state: &mut ServerState) -> Self {
        let mut atom = |name: &str| state.atoms.intern(name, false);
        Self {
            selections: [atom(NAMES[0]), atom(NAMES[1])],
            utf8: atom("UTF8_STRING"),
            string: atom("STRING"),
            text: atom("TEXT"),
            plain: atom("text/plain"),
            plain_utf8: atom("text/plain;charset=utf-8"),
            targets: atom("TARGETS"),
            atom: atom("ATOM"),
            incr: atom("INCR"),
            property: atom(PROPERTY),
        }
    }

    fn is_text(&self, target: AtomId) -> bool {
        [
            self.utf8,
            self.string,
            self.text,
            self.plain,
            self.plain_utf8,
        ]
        .contains(&target)
    }
}

/// The bridge.
#[derive(Debug, Default)]
pub(super) struct Clipboard {
    atoms: Option<Atoms>,
    /// The server's window that owns and asks for selections.
    window: Option<ResourceId>,
    /// The offer the compositor last named for each selection, and the
    /// types it said it has.
    named: [Option<(Offer, Vec<String>)>; 2],
    /// Whether it named one since the last sync.
    renamed: [bool; 2],
    /// The server's source standing for an X owner, for each selection.
    sources: [Option<Source>; 2],
    /// The X owner (window and time) the source stands for.
    mirrored: [Option<(ResourceId, u32)>; 2],
    sends: VecDeque<Send>,
    converting: Option<Converting>,
    answers: Vec<Answer>,
    writes: Vec<Writing>,
}

impl Clipboard {
    /// Whether a transfer is under way, for which the loop must come round
    /// again soon even with nothing else to do.
    pub(super) fn busy(&self) -> bool {
        !self.sends.is_empty()
            || self.converting.is_some()
            || !self.answers.is_empty()
            || !self.writes.is_empty()
    }

    /// A clipboard event of the compositor's.
    pub(super) fn event<T: Transport>(&mut self, transport: &mut T, event: Event) {
        match event {
            Event::Selection {
                which,
                offer,
                types,
            } => self.name(transport, which.index(), offer.map(|offer| (offer, types))),
            Event::Send { source, fd, .. } => {
                // A source already let go, whose last events are still
                // coming, has its pipe closed as the event goes.
                let Some(which) = self.sources.iter().position(|held| *held == Some(source)) else {
                    return;
                };
                if let Err(error) = nonblocking(&fd) {
                    log::warn!("wayland: a paste's pipe: {error}");
                } else {
                    self.sends.push_back(Send {
                        which,
                        fd,
                        asked: Instant::now(),
                    });
                }
            }
            Event::Cancelled(source) => {
                // Something else is the selection now; the offer naming it
                // follows.
                if let Some(which) = self.sources.iter().position(|held| *held == Some(source)) {
                    self.sources[which] = None;
                }
                transport.destroy_source(source);
            }
            _ => {}
        }
    }

    /// The compositor named `named` (an offer and its types, or none) the
    /// selection `which` holds. The offer it replaces is let go.
    fn name<T: Transport>(
        &mut self,
        transport: &mut T,
        which: usize,
        named: Option<(Offer, Vec<String>)>,
    ) {
        let new = named.as_ref().map(|(offer, _)| *offer);
        let old = std::mem::replace(&mut self.named[which], named).map(|(offer, _)| offer);
        self.renamed[which] = true;
        let other = self.named[1 - which].as_ref().map(|(offer, _)| *offer);
        if let Some(old) = old
            && Some(old) != new
            && other != Some(old)
        {
            transport.release_offer(old);
        }
    }

    /// The types the offer `which` holds said it has.
    fn types(&self, which: usize) -> Option<&[String]> {
        self.named[which]
            .as_ref()
            .map(|(_, types)| types.as_slice())
    }

    /// Once per loop iteration: follow each side's selection on the other,
    /// and move what transfers can move.
    pub(super) fn sync<T: Transport>(
        &mut self,
        transport: &mut T,
        state: &mut ServerState,
        backend: &mut dyn Backend,
    ) {
        let atoms = *self.atoms.get_or_insert_with(|| Atoms::intern(state));
        let window = requests::selection_window_for_server(state, self.window);
        self.window = Some(window);
        let owner_of = |state: &ServerState, which: usize| {
            state.selections.get(&atoms.selections[which]).copied()
        };

        for which in [0, 1] {
            // The compositor's selection changed: an offer of text that is
            // not the server's own makes the server's window X's owner.
            if std::mem::take(&mut self.renamed[which]) {
                let types = self.types(which).map(<[String]>::to_vec);
                let own = types
                    .as_ref()
                    .is_some_and(|types| types.iter().any(|mime| mime == OWN));
                let text = types
                    .as_ref()
                    .is_some_and(|types| types.iter().any(|mime| is_text(mime)));
                let ours = owner_of(state, which).is_some_and(|(owner, _)| owner == window);
                if !own && text {
                    log::info!(
                        "wayland: the compositor's {} ({}) is X's {} now",
                        if which == 0 {
                            "clipboard"
                        } else {
                            "primary selection"
                        },
                        types.unwrap_or_default().join(", "),
                        NAMES[which],
                    );
                    self.set_owner(state, which, Some(window));
                } else if !own && ours {
                    log::info!(
                        "wayland: the compositor's {} holds no text; X's {} has no owner",
                        if which == 0 {
                            "clipboard"
                        } else {
                            "primary selection"
                        },
                        NAMES[which],
                    );
                    self.set_owner(state, which, None);
                }
            }

            // An X client owns the selection: offer it to the compositor.
            match owner_of(state, which) {
                Some((owner, time))
                    if owner != window
                        && state.resources.window_owner(owner) != Some(SERVER_OWNER) =>
                {
                    if self.mirrored[which] != Some((owner, time)) {
                        self.mirror(transport, which);
                        self.mirrored[which] = Some((owner, time));
                        log::info!(
                            "wayland: X's {} (0x{:x}) is the compositor's {} now",
                            NAMES[which],
                            owner.0,
                            if which == 0 {
                                "clipboard"
                            } else {
                                "primary selection"
                            },
                        );
                    }
                }
                Some(_) => self.mirrored[which] = None,
                None => {
                    if self.mirrored[which].take().is_some()
                        && let Some(held) = self.sources[which].take()
                    {
                        // The X owner went, and what it held with it.
                        transport.set_selection(Selection::BOTH[which], None);
                        transport.destroy_source(held);
                    }
                }
            }
        }

        for event in std::mem::take(&mut state.server_selection_events) {
            match event {
                ServerSelectionEvent::Request {
                    time,
                    owner,
                    requestor,
                    selection,
                    target,
                    property,
                } if owner == window => {
                    let property = if property.0 == 0 { target } else { property };
                    self.asked(
                        transport, state, backend, &atoms, time, requestor, selection, target,
                        property,
                    );
                }
                ServerSelectionEvent::Notify {
                    requestor,
                    target,
                    property,
                    ..
                } if requestor == window => {
                    self.answered(state, &atoms, window, target, property);
                }
                other => log::debug!("wayland: a selection event for another window: {other:?}"),
            }
        }

        self.convert(state, &atoms, window);
        self.read_answers(state, backend, &atoms);
        self.write_pastes();
    }

    /// Make the server's `window` the owner of X's selection `which`, or
    /// no one.
    fn set_owner(&self, state: &mut ServerState, which: usize, window: Option<ResourceId>) {
        let Some(atoms) = self.atoms else {
            return;
        };
        if let Err(error) =
            requests::set_selection_owner_for_server(state, atoms.selections[which], window)
        {
            log::warn!("wayland: owning {}: {error}", NAMES[which]);
        }
    }

    /// A new source of the server's, offering text, as the compositor's
    /// selection `which`.
    fn mirror<T: Transport>(&mut self, transport: &mut T, which: usize) {
        let types: Vec<&str> = TEXT_TYPES.iter().copied().chain([OWN]).collect();
        let made = match transport.create_source(&types) {
            Ok(made) => made,
            Err(error) => {
                log::warn!("wayland: a source for {}: {error}", NAMES[which]);
                return;
            }
        };
        if let Some(old) = self.sources[which].replace(made) {
            transport.destroy_source(old);
        }
        transport.set_selection(Selection::BOTH[which], Some(made));
    }

    /// An X client asked the server's window for the selection it owns on
    /// the compositor's behalf.
    #[expect(
        clippy::too_many_arguments,
        reason = "a SelectionRequest's own fields, and the state to answer it with"
    )]
    fn asked<T: Transport>(
        &mut self,
        transport: &mut T,
        state: &mut ServerState,
        backend: &mut dyn Backend,
        atoms: &Atoms,
        time: u32,
        requestor: ResourceId,
        selection: AtomId,
        target: AtomId,
        property: AtomId,
    ) {
        let refuse = |state: &mut ServerState| {
            notify(state, time, requestor, selection, target, AtomId(0));
        };
        let Some(which) = atoms.selections.iter().position(|held| *held == selection) else {
            refuse(state);
            return;
        };
        if target == atoms.targets {
            let list: Vec<u8> = [
                atoms.targets,
                atoms.utf8,
                atoms.string,
                atoms.text,
                atoms.plain_utf8,
            ]
            .iter()
            .flat_map(|atom| atom.0.to_ne_bytes())
            .collect();
            let put = requests::change_property_for_server(
                state, backend, requestor, property, atoms.atom, 32, &list,
            );
            let property = if put.is_ok() { property } else { AtomId(0) };
            notify(state, time, requestor, selection, target, property);
            return;
        }
        let offered = self.named[which].as_ref().and_then(|(held, types)| {
            TEXT_TYPES
                .iter()
                .find(|wanted| types.iter().any(|mime| mime == *wanted))
                .map(|mime| (*held, *mime))
        });
        let (Some((held, mime)), true) = (offered, atoms.is_text(target)) else {
            refuse(state);
            return;
        };
        let (reader, writer) = match nix::unistd::pipe2(
            nix::fcntl::OFlag::O_CLOEXEC | nix::fcntl::OFlag::O_NONBLOCK,
        ) {
            Ok(pipe) => pipe,
            Err(error) => {
                log::warn!("wayland: a pipe for a paste: {error}");
                refuse(state);
                return;
            }
        };
        let asked = transport.receive(held, mime, writer.as_fd());
        // The compositor has its own copy of the writing end now; this one
        // closed is how the reader sees the end of the data.
        drop(writer);
        if let Err(error) = asked {
            log::warn!("wayland: asking the compositor for {mime}: {error}");
            refuse(state);
            return;
        }
        self.answers.push(Answer {
            which,
            reader,
            data: Vec::new(),
            time,
            requestor,
            selection,
            target,
            property,
            asked: Instant::now(),
        });
    }

    /// Read what the compositor's selections gave, and answer the X client
    /// once all of it has come.
    fn read_answers(&mut self, state: &mut ServerState, backend: &mut dyn Backend, atoms: &Atoms) {
        let mut done = Vec::new();
        for (at, answer) in self.answers.iter_mut().enumerate() {
            let mut chunk = [0u8; 65536];
            let ended = loop {
                match nix::unistd::read(&answer.reader, &mut chunk) {
                    Ok(0) => break Some(true),
                    Ok(read) => {
                        answer
                            .data
                            .extend_from_slice(chunk.get(..read).unwrap_or_default());
                        if answer.data.len() > LIMIT {
                            break Some(false);
                        }
                    }
                    Err(nix::errno::Errno::EAGAIN) => {
                        break (answer.asked.elapsed() > PATIENCE).then_some(false);
                    }
                    Err(nix::errno::Errno::EINTR) => {}
                    Err(_) => break Some(false),
                }
            };
            if let Some(whole) = ended {
                done.push((at, whole));
            }
        }
        for (at, whole) in done.into_iter().rev() {
            let answer = self.answers.remove(at);
            let (r#type, data) = if answer.target == atoms.string {
                (atoms.string, latin1(&answer.data))
            } else {
                (atoms.utf8, answer.data)
            };
            let put = whole
                && requests::change_property_for_server(
                    state,
                    backend,
                    answer.requestor,
                    answer.property,
                    r#type,
                    8,
                    &data,
                )
                .is_ok();
            if put {
                log::info!(
                    "wayland: gave an X client {} bytes of the compositor's {} as {}",
                    data.len(),
                    NAMES[answer.which],
                    state.atoms.name(answer.target).unwrap_or("?"),
                );
            } else {
                log::warn!(
                    "wayland: the compositor's {} did not reach the X client",
                    NAMES[answer.which]
                );
            }
            notify(
                state,
                answer.time,
                answer.requestor,
                answer.selection,
                answer.target,
                if put { answer.property } else { AtomId(0) },
            );
        }
    }

    /// Ask the X owner for the next paste waiting, when none is being
    /// asked for; give up on one that took too long.
    fn convert(&mut self, state: &mut ServerState, atoms: &Atoms, window: ResourceId) {
        if let Some(converting) = &self.converting
            && converting.send.asked.elapsed() > PATIENCE
        {
            log::warn!(
                "wayland: X's {} owner did not answer",
                NAMES[converting.send.which]
            );
            self.converting = None;
        }
        if self.converting.is_some() {
            return;
        }
        let Some(send) = self.sends.pop_front() else {
            return;
        };
        self.ask(state, atoms, window, send, atoms.utf8);
    }

    fn ask(
        &mut self,
        state: &mut ServerState,
        atoms: &Atoms,
        window: ResourceId,
        send: Send,
        target: AtomId,
    ) {
        let selection = atoms.selections[send.which];
        self.converting = Some(Converting { send, target });
        if let Err(error) =
            requests::convert_selection_for_server(state, window, selection, target, atoms.property)
        {
            log::warn!("wayland: asking for {}: {error}", NAMES[0]);
            self.converting = None;
        }
    }

    /// The X owner answered the server's window.
    fn answered(
        &mut self,
        state: &mut ServerState,
        atoms: &Atoms,
        window: ResourceId,
        target: AtomId,
        property: AtomId,
    ) {
        let Some(converting) = self.converting.take() else {
            return;
        };
        if converting.target != target {
            self.converting = Some(converting);
            return;
        }
        if property.0 == 0 {
            // Refused: an owner that has no UTF8_STRING may have STRING.
            if target == atoms.utf8 {
                self.ask(state, atoms, window, converting.send, atoms.string);
            } else {
                log::warn!(
                    "wayland: X's {} owner refused it as text",
                    NAMES[converting.send.which]
                );
            }
            return;
        }
        let Some(value) = state.resources.delete_window_property(window, property) else {
            log::warn!("wayland: X's selection owner said it answered, and did not");
            return;
        };
        if value.r#type == atoms.incr {
            log::warn!(
                "wayland: X's {} is too large to pass on (INCR)",
                NAMES[converting.send.which]
            );
            return;
        }
        let data = if value.r#type == atoms.string {
            latin1_to_utf8(&value.data)
        } else {
            value.data
        };
        self.writes.push(Writing {
            which: converting.send.which,
            fd: converting.send.fd,
            data,
            at: 0,
            asked: converting.send.asked,
        });
    }

    /// Write what X's owners gave to the pipes of the Wayland programs that
    /// asked, each closed once all of it is written.
    fn write_pastes(&mut self) {
        self.writes.retain_mut(|writing| {
            loop {
                let Some(rest) = writing.data.get(writing.at..) else {
                    return false;
                };
                if rest.is_empty() {
                    log::info!(
                        "wayland: gave the compositor {} bytes of X's {}",
                        writing.data.len(),
                        NAMES[writing.which]
                    );
                    return false;
                }
                let written = nix::unistd::write(&writing.fd, rest);
                match written {
                    Ok(count) => writing.at += count,
                    Err(nix::errno::Errno::EAGAIN) => {
                        return writing.asked.elapsed() <= PATIENCE;
                    }
                    Err(nix::errno::Errno::EINTR) => {}
                    Err(error) => {
                        log::warn!("wayland: writing a paste: {error}");
                        return false;
                    }
                }
            }
        });
    }
}

/// Tell an X requestor where its answer is.
fn notify(
    state: &mut ServerState,
    time: u32,
    requestor: ResourceId,
    selection: AtomId,
    target: AtomId,
    property: AtomId,
) {
    if let Err(error) = requests::send_selection_notify_for_server(
        state, time, requestor, selection, target, property,
    ) {
        log::warn!("wayland: answering a paste: {error}");
    }
}

/// Whether a Wayland type is one of text's.
fn is_text(mime: &str) -> bool {
    TEXT_TYPES.contains(&mime) || mime.starts_with("text/plain")
}

/// Make a pipe's end not block.
fn nonblocking(fd: &OwnedFd) -> nix::Result<()> {
    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL)?);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map(drop)
}

/// UTF-8 text as ICCCM `STRING`, Latin-1: what has no Latin-1 form is a
/// question mark.
fn latin1(text: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(text)
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect()
}

/// ICCCM `STRING`, Latin-1, as UTF-8.
fn latin1_to_utf8(text: &[u8]) -> Vec<u8> {
    text.iter()
        .map(|&byte| char::from(byte))
        .collect::<String>()
        .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_goes_between_utf8_and_latin1() {
        assert_eq!(latin1("café €".as_bytes()), b"caf\xe9 ?");
        assert_eq!(latin1_to_utf8(b"caf\xe9"), "café".as_bytes());
    }

    #[test]
    fn plain_text_of_any_charset_is_text_and_the_own_marker_is_not() {
        assert!(is_text("text/plain;charset=utf-8"));
        assert!(is_text("text/plain;charset=iso-8859-1"));
        assert!(is_text("UTF8_STRING"));
        assert!(!is_text("image/png"));
        assert!(!is_text(OWN));
    }
}
