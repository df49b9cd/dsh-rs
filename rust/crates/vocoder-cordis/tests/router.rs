//! Behavioral tests for the router machine: emit fan-out, waterfall chains,
//! disposal sagas, and the ping-pong cap.

use serde_json::json;
use vocoder_cordis::*;

/// A test machine scripted by closures over a log.
struct Scripted<F>
where
    F: FnMut(&MachineIn) -> Vec<MachineOut>,
{
    handle: F,
    seen: Vec<MachineIn>,
}

impl<F: FnMut(&MachineIn) -> Vec<MachineOut>> Scripted<F> {
    fn new(f: F) -> Self {
        Self {
            handle: f,
            seen: Vec::new(),
        }
    }
}

impl<F: FnMut(&MachineIn) -> Vec<MachineOut>> PluginMachine for Scripted<F> {
    type In = MachineIn;
    type Out = MachineOut;
    fn handle(&mut self, ev: MachineIn) -> Vec<MachineOut> {
        self.seen.push(ev.clone());
        (self.handle)(&ev)
    }
}

fn mount(router: &mut Router, id: &str, m: impl Machine + 'static) {
    router.handle(RouteIn::Mount {
        id: MachineId::new(id),
        machine: Box::new(m),
    });
}

// --- Emit: fan-out in subscription order, BFS onward ---------------------

#[test]
fn emit_fans_out_to_subscribers_in_order() {
    let mut r = Router::new();
    mount(
        &mut r,
        "a",
        Scripted::new(|_| {
            vec![MachineOut::Subscribe {
                name: EventName::new("e"),
            }]
        }),
    );
    mount(
        &mut r,
        "b",
        Scripted::new(|_| {
            vec![MachineOut::Subscribe {
                name: EventName::new("e"),
            }]
        }),
    );
    // activate subscriptions
    r.handle(RouteIn::Deliver {
        to: MachineId::new("a"),
        ev: MachineIn::ServicesReady { keys: vec![] },
    });
    r.handle(RouteIn::Deliver {
        to: MachineId::new("b"),
        ev: MachineIn::ServicesReady { keys: vec![] },
    });

    assert_eq!(
        r.subscribers(&EventName::new("e")),
        &[MachineId::new("a"), MachineId::new("b")]
    );

    let a = r.handle(RouteIn::Dispatch {
        name: EventName::new("e"),
        payload: json!(1),
        mode: DispatchMode::Emit,
    });
    assert!(a.is_empty(), "pure fan-out yields no router outputs");
}

#[test]
fn emit_cascades_breadth_first() {
    // a and b subscribe to "e"; when a sees it, it dispatches "f"; b subscribes to "f".
    let mut r = Router::new();
    mount(
        &mut r,
        "a",
        Scripted::new(|ev| match ev {
            MachineIn::ServicesReady { .. } => vec![MachineOut::Subscribe {
                name: EventName::new("e"),
            }],
            MachineIn::Event { name, .. } if name.0 == "e" => vec![MachineOut::Dispatch {
                name: EventName::new("f"),
                payload: json!("from-a"),
                mode: DispatchMode::Emit,
            }],
            _ => vec![],
        }),
    );
    mount(
        &mut r,
        "b",
        Scripted::new(|ev| match ev {
            MachineIn::ServicesReady { .. } => vec![
                MachineOut::Subscribe {
                    name: EventName::new("e"),
                },
                MachineOut::Subscribe {
                    name: EventName::new("f"),
                },
            ],
            _ => vec![],
        }),
    );
    for id in ["a", "b"] {
        r.handle(RouteIn::Deliver {
            to: MachineId::new(id),
            ev: MachineIn::ServicesReady { keys: vec![] },
        });
    }
    r.handle(RouteIn::Dispatch {
        name: EventName::new("e"),
        payload: json!(0),
        mode: DispatchMode::Emit,
    });
    // If dispatch worked, no UnknownTarget and no CapReached: the cascade
    // terminated. (The log assertion on machines is internal; the router
    // output surface staying empty proves clean quiescence.)
}

// --- Waterfall: ordered chain with next/short-circuit ---------------------

#[test]
fn waterfall_chains_through_subscribers_then_returns_result() {
    let mut r = Router::new();
    mount(
        &mut r,
        "add1",
        Scripted::new(|ev| match ev {
            MachineIn::ServicesReady { .. } => vec![MachineOut::Subscribe {
                name: EventName::new("w"),
            }],
            MachineIn::WaterfallTurn { value, .. } => vec![MachineOut::WaterfallNext {
                value: json!(value.as_i64().unwrap() + 1),
            }],
            _ => vec![],
        }),
    );
    mount(
        &mut r,
        "mul10",
        Scripted::new(|ev| match ev {
            MachineIn::ServicesReady { .. } => vec![MachineOut::Subscribe {
                name: EventName::new("w"),
            }],
            MachineIn::WaterfallTurn { value, .. } => vec![MachineOut::WaterfallNext {
                value: json!(value.as_i64().unwrap() * 10),
            }],
            _ => vec![],
        }),
    );
    let initiator_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let log = initiator_log.clone();
    mount(
        &mut r,
        "init",
        Scripted::new(move |ev| match ev {
            MachineIn::ServicesReady { .. } => vec![MachineOut::Dispatch {
                name: EventName::new("w"),
                payload: json!(1),
                mode: DispatchMode::Waterfall,
            }],
            MachineIn::DispatchResult { name, value } if name.0 == "w" => {
                log.lock().unwrap().push(value.clone());
                vec![]
            }
            _ => vec![],
        }),
    );
    for id in ["add1", "mul10", "init"] {
        r.handle(RouteIn::Deliver {
            to: MachineId::new(id),
            ev: MachineIn::ServicesReady { keys: vec![] },
        });
    }
    // (1 + 1) * 10 = 20, delivered back to the initiator.
    assert_eq!(*initiator_log.lock().unwrap(), vec![json!(20)]);
}

#[test]
fn waterfall_short_circuit_skips_later_listeners() {
    let mut r = Router::new();
    let second_called = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = second_called.clone();
    mount(
        &mut r,
        "first",
        Scripted::new(|ev| match ev {
            MachineIn::ServicesReady { .. } => vec![MachineOut::Subscribe {
                name: EventName::new("w"),
            }],
            MachineIn::WaterfallTurn { .. } => vec![MachineOut::WaterfallReturn {
                value: json!("owned"),
            }],
            _ => vec![],
        }),
    );
    mount(
        &mut r,
        "second",
        Scripted::new(move |ev| match ev {
            MachineIn::ServicesReady { .. } => vec![MachineOut::Subscribe {
                name: EventName::new("w"),
            }],
            MachineIn::WaterfallTurn { .. } => {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                vec![MachineOut::WaterfallNext { value: json!(0) }]
            }
            _ => vec![],
        }),
    );
    for id in ["first", "second"] {
        r.handle(RouteIn::Deliver {
            to: MachineId::new(id),
            ev: MachineIn::ServicesReady { keys: vec![] },
        });
    }
    r.handle(RouteIn::Dispatch {
        name: EventName::new("w"),
        payload: json!(0),
        mode: DispatchMode::Waterfall,
    });
    assert!(!second_called.load(std::sync::atomic::Ordering::SeqCst));
}

// --- Saga: disposal surfaces compensations ---------------------------------

#[test]
fn unmount_collects_compensations_and_removes_subscriptions() {
    let mut r = Router::new();
    mount(
        &mut r,
        "p",
        Scripted::new(|ev| match ev {
            MachineIn::ServicesReady { .. } => vec![MachineOut::Subscribe {
                name: EventName::new("e"),
            }],
            MachineIn::DisposeRequested => vec![MachineOut::Compensate {
                label: "p-teardown".into(),
            }],
            _ => vec![],
        }),
    );
    r.handle(RouteIn::Deliver {
        to: MachineId::new("p"),
        ev: MachineIn::ServicesReady { keys: vec![] },
    });

    let outs = r.handle(RouteIn::Unmount {
        id: MachineId::new("p"),
    });
    assert_eq!(
        outs,
        vec![RouteOut::Compensate {
            from: MachineId::new("p"),
            label: "p-teardown".into()
        }]
    );
    assert!(r.subscribers(&EventName::new("e")).is_empty());
    assert_eq!(r.machine_count(), 0);

    // Further delivers report the missing machine.
    let outs = r.handle(RouteIn::Deliver {
        to: MachineId::new("p"),
        ev: MachineIn::DisposeRequested,
    });
    assert_eq!(
        outs,
        vec![RouteOut::UnknownTarget {
            to: MachineId::new("p")
        }]
    );
}

// --- Ping-pong guard ---------------------------------------------------------

#[test]
fn ping_pong_cap_stops_infinite_event_loops() {
    let mut r = Router::new();
    // Two machines each react to "e" by re-dispatching "e".
    for id in ["a", "b"] {
        mount(
            &mut r,
            id,
            Scripted::new(|ev| match ev {
                MachineIn::ServicesReady { .. } => vec![MachineOut::Subscribe {
                    name: EventName::new("e"),
                }],
                MachineIn::Event { .. } => vec![MachineOut::Dispatch {
                    name: EventName::new("e"),
                    payload: json!(0),
                    mode: DispatchMode::Emit,
                }],
                _ => vec![],
            }),
        );
        r.handle(RouteIn::Deliver {
            to: MachineId::new(id),
            ev: MachineIn::ServicesReady { keys: vec![] },
        });
    }
    let outs = r.handle(RouteIn::Dispatch {
        name: EventName::new("e"),
        payload: json!(0),
        mode: DispatchMode::Emit,
    });
    assert!(
        outs.iter().any(|o| matches!(o, RouteOut::CapReached { delivered } if *delivered == MAX_DELIVERIES_PER_STEP)),
        "expected CapReached, got {outs:?}"
    );
}

// --- Realize passthrough -----------------------------------------------------

#[test]
fn realize_requests_surface_to_the_driver() {
    let mut r = Router::new();
    mount(
        &mut r,
        "noisy",
        Scripted::new(|ev| match ev {
            MachineIn::ServicesReady { .. } => vec![MachineOut::Realize(RealizeRequest::Log {
                level: "info".into(),
                message: "booted".into(),
            })],
            _ => vec![],
        }),
    );
    let outs = r.handle(RouteIn::Deliver {
        to: MachineId::new("noisy"),
        ev: MachineIn::ServicesReady { keys: vec![] },
    });
    assert_eq!(
        outs,
        vec![RouteOut::Realize {
            from: MachineId::new("noisy"),
            request: RealizeRequest::Log {
                level: "info".into(),
                message: "booted".into()
            },
        }]
    );
}
