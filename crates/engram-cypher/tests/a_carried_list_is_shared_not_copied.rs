//! A list value SHARES its contents when a row is cloned.
//!
//! This is the property the whole `Value::List(Arc<Vec<Value>>)` change exists
//! for, and it is invisible to every other test: a deep copy and a shared
//! reference return identical answers, so correctness suites cannot tell them
//! apart. The difference is only in cost — and the cost is the defect. Measured
//! on SNB BI at SF3 with the output row count pinned at 24,328 and only the
//! CARRIED list's size varied, a query that carried ~5,000 elements took over
//! 100 s where the same query carrying nothing took 1 s, because a MATCH that
//! fans one row into many clones the row, and cloning a `Vec<Value>` copied
//! every element — each Node with its own labels `Vec` and props `BTreeMap`.
//!
//! So these tests assert the SHARING directly, by pointer identity. They are
//! the only thing standing between the fix and a future refactor that quietly
//! reintroduces the copy while every answer stays right.
//!
//! The second half matters just as much: sharing must stay INVISIBLE to the
//! language. A Cypher list is a value, not a reference — two lists with equal
//! contents are one value whether or not they share storage, and mutating one
//! must never be observable through another.

use std::collections::BTreeMap;
use std::sync::Arc;

use engram_cypher::{Scope, Value, VarMap, eval, parse_expression};

/// `xs + 99` through the real evaluator — the `+` a query writes, not a
/// private helper, so the test cannot pass while the language path differs.
fn plus_99(xs: Value) -> Value {
    let mut params = BTreeMap::new();
    params.insert("xs".to_string(), xs);
    let vars = VarMap::new();
    let scope = Scope::over(&params, &vars, None, None);
    let e = parse_expression("$xs + 99").expect("parses");
    eval(&e, &scope).expect("list + value appends")
}

fn list(n: usize) -> Value {
    Value::List(Arc::new((0..n as i64).map(Value::Int).collect()))
}

#[test]
fn cloning_a_list_value_copies_no_elements() {
    let a = list(1000);
    let b = a.clone();
    let (Value::List(x), Value::List(y)) = (&a, &b) else {
        panic!("both are lists")
    };
    assert!(
        Arc::ptr_eq(x, y),
        "a cloned list must share its storage, not copy 1000 elements"
    );
}

#[test]
fn a_row_carrying_a_list_clones_in_constant_work() {
    // the shape that actually bit: a row is a Vec<Value>, and a MATCH that
    // fans one row into many clones it per output row.
    let row = vec![Value::Int(7), list(5000), Value::Str("x".into())];
    let copies: Vec<Vec<Value>> = (0..100).map(|_| row.clone()).collect();
    let Value::List(origin) = &row[1] else {
        panic!("list")
    };
    for (i, c) in copies.iter().enumerate() {
        let Value::List(carried) = &c[1] else {
            panic!("list")
        };
        assert!(
            Arc::ptr_eq(origin, carried),
            "copy {i} deep-copied the carried list"
        );
    }
    assert_eq!(
        Arc::strong_count(origin),
        101,
        "one original plus 100 rows, all sharing one buffer"
    );
}

#[test]
fn two_equal_lists_are_equal_whether_or_not_they_share() {
    // sharing is an implementation fact; equality is the language's. A list
    // built independently must still compare equal to a shared one.
    let shared = list(4);
    let clone_of_it = shared.clone();
    let built_separately = list(4);
    assert_eq!(shared, clone_of_it);
    assert_eq!(shared, built_separately, "equality is by CONTENTS, not by Arc");
}

#[test]
fn appending_to_one_list_does_not_reach_the_other() {
    // `xs + [y]` goes through `Arc::make_mut`, which copies WHEN SHARED. If it
    // ever mutated in place under sharing, this is where a value would change
    // out from under an unrelated binding.
    let original = list(3);
    let carried = original.clone();
    let appended = plus_99(original.clone());

    let Value::List(items) = &appended else {
        panic!("list")
    };
    assert_eq!(items.len(), 4, "the appended copy grew");

    let Value::List(untouched) = &carried else {
        panic!("list")
    };
    assert_eq!(untouched.len(), 3, "the other binding did NOT grow");
    assert_eq!(carried, list(3), "and still holds its original contents");
}

#[test]
fn an_append_through_a_parameter_always_copies_and_that_is_correct() {
    // The counterpart to the test above, and a correction worth recording: an
    // earlier version of this file asserted that an append REUSES the buffer
    // when nobody else holds the list. That case cannot arise on this path.
    // Reading `$xs` clones the value out of the params map, so the map still
    // holds a reference when `+` runs — the list is ALWAYS shared here, and
    // `make_mut` always copies.
    //
    // Which is exactly what must happen: the copy is what keeps `$xs` itself
    // unchanged. In-place reuse is an optimisation available only to a sole
    // owner, never to a parameter, and claiming otherwise would have pinned a
    // behaviour the engine does not and must not have.
    let mut params = BTreeMap::new();
    params.insert("xs".to_string(), list(3));
    let vars = VarMap::new();
    let scope = Scope::over(&params, &vars, None, None);
    let e = parse_expression("$xs + 99").expect("parses");
    let grown = eval(&e, &scope).expect("append");

    let Value::List(after) = &grown else {
        panic!("list")
    };
    assert_eq!(after.len(), 4, "the result grew");

    let Value::List(param) = params.get("xs").expect("still bound") else {
        panic!("list")
    };
    assert_eq!(param.len(), 3, "the PARAMETER did not grow");
    assert!(
        !Arc::ptr_eq(after, param),
        "the append copied rather than writing through the parameter"
    );
}
