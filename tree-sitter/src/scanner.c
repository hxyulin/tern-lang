// External scanner for tern.
//
// It produces two tokens the generated lexer cannot:
//
// - Automatic semicolons. At a line end, a semicolon is inserted where the
//   grammar can end a statement or declaration. The compiler's lexer uses
//   Go's rule instead, which inserts after certain tokens whatever follows.
//   The two agree on well-formed code. This scanner accepts some code that
//   Go's rule rejects, such as `fn f()` with `{` on the next line.
// - Nested block comments: `/* a /* b */ c */` is one comment.
//
// A block comment that contains a newline counts as a line end, as in Go.

#include "tree_sitter/parser.h"

#include <stdbool.h>
#include <stdint.h>

enum TokenType {
  AUTOMATIC_SEMICOLON,
  BLOCK_COMMENT,
  ERROR_SENTINEL,
};

void *tree_sitter_tern_external_scanner_create(void) { return NULL; }

void tree_sitter_tern_external_scanner_destroy(void *payload) { (void)payload; }

unsigned tree_sitter_tern_external_scanner_serialize(void *payload, char *buffer) {
  (void)payload;
  (void)buffer;
  return 0;
}

void tree_sitter_tern_external_scanner_deserialize(void *payload, const char *buffer,
                                                    unsigned length) {
  (void)payload;
  (void)buffer;
  (void)length;
}

static bool is_horizontal_space(int32_t c) { return c == ' ' || c == '\t' || c == '\r'; }

static bool is_space(int32_t c) { return is_horizontal_space(c) || c == '\n' || c == '\f'; }

// Consumes the rest of a block comment. The lexer is just past the opening
// "/*". Returns false if the input ends first.
static bool scan_block_comment_body(TSLexer *lexer, bool *has_newline) {
  unsigned depth = 1;
  while (!lexer->eof(lexer)) {
    int32_t c = lexer->lookahead;
    lexer->advance(lexer, false);
    if (c == '\n') {
      *has_newline = true;
    } else if (c == '/' && lexer->lookahead == '*') {
      lexer->advance(lexer, false);
      depth++;
    } else if (c == '*' && lexer->lookahead == '/') {
      lexer->advance(lexer, false);
      depth--;
      if (depth == 0) {
        return true;
      }
    }
  }
  return false;
}

bool tree_sitter_tern_external_scanner_scan(void *payload, TSLexer *lexer,
                                            const bool *valid_symbols) {
  (void)payload;

  // During error recovery every symbol is valid. Only lex comments then.
  bool want_semicolon =
      valid_symbols[AUTOMATIC_SEMICOLON] && !valid_symbols[ERROR_SENTINEL];

  if (want_semicolon) {
    while (is_horizontal_space(lexer->lookahead)) {
      lexer->advance(lexer, true);
    }
    // Any semicolon returned below is zero width, ending here.
    lexer->mark_end(lexer);

    if (lexer->eof(lexer) || lexer->lookahead == '\n') {
      lexer->result_symbol = AUTOMATIC_SEMICOLON;
      return true;
    }
    if (lexer->lookahead != '/') {
      return false;
    }
    lexer->advance(lexer, false);
    if (lexer->lookahead == '/') {
      // A line comment runs to the line end.
      lexer->result_symbol = AUTOMATIC_SEMICOLON;
      return true;
    }
    if (lexer->lookahead != '*') {
      return false;
    }
    lexer->advance(lexer, false);
    bool has_newline = false;
    bool closed = scan_block_comment_body(lexer, &has_newline);
    if (has_newline) {
      // The comment counts as a line end. It is lexed on the next call.
      lexer->result_symbol = AUTOMATIC_SEMICOLON;
      return true;
    }
    if (!closed) {
      return false;
    }
    // A comment within one line acts as a space. Return it, and the next
    // call decides about the semicolon.
    lexer->mark_end(lexer);
    lexer->result_symbol = BLOCK_COMMENT;
    return true;
  }

  if (!valid_symbols[BLOCK_COMMENT]) {
    return false;
  }
  while (is_space(lexer->lookahead)) {
    lexer->advance(lexer, true);
  }
  if (lexer->lookahead != '/') {
    return false;
  }
  lexer->advance(lexer, false);
  if (lexer->lookahead != '*') {
    return false;
  }
  lexer->advance(lexer, false);
  bool has_newline = false;
  if (!scan_block_comment_body(lexer, &has_newline)) {
    return false;
  }
  lexer->mark_end(lexer);
  lexer->result_symbol = BLOCK_COMMENT;
  return true;
}
