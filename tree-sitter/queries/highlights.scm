["fn" "type" "let" "var" "return" "if" "else" "for" "in"] @keyword

(break_statement) @keyword
(continue_statement) @keyword

".." @operator

(parameter_mode) @keyword
(argument "inout" @keyword)

(type_name (identifier) @type)
(type_declaration name: (identifier) @type.definition)
(construct_expression type: (identifier) @type)
(construct_expression type: (field_expression field: (identifier) @type))

(function_declaration name: (identifier) @function)
(call_expression function: (identifier) @function.call)

(parameter name: (identifier) @variable.parameter)
(tuple_type_field name: (identifier) @property)
(tuple_field name: (identifier) @property)
(destructuring_field name: (identifier) @property)
(field_expression field: (identifier) @property)

(discard) @variable.builtin

(integer_literal) @number
(float_literal) @number

[(line_comment) (block_comment)] @comment

["+" "-" "*" "/" "%" "&" "|" "^" "<<" ">>" "~" "!" "&&" "||"
 "==" "!=" "<" "<=" ">" ">="
 "=" "+=" "-=" "*=" "/=" "%=" "&=" "|=" "^=" "<<=" ">>="] @operator

(string_literal) @string
(char_literal) @character
(escape_sequence) @string.escape
(interpolation ["{" "}"] @punctuation.special)
