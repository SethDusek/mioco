use super::{Event, EventSourceId, RW, coroutine, token_to_ids, sender_retry};
use super::CoroutineControl;
use super::thread::{tl_current_coroutine_ptr_save, tl_current_coroutine_ptr_restore,
                    tl_current_coroutine};
use super::thread::{HandlerShared, Message};
use super::thread::Handler;
use super::evented::{RcEventSourceTrait, RcEventSource, EventSourceTrait};
use super::thread::RcHandlerShared;
use super::thunk::Thunk;
use super::sync::mpsc;
use mio_orig::{Token, EventSet, EventLoop};

use context::{self, stack};
use slab;

use std::thread;
use std::any::Any;
use std::cell;
use std::ops::Deref;
use std::sync::Arc;
use std::cell::{RefCell, RefMut};
use std::rc::Rc;
use std::mem;
use std::panic;

/// Special Token used to signal scheduler timeout
/// See `SchedulerThread::timeout`
pub static SPECIAL_ID: coroutine::Id = coroutine::Id(0);
pub static SPECIAL_ID_SCHED_TIMEOUT: EventSourceId = EventSourceId(0);

/// Coroutines are number from this upwards
pub static STARTING_ID: coroutine::Id = coroutine::Id(1);


/// Id of a Coroutine used to enumerate them
///
/// It's unique within a thread
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Id(usize);

impl Id {
    pub fn new(id: usize) -> Self {
        Id(id)
    }

    pub fn as_usize(&self) -> usize {
        self.0
    }
}

impl slab::Index for Id {
    fn as_usize(&self) -> usize {
        self.0
    }
    fn from_usize(i: usize) -> Self {
        Id(i)
    }
}

pub type ExitStatus<T> = thread::Result<T>;
pub type ExitSender<T> = mpsc::Sender<ExitStatus<T>>;

/// State of `mioco` coroutine
#[derive(Clone, Debug)]
pub enum State {
    /// Blocked on EventSource(s)
    Blocked,
    /// Yielding
    Yielding,
    /// Ready to be started
    Ready,
    /// Done
    Finished,
}

impl State {
    /// Is the `State` `Ready`?
    pub fn is_ready(&self) -> bool {
        match *self {
            State::Ready => true,
            _ => false,
        }
    }

    /// Is the `State` `Blocked`?
    pub fn is_blocked(&self) -> bool {
        match *self {
            State::Blocked => true,
            _ => false,
        }
    }

    /// Is the `State` `Yielding`?
    pub fn is_yielding(&self) -> bool {
        match *self {
            State::Yielding => true,
            _ => false,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct Config {
    pub stack_size: usize,
    pub catch_panics: bool,
    pub stack_protection: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            stack_size: 2 * 1024 * 1024,
            catch_panics: true,
            stack_protection: true,
        }
    }
}

pub type RcCoroutine = Rc<RefCell<Coroutine>>;

enum AnyStack {
    #[allow(dead_code)]
    Unprotected(stack::FixedSizeStack),
    Protected(stack::ProtectedFixedSizeStack),
}

impl Deref for AnyStack {
    type Target = stack::Stack;

    fn deref(&self) -> &stack::Stack {
        match *self {
            AnyStack::Unprotected(ref s) => &s,
            AnyStack::Protected(ref s) => &s,
        }
    }
}

struct Killed;

/// Used to store Any userdata
pub type UserDataAny = Arc<Box<Any + Send + Sync>>;

/// Mioco coroutine (a.k.a. *mioco handler*)
// TODO: Make everything private
pub struct Coroutine {
    /// Context with a state of coroutine
    context: Option<context::Context>,

    #[allow(dead_code)]
    /// Coroutine stack
    stack: AnyStack,

    /// Current state
    pub state: State,

    /// Last event that resumed the coroutine
    pub last_event: Event,

    /// `Handler` shared data that this `Coroutine` is running in
    handler_shared: Option<RcHandlerShared>,

    /// Current coroutine Id
    pub id: Id,

    /// All event sources the coroutine is blocked on
    pub blocked_on: Vec<Box<RcEventSourceTrait + 'static>>,

    /// Newly spawned `Coroutine`-es
    pub children_to_start: Vec<RcCoroutine>,

    /// Function to be run inside Coroutine
    coroutine_func: Option<Thunk<'static>>,

    /// In case Rc to self is needed
    pub self_rc: Option<RcCoroutine>,

    pub sync_channel: Option<(mpsc::Sender<()>, mpsc::Receiver<()>)>,

    /// Userdata of the coroutine
    pub user_data: Option<UserDataAny>,

    /// Userdata meant for inheritance
    pub inherited_user_data: Option<UserDataAny>,

    /// Force exit
    killed: bool,
}

extern "C" fn unwind_stack(t: context::Transfer) -> context::Transfer {
    {
        let coroutine: &mut Coroutine = unsafe { mem::transmute(t.data) };
        let mut o_c = coroutine.out_context();
        *o_c = Some(t.context);
    }

    panic::resume_unwind(Box::new(Killed))
}

impl Coroutine {
    /// Spawn a new Coroutine
    pub fn spawn<F, T>(handler_shared: RcHandlerShared,
                       inherited_user_data: Option<UserDataAny>,
                       coroutine_user_fn: F,
                       exit_sender: ExitSender<T>)
                       -> RcCoroutine
        where F: FnOnce() -> T,
              F: Send + 'static,
              T: Send + 'static
    {
        /// C function that context-rs needs
        extern "C" fn coroutine_context_start_fn(t: context::Transfer) -> ! {
            let data = t.data;

            let f = {
                let coroutine: &mut Coroutine = unsafe { mem::transmute(data) };
                co_debug!(coroutine, "started");

                *coroutine.out_context() = Some(t.context);

                coroutine.coroutine_func.take().unwrap()
            };

            f.invoke(());

            let context = {
                let coroutine: &mut Coroutine = unsafe { mem::transmute(data) };
                let context = coroutine.out_context().take().unwrap();
                co_debug!(coroutine, "ended");
                context
            };

            let _ = context.resume(0);
            unreachable!();
        }

        // Inner body of the coroutine, handling the user-provided
        // coroutine code (which can have different signatures),
        // and sending the notification back
        let coroutine_main_fn = move || {
            let coroutine_user_fn = panic::AssertUnwindSafe(coroutine_user_fn);
            let res = panic::catch_unwind(move || {

                let coroutine = unsafe { tl_current_coroutine() };
                if coroutine.killed {
                    panic::resume_unwind(Box::new(Killed))
                }

                coroutine_user_fn()
            });

            let coroutine = unsafe { tl_current_coroutine() };
            coroutine.blocked_on.clear();
            coroutine.self_rc = None;
            coroutine.state = State::Finished;

            let id = coroutine.id;
            {
                let mut handler_shared = coroutine.handler_shared
                                                  .as_ref()
                                                  .unwrap()
                                                  .borrow_mut();
                handler_shared.coroutines.remove(id).unwrap();
                handler_shared.coroutines_dec();
            }

            let config = coroutine.handler_shared().coroutine_config;
            match res {
                Ok(res) => {
                    co_debug!(coroutine, "finished normally");
                    let _ = exit_sender.send(Ok(res));
                }
                Err(cause) => {
                    if config.catch_panics {
                        co_debug!(coroutine, "finished by panick");
                        let _ = exit_sender.send(Err(cause));
                    } else {
                        // send fail here instead with the internal reason,
                        // so the user may get a nice backtrace
                        let handler = coroutine.handler_shared.as_ref().unwrap().borrow();
                        sender_retry(&handler.get_sender_to_own_thread(),
                                     Message::PropagatePanic(cause));
                    }
                }
            }

        };

        let config = handler_shared.borrow().coroutine_config;

        let id = {
            let coroutines = &mut handler_shared.borrow_mut().coroutines;

            if !coroutines.has_remaining() {
                let count = coroutines.count();
                coroutines.grow(count);
            }

            let stack = if config.stack_protection {
                AnyStack::Protected(stack::ProtectedFixedSizeStack::new(config.stack_size).unwrap())
            } else {
                AnyStack::Unprotected(stack::FixedSizeStack::new(config.stack_size).unwrap())
            };

            coroutines.insert_with(|id| {
                          let coroutine = Coroutine {
                              state: State::Ready,
                              killed: false,
                              id: id,
                              last_event: Event {
                                  rw: RW::read(),
                                  id: EventSourceId(0),
                              },
                              context: Some(context::Context::new(&stack,
                                                                  coroutine_context_start_fn)),
                              stack: stack,
                              handler_shared: Some(handler_shared.clone()),
                              blocked_on: Vec::with_capacity(4),
                              children_to_start: Vec::new(),
                              coroutine_func: Some(Thunk::new(coroutine_main_fn)),
                              self_rc: None,
                              sync_channel: None,
                              user_data: inherited_user_data.clone(),
                              inherited_user_data: inherited_user_data,
                          };

                          CoroutineSlabHandle::new(Rc::new(RefCell::new(coroutine)))
                      })
                      .expect("Run out of slab for coroutines")
        };
        handler_shared.borrow_mut().coroutines_inc();

        let coroutine_rc = handler_shared.borrow().coroutines[id].rc.clone();

        coroutine_rc.borrow_mut().self_rc = Some(coroutine_rc.clone());



        {
            let co = coroutine_rc.borrow();
            co_debug!(co, "spawned");
        }
        coroutine_rc
    }

    pub fn spawn_child<F, T>(&mut self, f: F, exit_sender: ExitSender<T>)
        where F: FnOnce() -> T,
              F: Send + 'static,
              T: Send + 'static
    {

        co_debug!(self, "spawning child");
        let child = Coroutine::spawn(self.handler_shared.as_ref().unwrap().clone(),
                                     self.inherited_user_data.clone(),
                                     f,
                                     exit_sender);
        self.children_to_start.push(child);
    }

    pub fn handler_shared(&self) -> cell::Ref<HandlerShared> {
        self.handler_shared.as_ref().unwrap().borrow()
    }

    pub fn handler_shared_mut(&self) -> cell::RefMut<HandlerShared> {
        self.handler_shared.as_ref().unwrap().borrow_mut()
    }

    pub fn block_on<T>(&mut self, event_source: &RcEventSource<T>, rw: RW)
        where T: EventSourceTrait + 'static
    {

        co_debug!(self, "blocked on {:?}", rw);
        self.state = coroutine::State::Blocked;
        let id = self.blocked_on.len();

        {
            let mut common = &mut event_source.common_mut();
            common.id = Some(EventSourceId::new(id));
            common.blocked_on = rw;
        }
        self.blocked_on.push(event_source.to_trait());
    }

    pub fn unblock(&mut self, event_loop: &mut EventLoop<Handler>, event: Event) {
        self.state = coroutine::State::Ready;
        self.last_event = event;

        self.deregister_all(event_loop);
    }


    pub fn unblock_after_yield(&mut self) {
        self.state = coroutine::State::Ready;
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    // TODO: Make priv.
    pub fn deregister_all(&mut self, event_loop: &mut EventLoop<Handler>) {
        for mut io in self.blocked_on.drain(..) {
            io.deregister(event_loop, self.id);
        }
    }

    // TODO: Make priv.
    pub fn register_all(&mut self, event_loop: &mut EventLoop<Handler>) -> bool {
        let mut id = None;
        let mut rw = None;

        for io in &mut self.blocked_on {
            if io.register(event_loop, self.id) {
                id = Some(io.id().unwrap());
                rw = Some(io.blocked_on());
            }
        }
        if let Some(id) = id {
            self.unblock(event_loop,
                         Event {
                             rw: rw.unwrap(),
                             id: id,
                         });
            true
        } else {
            false
        }
    }

    // TODO: Remove and call add_spawned directly?
    pub fn start_children(&mut self) {
        let Coroutine { ref mut children_to_start, ref handler_shared, .. } = *self;

        let mut handler_shared = handler_shared.as_ref().unwrap().borrow_mut();

        for child in children_to_start.drain(..) {
            handler_shared.add_spawned(CoroutineControl::new(child));
        }
    }

    pub fn detach_from(&mut self,
                       event_loop: &mut EventLoop<Handler>,
                       to_thread_id: usize)
                       -> RcHandlerShared {
        // for better formatting print here instead of calling function
        co_debug!(self, "migrating to thread {}", to_thread_id);

        self.deregister_all(event_loop);

        let handler_shared = self.handler_shared.take();
        debug_assert!(self.handler_shared.is_none());

        debug!(
            concat!(co_trace_fmt_prefix!(), "detached"),
            handler_shared.as_ref().unwrap().borrow().thread_id(),
            self.id.as_usize(),
        );

        handler_shared.unwrap()
    }

    pub fn attach_to(&mut self,
                     event_loop: &mut EventLoop<Handler>,
                     handler_shared: RcHandlerShared,
                     id: Id) {
        self.id = id;
        self.handler_shared = Some(handler_shared);

        self.register_all(event_loop);
        co_debug!(self, "attached");
    }

    pub fn out_context(&self) -> RefMut<Option<context::Context>> {
        RefMut::map(self.handler_shared.as_ref().unwrap().borrow_mut(),
                    |h| &mut h.context)
    }

    pub fn was_running_before(&self) -> bool {
        self.coroutine_func.is_none()
    }
}

impl CoroutineControl {
    /// Finish coroutine
    pub fn kill(&self) {
        {
            self.rc.borrow_mut().killed = true;
        }
        coroutine::jump_in(&self.rc);
    }
}

/// Event delivery point, kept in Handler slab.
#[derive(Clone)]
pub struct CoroutineSlabHandle {
    rc: RcCoroutine,
}

impl CoroutineSlabHandle {
    pub fn new(rc: RcCoroutine) -> Self {
        CoroutineSlabHandle { rc: rc }
    }

    pub fn as_rc(&self) -> &RcCoroutine {
        &self.rc
    }

    pub fn to_coroutine_control(&self) -> CoroutineControl {
        CoroutineControl::new(self.rc.clone())
    }

    /// Deliver an event to a Coroutine
    pub fn event(&self,
                 event_loop: &mut EventLoop<Handler>,
                 token: Token,
                 events: EventSet)
                 -> bool {

        let (_, io_id) = token_to_ids(token);

        {
            let co = self.rc.borrow();
            co_debug!(co, "event for io({})", io_id.as_usize());
        }

        if !self.rc.borrow().state().is_blocked() {
            // subsequent event to coroutine that is either already
            // Exiting, or Ready
            return false;
        }

        {
            let co = self.rc.borrow();
            co_debug!(co, "ready");
        }
        // Wake coroutine on HUP, as it was read, to potentially let it fail the read and move on
        let event = match (events.is_readable() | events.is_hup(), events.is_writable()) {
            (true, true) => RW::both(),
            (true, false) => RW::read(),
            (false, true) => RW::write(),
            (false, false) => panic!(),
        };

        let mut co = self.rc.borrow_mut();
        co.unblock(event_loop,
                   Event {
                       rw: event,
                       id: io_id,
                   });
        true
    }

    pub fn id(&self) -> coroutine::Id {
        let coroutine = self.rc.borrow();
        coroutine.id
    }
}

/// Resume coroutine execution, jumping into it
// TODO: Make part of the Coroutine, using rc_self
pub fn jump_in(coroutine: &RefCell<Coroutine>) {

    {
        let co = &coroutine.borrow();
        match co.state {
            State::Ready => {}
            State::Finished => return,
            _ => debug_assert!(co.killed),
        }
    }

    let (prev, co_ptr) = {
        let co_ptr = {
            &mut *coroutine.borrow_mut() as *mut Coroutine
        };
        let prev = tl_current_coroutine_ptr_save(co_ptr);
        let co_ptr: usize = unsafe { mem::transmute(co_ptr) };
        (prev, co_ptr)
    };

    {
        let co = coroutine.borrow();
        co_debug!(co, "resuming");
    }

    let (context, was_running_before, killed) = {
        let mut co = coroutine.borrow_mut();
        (co.context.take().unwrap(), co.was_running_before(), co.killed)
    };

    let t = if killed && was_running_before {
        context.resume_ontop(co_ptr, unwind_stack)
    } else {
        context.resume(co_ptr)
    };

    coroutine.borrow_mut().context = Some(t.context);

    tl_current_coroutine_ptr_restore(prev);
}

/// Block coroutine execution, jumping out of it
// TODO: Make part of the Coroutine, using rc_self
pub fn jump_out(coroutine: &RefCell<Coroutine>) {
    {
        let state = &coroutine.borrow().state;
        debug_assert!(state.is_blocked() || state.is_yielding());
    }

    let context = {
        let co = coroutine.borrow();
        let mut o_c = co.out_context();
        o_c.take().unwrap()
    };
    {
        let co = coroutine.borrow();
        co_debug!(co, "pausing");
    }
    let t = context.resume(0);
    {
        let co = coroutine.borrow();
        let mut o_c = co.out_context();
        *o_c = Some(t.context);
    }
}
