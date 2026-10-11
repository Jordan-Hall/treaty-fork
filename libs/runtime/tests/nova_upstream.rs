//! Upstream Nova defects found by the downstream Angular work (`ENGINE-NOTES.md` E1, E2, E8, E11 in
//! `native-angular/phase2`). Each test states the expected JavaScript result and is `#[ignore]`d: they fail on the
//! pinned Nova rev, and they document the defect for a Nova upgrade. Run them with `cargo test -- --ignored`.
//! E1 is a VM assertion that aborts the process rather than a JavaScript error, so its test panics on this rev.

use serde_json::json;
use treaty_runtime::JsRuntime;

#[test]
#[ignore = "upstream Nova E1: super inside an arrow in a class field initialiser asserts in the VM (execute_instructions.rs)"]
fn e1_super_in_a_field_arrow_returns_the_parent_method_result() {
    let mut rt = JsRuntime::new();
    let value = rt
        .eval("class A { m() { return 2 } }; class B extends A { f = () => super.m(); }; new B().f()")
        .expect("E1 must not abort");
    assert_eq!(value, json!(2));
}

#[test]
#[ignore = "upstream Nova E2: class fields with an explicit constructor that calls super() throw 'Uninitialized this binding'"]
fn e2_field_initialiser_runs_after_super_in_an_explicit_constructor() {
    let mut rt = JsRuntime::new();
    let value = rt
        .eval("class A {} class B extends A { x = 1; constructor() { super(); } }; new B().x")
        .expect("E2 must not throw");
    assert_eq!(value, json!(1));
}

#[test]
#[ignore = "upstream Nova E8: named capture groups read back undefined"]
fn e8_named_capture_groups_are_readable() {
    let mut rt = JsRuntime::new();
    assert_eq!(rt.eval(r"/(?<m>\d+)/.exec('ab12').groups.m").unwrap(), json!("12"));
}

#[test]
#[ignore = "upstream Nova E11: RegExp look-behind is not supported (the router's URL check fails)"]
fn e11_regexp_lookbehind_is_supported() {
    let mut rt = JsRuntime::new();
    assert_eq!(rt.eval(r"/(?<=a)b/.test('ab')").unwrap(), json!(true));
}
