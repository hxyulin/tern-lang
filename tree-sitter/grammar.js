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

module.exports = grammar({
  name: 'tern',

  // The scanner (src/scanner.c) inserts semicolons at line ends and
  // lexes nested block comments.
  externals: ($) => [$._automatic_semicolon, $.block_comment, $._error_sentinel],

  extras: ($) => [/\s/, $.line_comment, $.block_comment],

  word: ($) => $.identifier,

  // Keywords are reserved everywhere, as in the compiler's lexer.
  // Tree-sitter keywords are contextual unless listed here.
  reserved: {
    global: (_) => ['fn', 'type', 'let', 'var', 'return', 'inout', 'sink'],
  },

  rules: {
    source_file: ($) => optional($._declarations),

    _declarations: ($) =>
      seq($._declaration, repeat(seq($._terminator, $._declaration)), optional($._terminator)),

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

    // type Name T
    type_declaration: ($) => seq('type', field('name', $.identifier), field('type', $._type)),

    // Types

    _type: ($) => choice($.type_name, $.tuple_type, $.view_type, $.pointer_type),

    type_name: ($) => $.identifier,

    tuple_type: ($) => seq('(', commaSep1($.tuple_type_field), ')'),

    tuple_type_field: ($) => seq(field('name', $.identifier), ':', field('type', $._type)),

    view_type: ($) => seq('[', ']', field('element', $._type)),

    pointer_type: ($) => seq('*', field('pointee', $._type)),

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

    block: ($) => seq('{', optional($._statements), '}'),

    _statements: ($) =>
      seq($._statement, repeat(seq($._terminator, $._statement)), optional($._terminator)),

    _statement: ($) =>
      choice(
        $.let_declaration,
        $.var_declaration,
        $.return_statement,
        $.assignment_statement,
        $.expression_statement,
      ),

    return_statement: ($) => seq('return', optional(field('value', $._expression))),

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

    // Only a call can stand alone as a statement, as in Go.
    expression_statement: ($) => $.call_expression,

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

    _expression: ($) => choice($._comparison_level, $.and_expression, $.or_expression),

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

    _unary_level: ($) => choice($._postfix, $.unary_expression),

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
