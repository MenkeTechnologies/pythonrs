//! `_opcode` — the classification tables `opcode.py` builds its public lists from.
//!
//! This module is pure METADATA about CPython's instruction set: `has_arg(op)`,
//! `has_jump(op)` and friends are predicates over opcode NUMBERS, and `opcode.py`
//! turns them into the `hasarg`/`hasjump`/`hasconst` lists that `dis` documents.
//! None of it describes how pythonrs executes anything — pythonrs compiles to
//! fusevm bytecode, not to CPython's — so `dis` over a pythonrs function is not
//! meaningful and is not claimed to be. What IS needed is that the tables exist
//! and are correct, because `inspect` imports `dis`, and `traceback`, `logging`,
//! `unittest`, `hashlib` and `dataclasses` all import `inspect`.
//!
//! The sets below are CPython 3.14's own answers, transcribed rather than
//! re-derived: they are generated data in CPython too (`Include/opcode_ids.h`),
//! and inventing a second derivation would only create a way to disagree.

use crate::host::{PyHost, PyObj};
use fusevm::Value;

const HAS_ARG: &[i64] = &[44,45,46,47,48,49,50,51,52,53,54,55,56,57,58,59,60,61,62,63,64,65,66,67,68,69,70,71,72,73,74,75,76,77,78,79,80,81,82,83,84,85,86,87,88,89,90,91,92,93,94,95,96,97,98,99,100,101,102,103,104,105,106,107,108,109,110,111,112,113,114,115,116,117,118,119,120,128,143,144,145,146,147,148,149,150,151,152,153,155,156,157,158,159,160,161,162,163,164,165,166,167,168,169,170,171,172,173,174,175,176,177,178,179,180,181,182,183,184,185,186,187,188,189,190,191,192,193,194,195,197,200,209,210,211,237,239,241,242,243,244,245,247,248,249,250,251,253,255,257,258,259,260,261,263,264,265,266];
const HAS_CONST: &[i64] = &[82,190,191];
const HAS_NAME: &[i64] = &[61,64,65,72,73,80,91,92,93,96,110,115,116,179,189,194,195,200,249];
const HAS_JUMP: &[i64] = &[68,70,75,76,77,100,101,102,103,106,172,173,174,175,176,237,248,257,258,259,260];
const HAS_FREE: &[i64] = &[62,90,97,111];
const HAS_LOCAL: &[i64] = &[3,63,83,84,85,86,87,88,89,112,113,114,261,266];
const HAS_EXC: &[i64] = &[263,264,265];

const INTRINSIC1: &[&str] = &["INTRINSIC_1_INVALID", "INTRINSIC_PRINT", "INTRINSIC_IMPORT_STAR", "INTRINSIC_STOPITERATION_ERROR", "INTRINSIC_ASYNC_GEN_WRAP", "INTRINSIC_UNARY_POSITIVE", "INTRINSIC_LIST_TO_TUPLE", "INTRINSIC_TYPEVAR", "INTRINSIC_PARAMSPEC", "INTRINSIC_TYPEVARTUPLE", "INTRINSIC_SUBSCRIPT_GENERIC", "INTRINSIC_TYPEALIAS"];
const INTRINSIC2: &[&str] = &["INTRINSIC_2_INVALID", "INTRINSIC_PREP_RERAISE_STAR", "INTRINSIC_TYPEVAR_WITH_BOUND", "INTRINSIC_TYPEVAR_WITH_CONSTRAINTS", "INTRINSIC_SET_FUNCTION_TYPE_PARAMS", "INTRINSIC_SET_TYPEPARAM_DEFAULT"];
const SPECIAL_METHODS: &[&str] = &["__enter__", "__exit__", "__aenter__", "__aexit__"];
const NB_OPS: &[(&str, &str)] = &[("NB_ADD", "+"), ("NB_AND", "&"), ("NB_FLOOR_DIVIDE", "//"), ("NB_LSHIFT", "<<"), ("NB_MATRIX_MULTIPLY", "@"), ("NB_MULTIPLY", "*"), ("NB_REMAINDER", "%"), ("NB_OR", "|"), ("NB_POWER", "**"), ("NB_RSHIFT", ">>"), ("NB_SUBTRACT", "-"), ("NB_TRUE_DIVIDE", "/"), ("NB_XOR", "^"), ("NB_INPLACE_ADD", "+="), ("NB_INPLACE_AND", "&="), ("NB_INPLACE_FLOOR_DIVIDE", "//="), ("NB_INPLACE_LSHIFT", "<<="), ("NB_INPLACE_MATRIX_MULTIPLY", "@="), ("NB_INPLACE_MULTIPLY", "*="), ("NB_INPLACE_REMAINDER", "%="), ("NB_INPLACE_OR", "|="), ("NB_INPLACE_POWER", "**="), ("NB_INPLACE_RSHIFT", ">>="), ("NB_INPLACE_SUBTRACT", "-="), ("NB_INPLACE_TRUE_DIVIDE", "/="), ("NB_INPLACE_XOR", "^="), ("NB_SUBSCR", "[]")];

/// `_opcode.<fn>(...)`.
pub fn call(h: &mut PyHost, name: &str, args: &[Value]) -> Option<Result<Value, String>> {
    // The seven predicates share one shape: is this opcode number in the set?
    let table: Option<&[i64]> = match name {
        "has_arg" => Some(HAS_ARG),
        "has_const" => Some(HAS_CONST),
        "has_name" => Some(HAS_NAME),
        "has_jump" => Some(HAS_JUMP),
        "has_free" => Some(HAS_FREE),
        "has_local" => Some(HAS_LOCAL),
        "has_exc" => Some(HAS_EXC),
        _ => None,
    };
    if let Some(table) = table {
        let op = args.first().and_then(|v| h.as_int(v))?;
        return Some(Ok(Value::Bool(table.contains(&op))));
    }
    Some(match name {
        "get_intrinsic1_descs" => Ok(str_tuple(h, INTRINSIC1)),
        "get_intrinsic2_descs" => Ok(str_tuple(h, INTRINSIC2)),
        "get_special_method_names" => Ok(str_tuple(h, SPECIAL_METHODS)),
        "get_nb_ops" => {
            let rows: Vec<Value> = NB_OPS
                .iter()
                .map(|(name, symbol)| {
                    let n = h.new_str((*name).to_string());
                    let s = h.new_str((*symbol).to_string());
                    h.new_tuple(vec![n, s])
                })
                .collect();
            Ok(h.new_list(rows))
        }
        // An opcode number is valid when it is one CPython defines.
        "is_valid" => {
            let op = args.first().and_then(|v| h.as_int(v)).unwrap_or(-1);
            Ok(Value::Bool((0..=260).contains(&op)))
        }
        // `stack_effect` describes CPython's evaluation stack, which pythonrs does
        // not have. Reporting 0 is honest for a runtime that never executes these
        // instructions; `opcode.py` only re-exports the function.
        "stack_effect" => Ok(Value::Int(0)),
        "get_executor" | "get_specialization_stats" => Ok(Value::Undef),
        _ => return None,
    })
}

fn str_tuple(h: &mut PyHost, items: &[&str]) -> Value {
    let vals: Vec<Value> = items.iter().map(|s| h.new_str((*s).to_string())).collect();
    h.new_tuple(vals)
}

/// The `_opcode` namespace.
pub fn entries(h: &mut PyHost) -> Vec<(String, Value)> {
    const FNS: &[&str] = &[
        "has_arg",
        "has_const",
        "has_name",
        "has_jump",
        "has_free",
        "has_local",
        "has_exc",
        "is_valid",
        "stack_effect",
        "get_intrinsic1_descs",
        "get_intrinsic2_descs",
        "get_special_method_names",
        "get_nb_ops",
        "get_executor",
        "get_specialization_stats",
    ];
    let mut out: Vec<(String, Value)> = FNS
        .iter()
        .map(|f| ((*f).to_string(), h.alloc(PyObj::Builtin(format!("_opcode.{f}")))))
        .collect();
    // Specialization is a CPython interpreter feature; pythonrs has its own JIT
    // and never runs these instructions, so the flags are off.
    out.push(("ENABLE_SPECIALIZATION".to_string(), Value::Bool(false)));
    out.push(("ENABLE_SPECIALIZATION_FT".to_string(), Value::Bool(false)));
    out
}
