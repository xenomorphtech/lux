//! Generates the C runtime source that is compiled and linked with the
//! Cranelift-generated object file to produce a native executable.
//!
//! The runtime provides:
//! - Tagged value representation (64-bit values with 3-bit tag)
//! - Simple bump-allocator heap
//! - Atom table (pre-populated from compiler)
//! - Tuple and list operations
//! - Print / IO support
//! - `main()` entry point that calls the Lux entry function

/// Tag constants — must match the values used by the Cranelift compiler.
pub const TAG_BITS: i64 = 3;
pub const TAG_MASK: i64 = 0x7;
pub const TAG_TUPLE: i64 = 0; // pointer to (arity, elem0, elem1, ...)
pub const TAG_INT: i64 = 1; // immediate small integer
pub const TAG_CONS: i64 = 2; // pointer to (head, tail)
pub const TAG_ATOM: i64 = 3; // immediate atom index
pub const TAG_BOXED: i64 = 4; // pointer to boxed data (strings, floats)
pub const TAG_FUN: i64 = 6; // pointer to closure [func_ptr, n_caps, cap0, ...]
pub const TAG_NIL: i64 = 7; // special nil constant (entire word)

pub const _BOXED_STRING: i64 = 1;
pub const BOXED_FLOAT: i64 = 2;

pub const VALUE_NIL: i64 = TAG_NIL; // 0b111 = 7
pub const VALUE_TRUE: i64 = (0 << TAG_BITS) | TAG_ATOM; // atom index 0 = "true"
pub const VALUE_FALSE: i64 = (1 << TAG_BITS) | TAG_ATOM; // atom index 1 = "false"

/// The first N atom indices are reserved for well-known atoms.
pub const RESERVED_ATOMS: &[&str] = &["true", "false", "ok", "error", "unit"];

#[inline]
pub fn make_tagged_int(n: i64) -> i64 {
    (n << TAG_BITS) | TAG_INT
}

#[inline]
pub fn make_tagged_atom(index: usize) -> i64 {
    ((index as i64) << TAG_BITS) | TAG_ATOM
}

/// Generate the C runtime source code.
///
/// `atoms` is the complete atom table (including the reserved atoms at the front).
/// `entry_symbol` is the linker symbol for the Lux entry function.
pub fn generate_runtime_c(atoms: &[String], entry_symbol: &str, entry_arity: usize) -> String {
    let mut c = String::with_capacity(8192);

    c.push_str(
        r#"/* Lux native runtime — auto-generated */
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <inttypes.h>
#include <setjmp.h>

typedef int64_t Value;

/* ---- Tag constants ---- */
#define TAG_BITS  3
#define TAG_MASK  0x7LL
#define TAG_TUPLE 0LL
#define TAG_INT   1LL
#define TAG_CONS  2LL
#define TAG_ATOM  3LL
#define TAG_BOXED 4LL
#define TAG_FUN   6LL
#define TAG_NIL   7LL

#define VALUE_NIL   TAG_NIL
#define VALUE_TRUE  ((0LL << TAG_BITS) | TAG_ATOM)
#define VALUE_FALSE ((1LL << TAG_BITS) | TAG_ATOM)

/* ---- Heap ---- */
#define HEAP_SIZE (64 * 1024 * 1024)
static char heap[HEAP_SIZE];
static size_t heap_ptr = 0;

void* lux_alloc(size_t bytes) {
    size_t aligned = (bytes + 7) & ~(size_t)7;
    if (heap_ptr + aligned > HEAP_SIZE) {
        fprintf(stderr, "lux: heap exhausted\n");
        exit(1);
    }
    void* ptr = &heap[heap_ptr];
    heap_ptr += aligned;
    return ptr;
}

"#,
    );

    // Atom table
    c.push_str(
        "/* Forward declarations */
static void print_value_inline(Value v);
Value lux_rt_make_tuple2(Value a, Value b);
Value lux_rt_make_cons(Value head, Value tail);
Value lux_rt_apply0(Value closure);
Value lux_rt_apply1(Value closure, Value arg0);
Value lux_rt_apply2(Value closure, Value arg0, Value arg1);
Value lux_rt_apply3(Value closure, Value arg0, Value arg1, Value arg2);
Value lux_rt_value_equal(Value a, Value b);
Value lux_rt_byte_size(Value v);

/* ---- Try/Catch support ---- */
/* We use setjmp/longjmp for exception handling.
   try_call_1 wraps a closure call in setjmp so longjmp works correctly
   because setjmp's frame stays on the stack during the closure execution. */
#define MAX_TRY_DEPTH 64
static jmp_buf try_stack[MAX_TRY_DEPTH];
static Value try_error_val[MAX_TRY_DEPTH];
static int try_depth = 0;

static void lux_rt_raise(Value reason) {
    if (try_depth > 0) {
        try_depth--;
        try_error_val[try_depth] = reason;
        longjmp(try_stack[try_depth], 1);
    }
    fprintf(stderr, \"lux: uncaught error: \");
    print_value_inline(reason);
    fprintf(stderr, \"\\n\");
    exit(1);
}

/* Try calling a 0-arity closure. Returns {ok, result} or {error, reason}. */
Value lux_rt_try_call(Value closure) {
    if (try_depth >= MAX_TRY_DEPTH) { fprintf(stderr, \"lux: try stack overflow\\n\"); exit(1); }
    /* setjmp called here — this frame stays active while the closure runs */
    if (setjmp(try_stack[try_depth]) == 0) {
        try_depth++;
        Value result = lux_rt_apply1(closure, VALUE_NIL);
        try_depth--;
        /* Return {ok, result} — use atom indices: ok=2 */
        return lux_rt_make_tuple2(((2LL << TAG_BITS) | TAG_ATOM), result);
    } else {
        /* Exception caught — try_depth already decremented by lux_rt_raise */
        Value reason = try_error_val[try_depth];
        /* Return {error, reason} — use atom indices: error=3 */
        return lux_rt_make_tuple2(((3LL << TAG_BITS) | TAG_ATOM), reason);
    }
}

void lux_rt_try_end(void) {
    if (try_depth > 0) try_depth--;
}

Value lux_rt_try_get_error(void) {
    return try_depth >= 0 && try_depth < MAX_TRY_DEPTH ? try_error_val[try_depth] : VALUE_NIL;
}

/* Compatibility wrappers for the compiler's direct try_begin interface */
int lux_rt_try_begin(void) {
    if (try_depth >= MAX_TRY_DEPTH) return 1;
    if (setjmp(try_stack[try_depth]) == 0) { try_depth++; return 0; }
    return 1;
}

/* Safe division — raises on division by zero */
Value lux_rt_safe_div(Value a, Value b) {
    int64_t bv = b >> TAG_BITS;
    if (bv == 0) { lux_rt_raise(((3LL << TAG_BITS) | TAG_ATOM)); return VALUE_NIL; }
    int64_t av = a >> TAG_BITS;
    int64_t result = av / bv;
    return (result << TAG_BITS) | TAG_INT;
}

Value lux_rt_safe_rem(Value a, Value b) {
    int64_t bv = b >> TAG_BITS;
    if (bv == 0) { lux_rt_raise(((3LL << TAG_BITS) | TAG_ATOM)); return VALUE_NIL; }
    int64_t av = a >> TAG_BITS;
    int64_t result = av % bv;
    return (result << TAG_BITS) | TAG_INT;
}

/* ---- Atom table ---- */\n",
    );
    c.push_str(&format!(
        "static const char* atom_names[{}] = {{\n",
        atoms.len().max(1)
    ));
    for (i, name) in atoms.iter().enumerate() {
        // Escape the atom name for C string literal
        let escaped = name.replace('\\', "\\\\").replace('"', "\\\"");
        c.push_str(&format!(
            "    \"{}\"{}\n",
            escaped,
            if i + 1 < atoms.len() { "," } else { "" }
        ));
    }
    c.push_str("};\n");
    c.push_str(&format!("static int num_atoms = {};\n\n", atoms.len()));

    c.push_str(
        r#"/* ---- Value constructors ---- */
Value lux_rt_make_int(int64_t n) {
    return (n << TAG_BITS) | TAG_INT;
}

int64_t lux_rt_unbox_int(Value v) {
    return v >> TAG_BITS;
}

Value lux_rt_make_atom(int64_t index) {
    return (index << TAG_BITS) | TAG_ATOM;
}

int64_t lux_rt_atom_index(Value v) {
    return v >> TAG_BITS;
}

Value lux_rt_make_tuple2(Value a, Value b) {
    Value* ptr = (Value*)lux_alloc(3 * sizeof(Value));
    ptr[0] = 2;
    ptr[1] = a;
    ptr[2] = b;
    return ((int64_t)(intptr_t)ptr) | TAG_TUPLE;
}

Value lux_rt_make_tuple3(Value a, Value b, Value c) {
    Value* ptr = (Value*)lux_alloc(4 * sizeof(Value));
    ptr[0] = 3;
    ptr[1] = a;
    ptr[2] = b;
    ptr[3] = c;
    return ((int64_t)(intptr_t)ptr) | TAG_TUPLE;
}

Value lux_rt_make_tuple_n(int64_t arity, Value* elems) {
    Value* ptr = (Value*)lux_alloc((arity + 1) * sizeof(Value));
    ptr[0] = arity;
    for (int64_t i = 0; i < arity; i++) {
        ptr[i + 1] = elems[i];
    }
    return ((int64_t)(intptr_t)ptr) | TAG_TUPLE;
}

int64_t lux_rt_tuple_arity(Value tuple) {
    Value* ptr = (Value*)((intptr_t)(tuple & ~TAG_MASK));
    return ptr[0];
}

Value lux_rt_tuple_element(Value tuple, int64_t index) {
    Value* ptr = (Value*)((intptr_t)(tuple & ~TAG_MASK));
    return ptr[index + 1];
}

Value lux_rt_make_cons(Value head, Value tail) {
    Value* ptr = (Value*)lux_alloc(2 * sizeof(Value));
    ptr[0] = head;
    ptr[1] = tail;
    return ((int64_t)(intptr_t)ptr) | TAG_CONS;
}

Value lux_rt_cons_head(Value cons) {
    Value* ptr = (Value*)((intptr_t)(cons & ~TAG_MASK));
    return ptr[0];
}

Value lux_rt_cons_tail(Value cons) {
    Value* ptr = (Value*)((intptr_t)(cons & ~TAG_MASK));
    return ptr[1];
}

Value lux_rt_list_length(Value list) {
    int64_t len = 0;
    while ((list & TAG_MASK) == TAG_CONS) {
        len++;
        Value* ptr = (Value*)((intptr_t)(list & ~TAG_MASK));
        list = ptr[1];
    }
    return (len << TAG_BITS) | TAG_INT;
}

/* ---- String support ---- */
/* Boxed strings: TAG_BOXED pointer to [type_tag=1, length, char data...] */
#define BOXED_STRING 1
#define BOXED_FLOAT  2

Value lux_rt_make_string(const char* data, int64_t len) {
    /* layout: [type_tag, length, bytes...] */
    int64_t header_size = 2 * sizeof(Value);
    char* ptr = (char*)lux_alloc(header_size + len + 1);
    ((Value*)ptr)[0] = BOXED_STRING;
    ((Value*)ptr)[1] = len;
    memcpy(ptr + header_size, data, len);
    ptr[header_size + len] = '\0';
    return ((int64_t)(intptr_t)ptr) | TAG_BOXED;
}

const char* lux_rt_string_data(Value v) {
    char* ptr = (char*)((intptr_t)(v & ~TAG_MASK));
    return ptr + 2 * sizeof(Value);
}

int64_t lux_rt_string_length(Value v) {
    Value* ptr = (Value*)((intptr_t)(v & ~TAG_MASK));
    return ptr[1];
}

Value lux_rt_string_concat(Value a, Value b) {
    const char* a_data = lux_rt_string_data(a);
    int64_t a_len = lux_rt_string_length(a);
    const char* b_data = lux_rt_string_data(b);
    int64_t b_len = lux_rt_string_length(b);
    int64_t total = a_len + b_len;
    int64_t header_size = 2 * sizeof(Value);
    char* ptr = (char*)lux_alloc(header_size + total + 1);
    ((Value*)ptr)[0] = BOXED_STRING;
    ((Value*)ptr)[1] = total;
    memcpy(ptr + header_size, a_data, a_len);
    memcpy(ptr + header_size + a_len, b_data, b_len);
    ptr[header_size + total] = '\0';
    return ((int64_t)(intptr_t)ptr) | TAG_BOXED;
}

Value lux_rt_binary_at(Value value, Value index_value) {
    if ((value & TAG_MASK) != TAG_BOXED || (index_value & TAG_MASK) != TAG_INT) {
        return VALUE_NIL;
    }
    Value* boxed = (Value*)((intptr_t)(value & ~TAG_MASK));
    if (boxed[0] != BOXED_STRING) return VALUE_NIL;
    int64_t index = index_value >> TAG_BITS;
    if (index < 0 || index >= lux_rt_string_length(value)) {
        return VALUE_NIL;
    }
    unsigned char byte = (unsigned char)lux_rt_string_data(value)[index];
    return (((int64_t)byte) << TAG_BITS) | TAG_INT;
}

Value lux_rt_binary_slice(Value value, Value start_value, Value length_value) {
    if ((value & TAG_MASK) != TAG_BOXED ||
        (start_value & TAG_MASK) != TAG_INT ||
        (length_value & TAG_MASK) != TAG_INT) {
        return VALUE_NIL;
    }
    Value* boxed = (Value*)((intptr_t)(value & ~TAG_MASK));
    if (boxed[0] != BOXED_STRING) return VALUE_NIL;
    int64_t start = start_value >> TAG_BITS;
    int64_t length = length_value >> TAG_BITS;
    int64_t available = lux_rt_string_length(value);
    if (start < 0 || length < 0 || start > available || length > available - start) {
        return VALUE_NIL;
    }
    return lux_rt_make_string(lux_rt_string_data(value) + start, length);
}

/* ---- IO ---- */
static void print_value(Value v);

static void print_value_inline(Value v) {
    int64_t tag = v & TAG_MASK;
    switch (tag) {
    case TAG_INT:
        printf("%" PRId64, v >> TAG_BITS);
        break;
    case TAG_ATOM: {
        int64_t idx = v >> TAG_BITS;
        if (idx >= 0 && idx < num_atoms)
            printf(":%s", atom_names[idx]);
        else
            printf(":atom_%d", (int)idx);
        break;
    }
    case TAG_NIL:
        printf("[]");
        break;
    case TAG_TUPLE: {
        Value* ptr = (Value*)((intptr_t)(v & ~TAG_MASK));
        int64_t arity = ptr[0];
        printf("{");
        for (int64_t i = 0; i < arity; i++) {
            if (i > 0) printf(", ");
            print_value_inline(ptr[i + 1]);
        }
        printf("}");
        break;
    }
    case TAG_CONS: {
        printf("[");
        Value cur = v;
        int first = 1;
        while ((cur & TAG_MASK) == TAG_CONS) {
            if (!first) printf(", ");
            first = 0;
            Value* ptr = (Value*)((intptr_t)(cur & ~TAG_MASK));
            print_value_inline(ptr[0]);
            cur = ptr[1];
        }
        if ((cur & TAG_MASK) != TAG_NIL) {
            printf(" | ");
            print_value_inline(cur);
        }
        printf("]");
        break;
    }
    case TAG_BOXED: {
        Value* ptr = (Value*)((intptr_t)(v & ~TAG_MASK));
        if (ptr[0] == BOXED_STRING) {
            const char* data = (const char*)(ptr + 2);
            printf("\"%.*s\"", (int)ptr[1], data);
        } else if (ptr[0] == BOXED_FLOAT) {
            double f;
            memcpy(&f, &ptr[1], sizeof(double));
            printf("%g", f);
        } else {
            printf("<boxed:%d>", (int)ptr[0]);
        }
        break;
    }
    default:
        printf("<unknown:0x%" PRIx64 ">", v);
        break;
    }
}

static void print_value(Value v) {
    print_value_inline(v);
    printf("\n");
}

/* io:format("~p~n", [x]) — used by Lux print() */
Value lux_rt_io_format(Value fmt, Value args) {
    (void)fmt;
    /* Walk the args list and print each element */
    Value cur = args;
    while ((cur & TAG_MASK) == TAG_CONS) {
        Value* ptr = (Value*)((intptr_t)(cur & ~TAG_MASK));
        print_value(ptr[0]);
        cur = ptr[1];
    }
    return VALUE_NIL; /* io:format returns 'ok' but we use nil for simplicity */
}

/* erlang:display/1 */
Value lux_rt_display(Value v) {
    print_value(v);
    return VALUE_TRUE;
}

/* to_string support */
Value lux_rt_to_string(Value v) {
    char buf[256];
    int64_t tag = v & TAG_MASK;
    int len;
    switch (tag) {
    case TAG_INT:
        len = snprintf(buf, sizeof(buf), "%" PRId64, v >> TAG_BITS);
        return lux_rt_make_string(buf, len);
    case TAG_ATOM: {
        int64_t idx = v >> TAG_BITS;
        if (idx >= 0 && idx < num_atoms)
            return lux_rt_make_string(atom_names[idx], strlen(atom_names[idx]));
        len = snprintf(buf, sizeof(buf), "atom_%d", (int)idx);
        return lux_rt_make_string(buf, len);
    }
    default:
        len = snprintf(buf, sizeof(buf), "<value:0x%" PRIx64 ">", v);
        return lux_rt_make_string(buf, len);
    }
}

/* ---- Float arithmetic helpers ---- */
/* Unbox a Value to double: tagged int -> cast to double, boxed float -> read double */
static double value_to_double(Value v) {
    int64_t tag = v & TAG_MASK;
    if (tag == TAG_INT) return (double)(v >> TAG_BITS);
    if (tag == TAG_BOXED) {
        Value* ptr = (Value*)((intptr_t)(v & ~TAG_MASK));
        if (ptr[0] == BOXED_FLOAT) {
            double f;
            memcpy(&f, &ptr[1], sizeof(double));
            return f;
        }
    }
    return 0.0;
}

static Value make_boxed_float(double f) {
    Value* ptr = (Value*)lux_alloc(2 * sizeof(Value));
    ptr[0] = BOXED_FLOAT;
    memcpy(&ptr[1], &f, sizeof(double));
    return ((int64_t)(intptr_t)ptr) | TAG_BOXED;
}

static int is_float_value(Value v) {
    if ((v & TAG_MASK) != TAG_BOXED) return 0;
    Value* ptr = (Value*)((intptr_t)(v & ~TAG_MASK));
    return ptr[0] == BOXED_FLOAT;
}

/* Arithmetic that handles both int and float operands. Returns tagged int
   if both are ints, boxed float otherwise. */
Value lux_rt_add(Value a, Value b) {
    if ((a & TAG_MASK) == TAG_INT && (b & TAG_MASK) == TAG_INT) {
        int64_t result = (a >> TAG_BITS) + (b >> TAG_BITS);
        return (result << TAG_BITS) | TAG_INT;
    }
    return make_boxed_float(value_to_double(a) + value_to_double(b));
}

Value lux_rt_sub(Value a, Value b) {
    if ((a & TAG_MASK) == TAG_INT && (b & TAG_MASK) == TAG_INT) {
        int64_t result = (a >> TAG_BITS) - (b >> TAG_BITS);
        return (result << TAG_BITS) | TAG_INT;
    }
    return make_boxed_float(value_to_double(a) - value_to_double(b));
}

Value lux_rt_mul(Value a, Value b) {
    if ((a & TAG_MASK) == TAG_INT && (b & TAG_MASK) == TAG_INT) {
        int64_t result = (a >> TAG_BITS) * (b >> TAG_BITS);
        return (result << TAG_BITS) | TAG_INT;
    }
    return make_boxed_float(value_to_double(a) * value_to_double(b));
}

Value lux_rt_float_div(Value a, Value b) {
    double bd = value_to_double(b);
    if (bd == 0.0) { lux_rt_raise(((3LL << TAG_BITS) | TAG_ATOM)); return VALUE_NIL; }
    return make_boxed_float(value_to_double(a) / bd);
}

Value lux_rt_negate(Value a) {
    if ((a & TAG_MASK) == TAG_INT) {
        int64_t result = -(a >> TAG_BITS);
        return (result << TAG_BITS) | TAG_INT;
    }
    return make_boxed_float(-value_to_double(a));
}

/* Comparisons for mixed int/float */
Value lux_rt_less_than(Value a, Value b) {
    if ((a & TAG_MASK) == TAG_INT && (b & TAG_MASK) == TAG_INT)
        return (a >> TAG_BITS) < (b >> TAG_BITS) ? VALUE_TRUE : VALUE_FALSE;
    return value_to_double(a) < value_to_double(b) ? VALUE_TRUE : VALUE_FALSE;
}

Value lux_rt_less_equal(Value a, Value b) {
    if ((a & TAG_MASK) == TAG_INT && (b & TAG_MASK) == TAG_INT)
        return (a >> TAG_BITS) <= (b >> TAG_BITS) ? VALUE_TRUE : VALUE_FALSE;
    return value_to_double(a) <= value_to_double(b) ? VALUE_TRUE : VALUE_FALSE;
}

Value lux_rt_greater_than(Value a, Value b) {
    if ((a & TAG_MASK) == TAG_INT && (b & TAG_MASK) == TAG_INT)
        return (a >> TAG_BITS) > (b >> TAG_BITS) ? VALUE_TRUE : VALUE_FALSE;
    return value_to_double(a) > value_to_double(b) ? VALUE_TRUE : VALUE_FALSE;
}

Value lux_rt_greater_equal(Value a, Value b) {
    if ((a & TAG_MASK) == TAG_INT && (b & TAG_MASK) == TAG_INT)
        return (a >> TAG_BITS) >= (b >> TAG_BITS) ? VALUE_TRUE : VALUE_FALSE;
    return value_to_double(a) >= value_to_double(b) ? VALUE_TRUE : VALUE_FALSE;
}

/* ---- io_lib:format — string interpolation support ---- */
/* Writes the inline representation of a value into buf, returns chars written. */
static int sprint_value(char* buf, int max, Value v) {
    int64_t tag = v & TAG_MASK;
    switch (tag) {
    case TAG_INT:
        return snprintf(buf, max, "%" PRId64, v >> TAG_BITS);
    case TAG_ATOM: {
        int64_t idx = v >> TAG_BITS;
        if (idx >= 0 && idx < num_atoms)
            return snprintf(buf, max, "%s", atom_names[idx]);
        return snprintf(buf, max, "atom_%d", (int)idx);
    }
    case TAG_NIL:
        return snprintf(buf, max, "[]");
    case TAG_BOXED: {
        Value* ptr = (Value*)((intptr_t)(v & ~TAG_MASK));
        if (ptr[0] == BOXED_STRING) {
            const char* data = (const char*)(ptr + 2);
            int64_t len = ptr[1];
            int n = len < max - 1 ? (int)len : max - 1;
            memcpy(buf, data, n);
            return n;
        }
        if (ptr[0] == BOXED_FLOAT) {
            double f;
            memcpy(&f, &ptr[1], sizeof(double));
            return snprintf(buf, max, "%g", f);
        }
        return snprintf(buf, max, "<boxed>");
    }
    case TAG_TUPLE: {
        Value* ptr = (Value*)((intptr_t)(v & ~TAG_MASK));
        int64_t arity = ptr[0];
        int pos = 0;
        if (pos < max) buf[pos++] = '{';
        for (int64_t i = 0; i < arity && pos < max - 2; i++) {
            if (i > 0 && pos < max - 2) { buf[pos++] = ','; buf[pos++] = ' '; }
            pos += sprint_value(buf + pos, max - pos, ptr[i + 1]);
        }
        if (pos < max) buf[pos++] = '}';
        return pos;
    }
    case TAG_CONS: {
        int pos = 0;
        if (pos < max) buf[pos++] = '[';
        Value cur = v;
        int first = 1;
        while ((cur & TAG_MASK) == TAG_CONS && pos < max - 3) {
            if (!first && pos < max - 2) { buf[pos++] = ','; buf[pos++] = ' '; }
            first = 0;
            Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
            pos += sprint_value(buf + pos, max - pos, p[0]);
            cur = p[1];
        }
        if (pos < max) buf[pos++] = ']';
        return pos;
    }
    default:
        return snprintf(buf, max, "?");
    }
}

/* Process format string: replace each ~p with the next arg from the list.
   Other ~X sequences are passed through literally. Returns a boxed string. */
Value lux_rt_io_lib_format(Value fmt, Value args) {
    char buf[4096];
    int pos = 0;
    int max = sizeof(buf) - 1;
    Value cur_arg = args;

    /* If fmt is a boxed string, walk through it */
    if ((fmt & TAG_MASK) == TAG_BOXED) {
        Value* fp = (Value*)((intptr_t)(fmt & ~TAG_MASK));
        if (fp[0] == BOXED_STRING) {
            const char* fdata = (const char*)(fp + 2);
            int64_t flen = fp[1];
            for (int64_t i = 0; i < flen && pos < max; i++) {
                if (fdata[i] == '~' && i + 1 < flen) {
                    char spec = fdata[i + 1];
                    i++; /* skip spec char */
                    if (spec == 'p' || spec == 'w' || spec == 's' || spec == 'B') {
                        /* Substitute next arg */
                        if ((cur_arg & TAG_MASK) == TAG_CONS) {
                            Value* cp = (Value*)((intptr_t)(cur_arg & ~TAG_MASK));
                            pos += sprint_value(buf + pos, max - pos, cp[0]);
                            cur_arg = cp[1];
                        }
                    } else if (spec == 'n') {
                        if (pos < max) buf[pos++] = '\n';
                    } else {
                        /* Pass through unknown escape */
                        if (pos < max) buf[pos++] = '~';
                        if (pos < max) buf[pos++] = spec;
                    }
                } else {
                    buf[pos++] = fdata[i];
                }
            }
        }
    }
    buf[pos] = '\0';
    return lux_rt_make_string(buf, pos);
}

/* ---- Atom lookup helper ---- */
static Value find_atom_value(const char* name) {
    for (int i = 0; i < num_atoms; i++) {
        if (strcmp(atom_names[i], name) == 0) return ((int64_t)i << TAG_BITS) | TAG_ATOM;
    }
    return VALUE_NIL;
}

/* ---- Deep value equality ---- */
Value lux_rt_value_equal(Value a, Value b) {
    if (a == b) return VALUE_TRUE;
    int64_t tag_a = a & TAG_MASK;
    int64_t tag_b = b & TAG_MASK;
    if (tag_a != tag_b) return VALUE_FALSE;
    switch (tag_a) {
    case TAG_BOXED: {
        Value* pa = (Value*)((intptr_t)(a & ~TAG_MASK));
        Value* pb = (Value*)((intptr_t)(b & ~TAG_MASK));
        if (pa[0] != pb[0]) return VALUE_FALSE; /* different boxed type */
        if (pa[0] == BOXED_STRING) {
            int64_t la = pa[1], lb = pb[1];
            if (la != lb) return VALUE_FALSE;
            return memcmp((char*)(pa+2), (char*)(pb+2), la) == 0 ? VALUE_TRUE : VALUE_FALSE;
        }
        if (pa[0] == BOXED_FLOAT) {
            double fa, fb;
            memcpy(&fa, &pa[1], sizeof(double));
            memcpy(&fb, &pb[1], sizeof(double));
            return fa == fb ? VALUE_TRUE : VALUE_FALSE;
        }
        return VALUE_FALSE;
    }
    case TAG_TUPLE: {
        Value* pa = (Value*)((intptr_t)(a & ~TAG_MASK));
        Value* pb = (Value*)((intptr_t)(b & ~TAG_MASK));
        if (pa[0] != pb[0]) return VALUE_FALSE; /* different arity */
        int64_t arity = pa[0];
        for (int64_t i = 1; i <= arity; i++) {
            if (lux_rt_value_equal(pa[i], pb[i]) != VALUE_TRUE) return VALUE_FALSE;
        }
        return VALUE_TRUE;
    }
    case TAG_CONS: {
        Value ca = a, cb = b;
        while ((ca & TAG_MASK) == TAG_CONS && (cb & TAG_MASK) == TAG_CONS) {
            Value* pa = (Value*)((intptr_t)(ca & ~TAG_MASK));
            Value* pb = (Value*)((intptr_t)(cb & ~TAG_MASK));
            if (lux_rt_value_equal(pa[0], pb[0]) != VALUE_TRUE) return VALUE_FALSE;
            ca = pa[1]; cb = pb[1];
        }
        return lux_rt_value_equal(ca, cb);
    }
    default:
        return VALUE_FALSE;
    }
}

/* ---- byte_size/1 ---- */
Value lux_rt_byte_size(Value v) {
    if ((v & TAG_MASK) == TAG_BOXED) {
        Value* ptr = (Value*)((intptr_t)(v & ~TAG_MASK));
        if (ptr[0] == BOXED_STRING) {
            return (ptr[1] << TAG_BITS) | TAG_INT;
        }
    }
    return (0LL << TAG_BITS) | TAG_INT;
}

/* ---- String comparison for pattern matching ---- */
int64_t lux_rt_string_equal(Value a, Value b) {
    if (a == b) return 1;
    int64_t tag_a = a & TAG_MASK;
    int64_t tag_b = b & TAG_MASK;
    if (tag_a != TAG_BOXED || tag_b != TAG_BOXED) return 0;
    Value* pa = (Value*)((intptr_t)(a & ~TAG_MASK));
    Value* pb = (Value*)((intptr_t)(b & ~TAG_MASK));
    if (pa[0] != BOXED_STRING || pb[0] != BOXED_STRING) return 0;
    int64_t len_a = pa[1], len_b = pb[1];
    if (len_a != len_b) return 0;
    return memcmp((char*)(pa + 2), (char*)(pb + 2), len_a) == 0 ? 1 : 0;
}

/* ---- Match error ---- */
void lux_rt_match_error(void) {
    fprintf(stderr, "lux: match error — no clause matched\n");
    exit(1);
}

void lux_rt_case_clause_error(Value v) {
    fprintf(stderr, "lux: case clause error, no match for: ");
    print_value_inline(v);
    fprintf(stderr, "\n");
    exit(1);
}

/* erlang:error/1 */
void lux_rt_erlang_error(Value v) {
    lux_rt_raise(v);
}

/* erlang:throw/1 */
void lux_rt_erlang_throw(Value v) {
    lux_rt_raise(v);
}

/* ---- Closure support ---- */
/* Closure layout: [func_ptr, n_captures, cap0, cap1, ...]
   Tagged with TAG_FUN (6).
   Calling convention: func_ptr(closure_env, arg0, arg1, ...) */
#define TAG_FUN 6LL

typedef Value (*ClosureFn0)(Value env);
typedef Value (*ClosureFn1)(Value env, Value a0);
typedef Value (*ClosureFn2)(Value env, Value a0, Value a1);
typedef Value (*ClosureFn3)(Value env, Value a0, Value a1, Value a2);

Value lux_rt_apply0(Value closure) {
    Value* ptr = (Value*)((intptr_t)(closure & ~TAG_MASK));
    ClosureFn0 fn = (ClosureFn0)(intptr_t)(ptr[0]);
    return fn(closure);
}

Value lux_rt_apply1(Value closure, Value arg0) {
    Value* ptr = (Value*)((intptr_t)(closure & ~TAG_MASK));
    ClosureFn1 fn = (ClosureFn1)(intptr_t)(ptr[0]);
    return fn(closure, arg0);
}

Value lux_rt_apply2(Value closure, Value arg0, Value arg1) {
    Value* ptr = (Value*)((intptr_t)(closure & ~TAG_MASK));
    ClosureFn2 fn = (ClosureFn2)(intptr_t)(ptr[0]);
    return fn(closure, arg0, arg1);
}

Value lux_rt_apply3(Value closure, Value arg0, Value arg1, Value arg2) {
    Value* ptr = (Value*)((intptr_t)(closure & ~TAG_MASK));
    ClosureFn3 fn = (ClosureFn3)(intptr_t)(ptr[0]);
    return fn(closure, arg0, arg1, arg2);
}

/* ---- List stdlib ---- */

Value lux_lists_reverse1(Value list) {
    Value result = VALUE_NIL;
    Value cur = list;
    while ((cur & TAG_MASK) == TAG_CONS) {
        Value* ptr = (Value*)((intptr_t)(cur & ~TAG_MASK));
        result = lux_rt_make_cons(ptr[0], result);
        cur = ptr[1];
    }
    return result;
}

/* Merge sort for lists */
static Value list_merge(Value a, Value b) {
    if ((a & TAG_MASK) != TAG_CONS) return b;
    if ((b & TAG_MASK) != TAG_CONS) return a;
    Value* pa = (Value*)((intptr_t)(a & ~TAG_MASK));
    Value* pb = (Value*)((intptr_t)(b & ~TAG_MASK));
    /* Compare: integers by value, otherwise by raw tagged value */
    int64_t av = pa[0], bv = pb[0];
    if ((av & TAG_MASK) == TAG_INT && (bv & TAG_MASK) == TAG_INT) {
        if ((av >> TAG_BITS) <= (bv >> TAG_BITS))
            return lux_rt_make_cons(pa[0], list_merge(pa[1], b));
        else
            return lux_rt_make_cons(pb[0], list_merge(a, pb[1]));
    }
    if (av <= bv)
        return lux_rt_make_cons(pa[0], list_merge(pa[1], b));
    else
        return lux_rt_make_cons(pb[0], list_merge(a, pb[1]));
}

static void list_split(Value list, Value* left, Value* right) {
    Value slow = list, fast = list;
    Value prev = VALUE_NIL;
    while ((fast & TAG_MASK) == TAG_CONS) {
        Value* pf = (Value*)((intptr_t)(fast & ~TAG_MASK));
        fast = pf[1];
        if ((fast & TAG_MASK) == TAG_CONS) {
            Value* pf2 = (Value*)((intptr_t)(fast & ~TAG_MASK));
            fast = pf2[1];
        }
        prev = slow;
        Value* ps = (Value*)((intptr_t)(slow & ~TAG_MASK));
        slow = ps[1];
    }
    *left = list;
    *right = slow;
    if ((prev & TAG_MASK) == TAG_CONS) {
        Value* pp = (Value*)((intptr_t)(prev & ~TAG_MASK));
        pp[1] = VALUE_NIL;
    }
}

static Value list_mergesort(Value list) {
    if ((list & TAG_MASK) != TAG_CONS) return list;
    Value* p = (Value*)((intptr_t)(list & ~TAG_MASK));
    if ((p[1] & TAG_MASK) != TAG_CONS) return list; /* single element */
    Value left, right;
    list_split(list, &left, &right);
    left = list_mergesort(left);
    right = list_mergesort(right);
    return list_merge(left, right);
}

Value lux_lists_sort1(Value list) {
    /* Copy list first (mergesort is destructive on cons cells) */
    Value copy = VALUE_NIL;
    Value cur = list;
    while ((cur & TAG_MASK) == TAG_CONS) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        copy = lux_rt_make_cons(p[0], copy);
        cur = p[1];
    }
    copy = lux_lists_reverse1(copy);
    return list_mergesort(copy);
}

Value lux_lists_append2(Value a, Value b) {
    Value rev = lux_lists_reverse1(a);
    Value result = b;
    Value cur = rev;
    while ((cur & TAG_MASK) == TAG_CONS) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        result = lux_rt_make_cons(p[0], result);
        cur = p[1];
    }
    return result;
}

Value lux_lists_flatten1(Value list) {
    if ((list & TAG_MASK) != TAG_CONS) return list;
    Value result = VALUE_NIL;
    /* Flatten one level */
    Value rev = lux_lists_reverse1(list);
    Value cur = rev;
    while ((cur & TAG_MASK) == TAG_CONS) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        Value elem = p[0];
        if ((elem & TAG_MASK) == TAG_CONS || elem == VALUE_NIL) {
            result = lux_lists_append2(lux_lists_flatten1(elem), result);
        } else {
            result = lux_rt_make_cons(elem, result);
        }
        cur = p[1];
    }
    return result;
}

Value lux_lists_seq2(Value from, Value to) {
    int64_t f = from >> TAG_BITS;
    int64_t t = to >> TAG_BITS;
    Value result = VALUE_NIL;
    for (int64_t i = t; i >= f; i--) {
        result = lux_rt_make_cons((i << TAG_BITS) | TAG_INT, result);
    }
    return result;
}

Value lux_lists_nth2(Value n, Value list) {
    int64_t idx = n >> TAG_BITS;
    Value cur = list;
    for (int64_t i = 1; i < idx && (cur & TAG_MASK) == TAG_CONS; i++) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        cur = p[1];
    }
    if ((cur & TAG_MASK) == TAG_CONS) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        return p[0];
    }
    return VALUE_NIL;
}

Value lux_lists_member2(Value elem, Value list) {
    Value cur = list;
    while ((cur & TAG_MASK) == TAG_CONS) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        if (p[0] == elem) return VALUE_TRUE;
        cur = p[1];
    }
    return VALUE_FALSE;
}

Value lux_lists_map2(Value fun, Value list) {
    Value result = VALUE_NIL;
    Value cur = list;
    while ((cur & TAG_MASK) == TAG_CONS) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        Value mapped = lux_rt_apply1(fun, p[0]);
        result = lux_rt_make_cons(mapped, result);
        cur = p[1];
    }
    return lux_lists_reverse1(result);
}

Value lux_lists_flatmap2(Value fun, Value list) {
    Value result = VALUE_NIL;
    Value cur = list;
    while ((cur & TAG_MASK) == TAG_CONS) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        Value mapped = lux_rt_apply1(fun, p[0]);
        /* mapped should be a list — append to result */
        result = lux_lists_append2(result, mapped);
        cur = p[1];
    }
    return result;
}

Value lux_lists_filter2(Value fun, Value list) {
    Value result = VALUE_NIL;
    Value cur = list;
    while ((cur & TAG_MASK) == TAG_CONS) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        Value keep = lux_rt_apply1(fun, p[0]);
        if (keep == VALUE_TRUE) {
            result = lux_rt_make_cons(p[0], result);
        }
        cur = p[1];
    }
    return lux_lists_reverse1(result);
}

Value lux_lists_foldl3(Value fun, Value acc, Value list) {
    Value cur = list;
    while ((cur & TAG_MASK) == TAG_CONS) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        acc = lux_rt_apply2(fun, p[0], acc);
        cur = p[1];
    }
    return acc;
}

Value lux_lists_usort1(Value list) {
    Value sorted = lux_lists_sort1(list);
    /* Remove adjacent duplicates */
    if ((sorted & TAG_MASK) != TAG_CONS) return sorted;
    Value result = VALUE_NIL;
    Value prev = VALUE_NIL;
    int has_prev = 0;
    Value cur = sorted;
    while ((cur & TAG_MASK) == TAG_CONS) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        if (!has_prev || p[0] != prev) {
            result = lux_rt_make_cons(p[0], result);
            prev = p[0];
            has_prev = 1;
        }
        cur = p[1];
    }
    return lux_lists_reverse1(result);
}

Value lux_lists_zip2(Value a, Value b) {
    Value result = VALUE_NIL;
    Value ca = a, cb = b;
    while ((ca & TAG_MASK) == TAG_CONS && (cb & TAG_MASK) == TAG_CONS) {
        Value* pa = (Value*)((intptr_t)(ca & ~TAG_MASK));
        Value* pb = (Value*)((intptr_t)(cb & ~TAG_MASK));
        result = lux_rt_make_cons(lux_rt_make_tuple2(pa[0], pb[0]), result);
        ca = pa[1]; cb = pb[1];
    }
    return lux_lists_reverse1(result);
}

Value lux_lists_enumerate1(Value list) {
    Value result = VALUE_NIL;
    int64_t idx = 1;
    Value cur = list;
    while ((cur & TAG_MASK) == TAG_CONS) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        Value pair = lux_rt_make_tuple2((idx << TAG_BITS) | TAG_INT, p[0]);
        result = lux_rt_make_cons(pair, result);
        idx++;
        cur = p[1];
    }
    return lux_lists_reverse1(result);
}

Value lux_lists_join2(Value sep, Value list) {
    if ((list & TAG_MASK) != TAG_CONS) return VALUE_NIL;
    Value* first = (Value*)((intptr_t)(list & ~TAG_MASK));
    Value result = first[0];
    Value cur = first[1];
    while ((cur & TAG_MASK) == TAG_CONS) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        result = lux_rt_string_concat(result, sep);
        result = lux_rt_string_concat(result, p[0]);
        cur = p[1];
    }
    return result;
}

Value lux_lists_sublist2(Value list, Value len) {
    int64_t n = len >> TAG_BITS;
    Value result = VALUE_NIL;
    Value cur = list;
    for (int64_t i = 0; i < n && (cur & TAG_MASK) == TAG_CONS; i++) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        result = lux_rt_make_cons(p[0], result);
        cur = p[1];
    }
    return lux_lists_reverse1(result);
}

Value lux_lists_nthtail2(Value n, Value list) {
    int64_t idx = n >> TAG_BITS;
    Value cur = list;
    for (int64_t i = 0; i < idx && (cur & TAG_MASK) == TAG_CONS; i++) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        cur = p[1];
    }
    return cur;
}

/* ---- Map support ---- */
/* Maps: sorted array of key-value pairs stored as a tuple-like structure
   Layout: [num_entries, key0, val0, key1, val1, ...]
   Tagged with TAG_TUPLE (0) but with a special marker.
   Actually, we use a distinct tag approach: store as regular heap object. */

#define MAP_MARKER 0x4D41505F /* 'MAP_' */

Value lux_rt_make_map(int64_t n_entries, Value* keys, Value* vals) {
    /* Layout: [MAP_MARKER, n_entries, k0, v0, k1, v1, ...] */
    Value* ptr = (Value*)lux_alloc((2 + n_entries * 2) * sizeof(Value));
    ptr[0] = MAP_MARKER;
    ptr[1] = n_entries;
    for (int64_t i = 0; i < n_entries; i++) {
        ptr[2 + i*2] = keys[i];
        ptr[2 + i*2 + 1] = vals[i];
    }
    return ((int64_t)(intptr_t)ptr) | TAG_BOXED;
}

static int is_map(Value v) {
    if ((v & TAG_MASK) != TAG_BOXED) return 0;
    Value* ptr = (Value*)((intptr_t)(v & ~TAG_MASK));
    return ptr[0] == MAP_MARKER;
}

Value lux_maps_get2(Value key, Value map) {
    if (!is_map(map)) return VALUE_NIL;
    Value* ptr = (Value*)((intptr_t)(map & ~TAG_MASK));
    int64_t n = ptr[1];
    for (int64_t i = 0; i < n; i++) {
        if (ptr[2 + i*2] == key) return ptr[2 + i*2 + 1];
    }
    return VALUE_NIL;
}

Value lux_maps_get3(Value key, Value map, Value default_val) {
    if (!is_map(map)) return default_val;
    Value* ptr = (Value*)((intptr_t)(map & ~TAG_MASK));
    int64_t n = ptr[1];
    for (int64_t i = 0; i < n; i++) {
        if (ptr[2 + i*2] == key) return ptr[2 + i*2 + 1];
    }
    return default_val;
}

Value lux_maps_put3(Value key, Value val, Value map) {
    int64_t old_n = 0;
    Value* old_ptr = NULL;
    if (is_map(map)) {
        old_ptr = (Value*)((intptr_t)(map & ~TAG_MASK));
        old_n = old_ptr[1];
    }
    /* Check if key exists */
    for (int64_t i = 0; i < old_n; i++) {
        if (old_ptr[2 + i*2] == key) {
            /* Update existing */
            Value* new_ptr = (Value*)lux_alloc((2 + old_n * 2) * sizeof(Value));
            memcpy(new_ptr, old_ptr, (2 + old_n * 2) * sizeof(Value));
            new_ptr[2 + i*2 + 1] = val;
            return ((int64_t)(intptr_t)new_ptr) | TAG_BOXED;
        }
    }
    /* Add new entry */
    Value* new_ptr = (Value*)lux_alloc((2 + (old_n+1) * 2) * sizeof(Value));
    new_ptr[0] = MAP_MARKER;
    new_ptr[1] = old_n + 1;
    if (old_ptr) memcpy(new_ptr + 2, old_ptr + 2, old_n * 2 * sizeof(Value));
    new_ptr[2 + old_n*2] = key;
    new_ptr[2 + old_n*2 + 1] = val;
    return ((int64_t)(intptr_t)new_ptr) | TAG_BOXED;
}

Value lux_maps_remove2(Value key, Value map) {
    if (!is_map(map)) return map;
    Value* old_ptr = (Value*)((intptr_t)(map & ~TAG_MASK));
    int64_t old_n = old_ptr[1];
    Value* new_ptr = (Value*)lux_alloc((2 + old_n * 2) * sizeof(Value));
    new_ptr[0] = MAP_MARKER;
    int64_t j = 0;
    for (int64_t i = 0; i < old_n; i++) {
        if (old_ptr[2 + i*2] != key) {
            new_ptr[2 + j*2] = old_ptr[2 + i*2];
            new_ptr[2 + j*2 + 1] = old_ptr[2 + i*2 + 1];
            j++;
        }
    }
    new_ptr[1] = j;
    return ((int64_t)(intptr_t)new_ptr) | TAG_BOXED;
}

Value lux_maps_is_key2(Value key, Value map) {
    if (!is_map(map)) return VALUE_FALSE;
    Value* ptr = (Value*)((intptr_t)(map & ~TAG_MASK));
    int64_t n = ptr[1];
    for (int64_t i = 0; i < n; i++) {
        if (ptr[2 + i*2] == key) return VALUE_TRUE;
    }
    return VALUE_FALSE;
}

Value lux_maps_find2(Value key, Value map) {
    if (!is_map(map)) return lux_rt_make_tuple2(((3LL << TAG_BITS) | TAG_ATOM), VALUE_NIL);
    Value* ptr = (Value*)((intptr_t)(map & ~TAG_MASK));
    int64_t n = ptr[1];
    for (int64_t i = 0; i < n; i++) {
        if (ptr[2 + i*2] == key) {
            return lux_rt_make_tuple2(((2LL << TAG_BITS) | TAG_ATOM), ptr[2 + i*2 + 1]);
        }
    }
    return ((3LL << TAG_BITS) | TAG_ATOM); /* :error atom */
}

Value lux_maps_keys1(Value map) {
    if (!is_map(map)) return VALUE_NIL;
    Value* ptr = (Value*)((intptr_t)(map & ~TAG_MASK));
    int64_t n = ptr[1];
    Value result = VALUE_NIL;
    for (int64_t i = n - 1; i >= 0; i--) {
        result = lux_rt_make_cons(ptr[2 + i*2], result);
    }
    return result;
}

Value lux_maps_values1(Value map) {
    if (!is_map(map)) return VALUE_NIL;
    Value* ptr = (Value*)((intptr_t)(map & ~TAG_MASK));
    int64_t n = ptr[1];
    Value result = VALUE_NIL;
    for (int64_t i = n - 1; i >= 0; i--) {
        result = lux_rt_make_cons(ptr[2 + i*2 + 1], result);
    }
    return result;
}

Value lux_maps_to_list1(Value map) {
    if (!is_map(map)) return VALUE_NIL;
    Value* ptr = (Value*)((intptr_t)(map & ~TAG_MASK));
    int64_t n = ptr[1];
    Value result = VALUE_NIL;
    for (int64_t i = n - 1; i >= 0; i--) {
        Value pair = lux_rt_make_tuple2(ptr[2 + i*2], ptr[2 + i*2 + 1]);
        result = lux_rt_make_cons(pair, result);
    }
    return result;
}

Value lux_maps_from_list1(Value list) {
    /* Count entries */
    int64_t n = 0;
    Value cur = list;
    while ((cur & TAG_MASK) == TAG_CONS) { n++; Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK)); cur = p[1]; }
    Value* new_ptr = (Value*)lux_alloc((2 + n * 2) * sizeof(Value));
    new_ptr[0] = MAP_MARKER;
    new_ptr[1] = n;
    cur = list;
    for (int64_t i = 0; i < n && (cur & TAG_MASK) == TAG_CONS; i++) {
        Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));
        Value pair = p[0];
        if ((pair & TAG_MASK) == TAG_TUPLE) {
            Value* tp = (Value*)((intptr_t)(pair & ~TAG_MASK));
            new_ptr[2 + i*2] = tp[1];
            new_ptr[2 + i*2 + 1] = tp[2];
        }
        cur = p[1];
    }
    return ((int64_t)(intptr_t)new_ptr) | TAG_BOXED;
}

Value lux_maps_merge2(Value map1, Value map2) {
    Value result = map1;
    if (!is_map(map2)) return result;
    Value* ptr2 = (Value*)((intptr_t)(map2 & ~TAG_MASK));
    int64_t n2 = ptr2[1];
    for (int64_t i = 0; i < n2; i++) {
        result = lux_maps_put3(ptr2[2+i*2], ptr2[2+i*2+1], result);
    }
    return result;
}

Value lux_maps_size1(Value map) {
    if (!is_map(map)) return (0LL << TAG_BITS) | TAG_INT;
    Value* ptr = (Value*)((intptr_t)(map & ~TAG_MASK));
    return (ptr[1] << TAG_BITS) | TAG_INT;
}

/* ---- String operations ---- */

Value lux_string_trim1(Value s) {
    if ((s & TAG_MASK) != TAG_BOXED) return s;
    const char* data = lux_rt_string_data(s);
    int64_t len = lux_rt_string_length(s);
    int64_t start = 0, end = len;
    while (start < end && (data[start] == ' ' || data[start] == '\t' || data[start] == '\n' || data[start] == '\r')) start++;
    while (end > start && (data[end-1] == ' ' || data[end-1] == '\t' || data[end-1] == '\n' || data[end-1] == '\r')) end--;
    return lux_rt_make_string(data + start, end - start);
}

Value lux_string_uppercase1(Value s) {
    if ((s & TAG_MASK) != TAG_BOXED) return s;
    const char* data = lux_rt_string_data(s);
    int64_t len = lux_rt_string_length(s);
    char* buf = (char*)lux_alloc(len + 1);
    for (int64_t i = 0; i < len; i++) buf[i] = (data[i] >= 'a' && data[i] <= 'z') ? data[i] - 32 : data[i];
    buf[len] = 0;
    return lux_rt_make_string(buf, len);
}

Value lux_string_lowercase1(Value s) {
    if ((s & TAG_MASK) != TAG_BOXED) return s;
    const char* data = lux_rt_string_data(s);
    int64_t len = lux_rt_string_length(s);
    char* buf = (char*)lux_alloc(len + 1);
    for (int64_t i = 0; i < len; i++) buf[i] = (data[i] >= 'A' && data[i] <= 'Z') ? data[i] + 32 : data[i];
    buf[len] = 0;
    return lux_rt_make_string(buf, len);
}

Value lux_string_find2(Value haystack, Value needle) {
    if ((haystack & TAG_MASK) != TAG_BOXED || (needle & TAG_MASK) != TAG_BOXED)
        return find_atom_value("nomatch");
    const char* h = lux_rt_string_data(haystack);
    const char* n = lux_rt_string_data(needle);
    const char* found = strstr(h, n);
    if (found) return lux_rt_make_string(found, strlen(found));
    return find_atom_value("nomatch");
}

Value lux_string_split3(Value s, Value delim, Value _opts) {
    (void)_opts;
    if ((s & TAG_MASK) != TAG_BOXED) return lux_rt_make_cons(s, VALUE_NIL);
    const char* data = lux_rt_string_data(s);
    int64_t len = lux_rt_string_length(s);
    const char* d = "";
    int64_t dlen = 0;
    if ((delim & TAG_MASK) == TAG_BOXED) { d = lux_rt_string_data(delim); dlen = lux_rt_string_length(delim); }
    if (dlen == 0) return lux_rt_make_cons(s, VALUE_NIL);
    Value result = VALUE_NIL;
    const char* p = data;
    const char* end = data + len;
    while (p < end) {
        const char* found = strstr(p, d);
        if (!found) { result = lux_rt_make_cons(lux_rt_make_string(p, end - p), result); break; }
        result = lux_rt_make_cons(lux_rt_make_string(p, found - p), result);
        p = found + dlen;
    }
    return lux_lists_reverse1(result);
}

Value lux_string_replace4(Value s, Value pattern, Value replacement, Value _opts) {
    (void)_opts;
    if ((s & TAG_MASK) != TAG_BOXED) return s;
    const char* data = lux_rt_string_data(s);
    const char* pat = lux_rt_string_data(pattern);
    const char* rep = lux_rt_string_data(replacement);
    int64_t plen = lux_rt_string_length(pattern);
    int64_t rlen = lux_rt_string_length(replacement);
    if (plen == 0) return s;
    /* Simple single replacement */
    const char* found = strstr(data, pat);
    if (!found) return s;
    int64_t slen = lux_rt_string_length(s);
    int64_t nlen = slen - plen + rlen;
    char* buf = (char*)lux_alloc(nlen + 1);
    int64_t prefix = found - data;
    memcpy(buf, data, prefix);
    memcpy(buf + prefix, rep, rlen);
    memcpy(buf + prefix + rlen, found + plen, slen - prefix - plen);
    buf[nlen] = 0;
    return lux_rt_make_string(buf, nlen);
}

"#,
    );

    // System utilities, timer, rand, etc.
    c.push_str(
        "/* ---- System utilities ---- */\n\
         Value lux_init_get_plain_arguments0(void) { return VALUE_NIL; }\n\
         Value lux_os_getenv1(Value name) {\n\
           if ((name & TAG_MASK) != TAG_BOXED) return VALUE_FALSE;\n\
           const char* key = lux_rt_string_data(name);\n\
           const char* val = getenv(key);\n\
           if (!val) return VALUE_FALSE;\n\
           return lux_rt_make_string(val, strlen(val));\n\
         }\n",
    );
    c.push_str(
        "Value lux_rt_list_to_binary(Value list) {\n\
           int64_t len = 0; Value cur = list;\n\
           while ((cur & TAG_MASK) == TAG_CONS) { len++; Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK)); cur = p[1]; }\n\
           if (len == 0) { char e = 0; return lux_rt_make_string(&e, 0); }\n\
           int64_t hs = 2 * sizeof(Value);\n\
           char* ptr = (char*)lux_alloc(hs + len + 1);\n\
           ((Value*)ptr)[0] = BOXED_STRING; ((Value*)ptr)[1] = len;\n\
           cur = list;\n\
           for (int64_t i = 0; i < len && (cur & TAG_MASK) == TAG_CONS; i++) {\n\
             Value* p = (Value*)((intptr_t)(cur & ~TAG_MASK));\n\
             ptr[hs + i] = (p[0] & TAG_MASK) == TAG_INT ? (char)(p[0] >> TAG_BITS) : '?';\n\
             cur = p[1];\n\
           }\n\
           ptr[hs + len] = '\\0';\n\
           return ((int64_t)(intptr_t)ptr) | TAG_BOXED;\n\
         }\n",
    );
    c.push_str(
        "Value lux_rt_binary_to_list(Value bin) {\n\
           if ((bin & TAG_MASK) != TAG_BOXED) return VALUE_NIL;\n\
           Value* bp = (Value*)((intptr_t)(bin & ~TAG_MASK));\n\
           if (bp[0] != BOXED_STRING) return VALUE_NIL;\n\
           int64_t len = bp[1]; const char* data = (const char*)(bp + 2);\n\
           Value result = VALUE_NIL;\n\
           for (int64_t i = len - 1; i >= 0; i--) {\n\
             result = lux_rt_make_cons(((int64_t)(unsigned char)data[i] << TAG_BITS) | TAG_INT, result);\n\
           }\n\
           return result;\n\
         }\n",
    );
    c.push_str(
        "Value lux_rt_put(Value k, Value v) { (void)k; (void)v; return VALUE_NIL; }\n\
         Value lux_rt_get(Value k) { (void)k; return find_atom_value(\"undefined\"); }\n",
    );
    c.push_str(
        "Value lux_timer_sleep1(Value ms) {\n\
           int64_t millis = ms >> TAG_BITS;\n\
           if (millis > 0) { struct timespec ts; ts.tv_sec = millis/1000; ts.tv_nsec = (millis%1000)*1000000; nanosleep(&ts, NULL); }\n\
           return VALUE_NIL;\n\
         }\n\
         static unsigned long rand_state = 12345;\n\
         Value lux_rand_seed1(Value s) { (void)s; rand_state = 67890; return VALUE_NIL; }\n\
         Value lux_rand_uniform0(void) {\n\
           rand_state = rand_state * 6364136223846793005ULL + 1442695040888963407ULL;\n\
           double f = (double)(rand_state >> 33) / (double)(1ULL << 31);\n\
           return make_boxed_float(f);\n\
         }\n\
         Value lux_rand_uniform1(Value mx) {\n\
           rand_state = rand_state * 6364136223846793005ULL + 1442695040888963407ULL;\n\
           int64_t m = mx >> TAG_BITS; if (m <= 0) m = 1;\n\
           int64_t r = (int64_t)((rand_state >> 33) % (uint64_t)m) + 1;\n\
           return (r << TAG_BITS) | TAG_INT;\n\
         }\n\
         Value lux_rt_system_time(Value u) { (void)u; struct timespec ts; clock_gettime(CLOCK_REALTIME, &ts);\n\
           int64_t ms = ts.tv_sec * 1000 + ts.tv_nsec / 1000000; return (ms << TAG_BITS) | TAG_INT; }\n\
         static int64_t ref_counter = 0;\n\
         Value lux_rt_make_ref(void) { return ((++ref_counter) << TAG_BITS) | TAG_INT; }\n\
         Value lux_maps_fold3(Value fun, Value acc, Value map) {\n\
           if (!is_map(map)) return acc;\n\
           Value* ptr = (Value*)((intptr_t)(map & ~TAG_MASK));\n\
           int64_t n = ptr[1];\n\
           for (int64_t i = 0; i < n; i++) { acc = lux_rt_apply3(fun, ptr[2+i*2], ptr[2+i*2+1], acc); }\n\
           return acc;\n\
         }\n",
    );

    // Add time.h to the includes at top
    c = c.replace(
        "#include <setjmp.h>",
        "#include <setjmp.h>\n#include <time.h>",
    );

    c.push_str("/* ---- Entry point ---- */\n");
    if entry_arity == 0 {
        c.push_str(&format!("extern Value {}(void);\n\n", entry_symbol));
    } else {
        let params: Vec<String> = (0..entry_arity).map(|i| format!("Value a{}", i)).collect();
        c.push_str(&format!(
            "extern Value {}({});\n\n",
            entry_symbol,
            params.join(", ")
        ));
    }

    c.push_str(&format!(
        r#"int main(int argc, char** argv) {{
    (void)argc;
    (void)argv;
    Value result = {}();
    /* Only print the result if it is not unit/nil */
    if (result != VALUE_NIL && result != ((4LL << TAG_BITS) | TAG_ATOM)) {{
        print_value(result);
    }}
    return 0;
}}
"#,
        entry_symbol,
    ));

    c
}
