use crate::agent::Agent;
use block2::RcBlock;
use dispatch2::{
    DispatchObject, DispatchQueue, DispatchRetained, DispatchSource, _dispatch_source_type_memorypressure,
    _dispatch_source_type_signal, _dispatch_source_type_timer, dispatch_source_memorypressure_flags_t,
    dispatch_source_type_t,
};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_foundation::{
    NSNotification, NSNotificationCenter, NSObjectProtocol, NSOperationQueue,
    NSProcessInfoThermalStateDidChangeNotification,
};
use std::cell::RefCell;
use std::ptr::NonNull;
use std::rc::Rc;

pub struct Subscriptions {
    _sources: Vec<DispatchRetained<DispatchSource>>,
    _thermal: Retained<ProtocolObject<dyn NSObjectProtocol>>,
}

pub fn subscribe(agent: &Rc<RefCell<Agent>>) -> Subscriptions {
    let timer = main_queue_source(source_type(&raw const _dispatch_source_type_timer), 0, 0, {
        let agent = Rc::clone(agent);
        move || agent.borrow_mut().scan()
    });
    agent.borrow_mut().attach_timer(timer.clone());
    let mut sources = vec![timer, watch_memory_pressure(agent)];
    for signal in [libc::SIGTERM, libc::SIGINT] {
        sources.push(watch_signal(agent, signal, |agent| {
            agent.shut_down();
            std::process::exit(0);
        }));
    }
    sources.push(watch_signal(agent, libc::SIGHUP, |agent| {
        agent.reload_config();
        agent.scan();
    }));
    Subscriptions { _sources: sources, _thermal: watch_thermal_state(agent) }
}

fn watch_memory_pressure(agent: &Rc<RefCell<Agent>>) -> DispatchRetained<DispatchSource> {
    let levels = dispatch_source_memorypressure_flags_t::DISPATCH_MEMORYPRESSURE_NORMAL.0
        | dispatch_source_memorypressure_flags_t::DISPATCH_MEMORYPRESSURE_WARN.0
        | dispatch_source_memorypressure_flags_t::DISPATCH_MEMORYPRESSURE_CRITICAL.0;
    let agent = Rc::clone(agent);
    main_queue_source(source_type(&raw const _dispatch_source_type_memorypressure), 0, levels as usize, move || {
        agent.borrow_mut().wake("a memory pressure change");
    })
}

fn watch_signal(
    agent: &Rc<RefCell<Agent>>,
    signal: libc::c_int,
    respond: impl Fn(&mut Agent) + 'static,
) -> DispatchRetained<DispatchSource> {
    unsafe { libc::signal(signal, libc::SIG_IGN) };
    let agent = Rc::clone(agent);
    main_queue_source(source_type(&raw const _dispatch_source_type_signal), signal as usize, 0, move || {
        respond(&mut agent.borrow_mut())
    })
}

fn watch_thermal_state(agent: &Rc<RefCell<Agent>>) -> Retained<ProtocolObject<dyn NSObjectProtocol>> {
    let agent = Rc::clone(agent);
    let block = RcBlock::new(move |_: NonNull<NSNotification>| {
        agent.borrow_mut().wake("a thermal state change");
    });
    unsafe {
        NSNotificationCenter::defaultCenter().addObserverForName_object_queue_usingBlock(
            Some(NSProcessInfoThermalStateDidChangeNotification),
            None,
            Some(&NSOperationQueue::mainQueue()),
            &block,
        )
    }
}

fn source_type(kind: *const dispatch2::dispatch_source_type_s) -> dispatch_source_type_t {
    kind.cast_mut()
}

fn main_queue_source(
    kind: dispatch_source_type_t,
    handle: usize,
    mask: usize,
    handler: impl Fn() + 'static,
) -> DispatchRetained<DispatchSource> {
    let source = unsafe { DispatchSource::new(kind, handle, mask, Some(DispatchQueue::main())) };
    let block = RcBlock::new(handler);
    unsafe { source.set_event_handler_with_block(RcBlock::as_ptr(&block)) };
    source.resume();
    source
}
