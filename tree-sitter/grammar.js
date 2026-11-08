/**
 * Tree-sitter grammar for tern: the syntax designed so far.
 *
 * The grammar uses no `prec` and no `conflicts`, so a clean
 * `tree-sitter generate` shows it has no LR(1) conflicts. Keep it that
 * way: a conflict is a design question, not something to paper over.
 *
 * Part 2 adds operators, number literal forms and compound assignment.
 * String literals come later.
 */

const commaSep1 = (rule) => seq(rule, repeat(seq(',', rule)), optional(','));
const commaSep = (rule) => optional(commaSep1(rule));

// Braces hold items separated by line ends or `;`. Parentheses hold items
// separated by commas.
const lines = ($, rule) =>
  optional(seq(rule, repeat(seq($._terminator, rule)), optional($._terminator)));

module.exports = grammar({
  name: 'tern',

  // The scanner (src/scanner.c) inserts semicolons at line ends, lexes
  // nested block comments and lexes the text between a string's escapes
  // and interpolations.
  externals: ($) => [
    $._automatic_semicolon,
    $.block_comment,
    $.string_content,
    $._error_sentinel,
  ],

  extras: ($) => [/\s/, $.line_comment, $.block_comment],

  word: ($) => $.identifier,

  // Keywords are reserved everywhere, as in the compiler's lexer.
  // Tree-sitter keywords are contextual unless listed here.
  reserved: {
    global: (_) => [
      'fn',
      'type',
      'let',
      'var',
      'return',
      'inout',
      'sink',
      'if',
      'else',
      'for',
      'in',
      'break',
      'continue',
      'enum',
      'match',
      'try',
      'defer',
    ],
  },

  rules: {
    source_file: ($) => lines($, $._declaration),

    _declaration: ($) =>
      choice($.function_declaration, $.type_declaration, $.let_declaration, $.var_declaration),

    _terminator: ($) => choice(';', $._automatic_semicolon),


    // fn name(params) -> Result { ... }
    function_declaration: ($) =>
      seq(
        'fn',
        field('name', $.identifier),
        field('parameters', $.parameter_list),
        optional(seq('->', field('result', $._type))),
        field('body', $.block),
      ),

    parameter_list: ($) => seq('(', commaSep($.parameter), ')'),

    parameter: ($) =>
      seq(
        field('name', choice($.identifier, $.discard)),
        ':',
        optional(field('mode', $.parameter_mode)),
        field('type', $._type),
      ),

    parameter_mode: (_) => choice('inout', 'sink'),

    // type Name T. An enum appears only here, so every enum has a name.
    type_declaration: ($) =>
      seq('type', field('name', $.identifier), field('type', choice($._type, $.enum_type))),

    // enum { circle(radius: f32); empty }, enum u8 { nop = 0; halt = 1; _ }
    // A trailing `_` makes the enum open: every value of the integer type
    // is valid.
    enum_type: ($) =>
      seq('enum', optional(field('integer_type', $.type_name)), '{', lines($, $._enum_item), '}'),

    _enum_item: ($) => choice($.variant, $.discard),

    variant: ($) =>
      seq(
        field('name', $.identifier),
        optional(field('payload', $.tuple_type)),
        optional(seq('=', field('value', $._expression))),
      ),

    // Types

    // A result type sits at the top of a type only: `[]T ! E` is a
    // result whose value is a view. A view of results needs a named type.
    _type: ($) => choice($._simple_type, $.result_type),

    _simple_type: ($) =>
      choice($.type_name, $.tuple_type, $.view_type, $.pointer_type, $.optional_type),

    // usize ! IoError. Without a value type, `! IoError` is a function
    // that returns nothing but can fail.
    result_type: ($) =>
      seq(optional(field('value', $._simple_type)), '!', field('error', $._simple_type)),

    optional_type: ($) => seq('?', field('value', $._simple_type)),

    type_name: ($) => $.identifier,

    tuple_type: ($) => seq('(', commaSep1($.tuple_type_field), ')'),

    tuple_type_field: ($) => seq(field('name', $.identifier), ':', field('type', $._type)),

    view_type: ($) => seq('[', ']', field('element', $._simple_type)),

    pointer_type: ($) => seq('*', field('pointee', $._simple_type)),

    // Bindings

    let_declaration: ($) => seq('let', $._binding, '=', field('value', $._expression)),

    var_declaration: ($) => seq('var', $._binding, '=', field('value', $._expression)),

    _binding: ($) =>
      choice(
        seq(field('name', choice($.identifier, $.discard)), ':', field('type', $._type)),
        field('pattern', $.destructuring_pattern),
      ),

    // let (q: u32, r: _) = ...
    destructuring_pattern: ($) => seq('(', commaSep1($.destructuring_field), ')'),

    destructuring_field: ($) =>
      seq(field('name', $.identifier), ':', field('type', choice($._type, $.discard))),

    discard: (_) => '_',

    // Statements

    block: ($) => seq('{', lines($, $._statement), '}'),

    _statement: ($) =>
      choice(
        $.let_declaration,
        $.var_declaration,
        $.return_statement,
        $.if_statement,
        $.match_statement,
        $.for_statement,
        $.break_statement,
        $.continue_statement,
        $.defer_statement,
        $.assignment_statement,
        $.expression_statement,
      ),

    return_statement: ($) => seq('return', optional(field('value', $._expression))),

    // No parentheses around the condition; braces always.
    if_statement: ($) =>
      seq(
        'if',
        field('condition', $._expression),
        field('consequence', $.block),
        optional(seq('else', field('alternative', choice($.block, $.if_statement)))),
      ),

    match_statement: ($) =>
      seq('match', field('value', $._expression), '{', lines($, $.match_arm), '}'),

    match_arm: ($) => seq(field('pattern', $._pattern), field('body', $.block)),

    // A bare name is a variant, never a new binding.
    _pattern: ($) =>
      choice(
        $.variant_pattern,
        $.integer_literal,
        $.negative_literal,
        $.char_literal,
        $.string_literal,
        $.discard,
      ),

    variant_pattern: ($) =>
      seq(field('name', $.identifier), optional(field('payload', $.destructuring_pattern))),

    negative_literal: ($) => seq('-', $.integer_literal),

    // for { }, for cond { }, for let x: T in xs { }, for let i: u32 in 0..n { }
    for_statement: ($) =>
      seq('for', optional(choice(field('condition', $._expression), $.for_in_clause)), field('body', $.block)),

    // for let p: Point in points, for var (k: K, v: V) in pairs. The
    // leading let or var tells a loop variable from a condition, which
    // could otherwise start with a tuple literal.
    for_in_clause: ($) =>
      seq(
        field('kind', choice('let', 'var')),
        $._binding,
        'in',
        field('iterable', choice($._expression, $.range)),
      ),

    // Only in for loops for now. The end is excluded; there is no
    // inclusive form.
    range: ($) => seq(field('start', $._additive_level), '..', field('end', $._additive_level)),

    // Runs the call when the enclosing block ends.
    defer_statement: ($) => seq('defer', field('call', $.call_expression)),

    break_statement: (_) => 'break',

    continue_statement: (_) => 'continue',

    // The left side is a postfix expression, so `a + b = c` does not
    // parse. Compound assignment is a statement too, and there is no ++.
    assignment_statement: ($) =>
      seq(
        field('left', $._postfix),
        field(
          'operator',
          choice('=', '+=', '-=', '*=', '/=', '%=', '&=', '|=', '^=', '<<=', '>>='),
        ),
        field('right', $._expression),
      ),

    // Only a call can stand alone as a statement, as in Go, with or
    // without try.
    expression_statement: ($) => choice($.call_expression, $.try_expression),

    // Expressions
    //
    // Operator groups form a partial order. Each group is a layer of
    // nonterminals, so mixing unrelated groups without parentheses fails
    // to parse. From loosest to tightest:
    //
    //   ||, &&            logical; the two do not mix
    //   == != < <= > >=   comparison; non-associative
    //   + -  above  * / %   arithmetic
    //   & | ^, << >>      bitwise; never mixed with arithmetic or with a
    //                     different bitwise operator
    //   - ! ~             prefix
    //   call . .( [ ]     postfix

    // An if expression sits at the top level only, so it is never an
    // operand: `1 + if c { a } else { b }` does not parse.
    _expression: ($) =>
      choice(
        $._comparison_level,
        $.and_expression,
        $.or_expression,
        $.if_expression,
        $.match_expression,
      ),

    // Each arm is one expression, as in an if expression.
    match_expression: ($) =>
      seq('match', field('value', $._expression), '{', lines($, $.match_expression_arm), '}'),

    match_expression_arm: ($) => seq(field('pattern', $._pattern), field('body', $.branch)),

    // let x: i32 = if c { a } else { b }. The else is required and each
    // branch is one expression.
    if_expression: ($) =>
      seq(
        'if',
        field('condition', $._expression),
        field('consequence', $.branch),
        'else',
        field('alternative', choice($.branch, $.if_expression)),
      ),

    branch: ($) => seq('{', field('value', $._expression), '}'),

    or_expression: ($) =>
      seq(
        field('left', choice($._comparison_level, $.or_expression)),
        field('operator', '||'),
        field('right', $._comparison_level),
      ),

    and_expression: ($) =>
      seq(
        field('left', choice($._comparison_level, $.and_expression)),
        field('operator', '&&'),
        field('right', $._comparison_level),
      ),

    _comparison_level: ($) => choice($._comparison_operand, $.comparison_expression),

    comparison_expression: ($) =>
      seq(
        field('left', $._comparison_operand),
        field('operator', choice('==', '!=', '<', '<=', '>', '>=')),
        field('right', $._comparison_operand),
      ),

    _comparison_operand: ($) =>
      choice(
        $._additive_level,
        $.bit_and_expression,
        $.bit_or_expression,
        $.bit_xor_expression,
        $.shift_expression,
      ),

    _additive_level: ($) => choice($._multiplicative_level, $.additive_expression),

    additive_expression: ($) =>
      seq(
        field('left', $._additive_level),
        field('operator', choice('+', '-')),
        field('right', $._multiplicative_level),
      ),

    _multiplicative_level: ($) => choice($._unary_level, $.multiplicative_expression),

    multiplicative_expression: ($) =>
      seq(
        field('left', $._multiplicative_level),
        field('operator', choice('*', '/', '%')),
        field('right', $._unary_level),
      ),

    // a & b & c is allowed; a & b | c is not.
    bit_and_expression: ($) =>
      seq(
        field('left', choice($._unary_level, $.bit_and_expression)),
        field('operator', '&'),
        field('right', $._unary_level),
      ),

    bit_or_expression: ($) =>
      seq(
        field('left', choice($._unary_level, $.bit_or_expression)),
        field('operator', '|'),
        field('right', $._unary_level),
      ),

    bit_xor_expression: ($) =>
      seq(
        field('left', choice($._unary_level, $.bit_xor_expression)),
        field('operator', '^'),
        field('right', $._unary_level),
      ),

    // Shifts do not chain: x << 1 << 2 is an error.
    shift_expression: ($) =>
      seq(
        field('left', $._unary_level),
        field('operator', choice('<<', '>>')),
        field('right', $._unary_level),
      ),

    _unary_level: ($) => choice($._postfix, $.unary_expression, $.try_expression),

    // try read(inout f, inout buf). Returns early on err or none.
    try_expression: ($) => seq('try', field('operand', $._unary_level)),

    unary_expression: ($) =>
      seq(field('operator', choice('-', '!', '~')), field('operand', $._unary_level)),

    _postfix: ($) =>
      choice(
        $._primary,
        $.call_expression,
        $.field_expression,
        $.construct_expression,
        $.index_expression,
      ),

    _primary: ($) =>
      choice(
        $.identifier,
        $.integer_literal,
        $.float_literal,
        $.parenthesized_expression,
        $.tuple_literal,
        $.string_literal,
        $.char_literal,
      ),

    // "count: {n}\n". Printable ASCII only; other bytes need escapes.
    string_literal: ($) =>
      seq(
        '"',
        repeat(choice($.string_content, $.escape_sequence, $.interpolation)),
        token.immediate('"'),
      ),

    interpolation: ($) => seq(token.immediate('{'), field('value', $._expression), '}'),

    escape_sequence: (_) =>
      token.immediate(seq('\\', choice(/[ntr0\\"'{]/, /x[0-7][0-9a-fA-F]/))),

    // An ASCII code, typed from context like a number: 'a', '\n', '\x7F'.
    char_literal: (_) =>
      token(
        seq(
          "'",
          choice(/[\x20-\x26\x28-\x5B\x5D-\x7E]/, seq('\\', choice(/[ntr0\\"'{]/, /x[0-7][0-9a-fA-F]/))),
          "'",
        ),
      ),

    parenthesized_expression: ($) => seq('(', $._expression, ')'),

    // (q: 1, r: 2)
    tuple_literal: ($) => seq('(', commaSep1($.tuple_field), ')'),

    tuple_field: ($) => seq(field('name', $.identifier), ':', field('value', $._expression)),

    call_expression: ($) =>
      seq(field('function', $._postfix), field('arguments', $.argument_list)),

    argument_list: ($) => seq('(', commaSep($.argument), ')'),

    // The call site writes `inout` too.
    argument: ($) => seq(optional('inout'), field('value', $._expression)),

    field_expression: ($) =>
      seq(field('value', $._postfix), '.', field('field', $.identifier)),

    // Point.(x: 1, y: 2), Celsius.(20.0). The checker requires the
    // prefix to name a type.
    construct_expression: ($) =>
      seq(
        field('type', $._postfix),
        '.',
        field('value', choice($.tuple_literal, $.parenthesized_expression)),
      ),

    // v[i]. Also type arguments to built-ins, as in truncate[u8](x); the
    // checker tells them apart, as Go's does.
    index_expression: ($) =>
      seq(field('value', $._postfix), '[', field('index', $._expression), ']'),

    // Tokens

    // A lone `_` is the discard token, not an identifier.
    identifier: (_) => /[A-Za-z][A-Za-z0-9_]*|_[A-Za-z0-9_]+/,

    integer_literal: (_) =>
      token(choice(/[0-9][0-9_]*/, /0x[0-9a-fA-F_]+/, /0b[01_]+/, /0o[0-7_]+/)),

    // Digits on both sides of the point: 1.0, not 1. or .5
    float_literal: (_) => /[0-9][0-9_]*\.[0-9][0-9_]*([eE][+-]?[0-9_]+)?/,

    line_comment: (_) => token(seq('//', /[^\n]*/)),
  },
});
