//! The catalogue's four invariants, each of which something else assumes.
//!
//! `lookup` is a binary search, so SORTEDNESS is a correctness precondition
//! rather than a tidiness preference: an entry in the wrong place is not found
//! at all, and the symptom is "the procedure I just added says it is
//! unsupported" — a long way from the cause.
//!
//! The other three are assumed by the interpreter: a non-empty `outputs` is
//! what a `YIELD`-less call returns, optional-arguments-last is what makes an
//! arity check a range test, and a lower-cased `name` is what the parser hands
//! to `lookup`.

use engram_proc::{CATALOG, ProcMode, lookup, names};

#[test]
fn the_catalogue_is_sorted_by_name() {
    for w in CATALOG.windows(2) {
        assert!(
            w[0].name < w[1].name,
            "catalogue out of order: `{}` must come after `{}`. `lookup` binary-searches, \
             so an entry in the wrong place is simply not found.",
            w[0].name,
            w[1].name,
        );
    }
}

#[test]
fn every_procedure_declares_at_least_one_output_column() {
    for s in CATALOG {
        assert!(
            !s.outputs.is_empty(),
            "`{}` declares no output columns. A YIELD-less call of it would produce \
             nothing, which is the behaviour the catalogue exists to end.",
            s.name,
        );
    }
}

#[test]
fn optional_arguments_come_last() {
    for s in CATALOG {
        let mut seen_optional = false;
        for arg in s.args {
            if arg.optional {
                seen_optional = true;
            } else {
                assert!(
                    !seen_optional,
                    "`{}` declares the required argument `{}` after an optional one, \
                     which makes its arity ambiguous.",
                    s.name, arg.name,
                );
            }
        }
    }
}

#[test]
fn every_name_is_the_lower_cased_spelling_the_parser_produces() {
    for s in CATALOG {
        assert_eq!(
            s.name,
            s.name.to_lowercase(),
            "`{}` is not lower-cased; the parser lower-cases every procedure name \
             before it reaches `lookup`, so this entry could never be found.",
            s.name,
        );
        assert_eq!(
            s.name,
            s.display.to_lowercase(),
            "`{}` and `{}` are not the same name in two spellings.",
            s.name,
            s.display,
        );
    }
}

#[test]
fn every_catalogued_name_is_findable() {
    for name in names() {
        assert!(
            lookup(name).is_some(),
            "`{name}` is in the catalogue but `lookup` cannot find it.",
        );
    }
}

// ─── The negatives ─────────────────────────────────────────────────────────

#[test]
fn an_uncatalogued_name_is_not_found_and_is_not_read_only() {
    assert!(lookup("db.nosuchprocedure").is_none());
    // Fail closed. An unknown procedure is refused at run time regardless, and
    // calling it read-only in the meantime would admit it to a read
    // transaction on the strength of nothing at all.
    assert!(
        !engram_proc::is_read_only("db.nosuchprocedure"),
        "an unknown procedure must not be classified read-only",
    );
}

#[test]
fn a_write_procedure_is_not_classified_read_only() {
    let sig = lookup("engram.checkpoint").expect("engram.checkpoint is catalogued");
    assert_eq!(sig.mode, ProcMode::Write);
    assert!(
        !engram_proc::is_read_only("engram.checkpoint"),
        "engram.checkpoint mutates and must not be classified read-only",
    );
}

#[test]
fn a_prefix_of_a_catalogued_name_is_not_itself_catalogued() {
    // The classification this replaced was a PREFIX match: anything starting
    // `db.index.` counted as read-only, implemented or not. Pin that prefixes
    // no longer resolve, so a future `db.index.fulltext.createNodeIndex` is
    // classified by its own entry rather than by its ancestors' spelling.
    assert!(lookup("db.index").is_none());
    assert!(lookup("db.index.fulltext").is_none());
    assert!(!engram_proc::is_read_only(
        "db.index.fulltext.createnodeindex"
    ));
}

#[test]
fn arity_is_a_range_when_an_argument_is_optional() {
    let await_ = lookup("db.awaitindexes").expect("db.awaitIndexes is catalogued");
    assert_eq!(await_.required_args(), 0);
    assert!(await_.accepts_arity(0));
    assert!(await_.accepts_arity(1));
    assert!(!await_.accepts_arity(2));

    let vector = lookup("db.index.vector.querynodes").expect("catalogued");
    assert_eq!(vector.required_args(), 3);
    assert!(!vector.accepts_arity(2));
    assert!(vector.accepts_arity(3));
    assert!(!vector.accepts_arity(4));
}

#[test]
fn a_yield_field_the_procedure_does_not_declare_is_not_accepted() {
    let sig = lookup("db.labels").expect("db.labels is catalogued");
    assert!(sig.yields("label"));
    assert!(!sig.yields("labels"));
    assert!(!sig.yields("node"));
    assert_eq!(sig.output_list(), "label");
}

#[test]
fn the_two_fixed_signature_procedures_still_yield_node_and_score() {
    // These two are bound by production call sites. If this test ever needs
    // changing, the change is a break for every one of them.
    for name in ["db.index.vector.querynodes", "db.index.fulltext.querynodes"] {
        let sig = lookup(name).expect("catalogued");
        assert_eq!(
            sig.output_list(),
            "node, score",
            "`{name}` has a fixed two-column signature that production call sites depend on",
        );
    }
}
