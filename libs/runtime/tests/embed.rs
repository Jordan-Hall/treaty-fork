//! The embedding API of `JsRuntime::embedded()`: host functions with JSON marshalling, error capture, `call`,
//! and an event loop the host pumps itself on an injectable clock (TREATY-ISSUES T5).

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use treaty_runtime::{JsRuntime, RuntimeError};

#[test]
fn host_function_receives_json_arguments_and_returns_json() {
    let mut rt = JsRuntime::embedded();
    rt.register_host_function("add", |args| {
        let a = args[0].as_f64().ok_or("first argument is not a number")?;
        let b = args[1].as_f64().ok_or("second argument is not a number")?;
        Ok(json!(a + b))
    })
    .unwrap();
    assert_eq!(rt.eval("add(2, 3)").unwrap(), json!(5));
    // Structured values cross in both directions as JSON.
    rt.register_host_function("echo", |args| Ok(args[0].clone())).unwrap();
    assert_eq!(
        rt.eval("echo({ name: 'treaty', list: [1, 'two', null], ok: true })").unwrap(),
        json!({ "name": "treaty", "list": [1, "two", null], "ok": true })
    );
}

#[test]
fn host_error_is_catchable_in_javascript_and_surfaces_when_uncaught() {
    let mut rt = JsRuntime::embedded();
    rt.register_host_function("fail", |_| Err("boom from the host".to_string())).unwrap();
    assert_eq!(
        rt.eval("try { fail(); 'no' } catch (e) { e.message }").unwrap(),
        json!("boom from the host")
    );
    match rt.eval("fail()") {
        Err(RuntimeError::Runtime(message)) => assert!(message.contains("boom from the host"), "{message}"),
        other => panic!("expected a runtime error, got {other:?}"),
    }
}

#[test]
fn calling_an_unknown_host_function_is_an_error_not_a_crash() {
    let mut rt = JsRuntime::embedded();
    assert!(matches!(
        rt.eval("try { globalThis['__treaty_host_call']('missing', '[]') } catch (e) { e.message }"),
        Ok(Value::String(_))
    ));
}

#[test]
fn call_invokes_a_javascript_function_with_json_arguments() {
    let mut rt = JsRuntime::embedded();
    rt.eval("globalThis.double = (x) => x * 2; globalThis.join = (a, b) => `${a}-${b}`;").unwrap();
    assert_eq!(rt.call("double", &[json!(21)]).unwrap(), json!(42));
    assert_eq!(rt.call("join", &[json!("a"), json!(7)]).unwrap(), json!("a-7"));
    assert!(matches!(rt.call("nope", &[]), Err(RuntimeError::Runtime(_))));
}

#[test]
fn eval_does_not_run_promise_jobs_until_the_host_pumps() {
    let mut rt = JsRuntime::embedded();
    rt.eval("globalThis.v = 0; Promise.resolve().then(() => { globalThis.v = 1; });").unwrap();
    assert_eq!(rt.eval("globalThis.v").unwrap(), json!(0), "eval does not drain the loop");
    assert!(!rt.is_idle());
    let report = rt.pump().unwrap();
    assert!(report.jobs_run >= 1, "{report:?}");
    assert!(report.idle, "{report:?}");
    assert_eq!(rt.eval("globalThis.v").unwrap(), json!(1));
}

/// Timers come from `Atomics.waitAsync(…, timeout)`, the one timeout source Nova produces (its timeout job is
/// enqueued through the host's timer hook). With an injected clock the host decides when they are due: the pump
/// leaves a timer that is not yet due in place and reports its deadline, and runs it once the clock reaches it.
/// No real time passes. (The wait's promise value is not asserted: see the ignored Nova test below.)
#[test]
fn timers_run_when_the_injected_clock_reaches_their_deadline() {
    let base = Instant::now();
    let virtual_ms = Rc::new(Cell::new(0u64));
    let clock = {
        let virtual_ms = virtual_ms.clone();
        move || base + Duration::from_millis(virtual_ms.get())
    };
    let mut rt = JsRuntime::embedded();
    rt.set_clock(clock).unwrap();
    rt.eval("Atomics.waitAsync(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);").unwrap();

    // The wait's own job runs at once; its 100 ms timeout stays scheduled, and the deadline is reported.
    let first = rt.pump().unwrap();
    assert!(!first.idle, "{first:?}");
    let deadline = first.next_timer.expect("the wait's timeout is scheduled");
    assert_eq!(deadline, base + Duration::from_millis(100), "deadline = clock now + timeout");

    // Before the deadline: nothing is due.
    virtual_ms.set(50);
    let early = rt.pump().unwrap();
    assert_eq!(early.jobs_run, 0, "{early:?}");
    assert_eq!(rt.next_timer_deadline(), Some(deadline));

    // At and after the deadline: the timer runs and the loop goes idle.
    virtual_ms.set(150);
    let late = rt.pump().unwrap();
    assert_eq!(late.jobs_run, 1, "{late:?}");
    assert_eq!(late.next_timer, None);
    assert!(late.idle);
}

/// Upstream Nova defect, recorded so it can be re-checked on a newer Nova: `Atomics.waitAsync` resolves its
/// promise to `"ok"` as soon as its generic job runs, even though nothing notified the waiter. Per the spec the
/// promise stays pending until a notify or the timeout. Ignored until the pinned Nova rev is fixed.
#[test]
#[ignore = "upstream Nova: WaitAsyncJob::run resolves \"ok\" without a notify (atomics_object.rs)"]
fn waitasync_stays_pending_until_notified_or_timed_out() {
    let base = Instant::now();
    let mut rt = JsRuntime::embedded();
    rt.set_clock(move || base).unwrap();
    rt.eval(
        "globalThis.result = 'pending';\n\
         Atomics.waitAsync(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100).value\n\
           .then((v) => { globalThis.result = v; });",
    )
    .unwrap();
    rt.pump().unwrap();
    assert_eq!(rt.eval("globalThis.result").unwrap(), json!("pending"));
}

/// A throw inside a promise reaction is not a host error: JavaScript rejects the derived promise with it, so the
/// pump completes and the error is captured in the script, where a `catch` can observe it.
#[test]
fn an_error_in_a_promise_reaction_is_captured_by_the_script_not_the_host() {
    let mut rt = JsRuntime::embedded();
    rt.eval(
        "globalThis.caught = null;\n\
         Promise.resolve().then(() => { throw new Error('job exploded'); })\n\
           .catch((e) => { globalThis.caught = e.message; });",
    )
    .unwrap();
    rt.pump().expect("the pump completes; the rejection is handled by the script");
    assert_eq!(rt.eval("globalThis.caught").unwrap(), json!("job exploded"));
}

#[test]
fn an_embedded_runtime_has_no_node_globals() {
    let mut rt = JsRuntime::embedded();
    assert_eq!(rt.eval("typeof process").unwrap(), json!("undefined"));
    assert_eq!(rt.eval("typeof setTimeout").unwrap(), json!("undefined"));
}

#[test]
fn host_registration_needs_an_embedded_runtime() {
    let mut plain = JsRuntime::new();
    assert!(matches!(
        plain.register_host_function("f", |_| Ok(Value::Null)),
        Err(RuntimeError::Runtime(_))
    ));
    assert!(matches!(plain.set_clock(Instant::now), Err(RuntimeError::Runtime(_))));
    // A plain runtime's pump is a no-op rather than an error.
    assert_eq!(plain.pump().unwrap().jobs_run, 0);
}

#[test]
fn the_pure_engine_and_node_compat_runtimes_keep_their_behaviour() {
    // Existing surface: eval on a plain runtime, and the Node globals on the node-compat runtime.
    let mut plain = JsRuntime::new();
    assert_eq!(plain.eval("1 + 1").unwrap(), json!(2));
    let mut node = JsRuntime::with_node_compat();
    assert_eq!(node.eval("typeof process").unwrap(), json!("object"));
}
