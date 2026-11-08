/**
 * Tree-sitter grammar for tern, part 1: declarations and bindings.
 *
 * The grammar uses no `prec` and no `conflicts`, so a clean
 * `tree-sitter generate` shows it has no LR(1) conflicts. Keep it that
 * way: a conflict is a design question, not something to paper over.
 *
 * Expressions are a placeholder until part 2: names, number literals,
 * calls, field access, construction and tuple literals.
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

    assignment_statement: ($) =>
      seq(field('left', $._expression), '=', field('right', $._expression)),

    expression_statement: ($) => $._expression,

    // Expressions (placeholder until part 2)

    _expression: ($) =>
      choice(
        $.identifier,
        $.integer_literal,
        $.float_literal,
        $.parenthesized_expression,
        $.tuple_literal,
        $.call_expression,
        $.field_expression,
        $.construct_expression,
      ),

    parenthesized_expression: ($) => seq('(', $._expression, ')'),

    // (q: 1, r: 2)
    tuple_literal: ($) => seq('(', commaSep1($.tuple_field), ')'),

    tuple_field: ($) => seq(field('name', $.identifier), ':', field('value', $._expression)),

    call_expression: ($) =>
      seq(field('function', $._expression), field('arguments', $.argument_list)),

    argument_list: ($) => seq('(', commaSep($.argument), ')'),

    // The call site writes `inout` too.
    argument: ($) => seq(optional('inout'), field('value', $._expression)),

    field_expression: ($) =>
      seq(field('value', $._expression), '.', field('field', $.identifier)),

    // Point.(x: 1, y: 2), Celsius.(20.0). The checker requires the
    // prefix to name a type.
    construct_expression: ($) =>
      seq(
        field('type', $._expression),
        '.',
        field('value', choice($.tuple_literal, $.parenthesized_expression)),
      ),

    // Tokens

    // A lone `_` is the discard token, not an identifier.
    identifier: (_) => /[A-Za-z][A-Za-z0-9_]*|_[A-Za-z0-9_]+/,

    integer_literal: (_) => /[0-9][0-9_]*/,

    float_literal: (_) => /[0-9][0-9_]*\.[0-9][0-9_]*/,

    line_comment: (_) => token(seq('//', /[^\n]*/)),
  },
});
